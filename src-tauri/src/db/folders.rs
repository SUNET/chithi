use rusqlite::{params, Connection};
use serde::Serialize;

use crate::error::Result;

#[derive(Debug, Clone, Serialize)]
pub struct Folder {
    pub name: String,
    pub path: String,
    pub folder_type: Option<String>,
    pub unread_count: i64,
    pub total_count: i64,
    pub children: Vec<Folder>,
}

pub fn upsert_folder(
    conn: &Connection,
    account_id: &str,
    name: &str,
    path: &str,
    folder_type: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO folders (account_id, name, path, folder_type)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(account_id, path) DO UPDATE SET name = ?2, folder_type = ?4",
        params![account_id, name, path, folder_type],
    )?;
    Ok(())
}

pub fn list_folders(conn: &Connection, account_id: &str) -> Result<Vec<Folder>> {
    let mut stmt = conn.prepare(
        "SELECT name, path, folder_type, unread_count, total_count
         FROM folders WHERE account_id = ?1 ORDER BY
         CASE folder_type
           WHEN 'inbox' THEN 0
           WHEN 'drafts' THEN 1
           WHEN 'sent' THEN 2
           WHEN 'junk' THEN 3
           WHEN 'trash' THEN 4
           WHEN 'archive' THEN 5
           ELSE 6
         END, name",
    )?;
    let folders = stmt
        .query_map(params![account_id], |row| {
            Ok(Folder {
                name: row.get(0)?,
                path: row.get(1)?,
                folder_type: row.get(2)?,
                unread_count: row.get(3)?,
                total_count: row.get(4)?,
                children: vec![],
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(folders)
}

pub fn update_folder_counts(
    conn: &Connection,
    account_id: &str,
    path: &str,
    unread: i64,
    total: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE folders SET unread_count = ?1, total_count = ?2
         WHERE account_id = ?3 AND path = ?4",
        params![unread, total, account_id, path],
    )?;
    Ok(())
}

pub fn update_last_seen_uid(
    conn: &Connection,
    account_id: &str,
    path: &str,
    uid: u32,
) -> Result<()> {
    conn.execute(
        "UPDATE folders SET last_seen_uid = ?1 WHERE account_id = ?2 AND path = ?3",
        params![uid, account_id, path],
    )?;
    Ok(())
}

pub fn get_last_seen_uid(conn: &Connection, account_id: &str, path: &str) -> Result<u32> {
    let uid: u32 = conn
        .query_row(
            "SELECT last_seen_uid FROM folders WHERE account_id = ?1 AND path = ?2",
            params![account_id, path],
            |row| row.get(0),
        )
        .unwrap_or(0);
    Ok(uid)
}

/// Guess folder type from name for common IMAP folder names
pub fn guess_folder_type(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    // Gmail uses [Gmail]/... prefixed names
    let normalized = lower
        .trim_start_matches("[gmail]/")
        .trim_start_matches("[google mail]/");
    match normalized {
        "inbox" => Some("inbox"),
        "sent" | "sent mail" | "sent messages" | "sent items" => Some("sent"),
        "drafts" | "draft" => Some("drafts"),
        "trash" | "deleted" | "deleted messages" | "deleted items" | "bin" => Some("trash"),
        "junk" | "spam" | "bulk mail" => Some("junk"),
        "archive" | "all mail" | "all" => Some("archive"),
        _ => None,
    }
}
