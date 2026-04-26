use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::mail::msgid::normalize_message_id;

/// Quick filter options for the message list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuickFilter {
    #[serde(default)]
    pub unread: bool,
    #[serde(default)]
    pub starred: bool,
    #[serde(default)]
    pub has_attachment: bool,
    #[serde(default)]
    pub contact: bool,
    /// Text search term (SQL LIKE on selected fields)
    #[serde(default)]
    pub text: String,
    /// Which fields to search: "sender", "recipients", "subject", "body"
    #[serde(default)]
    pub text_fields: Vec<String>,
}

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
    /// RFC 5322 Message-ID, with angle brackets, exactly as stored.
    /// Used by the frontend to build the in-thread reply hierarchy.
    pub message_id: Option<String>,
    /// In-Reply-To header pointing at this message's parent within the
    /// thread. Empty for the thread root or when the source message/backend
    /// does not provide the header; Microsoft Graph populates this from
    /// `internetMessageHeaders` when available.
    pub in_reply_to: Option<String>,
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
    pub has_remote_images: bool,
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

/// Build extra SQL WHERE clauses for quick filter options.
/// Text search terms are escaped (FTS5 quoting or LIKE backslash escaping)
/// before interpolation. Other filter fields use fixed SQL patterns.
fn build_filter_clauses(filter: &QuickFilter, account_id: &str, use_fts: bool) -> String {
    let mut clauses = Vec::new();
    if filter.unread {
        clauses.push("flags NOT LIKE '%seen%'".to_string());
    }
    if filter.starred {
        clauses.push("flags LIKE '%flagged%'".to_string());
    }
    if filter.has_attachment {
        clauses.push("has_attachments = 1".to_string());
    }
    if filter.contact {
        clauses.push(format!(
            "from_email IN (
                SELECT json_extract(je.value, '$.email')
                FROM contacts, json_each(contacts.emails_json) AS je
                WHERE contacts.book_id IN (
                    SELECT id FROM contact_books WHERE account_id = '{acct}'
                )
                UNION
                SELECT email FROM collected_contacts WHERE account_id = '{acct}'
            )",
            acct = account_id.replace('\'', "''")
        ));
    }
    // Text search
    let text = filter.text.trim();
    if !text.is_empty() {
        // Determine which columns to search (default: all)
        let fields = &filter.text_fields;
        let search_sender = fields.is_empty() || fields.iter().any(|f| f == "sender");
        let search_recipients = fields.is_empty() || fields.iter().any(|f| f == "recipients");
        let search_subject = fields.is_empty() || fields.iter().any(|f| f == "subject");
        let search_body = fields.is_empty() || fields.iter().any(|f| f == "body");

        if use_fts {
            // FTS5 for fast full-text matching
            let escaped = text.replace('"', "\"\"").replace('\'', "''");

            let mut fts_columns = Vec::new();
            if search_sender {
                fts_columns.push("from_name");
                fts_columns.push("from_email");
            }
            if search_recipients {
                fts_columns.push("to_addresses");
                fts_columns.push("cc_addresses");
            }
            if search_subject {
                fts_columns.push("subject");
            }
            if search_body {
                fts_columns.push("snippet");
            }

            if !fts_columns.is_empty() {
                let col_filter = format!("{{{}}}", fts_columns.join(" "));
                clauses.push(format!(
                    "rowid IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH '{} : \"{}\"*')",
                    col_filter, escaped
                ));
            }
        } else {
            // LIKE fallback — escape backslash first, then SQL special chars
            let like_escaped = text
                .replace('\\', "\\\\")
                .replace('\'', "''")
                .replace('%', "\\%")
                .replace('_', "\\_");

            let mut like_clauses = Vec::new();
            if search_sender {
                like_clauses.push(format!("from_name LIKE '%{}%' ESCAPE '\\'", like_escaped));
                like_clauses.push(format!("from_email LIKE '%{}%' ESCAPE '\\'", like_escaped));
            }
            if search_recipients {
                like_clauses.push(format!(
                    "to_addresses LIKE '%{}%' ESCAPE '\\'",
                    like_escaped
                ));
                like_clauses.push(format!(
                    "cc_addresses LIKE '%{}%' ESCAPE '\\'",
                    like_escaped
                ));
            }
            if search_subject {
                like_clauses.push(format!("subject LIKE '%{}%' ESCAPE '\\'", like_escaped));
            }
            if search_body {
                like_clauses.push(format!("snippet LIKE '%{}%' ESCAPE '\\'", like_escaped));
            }
            if !like_clauses.is_empty() {
                clauses.push(format!("({})", like_clauses.join(" OR ")));
            }
        }
    }
    if clauses.is_empty() {
        String::new()
    } else {
        format!(" AND {}", clauses.join(" AND "))
    }
}

