//! Durable foreground write requests, separate from the remote display mirror.
//! A failed request is recorded, not automatically replayed or rendered.

use crate::error::{Error, Result};
use rusqlite::{params, Connection};

pub(crate) fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS calendar_write_requests (
        id TEXT PRIMARY KEY, account_id TEXT NOT NULL, event_id TEXT NOT NULL,
        kind TEXT NOT NULL, request TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'applying',
        error TEXT, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
    );",
    )?;
    Ok(())
}

pub(crate) fn begin(
    conn: &Connection,
    account: &str,
    event: &str,
    kind: &str,
    request: &impl serde::Serialize,
) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let request = serde_json::to_string(request)
        .map_err(|error| Error::Other(format!("Cannot journal calendar write: {error}")))?;
    conn.execute("INSERT INTO calendar_write_requests(id, account_id, event_id, kind, request) VALUES (?1, ?2, ?3, ?4, ?5)", params![id, account, event, kind, request])?;
    Ok(id)
}

pub(crate) fn finish(conn: &Connection, id: &str, result: &Result<()>) -> Result<()> {
    conn.execute(
        "UPDATE calendar_write_requests SET status = ?1, error = ?2 WHERE id = ?3",
        params![
            if result.is_ok() {
                "remote-confirmed"
            } else {
                "needs-refresh"
            },
            result.as_ref().err().map(ToString::to_string),
            id
        ],
    )?;
    Ok(())
}

/// Persist provider addressing before any canonical read or local publication.
/// These receipts are recovery evidence and never supply display content.
pub(crate) fn created(
    conn: &Connection,
    id: &str,
    remote: &str,
    uid: Option<&str>,
    revision: Option<&str>,
) -> Result<()> {
    let outcome = serde_json::json!({"remote_id": remote, "uid": uid, "revision": revision});
    conn.execute("UPDATE calendar_write_requests SET request = json_set(request, '$.remote_result', json(?1)), status = 'remote-confirmed', error = NULL WHERE id = ?2", params![outcome.to_string(), id])?;
    Ok(())
}

/// Completion is atomic with local publication, not just the HTTP response.
pub(crate) fn published(conn: &Connection, id: &str) -> Result<()> {
    if conn.is_autocommit() {
        return Err(Error::Other(
            "Calendar write completion requires a publication transaction".into(),
        ));
    }
    conn.execute(
        "UPDATE calendar_write_requests SET status = 'completed', error = NULL WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmed_transport_is_not_completed_until_publication_commits() {
        let mut conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        let id = begin(
            &conn,
            "account",
            "event",
            "create",
            &serde_json::json!({"input": {"title": "Requested"}}),
        )
        .unwrap();
        created(
            &conn,
            &id,
            "remote-event",
            Some("canonical-uid"),
            Some("revision"),
        )
        .unwrap();
        let status = |conn: &Connection| -> String {
            conn.query_row(
                "SELECT status FROM calendar_write_requests WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(status(&conn), "remote-confirmed");
        assert!(published(&conn, &id).is_err());
        let tx = conn.transaction().unwrap();
        published(&tx, &id).unwrap();
        tx.rollback().unwrap();
        assert_eq!(status(&conn), "remote-confirmed");
        let tx = conn.transaction().unwrap();
        published(&tx, &id).unwrap();
        tx.commit().unwrap();
        assert_eq!(status(&conn), "completed");
        let remote: String = conn.query_row("SELECT json_extract(request, '$.remote_result.remote_id') FROM calendar_write_requests WHERE id = ?1", [&id], |row| row.get(0)).unwrap();
        assert_eq!(remote, "remote-event");
    }

    #[test]
    fn uncertain_write_keeps_the_request_and_error_without_publishing_success() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        let request = serde_json::json!({"title": "Requested"});
        let id = begin(&conn, "account", "event", "update", &request).unwrap();
        finish(&conn, &id, &Err(Error::Sync("Response lost".into()))).unwrap();
        let (status, saved, error): (String, String, String) = conn
            .query_row(
                "SELECT status, request, error FROM calendar_write_requests WHERE id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "needs-refresh");
        assert_eq!(saved, request.to_string());
        assert!(error.contains("Response lost"));
    }
}
