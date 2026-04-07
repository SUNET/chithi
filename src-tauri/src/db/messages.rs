use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, Serialize)]
pub struct MessageSummary {
    pub id: String,
    pub subject: Option<String>,
    pub from_name: Option<String>,
    pub from_email: String,
    pub date: String,
    pub flags: Vec<String>,
    pub has_attachments: bool,
    pub is_encrypted: bool,
    pub is_signed: bool,
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessagePage {
    pub messages: Vec<MessageSummary>,
    pub total: i64,
    pub page: u32,
    pub per_page: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Address {
    pub name: Option<String>,
    pub email: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Attachment {
    pub index: u32,
    pub filename: Option<String>,
    pub content_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageBody {
    pub id: String,
    pub subject: Option<String>,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub date: String,
    pub flags: Vec<String>,
    pub body_html: Option<String>,
    pub body_text: Option<String>,
    pub attachments: Vec<Attachment>,
    pub is_encrypted: bool,
    pub is_signed: bool,
    pub list_id: Option<String>,
}

pub struct NewMessage {
    pub id: String,
    pub account_id: String,
    pub folder_path: String,
    pub uid: u32,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub thread_id: Option<String>,
    pub subject: Option<String>,
    pub from_name: Option<String>,
    pub from_email: String,
    pub to_addresses: String,
    pub cc_addresses: String,
    pub date: String,
    pub size: u64,
    pub has_attachments: bool,
    pub is_encrypted: bool,
    pub is_signed: bool,
    pub flags: String,
    pub maildir_path: String,
    pub snippet: Option<String>,
}

pub fn insert_message(conn: &Connection, msg: &NewMessage) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO messages
         (id, account_id, folder_path, uid, message_id, in_reply_to, thread_id, subject,
          from_name, from_email, to_addresses, cc_addresses, date, size,
          has_attachments, is_encrypted, is_signed, flags, maildir_path, snippet)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
        params![
            msg.id,
            msg.account_id,
            msg.folder_path,
            msg.uid,
            msg.message_id,
            msg.in_reply_to,
            msg.thread_id,
            msg.subject,
            msg.from_name,
            msg.from_email,
            msg.to_addresses,
            msg.cc_addresses,
            msg.date,
            msg.size as i64,
            msg.has_attachments,
            msg.is_encrypted,
            msg.is_signed,
            msg.flags,
            msg.maildir_path,
            msg.snippet,
        ],
    )?;
    Ok(())
}

pub fn get_messages(
    conn: &Connection,
    account_id: &str,
    folder_path: &str,
    page: u32,
    per_page: u32,
    sort_column: &str,
    sort_asc: bool,
) -> Result<MessagePage> {
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE account_id = ?1 AND folder_path = ?2",
        params![account_id, folder_path],
        |row| row.get(0),
    )?;

    // Whitelist sort columns to prevent SQL injection
    let order_col = match sort_column {
        "subject" => "subject",
        "from" => "from_name",
        "date" => "date",
        "flagged" => "flags",
        _ => "date",
    };
    let order_dir = if sort_asc { "ASC" } else { "DESC" };

    let offset = page * per_page;
    let query = format!(
        "SELECT id, subject, from_name, from_email, date, flags,
                has_attachments, is_encrypted, is_signed, snippet
         FROM messages
         WHERE account_id = ?1 AND folder_path = ?2
         ORDER BY {} {}
         LIMIT ?3 OFFSET ?4",
        order_col, order_dir
    );
    let mut stmt = conn.prepare(&query)?;

    let messages = stmt
        .query_map(params![account_id, folder_path, per_page, offset], |row| {
            let flags_json: String = row.get(5)?;
            let flags: Vec<String> =
                serde_json::from_str(&flags_json).unwrap_or_default();
            Ok(MessageSummary {
                id: row.get(0)?,
                subject: row.get(1)?,
                from_name: row.get(2)?,
                from_email: row.get(3)?,
                date: row.get(4)?,
                flags,
                has_attachments: row.get(6)?,
                is_encrypted: row.get(7)?,
                is_signed: row.get(8)?,
                snippet: row.get(9)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(MessagePage {
        messages,
        total,
        page,
        per_page,
    })
}

pub fn get_message_metadata(
    conn: &Connection,
    account_id: &str,
    message_id: &str,
) -> Result<(String, String, String, String, String, bool, bool)> {
    // Returns: (maildir_path, from_email, to_addresses, cc_addresses, flags_json, is_encrypted, is_signed)
    conn.query_row(
        "SELECT maildir_path, from_email, to_addresses, cc_addresses, flags, is_encrypted, is_signed
         FROM messages WHERE id = ?1 AND account_id = ?2",
        params![message_id, account_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => {
            crate::error::Error::MessageNotFound(message_id.to_string())
        }
        other => crate::error::Error::Database(other),
    })
}

pub fn message_exists(conn: &Connection, account_id: &str, folder_path: &str, uid: u32) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE account_id = ?1 AND folder_path = ?2 AND uid = ?3",
        params![account_id, folder_path, uid],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Load all UIDs for a folder into a HashSet for fast batch existence checks.
pub fn get_existing_uids(
    conn: &Connection,
    account_id: &str,
    folder_path: &str,
) -> Result<std::collections::HashSet<u32>> {
    let mut stmt = conn.prepare(
        "SELECT uid FROM messages WHERE account_id = ?1 AND folder_path = ?2 AND uid > 0",
    )?;
    let uids = stmt
        .query_map(params![account_id, folder_path], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(uids)
}

pub fn get_folder_and_uid(conn: &Connection, message_id: &str) -> Result<(String, u32)> {
    conn.query_row(
        "SELECT folder_path, uid FROM messages WHERE id = ?1",
        params![message_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => {
            crate::error::Error::MessageNotFound(message_id.to_string())
        }
        other => crate::error::Error::Database(other),
    })
}

pub fn update_maildir_path(conn: &Connection, message_id: &str, path: &str) -> Result<()> {
    conn.execute(
        "UPDATE messages SET maildir_path = ?1 WHERE id = ?2",
        params![path, message_id],
    )?;
    Ok(())
}

/// Returns (message_id, folder_path, uid, flags_json) for messages whose body
/// has not been downloaded yet, ordered by date DESC (newest first).
/// Returns (message_id, folder_path, uid) for each of the given message IDs.
///
/// Looks up the folder and IMAP UID for each message so that IMAP actions
/// can be performed on them.
pub fn get_message_uids(
    conn: &Connection,
    message_ids: &[String],
) -> Result<Vec<(String, String, u32)>> {
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
        "SELECT id, folder_path, uid FROM messages WHERE id IN ({})",
        placeholders
    );

    let mut stmt = conn.prepare(&query)?;

    let params: Vec<&dyn rusqlite::types::ToSql> = message_ids
        .iter()
        .map(|id| id as &dyn rusqlite::types::ToSql)
        .collect();

    let rows = stmt
        .query_map(params.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u32>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(rows)
}

/// Delete messages from the local database by their IDs.
pub fn delete_messages_by_ids(conn: &Connection, message_ids: &[String]) -> Result<()> {
    if message_ids.is_empty() {
        return Ok(());
    }

    let placeholders: String = message_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(",");

    let query = format!("DELETE FROM messages WHERE id IN ({})", placeholders);

    let params: Vec<&dyn rusqlite::types::ToSql> = message_ids
        .iter()
        .map(|id| id as &dyn rusqlite::types::ToSql)
        .collect();

    conn.execute(&query, params.as_slice())?;
    Ok(())
}

/// Update the flags JSON string for a specific message.
pub fn update_flags(conn: &Connection, message_id: &str, flags: &str) -> Result<()> {
    conn.execute(
        "UPDATE messages SET flags = ?1 WHERE id = ?2",
        params![flags, message_id],
    )?;
    Ok(())
}

pub fn get_unfetched_messages(
    conn: &Connection,
    account_id: &str,
    limit: u32,
) -> Result<Vec<(String, String, u32, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, folder_path, uid, flags
         FROM messages
         WHERE account_id = ?1 AND maildir_path = ''
         ORDER BY date DESC
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![account_id, limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Compute a thread_id for a message using a multi-step strategy:
///
/// 1. **In-Reply-To lookup**: If the message has an `In-Reply-To` header, find the
///    referenced message in the DB and reuse its `thread_id`. This is the most
///    reliable threading signal.
///
/// 2. **Reverse lookup**: Check if any existing message has `In-Reply-To` pointing
///    to our `Message-ID` — if so, we're the parent and should join their thread.
///
/// 3. **Subject-based fallback**: Only for messages whose subject starts with
///    `Re:`, `Fwd:`, or `FW:` (explicit replies/forwards). Strip the prefix and
///    find an existing thread with the matching base subject. This catches cases
///    where `In-Reply-To` doesn't match (e.g., replies from different clients,
///    or Gmail conversations). Plain subjects like "OSSEC Notification" are NOT
///    matched to avoid false threading of automated/notification emails.
///
/// 4. **New thread root**: If no existing thread is found, use the message's own
///    `Message-ID` as a new `thread_id`.
///
/// Thread IDs are stored in the `thread_id` column of the `messages` table.
/// Empty string `''` is treated as "no thread" (same as NULL) throughout.
pub fn compute_thread_id(
    conn: &Connection,
    account_id: &str,
    message_id: Option<&str>,
    in_reply_to: Option<&str>,
    subject: Option<&str>,
) -> Option<String> {
    // Step 1: Look up thread_id of the message we are replying to
    if let Some(irt) = in_reply_to {
        let result: std::result::Result<Option<String>, _> = conn.query_row(
            "SELECT thread_id FROM messages WHERE account_id = ?1 AND message_id = ?2 LIMIT 1",
            params![account_id, irt],
            |row| row.get(0),
        );
        match result {
            Ok(Some(tid)) if !tid.is_empty() => return Some(tid),
            Ok(_) => return Some(irt.to_string()),
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(e) => {
                log::error!("compute_thread_id: DB error: {}", e);
            }
        }
    }

    // Step 2: Reverse lookup — does any existing message reply to us?
    if let Some(mid) = message_id {
        let result: std::result::Result<Option<String>, _> = conn.query_row(
            "SELECT thread_id FROM messages WHERE account_id = ?1 AND in_reply_to = ?2 AND thread_id IS NOT NULL AND thread_id != '' LIMIT 1",
            params![account_id, mid],
            |row| row.get(0),
        );
        if let Ok(Some(tid)) = result {
            return Some(tid);
        }
    }

    // Step 3: Subject-based threading — only for actual replies/forwards.
    if let Some(subj) = subject {
        let trimmed = subj.trim();
        let lower = trimmed.to_lowercase();
        let is_reply = lower.starts_with("re:") || lower.starts_with("fwd:") || lower.starts_with("fw:");
        if is_reply {
            let normalized = normalize_subject(trimmed);
            if !normalized.is_empty() {
                let result: std::result::Result<Option<String>, _> = conn.query_row(
                    "SELECT thread_id FROM messages
                     WHERE account_id = ?1 AND thread_id IS NOT NULL AND thread_id != ''
                     AND REPLACE(REPLACE(REPLACE(REPLACE(LOWER(TRIM(subject)), 're: ', ''), 'fwd: ', ''), 're:', ''), 'fwd:', '') = LOWER(?2)
                     ORDER BY date DESC LIMIT 1",
                    params![account_id, normalized],
                    |row| row.get(0),
                );
                if let Ok(Some(tid)) = result {
                    return Some(tid);
                }
            }
        }
    }

    // Step 4: New thread root
    if let Some(mid) = message_id {
        return Some(mid.to_string());
    }

    if let Some(irt) = in_reply_to {
        return Some(irt.to_string());
    }

    None
}

/// Strip Re:/Fwd:/FW: prefixes (case-insensitive, repeated) from a subject
/// for thread matching. "Re: Re: Fwd: Hello" → "Hello".
fn normalize_subject(subject: &str) -> String {
    let mut s = subject.trim();
    loop {
        let lower = s.to_lowercase();
        if lower.starts_with("re:") {
            s = s[3..].trim_start();
        } else if lower.starts_with("fwd:") {
            s = s[4..].trim_start();
        } else if lower.starts_with("fw:") {
            s = s[3..].trim_start();
        } else {
            break;
        }
    }
    s.to_string()
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub subject: Option<String>,
    pub last_date: String,
    pub message_count: u32,
    pub unread_count: u32,
    pub from_name: Option<String>,
    pub from_email: String,
    pub has_attachments: bool,
    pub flags: Vec<String>,
    pub snippet: Option<String>,
    pub message_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadedPage {
    pub threads: Vec<ThreadSummary>,
    pub total_threads: i64,
    pub total_messages: i64,
    pub page: u32,
    pub per_page: u32,
}

/// Get messages grouped by thread for a folder, with pagination.
///
/// Threading is per-folder: only messages physically in the requested folder
/// are grouped. This keeps the query fast (no cross-folder JOINs) and matches
/// user expectations — INBOX shows INBOX threads, Sent shows Sent threads.
/// Cross-folder thread messages are loaded on-demand via `get_thread_messages`.
///
/// Messages with empty/NULL `thread_id` are treated as individual threads.
/// The grouping expression `CASE WHEN thread_id != '' ... THEN thread_id ELSE id END`
/// handles both NULL and empty string, since many messages were synced before
/// threading was added and have `thread_id = ''`.
pub fn get_threaded_messages(
    conn: &Connection,
    account_id: &str,
    folder_path: &str,
    page: u32,
    per_page: u32,
    sort_column: &str,
    sort_asc: bool,
) -> Result<ThreadedPage> {
    // Count total messages in this folder
    let total_messages: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE account_id = ?1 AND folder_path = ?2",
        params![account_id, folder_path],
        |row| row.get(0),
    )?;

    // Find distinct thread_ids that have at least one message in this folder
    let total_threads: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT CASE WHEN thread_id != '' AND thread_id IS NOT NULL THEN thread_id ELSE id END)
         FROM messages WHERE account_id = ?1 AND folder_path = ?2",
        params![account_id, folder_path],
        |row| row.get(0),
    )?;

    let order_col = match sort_column {
        "subject" => "first_subject",
        "date" => "last_date",
        _ => "last_date",
    };
    let order_dir = if sort_asc { "ASC" } else { "DESC" };
    let offset = page * per_page;

    // Thread query: group messages within the current folder by thread_id.
    // Cross-folder thread messages are loaded on-demand when expanding a thread.
    let tid_expr = "CASE WHEN thread_id != '' AND thread_id IS NOT NULL THEN thread_id ELSE id END";
    let query = format!(
        "SELECT
            {tid} AS tid,
            MIN(subject) AS first_subject,
            MAX(date) AS last_date,
            COUNT(*) AS message_count,
            SUM(CASE WHEN flags NOT LIKE '%seen%' THEN 1 ELSE 0 END) AS unread_count,
            MAX(from_name) AS latest_from_name,
            MAX(from_email) AS latest_from_email,
            MAX(has_attachments) AS has_attach,
            MAX(flags) AS latest_flags,
            MAX(snippet) AS latest_snippet,
            GROUP_CONCAT(id, '||') AS all_ids
         FROM messages
         WHERE account_id = ?1 AND folder_path = ?2
         GROUP BY tid
         ORDER BY {order} {dir}
         LIMIT ?3 OFFSET ?4",
        tid = tid_expr,
        order = order_col,
        dir = order_dir,
    );

    let mut stmt = conn.prepare(&query)?;
    let threads = stmt
        .query_map(params![account_id, folder_path, per_page, offset], |row| {
            let tid: String = row.get(0)?;
            let subject: Option<String> = row.get(1)?;
            let last_date: String = row.get(2)?;
            let message_count: u32 = row.get(3)?;
            let unread_count: u32 = row.get(4)?;
            let from_name: Option<String> = row.get(5)?;
            let from_email: String = row.get::<_, Option<String>>(6)?.unwrap_or_default();
            let has_attachments: bool = row.get(7)?;
            let flags_json: Option<String> = row.get(8)?;
            let snippet: Option<String> = row.get(9)?;
            let all_ids_str: String = row.get(10)?;

            let flags: Vec<String> = flags_json
                .as_deref()
                .and_then(|f| serde_json::from_str(f).ok())
                .unwrap_or_default();

            let message_ids: Vec<String> = all_ids_str
                .split("||")
                .map(|s| s.to_string())
                .collect();

            Ok(ThreadSummary {
                thread_id: tid,
                subject,
                last_date,
                message_count,
                unread_count,
                from_name,
                from_email,
                has_attachments,
                flags,
                snippet,
                message_ids,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(ThreadedPage {
        threads,
        total_threads,
        total_messages,
        page,
        per_page,
    })
}

/// Get all messages in a specific thread, sorted by date ascending.
/// Get all messages in a thread, filtered to a specific folder.
///
/// Only returns messages from the given folder — Gmail stores the same email
/// in multiple folders (INBOX, All Mail, Sent Mail, Important) as "labels",
/// so without folder filtering a thread would show duplicates.
/// Messages are sorted by date ASC to show the conversation chronologically.
pub fn get_thread_messages(
    conn: &Connection,
    account_id: &str,
    folder_path: &str,
    thread_id: &str,
) -> Result<Vec<MessageSummary>> {
    log::debug!(
        "get_thread_messages: account={} folder={} thread={}",
        account_id,
        folder_path,
        thread_id
    );

    let mut stmt = conn.prepare(
        "SELECT id, subject, from_name, from_email, date, flags,
                has_attachments, is_encrypted, is_signed, snippet
         FROM messages
         WHERE account_id = ?1 AND folder_path = ?2
           AND (thread_id = ?3 OR (thread_id IS NULL AND id = ?3))
         ORDER BY date ASC",
    )?;

    let messages = stmt
        .query_map(params![account_id, folder_path, thread_id], |row| {
            let flags_json: String = row.get(5)?;
            let flags: Vec<String> =
                serde_json::from_str(&flags_json).unwrap_or_default();
            Ok(MessageSummary {
                id: row.get(0)?,
                subject: row.get(1)?,
                from_name: row.get(2)?,
                from_email: row.get(3)?,
                date: row.get(4)?,
                flags,
                has_attachments: row.get(6)?,
                is_encrypted: row.get(7)?,
                is_signed: row.get(8)?,
                snippet: row.get(9)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    log::debug!(
        "get_thread_messages: found {} messages in thread {}",
        messages.len(),
        thread_id
    );
    Ok(messages)
}

/// Remove a message from its thread by setting its thread_id to its own message_id,
/// effectively making it a standalone thread.
pub fn unthread_message(conn: &Connection, message_id: &str) -> Result<()> {
    log::info!("unthread_message: removing message '{}' from its thread", message_id);

    // Get the message's own message_id (the RFC 822 Message-ID header, stored in message_id column)
    let own_message_id: Option<String> = conn
        .query_row(
            "SELECT message_id FROM messages WHERE id = ?1",
            params![message_id],
            |row| row.get(0),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                crate::error::Error::MessageNotFound(message_id.to_string())
            }
            other => crate::error::Error::Database(other),
        })?;

    // Use the message's own message_id as thread_id, or fall back to the DB id
    let new_thread_id = own_message_id.unwrap_or_else(|| message_id.to_string());

    conn.execute(
        "UPDATE messages SET thread_id = ?1 WHERE id = ?2",
        params![new_thread_id, message_id],
    )?;

    log::info!(
        "unthread_message: message '{}' now has thread_id '{}'",
        message_id,
        new_thread_id
    );
    Ok(())
}
