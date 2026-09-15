use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::{
    calendar::recurrence_identity::{
        OccurrenceFields, RecurrenceIdentity, RecurrenceObjectKind, RecurrenceValueType,
    },
    error::{Error, Result},
};

const SELECT_COLUMNS: &str = "object_id, account_id, event_id,
     local_series_event_id, provider_calendar_id, provider_series_id,
     provider_occurrence_id, recurrence_id, recurrence_timezone,
     recurrence_value_type, effective_title, effective_description,
     effective_location, effective_start, effective_end, effective_all_day,
     effective_timezone,
     provider_native_data, provider_revision, object_kind";

/// Refresh mutable content only when an object ID still names the exact identity.
pub fn upsert(conn: &Connection, identity: &RecurrenceIdentity) -> Result<()> {
    identity.validate()?;
    let written = conn.execute(
        "INSERT INTO calendar_recurrence_objects
             (object_id, account_id, event_id, local_series_event_id,
               provider_calendar_id, provider_series_id,
               provider_occurrence_id, recurrence_id,
               recurrence_timezone, recurrence_value_type, effective_title,
               effective_description, effective_location, effective_start,
               effective_end, effective_all_day, effective_timezone,
               provider_native_data, provider_revision, object_kind)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, ?15, ?16, ?17, ?18, ?19, ?20)
          ON CONFLICT(object_id) DO UPDATE SET
              effective_title = excluded.effective_title,
             effective_description = excluded.effective_description,
             effective_location = excluded.effective_location,
             effective_start = excluded.effective_start,
             effective_end = excluded.effective_end,
             effective_all_day = excluded.effective_all_day,
             effective_timezone = excluded.effective_timezone,
             provider_native_data = excluded.provider_native_data,
             provider_revision = excluded.provider_revision,
              object_kind = excluded.object_kind
          WHERE calendar_recurrence_objects.account_id IS excluded.account_id
            AND calendar_recurrence_objects.event_id IS excluded.event_id
            AND calendar_recurrence_objects.local_series_event_id IS excluded.local_series_event_id
            AND calendar_recurrence_objects.provider_calendar_id IS excluded.provider_calendar_id
            AND calendar_recurrence_objects.provider_series_id IS excluded.provider_series_id
            AND calendar_recurrence_objects.provider_occurrence_id IS excluded.provider_occurrence_id
            AND calendar_recurrence_objects.recurrence_id IS excluded.recurrence_id
            AND calendar_recurrence_objects.recurrence_timezone IS excluded.recurrence_timezone
            AND calendar_recurrence_objects.recurrence_value_type IS excluded.recurrence_value_type",
        params![
            identity.object_id,
            identity.account_id,
            identity.event_id,
            identity.local_series_event_id,
            identity.provider_calendar_id,
            identity.provider_series_id,
            identity.provider_occurrence_id,
            identity.recurrence_id,
            identity.recurrence_timezone,
            identity
                .recurrence_value_type
                .map(RecurrenceValueType::as_str),
            identity.occurrence.title,
            identity.occurrence.description,
            identity.occurrence.location,
            identity.occurrence.start_time,
            identity.occurrence.end_time,
            identity.occurrence.all_day,
            identity.occurrence.timezone,
            identity.provider_native_data,
            identity.provider_revision,
            identity.kind.as_str(),
        ],
    )?;
    if written != 1 {
        return Err(Error::Other("recurrence identity is immutable".into()));
    }
    Ok(())
}

pub fn get_by_object_id(conn: &Connection, object_id: &str) -> Result<Option<RecurrenceIdentity>> {
    let sql =
        format!("SELECT {SELECT_COLUMNS} FROM calendar_recurrence_objects WHERE object_id = ?1");
    let raw = conn.query_row(&sql, [object_id], raw_row).optional()?;
    raw.map(RawIdentity::decode).transpose()
}

pub fn get_by_event_id(conn: &Connection, event_id: &str) -> Result<Vec<RecurrenceIdentity>> {
    query_list(
        conn,
        &format!(
            "SELECT {SELECT_COLUMNS} FROM calendar_recurrence_objects
             WHERE event_id = ?1 ORDER BY effective_start, object_id"
        ),
        [event_id],
    )
}

