use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};

use crate::commands::events::{emit_folders_changed, emit_messages_changed};
use crate::db;
use crate::db::pool::DbPool;
use crate::error::{Error, Result};
use crate::filters::engine::{self, AddressEntry, MessageData};
use crate::filters::rules::FilterAction;
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

/// Sync all folders for an account. If `current_folder` is set, sync it first.
pub async fn sync_account(
    app: AppHandle,
    db: Arc<DbPool>,
    data_dir: PathBuf,
    account_id: String,
    account_name: String,
    imap_config: ImapConfig,
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

    let app_clone = app.clone();
    let account_id_clone = account_id.clone();

    let current_folder_clone = current_folder;
    let result = tokio::task::spawn_blocking(move || {
        sync_account_blocking(
            &app_clone,
            db,
            &data_dir,
            &account_id_clone,
            &imap_config,
            current_folder_clone.as_deref(),
        )
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

/// Maximum number of concurrent IMAP connections for parallel folder sync.
const MAX_PARALLEL_CONNECTIONS: usize = 4;

fn sync_account_blocking(
    app: &AppHandle,
    db: Arc<DbPool>,
    data_dir: &Path,
    account_id: &str,
    imap_config: &ImapConfig,
    current_folder: Option<&str>,
) -> Result<u32> {
    let mut conn_imap = ImapConnection::connect(imap_config)?;

    let imap_folders = conn_imap.list_folders()?;

    // Update folders in DB
    {
        let rt = tokio::runtime::Handle::current();
        let conn = rt.block_on(db.writer());
        for (display_name, path) in &imap_folders {
            let folder_type = db::folders::guess_folder_type(display_name)
                .or_else(|| db::folders::guess_folder_type(path));
            db::folders::upsert_folder(&conn, account_id, display_name, path, folder_type, None)?;
        }
    }

    // Gmail virtual folders that duplicate content — skip envelope sync
    // but keep them in the DB (they're needed for move/delete operations).
    // [Gmail]/All Mail contains every email, [Gmail]/Important is a virtual view.
    // [Gmail] itself is not a selectable mailbox.
    let skip_sync_folders: &[&str] = &["[Gmail]/All Mail", "[Gmail]/Important", "[Gmail]"];

    // Sync order: priority folders first (current, INBOX), then rest in parallel.
    // Folders in skip_sync_folders are registered in DB (above) but not synced.
    let mut priority: Vec<String> = Vec::new();
    let mut others: Vec<String> = Vec::new();
    for (_, path) in &imap_folders {
        if skip_sync_folders
            .iter()
            .any(|skip| path.eq_ignore_ascii_case(skip))
        {
            log::debug!("Skipping envelope sync for Gmail virtual folder: {}", path);
            continue;
        }
        if current_folder
            .map(|cf| cf == path.as_str())
            .unwrap_or(false)
        {
            priority.insert(0, path.clone());
        } else if path.to_uppercase() == "INBOX" {
            priority.push(path.clone());
        } else {
            others.push(path.clone());
        }
    }

    let total_folders = priority.len() + others.len();
    let mut grand_total = 0u32;

    // Phase 1: Sync priority folders sequentially on the existing connection
    // (current folder + INBOX first so the UI is usable quickly)
    for (i, folder) in priority.iter().enumerate() {
        app.emit(
            "sync-progress",
            SyncProgress {
                account_id: account_id.to_string(),
                folder: folder.clone(),
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
                }
                app.emit(
                    "sync-progress",
                    SyncProgress {
                        account_id: account_id.to_string(),
                        folder: folder.clone(),
                        synced: count,
                        total_folders,
                        current_folder: i + 1,
                    },
                )
                .ok();
            }
            Err(e) => log::error!("Error syncing {}: {}", folder, e),
        }
    }

    conn_imap.logout();

    // Phase 2: Sync remaining folders in parallel with multiple connections.
    if !others.is_empty() {
        let parallel_count = MAX_PARALLEL_CONNECTIONS.min(others.len());
        log::info!(
            "Parallel sync: {} remaining folders with {} connections",
            others.len(),
            parallel_count
        );

        let mut thread_folders: Vec<Vec<String>> =
            (0..parallel_count).map(|_| Vec::new()).collect();
        for (i, folder) in others.iter().enumerate() {
            thread_folders[i % parallel_count].push(folder.clone());
        }

        let rt_handle = tokio::runtime::Handle::current();
        let priority_count = priority.len();
        let results: Vec<Result<u32>> = std::thread::scope(|s| {
            let handles: Vec<_> = thread_folders
                .into_iter()
                .enumerate()
                .map(|(thread_idx, folders)| {
                    let db = db.clone();
                    let imap_config = imap_config.clone();
                    let account_id = account_id.to_string();
                    let app = app.clone();
                    let rt = rt_handle.clone();
                    s.spawn(move || {
                        // Enter the Tokio runtime context for db.writer() calls inside sync
                        let _guard = rt.enter();
                        let mut conn = match ImapConnection::connect(&imap_config) {
                            Ok(c) => c,
                            Err(e) => {
                                log::error!(
                                    "Parallel sync thread {}: connect failed: {}",
                                    thread_idx,
                                    e
                                );
                                return Err(e);
                            }
                        };
                        let mut thread_total = 0u32;
                        for folder in &folders {
                            let folder_idx = priority_count
                                + thread_idx
                                + (folders.iter().position(|f| f == folder).unwrap_or(0)
                                    * parallel_count);
                            app.emit(
                                "sync-progress",
                                SyncProgress {
                                    account_id: account_id.clone(),
                                    folder: folder.clone(),
                                    synced: 0,
                                    total_folders,
                                    current_folder: folder_idx + 1,
                                },
                            )
                            .ok();
                            match sync_folder_envelopes(&db, &account_id, &mut conn, folder) {
                                Ok(count) => {
                                    thread_total += count;
                                    if count > 0 {
                                        log::info!(
                                            "Parallel sync: {} envelopes in {}",
                                            count,
                                            folder
                                        );
                                    }
                                }
                                Err(e) => {
                                    log::warn!("Parallel sync: skipping folder '{}': {}", folder, e)
                                }
                            }
                        }
                        conn.logout();
                        Ok(thread_total)
                    })
                })
                .collect();

            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or(Err(Error::Sync("Thread panicked".into())))
                })
                .collect()
        });

        for count in results.into_iter().flatten() {
            grand_total += count;
        }
    }

    // Ensure maildir base dirs exist for on-demand body fetching later
    let _ = std::fs::create_dir_all(data_dir.join(account_id));

    Ok(grand_total)
}

