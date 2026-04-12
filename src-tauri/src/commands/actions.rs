use std::collections::HashMap;
use tauri::State;

use crate::commands::events::{emit_folders_changed, emit_messages_changed};
use crate::commands::sync_cmd::{
    resume_imap_idle_for_account,
    should_suspend_idle_for_imap_operation,
    suspend_imap_idle_for_account,
};
use crate::db;
use crate::error::{Error, Result};
use crate::mail::imap::{ImapConfig, ImapConnection};
use crate::state::AppState;

/// Build an ImapConfig for an account, handling O365 XOAUTH2 token refresh.
async fn build_imap_config(account: &db::accounts::AccountFull) -> Result<ImapConfig> {
    let (password, use_xoauth2) = if account.provider == "o365" {
        let tokens = crate::oauth::load_tokens(&account.id)?
            .ok_or_else(|| Error::Other("No O365 tokens".into()))?;
        let refresh = tokens.refresh_token
            .ok_or_else(|| Error::Other("No O365 refresh token".into()))?;
        let new = crate::oauth::refresh_with_scopes(
            &crate::oauth::MICROSOFT, &refresh, crate::oauth::MICROSOFT_IMAP_SCOPES,
        ).await?;
        crate::oauth::store_tokens(&account.id, &new)?;
        (new.access_token, true)
    } else {
        (account.password.clone(), false)
    };
    Ok(ImapConfig {
        host: account.imap_host.clone(),
        port: account.imap_port,
        username: account.username.clone(),
        password,
        use_tls: account.use_tls,
        use_xoauth2,
    })
}

/// Move messages to a target folder on the IMAP/JMAP server and update local DB.
#[tauri::command]
pub async fn move_messages(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_ids: Vec<String>,
    target_folder: String,
) -> Result<()> {
    log::info!(
        "Move messages command: account={} messages={} target='{}'",
        account_id,
        message_ids.len(),
        target_folder
    );

    let account = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)?
    };

    if account.mail_protocol == "graph" {
        // Graph path: extract Graph message IDs and move via Graph API
        let token = crate::mail::graph::get_graph_token(&account_id).await?;
        let client = crate::mail::graph::GraphClient::new(&token);
        for mid in &message_ids {
            // Format: {account_id}_{graph_message_id}
            let graph_id = mid.strip_prefix(&format!("{}_", account_id)).unwrap_or(mid);
            if let Err(e) = client.move_message(graph_id, &target_folder).await {
                log::error!("Graph move failed for {}: {}", graph_id, e);
            }
        }
    } else if account.mail_protocol == "jmap" {
        // JMAP path: extract JMAP email IDs and source mailbox, then move via JMAP API
        let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;

        // Extract JMAP IDs and group by source folder
        let mut by_folder: HashMap<String, Vec<String>> = HashMap::new();
        for mid in &message_ids {
            // Format: {account_id}_{folder}_{jmap_email_id}
            let parts: Vec<&str> = mid.splitn(3, '_').collect();
            if parts.len() == 3 {
                by_folder.entry(parts[1].to_string()).or_default().push(parts[2].to_string());
            }
        }

        // Find the JMAP mailbox ID for the target folder
        let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
        let target_mailbox = target_folder.clone();

        for (source_mailbox, jmap_ids) in &by_folder {
            conn_jmap.move_emails(&jmap_config, jmap_ids, source_mailbox, &target_mailbox).await?;
        }
    } else {
        // IMAP path (includes O365 with XOAUTH2)
        let imap_config = build_imap_config(&account).await?;
        let by_folder = {
            let conn = state.db.lock().await;
            let uid_rows = db::messages::get_message_uids(&conn, &message_ids)?;
            group_by_folder(uid_rows)
        };

        if by_folder.is_empty() {
            log::warn!("No messages found for move operation");
            return Ok(());
        }

        let target = target_folder.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut conn = ImapConnection::connect(&imap_config)?;

            for (folder_path, uids) in &by_folder {
                log::debug!(
                    "Moving {} messages from '{}' to '{}'",
                    uids.len(),
                    folder_path,
                    target
                );
                conn.select_folder(folder_path)?;
                conn.move_messages(uids, &target)?;
            }

            conn.logout();
            Ok(())
        })
        .await
        .map_err(|e| Error::Other(format!("Move task panicked: {}", e)))??;
    }

    // Remove moved messages from local DB and recalculate folder counts
    {
        let conn = state.db.lock().await;
        db::messages::delete_messages_by_ids(&conn, &message_ids)?;
        db::folders::recalculate_folder_counts(&conn, &account_id)?;
    }

    log::info!(
        "Move complete: {} messages moved to '{}'",
        message_ids.len(),
        target_folder
    );

    emit_messages_changed(&app, &account_id);
    emit_folders_changed(&app, &account_id);

    Ok(())
}

