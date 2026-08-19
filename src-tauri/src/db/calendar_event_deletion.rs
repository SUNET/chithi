//! Meeting-aware local calendar event deletion.
//!
//! Callers must pass a connection inside a transaction. Keeping transaction
//! ownership at the call site lets sync code batch reconciliation changes and
//! avoids nested transactions when a command already has one open.

use rusqlite::{params, params_from_iter, Connection};

use crate::error::Result;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeletionResult {
    pub deleted: usize,
    pub cleanup_lifecycle_ids: Vec<String>,
}

/// Queue a bound meeting, if present, and delete one event.
///
/// This function is transaction-compatible but does not open or commit a
/// transaction. The caller must include it in its local deletion transaction.
pub fn delete_event(conn: &Connection, event_id: &str) -> Result<DeletionResult> {
    delete_events(conn, &[event_id.to_string()])
}

/// Queue bound meetings and delete all supplied events as one transaction unit.
pub fn delete_events(conn: &Connection, event_ids: &[String]) -> Result<DeletionResult> {
    if conn.is_autocommit() {
        return Err(crate::error::Error::Other(
            "Meeting-aware event deletion requires a caller-owned transaction".into(),
        ));
    }
    let mut result = DeletionResult::default();

    for event_id in event_ids {
        let lifecycle_id = uuid::Uuid::new_v4().to_string();
        let queued = conn.execute(
            "INSERT INTO meet_pending_meetings
                (lifecycle_id, account_id, protocol, meeting_id, join_url, created_at)
             SELECT ?1, account_id, protocol, meeting_id, join_url, CURRENT_TIMESTAMP
             FROM meet_meetings WHERE event_id = ?2",
            params![lifecycle_id, event_id],
        )?;
        let deleted = conn.execute(
            "DELETE FROM calendar_events WHERE id = ?1",
            params![event_id],
        )?;

        if queued != 0 && deleted == 0 {
            return Err(crate::error::Error::Other(format!(
                "Meeting cleanup was queued for missing calendar event {event_id}"
            )));
        }
        result.deleted += deleted;
        if queued != 0 {
            result.cleanup_lifecycle_ids.push(lifecycle_id);
        }
    }

    Ok(result)
}

pub fn delete_calendar_events(conn: &Connection, calendar_id: &str) -> Result<DeletionResult> {
    let event_ids = select_event_ids(
        conn,
        "SELECT id FROM calendar_events WHERE calendar_id = ?1",
        &[calendar_id],
    )?;
    delete_events(conn, &event_ids)
}

pub fn delete_account_events(conn: &Connection, account_id: &str) -> Result<DeletionResult> {
    let event_ids = select_event_ids(
        conn,
        "SELECT id FROM calendar_events WHERE account_id = ?1",
        &[account_id],
    )?;
    delete_events(conn, &event_ids)
}

pub fn delete_events_by_remote_id(
    conn: &Connection,
    account_id: &str,
    remote_id: &str,
) -> Result<DeletionResult> {
    let event_ids = select_event_ids(
        conn,
        "SELECT id FROM calendar_events WHERE account_id = ?1 AND remote_id = ?2",
        &[account_id, remote_id],
    )?;
    delete_events(conn, &event_ids)
}

pub fn delete_unpushed_events_by_uid(
    conn: &Connection,
    account_id: &str,
    uid: &str,
) -> Result<DeletionResult> {
    let event_ids = select_event_ids(
        conn,
        "SELECT id FROM calendar_events
         WHERE account_id = ?1 AND uid = ?2 AND remote_id IS NULL",
        &[account_id, uid],
    )?;
    delete_events(conn, &event_ids)
}

fn select_event_ids(conn: &Connection, sql: &str, values: &[&str]) -> Result<Vec<String>> {
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map(params_from_iter(values.iter()), |row| row.get(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn connection() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        db::schema::initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('account', 'Test', 'test@example.test', 'test@example.test')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendars (id, account_id, name) VALUES ('calendar', 'account', 'Test')",
            [],
        )
        .unwrap();
        conn
    }

    fn insert_event(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO calendar_events
                (id, account_id, calendar_id, title, start_time, end_time)
             VALUES (?1, 'account', 'calendar', ?1, '2026-08-19', '2026-08-20')",
            [id],
        )
        .unwrap();
    }

    fn bind(conn: &Connection, event_id: &str) {
        conn.execute(
            "INSERT INTO meet_meetings
                (event_id, account_id, protocol, meeting_id, join_url)
             VALUES (?1, 'account', 'zoom', ?1, 'https://example.test/join')",
            [event_id],
        )
        .unwrap();
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    #[test]
    fn deletes_unbound_event_without_queueing() {
        let mut conn = connection();
        insert_event(&conn, "event");

        let transaction = conn.transaction().unwrap();
        let result = delete_event(&transaction, "event").unwrap();
        transaction.commit().unwrap();

        assert_eq!(result.deleted, 1);
        assert!(result.cleanup_lifecycle_ids.is_empty());
        assert_eq!(count(&conn, "calendar_events"), 0);
        assert_eq!(count(&conn, "meet_pending_meetings"), 0);
    }

    #[test]
    fn refuses_deletion_without_a_transaction() {
        let conn = connection();
        insert_event(&conn, "event");

        assert!(delete_event(&conn, "event").is_err());
        assert_eq!(count(&conn, "calendar_events"), 1);
    }

    #[test]
    fn moves_bound_event_to_durable_queue_before_deletion() {
        let mut conn = connection();
        insert_event(&conn, "event");
        bind(&conn, "event");

        let transaction = conn.transaction().unwrap();
        let result = delete_event(&transaction, "event").unwrap();
        transaction.commit().unwrap();

        assert_eq!(result.deleted, 1);
        assert_eq!(result.cleanup_lifecycle_ids.len(), 1);
        assert_eq!(count(&conn, "calendar_events"), 0);
        assert_eq!(count(&conn, "meet_meetings"), 0);
        assert_eq!(count(&conn, "meet_pending_meetings"), 1);
    }

    #[test]
    fn bulk_deletion_queues_only_bound_events() {
        let mut conn = connection();
        for id in ["bound", "unbound", "other"] {
            insert_event(&conn, id);
        }
        bind(&conn, "bound");

        let transaction = conn.transaction().unwrap();
        let result =
            delete_events(&transaction, &["bound".to_string(), "unbound".to_string()]).unwrap();
        transaction.commit().unwrap();

        assert_eq!(result.deleted, 2);
        assert_eq!(result.cleanup_lifecycle_ids.len(), 1);
        assert_eq!(count(&conn, "calendar_events"), 1);
        assert_eq!(count(&conn, "meet_pending_meetings"), 1);
    }

    #[test]
    fn queue_failure_rolls_back_the_entire_bulk_deletion() {
        let mut conn = connection();
        insert_event(&conn, "first");
        insert_event(&conn, "second");
        bind(&conn, "second");
        conn.execute_batch(
            "CREATE TRIGGER reject_pending_meeting
             BEFORE INSERT ON meet_pending_meetings
             BEGIN SELECT RAISE(ABORT, 'queue rejected'); END;",
        )
        .unwrap();

        let transaction = conn.transaction().unwrap();
        assert!(delete_events(&transaction, &["first".to_string(), "second".to_string()]).is_err());
        transaction.rollback().unwrap();

        assert_eq!(count(&conn, "calendar_events"), 2);
        assert_eq!(count(&conn, "meet_meetings"), 1);
        assert_eq!(count(&conn, "meet_pending_meetings"), 0);
    }
}
