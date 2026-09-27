//! Private revision-bound read snapshots and durable action intents.
//!
//! Snapshots are disposable views of calendar_events/calendar_recurrence_objects.
//! Every use checks the source revisions; sync never competes with another truth.
//! Operation records, in contrast, survive stale snapshots and application restart.

use chrono::TimeZone;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::calendar::actions::{invalid, CalendarActionInput, CalendarActionStage};
use crate::calendar::event_set::{
    apply_event_fields, event_fields, CalendarEventSet, CalendarOverride,
};
use crate::calendar::recurrence_identity::{
    RecurrenceIdentitySeed, RecurrenceObjectKind, RecurrenceValueType,
};
use crate::calendar::{CalendarEvent, RecurrenceKind};
use crate::error::Result;

pub(crate) fn encode<T: Serialize>(value: &T) -> Result<String> {
    let encoded = serde_json::to_string(value)
        .map_err(|error| invalid(&format!("cannot serialize private journal: {error}")))?;
    if encoded.len() > 16 * 1024 * 1024 {
        return Err(invalid("private snapshot exceeds 16 MiB budget"));
    }
    Ok(encoded)
}

pub(crate) fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value)
        .map_err(|error| invalid(&format!("invalid private journal: {error}")))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EventVersion {
    pub event: CalendarEvent,
    pub revision: i64,
    #[serde(default)]
    pub invitation_source: Option<(String, String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub token: String,
    pub anchor: CalendarEvent,
    pub members: Vec<EventVersion>,
    pub set: CalendarEventSet,
    pub remote_calendar_id: Option<String>,
    pub account_route: String,
    pub invitation_source: Option<(String, String, String)>,
    pub calendar_revision: i64,
}

pub(crate) fn invitation_source(
    conn: &Connection,
    event_id: &str,
) -> Result<Option<(String, String, String)>> {
    Ok(
        super::calendar_invitation_source::get(conn, event_id)?.map(|source| {
            (
                source.source_account_id,
                source.source_message_id,
                source.invitation_uid,
            )
        }),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Destination {
    pub calendar_id: String,
    pub account_id: String,
    pub remote_calendar_id: Option<String>,
    pub protocol: Option<String>,
    pub account_route: String,
}

pub(crate) fn account_route(conn: &Connection, account_id: &str) -> Result<String> {
    super::accounts::calendar_route_fingerprint(conn, account_id)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Operation {
    pub id: String,
    pub source: Snapshot,
    pub input: CalendarActionInput,
    pub desired: CalendarEventSet,
    pub destination: Option<Destination>,
    pub destination_event_id: String,
    pub stage: CalendarActionStage,
    pub canonical: Option<CalendarEventSet>,
    pub source_after: Option<CalendarEventSet>,
    pub native_move: bool,
    #[serde(default)]
    pub auto_resume: bool,
}

pub(crate) fn initialize(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS calendar_action_calendar_revisions (
            revision INTEGER PRIMARY KEY AUTOINCREMENT,
            calendar_id TEXT NOT NULL UNIQUE REFERENCES calendars(id) ON DELETE CASCADE
        );
        CREATE TRIGGER IF NOT EXISTS calendar_action_calendar_revision_write
        AFTER INSERT ON calendar_event_revisions BEGIN
            DELETE FROM calendar_action_calendar_revisions WHERE calendar_id IN
                (SELECT calendar_id FROM calendar_events WHERE id = NEW.event_id);
            INSERT INTO calendar_action_calendar_revisions(calendar_id)
                SELECT calendar.id FROM calendars calendar JOIN calendar_events event
                ON event.calendar_id = calendar.id WHERE event.id = NEW.event_id;
        END;
        CREATE TRIGGER IF NOT EXISTS calendar_action_calendar_revision_delete
        AFTER DELETE ON calendar_events BEGIN
            DELETE FROM calendar_action_calendar_revisions WHERE calendar_id = OLD.calendar_id;
            INSERT INTO calendar_action_calendar_revisions(calendar_id)
                SELECT id FROM calendars WHERE id = OLD.calendar_id;
        END;
        CREATE TRIGGER IF NOT EXISTS calendar_action_calendar_revision_move
        AFTER UPDATE OF calendar_id ON calendar_events WHEN OLD.calendar_id IS NOT NEW.calendar_id BEGIN
            DELETE FROM calendar_action_calendar_revisions WHERE calendar_id = OLD.calendar_id;
            INSERT INTO calendar_action_calendar_revisions(calendar_id)
                SELECT id FROM calendars WHERE id = OLD.calendar_id;
        END;
        INSERT INTO calendar_action_calendar_revisions(calendar_id)
            SELECT id FROM calendars WHERE NOT EXISTS (
                SELECT 1 FROM calendar_action_calendar_revisions WHERE calendar_id = calendars.id
            );
        CREATE TABLE IF NOT EXISTS calendar_action_snapshots (
            token TEXT PRIMARY KEY,
            event_id TEXT NOT NULL REFERENCES calendar_events(id) ON DELETE CASCADE,
            data TEXT NOT NULL,
            expires_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS calendar_action_snapshots_event
            ON calendar_action_snapshots(event_id);
        CREATE TABLE IF NOT EXISTS calendar_action_operations (
            operation_id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
            event_id TEXT NOT NULL,
            data TEXT NOT NULL,
            completed INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS calendar_action_operations_pending
            ON calendar_action_operations(account_id, completed);
        CREATE TABLE IF NOT EXISTS calendar_action_claims (
            event_id TEXT PRIMARY KEY,
            operation_id TEXT NOT NULL REFERENCES calendar_action_operations(operation_id) ON DELETE CASCADE
        );
        CREATE TABLE IF NOT EXISTS calendar_action_creations (
            operation_id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
            event_id TEXT NOT NULL,
            data TEXT NOT NULL,
            completed INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS calendar_action_sets (
            event_id TEXT PRIMARY KEY REFERENCES calendar_events(id) ON DELETE CASCADE,
            data TEXT NOT NULL,
            revision INTEGER NOT NULL,
            dirty INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS calendar_action_members (
            event_id TEXT PRIMARY KEY REFERENCES calendar_events(id) ON DELETE CASCADE,
            owner_event_id TEXT NOT NULL REFERENCES calendar_events(id) ON DELETE CASCADE,
            original_start TEXT
        );
        CREATE TABLE IF NOT EXISTS calendar_action_addresses (
            account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
            calendar_id TEXT NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
            remote_id TEXT NOT NULL,
            owner_event_id TEXT NOT NULL,
            retired INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(account_id, calendar_id, remote_id)
        );
        CREATE TABLE IF NOT EXISTS calendar_action_retired_objects (
            object_id TEXT PRIMARY KEY,
            data TEXT NOT NULL
        );"
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn event_version(conn: &Connection, event: CalendarEvent) -> Result<EventVersion> {
    Ok(EventVersion {
        revision: super::calendar_revision::get(conn, &event.id)?,
        invitation_source: invitation_source(conn, &event.id)?,
        event,
    })
}

pub(crate) fn ensure_current(conn: &Connection, snapshot: &Snapshot) -> Result<()> {
    let current_calendar_revision = calendar_revision(conn, &snapshot.anchor.calendar_id)?;
    if current_calendar_revision != snapshot.calendar_revision {
        log::warn!(
            "Calendar action snapshot revision changed: event_id={} calendar_id={} expected_revision={} current_revision={current_calendar_revision}",
            snapshot.anchor.id,
            snapshot.anchor.calendar_id,
            snapshot.calendar_revision
        );
        return Err(invalid("calendar contents changed; refresh before editing"));
    }
    if invitation_source(conn, &snapshot.anchor.id)? != snapshot.invitation_source {
        return Err(invalid(
            "invitation provenance changed; refresh before editing",
        ));
    }
    if account_route(conn, &snapshot.anchor.account_id)? != snapshot.account_route {
        return Err(invalid(
            "account calendar routing changed; refresh before editing",
        ));
    }
    for member in &snapshot.members {
        if super::calendar_revision::get(conn, &member.event.id)? != member.revision
            || super::calendar::get_event(conn, &member.event.id)? != member.event
            || invitation_source(conn, &member.event.id)? != member.invitation_source
        {
            return Err(invalid("selection is stale; refresh before editing"));
        }
    }
    let calendar = super::calendar::get_calendar(conn, &snapshot.anchor.calendar_id)?;
    if calendar.account_id != snapshot.anchor.account_id
        || calendar.remote_id != snapshot.remote_calendar_id
        || !calendar.is_subscribed
    {
        return Err(invalid("source calendar changed or is unavailable"));
    }
    Ok(())
}

pub(crate) fn ensure_operation_current(conn: &Connection, operation: &Operation) -> Result<()> {
    if operation.stage == CalendarActionStage::Planned {
        return ensure_current(conn, &operation.source);
    }
    let mut snapshot = operation.source.clone();
    snapshot.calendar_revision = calendar_revision(conn, &snapshot.anchor.calendar_id)?;
    ensure_current(conn, &snapshot)?;
    let current = set_members(conn, &snapshot.anchor, &snapshot.set)?;
    let ids: std::collections::HashSet<_> = current.iter().map(|member| &member.event.id).collect();
    let expected: std::collections::HashSet<_> = snapshot
        .members
        .iter()
        .map(|member| &member.event.id)
        .collect();
    if ids != expected {
        return Err(invalid("claimed series membership changed during recovery"));
    }
    Ok(())
}

pub(crate) fn calendar_revision(conn: &Connection, calendar_id: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT revision FROM calendar_action_calendar_revisions WHERE calendar_id = ?1",
        [calendar_id],
        |row| row.get(0),
    )?)
}

pub(crate) fn save_snapshot(conn: &Connection, snapshot: &Snapshot) -> Result<()> {
    ensure_current(conn, snapshot)?;
    conn.execute(
        "DELETE FROM calendar_action_snapshots WHERE expires_at < unixepoch()",
        [],
    )?;
    // A bounded number per anchor prevents repeated visible-window reads from
    // accumulating unbounded native resources during one long session.
    conn.execute(
        "DELETE FROM calendar_action_snapshots WHERE event_id = ?1 AND token NOT IN
         (SELECT token FROM calendar_action_snapshots WHERE event_id = ?1 ORDER BY expires_at DESC LIMIT 3)",
        [&snapshot.anchor.id],
    )?;
    conn.execute(
        "INSERT INTO calendar_action_snapshots(token, event_id, data, expires_at)
         VALUES (?1, ?2, ?3, unixepoch() + 1800)
         ON CONFLICT(token) DO NOTHING",
        params![snapshot.token, snapshot.anchor.id, encode(snapshot)?],
    )?;
    Ok(())
}

pub(crate) fn load_snapshot(conn: &Connection, token: &str, event_id: &str) -> Result<Snapshot> {
    let data: Option<String> = conn.query_row(
        "SELECT data FROM calendar_action_snapshots WHERE token = ?1 AND event_id = ?2 AND expires_at >= unixepoch()",
        params![token, event_id], |row| row.get(0),
    ).optional()?;
    let snapshot: Snapshot =
        decode(&data.ok_or_else(|| invalid("selection expired; refresh before editing"))?)?;
    ensure_current(conn, &snapshot)?;
    Ok(snapshot)
}

pub(crate) fn latest_snapshot(conn: &Connection, event_id: &str) -> Result<Option<Snapshot>> {
    let owner = owner_id(conn, event_id)?;
    if let Some(set) = owned_set(conn, &owner)? {
        return Ok(Some(snapshot_for_set(conn, &owner, set)?));
    }
    let mut stmt = conn.prepare(
        "SELECT data FROM calendar_action_snapshots WHERE event_id = ?1
         AND expires_at >= unixepoch() ORDER BY expires_at DESC LIMIT 4",
    )?;
    let rows = stmt
        .query_map([event_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for data in rows {
        let snapshot: Snapshot = decode(&data)?;
        if snapshot.set.native.is_some() && ensure_current(conn, &snapshot).is_ok() {
            return Ok(Some(snapshot));
        }
    }
    Ok(None)
}

pub(crate) fn cache_canonical(
    conn: &Connection,
    anchor: CalendarEvent,
    set: &CalendarEventSet,
) -> Result<()> {
    let owned: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM calendar_action_sets WHERE event_id = ?1)",
        [&anchor.id],
        |row| row.get(0),
    )?;
    if owned {
        save_owned_set(conn, &anchor, set)?;
    }
    let snapshot = snapshot_for_set(conn, &anchor.id, set.clone())?;
    save_snapshot(conn, &snapshot)
}

fn snapshot_for_set(conn: &Connection, event_id: &str, set: CalendarEventSet) -> Result<Snapshot> {
    let anchor = super::calendar::get_event(conn, event_id)?;
    let calendar = super::calendar::get_calendar(conn, &anchor.calendar_id)?;
    let members = set_members(conn, &anchor, &set)?;
    let snapshot = Snapshot {
        calendar_revision: calendar_revision(conn, &anchor.calendar_id)?,
        token: uuid::Uuid::new_v4().to_string(),
        account_route: account_route(conn, &anchor.account_id)?,
        invitation_source: invitation_source(conn, &anchor.id)?,
        members,
        anchor,
        set,
        remote_calendar_id: calendar.remote_id,
    };
    Ok(snapshot)
}

/// Reconstruct a clean local projection for a claimed operation whose remote
/// effect may already have been imported. Planned actions never use this path.
pub(crate) fn recovery_snapshot(
    conn: &Connection,
    operation: &Operation,
) -> Result<Option<Snapshot>> {
    if operation.stage == CalendarActionStage::Planned || operation.destination.is_some() {
        return Ok(None);
    }
    let owner = owner_id(conn, &operation.source.anchor.id)?;
    let Some(set) = owned_set(conn, &owner)? else {
        log::warn!(
            "Calendar action recovery projection is absent or dirty: operation_id={} owner_event_id={owner}",
            operation.id
        );
        return Ok(None);
    };
    let snapshot = snapshot_for_set(conn, &operation.source.anchor.id, set)?;
    if snapshot.anchor.account_id != operation.source.anchor.account_id
        || snapshot.anchor.calendar_id != operation.source.anchor.calendar_id
        || snapshot.remote_calendar_id != operation.source.remote_calendar_id
        || snapshot.account_route != operation.source.account_route
        || snapshot.invitation_source != operation.source.invitation_source
    {
        log::warn!(
            "Calendar action recovery projection changed routing or provenance: operation_id={}",
            operation.id
        );
        return Ok(None);
    }
    let current: std::collections::HashSet<_> = snapshot
        .members
        .iter()
        .map(|member| member.event.id.as_str())
        .collect();
    let expected: std::collections::HashSet<_> = operation
        .source
        .members
        .iter()
        .map(|member| member.event.id.as_str())
        .collect();
    if current != expected {
        log::warn!(
            "Calendar action recovery member set changed: operation_id={} current_members={} expected_members={} current_is_subset={} expected_is_subset={}",
            operation.id,
            current.len(),
            expected.len(),
            current.is_subset(&expected),
            expected.is_subset(&current)
        );
        return Ok(None);
    }
    for event_id in current {
        let claim: Option<String> = conn
            .query_row(
                "SELECT operation_id FROM calendar_action_claims WHERE event_id = ?1",
                [event_id],
                |row| row.get(0),
            )
            .optional()?;
        if claim.as_deref() != Some(operation.id.as_str()) {
            log::warn!(
                "Calendar action recovery member is not exclusively claimed: operation_id={} event_id={event_id}",
                operation.id
            );
            return Ok(None);
        }
    }
    Ok(Some(snapshot))
}

pub(crate) fn owner_id(conn: &Connection, event_id: &str) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT owner_event_id FROM calendar_action_members WHERE event_id = ?1",
            [event_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_else(|| event_id.to_owned()))
}

fn owned_set(conn: &Connection, event_id: &str) -> Result<Option<CalendarEventSet>> {
    let data: Option<String> = conn
        .query_row(
            "SELECT s.data FROM calendar_action_sets s JOIN calendar_event_revisions r ON r.event_id = s.event_id
             WHERE s.event_id = ?1 AND s.dirty = 0 AND s.revision = r.revision",
            [event_id],
            |row| row.get(0),
        )
        .optional()?;
    data.map(|data| decode(&data)).transpose()
}

/// Last complete provider set for display only. A dirty set is not proof for
/// planning or mutation, but remains more coherent than expanding its master
/// while dropping all known exceptions.
pub(crate) fn display_owned_set(
    conn: &Connection,
    event_id: &str,
) -> Result<Option<CalendarEventSet>> {
    let owner = owner_id(conn, event_id)?;
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM calendar_action_sets WHERE event_id = ?1",
            [owner],
            |row| row.get(0),
        )
        .optional()?;
    data.map(|data| decode(&data)).transpose()
}

/// Last provider-verified result from a completed in-place action. This is a
/// display-only bridge for operations completed before recurring sets were
/// stored durably; callers must still require hydration before mutation.
pub(crate) fn completed_canonical_for_display(
    conn: &Connection,
    event_id: &str,
) -> Result<Option<CalendarEventSet>> {
    let mut stmt = conn.prepare(
        "SELECT data FROM calendar_action_operations
         WHERE event_id = ?1 AND completed = 1
         ORDER BY created_at DESC, operation_id DESC LIMIT 10",
    )?;
    let rows = stmt
        .query_map([event_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for data in rows {
        let operation: Operation = decode(&data)?;
        if operation.stage == CalendarActionStage::Completed
            && operation.destination.is_none()
            && operation.source.anchor.id == event_id
        {
            if let Some(set) = operation.canonical {
                return Ok(Some(set));
            }
        }
    }
    Ok(None)
}

/// Membership is determined by exact addresses and explicit recurrence identity,
/// including historical projections outside the current display window.
pub(crate) fn set_members(
    conn: &Connection,
    anchor: &CalendarEvent,
    set: &CalendarEventSet,
) -> Result<Vec<EventVersion>> {
    let mut ids = std::collections::HashSet::from([anchor.id.clone()]);
    let owner = owner_id(conn, &anchor.id)?;
    ids.insert(owner.clone());
    let mut stmt =
        conn.prepare("SELECT event_id FROM calendar_action_members WHERE owner_event_id = ?1")?;
    for id in stmt.query_map([&owner], |row| row.get::<_, String>(0))? {
        ids.insert(id?);
    }
    if let Some(native) = &set.native {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT e.id FROM calendar_events e
             LEFT JOIN calendar_recurrence_objects r ON r.event_id = e.id
             WHERE e.account_id = ?1 AND e.calendar_id = ?2
               AND (e.remote_id = ?3 OR (r.provider_calendar_id = ?4 AND r.provider_series_id = ?3))")?;
        for id in stmt.query_map(
            params![
                anchor.account_id,
                anchor.calendar_id,
                native.event_id,
                native.calendar_id
            ],
            |row| row.get::<_, String>(0),
        )? {
            ids.insert(id?);
        }
        for item in &set.overrides {
            if let Some(resource) = &item.native {
                if resource.protocol != native.protocol
                    || resource.calendar_id != native.calendar_id
                {
                    return Err(invalid("override belongs to another provider calendar"));
                }
                let mut stmt = conn.prepare("SELECT id FROM calendar_events WHERE account_id = ?1 AND calendar_id = ?2 AND remote_id = ?3")?;
                for id in stmt.query_map(
                    params![anchor.account_id, anchor.calendar_id, resource.event_id],
                    |row| row.get::<_, String>(0),
                )? {
                    ids.insert(id?);
                }
            }
        }
    }
    if ids.len() > 10_000 {
        return Err(invalid("series cache exceeds member budget"));
    }
    ids.into_iter()
        .map(|id| event_version(conn, super::calendar::get_event(conn, &id)?))
        .collect()
}

fn retire_objects(conn: &Connection, event_id: &str) -> Result<()> {
    for identity in super::calendar_recurrence::get_by_event_id(conn, event_id)? {
        conn.execute("INSERT OR IGNORE INTO calendar_action_retired_objects(object_id, data) VALUES (?1, ?2)", params![identity.object_id, encode(&identity)?])?;
        super::calendar_recurrence::delete(conn, &identity.object_id)?;
    }
    Ok(())
}

fn address(
    conn: &Connection,
    event: &CalendarEvent,
    remote: &str,
    owner: &str,
    retired: bool,
) -> Result<()> {
    address_identity(
        conn,
        &event.account_id,
        &event.calendar_id,
        remote,
        owner,
        retired,
    )
}

fn address_identity(
    conn: &Connection,
    account_id: &str,
    calendar_id: &str,
    remote: &str,
    owner: &str,
    retired: bool,
) -> Result<()> {
    conn.execute("INSERT INTO calendar_action_addresses(account_id, calendar_id, remote_id, owner_event_id, retired)
        VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(account_id, calendar_id, remote_id)
        DO UPDATE SET owner_event_id = excluded.owner_event_id, retired = excluded.retired",
        params![account_id, calendar_id, remote, owner, retired])?;
    Ok(())
}

/// The complete canonical resource owns recurrence objects. Expanded child rows
/// remain stable references, but never acquire a master's provider address.
pub(crate) fn persist_source(
    conn: &Connection,
    source: &Snapshot,
    set: &CalendarEventSet,
) -> Result<CalendarEvent> {
    persist_verified_set(conn, &source.anchor, &source.set, set)
}

/// Sync may verify a complete provider set without a renderer selection token.
/// Keep ingestion, membership and the canonical cache in the caller's transaction.
pub(crate) fn persist_synced_caldav_set(
    tx: &rusqlite::Transaction<'_>,
    resources: &[(&CalendarEvent, &[RecurrenceIdentitySeed])],
    set: &CalendarEventSet,
) -> Result<bool> {
    if tx.is_autocommit()
        || resources.is_empty()
        || set.native.as_ref().is_none_or(|native| {
            native.protocol != "caldav"
                || set.event.remote_id.as_deref() != Some(native.event_id.as_str())
        })
    {
        return Err(invalid("sync requires a complete CalDAV source"));
    }
    set.validate()?;
    let master = set.native.as_ref().expect("checked native CalDAV source");
    let claimed: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM calendar_action_claims claim
         JOIN calendar_events event ON event.id = claim.event_id
         WHERE event.account_id = ?1 AND event.calendar_id = ?2
           AND (event.uid = ?3 OR event.remote_id = ?4))",
        params![
            set.event.account_id,
            set.event.calendar_id,
            set.event.uid,
            master.event_id
        ],
        |row| row.get(0),
    )?;
    if claimed {
        return Ok(false);
    }
    for (event, seeds) in resources {
        if event.account_id != set.event.account_id
            || event.calendar_id != set.event.calendar_id
            || event.uid != set.event.uid
        {
            return Err(invalid("sync resource belongs to another series"));
        }
        super::calendar::upsert_event_by_remote_id_with_recurrence_in_transaction(
            tx, event, seeds,
        )?;
    }
    let anchor_id: String = tx.query_row(
        "SELECT id FROM calendar_events WHERE account_id = ?1 AND calendar_id = ?2 AND remote_id = ?3",
        params![set.event.account_id, set.event.calendar_id, master.event_id],
        |row| row.get(0),
    )?;
    let anchor = super::calendar::get_event(tx, &anchor_id)?;
    let owner = persist_verified_set(tx, &anchor, set, set)?;
    let mut active = std::collections::HashSet::from([master.event_id.as_str()]);
    for native in set.overrides.iter().filter_map(|item| item.native.as_ref()) {
        active.insert(native.event_id.as_str());
    }
    let mut stmt = tx.prepare(
        "SELECT remote_id FROM calendar_action_addresses
         WHERE account_id = ?1 AND calendar_id = ?2 AND owner_event_id = ?3 AND retired = 0",
    )?;
    let addresses = stmt
        .query_map(
            params![owner.account_id, owner.calendar_id, owner.id],
            |row| row.get::<_, String>(0),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for remote in addresses {
        if !active.contains(remote.as_str()) {
            tx.execute(
                "DELETE FROM calendar_action_addresses WHERE account_id = ?1 AND calendar_id = ?2 AND remote_id = ?3 AND owner_event_id = ?4 AND retired = 0",
                params![owner.account_id, owner.calendar_id, remote, owner.id],
            )?;
        }
    }
    Ok(true)
}

/// Delete missing CalDAV owners with their addressless detached rows. A claim
/// keeps the whole set intact until its in-progress action is resolved.
pub(crate) fn delete_missing_caldav_events(
    tx: &rusqlite::Transaction<'_>,
    missing_owners: &[String],
) -> Result<usize> {
    let mut deleted = 0;
    for owner in missing_owners {
        let mut members = vec![owner.clone()];
        let mut stmt = tx.prepare(
            "SELECT event_id FROM calendar_action_members WHERE owner_event_id = ?1 AND event_id != ?1",
        )?;
        members.extend(
            stmt.query_map([owner], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
        let mut claimed = false;
        for id in &members {
            let active: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM calendar_action_claims WHERE event_id = ?1)",
                [id],
                |row| row.get(0),
            )?;
            claimed |= active;
        }
        if claimed {
            log::info!("sync_calendars: deferred deletion of claimed CalDAV owner={owner}");
            continue;
        }
        tx.execute(
            "DELETE FROM calendar_action_addresses WHERE owner_event_id = ?1 AND retired = 0",
            [owner],
        )?;
        deleted += super::calendar_event_deletion::delete_events(tx, &members)?.deleted;
    }
    Ok(deleted)
}

fn persist_verified_set(
    conn: &Connection,
    anchor: &CalendarEvent,
    source_set: &CalendarEventSet,
    set: &CalendarEventSet,
) -> Result<CalendarEvent> {
    let members = set_members(conn, anchor, source_set)?;
    let owner = match &set.native {
        Some(native) => members
            .iter()
            .find(|member| {
                member.event.remote_id.as_deref() == Some(native.event_id.as_str())
                    && member.event.recurrence_kind != RecurrenceKind::Occurrence
            })
            .map(|member| member.event.clone()),
        None => Some(super::calendar::get_event(
            conn,
            &owner_id(conn, &anchor.id)?,
        )?),
    };
    let owner = match owner {
        Some(owner) => owner,
        None => {
            let mut event = set.event.clone();
            event.id = uuid::Uuid::new_v4().to_string();
            event.account_id = anchor.account_id.clone();
            event.calendar_id = anchor.calendar_id.clone();
            event.source_message_id = anchor.source_message_id.clone();
            super::calendar::insert_event(conn, &event)?;
            if let Some(mut binding) = super::meet_meetings::get(conn, &anchor.id)? {
                binding.event_id = event.id.clone();
                super::meet_meetings::upsert(conn, &binding)?;
            }
            event
        }
    };
    conn.execute(
        "DELETE FROM calendar_action_members
         WHERE event_id = ?1 AND owner_event_id = ?1",
        [&owner.id],
    )?;
    for member in &members {
        if member.event.id == owner.id {
            continue;
        }
        let identities = super::calendar_recurrence::get_by_event_id(conn, &member.event.id)?;
        let position = identities
            .iter()
            .find_map(|identity| {
                identity.recurrence_id.as_deref().map(|key| {
                    identity_position(
                        identity.recurrence_value_type == Some(RecurrenceValueType::Date),
                        key,
                        identity
                            .recurrence_timezone
                            .as_deref()
                            .or(member.event.timezone.as_deref()),
                    )
                })
            })
            .transpose()?;
        retire_objects(conn, &member.event.id)?;
        conn.execute("INSERT INTO calendar_action_members(event_id, owner_event_id, original_start) VALUES (?1, ?2, ?3)
            ON CONFLICT(event_id) DO UPDATE SET owner_event_id = excluded.owner_event_id, original_start = COALESCE(excluded.original_start, original_start)",
            params![member.event.id, owner.id, position])?;
        if let Some(remote) = &member.event.remote_id {
            address(conn, &member.event, remote, &owner.id, false)?;
        }
        conn.execute(
            "UPDATE calendar_events SET remote_id = NULL, etag = NULL WHERE id = ?1",
            [&member.event.id],
        )?;
    }
    persist_embedded(conn, &owner, set)?;
    if set.event.recurrence_kind == RecurrenceKind::Series
        || members.len() > 1
        || anchor.recurrence_kind == RecurrenceKind::Occurrence
    {
        save_owned_set(conn, &owner, set)?;
    }
    super::calendar::get_event(conn, &owner.id)
}

fn save_owned_set(conn: &Connection, event: &CalendarEvent, set: &CalendarEventSet) -> Result<()> {
    if let Some(native) = &set.native {
        address(conn, event, &native.event_id, &event.id, false)?;
        for resource in set.overrides.iter().filter_map(|item| item.native.as_ref()) {
            address(conn, event, &resource.event_id, &event.id, false)?;
        }
    }
    let mut cached = set.clone();
    cached.event.id = event.id.clone();
    cached.event.account_id = event.account_id.clone();
    cached.event.calendar_id = event.calendar_id.clone();
    conn.execute(
        "INSERT INTO calendar_action_sets(event_id, data, revision, dirty) VALUES (?1, ?2, ?3, 0)
        ON CONFLICT(event_id) DO UPDATE SET data = excluded.data, revision = excluded.revision, dirty = 0",
        params![event.id, encode(&cached)?, super::calendar_revision::get(conn, &event.id)?],
    )?;
    Ok(())
}

/// Retired source addresses survive deletion so a delayed sync page cannot
/// recreate a successfully transferred series.
pub(crate) fn retire_source(
    conn: &Connection,
    source: &Snapshot,
    target: &CalendarEvent,
) -> Result<()> {
    if let Some(native) = &source.set.native {
        address(conn, &source.anchor, &native.event_id, &target.id, true)?;
    }
    let members = set_members(conn, &source.anchor, &source.set)?;
    conn.execute(
        "UPDATE calendar_action_addresses SET retired = 1, owner_event_id = ?1
        WHERE account_id = ?2 AND calendar_id = ?3 AND owner_event_id IN
        (SELECT event_id FROM calendar_action_sets WHERE event_id = ?4)",
        params![
            target.id,
            source.anchor.account_id,
            source.anchor.calendar_id,
            owner_id(conn, &source.anchor.id)?
        ],
    )?;
    let retain_references = members.len() > 1;
    for member in members {
        if let Some(remote) = &member.event.remote_id {
            address(conn, &member.event, remote, &target.id, true)?;
        }
        retire_objects(conn, &member.event.id)?;
        if retain_references || member.event.recurrence_kind == RecurrenceKind::Occurrence {
            conn.execute(
                "DELETE FROM calendar_action_sets WHERE event_id = ?1",
                [&member.event.id],
            )?;
            conn.execute("UPDATE calendar_events SET account_id = ?1, calendar_id = ?2, remote_id = NULL, etag = NULL WHERE id = ?3", params![target.account_id, target.calendar_id, member.event.id])?;
            conn.execute(
                "INSERT INTO calendar_action_members(event_id, owner_event_id) VALUES (?1, ?2)
                ON CONFLICT(event_id) DO UPDATE SET owner_event_id = excluded.owner_event_id",
                params![member.event.id, target.id],
            )?;
        } else {
            super::calendar_event_deletion::delete_event(conn, &member.event.id)?;
        }
    }
    Ok(())
}

/// Route sync's expanded resources to their authoritative owner. A partial sync
/// response invalidates the finite read rather than replacing it with a child.
pub(crate) fn ingest_owned(
    conn: &Connection,
    event: &CalendarEvent,
    seeds: &[RecurrenceIdentitySeed],
) -> Result<Option<String>> {
    ingest_owned_identity(
        conn,
        &event.account_id,
        &event.calendar_id,
        event.remote_id.as_deref(),
        seeds,
    )
}

/// Route a provider identity before provider-specific cache reconciliation can
/// attach that identity to a second local row.
pub(crate) fn ingest_owned_identity(
    conn: &Connection,
    account_id: &str,
    calendar_id: &str,
    remote_id: Option<&str>,
    seeds: &[RecurrenceIdentitySeed],
) -> Result<Option<String>> {
    let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'calendar_action_addresses')", [], |row| row.get(0))?;
    if !exists {
        return Ok(None);
    }
    let claimed: Option<String> = conn.query_row(
        "SELECT e.id FROM calendar_action_claims claim JOIN calendar_events e ON e.id = claim.event_id
         WHERE e.account_id = ?1 AND e.calendar_id = ?2 AND e.remote_id = ?3",
        params![account_id, calendar_id, remote_id], |row| row.get(0),
    ).optional()?;
    if claimed.is_some() {
        return Ok(claimed);
    }
    let mut remotes = Vec::new();
    if let Some(remote) = remote_id {
        remotes.push(remote);
    }
    remotes.extend(
        seeds
            .iter()
            .filter_map(|seed| seed.provider_series_id.as_deref()),
    );
    for remote in remotes {
        let claimed_series: Option<String> = conn.query_row(
            "SELECT operation.event_id FROM calendar_action_operations operation
             WHERE operation.completed = 0 AND operation.account_id = ?1
                AND json_extract(operation.data, '$.source.anchor.calendar_id') = ?2
                AND json_extract(operation.data, '$.source.set.native.event_id') = ?3
                AND EXISTS (SELECT 1 FROM calendar_action_claims claim WHERE claim.operation_id = operation.operation_id)",
            params![account_id, calendar_id, remote], |row| row.get(0),
        ).optional()?;
        if claimed_series.is_some() {
            return Ok(claimed_series);
        }
        let ownership: Option<(String, bool)> = conn.query_row(
            "SELECT owner_event_id, retired FROM calendar_action_addresses WHERE account_id = ?1 AND calendar_id = ?2 AND remote_id = ?3",
            params![account_id, calendar_id, remote], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        if let Some((owner, retired)) = ownership {
            if !retired {
                conn.execute(
                    "UPDATE calendar_action_sets SET dirty = 1 WHERE event_id = ?1",
                    [&owner],
                )?;
                conn.execute(
                    "UPDATE calendar_events SET updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
                    [&owner],
                )?;
                conn.execute("DELETE FROM calendar_action_snapshots WHERE event_id = ?1 OR event_id IN (SELECT event_id FROM calendar_action_members WHERE owner_event_id = ?1)", [&owner])?;
                if let Some(remote) = remote_id {
                    address_identity(conn, account_id, calendar_id, remote, &owner, false)?;
                }
            }
            return Ok(Some(owner));
        }
    }
    Ok(None)
}

pub(crate) fn insert_operation(conn: &Connection, operation: &Operation) -> Result<()> {
    if conn.is_autocommit() {
        return Err(invalid("operation intent requires a transaction"));
    }
    ensure_current(conn, &operation.source)?;
    conn.execute(
        "INSERT INTO calendar_action_operations(operation_id, account_id, event_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![operation.id, operation.source.anchor.account_id, operation.source.anchor.id, encode(operation)?],
    )?;
    Ok(())
}

pub(crate) fn claim_operation(conn: &Connection, operation: &Operation) -> Result<()> {
    if conn.is_autocommit() {
        return Err(invalid("operation claim requires a transaction"));
    }
    for member in &operation.source.members {
        let owner: Option<String> = conn
            .query_row(
                "SELECT operation_id FROM calendar_action_claims WHERE event_id = ?1",
                [&member.event.id],
                |row| row.get(0),
            )
            .optional()?;
        if owner.as_deref().is_some_and(|owner| owner != operation.id) {
            return Err(invalid("another unfinished operation owns this series"));
        }
        conn.execute(
            "INSERT OR IGNORE INTO calendar_action_claims(event_id, operation_id) VALUES (?1, ?2)",
            params![member.event.id, operation.id],
        )?;
    }
    Ok(())
}

pub(crate) fn ensure_unclaimed(conn: &Connection, event_id: &str) -> Result<()> {
    let claimed: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM calendar_action_claims WHERE event_id = ?1)",
        [event_id],
        |row| row.get(0),
    )?;
    if claimed {
        return Err(invalid(
            "an unfinished calendar action owns this event; resume it before editing",
        ));
    }
    Ok(())
}

pub(crate) fn load_operation(conn: &Connection, id: &str) -> Result<Operation> {
    let data: String = conn.query_row(
        "SELECT data FROM calendar_action_operations WHERE operation_id = ?1",
        [id],
        |row| row.get(0),
    )?;
    decode(&data)
}

pub(crate) fn save_operation(conn: &Connection, operation: &Operation) -> Result<()> {
    let completed = operation.stage == CalendarActionStage::Completed;
    conn.execute(
        "UPDATE calendar_action_operations SET data = ?1, completed = ?2 WHERE operation_id = ?3",
        params![encode(operation)?, completed, operation.id],
    )?;
    if completed {
        conn.execute(
            "DELETE FROM calendar_action_claims WHERE operation_id = ?1",
            [&operation.id],
        )?;
    }
    Ok(())
}

pub(crate) fn pending_operations(conn: &Connection, account_id: &str) -> Result<Vec<Operation>> {
    let mut stmt = conn.prepare("SELECT data FROM calendar_action_operations WHERE account_id = ?1 AND completed = 0 ORDER BY created_at LIMIT 100")?;
    let rows = stmt
        .query_map([account_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.into_iter().map(|data| decode(&data)).collect()
}

pub(crate) fn creation_data(conn: &Connection, id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT data FROM calendar_action_creations WHERE operation_id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()?)
}

pub(crate) fn save_creation(
    conn: &Connection,
    id: &str,
    account_id: &str,
    event_id: &str,
    data: &str,
    completed: bool,
) -> Result<()> {
    let changed = conn.execute(
        "INSERT INTO calendar_action_creations(operation_id, account_id, event_id, data, completed)
         VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(operation_id) DO UPDATE SET data = excluded.data, completed = excluded.completed
         WHERE calendar_action_creations.account_id = excluded.account_id AND calendar_action_creations.event_id = excluded.event_id",
        params![id, account_id, event_id, data, completed],
    )?;
    if changed != 1 {
        return Err(invalid("creation identity belongs to another operation"));
    }
    Ok(())
}

pub(crate) fn pending_creations(conn: &Connection, account_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT data FROM calendar_action_creations WHERE account_id = ?1 AND completed = 0 LIMIT 100")?;
    let rows = stmt
        .query_map([account_id], |row| row.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Embedded local series are read from the same finite recurrence objects used
/// by existing sync. Never infer an empty exception set from a detached instance.
pub(crate) fn local_set(conn: &Connection, event: &CalendarEvent) -> Result<CalendarEventSet> {
    if let Some(set) = owned_set(conn, &event.id)?.filter(|set| set.native.is_none()) {
        return Ok(set);
    }
    let mut set = CalendarEventSet {
        event: event.clone(),
        overrides: Vec::new(),
        native: None,
        content: None,
    };
    for identity in super::calendar_recurrence::get_by_event_id(conn, &event.id)? {
        if identity.kind == RecurrenceObjectKind::Master {
            continue;
        }
        let key = identity
            .recurrence_id
            .ok_or_else(|| invalid("recurrence object has no original position"))?;
        let mut occurrence = event.clone();
        apply_event_fields(&mut occurrence, &identity.occurrence);
        occurrence.recurrence_rule = None;
        occurrence.recurrence_kind = RecurrenceKind::Occurrence;
        set.overrides.push(CalendarOverride {
            original_start: identity_position(
                event.all_day,
                &key,
                identity
                    .recurrence_timezone
                    .as_deref()
                    .or(event.timezone.as_deref()),
            )?,
            event: (identity.kind != RecurrenceObjectKind::Exclusion).then_some(occurrence),
            native: None,
        });
    }
    set.validate()?;
    Ok(set)
}

pub(crate) fn identity_position(
    all_day: bool,
    key: &str,
    timezone: Option<&str>,
) -> Result<String> {
    use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
    if all_day {
        return NaiveDate::parse_from_str(key, "%Y-%m-%d")
            .or_else(|_| NaiveDate::parse_from_str(key, "%Y%m%d"))
            .map(|date| date.to_string())
            .map_err(|_| invalid("invalid original all-day position"));
    }
    if let Ok(time) = DateTime::parse_from_rfc3339(key) {
        return Ok(time
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::AutoSi, true));
    }
    if let Ok(time) = NaiveDateTime::parse_from_str(key, "%Y%m%dT%H%M%SZ") {
        return Ok(time.and_utc().to_rfc3339_opts(SecondsFormat::AutoSi, true));
    }
    let local = NaiveDateTime::parse_from_str(key, "%Y%m%dT%H%M%S")
        .or_else(|_| NaiveDateTime::parse_from_str(key, "%Y-%m-%dT%H:%M:%S"))
        .map_err(|_| invalid("invalid original recurrence position"))?;
    let zone: chrono_tz::Tz = timezone
        .ok_or_else(|| invalid("floating recurrence identity has no timezone"))?
        .parse()
        .map_err(|_| invalid("unknown recurrence identity timezone"))?;
    let time = zone
        .from_local_datetime(&local)
        .earliest()
        .ok_or_else(|| invalid("original position falls in a timezone gap"))?;
    Ok(time
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::AutoSi, true))
}

/// Persist an embedded set without ever assigning an existing immutable object
/// ID a new address. Retire changed identities before inserting fresh UUIDs.
pub(crate) fn persist_embedded(
    conn: &Connection,
    anchor: &CalendarEvent,
    set: &CalendarEventSet,
) -> Result<()> {
    if conn.is_autocommit() {
        return Err(invalid("set persistence requires a transaction"));
    }
    set.validate()?;
    let mut event = set.event.clone();
    if let Some(master) = &set.native {
        if event.remote_id.as_deref() != Some(master.event_id.as_str())
            || set
                .overrides
                .iter()
                .filter_map(|item| item.native.as_ref())
                .any(|resource| {
                    resource.protocol != master.protocol
                        || resource.calendar_id != master.calendar_id
                })
        {
            return Err(invalid(
                "canonical resources do not share an exact provider calendar",
            ));
        }
    }
    event.id = anchor.id.clone();
    event.account_id = anchor.account_id.clone();
    event.calendar_id = anchor.calendar_id.clone();
    event.source_message_id = anchor.source_message_id.clone().or(event.source_message_id);
    super::calendar::update_event(conn, &event)?;
    if !set.overrides.is_empty() {
        super::calendar_invitation::invalidate(conn, &event.id)?;
    }
    let previous = super::calendar_recurrence::get_by_event_id(conn, &event.id)?;
    let mut seeds = Vec::new();
    if event.recurrence_kind == RecurrenceKind::Series {
        let seed =
            |key: Option<String>,
             fields,
             kind,
             native: Option<&crate::calendar::event_set::NativeCalendarResource>| {
                RecurrenceIdentitySeed {
                    local_series_event_id: set.native.is_none().then(|| event.id.clone()),
                    provider_calendar_id: set
                        .native
                        .as_ref()
                        .map(|resource| resource.calendar_id.clone()),
                    provider_series_id: set
                        .native
                        .as_ref()
                        .map(|resource| resource.event_id.clone()),
                    provider_occurrence_id: native
                        .filter(|resource| {
                            set.native
                                .as_ref()
                                .is_some_and(|master| master.event_id != resource.event_id)
                        })
                        .map(|resource| resource.event_id.clone()),
                    recurrence_value_type: key.as_ref().map(|_| {
                        if event.all_day {
                            RecurrenceValueType::Date
                        } else {
                            RecurrenceValueType::DateTime
                        }
                    }),
                    recurrence_timezone: key.as_ref().and(event.timezone.clone()),
                    recurrence_id: key,
                    occurrence: fields,
                    provider_native_data: native.map(|resource| resource.data.clone()),
                    provider_revision: native.and_then(|resource| resource.revision.clone()),
                    kind,
                }
            };
        seeds.push(seed(
            None,
            event_fields(&event),
            RecurrenceObjectKind::Master,
            set.native.as_ref(),
        ));
        for exception in &set.overrides {
            let fields = match &exception.event {
                Some(event) => event_fields(event),
                None => {
                    crate::calendar::simple_recurrence::resolve(&event, &exception.original_start)?
                }
            };
            seeds.push(seed(
                Some(exception.original_start.clone()),
                fields,
                if exception.event.is_some() {
                    RecurrenceObjectKind::Exception
                } else {
                    RecurrenceObjectKind::Exclusion
                },
                exception.native.as_ref(),
            ));
        }
    }
    let same_identity = |old: &crate::calendar::recurrence_identity::RecurrenceIdentity,
                         seed: &RecurrenceIdentitySeed| {
        old.local_series_event_id == seed.local_series_event_id
            && old.provider_calendar_id == seed.provider_calendar_id
            && old.provider_series_id == seed.provider_series_id
            && old.provider_occurrence_id == seed.provider_occurrence_id
            && old.recurrence_id == seed.recurrence_id
            && old.recurrence_timezone == seed.recurrence_timezone
            && old.recurrence_value_type == seed.recurrence_value_type
    };
    for old in &previous {
        if !seeds.iter().any(|seed| same_identity(old, seed)) {
            conn.execute("INSERT OR IGNORE INTO calendar_action_retired_objects(object_id, data) VALUES (?1, ?2)", params![old.object_id, encode(old)?])?;
            super::calendar_recurrence::delete(conn, &old.object_id)?;
        }
    }
    for seed in seeds {
        let object_id = previous
            .iter()
            .find(|old| same_identity(old, &seed))
            .map(|old| old.object_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        super::calendar_recurrence::upsert(
            conn,
            &seed.bind(&event.account_id, &event.id, &object_id)?,
        )?;
    }
    if set.native.is_none()
        || set
            .native
            .as_ref()
            .is_some_and(|native| matches!(native.protocol.as_str(), "google" | "graph"))
    {
        save_owned_set(conn, &event, set)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::actions::{CalendarEdit, CalendarSelection};
    use crate::calendar::recurrence_identity::RecurrenceMutationScope;

    fn fixture() -> (Connection, CalendarEventSet) {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::schema::initialize(&conn).unwrap();
        conn.execute_batch("INSERT INTO accounts(id, display_name, email, username) VALUES ('account', 'Account', 'a@example.test', 'a');
            INSERT INTO calendars(id, account_id, name) VALUES ('calendar', 'account', 'Calendar');").unwrap();
        let event = CalendarEvent {
            id: "event".into(),
            account_id: "account".into(),
            calendar_id: "calendar".into(),
            start_time: "2026-09-14".into(),
            end_time: "2026-09-15".into(),
            all_day: true,
            recurrence_rule: Some("FREQ=DAILY;COUNT=4".into()),
            recurrence_kind: RecurrenceKind::Series,
            ..crate::backend::testutil::event()
        };
        let set = CalendarEventSet {
            event,
            overrides: vec![CalendarOverride {
                original_start: "2026-09-15".into(),
                event: None,
                native: None,
            }],
            native: None,
            content: None,
        };
        let tx = conn.transaction().unwrap();
        crate::db::calendar::insert_event(&tx, &set.event).unwrap();
        persist_embedded(&tx, &set.event, &set).unwrap();
        tx.commit().unwrap();
        (conn, set)
    }

    fn snapshot(conn: &Connection, set: CalendarEventSet) -> Snapshot {
        Snapshot {
            calendar_revision: calendar_revision(conn, "calendar").unwrap(),
            token: uuid::Uuid::new_v4().to_string(),
            anchor: set.event.clone(),
            members: vec![event_version(conn, set.event.clone()).unwrap()],
            set,
            remote_calendar_id: None,
            account_route: account_route(conn, "account").unwrap(),
            invitation_source: invitation_source(conn, "event").unwrap(),
        }
    }

    fn operation(source: Snapshot) -> Operation {
        Operation {
            id: uuid::Uuid::new_v4().to_string(),
            desired: source.set.clone(),
            destination: None,
            input: CalendarActionInput {
                selection: CalendarSelection {
                    event_id: source.anchor.id.clone(),
                    token: source.token.clone(),
                    original_start: Some("2026-09-14".into()),
                },
                scope: RecurrenceMutationScope::EntireSeries,
                edit: CalendarEdit::default(),
                destination_calendar_id: None,
                reset_exceptions: false,
            },
            source,
            stage: CalendarActionStage::Planned,
            destination_event_id: "destination".into(),
            canonical: None,
            source_after: None,
            native_move: false,
            auto_resume: false,
        }
    }

    #[test]
    fn finite_set_roundtrips_and_changed_identity_is_retired() {
        let (mut conn, mut set) = fixture();
        assert_eq!(local_set(&conn, &set.event).unwrap(), set);
        let old = crate::db::calendar_recurrence::get_by_event_id(&conn, "event").unwrap();
        let excluded_id = old
            .iter()
            .find(|item| item.kind == RecurrenceObjectKind::Exclusion)
            .unwrap()
            .object_id
            .clone();
        let master_id = old
            .iter()
            .find(|item| item.kind == RecurrenceObjectKind::Master)
            .unwrap()
            .object_id
            .clone();
        set.overrides[0].original_start = "2026-09-16".into();
        let tx = conn.transaction().unwrap();
        persist_embedded(&tx, &set.event, &set).unwrap();
        tx.commit().unwrap();
        assert!(
            crate::db::calendar_recurrence::get_by_object_id(&conn, &excluded_id)
                .unwrap()
                .is_none()
        );
        assert!(
            crate::db::calendar_recurrence::get_by_object_id(&conn, &master_id)
                .unwrap()
                .is_some()
        );
        assert_eq!(local_set(&conn, &set.event).unwrap(), set);
    }

    #[test]
    fn stale_and_cross_event_selection_tokens_are_rejected() {
        let (conn, set) = fixture();
        let snapshot = snapshot(&conn, set);
        save_snapshot(&conn, &snapshot).unwrap();
        assert!(load_snapshot(&conn, &snapshot.token, "wrong-event").is_err());
        load_snapshot(&conn, &snapshot.token, "event").unwrap();
        conn.execute(
            "UPDATE calendar_events SET title = 'Concurrent edit' WHERE id = 'event'",
            [],
        )
        .unwrap();
        assert!(load_snapshot(&conn, &snapshot.token, "event").is_err());
    }

    #[test]
    fn mutation_then_sync_invalidates_snapshot_instead_of_competing_with_it() {
        let (mut conn, mut set) = fixture();
        set.event.remote_id = Some("remote".into());
        let tx = conn.transaction().unwrap();
        persist_embedded(&tx, &set.event, &set).unwrap();
        tx.commit().unwrap();
        let snapshot = snapshot(&conn, set.clone());
        save_snapshot(&conn, &snapshot).unwrap();
        crate::db::calendar::upsert_event_by_remote_id(&conn, &set.event).unwrap();
        assert!(load_snapshot(&conn, &snapshot.token, "event").is_err());
        assert_eq!(
            crate::db::calendar::get_event(&conn, "event")
                .unwrap()
                .title,
            set.event.title
        );
    }

    #[test]
    fn provider_native_reingestion_can_replace_canonical_positions_without_master_collision() {
        let (mut conn, mut set) = fixture();
        conn.execute(
            "UPDATE calendars SET remote_id = 'remote-calendar' WHERE id = 'calendar'",
            [],
        )
        .unwrap();
        set.event.remote_id = Some("remote".into());
        set.native = Some(crate::calendar::event_set::NativeCalendarResource {
            protocol: "caldav".into(),
            calendar_id: "remote-calendar".into(),
            event_id: "remote".into(),
            revision: Some("etag".into()),
            data: "private calendar resource".into(),
        });
        let tx = conn.transaction().unwrap();
        persist_embedded(&tx, &set.event, &set).unwrap();
        cache_canonical(&tx, set.event.clone(), &set).unwrap();
        tx.commit().unwrap();
        assert!(latest_snapshot(&conn, "event").unwrap().is_some());
        let mut seeds: Vec<_> = crate::db::calendar_recurrence::get_by_event_id(&conn, "event")
            .unwrap()
            .into_iter()
            .map(|identity| RecurrenceIdentitySeed {
                local_series_event_id: identity.local_series_event_id,
                provider_calendar_id: identity.provider_calendar_id,
                provider_series_id: identity.provider_series_id,
                provider_occurrence_id: identity.provider_occurrence_id,
                recurrence_id: identity.recurrence_id,
                recurrence_timezone: identity.recurrence_timezone,
                recurrence_value_type: identity.recurrence_value_type,
                occurrence: identity.occurrence,
                provider_native_data: identity.provider_native_data,
                provider_revision: identity.provider_revision,
                kind: identity.kind,
            })
            .collect();
        for seed in &mut seeds {
            assert!(seed.local_series_event_id.is_none());
            if seed.recurrence_id.is_some() {
                seed.recurrence_id = Some("20260915".into());
            }
        }
        crate::db::calendar::upsert_event_by_remote_id_with_recurrence(&conn, &set.event, &seeds)
            .unwrap();
        assert_eq!(
            crate::db::calendar_recurrence::get_by_event_id(&conn, "event")
                .unwrap()
                .len(),
            2
        );
        assert!(latest_snapshot(&conn, "event").unwrap().is_none());
        assert_eq!(
            local_set(&conn, &set.event).unwrap().overrides[0].original_start,
            "2026-09-15"
        );
    }

    #[test]
    fn journal_and_exclusive_claim_survive_reinitialization() {
        let (mut conn, set) = fixture();
        let first = operation(snapshot(&conn, set.clone()));
        let second = operation(snapshot(&conn, set));
        let tx = conn.transaction().unwrap();
        insert_operation(&tx, &first).unwrap();
        insert_operation(&tx, &second).unwrap();
        claim_operation(&tx, &first).unwrap();
        tx.commit().unwrap();
        crate::db::schema::initialize(&conn).unwrap();
        assert_eq!(
            load_operation(&conn, &first.id).unwrap().source.set,
            first.source.set
        );
        let tx = conn.transaction().unwrap();
        assert!(claim_operation(&tx, &second).is_err());
        tx.rollback().unwrap();
    }

    #[test]
    fn failed_set_persistence_rolls_back_fields_and_identity_retirement() {
        let (mut conn, set) = fixture();
        let initial_revision = crate::db::calendar_revision::get(&conn, "event").unwrap();
        let mut invalid_set = set.clone();
        invalid_set.event.title = "Should roll back".into();
        invalid_set.overrides[0].original_start = "2027-01-01".into();
        let tx = conn.transaction().unwrap();
        assert!(persist_embedded(&tx, &set.event, &invalid_set).is_err());
        tx.rollback().unwrap();
        assert_eq!(local_set(&conn, &set.event).unwrap(), set);
        assert_eq!(
            crate::db::calendar_revision::get(&conn, "event").unwrap(),
            initial_revision
        );
    }

    #[test]
    fn original_native_identity_formats_normalize_without_using_effective_start() {
        assert_eq!(
            identity_position(true, "20260914", None).unwrap(),
            "2026-09-14"
        );
        assert_eq!(
            identity_position(false, "20260914T090000", Some("Europe/Stockholm")).unwrap(),
            "2026-09-14T07:00:00Z"
        );
        assert_eq!(
            identity_position(false, "20260914T090000Z", None).unwrap(),
            "2026-09-14T09:00:00Z"
        );
        assert!(identity_position(false, "20260308T023000", Some("America/New_York")).is_err());
    }
}
