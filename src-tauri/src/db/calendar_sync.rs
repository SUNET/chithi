//! Disposable remote calendar mirror. Operation journals are deliberately not
//! consulted here: pending writes cannot veto a provider read or supply content.

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::calendar::event_set::{event_fields, CalendarEventSet, NativeCalendarResource};
use crate::calendar::recurrence_identity::{
    RecurrenceIdentitySeed, RecurrenceObjectKind, RecurrenceValueType,
};
use crate::calendar::RecurrenceKind;
use crate::error::{Error, Result};

/// One resource in a complete provider inventory. Unsupported resources retain
/// their current wire representation, never their previous display projection.
pub(crate) struct Resource {
    pub native: NativeCalendarResource,
    pub set: Option<CalendarEventSet>,
    pub error: Option<String>,
}

impl Resource {
    pub fn readable(set: CalendarEventSet) -> Result<Self> {
        let native = set.native.clone().ok_or_else(|| {
            Error::Sync("Remote calendar projection has no provider resource".into())
        })?;
        if let Err(error) = set.validate_display() {
            return Ok(Self::unsupported(native, error));
        }
        Ok(Self {
            native,
            set: Some(set),
            error: None,
        })
    }

    pub fn unsupported(native: NativeCalendarResource, error: impl ToString) -> Self {
        Self {
            native,
            set: None,
            error: Some(error.to_string()),
        }
    }
}

pub(crate) fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS calendar_remote_resources (
            calendar_id TEXT NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
            remote_id TEXT NOT NULL,
            protocol TEXT NOT NULL,
            native_data TEXT NOT NULL,
            set_data TEXT,
            event_id TEXT,
            error TEXT,
            PRIMARY KEY(calendar_id, remote_id)
        );
        CREATE UNIQUE INDEX IF NOT EXISTS calendar_remote_resources_event
            ON calendar_remote_resources(event_id) WHERE event_id IS NOT NULL;
        CREATE TABLE IF NOT EXISTS calendar_remote_sync (
            calendar_id TEXT PRIMARY KEY REFERENCES calendars(id) ON DELETE CASCADE,
            protocol TEXT NOT NULL,
            refreshed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );",
    )?;
    Ok(())
}

