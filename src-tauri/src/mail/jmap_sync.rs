use std::path::PathBuf;
use std::sync::Arc;

use crate::db;
use crate::db::pool::DbPool;
use crate::error::{Error, Result};
use crate::event::{
    ApplicationEvent, EventSink, SharedEventSink, SyncComplete, SyncError, SyncProgress,
    SyncStarted,
};
use crate::mail::compat::BackendMessageRef;
use crate::mail::jmap::{JmapConfig, JmapConnection};

/// Sync all folders for a JMAP account. This is the JMAP equivalent of
/// `mail::sync::sync_account` for IMAP.
pub async fn sync_jmap_account(
    events: SharedEventSink,
    db: Arc<DbPool>,
    _data_dir: PathBuf,
    account_id: String,
    account_name: String,
    jmap_config: JmapConfig,
    conn_jmap: JmapConnection,
    current_folder: Option<String>,
) -> Result<()> {
    events.publish(ApplicationEvent::SyncStarted(SyncStarted {
        account_id: account_id.clone(),
        account_name: account_name.clone(),
    }));

    let result = sync_jmap_account_inner(
        events.as_ref(),
        &db,
        &account_id,
        &jmap_config,
        &conn_jmap,
        current_folder.as_deref(),
    )
    .await;

    match &result {
        Ok(total) => {
            events.publish(ApplicationEvent::SyncComplete(SyncComplete {
                account_id: account_id.clone(),
                total_synced: *total,
            }));
            events.publish(ApplicationEvent::FoldersChanged(account_id.clone()));
            events.publish(ApplicationEvent::MessagesChanged(account_id.clone()));
        }
        Err(e) => {
            events.publish(ApplicationEvent::SyncError(SyncError {
                account_id: account_id.clone(),
                error: e.to_string(),
            }));
        }
    }

    result.map(|_| ())
}

async fn sync_jmap_account_inner(
    events: &dyn EventSink,
    db: &Arc<DbPool>,
    account_id: &str,
    jmap_config: &JmapConfig,
    conn_jmap: &JmapConnection,
    current_folder: Option<&str>,
) -> Result<u32> {
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
        events.publish(ApplicationEvent::SyncProgress(SyncProgress {
            account_id: account_id.to_string(),
            folder: folder_name.to_string(),
            synced: 0,
            total_folders,
            current_folder: i + 1,
        }));

        match sync_jmap_folder(
            db,
            account_id,
            conn_jmap,
            jmap_config,
            mailbox_id,
            folder_name,
        )
        .await
        {
            Ok(count) => {
                grand_total += count;
                if count > 0 {
                    log::info!("JMAP synced {} emails in {}", count, folder_name);
                    events.publish(ApplicationEvent::SyncProgress(SyncProgress {
                        account_id: account_id.to_string(),
                        folder: folder_name.to_string(),
                        synced: count,
                        total_folders,
                        current_folder: i + 1,
                    }));
                }
            }
            Err(e) => log::error!("JMAP error syncing {}: {}", folder_name, e),
        }
    }

    Ok(grand_total)
}