/// Sync a folder by fetching envelopes only (no message bodies).
/// Bodies are fetched on-demand when the user opens a message.
/// After syncing, runs filter rules on any newly synced messages.
/// Public entry point for single-folder sync from commands.
pub fn sync_folder_envelopes_public(
    db: &Arc<DbPool>,
    account_id: &str,
    conn_imap: &mut ImapConnection,
    folder_path: &str,
) -> Result<u32> {
    sync_folder_envelopes(db, account_id, conn_imap, folder_path)
}

fn sync_folder_envelopes(
    db: &Arc<DbPool>,
    account_id: &str,
    conn_imap: &mut ImapConnection,
    folder_path: &str,
) -> Result<u32> {
    let (last_uid, stored_uid_next, stored_total) = {
        let conn = db.reader();
        let last_uid = db::folders::get_last_seen_uid(&conn, account_id, folder_path)?;
        let (uid_next, total) = db::folders::get_folder_sync_state(&conn, account_id, folder_path)?;
        (last_uid, uid_next, total)
    };

    let (exists, _uid_validity, uid_next) = conn_imap.select_folder(folder_path)?;

    // Preflight: if UIDNEXT and EXISTS haven't changed since last sync, the
    // folder is unchanged — skip deletion reconciliation, flag sync, and
    // envelope fetch entirely. Most folders are dormant, so this skips ~80%
    // of folders on a typical sync cycle.
    if last_uid > 0
        && stored_uid_next > 0
        && uid_next == stored_uid_next
        && exists as i64 == stored_total
    {
        log::debug!(
            "Folder '{}' unchanged (uidnext={}, exists={}), skipping",
            folder_path,
            uid_next,
            exists
        );
        return Ok(0);
    }

    // Reconcile deletions — skip on first sync (last_uid == 0) since the local DB is empty.
    if last_uid > 0 {
        let all_server_uids = conn_imap.fetch_uids(0)?;
        let server_uid_set: std::collections::HashSet<u32> = all_server_uids.into_iter().collect();

        let rt = tokio::runtime::Handle::current();
        let conn = rt.block_on(db.writer());

        let mut stmt = conn
            .prepare(
                "SELECT id, uid FROM messages WHERE account_id = ?1 AND folder_path = ?2 AND uid > 0",
            )
            .map_err(Error::Database)?;
        let local_msgs: Vec<(String, u32)> = stmt
            .query_map(rusqlite::params![account_id, folder_path], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(Error::Database)?
            .filter_map(|r| r.ok())
            .collect();

        let mut deleted = 0u32;
        for (local_id, uid) in &local_msgs {
            if !server_uid_set.contains(uid) {
                conn.execute(
                    "DELETE FROM messages WHERE id = ?1",
                    rusqlite::params![local_id],
                )
                .ok();
                deleted += 1;
            }
        }
        if deleted > 0 {
            log::info!(
                "Removed {} server-deleted messages from '{}'",
                deleted,
                folder_path
            );
        }
    }

    // Sync flag changes from server (e.g., read/unread toggled on webmail)
    if last_uid > 0 {
        match conn_imap.fetch_all_flags() {
            Ok(server_flags) => {
                let uid_flags: Vec<(u32, String)> = server_flags
                    .into_iter()
                    .map(|(uid, flags)| (uid, serde_json::to_string(&flags).unwrap_or_default()))
                    .collect();
                let rt = tokio::runtime::Handle::current();
                let conn = rt.block_on(db.writer());
                match db::messages::sync_flags_by_uid(&conn, account_id, folder_path, &uid_flags) {
                    Ok(changed) if changed > 0 => {
                        log::info!("Updated flags on {} messages in '{}'", changed, folder_path);
                    }
                    Err(e) => {
                        log::warn!("Flag sync failed for '{}': {}", folder_path, e);
                    }
                    _ => {}
                }
            }
            Err(e) => {
                log::warn!("Failed to fetch flags for '{}': {}", folder_path, e);
            }
        }
    }

    let mut new_uids = conn_imap.fetch_uids(last_uid)?;

    if new_uids.is_empty() {
        let rt = tokio::runtime::Handle::current();
        let conn = rt.block_on(db.writer());
        let page = db::messages::get_messages(
            &conn,
            account_id,
            folder_path,
            0,
            1,
            "date",
            false,
            &Default::default(),
        )?;
        let unread = count_unread(&conn, account_id, folder_path)?;
        db::folders::update_folder_counts(&conn, account_id, folder_path, unread, page.total)?;
        if uid_next > 0 {
            db::folders::update_uid_next(&conn, account_id, folder_path, uid_next)?;
        }
        return Ok(0);
    }

    new_uids.sort_unstable_by(|a, b| b.cmp(a));

    log::info!(
        "Found {} new messages in {} for account {}",
        new_uids.len(),
        folder_path,
        account_id
    );

    // Pre-load existing UIDs for this folder into a HashSet for fast existence check.
    // This replaces 1 SELECT per message with 1 bulk query.
    let existing_uids = {
        let conn = db.reader();
        db::messages::get_existing_uids(&conn, account_id, folder_path)?
    };

    let mut total_synced = 0u32;
    let mut new_message_ids: Vec<String> = Vec::new();

    for chunk in new_uids.chunks(1000) {
        let envelopes = conn_imap.fetch_envelopes_batch(chunk)?;

        let rt = tokio::runtime::Handle::current();
        let conn = rt.block_on(db.writer());

        conn.execute_batch("BEGIN")?;

        for env in &envelopes {
            // In-memory existence check instead of per-message DB query
            if existing_uids.contains(&env.uid) {
                continue;
            }

            let date = env
                .date
                .as_ref()
                .and_then(|d| parse_imap_date(d))
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

            let snippet = env
                .subject
                .as_deref()
                .map(|s| s.chars().take(200).collect());

            let id = format!("{}_{}_{}", account_id, folder_path, env.uid);

            // Thread computation still needs DB queries for cross-references,
            // but the in_reply_to lookup is fast with index.
            let thread_id = db::messages::compute_thread_id(
                &conn,
                account_id,
                env.message_id.as_deref(),
                env.in_reply_to.as_deref(),
                env.subject.as_deref(),
            );

            let new_msg = db::messages::NewMessage {
                id: id.clone(),
                account_id: account_id.to_string(),
                folder_path: folder_path.to_string(),
                uid: env.uid,
                message_id: env.message_id.clone(),
                in_reply_to: env.in_reply_to.clone(),
                thread_id,
                subject: env.subject.clone(),
                from_name: env.from_name.clone(),
                from_email: env
                    .from_email
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                to_addresses: env.to_addresses.clone(),
                cc_addresses: env.cc_addresses.clone(),
                date,
                size: env.size,
                has_attachments: env.has_attachments,
                is_encrypted: false,
                is_signed: false,
                flags: serde_json::to_string(&env.flags).unwrap_or_default(),
                maildir_path: String::new(),
                snippet,
            };
            db::messages::insert_message(&conn, &new_msg)?;
            new_message_ids.push(id);
            total_synced += 1;
        }

        if let Some(&max_uid) = chunk.iter().max() {
            db::folders::update_last_seen_uid(&conn, account_id, folder_path, max_uid)?;
        }

        conn.execute_batch("COMMIT")?;
    }

    // Run filter rules on newly synced messages
    if !new_message_ids.is_empty() {
        match run_filters_on_new_messages(db, account_id, folder_path, &new_message_ids, conn_imap)
        {
            Ok(filtered) => {
                if filtered > 0 {
                    log::info!(
                        "Filters applied to {} of {} new messages in '{}'",
                        filtered,
                        new_message_ids.len(),
                        folder_path
                    );
                }
            }
            Err(e) => {
                log::error!(
                    "Error running filters on new messages in '{}': {}",
                    folder_path,
                    e
                );
                // Don't fail the sync if filters error out
            }
        }
    }

    // Update folder counts
    {
        let rt = tokio::runtime::Handle::current();
        let conn = rt.block_on(db.writer());
        let page = db::messages::get_messages(
            &conn,
            account_id,
            folder_path,
            0,
            1,
            "date",
            false,
            &Default::default(),
        )?;
        let unread = count_unread(&conn, account_id, folder_path)?;
        db::folders::update_folder_counts(&conn, account_id, folder_path, unread, page.total)?;
        // Store uid_next for preflight optimization on next sync
        if uid_next > 0 {
            db::folders::update_uid_next(&conn, account_id, folder_path, uid_next)?;
        }
    }

    Ok(total_synced)
}

/// Fetch and store the full body for a single message on-demand.
/// Called when the user opens a message that hasn't been downloaded yet.
pub fn fetch_and_store_body(
    imap_config: &ImapConfig,
    data_dir: &Path,
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

    // Write to Maildir — validate path components before creating directories
    let sanitized = sanitize_folder_name(folder_path);
    let maildir_base = data_dir.join(account_id).join(&sanitized);

    // Reject any remaining ".." or absolute components before touching the filesystem
    for component in maildir_base.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(Error::Other(format!(
                "Path traversal detected in maildir path: '{}'",
                maildir_base.display()
            )));
        }
    }

    create_maildir_dirs(&maildir_base)?;

    // Post-creation canonical check as defence-in-depth (catches symlink attacks)
    let canonical_data_dir =
        std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let canonical_maildir = std::fs::canonicalize(&maildir_base)
        .map_err(|e| Error::Other(format!("Failed to resolve maildir path: {}", e)))?;
    if !canonical_maildir.starts_with(&canonical_data_dir) {
        // Clean up the directory we just created since it's outside our tree
        let _ = std::fs::remove_dir_all(&canonical_maildir);
        return Err(Error::Other(format!(
            "Path traversal detected: maildir path '{}' escapes data directory",
            maildir_base.display()
        )));
    }

    let filename = format!("{}:2,{}", uid, flags_to_maildir_suffix(flags));
    let msg_path = maildir_base.join("cur").join(&filename);
    std::fs::write(&msg_path, &body)?;

    let relative_path = format!(
        "{}/{}/cur/{}",
        account_id,
        sanitize_folder_name(folder_path),
        filename
    );

    log::info!("Body saved: {} ({} bytes)", relative_path, body.len());

    Ok(relative_path)
}

