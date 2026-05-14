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
    OutboxRow {
        id,
        account_id,
        action_type,
        status,
        retry_count,
        error_message,
        subject,
        to,
        cc,
        bcc,
    }
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
             WHERE account_id = ?1 AND status IN ('pending', 'sending', 'dead')
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

/// Flip a dead or pending row back to 'pending' with retry_count reset
/// so the next sync drain will replay it.
#[tauri::command]
pub async fn retry_outbox_op(state: State<'_, AppState>, outbox_id: i64) -> Result<()> {
    let conn = state.db.writer().await;
    conn.execute(
        "UPDATE outbox SET status = 'pending', retry_count = 0, error_message = NULL
         WHERE id = ?1",
        rusqlite::params![outbox_id],
    )
    .map_err(Error::Database)?;
    log::info!("Outbox row {} requeued for retry", outbox_id);
    Ok(())
}

/// Permanently delete an outbox row.
#[tauri::command]
pub async fn discard_outbox_op(state: State<'_, AppState>, outbox_id: i64) -> Result<()> {
    let conn = state.db.writer().await;
    conn.execute(
        "DELETE FROM outbox WHERE id = ?1",
        rusqlite::params![outbox_id],
    )
    .map_err(Error::Database)?;
    log::info!("Outbox row {} discarded", outbox_id);
    Ok(())
}