/// Delete messages on the IMAP server and remove from local DB.
#[tauri::command]
pub async fn delete_messages(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_ids: Vec<String>,
) -> Result<()> {
    log::info!(
        "Delete messages command: account={} messages={}",
        account_id,
        message_ids.len()
    );

    let account = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)?
    };

    if account.mail_protocol == "graph" {
        let token = crate::mail::graph::get_graph_token(&account_id).await?;
        let client = crate::mail::graph::GraphClient::new(&token);
        for mid in &message_ids {
            let graph_id = mid.strip_prefix(&format!("{}_", account_id)).unwrap_or(mid);
            if let Err(e) = client.delete_message(graph_id).await {
                log::error!("Graph delete failed for {}: {}", graph_id, e);
            }
        }
    } else if account.mail_protocol == "jmap" {
        // JMAP path: extract JMAP email IDs and delete via Email/set destroy
        let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;

        // Extract JMAP email IDs from composite message IDs
        // Format: {account_id}_{folder}_{jmap_email_id}
        let jmap_ids: Vec<String> = message_ids
            .iter()
            .filter_map(|mid| {
                let parts: Vec<&str> = mid.splitn(3, '_').collect();
                if parts.len() == 3 { Some(parts[2].to_string()) } else { None }
            })
            .collect();

        if !jmap_ids.is_empty() {
            let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
            conn_jmap.delete_emails(&jmap_config, &jmap_ids).await?;
        }
    } else {
        // IMAP path (includes O365 with XOAUTH2)
        let suspended_idle = if should_suspend_idle_for_imap_operation(&account.provider) {
            suspend_imap_idle_for_account(&state, &account_id).await?
        } else {
            false
        };
        let resume_account = account.clone();
        let imap_config = build_imap_config(&account).await?;
        let by_folder = {
            let conn = state.db.lock().await;
            let uid_rows = db::messages::get_message_uids(&conn, &message_ids)?;
            group_by_folder(uid_rows)
        };

        if by_folder.is_empty() {
            log::warn!("No messages found for delete operation");
            return Ok(());
        }

        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut conn = ImapConnection::connect(&imap_config)?;

            for (folder_path, uids) in &by_folder {
                log::debug!(
                    "Deleting {} messages from '{}'",
                    uids.len(),
                    folder_path
                );
                conn.select_folder(folder_path)?;
                conn.delete_messages(uids)?;
            }

            conn.logout();
            Ok(())
        })
        .await
        .map_err(|e| Error::Other(format!("Delete task panicked: {}", e)))??;

        resume_imap_idle_for_account(&app, &state, &resume_account, suspended_idle).await?;
    }

    // Remove from local DB and recalculate folder counts
    {
        let conn = state.db.lock().await;
        db::messages::delete_messages_by_ids(&conn, &message_ids)?;
        db::folders::recalculate_folder_counts(&conn, &account_id)?;
    }

    log::info!(
        "Delete complete: {} messages deleted",
        message_ids.len()
    );

    emit_messages_changed(&app, &account_id);
    emit_folders_changed(&app, &account_id);

    Ok(())
}

