use std::collections::HashMap;
use tauri::{Emitter, State};

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
///
/// Uses optimistic UI: the local DB is updated and events emitted immediately
/// so the frontend sees the change without waiting for the server round-trip.
/// The server operation runs in the background; on failure an `op-failed` event
/// is emitted and the next sync will reconcile.
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
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    // Gather data needed for the server operation before we modify the DB.
    // For IMAP we need UIDs grouped by folder; for JMAP/Graph we need the IDs.
    let imap_by_folder = if account.mail_protocol != "graph" && account.mail_protocol != "jmap" {
        let conn = state.db.reader();
        let uid_rows = db::messages::get_message_uids(&conn, &message_ids)?;
        let grouped = group_by_folder(uid_rows);
        if grouped.is_empty() {
            log::warn!("No messages found for move operation");
            return Ok(());
        }
        Some(grouped)
    } else {
        None
    };

    // --- Optimistic: update local DB and notify UI immediately ---
    {
        let conn = state.db.writer().await;
        db::messages::delete_messages_by_ids(&conn, &message_ids)?;
        db::folders::recalculate_folder_counts(&conn, &account_id)?;
    }
    emit_messages_changed(&app, &account_id);
    emit_folders_changed(&app, &account_id);

    // --- Background: perform server operation ---
    let app_bg = app.clone();
    let account_id_bg = account_id.clone();
    let message_ids_bg = message_ids.clone();
    let target_folder_bg = target_folder.clone();
    let db_bg = state.db.clone();

    tokio::spawn(async move {
        let result: std::result::Result<(), Error> = async {
            if account.mail_protocol == "graph" {
                let token = crate::mail::graph::get_graph_token(&account_id_bg).await?;
                let client = crate::mail::graph::GraphClient::new(&token);
                for mid in &message_ids_bg {
                    let graph_id = mid.strip_prefix(&format!("{}_", account_id_bg)).unwrap_or(mid);
                    if let Err(e) = client.move_message(graph_id, &target_folder_bg).await {
                        log::error!("Graph move failed for {}: {}", graph_id, e);
                    }
                }
            } else if account.mail_protocol == "jmap" {
                let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;
                let mut by_folder: HashMap<String, Vec<String>> = HashMap::new();
                for mid in &message_ids_bg {
                    let parts: Vec<&str> = mid.splitn(3, '_').collect();
                    if parts.len() == 3 {
                        by_folder.entry(parts[1].to_string()).or_default().push(parts[2].to_string());
                    }
                }
                let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
                for (source_mailbox, jmap_ids) in &by_folder {
                    conn_jmap.move_emails(&jmap_config, jmap_ids, source_mailbox, &target_folder_bg).await?;
                }
            } else {
                // IMAP path
                let imap_config = build_imap_config(&account).await?;
                let by_folder = imap_by_folder.unwrap_or_default();
                let target = target_folder_bg.clone();
                tokio::task::spawn_blocking(move || -> Result<()> {
                    let mut conn = ImapConnection::connect(&imap_config)?;
                    for (folder_path, uids) in &by_folder {
                        conn.select_folder(folder_path)?;
                        conn.move_messages(uids, &target)?;
                    }
                    conn.logout();
                    Ok(())
                })
                .await
                .map_err(|e| Error::Other(format!("Move task panicked: {}", e)))??;
            }
            Ok(())
        }
        .await;

        if let Err(e) = result {
            log::error!("Background move failed for account {}: {}", account_id_bg, e);
            app_bg.emit("op-failed", serde_json::json!({
                "account_id": account_id_bg,
                "op_type": "move",
                "error": e.to_string(),
            })).ok();
            // Recalculate folder counts since the next sync will restore messages
            if let Ok(conn) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| db_bg.reader())) {
                let _ = db::folders::recalculate_folder_counts(&conn, &account_id_bg);
            }
        } else {
            log::info!("Move complete: {} messages moved to '{}'", message_ids_bg.len(), target_folder_bg);
        }
    });

    Ok(())
}

