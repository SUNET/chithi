//! Durable ownership for remote meetings not currently bound to an event.

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMeeting {
    pub lifecycle_id: String,
    pub account_id: String,
    pub protocol: String,
    pub meeting_id: String,
    pub join_url: String,
    pub created_at: String,
    pub cleanup_requested: bool,
}

pub fn insert(conn: &Connection, meeting: &PendingMeeting) -> Result<()> {
    conn.execute(
        "INSERT INTO meet_pending_meetings
            (lifecycle_id, account_id, protocol, meeting_id, join_url, created_at,
             cleanup_requested)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            meeting.lifecycle_id,
            meeting.account_id,
            meeting.protocol,
            meeting.meeting_id,
            meeting.join_url,
            meeting.created_at,
            meeting.cleanup_requested,
        ],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, lifecycle_id: &str) -> Result<Option<PendingMeeting>> {
    Ok(conn
        .query_row(
            "SELECT lifecycle_id, account_id, protocol, meeting_id, join_url, created_at,
                    cleanup_requested
             FROM meet_pending_meetings WHERE lifecycle_id = ?1",
            params![lifecycle_id],
            row,
        )
        .optional()?)
}

pub fn list(conn: &Connection) -> Result<Vec<PendingMeeting>> {
    let mut statement = conn.prepare(
        "SELECT lifecycle_id, account_id, protocol, meeting_id, join_url, created_at,
                cleanup_requested
         FROM meet_pending_meetings ORDER BY created_at, lifecycle_id",
    )?;
    let rows = statement.query_map([], row)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn list_cleanup_requested(conn: &Connection) -> Result<Vec<PendingMeeting>> {
    let mut statement = conn.prepare(
        "SELECT lifecycle_id, account_id, protocol, meeting_id, join_url, created_at,
                cleanup_requested
         FROM meet_pending_meetings WHERE cleanup_requested = 1
         ORDER BY created_at, lifecycle_id",
    )?;
    let rows = statement.query_map([], row)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn list_cleanup_requested_for_account(
    conn: &Connection,
    account_id: &str,
) -> Result<Vec<PendingMeeting>> {
    let mut statement = conn.prepare(
        "SELECT lifecycle_id, account_id, protocol, meeting_id, join_url, created_at,
                cleanup_requested
         FROM meet_pending_meetings
         WHERE cleanup_requested = 1 AND account_id = ?1
         ORDER BY created_at, lifecycle_id",
    )?;
    let rows = statement.query_map(params![account_id], row)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn request_cleanup(conn: &Connection, lifecycle_id: &str) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE meet_pending_meetings SET cleanup_requested = 1
         WHERE lifecycle_id = ?1",
        params![lifecycle_id],
    )? != 0)
}

pub fn delete(conn: &Connection, lifecycle_id: &str) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM meet_pending_meetings WHERE lifecycle_id = ?1",
        params![lifecycle_id],
    )? != 0)
}

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingMeeting> {
    Ok(PendingMeeting {
        lifecycle_id: row.get(0)?,
        account_id: row.get(1)?,
        protocol: row.get(2)?,
        meeting_id: row.get(3)?,
        join_url: row.get(4)?,
        created_at: row.get(5)?,
        cleanup_requested: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE meet_pending_meetings (
                lifecycle_id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                protocol TEXT NOT NULL,
                meeting_id TEXT NOT NULL,
                join_url TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                cleanup_requested INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
        conn
    }

    fn pending(id: &str) -> PendingMeeting {
        PendingMeeting {
            lifecycle_id: id.into(),
            account_id: "account".into(),
            protocol: "zoom".into(),
            meeting_id: "meeting".into(),
            join_url: "https://example.test/join".into(),
            created_at: "2026-08-11T20:00:00Z".into(),
            cleanup_requested: false,
        }
    }

    #[test]
    fn crud_and_list() {
        let conn = connection();
        let first = pending("first");
        let mut second = pending("second");
        second.created_at = "2026-08-11T21:00:00Z".into();
        second.cleanup_requested = true;

        insert(&conn, &first).unwrap();
        insert(&conn, &second).unwrap();
        assert_eq!(get(&conn, "first").unwrap(), Some(first.clone()));
        assert_eq!(list(&conn).unwrap(), vec![first, second.clone()]);
        assert_eq!(list_cleanup_requested(&conn).unwrap(), vec![second.clone()]);
        assert_eq!(
            list_cleanup_requested_for_account(&conn, "account").unwrap(),
            vec![second.clone()]
        );
        assert!(request_cleanup(&conn, "first").unwrap());
        assert!(get(&conn, "first").unwrap().unwrap().cleanup_requested);
        assert!(!request_cleanup(&conn, "missing").unwrap());
        assert!(delete(&conn, "first").unwrap());
        assert!(!delete(&conn, "first").unwrap());
        assert!(get(&conn, "first").unwrap().is_none());
    }

    #[test]
    fn transaction_failure_retains_pending_ownership() {
        let mut conn = connection();
        let meeting = pending("claim");
        insert(&conn, &meeting).unwrap();

        {
            let transaction = conn.transaction().unwrap();
            delete(&transaction, "claim").unwrap();
            transaction.rollback().unwrap();
        }

        assert_eq!(get(&conn, "claim").unwrap(), Some(meeting));
    }
}
