use rusqlite::Connection;

use crate::error::{Error, Result};
use crate::ops::queue::MailOp;

/// Replay order matching Thunderbird's `nsImapOfflineSync`:
/// flags (0) -> moves (1) -> copies (2) -> deletes (3).
/// `send` is independent of folder state, so it goes last (4).
fn replay_order(action_type: &str) -> i32 {
    match action_type {
        "set_flags" => 0,
        "move" => 1,
        "copy" => 2,
        "delete" => 3,
        "send" => 4,
        _ => 5,
    }
}

/// An entry from the outbox table.
#[derive(Debug)]
pub struct OutboxEntry {
    pub id: i64,
    pub account_id: String,
    pub action_type: String,
    pub payload_json: String,
    pub status: String,
    pub retry_count: i32,
    pub error_message: Option<String>,
}

/// Write a failed operation to the outbox for later replay.
///
/// Replay order (flags -> moves -> copies -> deletes) is computed at read
/// time in `get_pending_ops` via `replay_order()`, so no extra column or
/// workaround is needed here.
pub fn queue_offline_op(
    conn: &Connection,
    account_id: &str,
    action_type: &str,
    payload: &serde_json::Value,
) -> Result<i64> {
    queue_offline_op_with_status(conn, account_id, action_type, payload, "pending")
}

/// Like `queue_offline_op` but inserts with an explicit initial status.
///
/// `compose::send_message` uses this with `'sending'` to claim the row
/// for the in-process spawn task, keeping the worker's replay loop from
/// picking it up while the first attempt is still in flight.
pub fn queue_offline_op_with_status(
    conn: &Connection,
    account_id: &str,
    action_type: &str,
    payload: &serde_json::Value,
    initial_status: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO outbox (account_id, action_type, payload_json, status, retry_count, error_message)
         VALUES (?1, ?2, ?3, ?4, 0, NULL)",
        rusqlite::params![account_id, action_type, payload.to_string(), initial_status],
    )
    .map_err(Error::Database)?;
    let id = conn.last_insert_rowid();
    Ok(id)
}

/// Flip a 'sending' row back to 'pending' so the worker will retry it
/// on the next sync.
pub fn mark_pending(conn: &Connection, outbox_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE outbox SET status = 'pending' WHERE id = ?1",
        rusqlite::params![outbox_id],
    )
    .map_err(Error::Database)?;
    Ok(())
}

/// On app startup, revive any rows left as 'sending' from a previous run.
/// The owning task is gone, so the message would otherwise be invisible
/// to both the user and the worker. Returns how many rows were revived.
pub fn revive_stuck_sending(conn: &Connection) -> Result<usize> {
    let count = conn
        .execute(
            "UPDATE outbox SET status = 'pending' WHERE status = 'sending'",
            [],
        )
        .map_err(Error::Database)?;
    if count > 0 {
        log::info!("Revived {} outbox row(s) stuck in 'sending'", count);
    }
    Ok(count)
}

/// Get all pending operations for an account, ordered by replay priority.
pub fn get_pending_ops(conn: &Connection, account_id: &str) -> Result<Vec<OutboxEntry>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, account_id, action_type, payload_json, status, retry_count, error_message
             FROM outbox
             WHERE account_id = ?1 AND status = 'pending'
             ORDER BY id ASC",
        )
        .map_err(Error::Database)?;

    let mut entries: Vec<OutboxEntry> = stmt
        .query_map(rusqlite::params![account_id], |row| {
            Ok(OutboxEntry {
                id: row.get(0)?,
                account_id: row.get(1)?,
                action_type: row.get(2)?,
                payload_json: row.get(3)?,
                status: row.get(4)?,
                retry_count: row.get(5)?,
                error_message: row.get(6)?,
            })
        })
        .map_err(Error::Database)?
        .filter_map(|r| r.ok())
        .collect();

    // Sort by replay order: flags -> moves -> copies -> deletes.
    // Use stable ordering: break ties by id to preserve insertion order.
    entries.sort_by(|a, b| {
        replay_order(&a.action_type)
            .cmp(&replay_order(&b.action_type))
            .then(a.id.cmp(&b.id))
    });
    Ok(entries)
}

/// Mark an outbox entry as completed (will be deleted).
pub fn mark_completed(conn: &Connection, outbox_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM outbox WHERE id = ?1",
        rusqlite::params![outbox_id],
    )
    .map_err(Error::Database)?;
    Ok(())
}