/// Sync a single JMAP mailbox.
///
/// Known per-folder redundancy
/// ---------------------------
/// `fetch_emails` (below) calls `Email/changes` against the per-folder
/// stored state. `Email/changes` itself is account-wide — it returns
/// the same created/updated/destroyed lists regardless of which
/// mailbox you're "syncing". When `sync_jmap_account_inner` iterates
/// every folder and calls `sync_jmap_folder` for each, the same delta
/// window is fetched and parsed once per folder, then filtered
/// per-mailbox by the loop above. For an account with N folders all
/// at the same JMAP state this is N times the network traffic and
/// JSON parsing of the minimum needed.
///
/// We accept this redundancy in the current implementation because:
///   * per-folder state lets the user trigger "sync only Inbox" (the
///     right-click sync action) without advancing the global cursor
///     and silently dropping changes that other folders still need;
///   * folders can legitimately have different states (e.g. a freshly
///     created folder starts with no state and gets a full scan
///     while the inbox keeps its delta cursor);
///   * a single Email/changes call is fast enough on Fastmail /
///     Stalwart that the wasted work is not visible in practice.
///
/// The proper fix is an account-level Email/changes step that fans
/// out per-folder, with a single per-account state column. That's a
/// schema + sync-orchestration change tracked separately, not in the
/// Fastmail-support PR that introduced delta sync.
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

    let fetch = conn_jmap
        .fetch_emails(jmap_config, mailbox_id, jmap_state.as_deref())
        .await?;
    let crate::mail::jmap::JmapFetchResult {
        emails,
        destroyed,
        state: new_state,
        is_full,
    } = fetch;

    // Delta path only: an empty result with no destroyed IDs is a true
    // no-op — persist the advanced state and skip the rest.
    //
    // Full path must NOT take this early return even when emails is
    // empty: the deletion reconciliation below compares the server set
    // against local rows, and a server-side empty mailbox means every
    // local row is stale. Returning here would leave those orphans
    // behind on first-sync-after-purge or after a state-expiry
    // fallback that returns 0 messages.
    if !is_full && emails.is_empty() && destroyed.is_empty() {
        if !new_state.is_empty() {
            let conn = db.writer().await;
            db::folders::update_jmap_state(&conn, account_id, mailbox_id, &new_state)?;
        }
        return Ok(0);
    }

    // Delta sync returns account-wide changes (Email/changes is global),
    // so filter to just the messages whose mailboxIds contain this folder.
    // For messages that USED to be in this folder but no longer are (moved
    // out), drop the local row. Full sync uses Email/query with an
    // `inMailbox` filter, so its result set is already per-mailbox — skip
    // the filtering in that case and trust the server.
    let mut moved_out: Vec<String> = Vec::new();
    let filtered_emails: Vec<&crate::mail::jmap::JmapEmail> = if is_full {
        emails.iter().collect()
    } else {
        emails
            .iter()
            .filter(|e| {
                if e.mailbox_ids.iter().any(|m| m == mailbox_id) {
                    true
                } else {
                    // Email is no longer in this mailbox (or never was).
                    // Schedule a delete of its row in this folder; the row
                    // may not exist, in which case the DELETE is a no-op.
                    moved_out.push(e.id.clone());
                    false
                }
            })
            .collect()
    };

    log::info!(
        "JMAP found {} new/updated emails in {} ({}){}",
        filtered_emails.len(),
        folder_name,
        mailbox_id,
        if !is_full && filtered_emails.len() < emails.len() {
            format!(
                " ({} skipped — not in this mailbox)",
                emails.len() - filtered_emails.len()
            )
        } else {
            String::new()
        },
    );

    let mut total_synced = 0u32;
    let mut new_ids: Vec<String> = Vec::new();

    {
        let conn = db.writer().await;

        for email in &filtered_emails {
            // Use the JMAP email ID as the unique identifier
            let message_ref = BackendMessageRef::jmap(mailbox_id, &email.id);
            let id = message_ref.to_db_id(account_id);

            // Check if this message already exists (by JMAP ID in the folder)
            if jmap_message_exists(&conn, account_id, mailbox_id, &email.id)? {
                // Update flags in case they changed on the server (read/unread, flagged, etc.)
                let new_flags = serde_json::to_string(&email.flags).unwrap_or_default();
                let msg_id = message_ref.to_db_id(account_id);
                let _ = db::messages::update_flags(&conn, &msg_id, &new_flags);
                continue;
            }

            let snippet = email
                .preview
                .as_deref()
                .or(email.subject.as_deref())
                .map(|s| s.chars().take(200).collect());

            // Compute thread_id
            let refs_slice: Option<&[String]> = if email.references.is_empty() {
                None
            } else {
                Some(email.references.as_slice())
            };
            let thread_id = db::messages::compute_thread_id(
                &conn,
                account_id,
                email.message_id.as_deref(),
                email.in_reply_to.as_deref(),
                email.subject.as_deref(),
                refs_slice,
            );
            if let Some(ref tid) = thread_id {
                log::debug!("JMAP assigned thread_id '{}' to email {}", tid, email.id);
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
            new_ids.push(id);
            total_synced += 1;
        }

        // Update JMAP state for this folder
        if !new_state.is_empty() {
            db::folders::update_jmap_state(&conn, account_id, mailbox_id, &new_state)?;
        }
    }

    // Remove local messages that no longer exist on the server.
    // Delta sync (Email/changes): server told us exactly which IDs were
    // destroyed since the previous state — apply those.
    // Full sync (initial / state-expired): server returned the complete
    // current set, so any local row not in that set is stale.
    {
        let conn = db.writer().await;
        let mut deleted = 0u32;
        if is_full {
            let server_ids: std::collections::HashSet<String> =
                emails.iter().map(|e| e.id.clone()).collect();
            let mut stmt = conn
                .prepare("SELECT id FROM messages WHERE account_id = ?1 AND folder_path = ?2")
                .map_err(Error::Database)?;
            let local_ids: Vec<String> = stmt
                .query_map(rusqlite::params![account_id, mailbox_id], |row| row.get(0))
                .map_err(Error::Database)?
                .filter_map(|r| r.ok())
                .collect();
            for local_id in &local_ids {
                let jmap_id = BackendMessageRef::jmap_from_db_id(account_id, mailbox_id, local_id)
                    .and_then(BackendMessageRef::into_jmap_email_id)
                    .unwrap_or_else(|| local_id.clone());
                if !server_ids.contains(&jmap_id) {
                    conn.execute(
                        "DELETE FROM messages WHERE id = ?1",
                        rusqlite::params![local_id],
                    )
                    .ok();
                    deleted += 1;
                }
            }
        } else {
            // Email/changes destroyed list: emails removed from the account.
            // moved_out: emails still in the account but no longer in this
            // mailbox. Both reduce to the same DB op (drop the per-folder
            // composite row); apply them together.
            for jmap_id in destroyed.iter().chain(moved_out.iter()) {
                let composite = BackendMessageRef::jmap(mailbox_id, jmap_id).to_db_id(account_id);
                if conn
                    .execute(
                        "DELETE FROM messages WHERE id = ?1",
                        rusqlite::params![composite],
                    )
                    .unwrap_or(0)
                    > 0
                {
                    deleted += 1;
                }
            }
        }
        if deleted > 0 {
            log::info!(
                "JMAP removed {} locally deleted messages from {}",
                deleted,
                folder_name
            );
        }
    }

    // Run filter rules against newly inserted messages. Errors are logged
    // and swallowed so a transient JMAP hiccup can't poison the sync.
    if !new_ids.is_empty() {
        match crate::filters::service::apply_filters_to_new_messages(
            db, account_id, mailbox_id, &new_ids,
        )
        .await
        {
            Ok(outcome) => {
                if outcome.affected > 0 {
                    log::info!(
                        "JMAP filters matched {} of {} new messages in '{}'",
                        outcome.affected,
                        new_ids.len(),
                        folder_name
                    );
                }
            }
            Err(e) => log::warn!("JMAP filter pass failed for '{}': {}", folder_name, e),
        }
    }

    // Update folder counts
    {
        let conn = db.writer().await;
        let page = db::messages::get_messages(
            &conn,
            account_id,
            mailbox_id,
            0,
            1,
            "date",
            false,
            &Default::default(),
        )?;
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
    let id = BackendMessageRef::jmap(mailbox_id, jmap_email_id).to_db_id(account_id);
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
    events: SharedEventSink,
    db: Arc<DbPool>,
    account_id: String,
    account_name: String,
    mailbox_id: String,
    jmap_config: JmapConfig,
    conn_jmap: JmapConnection,
) -> Result<u32> {
    events.publish(ApplicationEvent::SyncStarted(SyncStarted {
        account_id: account_id.clone(),
        account_name,
    }));

    let folder_name = mailbox_id.clone();
    let result = sync_jmap_folder(
        &db,
        &account_id,
        &conn_jmap,
        &jmap_config,
        &mailbox_id,
        &folder_name,
    )
    .await;

    match &result {
        Ok(count) => {
            events.publish(ApplicationEvent::SyncComplete(SyncComplete {
                account_id: account_id.clone(),
                total_synced: *count,
            }));
            events.publish(ApplicationEvent::FoldersChanged(account_id.clone()));
            events.publish(ApplicationEvent::MessagesChanged(account_id.clone()));
        }
        Err(e) => {
            events.publish(ApplicationEvent::SyncError(SyncError {
                account_id: account_id.clone(),
                error: e.to_string(),
            }));
        }
    }

    result
}

/// Validate a JMAP email id per RFC 8620 §1.2: ASCII `[A-Za-z0-9_-]`,
/// 1..=255 octets. The id flows into a maildir filename, so anything
/// outside that charset (path separators, NUL, control chars, `..`) is
/// rejected up front to stop a malicious or non-conforming server from
/// smuggling filesystem escapes through the body-fetch path.
fn validate_jmap_email_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 255 {
        return Err(Error::Other(format!(
            "Invalid JMAP email id length (expected 1..=255 bytes, got {})",
            id.len()
        )));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(Error::Other(format!("Invalid JMAP email id: {:?}", id)));
    }
    Ok(())
}

