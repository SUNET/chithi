use std::collections::BTreeMap;
use tauri::{AppHandle, State};

use crate::db;
use crate::error::{Error, Result};
use crate::mail::imap::{ImapConfig, ImapConnection};
use crate::mail::sync as mail_sync;
use crate::state::AppState;

#[tauri::command]
pub async fn trigger_sync(
    app: AppHandle,
    state: State<'_, AppState>,
    account_id: String,
) -> Result<()> {
    log::info!("Sync requested for account {}", account_id);
    let account = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)?
    };
    log::info!(
        "Syncing account {} ({}) via {}:{}",
        account.display_name, account.email, account.imap_host, account.imap_port
    );

    let imap_config = ImapConfig {
        host: account.imap_host,
        port: account.imap_port,
        username: account.username,
        password: account.password,
        use_tls: account.use_tls,
    };

    mail_sync::sync_account(
        app,
        state.db.clone(),
        state.data_dir.clone(),
        account_id,
        account.display_name,
        imap_config,
    )
    .await?;

    Ok(())
}

#[derive(serde::Serialize)]
pub struct SyncStatus {
    pub account_id: String,
    pub is_syncing: bool,
    pub last_sync: Option<String>,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn get_sync_status(
    _state: State<'_, AppState>,
    account_id: String,
) -> Result<SyncStatus> {
    Ok(SyncStatus {
        account_id,
        is_syncing: false,
        last_sync: None,
        error: None,
    })
}

/// Prefetch message bodies in the background after sync completes.
/// Opens a single IMAP connection, groups messages by folder to minimize
/// SELECT commands, fetches each body, writes to Maildir, and updates DB.
/// Returns the number of bodies successfully fetched.
#[tauri::command]
pub async fn prefetch_bodies(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<u32> {
    log::info!("Prefetch bodies requested for account {}", account_id);

    let (imap_config, data_dir) = {
        let conn = state.db.lock().await;
        let account = db::accounts::get_account_full(&conn, &account_id)?;
        let config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username,
            password: account.password,
            use_tls: account.use_tls,
        };
        (config, state.data_dir.clone())
    };

    // Fetch the list of unfetched messages (up to 100)
    let unfetched = {
        let conn = state.db.lock().await;
        db::messages::get_unfetched_messages(&conn, &account_id, 100)?
    };

    if unfetched.is_empty() {
        log::info!("Prefetch: no unfetched messages for account {}", account_id);
        return Ok(0);
    }

    log::info!(
        "Prefetch: {} unfetched messages to process for account {}",
        unfetched.len(),
        account_id
    );

    // Group messages by folder to minimize IMAP SELECT commands.
    // BTreeMap keeps folders sorted for deterministic ordering.
    let mut by_folder: BTreeMap<String, Vec<(String, u32, String)>> = BTreeMap::new();
    for (message_id, folder_path, uid, flags_json) in unfetched {
        by_folder
            .entry(folder_path)
            .or_default()
            .push((message_id, uid, flags_json));
    }

    let db = state.db.clone();

    let fetched_count = tokio::task::spawn_blocking(move || -> Result<u32> {
        let mut conn_imap = ImapConnection::connect(&imap_config)?;
        let mut count = 0u32;

        for (folder_path, messages) in &by_folder {
            log::info!(
                "Prefetch: selecting folder '{}' ({} messages)",
                folder_path,
                messages.len()
            );
            if let Err(e) = conn_imap.select_folder(folder_path) {
                log::error!("Prefetch: failed to select folder '{}': {}", folder_path, e);
                continue;
            }

            for (message_id, uid, flags_json) in messages {
                log::debug!(
                    "Prefetch: fetching body for message_id={} uid={} folder={}",
                    message_id,
                    uid,
                    folder_path
                );

                let body = match conn_imap.fetch_message_body(*uid) {
                    Ok(Some(b)) => b,
                    Ok(None) => {
                        log::warn!(
                            "Prefetch: no body returned for uid={} in folder '{}'",
                            uid,
                            folder_path
                        );
                        continue;
                    }
                    Err(e) => {
                        log::error!(
                            "Prefetch: failed to fetch body for uid={} in folder '{}': {}",
                            uid,
                            folder_path,
                            e
                        );
                        continue;
                    }
                };

                // Parse flags from JSON to compute Maildir suffix
                let flags: Vec<String> =
                    serde_json::from_str(flags_json).unwrap_or_default();

                // Write body to Maildir
                let sanitized_folder = mail_sync::sanitize_folder_name(folder_path);
                let maildir_base = data_dir.join(&account_id).join(&sanitized_folder);
                if let Err(e) = mail_sync::create_maildir_dirs(&maildir_base) {
                    log::error!(
                        "Prefetch: failed to create maildir dirs for '{}': {}",
                        maildir_base.display(),
                        e
                    );
                    continue;
                }

                let suffix = mail_sync::flags_to_maildir_suffix(&flags);
                let filename = format!("{}:2,{}", uid, suffix);
                let msg_path = maildir_base.join("cur").join(&filename);

                if let Err(e) = std::fs::write(&msg_path, &body) {
                    log::error!(
                        "Prefetch: failed to write body to '{}': {}",
                        msg_path.display(),
                        e
                    );
                    continue;
                }

                let relative_path = format!(
                    "{}/{}/cur/{}",
                    account_id, sanitized_folder, filename
                );

                log::debug!(
                    "Prefetch: body saved {} ({} bytes)",
                    relative_path,
                    body.len()
                );

                // Update DB with the maildir path
                let rt = tokio::runtime::Handle::current();
                let conn = rt.block_on(db.lock());
                if let Err(e) = db::messages::update_maildir_path(&conn, message_id, &relative_path) {
                    log::error!(
                        "Prefetch: failed to update maildir_path for message_id={}: {}",
                        message_id,
                        e
                    );
                    continue;
                }

                count += 1;
            }
        }

        conn_imap.logout();
        log::info!(
            "Prefetch: completed for account {}, {} bodies fetched",
            account_id,
            count
        );
        Ok(count)
    })
    .await
    .map_err(|e| Error::Sync(format!("Prefetch task panicked: {}", e)))??;

    Ok(fetched_count)
}