/// Delete messages on the IMAP server and remove from local DB.
///
/// Uses optimistic UI: the local DB is updated and events emitted immediately.
/// The server deletion runs in the background; on failure an `op-failed` event
/// is emitted and the next sync will reconcile.
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
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    // Gather IMAP UIDs before modifying the DB
    let imap_by_folder = if account.mail_protocol != "graph" && account.mail_protocol != "jmap" {
        let conn = state.db.reader();
        let uid_rows = db::messages::get_message_uids(&conn, &message_ids)?;
        let grouped = group_by_folder(uid_rows);
        if grouped.is_empty() {
            log::warn!("No messages found for delete operation");
            return Ok(());
        }
        Some(grouped)
    } else {
        None
    };

    // For O365 IMAP: suspend IDLE before background op (needs to happen on this task)
    let suspended_idle = if account.mail_protocol != "graph"
        && account.mail_protocol != "jmap"
        && should_suspend_idle_for_imap_operation(&account.provider)
    {
        suspend_imap_idle_for_account(&state, &account_id).await?
    } else {
        false
    };
    let resume_account = account.clone();

    // --- Optimistic: update local DB and notify UI immediately ---
    {
        let conn = state.db.writer().await;
        db::messages::delete_messages_by_ids(&conn, &message_ids)?;
        db::folders::recalculate_folder_counts(&conn, &account_id)?;
    }
    emit_messages_changed(&app, &account_id);
    emit_folders_changed(&app, &account_id);

    // --- Background: perform server operation ---
    let app_bg = app.clone();
    let account_id_bg = account_id.clone();
    let message_ids_bg = message_ids.clone();

    tokio::spawn(async move {
        let result: std::result::Result<(), Error> = async {
            if account.mail_protocol == "graph" {
                let token = crate::mail::graph::get_graph_token(&account_id_bg).await?;
                let client = crate::mail::graph::GraphClient::new(&token);
                for mid in &message_ids_bg {
                    let graph_id = mid.strip_prefix(&format!("{}_", account_id_bg)).unwrap_or(mid);
                    if let Err(e) = client.delete_message(graph_id).await {
                        log::error!("Graph delete failed for {}: {}", graph_id, e);
                    }
                }
            } else if account.mail_protocol == "jmap" {
                let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;
                let jmap_ids: Vec<String> = message_ids_bg
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
                // IMAP path
                let imap_config = build_imap_config(&account).await?;
                let by_folder = imap_by_folder.unwrap_or_default();
                tokio::task::spawn_blocking(move || -> Result<()> {
                    let mut conn = ImapConnection::connect(&imap_config)?;
                    for (folder_path, uids) in &by_folder {
                        conn.select_folder(folder_path)?;
                        conn.delete_messages(uids)?;
                    }
                    conn.logout();
                    Ok(())
                })
                .await
                .map_err(|e| Error::Other(format!("Delete task panicked: {}", e)))??;
            }
            Ok(())
        }
        .await;

        if let Err(e) = result {
            log::error!("Background delete failed for account {}: {}", account_id_bg, e);
            app_bg.emit("op-failed", serde_json::json!({
                "account_id": account_id_bg,
                "op_type": "delete",
                "error": e.to_string(),
            })).ok();
        } else {
            log::info!("Delete complete: {} messages deleted", message_ids_bg.len());
        }
    });

    // Resume IDLE (for O365) — do this on the main task, not in the background,
    // since State<'_, AppState> can't be sent across spawn boundaries.
    if suspended_idle {
        resume_imap_idle_for_account(&app, &state, &resume_account, suspended_idle).await?;
    }

    Ok(())
}

