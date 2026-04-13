use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};

use crate::commands::events::{emit_folders_changed, emit_messages_changed};
use crate::db;
use crate::db::pool::DbPool;
use crate::error::{Error, Result};
use crate::mail::jmap::{JmapConfig, JmapConnection};

#[derive(Clone, serde::Serialize)]
struct SyncStarted {
    account_id: String,
    account_name: String,
}

#[derive(Clone, serde::Serialize)]
struct SyncProgress {
    account_id: String,
    folder: String,
    synced: u32,
    total_folders: usize,
    current_folder: usize,
}

#[derive(Clone, serde::Serialize)]
struct SyncComplete {
    account_id: String,
    total_synced: u32,
}

#[derive(Clone, serde::Serialize)]
struct SyncError {
    account_id: String,
    error: String,
}

/// Sync all folders for a JMAP account. This is the JMAP equivalent of
/// `mail::sync::sync_account` for IMAP.
pub async fn sync_jmap_account(
    app: AppHandle,
    db: Arc<DbPool>,
    _data_dir: PathBuf,
    account_id: String,
    account_name: String,
    jmap_config: JmapConfig,
    current_folder: Option<String>,
) -> Result<()> {
    app.emit(
        "sync-started",
        SyncStarted {
            account_id: account_id.clone(),
            account_name: account_name.clone(),
        },
    )
    .ok();

    let result =
        sync_jmap_account_inner(&app, &db, &account_id, &jmap_config, current_folder.as_deref())
            .await;

    match &result {
        Ok(total) => {
            app.emit(
                "sync-complete",
                SyncComplete {
                    account_id: account_id.clone(),
                    total_synced: *total,
                },
            )
            .ok();
            emit_folders_changed(&app, &account_id);
            emit_messages_changed(&app, &account_id);
        }
        Err(e) => {
            app.emit(
                "sync-error",
                SyncError {
                    account_id: account_id.clone(),
                    error: e.to_string(),
                },
            )
            .ok();
        }
    }

    result.map(|_| ())
}

async fn sync_jmap_account_inner(
    app: &AppHandle,
    db: &Arc<DbPool>,
    account_id: &str,
    jmap_config: &JmapConfig,
    current_folder: Option<&str>,
) -> Result<u32> {
    let conn_jmap = JmapConnection::connect(jmap_config).await?;

    // List and update mailboxes in DB
    let jmap_folders = conn_jmap.list_folders(jmap_config).await?;
    {
        let conn = db.writer().await;
        for (display_name, mailbox_id, folder_type, parent_id) in &jmap_folders {
            // For JMAP, we store the mailbox_id in the `path` column
            db::folders::upsert_folder(
                &conn,
                account_id,
                display_name,
                mailbox_id,
                *folder_type,
                parent_id.as_deref(),
            )?;
        }
    }

    // Determine sync order: current folder first, then Inbox, then rest
    let mut priority: Vec<(&str, &str)> = Vec::new();
    let mut others: Vec<(&str, &str)> = Vec::new();
    for (name, mailbox_id, folder_type, _parent_id) in &jmap_folders {
        if current_folder
            .map(|cf| cf == mailbox_id.as_str())
            .unwrap_or(false)
        {
            priority.insert(0, (name.as_str(), mailbox_id.as_str()));
        } else if *folder_type == Some("inbox") {
            priority.push((name.as_str(), mailbox_id.as_str()));
        } else {
            others.push((name.as_str(), mailbox_id.as_str()));
        }
    }
    let all_folders: Vec<(&str, &str)> = priority.into_iter().chain(others).collect();

    let total_folders = all_folders.len();
    let mut grand_total = 0u32;

    for (i, (folder_name, mailbox_id)) in all_folders.iter().enumerate() {
        app.emit(
            "sync-progress",
            SyncProgress {
                account_id: account_id.to_string(),
                folder: folder_name.to_string(),
                synced: 0,
                total_folders,
                current_folder: i + 1,
            },
        )
        .ok();

        match sync_jmap_folder(db, account_id, &conn_jmap, jmap_config, mailbox_id, folder_name).await {
            Ok(count) => {
                grand_total += count;
                if count > 0 {
                    log::info!("JMAP synced {} emails in {}", count, folder_name);
                    app.emit(
                        "sync-progress",
                        SyncProgress {
                            account_id: account_id.to_string(),
                            folder: folder_name.to_string(),
                            synced: count,
                            total_folders,
                            current_folder: i + 1,
                        },
                    )
                    .ok();
                }
            }
            Err(e) => log::error!("JMAP error syncing {}: {}", folder_name, e),
        }
    }

    Ok(grand_total)
}

