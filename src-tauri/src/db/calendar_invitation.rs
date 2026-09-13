//! Private proof that a newly created local series has complete RRULE recurrence.
//!
//! Only the local creation command may record proof, inside its event insertion
//! transaction. Missing provider/raw metadata is never evidence of provenance.

use rusqlite::{params, Connection, OptionalExtension};

use crate::calendar::recurrence::normalize_invitation_rrule;
use crate::calendar::{CalendarEvent, RecurrenceKind};
use crate::error::{Error, Result};

/// Record validated recurrence only for a known new local series.
///
/// The caller must have just inserted this event in the same transaction, after
/// validating its creation request. Never call this from import, refresh, edit,
/// migration, or invitation-send paths. The field checks defend that contract;
/// they cannot establish provenance on their own.
pub fn record_local_series(conn: &Connection, event: &CalendarEvent) -> Result<()> {
    if conn.is_autocommit() {
        return Err(Error::Other(
            "Local series invitation proof requires the event insertion transaction".into(),
        ));
    }
    let (persisted, rule) = matching_persisted_series(conn, event)?;
    if event.remote_id.is_some() || persisted.remote_id.is_some() {
        return Err(unrepresentable(
            "only new local series may record recurrence proof",
        ));
    }
    conn.execute(
        "INSERT INTO calendar_invitation_recurrence (event_id, recurrence_rule)
         VALUES (?1, ?2)",
        params![event.id, rule],
    )?;
    Ok(())
}

/// Return a complete supported RRULE only when its persisted local proof matches.
///
/// An initial provider push may attach a remote ID and canonical UID without
/// losing proof. A provider refresh must explicitly invalidate it, even when the
/// reported RRULE is identical, because overrides may not be represented here.
pub fn validated_series_rule(conn: &Connection, event: &CalendarEvent) -> Result<String> {
    let (_, rule) = matching_persisted_series(conn, event)?;
    let proof: Option<String> = conn
        .query_row(
            "SELECT recurrence_rule FROM calendar_invitation_recurrence WHERE event_id = ?1",
            params![event.id],
            |row| row.get(0),
        )
        .optional()?;
    if proof.as_deref() != Some(rule.as_str()) {
        return Err(unrepresentable(
            "complete local recurrence proof is missing or stale",
        ));
    }
    Ok(rule)
}

/// Clear proof within the caller's provider-refresh transaction.
pub fn invalidate(conn: &Connection, event_id: &str) -> Result<()> {
    if conn.is_autocommit() {
        return Err(Error::Other(
            "Invitation proof invalidation requires the provider refresh transaction".into(),
        ));
    }
    conn.execute(
        "DELETE FROM calendar_invitation_recurrence WHERE event_id = ?1",
        params![event_id],
    )?;
    Ok(())
}

fn matching_persisted_series(
    conn: &Connection,
    event: &CalendarEvent,
) -> Result<(CalendarEvent, String)> {
    let rule = supported_series_rule(event)?;
    let persisted = super::calendar::get_event(conn, &event.id)?;
    let persisted_rule = supported_series_rule(&persisted)?;
    if event.account_id != persisted.account_id
        || event.timezone != persisted.timezone
        || rule != persisted_rule
    {
        return Err(unrepresentable(
            "the event no longer matches its persisted recurrence",
        ));
    }
    Ok((persisted, persisted_rule))
}

fn supported_series_rule(event: &CalendarEvent) -> Result<String> {
    if event.recurrence_kind != RecurrenceKind::Series
        || event.ical_data.is_some()
        || event.source_message_id.is_some()
    {
        return Err(unrepresentable(
            "the event is not a complete local RRULE series",
        ));
    }
    event
        .recurrence_rule
        .as_deref()
        .and_then(|rule| normalize_invitation_rrule(rule, event.timezone.as_deref()))
        .ok_or_else(|| unrepresentable("the recurrence rule cannot be represented faithfully"))
}