pub fn list_series_objects(
    conn: &Connection,
    account_id: &str,
    local_series_event_id: Option<&str>,
    provider_calendar_id: Option<&str>,
    provider_series_id: Option<&str>,
) -> Result<Vec<RecurrenceIdentity>> {
    if local_series_event_id.is_none() && provider_series_id.is_none() {
        return Err(Error::Other(
            "a local or provider series identity is required".into(),
        ));
    }
    if provider_calendar_id.is_some() != provider_series_id.is_some() {
        return Err(Error::Other(
            "provider calendar and series identities must be supplied together".into(),
        ));
    }
    query_list(
        conn,
        &format!(
            "SELECT {SELECT_COLUMNS} FROM calendar_recurrence_objects
             WHERE account_id = ?1 AND (
                    (?2 IS NOT NULL AND local_series_event_id = ?2)
                  OR (?3 IS NOT NULL AND ?4 IS NOT NULL
                      AND provider_calendar_id = ?3 AND provider_series_id = ?4)
             )
             ORDER BY effective_start, object_id"
        ),
        params![
            account_id,
            local_series_event_id,
            provider_calendar_id,
            provider_series_id
        ],
    )
}

/// Resolve the one master named by exact local and/or provider series IDs.
/// When both are supplied they must identify the same persisted master.
pub fn resolve_master(
    conn: &Connection,
    account_id: &str,
    local_series_event_id: Option<&str>,
    provider_calendar_id: Option<&str>,
    provider_series_id: Option<&str>,
) -> Result<Option<RecurrenceIdentity>> {
    if local_series_event_id.is_none() && provider_series_id.is_none() {
        return Err(Error::Other("a series identity is required".into()));
    }

    let local = local_series_event_id
        .map(|event_id| {
            exact_master(
                conn,
                "account_id = ?1 AND event_id = ?2",
                params![account_id, event_id],
            )
        })
        .transpose()?
        .flatten();
    if provider_calendar_id.is_some() != provider_series_id.is_some() {
        return Err(Error::Other(
            "provider calendar and series identities must be supplied together".into(),
        ));
    }
    let provider = provider_calendar_id
        .zip(provider_series_id)
        .map(|(calendar_id, series_id)| {
            exact_master(
                conn,
                "account_id = ?1 AND provider_calendar_id = ?2
                 AND provider_series_id = ?3",
                params![account_id, calendar_id, series_id],
            )
        })
        .transpose()?
        .flatten();

    if local_series_event_id.is_some() && local.is_none() {
        return Err(Error::Other(
            "local series identity has no exact recurrence master".into(),
        ));
    }
    if provider_series_id.is_some() && provider.is_none() {
        return Err(Error::Other(
            "provider series identity has no exact recurrence master".into(),
        ));
    }

    match (local, provider) {
        (Some(local), Some(provider)) if local.object_id != provider.object_id => {
            Err(Error::Other(
                "local and provider series identities resolve to different masters".into(),
            ))
        }
        (Some(master), Some(_)) | (Some(master), None) | (None, Some(master)) => Ok(Some(master)),
        (None, None) => Ok(None),
    }
}

fn exact_master<P>(
    conn: &Connection,
    predicate: &str,
    params: P,
) -> Result<Option<RecurrenceIdentity>>
where
    P: rusqlite::Params,
{
    let identities = query_list(
        conn,
        &format!(
            "SELECT {SELECT_COLUMNS} FROM calendar_recurrence_objects
             WHERE object_kind = 'master' AND {predicate}
             ORDER BY object_id LIMIT 2"
        ),
        params,
    )?;
    match identities.as_slice() {
        [] => Ok(None),
        [identity] => Ok(Some(identity.clone())),
        _ => Err(Error::Other("ambiguous recurrence series master".into())),
    }
}

pub fn delete(conn: &Connection, object_id: &str) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM calendar_recurrence_objects WHERE object_id = ?1",
        [object_id],
    )? != 0)
}