/// Sync a single JMAP mailbox.
async fn sync_jmap_folder(
    db: &Arc<DbPool>,
    account_id: &str,
    conn_jmap: &JmapConnection,
    jmap_config: &JmapConfig,
    mailbox_id: &str,
    folder_name: &str,
) -> Result<u32> {
    // Get the stored JMAP state for this folder (for delta sync)
    let jmap_state = {
        let conn = db.reader();
        db::folders::get_jmap_state(&conn, account_id, mailbox_id)?
    };

    let (emails, new_state) = conn_jmap
        .fetch_emails(jmap_config, mailbox_id, jmap_state.as_deref())
        .await?;

    if emails.is_empty() {
        // Still update the state so we don't re-fetch next time
        if !new_state.is_empty() {
            let conn = db.writer().await;
            db::folders::update_jmap_state(&conn, account_id, mailbox_id, &new_state)?;
        }
        return Ok(0);
    }

    log::info!(
        "JMAP found {} new/updated emails in {} ({})",
        emails.len(),
        folder_name,
        mailbox_id
    );

    let mut total_synced = 0u32;

    {
        let conn = db.writer().await;

        for email in &emails {
            // Use the JMAP email ID as the unique identifier
            let id = format!("{}_{}_{}", account_id, mailbox_id, email.id);

            // Check if this message already exists (by JMAP ID in the folder)
            if jmap_message_exists(&conn, account_id, mailbox_id, &email.id)? {
                // Update flags in case they changed on the server (read/unread, flagged, etc.)
                let new_flags = serde_json::to_string(&email.flags).unwrap_or_default();
                let msg_id = format!("{}_{}_{}", account_id, mailbox_id, email.id);
                let _ = db::messages::update_flags(&conn, &msg_id, &new_flags);
                continue;
            }

            let snippet = email
                .preview
                .as_deref()
                .or(email.subject.as_deref())
                .map(|s| s.chars().take(200).collect());

            // Compute thread_id
            let thread_id = db::messages::compute_thread_id(
                &conn,
                account_id,
                email.message_id.as_deref(),
                email.in_reply_to.as_deref(),
                email.subject.as_deref(),
            );
            if let Some(ref tid) = thread_id {
                log::debug!(
                    "JMAP assigned thread_id '{}' to email {}",
                    tid,
                    email.id
                );
            }

            let new_msg = db::messages::NewMessage {
                id: id.clone(),
                account_id: account_id.to_string(),
                folder_path: mailbox_id.to_string(),
                uid: 0, // JMAP doesn't use UIDs; we store 0
                message_id: email.message_id.clone(),
                in_reply_to: email.in_reply_to.clone(),
                thread_id,
                subject: email.subject.clone(),
                from_name: email.from_name.clone(),
                from_email: email
                    .from_email
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                to_addresses: email.to_addresses.clone(),
                cc_addresses: email.cc_addresses.clone(),
                date: email.date.clone(),
                size: email.size,
                has_attachments: email.has_attachments,
                is_encrypted: false,
                is_signed: false,
                flags: serde_json::to_string(&email.flags).unwrap_or_default(),
                maildir_path: String::new(), // Body fetched on-demand
                snippet,
            };
            db::messages::insert_message(&conn, &new_msg)?;
            total_synced += 1;
        }

        // Update JMAP state for this folder
        if !new_state.is_empty() {
            db::folders::update_jmap_state(&conn, account_id, mailbox_id, &new_state)?;
        }
    }

    // Remove local messages that no longer exist on the server.
    // Build a set of JMAP email IDs from the server response.
    let server_ids: std::collections::HashSet<String> = emails.iter().map(|e| e.id.clone()).collect();
    {
        let conn = db.writer().await;
        // Get all local message IDs for this folder
        let mut stmt = conn.prepare(
            "SELECT id FROM messages WHERE account_id = ?1 AND folder_path = ?2"
        ).map_err(Error::Database)?;
        let local_ids: Vec<String> = stmt.query_map(
            rusqlite::params![account_id, mailbox_id],
            |row| row.get(0),
        ).map_err(Error::Database)?
        .filter_map(|r| r.ok())
        .collect();

        let _prefix = format!("{}_{}_{}", account_id, mailbox_id, "");
        let mut deleted = 0u32;
        for local_id in &local_ids {
            // Extract the JMAP email ID from the composite local ID
            let jmap_id = local_id.strip_prefix(&format!("{}_{}_", account_id, mailbox_id))
                .unwrap_or(local_id);
            if !server_ids.contains(jmap_id) {
                conn.execute(
                    "DELETE FROM messages WHERE id = ?1",
                    rusqlite::params![local_id],
                ).ok();
                deleted += 1;
            }
        }
        if deleted > 0 {
            log::info!("JMAP removed {} locally deleted messages from {}", deleted, folder_name);
        }
    }

    // Update folder counts
    {
        let conn = db.writer().await;
        let page =
            db::messages::get_messages(&conn, account_id, mailbox_id, 0, 1, "date", false, &Default::default())?;
        let unread = count_unread(&conn, account_id, mailbox_id)?;
        db::folders::update_folder_counts(&conn, account_id, mailbox_id, unread, page.total)?;
    }

    Ok(total_synced)
}