fn unrepresentable(reason: &str) -> Error {
    Error::Other(format!(
        "Cannot send this recurring invitation: {reason}. Manage its recurrence in the source calendar"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{calendar, schema};

    fn connection() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('acc1', 'Test', 'test@example.test', 'test@example.test')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendars (id, account_id, name) VALUES ('cal1', 'acc1', 'Test')",
            [],
        )
        .unwrap();
        conn
    }

    fn series() -> CalendarEvent {
        let mut event = crate::backend::testutil::event();
        event.recurrence_kind = RecurrenceKind::Series;
        event.recurrence_rule = Some("FREQ=WEEKLY;COUNT=4;BYDAY=MO".into());
        event.uid = Some("local@example.test".into());
        event
    }

    fn create_local_series(conn: &Connection, event: &CalendarEvent) {
        let transaction = conn.unchecked_transaction().unwrap();
        calendar::insert_event(&transaction, event).unwrap();
        record_local_series(&transaction, event).unwrap();
        transaction.commit().unwrap();
    }

    #[test]
    fn inserted_or_provider_series_never_infer_proof_from_missing_raw_data() {
        for remote_id in [None, Some("remote")] {
            let conn = connection();
            let mut event = series();
            event.remote_id = remote_id.map(str::to_string);
            if remote_id.is_some() {
                calendar::upsert_event_by_remote_id(&conn, &event).unwrap();
            } else {
                calendar::insert_event(&conn, &event).unwrap();
            }
            assert!(event.ical_data.is_none());
            assert!(event.source_message_id.is_none());
            assert!(validated_series_rule(&conn, &event).is_err());
        }
    }

    #[test]
    fn imported_rrule_rdate_and_exdate_cannot_record_local_proof() {
        for (rrule, raw, source, remote) in [
            (Some("FREQ=WEEKLY"), None, Some("message"), None),
            (None, None, None, Some("rdate-only-provider-series")),
            (
                None,
                Some("BEGIN:VEVENT\r\nRDATE:20260914T100000Z\r\nEND:VEVENT"),
                None,
                None,
            ),
            (
                Some("FREQ=WEEKLY"),
                Some("BEGIN:VEVENT\r\nRRULE:FREQ=WEEKLY\r\nEXDATE:20260914T100000Z\r\nEND:VEVENT"),
                None,
                None,
            ),
            (Some("FREQ=WEEKLY"), None, None, Some("remote")),
            (Some("FREQ=WEEKLY"), Some(""), None, None),
            (Some("FREQ=WEEKLY"), None, Some(""), None),
        ] {
            let conn = connection();
            let mut event = series();
            event.recurrence_rule = rrule.map(str::to_string);
            event.ical_data = raw.map(str::to_string);
            event.source_message_id = source.map(str::to_string);
            event.remote_id = remote.map(str::to_string);
            let transaction = conn.unchecked_transaction().unwrap();
            calendar::insert_event(&transaction, &event).unwrap();
            assert!(record_local_series(&transaction, &event).is_err());
            assert!(validated_series_rule(&transaction, &event).is_err());
        }
    }

    #[test]
    fn recording_requires_insert_transaction_and_matching_persisted_series() {
        let conn = connection();
        let event = series();
        assert!(record_local_series(&conn, &event).is_err());
        let transaction = conn.unchecked_transaction().unwrap();
        assert!(record_local_series(&transaction, &event).is_err());
        calendar::insert_event(&transaction, &event).unwrap();
        let mut mismatched = event.clone();
        mismatched.recurrence_rule = Some("FREQ=DAILY".into());
        assert!(record_local_series(&transaction, &mismatched).is_err());
        record_local_series(&transaction, &event).unwrap();
        assert!(record_local_series(&transaction, &event).is_err());
        transaction.rollback().unwrap();
        assert!(calendar::get_event(&conn, &event.id).is_err());
        let proofs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM calendar_invitation_recurrence",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(proofs, 0);
    }

    #[test]
    fn normalized_proof_survives_initial_remote_id_and_canonical_uid_attachment() {
        let conn = connection();
        let mut event = series();
        event.recurrence_rule = Some(" rrule:freq=weekly;count=4;byday=mo ".into());
        create_local_series(&conn, &event);
        assert_eq!(
            validated_series_rule(&conn, &event).unwrap(),
            "FREQ=WEEKLY;COUNT=4;BYDAY=MO"
        );
        conn.execute(
            "UPDATE calendar_events SET remote_id = 'initial-jmap-id', uid = 'canonical-uid'
             WHERE id = ?1",
            params![event.id],
        )
        .unwrap();
        let attached = calendar::get_event(&conn, &event.id).unwrap();
        assert_eq!(attached.remote_id.as_deref(), Some("initial-jmap-id"));
        assert_eq!(
            validated_series_rule(&conn, &attached).unwrap(),
            "FREQ=WEEKLY;COUNT=4;BYDAY=MO"
        );
        calendar::upsert_event_by_remote_id(&conn, &attached).unwrap();
        let refreshed = calendar::get_event(&conn, &event.id).unwrap();
        assert_eq!(refreshed.recurrence_rule, attached.recurrence_rule);
        assert_eq!(refreshed.recurrence_kind, attached.recurrence_kind);
        assert!(validated_series_rule(&conn, &refreshed).is_err());
    }

    #[test]
    fn provider_uid_reconciliation_invalidates_unpushed_local_proof() {
        let conn = connection();
        let event = series();
        create_local_series(&conn, &event);
        let mut remote = event.clone();
        remote.id = "incoming-provider-id".into();
        remote.remote_id = Some("remote".into());
        calendar::upsert_event_by_remote_id(&conn, &remote).unwrap();
        let reconciled = calendar::get_event(&conn, &event.id).unwrap();
        assert_eq!(reconciled.remote_id, remote.remote_id);
        assert!(validated_series_rule(&conn, &reconciled).is_err());
    }

    #[test]
    fn existing_proof_cannot_authorize_modified_or_malformed_rule() {
        let conn = connection();
        let event = series();
        create_local_series(&conn, &event);
        for rule in [
            None,
            Some(""),
            Some("FREQ=WEEKLY;COUNT=5;BYDAY=MO"),
            Some("FREQ=WEEKLY;COUNT=bad"),
            Some("FREQ=WEEKLY\r\nEXDATE:20260914T100000Z"),
            Some("RRULE:RRULE:FREQ=WEEKLY;COUNT=4;BYDAY=MO"),
        ] {
            let mut changed = event.clone();
            changed.recurrence_rule = rule.map(str::to_string);
            assert!(validated_series_rule(&conn, &changed).is_err(), "{rule:?}");
        }
        for kind in [
            RecurrenceKind::Unknown,
            RecurrenceKind::Standalone,
            RecurrenceKind::Occurrence,
        ] {
            let mut changed = event.clone();
            changed.recurrence_kind = kind;
            assert!(validated_series_rule(&conn, &changed).is_err());
        }
        for (raw, source) in [
            (Some("EXDATE:20260914T100000Z"), None),
            (None, Some("message")),
        ] {
            let mut changed = event.clone();
            changed.ical_data = raw.map(str::to_string);
            changed.source_message_id = source.map(str::to_string);
            assert!(validated_series_rule(&conn, &changed).is_err());
        }
        let mut changed = event.clone();
        changed.account_id = "another-account".into();
        assert!(validated_series_rule(&conn, &changed).is_err());
        changed = event.clone();
        changed.timezone = Some("America/New_York".into());
        assert!(validated_series_rule(&conn, &changed).is_err());
        assert!(validated_series_rule(&conn, &event).is_ok());
        conn.execute(
            "UPDATE calendar_events SET recurrence_rule = 'FREQ=DAILY' WHERE id = ?1",
            params![event.id],
        )
        .unwrap();
        assert!(validated_series_rule(&conn, &event).is_err());
        let changed = calendar::get_event(&conn, &event.id).unwrap();
        assert!(validated_series_rule(&conn, &changed).is_err());
    }

    #[test]
    fn stored_proof_must_equal_the_exact_normalized_persisted_rule() {
        let conn = connection();
        let event = series();
        create_local_series(&conn, &event);
        for proof in [
            "RRULE:FREQ=WEEKLY;COUNT=4;BYDAY=MO",
            "COUNT=4;FREQ=WEEKLY;BYDAY=MO",
            "FREQ=WEEKLY;COUNT=5;BYDAY=MO",
        ] {
            conn.execute(
                "UPDATE calendar_invitation_recurrence SET recurrence_rule = ?1 WHERE event_id = ?2",
                params![proof, event.id],
            )
            .unwrap();
            assert!(validated_series_rule(&conn, &event).is_err(), "{proof}");
        }
    }
}