/// Set or remove flags on messages (e.g., \Seen, \Flagged).
#[tauri::command]
pub async fn set_message_flags(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_ids: Vec<String>,
    flags: Vec<String>,
    add: bool,
) -> Result<()> {
    log::info!(
        "Set flags command: account={} messages={} flags={:?} add={}",
        account_id,
        message_ids.len(),
        flags,
        add
    );

    let account = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)?
    };

    // Update local DB FIRST (before slow network ops) so that any
    // concurrent re-fetch from a "messages-changed" event always sees
    // the latest flags, preventing race conditions with rapid toggles.
    {
        let conn = state.db.lock().await;

        let normalized_flags: Vec<String> = flags
            .iter()
            .map(|f| normalize_flag_name(f))
            .collect();

        for msg_id in &message_ids {
            if let Ok((_, _, _, _, flags_json, _, _)) =
                db::messages::get_message_metadata(&conn, &account_id, msg_id)
            {
                let mut current: Vec<String> =
                    serde_json::from_str(&flags_json).unwrap_or_default();
                if add {
                    for flag in &normalized_flags {
                        if !current.contains(flag) {
                            current.push(flag.clone());
                        }
                    }
                } else {
                    current.retain(|f| !normalized_flags.contains(f));
                }
                let updated_json = serde_json::to_string(&current)
                    .unwrap_or_else(|_| "[]".to_string());
                db::messages::update_flags(&conn, msg_id, &updated_json)?;
            }
        }
    }

    // Now perform the remote operation (slow, network I/O)
    if account.mail_protocol == "graph" {
        // Graph path: use PATCH to update isRead
        let token = crate::mail::graph::get_graph_token(&account_id).await?;
        let client = crate::mail::graph::GraphClient::new(&token);
        let is_seen_flag = flags.iter().any(|f| f == "seen" || f == "\\Seen");
        if is_seen_flag {
            let graph_ids: Vec<String> = message_ids.iter().map(|mid| {
                mid.strip_prefix(&format!("{}_", account_id)).unwrap_or(mid).to_string()
            }).collect();
            client.set_read_status(&graph_ids, add).await?;
        }
    } else if account.mail_protocol == "jmap" {
        // JMAP path: extract JMAP email IDs and set flags via JMAP API
        let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;

        // Extract JMAP email IDs from composite message IDs
        let jmap_ids: Vec<String> = message_ids.iter().map(|mid| {
            // Format: {account_id}_{folder}_{jmap_email_id}
            let parts: Vec<&str> = mid.splitn(3, '_').collect();
            if parts.len() == 3 { parts[2].to_string() } else { mid.clone() }
        }).collect();

        let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
        let flag_strs: Vec<&str> = flags.iter().map(|s| s.as_str()).collect();
        conn_jmap.set_flags(&jmap_config, &jmap_ids, &flag_strs, add).await?;
    } else {
        // IMAP path (includes O365 with XOAUTH2)
        let suspended_idle = if should_suspend_idle_for_imap_operation(&account.provider) {
            suspend_imap_idle_for_account(&state, &account_id).await?
        } else {
            false
        };
        let resume_account = account.clone();
        let imap_config = build_imap_config(&account).await?;
        let by_folder = {
            let conn = state.db.lock().await;
            let uid_rows = db::messages::get_message_uids(&conn, &message_ids)?;
            group_by_folder(uid_rows)
        };

        if !by_folder.is_empty() {
            let flag_refs: Vec<String> = flags.clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                let mut conn = ImapConnection::connect(&imap_config)?;
                let flag_strs: Vec<&str> = flag_refs.iter().map(|s| s.as_str()).collect();
                for (folder_path, uids) in &by_folder {
                    conn.select_folder(folder_path)?;
                    conn.set_flags(uids, &flag_strs, add)?;
                }
                conn.logout();
                Ok(())
            })
            .await
            .map_err(|e| Error::Other(format!("Set flags task panicked: {}", e)))??;
        }

        resume_imap_idle_for_account(&app, &state, &resume_account, suspended_idle).await?;
    }

    log::info!(
        "Set flags complete: {} flags {} on {} messages",
        if add { "added" } else { "removed" },
        flags.join(", "),
        message_ids.len()
    );

    emit_messages_changed(&app, &account_id);

    Ok(())
}

/// Copy messages to a target folder on the IMAP server.
#[tauri::command]
pub async fn copy_messages(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_ids: Vec<String>,
    target_folder: String,
) -> Result<()> {
    log::info!(
        "Copy messages command: account={} messages={} target='{}'",
        account_id,
        message_ids.len(),
        target_folder
    );

    let account = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)?
    };
    let imap_config = build_imap_config(&account).await?;
    let by_folder = {
        let conn = state.db.lock().await;
        let uid_rows = db::messages::get_message_uids(&conn, &message_ids)?;
        group_by_folder(uid_rows)
    };

    if by_folder.is_empty() {
        log::warn!("No messages found for copy operation");
        return Ok(());
    }

    let target = target_folder.clone();

    // Perform IMAP copy in a blocking thread
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut conn = ImapConnection::connect(&imap_config)?;

        for (folder_path, uids) in &by_folder {
            log::debug!(
                "Copying {} messages from '{}' to '{}'",
                uids.len(),
                folder_path,
                target
            );
            conn.select_folder(folder_path)?;
            conn.copy_messages(uids, &target)?;
        }

        conn.logout();
        Ok(())
    })
    .await
    .map_err(|e| Error::Other(format!("Copy task panicked: {}", e)))??;

    log::info!(
        "Copy complete: {} messages copied to '{}'",
        message_ids.len(),
        target_folder
    );

    emit_folders_changed(&app, &account_id);
    emit_messages_changed(&app, &account_id);

    Ok(())
}

