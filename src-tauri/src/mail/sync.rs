use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

use crate::db;
use crate::error::{Error, Result};
use crate::mail::imap::{ImapConfig, ImapConnection};

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

/// Sync all folders for an account.
pub async fn sync_account(
    app: AppHandle,
    db: Arc<Mutex<rusqlite::Connection>>,
    data_dir: PathBuf,
    account_id: String,
    account_name: String,
    imap_config: ImapConfig,
) -> Result<()> {
    app.emit(
        "sync-started",
        SyncStarted {
            account_id: account_id.clone(),
            account_name: account_name.clone(),
        },
    )
    .ok();

    let app_clone = app.clone();
    let account_id_clone = account_id.clone();

    let result = tokio::task::spawn_blocking(move || {
        sync_account_blocking(&app_clone, db, &data_dir, &account_id_clone, &imap_config)
    })
    .await
    .map_err(|e| Error::Sync(format!("Sync task panicked: {}", e)))?;

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

fn sync_account_blocking(
    app: &AppHandle,
    db: Arc<Mutex<rusqlite::Connection>>,
    data_dir: &PathBuf,
    account_id: &str,
    imap_config: &ImapConfig,
) -> Result<u32> {
    let mut conn_imap = ImapConnection::connect(imap_config)?;

    let imap_folders = conn_imap.list_folders()?;

    // Update folders in DB
    {
        let rt = tokio::runtime::Handle::current();
        let conn = rt.block_on(db.lock());
        for (display_name, path) in &imap_folders {
            let folder_type = db::folders::guess_folder_type(display_name)
                .or_else(|| db::folders::guess_folder_type(path));
            db::folders::upsert_folder(&conn, account_id, display_name, path, folder_type)?;
        }
    }

    // Sync INBOX first, then others
    let mut all_folders: Vec<&str> = Vec::new();
    let mut others: Vec<&str> = Vec::new();
    for (_, path) in &imap_folders {
        if path.to_uppercase() == "INBOX" {
            all_folders.push(path.as_str());
        } else {
            others.push(path.as_str());
        }
    }
    all_folders.extend(others);

    let total_folders = all_folders.len();
    let mut grand_total = 0u32;

    for (i, folder) in all_folders.iter().enumerate() {
        app.emit(
            "sync-progress",
            SyncProgress {
                account_id: account_id.to_string(),
                folder: folder.to_string(),
                synced: 0,
                total_folders,
                current_folder: i + 1,
            },
        )
        .ok();

        match sync_folder_envelopes(&db, account_id, &mut conn_imap, folder) {
            Ok(count) => {
                grand_total += count;
                if count > 0 {
                    log::info!("Synced {} envelopes in {}", count, folder);
                    app.emit(
                        "sync-progress",
                        SyncProgress {
                            account_id: account_id.to_string(),
                            folder: folder.to_string(),
                            synced: count,
                            total_folders,
                            current_folder: i + 1,
                        },
                    )
                    .ok();
                }
            }
            Err(e) => log::error!("Error syncing {}: {}", folder, e),
        }
    }

    // Ensure maildir base dirs exist for on-demand body fetching later
    let _ = std::fs::create_dir_all(data_dir.join(account_id));

    conn_imap.logout();
    Ok(grand_total)
}

/// Sync a folder by fetching envelopes only (no message bodies).
/// Bodies are fetched on-demand when the user opens a message.
fn sync_folder_envelopes(
    db: &Arc<Mutex<rusqlite::Connection>>,
    account_id: &str,
    conn_imap: &mut ImapConnection,
    folder_path: &str,
) -> Result<u32> {
    let last_uid = {
        let rt = tokio::runtime::Handle::current();
        let conn = rt.block_on(db.lock());
        db::folders::get_last_seen_uid(&conn, account_id, folder_path)?
    };

    conn_imap.select_folder(folder_path)?;
    let mut new_uids = conn_imap.fetch_uids(last_uid)?;

    if new_uids.is_empty() {
        return Ok(0);
    }

    // Sort descending so newest messages are fetched first
    new_uids.sort_unstable_by(|a, b| b.cmp(a));

    log::info!(
        "Found {} new messages in {} for account {}",
        new_uids.len(),
        folder_path,
        account_id
    );

    let mut total_synced = 0u32;

    // Fetch envelopes in batches of 500 (envelopes are tiny, ~200 bytes each)
    for chunk in new_uids.chunks(500) {
        let envelopes = conn_imap.fetch_envelopes_batch(chunk)?;

        let rt = tokio::runtime::Handle::current();
        let conn = rt.block_on(db.lock());

        for env in &envelopes {
            if db::messages::message_exists(&conn, account_id, folder_path, env.uid)? {
                continue;
            }

            // Parse the envelope date or fallback to now
            let date = env
                .date
                .as_ref()
                .and_then(|d| parse_imap_date(d))
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

            let snippet = env.subject.as_deref().map(|s| s.chars().take(200).collect());

            let id = format!("{}_{}_{}", account_id, folder_path, env.uid);

            let new_msg = db::messages::NewMessage {
                id,
                account_id: account_id.to_string(),
                folder_path: folder_path.to_string(),
                uid: env.uid,
                message_id: env.message_id.clone(),
                in_reply_to: env.in_reply_to.clone(),
                subject: env.subject.clone(),
                from_name: env.from_name.clone(),
                from_email: env.from_email.clone().unwrap_or_else(|| "unknown".to_string()),
                to_addresses: env.to_addresses.clone(),
                cc_addresses: env.cc_addresses.clone(),
                date,
                size: env.size,
                has_attachments: env.has_attachments,
                is_encrypted: false,
                is_signed: false,
                flags: serde_json::to_string(&env.flags).unwrap_or_default(),
                maildir_path: String::new(), // Body not downloaded yet
                snippet,
            };
            db::messages::insert_message(&conn, &new_msg)?;
            total_synced += 1;
        }

        // Update last seen UID (use max of all UIDs in this chunk)
        if let Some(&max_uid) = chunk.iter().max() {
            db::folders::update_last_seen_uid(&conn, account_id, folder_path, max_uid)?;
        }
    }

    // Update folder counts
    {
        let rt = tokio::runtime::Handle::current();
        let conn = rt.block_on(db.lock());
        let page =
            db::messages::get_messages(&conn, account_id, folder_path, 0, 1, "date", false)?;
        let unread = count_unread(&conn, account_id, folder_path)?;
        db::folders::update_folder_counts(&conn, account_id, folder_path, unread, page.total)?;
    }

    Ok(total_synced)
}

/// Fetch and store the full body for a single message on-demand.
/// Called when the user opens a message that hasn't been downloaded yet.
pub fn fetch_and_store_body(
    imap_config: &ImapConfig,
    data_dir: &PathBuf,
    account_id: &str,
    folder_path: &str,
    uid: u32,
    flags: &[String],
) -> Result<String> {
    log::info!(
        "On-demand body fetch: account={} folder={} uid={}",
        account_id,
        folder_path,
        uid
    );

    let mut conn_imap = ImapConnection::connect(imap_config)?;
    conn_imap.select_folder(folder_path)?;
    let body = conn_imap
        .fetch_message_body(uid)?
        .ok_or_else(|| Error::Imap(format!("No body returned for UID {}", uid)))?;
    conn_imap.logout();

    // Write to Maildir
    let maildir_base = data_dir
        .join(account_id)
        .join(sanitize_folder_name(folder_path));
    create_maildir_dirs(&maildir_base)?;

    let filename = format!("{}:2,{}", uid, flags_to_maildir_suffix(flags));
    let msg_path = maildir_base.join("cur").join(&filename);
    std::fs::write(&msg_path, &body)?;

    let relative_path = format!(
        "{}/{}/cur/{}",
        account_id,
        sanitize_folder_name(folder_path),
        filename
    );

    log::info!(
        "Body saved: {} ({} bytes)",
        relative_path,
        body.len()
    );

    Ok(relative_path)
}

pub(crate) fn create_maildir_dirs(base: &PathBuf) -> Result<()> {
    std::fs::create_dir_all(base.join("cur"))?;
    std::fs::create_dir_all(base.join("new"))?;
    std::fs::create_dir_all(base.join("tmp"))?;
    Ok(())
}

pub(crate) fn sanitize_folder_name(name: &str) -> String {
    name.replace('/', ".")
        .replace('\\', ".")
        .replace('\0', "")
}

pub(crate) fn flags_to_maildir_suffix(flags: &[String]) -> String {
    let mut chars: Vec<char> = flags
        .iter()
        .filter_map(|f| match f.as_str() {
            "draft" => Some('D'),
            "flagged" => Some('F'),
            "answered" => Some('R'),
            "seen" => Some('S'),
            "deleted" => Some('T'),
            _ => None,
        })
        .collect();
    chars.sort();
    chars.into_iter().collect()
}

fn count_unread(conn: &rusqlite::Connection, account_id: &str, folder_path: &str) -> Result<i64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE account_id = ?1 AND folder_path = ?2 AND flags NOT LIKE '%seen%'",
        rusqlite::params![account_id, folder_path],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// Parse an IMAP date string (from ENVELOPE) into RFC 3339.
fn parse_imap_date(date_str: &str) -> Option<String> {
    // IMAP dates look like: "Thu, 3 Apr 2026 19:51:23 +0000"
    // Try chrono parsing with common formats
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(date_str) {
        return Some(dt.to_rfc3339());
    }
    // Try without day-of-week
    if let Ok(dt) = chrono::DateTime::parse_from_str(date_str, "%d %b %Y %H:%M:%S %z") {
        return Some(dt.to_rfc3339());
    }
    log::debug!("Could not parse IMAP date: {}", date_str);
    None
}