/// Mark an outbox entry as failed, incrementing retry count.
pub fn mark_failed(conn: &Connection, outbox_id: i64, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE outbox SET retry_count = retry_count + 1, error_message = ?1
         WHERE id = ?2",
        rusqlite::params![error, outbox_id],
    )
    .map_err(Error::Database)?;
    Ok(())
}

/// Mark an outbox entry as dead (too many retries).
pub fn mark_dead(conn: &Connection, outbox_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE outbox SET status = 'dead' WHERE id = ?1",
        rusqlite::params![outbox_id],
    )
    .map_err(Error::Database)?;
    Ok(())
}

/// Get dead operations (retry_count >= max_retries) for surfacing to user.
pub fn get_dead_ops(conn: &Connection, account_id: &str) -> Result<Vec<OutboxEntry>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, account_id, action_type, payload_json, status, retry_count, error_message
             FROM outbox
             WHERE account_id = ?1 AND status = 'dead'
             ORDER BY id ASC",
        )
        .map_err(Error::Database)?;

    let entries = stmt
        .query_map(rusqlite::params![account_id], |row| {
            Ok(OutboxEntry {
                id: row.get(0)?,
                account_id: row.get(1)?,
                action_type: row.get(2)?,
                payload_json: row.get(3)?,
                status: row.get(4)?,
                retry_count: row.get(5)?,
                error_message: row.get(6)?,
            })
        })
        .map_err(Error::Database)?
        .filter_map(|r| r.ok())
        .collect();

    Ok(entries)
}

/// Convert a MailOp to an action_type string and JSON payload for outbox storage.
pub fn mail_op_to_outbox(op: &MailOp) -> Option<(&'static str, serde_json::Value)> {
    match op {
        MailOp::MoveMessages {
            by_folder,
            target_folder,
        } => Some((
            "move",
            serde_json::json!({
                "by_folder": by_folder,
                "target_folder": target_folder,
            }),
        )),
        MailOp::DeleteMessages { by_folder } => {
            Some(("delete", serde_json::json!({ "by_folder": by_folder })))
        }
        MailOp::SetFlags {
            by_folder,
            flags,
            add,
        } => Some((
            "set_flags",
            serde_json::json!({
                "by_folder": by_folder,
                "flags": flags,
                "add": add,
            }),
        )),
        MailOp::CopyMessages {
            by_folder,
            target_folder,
        } => Some((
            "copy",
            serde_json::json!({
                "by_folder": by_folder,
                "target_folder": target_folder,
            }),
        )),
        MailOp::SendRaw {
            raw_message,
            from,
            to,
            cc,
            bcc,
            subject,
        } => {
            use base64::Engine;
            let raw_b64 = base64::engine::general_purpose::STANDARD.encode(raw_message);
            Some((
                "send",
                serde_json::json!({
                    "raw_message_b64": raw_b64,
                    "from": from,
                    "to": to,
                    "cc": cc,
                    "bcc": bcc,
                    "subject": subject,
                }),
            ))
        }
        // Sync ops are not queued offline
        _ => None,
    }
}

/// Validate that a folder path doesn't contain characters that could be
/// injected into IMAP commands (null bytes, bare newlines).
fn validate_folder_paths(by_folder: &std::collections::HashMap<String, Vec<u32>>) -> bool {
    by_folder
        .keys()
        .all(|path| !path.contains('\0') && !path.contains('\n') && !path.contains('\r'))
}