/// Mark all messages in all folders of an account as read.
/// Updates both the remote server and local DB.
#[tauri::command]
pub async fn mark_account_read(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
) -> Result<u64> {
    log::info!("Marking all messages as read for account {}", account_id);

    let account = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)?
    };

    // Mark read on the server first
    if account.mail_protocol == "graph" {
        let unread_ids = {
            let conn = state.db.lock().await;
            let mut stmt = conn.prepare(
                "SELECT id FROM messages WHERE account_id = ?1 AND flags NOT LIKE '%seen%'",
            ).map_err(crate::error::Error::Database)?;
            let ids: Vec<String> = stmt
                .query_map(rusqlite::params![&account_id], |row| row.get::<_, String>(0))
                .map_err(crate::error::Error::Database)?
                .filter_map(|r| r.ok())
                .collect();
            ids
        };
        if !unread_ids.is_empty() {
            let token = crate::mail::graph::get_graph_token(&account_id).await?;
            let client = crate::mail::graph::GraphClient::new(&token);
            let graph_ids: Vec<String> = unread_ids.iter().map(|mid| {
                mid.strip_prefix(&format!("{}_", account_id)).unwrap_or(mid).to_string()
            }).collect();
            client.set_read_status(&graph_ids, true).await?;
        }
    } else if account.mail_protocol == "imap" {
        // IMAP: SELECT each folder and STORE +FLAGS \Seen on all messages
        let suspended_idle = if should_suspend_idle_for_imap_operation(&account.provider) {
            suspend_imap_idle_for_account(&state, &account_id).await?
        } else {
            false
        };
        let resume_account = account.clone();
        let imap_config = build_imap_config(&account).await?;
        let folder_paths: Vec<String> = {
            let conn = state.db.lock().await;
            let folders = db::folders::list_folders(&conn, &account_id)?;
            folders.into_iter().map(|f| f.path).collect()
        };
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut conn = ImapConnection::connect(&imap_config)?;
            for folder_path in &folder_paths {
                if let Err(e) = conn.select_folder(folder_path) {
                    log::warn!("Cannot select '{}' for mark-read: {}", folder_path, e);
                    continue;
                }
                if let Err(e) = conn.mark_all_seen() {
                    log::warn!("Mark all seen failed on '{}': {}", folder_path, e);
                }
            }
            conn.logout();
            Ok(())
        })
        .await
        .map_err(|e| Error::Other(format!("Mark account read task panicked: {}", e)))??;

        resume_imap_idle_for_account(&app, &state, &resume_account, suspended_idle).await?;
    } else if account.mail_protocol == "jmap" {
        // JMAP: bulk update all unread emails to $seen via Email/set
        let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;
        let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;

        let unread_ids: Vec<String> = {
            let conn = state.db.lock().await;
            let mut stmt = conn.prepare(
                "SELECT id FROM messages WHERE account_id = ?1 AND flags NOT LIKE '%seen%'",
            ).map_err(crate::error::Error::Database)?;
            let rows = stmt.query_map(rusqlite::params![&account_id], |row| row.get::<_, String>(0))
                .map_err(crate::error::Error::Database)?;
            let ids: Vec<String> = rows.filter_map(|r| r.ok()).collect();
            ids
        };

        if !unread_ids.is_empty() {
            // Extract JMAP email IDs from composite message IDs
            let jmap_ids: Vec<String> = unread_ids.iter().map(|mid| {
                let parts: Vec<&str> = mid.splitn(3, '_').collect();
                if parts.len() == 3 { parts[2].to_string() } else { mid.clone() }
            }).collect();

            let flag_strs = vec!["seen"];
            conn_jmap.set_flags(&jmap_config, &jmap_ids, &flag_strs, true).await?;
        }
    }

    // Update local DB
    let updated = {
        let conn = state.db.lock().await;
        let count = conn.execute(
            "UPDATE messages SET flags = json_insert(flags, '$[#]', 'seen')
             WHERE account_id = ?1 AND flags NOT LIKE '%seen%'",
            rusqlite::params![account_id],
        ).map_err(crate::error::Error::Database)?;
        db::folders::recalculate_folder_counts(&conn, &account_id)?;
        count
    };

    log::info!(
        "mark_account_read: updated {} messages for account {}",
        updated,
        account_id,
    );

    emit_messages_changed(&app, &account_id);
    emit_folders_changed(&app, &account_id);

    Ok(updated as u64)
}

/// Group message UIDs by their folder path.
///
/// Takes (message_id, folder_path, uid) rows and returns a HashMap
/// of folder_path -> Vec<uid>.
fn group_by_folder(rows: Vec<(String, String, u32)>) -> HashMap<String, Vec<u32>> {
    let mut by_folder: HashMap<String, Vec<u32>> = HashMap::new();
    for (_message_id, folder_path, uid) in rows {
        by_folder.entry(folder_path).or_default().push(uid);
    }
    by_folder
}

/// Normalize an IMAP flag name (e.g. \Seen -> seen, \Flagged -> flagged).
fn normalize_flag_name(flag: &str) -> String {
    flag.trim_start_matches('\\').to_lowercase()
}