pub fn get_messages(
    conn: &Connection,
    account_id: &str,
    folder_path: &str,
    page: u32,
    per_page: u32,
    sort_column: &str,
    sort_asc: bool,
    filter: &QuickFilter,
) -> Result<MessagePage> {
    // Try FTS5 first; on failure (bad query syntax), fall back to LIKE
    let has_text = !filter.text.trim().is_empty();
    match get_messages_inner(
        conn,
        account_id,
        folder_path,
        page,
        per_page,
        sort_column,
        sort_asc,
        filter,
        true,
    ) {
        Ok(page) => Ok(page),
        Err(e) if has_text => {
            log::warn!("FTS5 query failed, retrying with LIKE fallback: {}", e);
            get_messages_inner(
                conn,
                account_id,
                folder_path,
                page,
                per_page,
                sort_column,
                sort_asc,
                filter,
                false,
            )
        }
        Err(e) => Err(e),
    }
}

fn get_messages_inner(
    conn: &Connection,
    account_id: &str,
    folder_path: &str,
    page: u32,
    per_page: u32,
    sort_column: &str,
    sort_asc: bool,
    filter: &QuickFilter,
    use_fts: bool,
) -> Result<MessagePage> {
    let filter_sql = build_filter_clauses(filter, account_id, use_fts);

    let total: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM messages WHERE account_id = ?1 AND folder_path = ?2{}",
            filter_sql
        ),
        params![account_id, folder_path],
        |row| row.get(0),
    )?;

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
                has_attachments, is_encrypted, is_signed, snippet,
                message_id, in_reply_to
         FROM messages
         WHERE account_id = ?1 AND folder_path = ?2{}
         ORDER BY {} {}
         LIMIT ?3 OFFSET ?4",
        filter_sql, order_col, order_dir
    );
    let mut stmt = conn.prepare(&query)?;

    let messages = stmt
        .query_map(params![account_id, folder_path, per_page, offset], |row| {
            let flags_json: String = row.get(5)?;
            let flags: Vec<String> = serde_json::from_str(&flags_json).unwrap_or_default();
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
                message_id: row.get(10)?,
                in_reply_to: row.get(11)?,
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

pub fn message_exists(
    conn: &Connection,
    account_id: &str,
    folder_path: &str,
    uid: u32,
) -> Result<bool> {
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

/// Returns (message_id, maildir_path) pairs for the given IDs within a specific account.
/// Rows with empty maildir_path are omitted (message body not yet synced).
pub fn get_maildir_paths(
    conn: &Connection,
    account_id: &str,
    message_ids: &[String],
) -> Result<Vec<(String, String)>> {
    if message_ids.is_empty() {
        return Ok(vec![]);
    }

    let placeholders: String = message_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");

    let query = format!(
        "SELECT id, maildir_path FROM messages
         WHERE account_id = ?1 AND id IN ({}) AND maildir_path != ''",
        placeholders
    );

    let mut stmt = conn.prepare(&query)?;

    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    params.push(Box::new(account_id.to_string()));
    for id in message_ids {
        params.push(Box::new(id.clone()));
    }
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
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

/// Bulk-update flags for messages by UID within a folder.
/// Returns the number of messages whose flags actually changed.
/// Sync flag changes from server. Uses a single bulk query to load all
/// messages in the folder into a HashMap, then compares in memory and
/// only UPDATEs changed rows. This replaces the previous per-message
/// SELECT loop which did N queries (e.g., 15k for Trash).
pub fn sync_flags_by_uid(
    conn: &Connection,
    account_id: &str,
    folder_path: &str,
    uid_flags: &[(u32, String)],
) -> Result<u32> {
    // One query: load all (uid -> (id, flags)) for the folder
    let mut stmt = conn.prepare(
        "SELECT uid, id, flags FROM messages WHERE account_id = ?1 AND folder_path = ?2 AND uid > 0",
    )?;
    let local: std::collections::HashMap<u32, (String, String)> = stmt
        .query_map(params![account_id, folder_path], |row| {
            Ok((
                row.get::<_, u32>(0)?,
                (row.get::<_, String>(1)?, row.get::<_, String>(2)?),
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Compare in memory, only UPDATE changed rows
    let mut changed = 0u32;
    for (uid, new_flags_json) in uid_flags {
        if let Some((msg_id, current_flags)) = local.get(uid) {
            if current_flags != new_flags_json {
                conn.execute(
                    "UPDATE messages SET flags = ?1 WHERE id = ?2",
                    params![new_flags_json, msg_id],
                )?;
                changed += 1;
            }
        }
    }
    Ok(changed)
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
///    referenced message in the DB and reuse its `thread_id`.
///
/// 2. **References chain walk**: The full RFC 5322 `References:` chain lists the
///    ancestry from root to direct parent. Walk it from the closest ancestor
///    backwards; the first id we find in our DB hands us its thread. This is
///    what stitches mailing-list patch series (`[PATCH n/m]`, `Re: [PATCH n/m]`)
///    back to the original discussion they're replying to.
///
/// 3. **Reverse lookup**: Check if any existing message has `In-Reply-To` pointing
///    to our `Message-ID` — if so, we're the parent and should join their thread.
///
/// 4. **Subject-based fallback**: Only for messages whose subject starts with
///    `Re:`, `Fwd:`, or `FW:` (explicit replies/forwards). Strip the prefix and
///    find an existing thread with the matching base subject. This catches cases
///    where `In-Reply-To` doesn't match (e.g., replies from different clients,
///    or Gmail conversations). Plain subjects like "OSSEC Notification" are NOT
///    matched to avoid false threading of automated/notification emails.
///
/// 5. **New thread root**: If no existing thread is found, use the message's own
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
    references: Option<&[String]>,
) -> Option<String> {
    // Defensive: callers in older code paths may still pass non-canonical
    // forms (leading whitespace, missing brackets). Normalize once so the
    // exact-match SQL below has a consistent comparand on both sides.
    let message_id_norm = message_id.and_then(normalize_message_id);
    let in_reply_to_norm = in_reply_to.and_then(normalize_message_id);
    let references_norm: Option<Vec<String>> = references.map(|refs| {
        refs.iter()
            .filter_map(|r| normalize_message_id(r))
            .collect::<Vec<_>>()
    });

    // Step 1: Look up thread_id of the message we are replying to
    if let Some(irt) = in_reply_to_norm.as_deref() {
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

    // Step 2: Walk References from newest to oldest. The chain is ordered
    // root-first per RFC 5322 §3.6.4, so the most recent ancestor — the one
    // most likely already synced — is at the tail.
    if let Some(refs) = references_norm.as_deref() {
        for r in refs.iter().rev() {
            // Skip the in_reply_to we already checked.
            if in_reply_to_norm.as_deref() == Some(r.as_str()) {
                continue;
            }
            let result: std::result::Result<Option<String>, _> = conn.query_row(
                "SELECT thread_id FROM messages WHERE account_id = ?1 AND message_id = ?2 LIMIT 1",
                params![account_id, r],
                |row| row.get(0),
            );
            match result {
                Ok(Some(tid)) if !tid.is_empty() => return Some(tid),
                Ok(_) => return Some(r.clone()),
                Err(rusqlite::Error::QueryReturnedNoRows) => continue,
                Err(e) => {
                    log::error!("compute_thread_id: DB error walking References: {}", e);
                    continue;
                }
            }
        }
        // None of the ancestors are in our DB yet. Use the oldest reference
        // (the root of the conversation) as a synthetic thread_id; later
        // siblings whose chain shares the same root will land in this thread.
        if let Some(root) = refs.first() {
            return Some(root.clone());
        }
    }

    // Step 3: Reverse lookup — does any existing message reply to us?
    if let Some(mid) = message_id_norm.as_deref() {
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
        let is_reply =
            lower.starts_with("re:") || lower.starts_with("fwd:") || lower.starts_with("fw:");
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
    if let Some(mid) = message_id_norm {
        return Some(mid);
    }

    if let Some(irt) = in_reply_to_norm {
        return Some(irt);
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
    filter: &QuickFilter,
) -> Result<ThreadedPage> {
    let has_text = !filter.text.trim().is_empty();
    match get_threaded_messages_inner(
        conn,
        account_id,
        folder_path,
        page,
        per_page,
        sort_column,
        sort_asc,
        filter,
        true,
    ) {
        Ok(page) => Ok(page),
        Err(e) if has_text => {
            log::warn!("FTS5 threaded query failed, retrying with LIKE: {}", e);
            get_threaded_messages_inner(
                conn,
                account_id,
                folder_path,
                page,
                per_page,
                sort_column,
                sort_asc,
                filter,
                false,
            )
        }
        Err(e) => Err(e),
    }
}

fn get_threaded_messages_inner(
    conn: &Connection,
    account_id: &str,
    folder_path: &str,
    page: u32,
    per_page: u32,
    sort_column: &str,
    sort_asc: bool,
    filter: &QuickFilter,
    use_fts: bool,
) -> Result<ThreadedPage> {
    let filter_sql = build_filter_clauses(filter, account_id, use_fts);

    // Count total messages in this folder (with filters)
    let total_messages: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM messages WHERE account_id = ?1 AND folder_path = ?2{}",
            filter_sql
        ),
        params![account_id, folder_path],
        |row| row.get(0),
    )?;

    // Find distinct thread_ids (with filters)
    let total_threads: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(DISTINCT CASE WHEN thread_id != '' AND thread_id IS NOT NULL THEN thread_id ELSE id END)
             FROM messages WHERE account_id = ?1 AND folder_path = ?2{}",
            filter_sql
        ),
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
         WHERE account_id = ?1 AND folder_path = ?2{filter}
         GROUP BY tid
         ORDER BY {order} {dir}
         LIMIT ?3 OFFSET ?4",
        filter = filter_sql,
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

            let message_ids: Vec<String> = all_ids_str.split("||").map(|s| s.to_string()).collect();

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
                has_attachments, is_encrypted, is_signed, snippet,
                message_id, in_reply_to
         FROM messages
         WHERE account_id = ?1 AND folder_path = ?2
           AND (thread_id = ?3 OR (thread_id IS NULL AND id = ?3))
         ORDER BY date ASC",
    )?;

    let messages = stmt
        .query_map(params![account_id, folder_path, thread_id], |row| {
            let flags_json: String = row.get(5)?;
            let flags: Vec<String> = serde_json::from_str(&flags_json).unwrap_or_default();
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
                message_id: row.get(10)?,
                in_reply_to: row.get(11)?,
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
    log::info!(
        "unthread_message: removing message '{}' from its thread",
        message_id
    );

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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                folder_path TEXT NOT NULL,
                uid INTEGER,
                message_id TEXT,
                in_reply_to TEXT,
                thread_id TEXT,
                subject TEXT,
                from_name TEXT,
                from_email TEXT,
                to_addresses TEXT,
                cc_addresses TEXT,
                date TEXT NOT NULL,
                size INTEGER,
                has_attachments INTEGER DEFAULT 0,
                is_encrypted INTEGER DEFAULT 0,
                is_signed INTEGER DEFAULT 0,
                flags TEXT DEFAULT '[]',
                maildir_path TEXT,
                snippet TEXT
            );",
        )
        .unwrap();
        conn
    }

    fn insert_row(conn: &Connection, id: &str, account_id: &str, maildir_path: &str) {
        conn.execute(
            "INSERT INTO messages (id, account_id, folder_path, date, maildir_path)
             VALUES (?1, ?2, 'INBOX', '2026-04-11T00:00:00Z', ?3)",
            params![id, account_id, maildir_path],
        )
        .unwrap();
    }

    #[test]
    fn test_get_maildir_paths_returns_matching_rows() {
        let conn = setup_db();
        insert_row(&conn, "msg1", "acc1", "acc1/INBOX/cur/1:2,S");
        insert_row(&conn, "msg2", "acc1", "acc1/INBOX/cur/2:2,S");
        insert_row(&conn, "msg3", "acc1", "acc1/INBOX/cur/3:2,S");

        let ids = vec!["msg1".to_string(), "msg3".to_string()];
        let paths = get_maildir_paths(&conn, "acc1", &ids).unwrap();

        assert_eq!(paths.len(), 2);
        let map: std::collections::HashMap<_, _> = paths.into_iter().collect();
        assert_eq!(
            map.get("msg1").map(String::as_str),
            Some("acc1/INBOX/cur/1:2,S")
        );
        assert_eq!(
            map.get("msg3").map(String::as_str),
            Some("acc1/INBOX/cur/3:2,S")
        );
    }

    #[test]
    fn test_get_maildir_paths_skips_empty_paths() {
        let conn = setup_db();
        insert_row(&conn, "msg1", "acc1", "acc1/INBOX/cur/1:2,S");
        insert_row(&conn, "msg2", "acc1", "");

        let ids = vec!["msg1".to_string(), "msg2".to_string()];
        let paths = get_maildir_paths(&conn, "acc1", &ids).unwrap();

        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].0, "msg1");
    }

    #[test]
    fn test_get_maildir_paths_scoped_by_account() {
        let conn = setup_db();
        insert_row(&conn, "msg1", "acc1", "acc1/INBOX/cur/1:2,S");
        insert_row(&conn, "msg2", "acc2", "acc2/INBOX/cur/2:2,S");

        // Requesting msg2 from acc1 should not return it
        let ids = vec!["msg1".to_string(), "msg2".to_string()];
        let paths = get_maildir_paths(&conn, "acc1", &ids).unwrap();

        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].0, "msg1");
    }

    #[test]
    fn test_get_maildir_paths_empty_input() {
        let conn = setup_db();
        let paths = get_maildir_paths(&conn, "acc1", &[]).unwrap();
        assert!(paths.is_empty());
    }

    fn insert_threaded_row(
        conn: &Connection,
        id: &str,
        message_id: Option<&str>,
        thread_id: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO messages
             (id, account_id, folder_path, uid, message_id, thread_id, date, from_email, maildir_path)
             VALUES (?1, 'acc1', 'INBOX', 1, ?2, ?3, '2026-04-26T00:00:00Z', 'x@y', '')",
            params![id, message_id, thread_id],
        )
        .unwrap();
    }

    #[test]
    fn compute_thread_id_walks_references_to_existing_ancestor() {
        let conn = setup_db();
        // Original [BUG] message is already synced with its own thread_id.
        insert_threaded_row(&conn, "row1", Some("<bug@list>"), Some("<bug@list>"));

        // A [PATCH 1/2] message arrives. Its In-Reply-To points at a child
        // we don't yet have, but its References chain goes back to the BUG.
        let refs = vec![
            "<bug@list>".to_string(),
            "<re-bug-1@list>".to_string(),
            "<unknown-parent@list>".to_string(),
        ];
        let tid = compute_thread_id(
            &conn,
            "acc1",
            Some("<patch1@list>"),
            Some("<unknown-parent@list>"),
            Some("[PATCH 1/2] foo"),
            Some(&refs),
        );
        assert_eq!(tid.as_deref(), Some("<bug@list>"));
    }

    #[test]
    fn compute_thread_id_uses_root_reference_when_no_ancestor_synced() {
        let conn = setup_db();
        // Nothing in the DB yet — the new message is the first arrival.
        let refs = vec!["<bug@list>".to_string(), "<re-bug-1@list>".to_string()];
        let tid = compute_thread_id(
            &conn,
            "acc1",
            Some("<patch1@list>"),
            Some("<re-bug-1@list>"),
            Some("[PATCH 1/2] foo"),
            Some(&refs),
        );
        // Falls back to the root of the chain so future siblings join it.
        assert_eq!(tid.as_deref(), Some("<bug@list>"));
    }

    #[test]
    fn compute_thread_id_works_without_references() {
        let conn = setup_db();
        // Plain root message, no in_reply_to, no references — uses its own
        // Message-ID as the thread root.
        let tid = compute_thread_id(
            &conn,
            "acc1",
            Some("<bug@list>"),
            None,
            Some("[BUG] foo"),
            None,
        );
        assert_eq!(tid.as_deref(), Some("<bug@list>"));
    }
}