/// Convert an outbox entry back to a MailOp for replay.
pub fn outbox_to_mail_op(entry: &OutboxEntry) -> Option<MailOp> {
    let payload: serde_json::Value = serde_json::from_str(&entry.payload_json).ok()?;
    match entry.action_type.as_str() {
        "move" => {
            let by_folder: std::collections::HashMap<String, Vec<u32>> =
                serde_json::from_value(payload.get("by_folder")?.clone()).ok()?;
            if !validate_folder_paths(&by_folder) {
                log::warn!("outbox_to_mail_op: rejected move op with invalid folder path");
                return None;
            }
            let target_folder = payload.get("target_folder")?.as_str()?.to_string();
            if target_folder.contains('\0') || target_folder.contains('\n') {
                log::warn!("outbox_to_mail_op: rejected move op with invalid target folder");
                return None;
            }
            Some(MailOp::MoveMessages {
                by_folder,
                target_folder,
            })
        }
        "delete" => {
            let by_folder: std::collections::HashMap<String, Vec<u32>> =
                serde_json::from_value(payload.get("by_folder")?.clone()).ok()?;
            if !validate_folder_paths(&by_folder) {
                log::warn!("outbox_to_mail_op: rejected delete op with invalid folder path");
                return None;
            }
            Some(MailOp::DeleteMessages { by_folder })
        }
        "set_flags" => {
            let by_folder = serde_json::from_value(payload.get("by_folder")?.clone()).ok()?;
            let flags = serde_json::from_value(payload.get("flags")?.clone()).ok()?;
            let add = payload.get("add")?.as_bool()?;
            Some(MailOp::SetFlags {
                by_folder,
                flags,
                add,
            })
        }
        "copy" => {
            let by_folder = serde_json::from_value(payload.get("by_folder")?.clone()).ok()?;
            let target_folder = payload.get("target_folder")?.as_str()?.to_string();
            Some(MailOp::CopyMessages {
                by_folder,
                target_folder,
            })
        }
        "send" => {
            use base64::Engine;
            let raw_b64 = payload.get("raw_message_b64")?.as_str()?;
            let raw_message = base64::engine::general_purpose::STANDARD
                .decode(raw_b64)
                .ok()?;
            let from = payload.get("from")?.as_str()?.to_string();
            let to: Vec<String> =
                serde_json::from_value(payload.get("to").cloned().unwrap_or_default())
                    .unwrap_or_default();
            let cc: Vec<String> =
                serde_json::from_value(payload.get("cc").cloned().unwrap_or_default())
                    .unwrap_or_default();
            let bcc: Vec<String> =
                serde_json::from_value(payload.get("bcc").cloned().unwrap_or_default())
                    .unwrap_or_default();
            let subject = payload
                .get("subject")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            if to.is_empty() && cc.is_empty() && bcc.is_empty() {
                log::warn!("outbox_to_mail_op: rejected send op with no recipients");
                return None;
            }
            Some(MailOp::SendRaw {
                raw_message,
                from,
                to,
                cc,
                bcc,
                subject,
            })
        }
        _ => None,
    }
}

const MAX_RETRIES: i32 = 5;

/// Check if an entry has exceeded the retry limit.
pub fn is_dead(entry: &OutboxEntry) -> bool {
    entry.retry_count >= MAX_RETRIES
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE outbox (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id TEXT NOT NULL,
                action_type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                status TEXT DEFAULT 'pending',
                retry_count INTEGER DEFAULT 0,
                error_message TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_queue_and_get_pending() {
        let conn = setup_db();
        let payload = serde_json::json!({"by_folder": {"INBOX": [1, 2]}});
        queue_offline_op(&conn, "acc1", "delete", &payload).unwrap();
        queue_offline_op(&conn, "acc1", "set_flags", &serde_json::json!({})).unwrap();

        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert_eq!(pending.len(), 2);
        // flags should come before deletes (replay order)
        assert_eq!(pending[0].action_type, "set_flags");
        assert_eq!(pending[1].action_type, "delete");
    }

    #[test]
    fn test_mark_completed_removes_entry() {
        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", "delete", &serde_json::json!({})).unwrap();
        mark_completed(&conn, id).unwrap();
        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn test_mark_failed_increments_retry() {
        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", "move", &serde_json::json!({})).unwrap();
        mark_failed(&conn, id, "network error").unwrap();
        mark_failed(&conn, id, "network error").unwrap();

        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert_eq!(pending[0].retry_count, 2);
    }

    #[test]
    fn test_dead_after_max_retries() {
        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", "delete", &serde_json::json!({})).unwrap();
        for _ in 0..5 {
            mark_failed(&conn, id, "timeout").unwrap();
        }
        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert!(is_dead(&pending[0]));

        mark_dead(&conn, id).unwrap();
        let dead = get_dead_ops(&conn, "acc1").unwrap();
        assert_eq!(dead.len(), 1);
        // Should no longer be in pending
        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn test_roundtrip_mail_op() {
        let op = MailOp::MoveMessages {
            by_folder: HashMap::from([("INBOX".to_string(), vec![1, 2, 3])]),
            target_folder: "Trash".to_string(),
        };
        let (action_type, payload) = mail_op_to_outbox(&op).unwrap();
        assert_eq!(action_type, "move");

        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", action_type, &payload).unwrap();
        let pending = get_pending_ops(&conn, "acc1").unwrap();
        let restored = outbox_to_mail_op(&pending[0]).unwrap();

        match restored {
            MailOp::MoveMessages {
                by_folder,
                target_folder,
            } => {
                assert_eq!(target_folder, "Trash");
                assert_eq!(by_folder["INBOX"], vec![1, 2, 3]);
            }
            _ => panic!("Expected MoveMessages"),
        }
        mark_completed(&conn, id).unwrap();
    }
}