pub(crate) fn create_maildir_dirs(base: &Path) -> Result<()> {
    std::fs::create_dir_all(base.join("cur"))?;
    std::fs::create_dir_all(base.join("new"))?;
    std::fs::create_dir_all(base.join("tmp"))?;
    Ok(())
}

pub(crate) fn sanitize_folder_name(name: &str) -> String {
    let normalized = name.replace('\\', "/").replace('\0', "");

    let sanitized: String = std::path::Path::new(&normalized)
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(part) => {
                let s = part.to_string_lossy().replace('.', "_");
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            }
            // Strip ., .., /, and prefix components
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(".");

    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        "_".to_string()
    } else {
        sanitized
    }
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

/// Run filter rules on newly synced messages.
///
/// Loads enabled filters for the account from DB, builds MessageData for each
/// new message, runs the filter engine, and executes any resulting IMAP actions
/// using the already-open connection. Returns the count of messages affected.
pub fn run_filters_on_new_messages(
    db: &Arc<DbPool>,
    account_id: &str,
    folder_path: &str,
    new_message_ids: &[String],
    imap_conn: &mut ImapConnection,
) -> Result<u32> {
    if new_message_ids.is_empty() {
        return Ok(0);
    }

    // Load enabled filter rules for this account
    let rules = {
        let conn = db.reader();
        let all_rules = db::filters::list_filters(&conn, Some(account_id))?;
        all_rules
            .into_iter()
            .filter(|r| r.enabled)
            .collect::<Vec<_>>()
    };

    if rules.is_empty() {
        return Ok(0);
    }

    log::info!(
        "Running {} filter rules on {} new messages in '{}'",
        rules.len(),
        new_message_ids.len(),
        folder_path
    );

    // Load message data for the new messages
    let messages = {
        let conn = db.reader();
        load_messages_by_ids(&conn, new_message_ids)?
    };

    // Evaluate filters for each message and collect actions
    let mut move_targets: std::collections::HashMap<String, Vec<u32>> =
        std::collections::HashMap::new();
    let mut copy_targets: std::collections::HashMap<String, Vec<u32>> =
        std::collections::HashMap::new();
    let mut delete_uids: Vec<u32> = Vec::new();
    let mut flag_add: std::collections::HashMap<String, Vec<u32>> =
        std::collections::HashMap::new();
    let mut flag_remove: std::collections::HashMap<String, Vec<u32>> =
        std::collections::HashMap::new();
    let mut mark_read_uids: Vec<u32> = Vec::new();
    let mut mark_unread_uids: Vec<u32> = Vec::new();
    let mut moved_ids: Vec<String> = Vec::new();
    let mut deleted_ids: Vec<String> = Vec::new();
    let mut affected = 0u32;

    for msg in &messages {
        let actions = engine::apply_filters(&rules, msg);
        if actions.is_empty() {
            continue;
        }
        affected += 1;

        for action in &actions {
            match action {
                FilterAction::Move { target } => {
                    move_targets
                        .entry(target.clone())
                        .or_default()
                        .push(msg.uid);
                    moved_ids.push(msg.id.clone());
                }
                FilterAction::Copy { target } => {
                    copy_targets
                        .entry(target.clone())
                        .or_default()
                        .push(msg.uid);
                }
                FilterAction::Delete => {
                    delete_uids.push(msg.uid);
                    deleted_ids.push(msg.id.clone());
                }
                FilterAction::Flag { value } => {
                    let flag = format!("\\{}", capitalize_first(value));
                    flag_add.entry(flag).or_default().push(msg.uid);
                }
                FilterAction::Unflag { value } => {
                    let flag = format!("\\{}", capitalize_first(value));
                    flag_remove.entry(flag).or_default().push(msg.uid);
                }
                FilterAction::MarkRead => {
                    mark_read_uids.push(msg.uid);
                }
                FilterAction::MarkUnread => {
                    mark_unread_uids.push(msg.uid);
                }
                FilterAction::Stop => {}
            }
        }
    }

    if affected == 0 {
        return Ok(0);
    }

    // The folder should already be selected from sync, but re-select to be safe
    imap_conn.select_folder(folder_path)?;

    // Execute IMAP actions
    if !mark_read_uids.is_empty() {
        log::info!("Filter: marking {} messages as read", mark_read_uids.len());
        imap_conn.set_flags(&mark_read_uids, &["\\Seen"], true)?;
    }

    if !mark_unread_uids.is_empty() {
        log::info!(
            "Filter: marking {} messages as unread",
            mark_unread_uids.len()
        );
        imap_conn.set_flags(&mark_unread_uids, &["\\Seen"], false)?;
    }

    for (flag, uids) in &flag_add {
        log::info!("Filter: adding flag '{}' to {} messages", flag, uids.len());
        imap_conn.set_flags(uids, &[flag.as_str()], true)?;
    }

    for (flag, uids) in &flag_remove {
        log::info!(
            "Filter: removing flag '{}' from {} messages",
            flag,
            uids.len()
        );
        imap_conn.set_flags(uids, &[flag.as_str()], false)?;
    }

    for (target, uids) in &copy_targets {
        log::info!("Filter: copying {} messages to '{}'", uids.len(), target);
        imap_conn.copy_messages(uids, target)?;
    }

    for (target, uids) in &move_targets {
        log::info!("Filter: moving {} messages to '{}'", uids.len(), target);
        imap_conn.move_messages(uids, target)?;
    }

    // Delete messages not already moved
    let delete_remaining: Vec<u32> = delete_uids
        .iter()
        .filter(|uid| {
            !move_targets
                .values()
                .any(|moved_uids| moved_uids.contains(uid))
        })
        .copied()
        .collect();
    if !delete_remaining.is_empty() {
        log::info!("Filter: deleting {} messages", delete_remaining.len());
        imap_conn.delete_messages(&delete_remaining)?;
    }

    // Update local DB: remove moved/deleted messages
    {
        let rt = tokio::runtime::Handle::current();
        let conn = rt.block_on(db.writer());
        let mut to_remove = moved_ids;
        to_remove.extend(deleted_ids);
        to_remove.sort();
        to_remove.dedup();
        if !to_remove.is_empty() {
            log::info!(
                "Filter: removing {} moved/deleted messages from local DB",
                to_remove.len()
            );
            db::messages::delete_messages_by_ids(&conn, &to_remove)?;
        }
    }

    Ok(affected)
}