/// Fetch and store the body for a JMAP email on-demand.
/// Called when the user opens a message whose body hasn't been downloaded yet.
pub async fn fetch_and_store_jmap_body(
    jmap_config: &JmapConfig,
    conn_jmap: &JmapConnection,
    data_dir: &std::path::Path,
    account_id: &str,
    folder_path: &str,
    jmap_email_id: &str,
    flags: &[String],
) -> Result<String> {
    use crate::mail::sync::{create_maildir_dirs, flags_to_maildir_suffix, sanitize_folder_name};

    validate_jmap_email_id(jmap_email_id)?;

    log::info!(
        "JMAP on-demand body fetch: account={} folder={} jmap_id={}",
        account_id,
        folder_path,
        jmap_email_id
    );

    let body = conn_jmap
        .fetch_email_body(jmap_config, jmap_email_id)
        .await?
        .ok_or_else(|| {
            Error::Other(format!("JMAP no body returned for email {}", jmap_email_id))
        })?;

    // Write to Maildir structure for parsing
    let maildir_base = data_dir
        .join(account_id)
        .join(sanitize_folder_name(folder_path));
    create_maildir_dirs(&maildir_base)?;

    // Defence-in-depth: canonical check that the maildir stays inside data_dir.
    // The id charset check above already makes traversal via the filename
    // impossible; this catches folder_path or symlink-based escapes.
    let canonical_data_dir =
        std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let canonical_maildir = std::fs::canonicalize(&maildir_base)
        .map_err(|e| Error::Other(format!("Failed to resolve JMAP maildir path: {}", e)))?;
    if !canonical_maildir.starts_with(&canonical_data_dir) {
        // Do NOT attempt to clean up: the canonical path is, by construction,
        // outside our data tree, and removing it would follow the symlink an
        // attacker used to cause the escape and recursively delete arbitrary
        // user data. Leaving the stray dir is the safer failure mode.
        return Err(Error::Other(format!(
            "Path traversal detected: JMAP maildir '{}' escapes data directory",
            maildir_base.display()
        )));
    }

    let filename = format!("{}:2,{}", jmap_email_id, flags_to_maildir_suffix(flags));
    let msg_path = maildir_base.join("cur").join(&filename);
    std::fs::write(&msg_path, &body)?;

    let relative_path = format!(
        "{}/{}/cur/{}",
        account_id,
        sanitize_folder_name(folder_path),
        filename
    );

    log::info!("JMAP body saved: {} ({} bytes)", relative_path, body.len());

    Ok(relative_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_rfc8620_ids() {
        validate_jmap_email_id("M12345").unwrap();
        validate_jmap_email_id("abcDEF_-09").unwrap();
        validate_jmap_email_id("a").unwrap();
        validate_jmap_email_id(&"x".repeat(255)).unwrap();
    }

    #[test]
    fn rejects_empty() {
        assert!(validate_jmap_email_id("").is_err());
    }

    #[test]
    fn rejects_too_long() {
        assert!(validate_jmap_email_id(&"x".repeat(256)).is_err());
    }

    #[test]
    fn rejects_path_separators() {
        assert!(validate_jmap_email_id("../etc/passwd").is_err());
        assert!(validate_jmap_email_id("a/b").is_err());
        assert!(validate_jmap_email_id("a\\b").is_err());
    }

    #[test]
    fn rejects_nul_and_control_chars() {
        assert!(validate_jmap_email_id("a\0b").is_err());
        assert!(validate_jmap_email_id("a\nb").is_err());
    }

    #[test]
    fn rejects_dot_and_spaces() {
        // Dots are not in the RFC 8620 charset and could form `..`.
        assert!(validate_jmap_email_id(".").is_err());
        assert!(validate_jmap_email_id("..").is_err());
        assert!(validate_jmap_email_id("a.b").is_err());
        assert!(validate_jmap_email_id("a b").is_err());
    }

    #[test]
    fn rejects_non_ascii() {
        assert!(validate_jmap_email_id("café").is_err());
    }
}