/// Check if a message with the given JMAP email ID already exists in a folder.
/// We store the JMAP ID as part of the composite message ID.
fn jmap_message_exists(
    conn: &rusqlite::Connection,
    account_id: &str,
    mailbox_id: &str,
    jmap_email_id: &str,
) -> Result<bool> {
    let id = format!("{}_{}_{}", account_id, mailbox_id, jmap_email_id);
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE id = ?1",
        rusqlite::params![id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn count_unread(conn: &rusqlite::Connection, account_id: &str, folder_path: &str) -> Result<i64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE account_id = ?1 AND folder_path = ?2 AND flags NOT LIKE '%seen%'",
        rusqlite::params![account_id, folder_path],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// Sync a single JMAP folder — public entry point for the `sync_folder` command.
pub async fn sync_jmap_folder_public(
    app: AppHandle,
    db: Arc<DbPool>,
    account_id: String,
    account_name: String,
    mailbox_id: String,
    jmap_config: JmapConfig,
) -> Result<u32> {
    app.emit(
        "sync-started",
        serde_json::json!({
            "account_id": account_id,
            "account_name": account_name,
        }),
    )
    .ok();

    let conn_jmap = JmapConnection::connect(&jmap_config).await?;
    let folder_name = mailbox_id.clone();
    let result = sync_jmap_folder(&db, &account_id, &conn_jmap, &jmap_config, &mailbox_id, &folder_name).await;

    match &result {
        Ok(count) => {
            app.emit(
                "sync-complete",
                serde_json::json!({
                    "account_id": account_id,
                    "total_synced": count,
                }),
            )
            .ok();
            emit_folders_changed(&app, &account_id);
            emit_messages_changed(&app, &account_id);
        }
        Err(e) => {
            app.emit(
                "sync-error",
                serde_json::json!({
                    "account_id": account_id,
                    "error": e.to_string(),
                }),
            )
            .ok();
        }
    }

    result
}

/// Fetch and store the body for a JMAP email on-demand.
/// Called when the user opens a message whose body hasn't been downloaded yet.
pub async fn fetch_and_store_jmap_body(
    jmap_config: &JmapConfig,
    data_dir: &std::path::Path,
    account_id: &str,
    folder_path: &str,
    jmap_email_id: &str,
    flags: &[String],
) -> Result<String> {
    use crate::mail::sync::{create_maildir_dirs, flags_to_maildir_suffix, sanitize_folder_name};

    log::info!(
        "JMAP on-demand body fetch: account={} folder={} jmap_id={}",
        account_id,
        folder_path,
        jmap_email_id
    );

    let conn_jmap = JmapConnection::connect(jmap_config).await?;
    let body = conn_jmap
        .fetch_email_body(jmap_config, jmap_email_id)
        .await?
        .ok_or_else(|| Error::Other(format!("JMAP no body returned for email {}", jmap_email_id)))?;

    // Write to Maildir structure for parsing
    let maildir_base = data_dir
        .join(account_id)
        .join(sanitize_folder_name(folder_path));
    create_maildir_dirs(&maildir_base)?;

    let filename = format!("{}:2,{}", jmap_email_id, flags_to_maildir_suffix(flags));
    let msg_path = maildir_base.join("cur").join(&filename);
    std::fs::write(&msg_path, &body)?;

    let relative_path = format!(
        "{}/{}/cur/{}",
        account_id,
        sanitize_folder_name(folder_path),
        filename
    );

    log::info!(
        "JMAP body saved: {} ({} bytes)",
        relative_path,
        body.len()
    );

    Ok(relative_path)
}
