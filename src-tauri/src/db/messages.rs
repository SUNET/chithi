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
}

pub struct NewMessage {
    pub id: String,
    pub account_id: String,
    pub folder_path: String,
    pub uid: u32,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
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
         (id, account_id, folder_path, uid, message_id, in_reply_to, subject,
          from_name, from_email, to_addresses, cc_addresses, date, size,
          has_attachments, is_encrypted, is_signed, flags, maildir_path, snippet)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            msg.id,
            msg.account_id,
            msg.folder_path,
            msg.uid,
            msg.message_id,
            msg.in_reply_to,
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
