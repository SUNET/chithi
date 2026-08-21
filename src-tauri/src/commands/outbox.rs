//! Tauri commands for the Outbox view.
//!
//! Surfaces the `outbox` table to the renderer: list pending+failed+dead
//! send rows for an account, manually retry a row, or discard one.

use serde::Serialize;
use tauri::State;

use crate::error::{Error, Result};
use crate::state::AppState;

/// A single outbox row as seen by the renderer. Restricted to the
/// fields the Outbox view actually displays so we don't leak the raw
/// message payload through IPC.
#[derive(Debug, Serialize)]
pub struct OutboxRow {
    pub id: i64,
    pub account_id: String,
    pub action_type: String,
    pub status: String,
    pub retry_count: i32,
    pub error_message: Option<String>,
    pub delivery_outcome_unknown: bool,
    pub subject: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
}

fn parse_string_array(payload: &serde_json::Value, key: &str) -> Vec<String> {
    payload
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn row_from_payload(
    id: i64,
    account_id: String,
    action_type: String,
    status: String,
    retry_count: i32,
    error_message: Option<String>,
    payload_json: &str,
) -> OutboxRow {
    let payload: serde_json::Value =
        serde_json::from_str(payload_json).unwrap_or(serde_json::Value::Null);
    let subject = payload
        .get("subject")
        .and_then(|s| s.as_str())
        .map(String::from);
    let to = parse_string_array(&payload, "to");
    let cc = parse_string_array(&payload, "cc");
    let bcc = parse_string_array(&payload, "bcc");
    let delivery_outcome_unknown = error_message
        .as_deref()
        .is_some_and(crate::ops::offline::is_indeterminate_delivery_error_message);
    OutboxRow {
        id,
        account_id,
        action_type,
        status,
        retry_count,
        error_message,
        delivery_outcome_unknown,
        subject,
        to,
        cc,
        bcc,
    }
}

fn retry_dead_row(conn: &rusqlite::Connection, account_id: &str, outbox_id: i64) -> Result<()> {
    let changed = conn
        .execute(
            "UPDATE outbox
             SET status = 'pending', retry_count = 0, error_message = NULL
             WHERE id = ?1 AND account_id = ?2
               AND action_type = 'send' AND status = 'dead'",
            rusqlite::params![outbox_id, account_id],
        )
        .map_err(Error::Database)?;
    if changed != 1 {
        return Err(Error::Other(format!(
            "Outbox row {} is not eligible for manual retry",
            outbox_id
        )));
    }
    Ok(())
}

fn discard_inactive_row(
    conn: &rusqlite::Connection,
    account_id: &str,
    outbox_id: i64,
) -> Result<()> {
    let changed = conn
        .execute(
            "DELETE FROM outbox
             WHERE id = ?1 AND account_id = ?2 AND action_type = 'send'
               AND status IN ('pending', 'dead')",
            rusqlite::params![outbox_id, account_id],
        )
        .map_err(Error::Database)?;
    if changed != 1 {
        return Err(Error::Other(format!(
            "Outbox row {} is not eligible for discard",
            outbox_id
        )));
    }
    Ok(())
}

/// List all outbox rows for an account that are visible to the user:
/// 'pending' (waiting for next replay), 'sending' (first attempt in
/// flight), and 'dead' (gave up). Ordered newest first.
#[tauri::command]
pub async fn list_outbox(state: State<'_, AppState>, account_id: String) -> Result<Vec<OutboxRow>> {
    let conn = state.db.reader();
    let mut stmt = conn
        .prepare(
            "SELECT id, account_id, action_type, status, retry_count, error_message, payload_json
             FROM outbox
             WHERE account_id = ?1 AND action_type = 'send'
               AND status IN ('pending', 'sending', 'dead')
             ORDER BY id DESC",
        )
        .map_err(Error::Database)?;
    let rows = stmt
        .query_map(rusqlite::params![account_id], |row| {
            let payload: String = row.get(6)?;
            Ok(row_from_payload(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                &payload,
            ))
        })
        .map_err(Error::Database)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Flip a dead row back to 'pending' with retry_count reset so the next sync
/// drain will replay it. Pending and actively sending rows are not mutable.
#[tauri::command]
pub async fn retry_outbox_op(
    state: State<'_, AppState>,
    account_id: String,
    outbox_id: i64,
) -> Result<()> {
    let conn = state.db.writer().await;
    retry_dead_row(&conn, &account_id, outbox_id)?;
    log::info!("Outbox row {} requeued for retry", outbox_id);
    Ok(())
}

/// Permanently delete an inactive outbox row. A sending row retains its claim.
#[tauri::command]
pub async fn discard_outbox_op(
    state: State<'_, AppState>,
    account_id: String,
    outbox_id: i64,
) -> Result<()> {
    let conn = state.db.writer().await;
    discard_inactive_row(&conn, &account_id, outbox_id)?;
    log::info!("Outbox row {} discarded", outbox_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE outbox (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id TEXT NOT NULL,
                action_type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                status TEXT NOT NULL,
                retry_count INTEGER NOT NULL,
                error_message TEXT
            );",
        )
        .unwrap();
        conn
    }

    fn insert_row(conn: &rusqlite::Connection, account_id: &str, status: &str) -> i64 {
        conn.execute(
            "INSERT INTO outbox
             (account_id, action_type, payload_json, status, retry_count, error_message)
             VALUES (?1, 'send', '{}', ?2, 3, 'old error')",
            rusqlite::params![account_id, status],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn row_state(conn: &rusqlite::Connection, id: i64) -> Option<(String, i32, Option<String>)> {
        conn.query_row(
            "SELECT status, retry_count, error_message FROM outbox WHERE id = ?1",
            rusqlite::params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok()
    }

    #[test]
    fn manual_retry_only_transitions_dead_to_pending() {
        let conn = setup_db();
        let dead = insert_row(&conn, "acc1", "dead");
        let pending = insert_row(&conn, "acc1", "pending");
        let sending = insert_row(&conn, "acc1", "sending");

        retry_dead_row(&conn, "acc1", dead).unwrap();
        assert_eq!(row_state(&conn, dead), Some(("pending".into(), 0, None)));
        assert!(retry_dead_row(&conn, "acc1", dead).is_err());
        assert!(retry_dead_row(&conn, "acc1", pending).is_err());
        assert!(retry_dead_row(&conn, "acc1", sending).is_err());
        assert!(retry_dead_row(&conn, "acc1", i64::MAX).is_err());
        assert_eq!(row_state(&conn, sending).unwrap().0, "sending");
    }

    #[test]
    fn discard_only_deletes_pending_or_dead_rows() {
        let conn = setup_db();
        let dead = insert_row(&conn, "acc1", "dead");
        let pending = insert_row(&conn, "acc1", "pending");
        let sending = insert_row(&conn, "acc1", "sending");

        discard_inactive_row(&conn, "acc1", dead).unwrap();
        discard_inactive_row(&conn, "acc1", pending).unwrap();
        assert!(row_state(&conn, dead).is_none());
        assert!(row_state(&conn, pending).is_none());
        assert!(discard_inactive_row(&conn, "acc1", sending).is_err());
        assert!(discard_inactive_row(&conn, "acc1", i64::MAX).is_err());
        assert_eq!(row_state(&conn, sending).unwrap().0, "sending");
    }

    #[test]
    fn manual_actions_reject_rows_from_another_account() {
        let conn = setup_db();
        let retry_row = insert_row(&conn, "acc1", "dead");
        let discard_row = insert_row(&conn, "acc1", "pending");

        assert!(retry_dead_row(&conn, "acc2", retry_row).is_err());
        assert!(discard_inactive_row(&conn, "acc2", discard_row).is_err());
        assert_eq!(row_state(&conn, retry_row).unwrap().0, "dead");
        assert_eq!(row_state(&conn, discard_row).unwrap().0, "pending");
    }

    #[test]
    fn renderer_row_distinguishes_unknown_delivery_from_failure() {
        let unknown = row_from_payload(
            1,
            "acc1".into(),
            "send".into(),
            "dead".into(),
            0,
            Some(crate::ops::offline::INDETERMINATE_DELIVERY_ERROR_MESSAGE.into()),
            r#"{"subject":"test"}"#,
        );
        assert!(unknown.delivery_outcome_unknown);

        let failed = row_from_payload(
            2,
            "acc1".into(),
            "send".into(),
            "dead".into(),
            5,
            Some("SMTP server rejected the message".into()),
            "{}",
        );
        assert!(!failed.delivery_outcome_unknown);
    }
}