fn query_list<P>(conn: &Connection, sql: &str, params: P) -> Result<Vec<RecurrenceIdentity>>
where
    P: rusqlite::Params,
{
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map(params, raw_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .map(RawIdentity::decode)
        .collect()
}

struct RawIdentity {
    object_id: String,
    account_id: String,
    event_id: String,
    local_series_event_id: Option<String>,
    provider_calendar_id: Option<String>,
    provider_series_id: Option<String>,
    provider_occurrence_id: Option<String>,
    recurrence_id: Option<String>,
    recurrence_timezone: Option<String>,
    recurrence_value_type: Option<String>,
    effective_title: String,
    effective_description: Option<String>,
    effective_location: Option<String>,
    effective_start: String,
    effective_end: String,
    effective_all_day: bool,
    effective_timezone: Option<String>,
    provider_native_data: Option<String>,
    provider_revision: Option<String>,
    kind: String,
}

fn raw_row(row: &Row<'_>) -> rusqlite::Result<RawIdentity> {
    Ok(RawIdentity {
        object_id: row.get(0)?,
        account_id: row.get(1)?,
        event_id: row.get(2)?,
        local_series_event_id: row.get(3)?,
        provider_calendar_id: row.get(4)?,
        provider_series_id: row.get(5)?,
        provider_occurrence_id: row.get(6)?,
        recurrence_id: row.get(7)?,
        recurrence_timezone: row.get(8)?,
        recurrence_value_type: row.get(9)?,
        effective_title: row.get(10)?,
        effective_description: row.get(11)?,
        effective_location: row.get(12)?,
        effective_start: row.get(13)?,
        effective_end: row.get(14)?,
        effective_all_day: row.get(15)?,
        effective_timezone: row.get(16)?,
        provider_native_data: row.get(17)?,
        provider_revision: row.get(18)?,
        kind: row.get(19)?,
    })
}

impl RawIdentity {
    fn decode(self) -> Result<RecurrenceIdentity> {
        let identity = RecurrenceIdentity {
            object_id: self.object_id,
            account_id: self.account_id,
            event_id: self.event_id,
            local_series_event_id: self.local_series_event_id,
            provider_calendar_id: self.provider_calendar_id,
            provider_series_id: self.provider_series_id,
            provider_occurrence_id: self.provider_occurrence_id,
            recurrence_id: self.recurrence_id,
            recurrence_timezone: self.recurrence_timezone,
            recurrence_value_type: self
                .recurrence_value_type
                .as_deref()
                .map(RecurrenceValueType::from_stored)
                .transpose()?,
            occurrence: OccurrenceFields {
                title: self.effective_title,
                description: self.effective_description,
                location: self.effective_location,
                start_time: self.effective_start,
                end_time: self.effective_end,
                all_day: self.effective_all_day,
                timezone: self.effective_timezone,
            },
            provider_native_data: self.provider_native_data,
            provider_revision: self.provider_revision,
            kind: RecurrenceObjectKind::from_stored(&self.kind)?,
        };
        identity.validate()?;
        Ok(identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;

    fn connection() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('account', 'Test', 'test@example.test', 'test@example.test');
             INSERT INTO calendar_events
                 (id, account_id, calendar_id, title, start_time, end_time)
             VALUES
                 ('master-event', 'account', 'calendar', 'Master',
                  '2026-09-15', '2026-09-16'),
                 ('event', 'account', 'calendar', 'Event',
                  '2026-09-16', '2026-09-17'),
                 ('other-event', 'account', 'calendar', 'Other',
                  '2026-09-17', '2026-09-18');",
        )
        .unwrap();
        conn
    }

    fn identity(kind: RecurrenceObjectKind, object_id: &str) -> RecurrenceIdentity {
        let non_master = kind != RecurrenceObjectKind::Master;
        RecurrenceIdentity {
            object_id: object_id.into(),
            account_id: "account".into(),
            event_id: if kind == RecurrenceObjectKind::Master {
                "master-event".into()
            } else {
                "event".into()
            },
            local_series_event_id: non_master.then(|| "master-event".into()),
            provider_calendar_id: non_master.then(|| "provider-calendar".into()),
            provider_series_id: non_master.then(|| "provider-series".into()),
            provider_occurrence_id: non_master.then(|| format!("provider-{object_id}")),
            recurrence_id: non_master.then(|| "2026-09-16".into()),
            recurrence_timezone: Some("Europe/Stockholm".into()),
            recurrence_value_type: non_master.then_some(RecurrenceValueType::Date),
            occurrence: OccurrenceFields {
                title: "Effective event".into(),
                description: Some("Effective description".into()),
                location: Some("Effective room".into()),
                start_time: "2026-09-16".into(),
                end_time: "2026-09-17".into(),
                all_day: true,
                timezone: Some("Europe/Helsinki".into()),
            },
            provider_native_data: Some("{\"provider\":true}".into()),
            provider_revision: Some("revision".into()),
            kind,
        }
    }

    #[test]
    fn every_kind_round_trips_and_crud_is_transaction_compatible() {
        let mut conn = connection();
        for (index, kind) in [
            RecurrenceObjectKind::Master,
            RecurrenceObjectKind::Occurrence,
            RecurrenceObjectKind::Exception,
            RecurrenceObjectKind::Exclusion,
        ]
        .into_iter()
        .enumerate()
        {
            let mut value = identity(kind, &format!("object-{index}"));
            if index > 1 {
                value.event_id = "other-event".into();
                value.recurrence_id = Some(format!("2026-09-{}", 15 + index));
            }
            upsert(&conn, &value).unwrap();
            assert_eq!(
                get_by_object_id(&conn, &value.object_id).unwrap(),
                Some(value)
            );
        }
        assert_eq!(get_by_event_id(&conn, "event").unwrap().len(), 1);
        assert_eq!(
            list_series_objects(&conn, "account", Some("master-event"), None, None)
                .unwrap()
                .len(),
            3
        );

        let tx = conn.transaction().unwrap();
        let mut updated = identity(RecurrenceObjectKind::Occurrence, "object-1");
        updated.provider_revision = Some("updated".into());
        updated.provider_native_data = Some("updated provider content".into());
        updated.kind = RecurrenceObjectKind::Exception;
        updated.occurrence.title = "Updated effective title".into();
        updated.occurrence.timezone = Some("America/Toronto".into());
        upsert(&tx, &updated).unwrap();
        let stored = get_by_object_id(&tx, "object-1").unwrap().unwrap();
        assert_eq!(stored.provider_revision, Some("updated".into()));
        assert_eq!(stored.occurrence, updated.occurrence);
        assert_eq!(stored, updated);
        assert!(delete(&tx, "object-1").unwrap());
        tx.rollback().unwrap();
        assert_eq!(
            get_by_object_id(&conn, "object-1")
                .unwrap()
                .unwrap()
                .provider_revision,
            Some("revision".into())
        );
    }

    #[test]
    fn validation_precedes_writes_and_duplicate_positions_are_rejected() {
        let conn = connection();
        let mut invalid = identity(RecurrenceObjectKind::Occurrence, "invalid");
        invalid.recurrence_id = None;
        assert!(upsert(&conn, &invalid).is_err());

        let first = identity(RecurrenceObjectKind::Occurrence, "first");
        upsert(&conn, &first).unwrap();
        let mut changed_calendar = first.clone();
        changed_calendar.provider_calendar_id = Some("other-provider-calendar".into());
        assert!(upsert(&conn, &changed_calendar).is_err());
        assert_eq!(
            get_by_object_id(&conn, "first").unwrap(),
            Some(first.clone())
        );

        let mut moved = first.clone();
        moved.recurrence_id = Some("2026-09-17".into());
        assert!(upsert(&conn, &moved).is_err());
        assert_eq!(
            get_by_object_id(&conn, "first").unwrap(),
            Some(first.clone())
        );

        let mut duplicate = first.clone();
        duplicate.object_id = "duplicate".into();
        duplicate.event_id = "other-event".into();
        assert!(upsert(&conn, &duplicate).is_err());

        duplicate.local_series_event_id = None;
        assert!(upsert(&conn, &duplicate).is_err());
    }

    #[test]
    fn schema_rejects_invalid_provider_calendar_pairing() {
        let conn = connection();
        for (object_id, provider_calendar_id, provider_series_id) in [
            ("missing-calendar", None, Some("provider-series")),
            ("orphan-calendar", Some("provider-calendar"), None),
            (
                "control-calendar",
                Some("provider\ncalendar"),
                Some("provider-series"),
            ),
        ] {
            assert!(conn
                .execute(
                    "INSERT INTO calendar_recurrence_objects
                         (object_id, account_id, event_id, local_series_event_id,
                          provider_calendar_id, provider_series_id, recurrence_id,
                          recurrence_value_type, effective_title, effective_start,
                          effective_end, effective_all_day, object_kind)
                     VALUES (?1, 'account', 'event', 'master-event', ?2, ?3,
                             '2026-09-16', 'date', 'Occurrence', '2026-09-16',
                             '2026-09-17', 1, 'occurrence')",
                    params![object_id, provider_calendar_id, provider_series_id],
                )
                .is_err());
        }
    }

    #[test]
    fn upsert_rejects_every_changed_identity_component_without_writes() {
        let conn = connection();
        let original = identity(RecurrenceObjectKind::Occurrence, "immutable");
        upsert(&conn, &original).unwrap();
        let revision = crate::db::calendar_revision::get(&conn, "event").unwrap();

        for field in [
            "account_id",
            "event_id",
            "local_series_event_id",
            "provider_calendar_id",
            "provider_series_id",
            "provider_occurrence_id",
            "recurrence_id",
            "recurrence_timezone",
            "recurrence_value_type",
        ] {
            let mut changed = original.clone();
            match field {
                "account_id" => changed.account_id = "other-account".into(),
                "event_id" => changed.event_id = "other-event".into(),
                "local_series_event_id" => changed.local_series_event_id = None,
                "provider_calendar_id" => changed.provider_calendar_id = Some("other".into()),
                "provider_series_id" => changed.provider_series_id = Some("other".into()),
                "provider_occurrence_id" => changed.provider_occurrence_id = Some("other".into()),
                "recurrence_id" => changed.recurrence_id = Some("2026-09-17".into()),
                "recurrence_timezone" => changed.recurrence_timezone = Some("UTC".into()),
                "recurrence_value_type" => {
                    changed.recurrence_value_type = Some(RecurrenceValueType::DateTime);
                }
                _ => unreachable!(),
            }
            changed.occurrence.title = "Must not persist".into();
            assert!(upsert(&conn, &changed).is_err(), "{field}");
            assert_eq!(
                get_by_object_id(&conn, "immutable").unwrap().as_ref(),
                Some(&original)
            );
            assert_eq!(
                crate::db::calendar_revision::get(&conn, "event").unwrap(),
                revision
            );
        }
    }

    #[test]
    fn schema_protects_identity_from_direct_updates_and_reparenting() {
        let conn = connection();
        let original = identity(RecurrenceObjectKind::Occurrence, "immutable");
        upsert(&conn, &original).unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('other-account', 'Other', 'other@example.test', 'other@example.test');
             INSERT INTO calendar_events
                 (id, account_id, calendar_id, title, start_time, end_time)
             VALUES ('other-account-event', 'other-account', 'other-calendar', 'Other',
                     '2026-09-16', '2026-09-17');",
        )
        .unwrap();
        let revision = crate::db::calendar_revision::get(&conn, "event").unwrap();
        for assignment in [
            "object_id = 'renamed'",
            "event_id = 'other-event'",
            "account_id = 'other-account', event_id = 'other-account-event', local_series_event_id = NULL",
            "local_series_event_id = 'other-event'",
            "local_series_event_id = NULL",
            "provider_calendar_id = 'other-calendar'",
            "provider_series_id = 'other-series'",
            "provider_series_id = NULL",
            "provider_occurrence_id = 'other-occurrence'",
            "provider_occurrence_id = NULL",
            "recurrence_id = '2026-09-17'",
            "recurrence_timezone = 'UTC'",
            "recurrence_timezone = NULL",
            "recurrence_value_type = 'date-time'",
        ] {
            let error = conn.execute(
                &format!("UPDATE calendar_recurrence_objects SET {assignment} WHERE object_id = 'immutable'"),
                [],
            ).unwrap_err();
            assert!(error.to_string().contains("recurrence identity is immutable"), "{assignment}: {error}");
            assert_eq!(get_by_object_id(&conn, "immutable").unwrap().as_ref(), Some(&original));
            assert_eq!(crate::db::calendar_revision::get(&conn, "event").unwrap(), revision);
        }

        let mut unlinked = original.clone();
        unlinked.object_id = "unlinked".into();
        unlinked.local_series_event_id = None;
        unlinked.provider_occurrence_id = Some("unlinked-occurrence".into());
        unlinked.recurrence_id = Some("2026-09-18".into());
        upsert(&conn, &unlinked).unwrap();
        assert!(conn
            .execute(
                "UPDATE calendar_recurrence_objects SET local_series_event_id = 'master-event'
             WHERE object_id = 'unlinked'",
                [],
            )
            .is_err());
    }

    #[test]
    fn owning_event_cascades_and_local_series_event_is_set_null() {
        let conn = connection();
        let occurrence = identity(RecurrenceObjectKind::Occurrence, "occurrence");
        upsert(&conn, &occurrence).unwrap();
        let mut local_only = identity(RecurrenceObjectKind::Exception, "local-only");
        local_only.event_id = "other-event".into();
        local_only.provider_calendar_id = None;
        local_only.provider_series_id = None;
        local_only.provider_occurrence_id = None;
        local_only.recurrence_id = Some("2026-09-17".into());
        upsert(&conn, &local_only).unwrap();
        let revision = crate::db::calendar_revision::get(&conn, "event").unwrap();
        conn.execute("DELETE FROM calendar_events WHERE id = 'master-event'", [])
            .unwrap();
        let stored = get_by_object_id(&conn, "occurrence").unwrap().unwrap();
        assert_eq!(stored.local_series_event_id, None);
        assert_eq!(
            stored.provider_series_id.as_deref(),
            Some("provider-series")
        );
        assert!(get_by_object_id(&conn, "local-only").unwrap().is_none());
        assert!(crate::db::calendar_revision::get(&conn, "event").unwrap() > revision);
        upsert(&conn, &stored).unwrap();

        conn.execute("DELETE FROM calendar_events WHERE id = 'event'", [])
            .unwrap();
        assert!(get_by_object_id(&conn, "occurrence").unwrap().is_none());
    }

    #[test]
    fn unknown_stored_enum_values_fail_closed() {
        let conn = connection();
        let value = identity(RecurrenceObjectKind::Occurrence, "unknown");
        upsert(&conn, &value).unwrap();
        conn.execute_batch(
            "PRAGMA ignore_check_constraints=ON;
             UPDATE calendar_recurrence_objects SET object_kind = 'future';
             PRAGMA ignore_check_constraints=OFF;",
        )
        .unwrap();
        assert!(get_by_object_id(&conn, "unknown").is_err());

        conn.execute(
            "DELETE FROM calendar_recurrence_objects WHERE object_id = 'unknown'",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "PRAGMA ignore_check_constraints=ON;
             INSERT INTO calendar_recurrence_objects
                 (object_id, account_id, event_id, local_series_event_id,
                   recurrence_id, recurrence_value_type, effective_title,
                   effective_start, effective_end, effective_all_day,
                   object_kind)
             VALUES ('unknown', 'account', 'event', 'master-event',
                      '2026-09-16', 'instant', 'Event', '2026-09-16',
                      '2026-09-17', 1,
                     'occurrence');
             PRAGMA ignore_check_constraints=OFF;",
        )
        .unwrap();
        assert!(get_by_object_id(&conn, "unknown").is_err());
    }

    #[test]
    fn master_resolution_is_account_scoped_and_cross_checks_both_identities() {
        let conn = connection();
        let mut master = identity(RecurrenceObjectKind::Master, "master");
        master.provider_calendar_id = Some("provider-calendar".into());
        master.provider_series_id = Some("provider-series".into());
        upsert(&conn, &master).unwrap();

        assert_eq!(
            resolve_master(
                &conn,
                "account",
                Some("master-event"),
                Some("provider-calendar"),
                Some("provider-series")
            )
            .unwrap(),
            Some(master)
        );
        assert!(resolve_master(&conn, "other-account", Some("master-event"), None, None).is_err());
        assert!(resolve_master(&conn, "account", None, None, None).is_err());

        conn.execute(
            "INSERT INTO calendar_events
                 (id, account_id, calendar_id, title, start_time, end_time)
             VALUES ('second-master', 'account', 'calendar', 'Second master',
                     '2026-09-18', '2026-09-19')",
            [],
        )
        .unwrap();
        let mut duplicate = identity(RecurrenceObjectKind::Master, "duplicate-master");
        duplicate.event_id = "second-master".into();
        duplicate.provider_calendar_id = Some("provider-calendar".into());
        duplicate.provider_series_id = Some("provider-series".into());
        duplicate.occurrence.start_time = "2026-09-18".into();
        duplicate.occurrence.end_time = "2026-09-19".into();
        assert!(upsert(&conn, &duplicate).is_err());

        let mut same_owner = identity(RecurrenceObjectKind::Master, "same-owner-master");
        same_owner.occurrence.title = "Duplicate owner".into();
        assert!(upsert(&conn, &same_owner).is_err());

        // The resolver remains defensive for databases created before the
        // uniqueness constraints existed.
        conn.execute_batch("DROP INDEX idx_calendar_recurrence_provider_master")
            .unwrap();
        upsert(&conn, &duplicate).unwrap();
        assert!(resolve_master(
            &conn,
            "account",
            None,
            Some("provider-calendar"),
            Some("provider-series")
        )
        .is_err());
    }

    #[test]
    fn provider_uniqueness_is_scoped_by_calendar() {
        let conn = connection();
        let first = identity(RecurrenceObjectKind::Occurrence, "first-calendar");
        upsert(&conn, &first).unwrap();

        let mut second = first.clone();
        second.object_id = "second-calendar".into();
        second.event_id = "other-event".into();
        second.local_series_event_id = None;
        second.provider_calendar_id = Some("other-provider-calendar".into());
        upsert(&conn, &second).unwrap();

        assert_eq!(
            get_by_object_id(&conn, "first-calendar").unwrap(),
            Some(first)
        );
        assert_eq!(
            get_by_object_id(&conn, "second-calendar").unwrap(),
            Some(second)
        );
    }

    #[test]
    fn recurrence_writes_revise_owners_and_cascades_leave_no_tokens() {
        for recursive in [false, true] {
            let conn = connection();
            conn.pragma_update(None, "recursive_triggers", recursive)
                .unwrap();
            let event_before = crate::db::calendar_revision::get(&conn, "event").unwrap();
            let other_before = crate::db::calendar_revision::get(&conn, "other-event").unwrap();
            let mut value = identity(RecurrenceObjectKind::Occurrence, "revision-object");
            upsert(&conn, &value).unwrap();
            let after_insert = crate::db::calendar_revision::get(&conn, "event").unwrap();
            assert!(after_insert > event_before);
            assert_eq!(
                crate::db::calendar_revision::get(&conn, "other-event").unwrap(),
                other_before
            );

            value.event_id = "other-event".into();
            value.recurrence_id = Some("2026-09-18".into());
            conn.execute_batch("DROP TRIGGER calendar_recurrence_id_immutable")
                .unwrap();
            // Exercise the revision trigger defensively against legacy raw writes.
            conn.execute(
                "UPDATE calendar_recurrence_objects SET event_id = ?1, recurrence_id = ?2
                 WHERE object_id = ?3",
                params![value.event_id, value.recurrence_id, value.object_id],
            )
            .unwrap();
            assert!(crate::db::calendar_revision::get(&conn, "event").unwrap() > after_insert);
            assert!(
                crate::db::calendar_revision::get(&conn, "other-event").unwrap() > other_before
            );

            let before_delete = crate::db::calendar_revision::get(&conn, "other-event").unwrap();
            delete(&conn, "revision-object").unwrap();
            assert!(
                crate::db::calendar_revision::get(&conn, "other-event").unwrap() > before_delete
            );

            value.object_id = "cascade-object".into();
            upsert(&conn, &value).unwrap();
            conn.execute("DELETE FROM calendar_events WHERE id = 'other-event'", [])
                .unwrap();
            assert!(get_by_object_id(&conn, "cascade-object").unwrap().is_none());
            assert!(crate::db::calendar_revision::get(&conn, "other-event").is_err());
        }
    }
}
