//! Persistent, opaque mutation tokens for calendar MOVE revalidation.
//!
//! Schema triggers allocate tokens for every event write and related meeting
//! write, including no-ops and replacement. AUTOINCREMENT preserves uniqueness
//! across committed deletions and restarts; rolled-back allocations roll back
//! with their data and must not be published as committed revisions.

use rusqlite::Connection;

use crate::error::Result;

/// Read the event's durable token, failing closed if either row is missing.
///
/// Read the event, meeting, and token in the same caller-owned transaction on
/// this connection. Revalidation belongs in the transaction that commits the
/// mutation; this function does not open a separate snapshot or synthesize a
/// token from event fields or timestamps.
pub(crate) fn get(conn: &Connection, event_id: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT r.revision FROM calendar_event_revisions AS r
         JOIN calendar_events AS e ON e.id = r.event_id
         WHERE r.event_id = ?1",
        [event_id],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;

    fn initialize(conn: &Connection, recursive: bool) {
        conn.pragma_update(None, "recursive_triggers", recursive)
            .unwrap();
        schema::initialize(conn).unwrap();
    }

    fn seed_account(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('account', 'Test', 'test@example.test', 'test@example.test');",
        )
        .unwrap();
    }

    fn connection(recursive: bool) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn, recursive);
        seed_account(&conn);
        conn
    }

    fn insert_event(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO calendar_events
                 (id, account_id, calendar_id, title, start_time, end_time,
                  created_at, updated_at)
             VALUES (?1, 'account', 'calendar', 'Event', '2026-09-13', '2026-09-14',
                     '2026-09-13 12:00:00', '2026-09-13 12:00:00')",
            [id],
        )
        .unwrap();
    }

    fn bind(conn: &Connection, event_id: &str) {
        conn.execute(
            "INSERT INTO meet_meetings
                 (event_id, account_id, protocol, meeting_id, join_url)
             VALUES (?1, 'account', 'zoom', 'meeting', 'https://example.test/join')",
            [event_id],
        )
        .unwrap();
    }

    fn revision_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM calendar_event_revisions", [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    fn assert_missing(conn: &Connection, id: &str) {
        assert!(matches!(
            get(conn, id),
            Err(crate::error::Error::Database(
                rusqlite::Error::QueryReturnedNoRows
            ))
        ));
    }

    fn remove_revision_schema(conn: &Connection) {
        conn.execute_batch(
            "DROP TRIGGER calendar_event_revision_insert;
             DROP TRIGGER calendar_event_revision_update;
             DROP TRIGGER calendar_event_revision_delete;
             DROP TRIGGER calendar_event_revision_meeting_insert;
             DROP TRIGGER calendar_event_revision_meeting_update;
             DROP TRIGGER calendar_event_revision_meeting_delete;
             DROP TABLE calendar_event_revisions;",
        )
        .unwrap();
    }

    #[test]
    fn migration_seeds_legacy_rows_and_preserves_tokens_across_restarts() {
        for recursive in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("calendar-revisions.db");
            let initial;
            {
                let conn = Connection::open(&path).unwrap();
                initialize(&conn, recursive);
                seed_account(&conn);
                remove_revision_schema(&conn);
                insert_event(&conn, "event");
                bind(&conn, "event");
                initialize(&conn, recursive);
                initial = get(&conn, "event").unwrap();
                assert!(initial > 0);
                assert_eq!(revision_count(&conn), 1);
            }

            for _ in 0..2 {
                let conn = Connection::open(&path).unwrap();
                initialize(&conn, recursive);
                assert_eq!(get(&conn, "event").unwrap(), initial);
                assert_eq!(revision_count(&conn), 1);
                assert_eq!(
                    conn.query_row::<i64, _, _>(
                        "SELECT COUNT(*) FROM calendar_invitation_recurrence",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap(),
                    0
                );
            }

            let conn = Connection::open(&path).unwrap();
            initialize(&conn, recursive);
            insert_event(&conn, "missing");
            conn.execute(
                "DELETE FROM calendar_event_revisions WHERE event_id = 'missing'",
                [],
            )
            .unwrap();
            assert_missing(&conn, "missing");
            initialize(&conn, recursive);
            assert_eq!(get(&conn, "event").unwrap(), initial);
            assert!(get(&conn, "missing").unwrap() > initial);
            assert_eq!(revision_count(&conn), 2);
        }
    }

    #[test]
    fn migration_failure_rolls_back_trigger_installation_and_seeding() {
        for recursive in [false, true] {
            let conn = connection(recursive);
            remove_revision_schema(&conn);
            insert_event(&conn, "a-event");
            insert_event(&conn, "b-event");
            conn.execute_batch(
                "DROP TRIGGER calendar_invitation_recurrence_insert;
                 DROP TRIGGER calendar_invitation_recurrence_update;
                 DROP TRIGGER calendar_invitation_recurrence_delete;
                 DROP TABLE calendar_invitation_recurrence;
                 CREATE TABLE calendar_event_revisions (
                     revision INTEGER PRIMARY KEY AUTOINCREMENT,
                     event_id TEXT NOT NULL UNIQUE
                 );
                 CREATE TRIGGER interrupt_revision_seed
                 BEFORE INSERT ON calendar_event_revisions
                 WHEN NEW.event_id = 'b-event'
                 BEGIN SELECT RAISE(ABORT, 'interrupted revision migration'); END;",
            )
            .unwrap();

            assert!(schema::initialize(&conn).is_err());
            assert!(conn.is_autocommit());
            assert_eq!(revision_count(&conn), 0);
            let installed: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_schema
                     WHERE name IN ('calendar_event_revision_insert',
                                    'calendar_invitation_recurrence')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(installed, 0);
            conn.execute_batch("DROP TRIGGER interrupt_revision_seed;")
                .unwrap();
            initialize(&conn, recursive);
            assert_ne!(
                get(&conn, "a-event").unwrap(),
                get(&conn, "b-event").unwrap()
            );
            assert_eq!(revision_count(&conn), 2);
        }
    }

    #[test]
    fn every_event_column_write_including_hidden_state_and_noops_revises() {
        for recursive in [false, true] {
            let conn = connection(recursive);
            insert_event(&conn, "event");
            insert_event(&conn, "untouched");
            let untouched = get(&conn, "untouched").unwrap();
            let columns: Vec<String> = conn
                .prepare("SELECT name FROM pragma_table_info('calendar_events')")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            for column in columns {
                let before = get(&conn, "event").unwrap();
                conn.execute(
                    &format!(
                        "UPDATE calendar_events SET \"{column}\" = \"{column}\" WHERE id = 'event'"
                    ),
                    [],
                )
                .unwrap();
                assert!(get(&conn, "event").unwrap() > before, "{column}");
                assert_eq!(get(&conn, "untouched").unwrap(), untouched);
            }

            for assignment in [
                "pending_rsvp_status = 'ACCEPTED'",
                "manually_managed_at = '2026-09-13 12:00:00'",
                "source_message_id = 'message'",
                "ical_data = 'retained invitation'",
                "etag = 'new-etag'",
                "created_at = '2026-09-13 12:00:01'",
                "updated_at = '2026-09-13 12:00:01'",
            ] {
                let before = get(&conn, "event").unwrap();
                conn.execute(
                    &format!("UPDATE calendar_events SET {assignment} WHERE id = 'event'"),
                    [],
                )
                .unwrap();
                assert!(get(&conn, "event").unwrap() > before, "{assignment}");
            }
            assert_eq!(revision_count(&conn), 2);
        }
    }

    #[test]
    fn same_second_noop_and_aba_writes_cannot_restore_a_token() {
        for recursive in [false, true] {
            let conn = connection(recursive);
            insert_event(&conn, "event");
            let original = get(&conn, "event").unwrap();
            let mut previous = original;
            for title in ["Event", "Changed", "Event"] {
                conn.execute(
                    "UPDATE calendar_events SET title = ?1 WHERE id = 'event'",
                    [title],
                )
                .unwrap();
                let current = get(&conn, "event").unwrap();
                assert!(current > previous);
                previous = current;
            }
            let data: (String, String) = conn
                .query_row(
                    "SELECT title, updated_at FROM calendar_events WHERE id = 'event'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(data, ("Event".into(), "2026-09-13 12:00:00".into()));
            assert_ne!(get(&conn, "event").unwrap(), original);
        }
    }

    #[test]
    fn event_insert_upsert_and_outer_conflict_policies_allocate_tokens() {
        for recursive in [false, true] {
            let conn = connection(recursive);
            insert_event(&conn, "event");
            for policy in ["", "OR ABORT", "OR FAIL", "OR IGNORE", "OR REPLACE"] {
                let before = get(&conn, "event").unwrap();
                conn.execute(
                    &format!(
                        "UPDATE {policy} calendar_events SET title = title WHERE id = 'event'"
                    ),
                    [],
                )
                .unwrap();
                assert!(get(&conn, "event").unwrap() > before, "{policy}");
            }

            for suffix in [
                "ON CONFLICT(id) DO UPDATE SET title = excluded.title",
                "ON CONFLICT(id) DO NOTHING",
            ] {
                let before = get(&conn, "event").unwrap();
                let changed = conn
                    .execute(
                        &format!(
                            "INSERT INTO calendar_events
                                 (id, account_id, calendar_id, title, start_time, end_time)
                             VALUES ('event', 'account', 'calendar', 'Event',
                                     '2026-09-13', '2026-09-14') {suffix}"
                        ),
                        [],
                    )
                    .unwrap();
                assert_eq!(get(&conn, "event").unwrap() != before, changed != 0);
            }
            bind(&conn, "event");
            let before = get(&conn, "event").unwrap();
            conn.execute_batch(
                "INSERT OR REPLACE INTO calendar_events
                 SELECT * FROM calendar_events WHERE id = 'event';",
            )
            .unwrap();
            assert!(get(&conn, "event").unwrap() > before);
            assert_eq!(revision_count(&conn), 1);
            let bindings: i64 = conn
                .query_row("SELECT COUNT(*) FROM meet_meetings", [], |row| row.get(0))
                .unwrap();
            assert_eq!(bindings, 0);
        }
    }

    #[test]
    fn meeting_insert_replacement_upsert_noop_aba_and_delete_revise() {
        for recursive in [false, true] {
            let conn = connection(recursive);
            insert_event(&conn, "event");
            insert_event(&conn, "untouched");
            let untouched = get(&conn, "untouched").unwrap();
            let before_binding = get(&conn, "event").unwrap();
            bind(&conn, "event");
            assert!(get(&conn, "event").unwrap() > before_binding);
            let columns: Vec<String> = conn
                .prepare("SELECT name FROM pragma_table_info('meet_meetings')")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            for column in columns {
                let before = get(&conn, "event").unwrap();
                conn.execute(
                    &format!(
                        "UPDATE meet_meetings SET \"{column}\" = \"{column}\"
                         WHERE event_id = 'event'"
                    ),
                    [],
                )
                .unwrap();
                assert!(get(&conn, "event").unwrap() > before, "{column}");
            }
            for sql in [
                "INSERT OR REPLACE INTO meet_meetings
                 SELECT * FROM meet_meetings WHERE event_id = 'event'",
                "INSERT INTO meet_meetings
                     (event_id, account_id, protocol, meeting_id, join_url)
                 VALUES ('event', 'account', 'zoom', 'meeting', 'https://example.test/join')
                 ON CONFLICT(event_id) DO UPDATE SET meeting_id = excluded.meeting_id",
                "UPDATE OR IGNORE meet_meetings SET meeting_id = meeting_id",
                "UPDATE meet_meetings SET meeting_id = 'other'",
                "UPDATE meet_meetings SET meeting_id = 'meeting'",
                "UPDATE meet_meetings SET created_at = '2026-09-13 12:00:00'",
                "DELETE FROM meet_meetings WHERE event_id = 'event'",
            ] {
                let before = get(&conn, "event").unwrap();
                conn.execute(sql, []).unwrap();
                assert!(get(&conn, "event").unwrap() > before, "{sql}");
                assert_eq!(get(&conn, "untouched").unwrap(), untouched);
            }
            assert_eq!(revision_count(&conn), 2);
        }
    }

    #[test]
    fn meeting_reassignment_and_destination_replacement_revise_both_events() {
        for recursive in [false, true] {
            for replace in [false, true] {
                let conn = connection(recursive);
                insert_event(&conn, "old");
                insert_event(&conn, "new");
                bind(&conn, "old");
                if replace {
                    bind(&conn, "new");
                }
                let old = get(&conn, "old").unwrap();
                let new = get(&conn, "new").unwrap();
                conn.execute_batch(
                    "UPDATE OR REPLACE meet_meetings SET event_id = 'new' WHERE event_id = 'old';",
                )
                .unwrap();
                assert!(get(&conn, "old").unwrap() > old.max(new));
                assert!(get(&conn, "new").unwrap() > old.max(new));
                assert_ne!(get(&conn, "old").unwrap(), get(&conn, "new").unwrap());
                assert_eq!(revision_count(&conn), 2);
            }
        }
    }

    #[test]
    fn root_id_reassignment_and_replacement_retire_both_previous_tokens() {
        for recursive in [false, true] {
            for replace in [false, true] {
                let conn = connection(recursive);
                insert_event(&conn, "old");
                if replace {
                    insert_event(&conn, "new");
                    bind(&conn, "new");
                }
                let high_water: i64 = conn
                    .query_row(
                        "SELECT MAX(revision) FROM calendar_event_revisions",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                conn.execute_batch(
                    "UPDATE OR REPLACE calendar_events SET id = 'new' WHERE id = 'old';",
                )
                .unwrap();
                assert_missing(&conn, "old");
                assert!(get(&conn, "new").unwrap() > high_water);
                assert_eq!(revision_count(&conn), 1);
                insert_event(&conn, "old");
                assert!(get(&conn, "old").unwrap() > high_water);
            }
        }
    }

    #[test]
    fn deleted_entries_are_pruned_and_sequence_survives_empty_table_and_restart() {
        for recursive in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("deleted-revisions.db");
            let high_water;
            {
                let conn = Connection::open(&path).unwrap();
                initialize(&conn, recursive);
                seed_account(&conn);
                insert_event(&conn, "event");
                bind(&conn, "event");
                high_water = get(&conn, "event").unwrap();
                conn.execute("DELETE FROM calendar_events WHERE id = 'event'", [])
                    .unwrap();
                assert_missing(&conn, "event");
                assert_eq!(revision_count(&conn), 0);
            }
            let conn = Connection::open(&path).unwrap();
            initialize(&conn, recursive);
            insert_event(&conn, "event");
            assert!(get(&conn, "event").unwrap() > high_water);
            bind(&conn, "event");
            insert_event(&conn, "other");
            bind(&conn, "other");
            conn.execute("DELETE FROM accounts WHERE id = 'account'", [])
                .unwrap();
            assert_eq!(revision_count(&conn), 0);
            assert_missing(&conn, "event");
            assert_missing(&conn, "other");
        }
    }

    #[test]
    fn missing_revision_or_event_fails_closed() {
        let conn = connection(false);
        assert_missing(&conn, "absent");
        insert_event(&conn, "event");
        conn.execute("DELETE FROM calendar_event_revisions", [])
            .unwrap();
        assert_missing(&conn, "event");
        conn.execute(
            "INSERT INTO calendar_event_revisions (event_id) VALUES ('orphan')",
            [],
        )
        .unwrap();
        assert_missing(&conn, "orphan");
    }

    #[test]
    fn event_and_meeting_transaction_rollback_restores_data_and_revision() {
        for recursive in [false, true] {
            let conn = connection(recursive);
            insert_event(&conn, "event");
            bind(&conn, "event");
            let before = get(&conn, "event").unwrap();
            {
                let tx = conn.unchecked_transaction().unwrap();
                tx.execute("UPDATE calendar_events SET title = 'Changed'", [])
                    .unwrap();
                let updated = get(&tx, "event").unwrap();
                assert!(updated > before);
                tx.execute("DELETE FROM meet_meetings", []).unwrap();
                assert!(get(&tx, "event").unwrap() > updated);
                tx.execute("DELETE FROM calendar_events", []).unwrap();
                assert_missing(&tx, "event");
                insert_event(&tx, "event");
                assert!(get(&tx, "event").unwrap() > before);
                tx.rollback().unwrap();
            }
            assert_eq!(get(&conn, "event").unwrap(), before);
            let data: (String, String) = conn
                .query_row(
                    "SELECT e.title, m.meeting_id FROM calendar_events AS e
                     JOIN meet_meetings AS m ON m.event_id = e.id WHERE e.id = 'event'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(data, ("Event".into(), "meeting".into()));
        }
    }

    #[test]
    fn revision_allocation_failure_aborts_the_source_write() {
        for recursive in [false, true] {
            let conn = connection(recursive);
            insert_event(&conn, "event");
            let before = get(&conn, "event").unwrap();
            conn.execute_batch(
                "CREATE TRIGGER interrupt_revision_allocation
                 BEFORE INSERT ON calendar_event_revisions
                 BEGIN SELECT RAISE(ABORT, 'revision allocation failed'); END;",
            )
            .unwrap();
            assert!(conn
                .execute("UPDATE OR IGNORE calendar_events SET title = 'Changed'", [])
                .is_err());
            assert_eq!(get(&conn, "event").unwrap(), before);
            let title: String = conn
                .query_row("SELECT title FROM calendar_events", [], |row| row.get(0))
                .unwrap();
            assert_eq!(title, "Event");
        }
    }

    #[test]
    fn transaction_snapshot_keeps_data_and_revision_together_across_connections() {
        for recursive in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("snapshot-revisions.db");
            let reader = Connection::open(&path).unwrap();
            initialize(&reader, recursive);
            seed_account(&reader);
            insert_event(&reader, "event");
            bind(&reader, "event");
            let before = get(&reader, "event").unwrap();
            let writer = Connection::open(&path).unwrap();
            initialize(&writer, recursive);

            let snapshot = reader.unchecked_transaction().unwrap();
            let title: String = snapshot
                .query_row("SELECT title FROM calendar_events", [], |row| row.get(0))
                .unwrap();
            assert_eq!(title, "Event");
            let write = writer.unchecked_transaction().unwrap();
            write
                .execute("UPDATE calendar_events SET title = 'Changed'", [])
                .unwrap();
            write
                .execute(
                    "UPDATE meet_meetings SET meeting_id = 'changed-meeting'",
                    [],
                )
                .unwrap();
            let committed = get(&write, "event").unwrap();
            write.commit().unwrap();

            assert!(committed > before);
            assert_eq!(get(&snapshot, "event").unwrap(), before);
            let meeting: String = snapshot
                .query_row("SELECT meeting_id FROM meet_meetings", [], |row| row.get(0))
                .unwrap();
            assert_eq!(meeting, "meeting");
            assert!(matches!(
                snapshot.execute("UPDATE calendar_events SET title = title", []),
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::DatabaseBusy
            ));
            snapshot.rollback().unwrap();
            assert_eq!(get(&reader, "event").unwrap(), committed);
            let current: (String, String) = reader
                .query_row(
                    "SELECT e.title, m.meeting_id FROM calendar_events AS e
                     JOIN meet_meetings AS m ON m.event_id = e.id",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(current, ("Changed".into(), "changed-meeting".into()));
        }
    }
}