/// Replace one complete, unbounded calendar inventory in the caller's transaction.
/// Identity is provider identity, not UID, title, local ownership, or edit history.
pub(crate) fn replace_calendar(
    tx: &Transaction<'_>,
    account: &str,
    calendar: &str,
    protocol: &str,
    resources: &[Resource],
) -> Result<()> {
    let target = super::calendar::get_calendar(tx, calendar)?;
    if target.account_id != account || target.remote_id.as_deref().is_none_or(str::is_empty) {
        return Err(Error::Sync(
            "Remote mirror destination is not a provider calendar".into(),
        ));
    }
    let mut remotes = HashSet::new();
    let mut projected = HashSet::new();
    for resource in resources {
        if resource.native.protocol != protocol
            || Some(resource.native.calendar_id.as_str()) != target.remote_id.as_deref()
            || resource.native.event_id.is_empty()
            || !remotes.insert(resource.native.event_id.as_str())
        {
            return Err(Error::Sync(
                "Remote inventory has conflicting resource identity".into(),
            ));
        }
        if let Some(set) = &resource.set {
            set.validate_display()?;
            if set.event.account_id != account
                || set.event.calendar_id != calendar
                || set.event.remote_id.as_deref() != Some(resource.native.event_id.as_str())
            {
                return Err(Error::Sync(
                    "Remote projection belongs to another resource".into(),
                ));
            }
        }
    }

    // These are editing aids, not sync identity. Drop obsolete routing before
    // replacing the mirror so neither retired addresses nor claims hide resources.
    tx.execute(
        "DELETE FROM calendar_action_addresses WHERE account_id = ?1 AND calendar_id = ?2",
        params![account, calendar],
    )?;
    tx.execute(
        "DELETE FROM calendar_recurrence_objects WHERE event_id IN
        (SELECT id FROM calendar_events WHERE account_id = ?1 AND calendar_id = ?2)",
        params![account, calendar],
    )?;
    for resource in resources {
        let mut canonical = resource.set.clone();
        let id = if let Some(set) = &mut canonical {
            let existing: Option<String> = tx.query_row(
                "SELECT id FROM calendar_events WHERE account_id = ?1 AND calendar_id = ?2 AND remote_id = ?3 ORDER BY id LIMIT 1",
                params![account, calendar, resource.native.event_id], |row| row.get(0),
            ).optional()?;
            set.event.id = existing.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM calendar_events WHERE id = ?1)",
                [&set.event.id],
                |row| row.get(0),
            )?;
            if exists {
                super::calendar::update_event(tx, &set.event)?;
                tx.execute("UPDATE calendar_events SET remote_id = ?1, etag = ?2, pending_rsvp_status = NULL WHERE id = ?3", params![set.event.remote_id, set.event.etag, set.event.id])?;
            } else {
                super::calendar::insert_event(tx, &set.event)?;
            }
            super::calendar_invitation::invalidate(tx, &set.event.id)?;
            tx.execute(
                "DELETE FROM calendar_action_members WHERE event_id = ?1 OR owner_event_id = ?1",
                [&set.event.id],
            )?;
            tx.execute(
                "DELETE FROM calendar_action_sets WHERE event_id = ?1",
                [&set.event.id],
            )?;
            // Recurrence identities are editing aids, not a prerequisite for
            // displaying an upstream event that the editor cannot mutate.
            if set.validate().is_ok() {
                replace_recurrence(tx, set)?;
            }
            projected.insert(set.event.id.clone());
            Some(set.event.id.clone())
        } else {
            None
        };
        tx.execute(
            "INSERT INTO calendar_remote_resources(calendar_id, remote_id, protocol, native_data, set_data, event_id, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(calendar_id, remote_id) DO UPDATE SET protocol = excluded.protocol,
                native_data = excluded.native_data, set_data = excluded.set_data,
                event_id = excluded.event_id, error = excluded.error",
            params![calendar, resource.native.event_id, protocol,
                encode(&resource.native)?,
                canonical.as_ref().map(encode).transpose()?, id, resource.error],
        )?;
    }
    let rows = {
        let mut stmt = tx
            .prepare("SELECT id FROM calendar_events WHERE account_id = ?1 AND calendar_id = ?2")?;
        let rows = stmt
            .query_map(params![account, calendar], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for id in rows {
        if !projected.contains(&id) {
            // Cache eviction is not a user request to delete a remote video room.
            // Journals contain their own source data and have no event-row FK.
            tx.execute("DELETE FROM calendar_events WHERE id = ?1", [&id])?;
        }
    }
    let rows = {
        let mut stmt =
            tx.prepare("SELECT remote_id FROM calendar_remote_resources WHERE calendar_id = ?1")?;
        let rows = stmt
            .query_map([calendar], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for id in rows {
        if !remotes.contains(id.as_str()) {
            tx.execute(
                "DELETE FROM calendar_remote_resources WHERE calendar_id = ?1 AND remote_id = ?2",
                params![calendar, id],
            )?;
        }
    }
    tx.execute("INSERT INTO calendar_remote_sync(calendar_id, protocol) VALUES (?1, ?2)
        ON CONFLICT(calendar_id) DO UPDATE SET protocol = excluded.protocol, refreshed_at = CURRENT_TIMESTAMP", params![calendar, protocol])?;
    Ok(())
}

pub(crate) fn displayed_set(conn: &Connection, event: &str) -> Result<Option<CalendarEventSet>> {
    let data: Option<String> = conn.query_row(
        "SELECT set_data FROM calendar_remote_resources WHERE event_id = ?1 AND set_data IS NOT NULL",
        [event], |row| row.get(0),
    ).optional()?;
    data.map(|data| decode(&data)).transpose()
}

/// Incremental adapters rebuild projections from the current wire mirror, not
/// from local edits or action results. A missing mirror requires a full read.
pub(crate) fn native_resources(
    conn: &Connection,
    calendar: &str,
    protocol: &str,
) -> Result<Option<Vec<NativeCalendarResource>>> {
    let present: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM calendar_remote_sync WHERE calendar_id = ?1 AND protocol = ?2)", params![calendar, protocol], |row| row.get(0))?;
    if !present {
        return Ok(None);
    }
    let mut stmt = conn.prepare("SELECT native_data FROM calendar_remote_resources WHERE calendar_id = ?1 AND protocol = ?2 ORDER BY remote_id")?;
    let data = stmt
        .query_map(params![calendar, protocol], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    data.into_iter()
        .map(|data| decode(&data))
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn encode<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|error| Error::Sync(format!("Cannot encode remote calendar resource: {error}")))
}

fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value)
        .map_err(|error| Error::Sync(format!("Cannot decode remote calendar resource: {error}")))
}

fn replace_recurrence(tx: &Transaction<'_>, set: &CalendarEventSet) -> Result<()> {
    let native = set.native.as_ref().expect("validated remote source");
    let seed = |key: Option<String>, fields, kind, resource: Option<&NativeCalendarResource>| {
        RecurrenceIdentitySeed {
            local_series_event_id: None,
            provider_calendar_id: Some(native.calendar_id.clone()),
            provider_series_id: Some(native.event_id.clone()),
            provider_occurrence_id: resource
                .filter(|resource| resource.event_id != native.event_id)
                .map(|resource| resource.event_id.clone()),
            recurrence_value_type: key.as_ref().map(|_| {
                if set.event.all_day {
                    RecurrenceValueType::Date
                } else {
                    RecurrenceValueType::DateTime
                }
            }),
            recurrence_timezone: key.as_ref().and(set.event.timezone.clone()),
            recurrence_id: key,
            occurrence: fields,
            provider_native_data: resource.map(|resource| resource.data.clone()),
            provider_revision: resource.and_then(|resource| resource.revision.clone()),
            kind,
        }
    };
    let mut seeds = Vec::new();
    if set.event.recurrence_kind == RecurrenceKind::Series {
        seeds.push(seed(
            None,
            event_fields(&set.event),
            RecurrenceObjectKind::Master,
            Some(native),
        ));
        for item in &set.overrides {
            let fields = item
                .event
                .as_ref()
                .map(event_fields)
                .unwrap_or_else(|| event_fields(&set.event));
            seeds.push(seed(
                Some(item.original_start.clone()),
                fields,
                if item.event.is_some() {
                    RecurrenceObjectKind::Exception
                } else {
                    RecurrenceObjectKind::Exclusion
                },
                item.native.as_ref(),
            ));
        }
    }
    super::calendar::replace_event_recurrence_objects(
        tx,
        &set.event.account_id,
        &set.event.id,
        &seeds,
    )
}

/// Publish a confirmed foreground write without treating other cached resources
/// as absent. Only an unbounded provider inventory may perform replacement.
pub(crate) fn record_confirmed_set(tx: &Connection, set: &CalendarEventSet) -> Result<()> {
    if tx.is_autocommit() {
        return Err(Error::Sync(
            "Confirmed calendar publication requires a transaction".into(),
        ));
    }
    set.validate()?;
    let native = set
        .native
        .as_ref()
        .ok_or_else(|| Error::Sync("Confirmed calendar write has no remote resource".into()))?;
    if let Some(previous) = displayed_set(tx, &set.event.id)? {
        for resource in previous
            .overrides
            .iter()
            .filter_map(|item| item.native.as_ref())
        {
            if resource.event_id != native.event_id {
                tx.execute("DELETE FROM calendar_remote_resources WHERE calendar_id = ?1 AND remote_id = ?2", params![set.event.calendar_id, resource.event_id])?;
            }
        }
    }
    tx.execute("INSERT INTO calendar_remote_resources(calendar_id, remote_id, protocol, native_data, set_data, event_id)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ON CONFLICT(calendar_id, remote_id) DO UPDATE SET protocol = excluded.protocol,
        native_data = excluded.native_data, set_data = excluded.set_data, event_id = excluded.event_id, error = NULL",
        params![set.event.calendar_id, native.event_id, native.protocol, encode(native)?, encode(set)?, set.event.id])?;
    for resource in set
        .overrides
        .iter()
        .filter_map(|item| item.native.as_ref())
        .filter(|resource| resource.event_id != native.event_id)
    {
        tx.execute("INSERT INTO calendar_remote_resources(calendar_id, remote_id, protocol, native_data)
            VALUES (?1, ?2, ?3, ?4) ON CONFLICT(calendar_id, remote_id) DO UPDATE SET
            protocol = excluded.protocol, native_data = excluded.native_data, set_data = NULL, event_id = NULL, error = NULL",
            params![set.event.calendar_id, resource.event_id, resource.protocol, encode(resource)?])?;
    }
    Ok(())
}

/// Remove a provider-confirmed deletion from the display and wire cache.
pub(crate) fn forget_confirmed_set(conn: &Connection, event: &str) -> Result<()> {
    if let Some(set) = displayed_set(conn, event)? {
        for resource in set
            .native
            .iter()
            .chain(set.overrides.iter().filter_map(|item| item.native.as_ref()))
        {
            conn.execute(
                "DELETE FROM calendar_remote_resources WHERE calendar_id = ?1 AND remote_id = ?2",
                params![set.event.calendar_id, resource.event_id],
            )?;
        }
    }
    Ok(())
}

/// Account-wide immutable IDs let Graph/JMAP preserve local references through
/// external moves. Only a complete current inventory may establish this mapping.
pub(crate) fn rebind_account_resources(
    tx: &Transaction<'_>,
    account: &str,
    protocol: &str,
    mirrors: &std::collections::HashMap<String, Vec<Resource>>,
) -> Result<()> {
    let mut locations = std::collections::HashMap::<&str, Vec<&str>>::new();
    for (calendar, resources) in mirrors {
        for resource in resources.iter().filter(|resource| resource.set.is_some()) {
            locations
                .entry(&resource.native.event_id)
                .or_default()
                .push(calendar);
        }
    }
    for (remote, calendars) in locations {
        if calendars.len() != 1 {
            if protocol == "graph" {
                return Err(Error::Sync(
                    "Graph immutable resource appeared in multiple calendars".into(),
                ));
            }
            continue;
        }
        let target = calendars[0];
        let rows = {
            let mut stmt = tx.prepare("SELECT id, calendar_id FROM calendar_events WHERE account_id = ?1 AND remote_id = ?2")?;
            let rows = stmt
                .query_map(params![account, remote], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        if rows.iter().any(|(_, calendar)| calendar == target) {
            continue;
        }
        if let [(id, _)] = rows.as_slice() {
            tx.execute(
                "DELETE FROM calendar_remote_resources WHERE event_id = ?1",
                [id],
            )?;
            tx.execute(
                "UPDATE calendar_events SET calendar_id = ?1 WHERE id = ?2",
                params![target, id],
            )?;
        }
    }
    Ok(())
}

/// Complete calendar discovery removes absent remote calendars, independently of
/// unfinished writes. Local-only calendars are outside the mirror's ownership.
pub(crate) fn replace_inventory(
    tx: &Transaction<'_>,
    account: &str,
    observed: &HashSet<&str>,
) -> Result<()> {
    let rows = {
        let mut stmt = tx.prepare("SELECT id, remote_id FROM calendars WHERE account_id = ?1 AND remote_id IS NOT NULL AND remote_id != ''")?;
        let rows = stmt
            .query_map([account], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (id, remote) in rows {
        if !observed.contains(remote.as_str()) {
            tx.execute("DELETE FROM calendar_events WHERE calendar_id = ?1", [&id])?;
            super::calendar::delete_calendar_row(tx, &id)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::CalendarEvent;

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::initialize(&conn).unwrap();
        conn.execute_batch("INSERT INTO accounts(id, display_name, email, username) VALUES ('account', 'Test', 'test@example.test', 'test');
            INSERT INTO calendars(id, account_id, name, remote_id) VALUES ('calendar', 'account', 'Remote', 'provider-calendar');
            INSERT INTO calendars(id, account_id, name) VALUES ('local', 'account', 'Local');").unwrap();
        conn
    }

    fn resource(protocol: &str, remote: &str, title: &str) -> Resource {
        Resource::readable(CalendarEventSet {
            event: CalendarEvent {
                id: "discardable".into(),
                account_id: "account".into(),
                calendar_id: "calendar".into(),
                title: title.into(),
                start_time: "2026-10-08T10:00:00Z".into(),
                end_time: "2026-10-08T11:00:00Z".into(),
                remote_id: Some(remote.into()),
                recurrence_kind: RecurrenceKind::Standalone,
                uid: Some("same-uid".into()),
                ..crate::backend::testutil::event()
            },
            native: Some(NativeCalendarResource {
                protocol: protocol.into(),
                calendar_id: "provider-calendar".into(),
                event_id: remote.into(),
                revision: Some("current".into()),
                data: format!("current remote content {remote}"),
            }),
            overrides: Vec::new(),
            content: None,
        })
        .unwrap()
    }

    fn contents(conn: &Connection) -> Vec<(String, String)> {
        super::super::calendar::list_events(
            conn,
            "account",
            Some("calendar"),
            "2026-10-01T00:00:00Z",
            "2026-11-01T00:00:00Z",
        )
        .unwrap()
        .into_iter()
        .map(|event| (event.remote_id.unwrap(), event.title))
        .collect()
    }

    #[test]
    fn local_content_and_action_membership_cannot_change_mirror_display_or_window() {
        let mut conn = fixture();
        let tx = conn.transaction().unwrap();
        replace_calendar(
            &tx,
            "account",
            "calendar",
            "caldav",
            &[resource("caldav", "lunch", "Lunch at 12")],
        )
        .unwrap();
        tx.commit().unwrap();
        let before = contents(&conn);
        let id: String = conn
            .query_row(
                "SELECT event_id FROM calendar_remote_resources",
                [],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute("UPDATE calendar_events SET title = 'Wrong local content', start_time = '2022-01-01T00:00:00Z', end_time = '2022-01-01T01:00:00Z', pending_rsvp_status = 'declined' WHERE id = ?1", [&id]).unwrap();
        conn.execute("INSERT INTO calendar_events(id, account_id, calendar_id, title, start_time, end_time) VALUES ('ghost-owner', 'account', 'calendar', 'Ghost', '2022-01-01', '2022-01-02')", []).unwrap();
        conn.execute("INSERT INTO calendar_action_members(event_id, owner_event_id) VALUES (?1, 'ghost-owner')", [&id]).unwrap();
        assert_eq!(contents(&conn), before);
    }

    #[test]
    fn range_reads_select_canonical_masters_without_decoding_provider_history() {
        let mut conn = fixture();
        let mut old = resource("graph", "old", "Old");
        let set = old.set.as_mut().unwrap();
        set.event.start_time = "2000-01-01T10:00:00Z".into();
        set.event.end_time = "2000-01-01T11:00:00Z".into();
        set.native.as_mut().unwrap().data = "historical provider data".repeat(10_000);
        let mut series = resource("graph", "series", "Series");
        let set = series.set.as_mut().unwrap();
        set.event.start_time = "2000-01-01T10:00:00Z".into();
        set.event.end_time = "2000-01-01T11:00:00Z".into();
        set.event.recurrence_kind = RecurrenceKind::Series;
        set.event.recurrence_rule = Some("FREQ=MONTHLY;BYDAY=2TH".into());
        let mut instant = resource("graph", "instant", "Instant");
        let set = instant.set.as_mut().unwrap();
        set.event.start_time = "2026-10-01T00:00:00Z".into();
        set.event.end_time = set.event.start_time.clone();
        let mut upper = resource("graph", "upper", "Upper");
        let set = upper.set.as_mut().unwrap();
        set.event.start_time = "2026-11-01T00:00:00Z".into();
        set.event.end_time = set.event.start_time.clone();
        let mut overlap = resource("graph", "overlap", "Overlap");
        let set = overlap.set.as_mut().unwrap();
        set.event.start_time = "2026-09-30T23:30:00Z".into();
        set.event.end_time = "2026-10-01T00:30:00Z".into();
        let tx = conn.transaction().unwrap();
        replace_calendar(
            &tx,
            "account",
            "calendar",
            "graph",
            &[
                old,
                series,
                instant,
                upper,
                overlap,
                resource("graph", "visible", "Visible"),
            ],
        )
        .unwrap();
        tx.commit().unwrap();
        // Neither local dates nor local grammar can determine remote eligibility.
        conn.execute("UPDATE calendar_events SET start_time = '2026-10-08T12:00:00Z', end_time = '2026-10-08T13:00:00Z', recurrence_kind = 'corrupt-local-content' WHERE remote_id = 'old'", []).unwrap();
        conn.execute("UPDATE calendar_events SET start_time = '1800-01-01', end_time = '1800-01-02', recurrence_kind = 'corrupt-local-content' WHERE remote_id = 'visible'", []).unwrap();
        // A master DTO read has no reason to deserialize private adapter data.
        // Full-set reads still reject malformed native metadata when it is needed.
        conn.execute("UPDATE calendar_remote_resources SET set_data = json_set(set_data, '$.native.data', json('{}')) WHERE remote_id = 'visible'", []).unwrap();
        for calendar in [None, Some("calendar")] {
            let mut events = super::super::calendar::list_events(
                &conn,
                "account",
                calendar,
                "2026-10-01T00:00:00Z",
                "2026-11-01T00:00:00Z",
            )
            .unwrap();
            events.sort_by(|a, b| a.remote_id.cmp(&b.remote_id));
            assert_eq!(
                events
                    .iter()
                    .map(|event| event.remote_id.as_deref().unwrap())
                    .collect::<Vec<_>>(),
                ["instant", "overlap", "series", "visible"]
            );
            assert_eq!(events.last().unwrap().start_time, "2026-10-08T10:00:00Z");
        }
        assert!(super::super::calendar::list_events(
            &conn,
            "another-account",
            None,
            "2026-10-01",
            "2026-11-01"
        )
        .unwrap()
        .is_empty());
        assert!(super::super::calendar::list_events(
            &conn,
            "account",
            Some("local"),
            "2026-10-01",
            "2026-11-01"
        )
        .unwrap()
        .is_empty());
        let id: String = conn
            .query_row(
                "SELECT event_id FROM calendar_remote_resources WHERE remote_id = 'visible'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(displayed_set(&conn, &id).is_err());
        conn.execute(
            "UPDATE calendars SET is_archived = 1 WHERE id = 'calendar'",
            [],
        )
        .unwrap();
        assert!(super::super::calendar::list_events(
            &conn,
            "account",
            None,
            "2026-10-01",
            "2026-11-01"
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn migrated_and_fresh_mirrors_converge_despite_local_history() {
        for protocol in ["caldav", "google", "graph", "jmap"] {
            let mut fresh = fixture();
            let mut migrated = fixture();
            migrated.execute_batch("INSERT INTO calendar_events(id, account_id, calendar_id, remote_id, title, start_time, end_time)
                VALUES ('ghost', 'account', 'calendar', 'deleted-upstream', 'Stale Lunch', '2026-10-08T12:00:00Z', '2026-10-08T13:00:00Z'),
                       ('unpublished', 'account', 'calendar', NULL, 'Never published', '2026-10-08T12:00:00Z', '2026-10-08T13:00:00Z');
                INSERT INTO calendar_action_operations(operation_id, account_id, event_id, data) VALUES ('pending', 'account', 'ghost', '{}');
                INSERT INTO calendar_action_claims(event_id, operation_id) VALUES ('ghost', 'pending');").unwrap();
            for conn in [&mut fresh, &mut migrated] {
                let tx = conn.transaction().unwrap();
                replace_calendar(
                    &tx,
                    "account",
                    "calendar",
                    protocol,
                    &[
                        resource(protocol, "lunch", "Lunch at 12"),
                        resource(protocol, "other", "Other"),
                    ],
                )
                .unwrap();
                tx.commit().unwrap();
            }
            assert_eq!(contents(&fresh), contents(&migrated), "{protocol}");
            assert_eq!(
                migrated
                    .query_row(
                        "SELECT COUNT(*) FROM calendar_action_operations",
                        [],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                1
            );
            assert!(super::super::calendar::get_event(&migrated, "ghost").is_err());
            assert!(super::super::calendar::get_event(&migrated, "unpublished").is_err());
        }
    }

    #[test]
    fn claims_do_not_veto_refresh_deletion_or_calendar_removal() {
        let mut conn = fixture();
        let tx = conn.transaction().unwrap();
        replace_calendar(
            &tx,
            "account",
            "calendar",
            "caldav",
            &[resource("caldav", "lunch", "Old Lunch")],
        )
        .unwrap();
        tx.commit().unwrap();
        let id: String = conn
            .query_row(
                "SELECT event_id FROM calendar_remote_resources",
                [],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute("INSERT INTO calendar_action_operations(operation_id, account_id, event_id, data) VALUES ('pending', 'account', ?1, '{}')", [&id]).unwrap();
        conn.execute(
            "INSERT INTO calendar_action_claims(event_id, operation_id) VALUES (?1, 'pending')",
            [&id],
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        replace_calendar(
            &tx,
            "account",
            "calendar",
            "caldav",
            &[resource("caldav", "lunch", "New Lunch")],
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(contents(&conn), [("lunch".into(), "New Lunch".into())]);
        assert_eq!(
            conn.query_row("SELECT id FROM calendar_events", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            id
        );
        let tx = conn.transaction().unwrap();
        replace_calendar(&tx, "account", "calendar", "caldav", &[]).unwrap();
        tx.commit().unwrap();
        assert!(contents(&conn).is_empty());
        let tx = conn.transaction().unwrap();
        replace_inventory(&tx, "account", &HashSet::new()).unwrap();
        tx.commit().unwrap();
        assert!(super::super::calendar::get_calendar(&conn, "calendar").is_err());
        assert!(super::super::calendar::get_calendar(&conn, "local").is_ok());
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM calendar_action_operations",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn unsupported_resource_replaces_its_old_projection_only() {
        let mut conn = fixture();
        let tx = conn.transaction().unwrap();
        replace_calendar(
            &tx,
            "account",
            "calendar",
            "caldav",
            &[resource("caldav", "bad", "Old content")],
        )
        .unwrap();
        tx.commit().unwrap();
        let native = resource("caldav", "bad", "New unsupported content").native;
        let tx = conn.transaction().unwrap();
        replace_calendar(
            &tx,
            "account",
            "calendar",
            "caldav",
            &[
                Resource::unsupported(native, "invalid DTSTART"),
                resource("caldav", "good", "Current content"),
            ],
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(contents(&conn), [("good".into(), "Current content".into())]);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM calendar_remote_resources WHERE error IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            native_resources(&conn, "calendar", "caldav")
                .unwrap()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn invalid_inventory_rolls_back_cache_and_cursor_together() {
        let mut conn = fixture();
        let tx = conn.transaction().unwrap();
        replace_calendar(
            &tx,
            "account",
            "calendar",
            "google",
            &[resource("google", "one", "Before")],
        )
        .unwrap();
        tx.commit().unwrap();
        conn.execute(
            "INSERT INTO app_metadata(key, value) VALUES ('cursor', 'before')",
            [],
        )
        .unwrap();
        let before = contents(&conn);
        {
            let tx = conn.transaction().unwrap();
            let mut other = resource("google", "two", "Wrong calendar");
            other.native.calendar_id = "another-calendar".into();
            tx.execute(
                "UPDATE app_metadata SET value = 'after' WHERE key = 'cursor'",
                [],
            )
            .unwrap();
            assert!(replace_calendar(
                &tx,
                "account",
                "calendar",
                "google",
                &[resource("google", "one", "After"), other]
            )
            .is_err());
        }
        assert_eq!(contents(&conn), before);
        assert_eq!(
            conn.query_row(
                "SELECT value FROM app_metadata WHERE key = 'cursor'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "before"
        );
    }

    #[test]
    fn same_uid_resources_remain_distinct_and_eviction_never_requests_room_deletion() {
        let mut conn = fixture();
        let tx = conn.transaction().unwrap();
        replace_calendar(
            &tx,
            "account",
            "calendar",
            "graph",
            &[
                resource("graph", "one", "One"),
                resource("graph", "two", "Two"),
            ],
        )
        .unwrap();
        tx.commit().unwrap();
        let id: String = conn
            .query_row(
                "SELECT event_id FROM calendar_remote_resources WHERE remote_id = 'one'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute("INSERT INTO meet_meetings(event_id, account_id, protocol, meeting_id, join_url) VALUES (?1, 'account', 'zoom', 'room', 'https://example.test/join')", [&id]).unwrap();
        assert_eq!(contents(&conn).len(), 2);
        let tx = conn.transaction().unwrap();
        replace_calendar(&tx, "account", "calendar", "graph", &[]).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM meet_pending_meetings WHERE cleanup_requested = 1",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
}
