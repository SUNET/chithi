use std::collections::HashMap;
use tauri::State;

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
        let jmap_config = crate::mail::jmap::JmapConfig {
            jmap_url: account.jmap_url.clone(),
            email: account.email.clone(),
            username: account.username.clone(),
            password: account.password.clone(),
        };

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

    Ok(())
}

/// Delete messages on the IMAP server and remove from local DB.
#[tauri::command]
pub async fn delete_messages(
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
        let jmap_config = crate::mail::jmap::JmapConfig {
            jmap_url: account.jmap_url.clone(),
            email: account.email.clone(),
            username: account.username.clone(),
            password: account.password.clone(),
        };

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

    Ok(())
}

/// Set or remove flags on messages (e.g., \Seen, \Flagged).
#[tauri::command]
pub async fn set_message_flags(
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
        let jmap_config = crate::mail::jmap::JmapConfig {
            jmap_url: account.jmap_url.clone(),
            email: account.email.clone(),
            username: account.username.clone(),
            password: account.password.clone(),
        };

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
    }

    // Build current flags map for local update
    let current_flags_map: HashMap<String, Vec<String>> = {
        let conn = state.db.lock().await;
        let mut map = HashMap::new();
        for msg_id in &message_ids {
            if let Ok((_, _, _, _, flags_json, _, _)) =
                db::messages::get_message_metadata(&conn, &account_id, msg_id)
            {
                let current: Vec<String> = serde_json::from_str(&flags_json).unwrap_or_default();
                map.insert(msg_id.clone(), current);
            }
        }
        map
    };

    // Update flags in local DB
    {
        let conn = state.db.lock().await;

        // Convert IMAP flag names (e.g. \Seen) to our lowercase names (e.g. seen)
        let normalized_flags: Vec<String> = flags
            .iter()
            .map(|f| normalize_flag_name(f))
            .collect();

        for (msg_id, mut current) in current_flags_map {
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
            db::messages::update_flags(&conn, &msg_id, &updated_json)?;
        }
    }

    log::info!(
        "Set flags complete: {} flags {} on {} messages",
        if add { "added" } else { "removed" },
        flags.join(", "),
        message_ids.len()
    );

    Ok(())
}

/// Copy messages to a target folder on the IMAP server.
#[tauri::command]
pub async fn copy_messages(
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

    Ok(())
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
