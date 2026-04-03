use std::collections::HashMap;
use tauri::State;

use crate::db;
use crate::error::{Error, Result};
use crate::mail::imap::{ImapConfig, ImapConnection};
use crate::state::AppState;

/// Move messages to a target folder on the IMAP server and update local DB.
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

    // Get account config and message UIDs from DB
    let (imap_config, by_folder) = {
        let conn = state.db.lock().await;
        let account = db::accounts::get_account_full(&conn, &account_id)?;
        let config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username,
            password: account.password,
            use_tls: account.use_tls,
        };
        let uid_rows = db::messages::get_message_uids(&conn, &message_ids)?;
        let grouped = group_by_folder(uid_rows);
        (config, grouped)
    };

    if by_folder.is_empty() {
        log::warn!("No messages found for move operation");
        return Ok(());
    }

    let target = target_folder.clone();

    // Perform IMAP operations in a blocking thread
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

    // Remove moved messages from local DB
    {
        let conn = state.db.lock().await;
        db::messages::delete_messages_by_ids(&conn, &message_ids)?;
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

    let (imap_config, by_folder) = {
        let conn = state.db.lock().await;
        let account = db::accounts::get_account_full(&conn, &account_id)?;
        let config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username,
            password: account.password,
            use_tls: account.use_tls,
        };
        let uid_rows = db::messages::get_message_uids(&conn, &message_ids)?;
        let grouped = group_by_folder(uid_rows);
        (config, grouped)
    };

    if by_folder.is_empty() {
        log::warn!("No messages found for delete operation");
        return Ok(());
    }

    // Perform IMAP deletions in a blocking thread
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

    // Remove from local DB
    {
        let conn = state.db.lock().await;
        db::messages::delete_messages_by_ids(&conn, &message_ids)?;
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

    let (imap_config, by_folder, current_flags_map) = {
        let conn = state.db.lock().await;
        let account = db::accounts::get_account_full(&conn, &account_id)?;
        let config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username,
            password: account.password,
            use_tls: account.use_tls,
        };
        let uid_rows = db::messages::get_message_uids(&conn, &message_ids)?;
        let grouped = group_by_folder(uid_rows);

        // Get current flags for each message so we can update them locally
        let mut flags_map: HashMap<String, Vec<String>> = HashMap::new();
        for msg_id in &message_ids {
            let (_, _, _, _, flags_json, _, _) =
                db::messages::get_message_metadata(&conn, &account_id, msg_id)?;
            let current: Vec<String> = serde_json::from_str(&flags_json).unwrap_or_default();
            flags_map.insert(msg_id.clone(), current);
        }

        (config, grouped, flags_map)
    };

    if by_folder.is_empty() {
        log::warn!("No messages found for flag operation");
        return Ok(());
    }

    let flag_refs: Vec<String> = flags.clone();

    // Perform IMAP flag operations in a blocking thread
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut conn = ImapConnection::connect(&imap_config)?;
        let flag_strs: Vec<&str> = flag_refs.iter().map(|s| s.as_str()).collect();

        for (folder_path, uids) in &by_folder {
            log::debug!(
                "Setting flags on {} messages in '{}'",
                uids.len(),
                folder_path
            );
            conn.select_folder(folder_path)?;
            conn.set_flags(uids, &flag_strs, add)?;
        }

        conn.logout();
        Ok(())
    })
    .await
    .map_err(|e| Error::Other(format!("Set flags task panicked: {}", e)))??;

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

    let (imap_config, by_folder) = {
        let conn = state.db.lock().await;
        let account = db::accounts::get_account_full(&conn, &account_id)?;
        let config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username,
            password: account.password,
            use_tls: account.use_tls,
        };
        let uid_rows = db::messages::get_message_uids(&conn, &message_ids)?;
        let grouped = group_by_folder(uid_rows);
        (config, grouped)
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