/// Load messages by their IDs for filter evaluation.
fn load_messages_by_ids(
    conn: &rusqlite::Connection,
    message_ids: &[String],
) -> Result<Vec<MessageData>> {
    if message_ids.is_empty() {
        return Ok(vec![]);
    }

    let placeholders: String = message_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(",");

    let query = format!(
        "SELECT id, uid, folder_path, from_name, from_email, to_addresses, cc_addresses, \
                subject, size, has_attachments, flags \
         FROM messages WHERE id IN ({})",
        placeholders
    );

    let mut stmt = conn.prepare(&query)?;

    let params: Vec<&dyn rusqlite::types::ToSql> = message_ids
        .iter()
        .map(|id| id as &dyn rusqlite::types::ToSql)
        .collect();

    let rows = stmt
        .query_map(params.as_slice(), |row| {
            let id: String = row.get(0)?;
            let uid: u32 = row.get(1)?;
            let folder: String = row.get(2)?;
            let from_name: Option<String> = row.get(3)?;
            let from_email: String = row.get(4)?;
            let to_json: String = row.get(5)?;
            let cc_json: String = row.get(6)?;
            let subject: Option<String> = row.get(7)?;
            let size: i64 = row.get(8)?;
            let has_attachments: bool = row.get(9)?;
            let flags_json: String = row.get(10)?;

            Ok((
                id,
                uid,
                folder,
                from_name,
                from_email,
                to_json,
                cc_json,
                subject,
                size,
                has_attachments,
                flags_json,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut messages = Vec::with_capacity(rows.len());
    for (
        id,
        uid,
        folder,
        from_name,
        from_email,
        to_json,
        cc_json,
        subject,
        size,
        has_attach,
        flags_json,
    ) in rows
    {
        let to_addresses: Vec<AddressEntry> = serde_json::from_str(&to_json).unwrap_or_default();
        let cc_addresses: Vec<AddressEntry> = serde_json::from_str(&cc_json).unwrap_or_default();
        let flags: Vec<String> = serde_json::from_str(&flags_json).unwrap_or_default();

        messages.push(MessageData {
            id,
            uid,
            folder_path: folder,
            from_name,
            from_email,
            to_addresses,
            cc_addresses,
            subject,
            size: size as u64,
            has_attachments: has_attach,
            flags,
        });
    }

    Ok(messages)
}

/// Capitalize the first letter of a string.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

/// Parse an IMAP date string (from ENVELOPE) into RFC 3339.
fn parse_imap_date(date_str: &str) -> Option<String> {
    // IMAP dates look like: "Thu, 3 Apr 2026 19:51:23 +0000"
    // Always convert to UTC for consistent sorting in SQLite.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(date_str) {
        return Some(dt.with_timezone(&chrono::Utc).to_rfc3339());
    }
    if let Ok(dt) = chrono::DateTime::parse_from_str(date_str, "%d %b %Y %H:%M:%S %z") {
        return Some(dt.with_timezone(&chrono::Utc).to_rfc3339());
    }
    log::debug!("Could not parse IMAP date: {}", date_str);
    None
}