/// Move messages from one account to a folder in a *different* account.
///
/// Reads the raw RFC822 bytes from the source account's maildir, appends
/// them to the destination folder via the destination protocol, and then
/// deletes the source messages.
#[tauri::command]
pub async fn move_messages_cross_account(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    source_account_id: String,
    message_ids: Vec<String>,
    target_account_id: String,
    target_folder: String,
) -> Result<()> {
    log::info!(
        "Cross-account move: {} -> {}/{} ({} messages)",
        source_account_id,
        target_account_id,
        target_folder,
        message_ids.len()
    );

    if source_account_id == target_account_id {
        return Err(Error::Other(
            "Use move_messages for same-account moves".into(),
        ));
    }

    // Load target account and source maildir paths (scoped to source account)
    let (target_account, maildir_paths) = {
        let conn = state.db.reader();
        let target = db::accounts::get_account_full(&conn, &target_account_id)?;
        let paths = db::messages::get_maildir_paths(&conn, &source_account_id, &message_ids)?;
        (target, paths)
    };

    if maildir_paths.len() != message_ids.len() {
        return Err(Error::Other(format!(
            "Cross-account move requires all messages to be synced locally \
             (found {}/{} maildir files). Sync the source folder first.",
            maildir_paths.len(),
            message_ids.len()
        )));
    }

    // Resolve and validate all maildir paths before starting the transfer.
    // Rejects non-disk entries (graph: prefix), absolute paths, and ".." segments.
    let data_dir = state.data_dir.clone();
    let validated_paths: Vec<std::path::PathBuf> = {
        let canonical_data_dir = std::fs::canonicalize(&data_dir)
            .unwrap_or_else(|_| data_dir.clone());
        let mut paths = Vec::with_capacity(maildir_paths.len());
        for (msg_id, maildir_path) in &maildir_paths {
            if maildir_path.starts_with("graph:") {
                return Err(Error::Other(format!(
                    "Message {} is not stored on disk (Graph API). \
                     Cross-account move requires locally synced messages.",
                    msg_id
                )));
            }
            let rel = std::path::Path::new(maildir_path);
            if rel.is_absolute() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                return Err(Error::Other(format!(
                    "Invalid maildir path for message {}: '{}'",
                    msg_id, maildir_path
                )));
            }
            let full_path = data_dir.join(maildir_path);
            let canonical = std::fs::canonicalize(&full_path).map_err(|e| {
                Error::Other(format!(
                    "Failed to resolve maildir file {}: {}",
                    full_path.display(), e
                ))
            })?;
            if !canonical.starts_with(&canonical_data_dir) {
                return Err(Error::Other(format!(
                    "Path traversal detected for message {}", msg_id
                )));
            }
            paths.push(canonical);
        }
        paths
    };

    // Append to destination — stream one message at a time to avoid
    // loading all message bodies into memory simultaneously.
    match target_account.mail_protocol.as_str() {
        "imap" => {
            // IMAP: read and append in a single blocking task (one connection)
            let imap_config = build_imap_config(&target_account).await?;
            let target_folder_clone = target_folder.clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                let mut conn = ImapConnection::connect(&imap_config)?;
                for path in &validated_paths {
                    let bytes = std::fs::read(path).map_err(|e| {
                        Error::Other(format!("Failed to read {}: {}", path.display(), e))
                    })?;
                    conn.append_message_raw(&target_folder_clone, &bytes)?;
                }
                conn.logout();
                Ok(())
            })
            .await
            .map_err(|e| Error::Other(format!("IMAP append task panicked: {}", e)))??;
        }
        "jmap" => {
            // JMAP: read each message in a blocking task, then import async
            let jmap_config =
                crate::commands::sync_cmd::build_jmap_config(&target_account).await?;
            let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
            for path in &validated_paths {
                let path_clone = path.clone();
                let bytes = tokio::task::spawn_blocking(move || {
                    std::fs::read(&path_clone)
                })
                .await
                .map_err(|e| Error::Other(format!("Read task panicked: {}", e)))?
                .map_err(|e| Error::Other(format!("Failed to read {}: {}", path.display(), e)))?;
                conn_jmap
                    .import_email_to_mailbox(&jmap_config, &bytes, &target_folder, false)
                    .await?;
            }
        }
        "graph" => {
            return Err(Error::Other(
                "Cross-account move to Microsoft 365 (Graph) is not yet supported.".into(),
            ));
        }
        other => {
            return Err(Error::Other(format!(
                "Unknown mail protocol for destination account: {}",
                other
            )));
        }
    }

    // Append succeeded — delete from source
    delete_messages(app.clone(), state, source_account_id.clone(), message_ids.clone()).await?;

    // Emit events for the destination account too so its folder counts refresh
    emit_messages_changed(&app, &target_account_id);
    emit_folders_changed(&app, &target_account_id);

    log::info!(
        "Cross-account move complete: {} messages moved to {}/{}",
        message_ids.len(),
        target_account_id,
        target_folder
    );
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
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    // Update local DB FIRST (before slow network ops) so that any
    // concurrent re-fetch from a "messages-changed" event always sees
    // the latest flags, preventing race conditions with rapid toggles.
    {
        let conn = state.db.writer().await;

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
            let conn = state.db.reader();
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
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };
    let imap_config = build_imap_config(&account).await?;
    let by_folder = {
        let conn = state.db.reader();
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
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    // Mark read on the server first
    if account.mail_protocol == "graph" {
        let unread_ids = {
            let conn = state.db.reader();
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
            let conn = state.db.reader();
            let folders = db::folders::list_folders(&conn, &account_id)?;
            folders.into_iter().map(|f| f.path).collect()
        };
        let imap_result = tokio::task::spawn_blocking(move || -> Result<()> {
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
        .map_err(|e| Error::Other(format!("Mark account read task panicked: {}", e)))?;

        // Always resume IDLE, even if the mark-read operation failed
        resume_imap_idle_for_account(&app, &state, &resume_account, suspended_idle).await?;
        imap_result?;
    } else if account.mail_protocol == "jmap" {
        // JMAP: bulk update all unread emails to $seen via Email/set
        let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;
        let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;

        let unread_ids: Vec<String> = {
            let conn = state.db.reader();
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
        let conn = state.db.writer().await;
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
