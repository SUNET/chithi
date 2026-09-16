//! Focused scoped calendar commands. Account locks span provider I/O; SQLite
//! transactions cover only snapshots, journal transitions, and local commits.

use std::collections::HashSet;

use tauri::State;

use crate::backend::calendar::{self as providers, CalendarBackendCtx, CalendarCapability};
use crate::calendar::actions::*;
use crate::calendar::event_set::{event_fields, CalendarEventSet, CalendarOverride};
use crate::calendar::recurrence_identity::{RecurrenceMutationScope, RecurrenceObjectKind};
use crate::calendar::{CalendarEvent, RecurrenceKind};
use crate::db::{self, calendar_actions as store};
use crate::error::Result;
use crate::state::AppState;

fn context(state: &AppState) -> CalendarBackendCtx<'_> {
    CalendarBackendCtx {
        db: &state.db,
        services: &state.providers,
    }
}

fn destination(conn: &rusqlite::Connection, id: &str) -> Result<store::Destination> {
    let calendar = db::calendar::get_calendar(conn, id)?;
    let account = db::accounts::get_account_full(conn, &calendar.account_id)?;
    if !calendar.is_subscribed || !account.enabled {
        return Err(invalid("calendar or account is unavailable"));
    }
    let remote = calendar.remote_id.filter(|id| !id.is_empty());
    let backend = providers::for_account(&account);
    if remote.is_some() && (backend.is_none() || !account.calendar_sync_enabled) {
        return Err(invalid("calendar provider is unavailable"));
    }
    if remote.is_none() && backend.is_some() {
        return Err(invalid(
            "local calendars on provider-bound accounts require deferred-sync isolation",
        ));
    }
    Ok(store::Destination {
        account_route: store::account_route(conn, &calendar.account_id)?,
        calendar_id: calendar.id,
        account_id: calendar.account_id,
        remote_calendar_id: remote,
        protocol: backend.map(|backend| backend.protocol().to_string()),
    })
}

fn ensure_destination(conn: &rusqlite::Connection, expected: &store::Destination) -> Result<()> {
    let current = destination(conn, &expected.calendar_id)?;
    if current.account_id != expected.account_id
        || current.remote_calendar_id != expected.remote_calendar_id
        || current.protocol != expected.protocol
        || current.account_route != expected.account_route
    {
        return Err(invalid("destination calendar changed; re-plan the action"));
    }
    Ok(())
}

async fn account_guards(
    state: &AppState,
    source: &str,
    target: Option<&str>,
) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
    let mut ids = vec![source];
    if let Some(target) = target {
        ids.push(target);
    }
    ids.sort_unstable();
    ids.dedup();
    let mut guards = Vec::new();
    for id in ids {
        guards.push(state.account_lifecycle.acquire(id).lock_owned().await);
    }
    guards
}

/// Hydrate a real provider master and all finite overrides. Addresses come from
/// the persisted anchor and its exact recurrence identity, never renderer input.
async fn hydrate_with_backends(
    state: &AppState,
    event_id: &str,
    backends: Option<&[&dyn providers::CalendarBackend]>,
) -> Result<store::Snapshot> {
    let (anchor, account, calendar, mut versions, revision) = {
        let conn = state.db.reader();
        let anchor = db::calendar::get_event(&conn, event_id)?;
        let account = db::accounts::get_account_full(&conn, &anchor.account_id)?;
        let calendar = db::calendar::get_calendar(&conn, &anchor.calendar_id)?;
        destination(&conn, &calendar.id)?;
        let identities = db::calendar_recurrence::get_by_event_id(&conn, event_id)?;
        let mut ids = HashSet::from([event_id.to_string()]);
        for identity in identities {
            if identity.provider_series_id.is_some() {
                for member in db::calendar_recurrence::list_series_objects(
                    &conn,
                    &anchor.account_id,
                    None,
                    identity.provider_calendar_id.as_deref(),
                    identity.provider_series_id.as_deref(),
                )? {
                    let cached = db::calendar::get_event(&conn, &member.event_id)?;
                    if cached.account_id == anchor.account_id
                        && cached.calendar_id == anchor.calendar_id
                    {
                        ids.insert(member.event_id);
                    }
                }
            }
        }
        if ids.len() > 10_000 {
            return Err(invalid("series cache exceeds member budget"));
        }
        let versions = ids
            .into_iter()
            .map(|id| store::event_version(&conn, db::calendar::get_event(&conn, &id)?))
            .collect::<Result<Vec<_>>>()?;
        let revision = store::calendar_revision(&conn, &anchor.calendar_id)?;
        (anchor, account, calendar, versions, revision)
    };
    let set = if let Some(remote_calendar_id) =
        calendar.remote_id.as_deref().filter(|id| !id.is_empty())
    {
        let provider_anchor = db::calendar::get_event(
            &state.db.reader(),
            &store::owner_id(&state.db.reader(), &anchor.id)?,
        )?;
        if provider_anchor
            .remote_id
            .as_deref()
            .is_none_or(str::is_empty)
        {
            return Err(invalid(
                "event has not been published; complete creation first",
            ));
        }
        let backend = backend_for(&account, backends)?;
        let set = backend
            .fetch_event_set(
                &context(state),
                &account,
                &provider_anchor,
                remote_calendar_id,
            )
            .await?;
        let native = set
            .native
            .as_ref()
            .ok_or_else(|| invalid("authoritative provider read omitted native identity"))?;
        if native.protocol != backend.protocol() || native.calendar_id != remote_calendar_id {
            return Err(invalid(
                "provider returned a resource outside the selected calendar",
            ));
        }
        set
    } else {
        if anchor.remote_id.as_deref().is_some_and(|id| !id.is_empty()) {
            return Err(invalid(
                "local event retains a provider identity; reconcile before editing",
            ));
        }
        let owner = db::calendar::get_event(
            &state.db.reader(),
            &store::owner_id(&state.db.reader(), &anchor.id)?,
        )?;
        store::local_set(&state.db.reader(), &owner)?
    };
    set.validate()?;
    if let Some(master) = &set.native {
        let conn = state.db.reader();
        let mut seen: HashSet<_> = versions
            .iter()
            .map(|version| version.event.id.clone())
            .collect();
        let mut resources = HashSet::new();
        for resource in std::iter::once(master)
            .chain(set.overrides.iter().filter_map(|item| item.native.as_ref()))
        {
            if resource.protocol != master.protocol || resource.calendar_id != master.calendar_id {
                return Err(invalid(
                    "provider returned an override outside the source calendar",
                ));
            }
            if !resources.insert(&resource.event_id) {
                continue;
            }
            let mut stmt = conn.prepare("SELECT id FROM calendar_events WHERE account_id = ?1 AND calendar_id = ?2 AND remote_id = ?3 LIMIT 10001")?;
            let ids = stmt
                .query_map(
                    rusqlite::params![anchor.account_id, anchor.calendar_id, resource.event_id],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for id in ids {
                if seen.insert(id.clone()) {
                    versions.push(store::event_version(
                        &conn,
                        db::calendar::get_event(&conn, &id)?,
                    )?);
                    if versions.len() > 10_000 {
                        return Err(invalid("series cache exceeds member budget"));
                    }
                }
            }
        }
    }
    for member in store::set_members(&state.db.reader(), &anchor, &set)? {
        if !versions
            .iter()
            .any(|version| version.event.id == member.event.id)
        {
            versions.push(member);
        }
    }
    let snapshot = store::Snapshot {
        calendar_revision: revision,
        invitation_source: store::invitation_source(&state.db.reader(), &anchor.id)?,
        account_route: store::account_route(&state.db.reader(), &anchor.account_id)?,
        token: uuid::Uuid::new_v4().to_string(),
        anchor,
        members: versions,
        set,
        remote_calendar_id: calendar.remote_id,
    };
    store::ensure_current(&state.db.reader(), &snapshot)?;
    Ok(snapshot)
}

#[tauri::command]
pub async fn read_calendar_event_set(
    state: State<'_, AppState>,
    event_id: String,
    start: String,
    end: String,
    limit: usize,
) -> Result<CalendarEventSetView> {
    read_event_set(&state, &event_id, &start, &end, limit, None).await
}

async fn read_event_set(
    state: &AppState,
    event_id: &str,
    start: &str,
    end: &str,
    limit: usize,
    backends: Option<&[&dyn providers::CalendarBackend]>,
) -> Result<CalendarEventSetView> {
    window(start, end, limit)?;
    let account_id = db::calendar::get_event(&state.db.reader(), event_id)?.account_id;
    let _guards = account_guards(state, &account_id, None).await;
    let mut snapshot = hydrate_with_backends(state, event_id, backends).await?;
    let mut conn = state.db.writer().await;
    let tx = conn.transaction()?;
    store::ensure_current(&tx, &snapshot)?;
    let owner = store::persist_source(&tx, &snapshot, &snapshot.set)?;
    store::cache_canonical(&tx, owner, &snapshot.set)?;
    snapshot.anchor = db::calendar::get_event(&tx, &snapshot.anchor.id)?;
    snapshot.members = store::set_members(&tx, &snapshot.anchor, &snapshot.set)?;
    snapshot.calendar_revision = store::calendar_revision(&tx, &snapshot.anchor.calendar_id)?;
    let view = event_set_view(
        &snapshot.set,
        &snapshot.anchor,
        &snapshot.token,
        start,
        end,
        limit,
    )?;
    store::save_snapshot(&tx, &snapshot)?;
    tx.commit()?;
    Ok(view)
}

/// Bounded database-only projection. Detached provider instances stay anchored
/// to their existing event IDs; planning hydrates their real master separately.
#[tauri::command]
pub async fn list_calendar_occurrences(
    state: State<'_, AppState>,
    account_id: String,
    calendar_id: Option<String>,
    start: String,
    end: String,
    limit: usize,
) -> Result<CalendarOccurrencePage> {
    list_occurrences(&state, account_id, calendar_id, start, end, limit).await
}

async fn list_occurrences(
    state: &AppState,
    account_id: String,
    calendar_id: Option<String>,
    start: String,
    end: String,
    limit: usize,
) -> Result<CalendarOccurrencePage> {
    window(&start, &end, limit)?;
    let _guards = account_guards(state, &account_id, None).await;
    let mut conn = state.db.writer().await;
    let tx = conn.transaction()?;
    if let Some(id) = &calendar_id {
        if db::calendar::get_calendar(&tx, id)?.account_id != account_id {
            return Err(invalid("calendar belongs to another account"));
        }
    }
    let ids = {
        let mut stmt = tx.prepare(
            "SELECT event.id FROM calendar_events event JOIN calendars calendar ON calendar.id = event.calendar_id
             WHERE event.account_id = ?1 AND (?2 IS NULL OR event.calendar_id = ?2) AND calendar.is_subscribed = 1
               AND NOT EXISTS (SELECT 1 FROM calendar_action_members member WHERE member.event_id = event.id)
               AND ((julianday(event.start_time) < julianday(?4) AND julianday(event.end_time) > julianday(?3))
                 OR event.recurrence_kind = 'series')
             ORDER BY event.start_time, event.id LIMIT 2001")?;
        let rows = stmt.query_map(
            rusqlite::params![account_id, calendar_id, start, end],
            |row| row.get::<_, String>(0),
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut page = CalendarOccurrencePage {
        occurrences: Vec::new(),
        has_more: ids.len() > 2000,
        needs_hydration: Vec::new(),
    };
    let mut projected_members = HashSet::new();
    for id in ids.into_iter().take(2000) {
        if projected_members.contains(&id) {
            continue;
        }
        let anchor = db::calendar::get_event(&tx, &id)?;
        let calendar = db::calendar::get_calendar(&tx, &anchor.calendar_id)?;
        if matches!(
            anchor.recurrence_kind,
            RecurrenceKind::Series | RecurrenceKind::Unknown
        ) && calendar
            .remote_id
            .as_deref()
            .is_some_and(|id| !id.is_empty())
        {
            if let Some(snapshot) = store::latest_snapshot(&tx, &id)? {
                let members: HashSet<_> = snapshot
                    .members
                    .iter()
                    .map(|member| member.event.id.as_str())
                    .collect();
                page.occurrences
                    .retain(|row| !members.contains(row.event_id.as_str()));
                page.needs_hydration
                    .retain(|id| !members.contains(id.as_str()));
                projected_members.extend(members.into_iter().map(str::to_owned));
                let projected =
                    project(&snapshot.set, &anchor, &snapshot.token, &start, &end, limit)?;
                store::save_snapshot(&tx, &snapshot)?;
                page.has_more |= projected.has_more;
                page.occurrences.extend(projected.occurrences);
                page.has_more |= page.occurrences.len() > limit;
                page.occurrences.sort_by(|a, b| {
                    a.fields
                        .start_time
                        .cmp(&b.fields.start_time)
                        .then(a.event_id.cmp(&b.event_id))
                });
                page.occurrences.truncate(limit);
            } else {
                page.needs_hydration.push(id);
            }
            continue;
        }
        let token = uuid::Uuid::new_v4().to_string();
        let (set, original) = if anchor.recurrence_kind == RecurrenceKind::Occurrence {
            let identities = db::calendar_recurrence::get_by_event_id(&tx, &id)?;
            let identity = identities
                .iter()
                .find(|identity| {
                    matches!(
                        identity.kind,
                        RecurrenceObjectKind::Occurrence | RecurrenceObjectKind::Exception
                    )
                })
                .ok_or_else(|| invalid("detached instance lacks a trusted original position"))?;
            let original = store::identity_position(
                identity.recurrence_value_type
                    == Some(crate::calendar::recurrence_identity::RecurrenceValueType::Date),
                identity
                    .recurrence_id
                    .as_deref()
                    .ok_or_else(|| invalid("detached instance lacks its original position"))?,
                identity
                    .recurrence_timezone
                    .as_deref()
                    .or(anchor.timezone.as_deref()),
            )?;
            let mut event = anchor.clone();
            event.recurrence_kind = RecurrenceKind::Standalone;
            event.recurrence_rule = None;
            (
                CalendarEventSet {
                    event,
                    overrides: vec![],
                    native: None,
                    content: None,
                },
                Some(original),
            )
        } else {
            (store::local_set(&tx, &anchor)?, None)
        };
        let mut projected = project(&set, &anchor, &token, &start, &end, limit)?;
        for occurrence in &mut projected.occurrences {
            if let Some(original) = &original {
                occurrence.selection.original_start = Some(original.clone());
                occurrence.recurrence_kind = RecurrenceKind::Occurrence;
            }
        }
        if !projected.occurrences.is_empty() {
            let snapshot = store::Snapshot {
                calendar_revision: store::calendar_revision(&tx, &anchor.calendar_id)?,
                invitation_source: store::invitation_source(&tx, &anchor.id)?,
                account_route: store::account_route(&tx, &anchor.account_id)?,
                token,
                anchor: anchor.clone(),
                members: vec![store::event_version(&tx, anchor)?],
                set,
                remote_calendar_id: calendar.remote_id,
            };
            store::save_snapshot(&tx, &snapshot)?;
        }
        page.has_more |= projected.has_more;
        page.occurrences.extend(projected.occurrences);
        if page.occurrences.len() > limit {
            page.has_more = true;
        }
        page.occurrences.sort_by(|a, b| {
            a.fields
                .start_time
                .cmp(&b.fields.start_time)
                .then(a.event_id.cmp(&b.event_id))
        });
        page.occurrences.truncate(limit);
    }
    page.occurrences.sort_by(|a, b| {
        a.fields
            .start_time
            .cmp(&b.fields.start_time)
            .then(a.event_id.cmp(&b.event_id))
    });
    page.occurrences.truncate(limit);
    tx.commit()?;
    Ok(page)
}

#[tauri::command]
pub async fn plan_calendar_action(
    state: State<'_, AppState>,
    input: CalendarActionInput,
) -> Result<CalendarActionPlan> {
    plan_action(&state, input).await
}

async fn plan_action(state: &AppState, input: CalendarActionInput) -> Result<CalendarActionPlan> {
    plan_with_backends(state, input, None).await
}

async fn plan_with_backends(
    state: &AppState,
    input: CalendarActionInput,
    backends: Option<&[&dyn providers::CalendarBackend]>,
) -> Result<CalendarActionPlan> {
    let (initial, target) = {
        let conn = state.db.reader();
        (
            store::load_snapshot(&conn, &input.selection.token, &input.selection.event_id)?,
            input
                .destination_calendar_id
                .as_deref()
                .map(|id| destination(&conn, id))
                .transpose()?,
        )
    };
    let _guards = account_guards(
        state,
        &initial.anchor.account_id,
        target.as_ref().map(|target| target.account_id.as_str()),
    )
    .await;
    store::ensure_current(&state.db.reader(), &initial)?;
    let source = hydrate_with_backends(state, &initial.anchor.id, backends).await?;
    let desired = desired_set(&source.set, &input)?;
    let target = target.filter(|target| target.calendar_id != source.anchor.calendar_id);
    let requirements = CalendarConfirmations {
        replacement_meeting_identity: target.is_some()
            && (source.set.event.organizer_email.is_some()
                || source.set.event.attendees_json.is_some()),
        reset_exceptions: input.reset_exceptions && !source.set.overrides.is_empty(),
    };
    let preview = event_fields(&selected_event(
        &desired,
        mapped_selection(&source.set, &desired, &input)?.as_deref(),
    )?);
    let operation = store::Operation {
        id: uuid::Uuid::new_v4().to_string(),
        destination_event_id: uuid::Uuid::new_v4().to_string(),
        source,
        input,
        desired,
        destination: target,
        stage: CalendarActionStage::Planned,
        canonical: None,
        source_after: None,
        native_move: false,
    };
    let mut conn = state.db.writer().await;
    let tx = conn.transaction()?;
    if let Some(target) = &operation.destination {
        ensure_destination(&tx, target)?;
    }
    store::insert_operation(&tx, &operation)?;
    tx.commit()?;
    Ok(CalendarActionPlan {
        operation_id: operation.id,
        requires: requirements,
        preview,
    })
}

fn mapped_selection(
    before: &CalendarEventSet,
    desired: &CalendarEventSet,
    input: &CalendarActionInput,
) -> Result<Option<String>> {
    if desired.event.recurrence_kind != RecurrenceKind::Series {
        return Ok(None);
    }
    if input.scope == RecurrenceMutationScope::ThisOccurrence {
        return Ok(input.selection.original_start.clone());
    }
    input
        .selection
        .original_start
        .as_deref()
        .map(|key| {
            crate::calendar::simple_recurrence::position_at(
                &desired.event,
                crate::calendar::simple_recurrence::occurrence_index(&before.event, key)?,
            )
        })
        .transpose()
}

fn result(operation: &store::Operation) -> CalendarActionResult {
    CalendarActionResult {
        operation_id: operation.id.clone(),
        stage: operation.stage,
        event_id: if operation.destination.is_some() {
            operation.destination_event_id.clone()
        } else {
            operation.source.anchor.id.clone()
        },
        requires: CalendarConfirmations {
            replacement_meeting_identity: operation.stage != CalendarActionStage::Completed
                && operation.destination.is_some()
                && (operation.source.set.event.organizer_email.is_some()
                    || operation.source.set.event.attendees_json.is_some()),
            reset_exceptions: operation.stage != CalendarActionStage::Completed
                && operation.input.reset_exceptions
                && !operation.source.set.overrides.is_empty(),
        },
    }
}

#[tauri::command]
pub async fn get_calendar_action(
    state: State<'_, AppState>,
    operation_id: String,
) -> Result<CalendarActionResult> {
    if let Some(data) = store::creation_data(&state.db.reader(), &operation_id)? {
        let creation: Creation = store::decode(&data)?;
        return Ok(creation.result());
    }
    Ok(result(&store::load_operation(
        &state.db.reader(),
        &operation_id,
    )?))
}

#[tauri::command]
pub async fn list_pending_calendar_actions(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Vec<CalendarActionResult>> {
    let conn = state.db.reader();
    let mut results: Vec<_> = store::pending_operations(&conn, &account_id)?
        .iter()
        .map(result)
        .collect();
    for data in store::pending_creations(&conn, &account_id)? {
        results.push(store::decode::<Creation>(&data)?.result());
    }
    Ok(results)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Creation {
    id: String,
    request: String,
    desired: CalendarEventSet,
    destination: store::Destination,
    canonical: Option<CalendarEventSet>,
    completed: bool,
}

impl Creation {
    fn result(&self) -> CalendarActionResult {
        CalendarActionResult {
            operation_id: self.id.clone(),
            event_id: self.desired.event.id.clone(),
            stage: if self.completed {
                CalendarActionStage::Completed
            } else if self.canonical.is_some() {
                CalendarActionStage::Reconciling
            } else {
                CalendarActionStage::Applying
            },
            requires: CalendarConfirmations::default(),
        }
    }

    fn save(&self, conn: &rusqlite::Connection) -> Result<()> {
        store::save_creation(
            conn,
            &self.id,
            &self.destination.account_id,
            &self.desired.event.id,
            &store::encode(self)?,
            self.completed,
        )
    }
}

/// The caller persists a UUID before invoking creation and reuses it after an
/// uncertain response. No calendar_events row exists until publication succeeds,
/// so deferred provider sync cannot independently push the same event.
#[tauri::command]
pub async fn create_calendar_event(
    state: State<'_, AppState>,
    event: super::calendar::NewEventInput,
    operation_id: String,
) -> Result<CalendarActionResult> {
    create_completed(&state, event, &operation_id).await
}

pub(crate) async fn create_completed(
    state: &AppState,
    input: super::calendar::NewEventInput,
    operation_id: &str,
) -> Result<CalendarActionResult> {
    create_with_backends(state, input, operation_id, None).await
}

async fn create_with_backends(
    state: &AppState,
    input: super::calendar::NewEventInput,
    operation_id: &str,
    backends: Option<&[&dyn providers::CalendarBackend]>,
) -> Result<CalendarActionResult> {
    uuid::Uuid::parse_str(operation_id)
        .map_err(|_| invalid("creation operation ID must be a UUID"))?;
    let request = store::encode(&input)?;
    let lifecycle = input
        .meet_binding
        .as_ref()
        .map(|binding| state.meet_lifecycle.acquire(&binding.lifecycle_id))
        .transpose()?;
    let _meeting_guard = match lifecycle {
        Some(lock) => Some(lock.lock_owned().await),
        None => None,
    };
    let _guards = account_guards(
        state,
        &input.account_id,
        input
            .meet_binding
            .as_ref()
            .map(|binding| binding.account_id.as_str()),
    )
    .await;
    let target = destination(&state.db.reader(), &input.calendar_id)?;
    if target.account_id != input.account_id {
        return Err(invalid("calendar belongs to another account"));
    }
    let account = db::accounts::get_account_full(&state.db.reader(), &input.account_id)?;
    let existing = store::creation_data(&state.db.reader(), operation_id)?;
    let mut creation = if let Some(data) = existing {
        let creation: Creation = store::decode(&data)?;
        if creation.request != request {
            return Err(invalid(
                "creation operation ID was already used for different input",
            ));
        }
        if creation.completed {
            return Ok(creation.result());
        }
        ensure_destination(&state.db.reader(), &creation.destination)?;
        creation
    } else {
        if input.meet_binding.is_some() && target.remote_calendar_id.is_some() {
            return Err(invalid("remote recurring creation with a pending meeting requires durable meeting reservation integration"));
        }
        if let Some(binding) = &input.meet_binding {
            super::calendar::validate_pending_meet_binding(&state.db.reader(), binding)?;
        }
        let mut event = CalendarEvent {
            id: uuid::Uuid::new_v4().to_string(),
            account_id: input.account_id.clone(),
            calendar_id: input.calendar_id.clone(),
            uid: Some(format!("{operation_id}@chithi")),
            title: input.title,
            description: input.description,
            location: input.location,
            start_time: input.start_time,
            end_time: input.end_time,
            all_day: input.all_day,
            timezone: input.timezone,
            recurrence_kind: RecurrenceKind::from_rule(input.recurrence_rule.as_deref()),
            recurrence_rule: input.recurrence_rule.filter(|rule| !rule.is_empty()),
            organizer_email: Some(account.email.clone()),
            attendees_json: if input.attendees.is_empty() {
                None
            } else {
                Some(store::encode(&input.attendees)?)
            },
            my_status: None,
            source_message_id: None,
            ical_data: None,
            remote_id: None,
            etag: None,
        };
        if let Some(rule) = event.recurrence_rule.as_deref() {
            event.recurrence_rule = Some(crate::calendar::simple_recurrence::normalize_rule(
                rule, &event,
            )?);
        }
        let desired = CalendarEventSet {
            event,
            overrides: vec![],
            native: None,
            content: None,
        };
        desired.validate()?;
        let creation = Creation {
            id: operation_id.to_string(),
            request,
            desired,
            destination: target,
            canonical: None,
            completed: false,
        };
        let conn = state.db.writer().await;
        creation.save(&conn)?;
        creation
    };
    if creation.canonical.is_none() {
        creation.canonical = Some(
            if let Some(remote) = &creation.destination.remote_calendar_id {
                let backend = backend_for(&account, backends)?;
                let canonical = backend
                    .create_event_set(
                        &context(state),
                        &account,
                        remote,
                        &creation.desired,
                        operation_id,
                    )
                    .await?;
                verify_created(&canonical, &creation.desired, remote, backend.protocol())?;
                canonical
            } else {
                creation.desired.clone()
            },
        );
        let conn = state.db.writer().await;
        creation.save(&conn)?;
    }
    let mut conn = state.db.writer().await;
    let tx = conn.transaction()?;
    ensure_destination(&tx, &creation.destination)?;
    let canonical = creation
        .canonical
        .as_ref()
        .ok_or_else(|| invalid("creation has no verified result"))?;
    let mut event = canonical.event.clone();
    event.id = creation.desired.event.id.clone();
    event.account_id = creation.destination.account_id.clone();
    event.calendar_id = creation.destination.calendar_id.clone();
    db::calendar::insert_event(&tx, &creation.desired.event)?;
    if creation.desired.event.recurrence_kind == RecurrenceKind::Series {
        db::calendar_invitation::record_local_series(&tx, &creation.desired.event)?;
    }
    store::persist_embedded(&tx, &event, canonical)?;
    if let Some(binding) = input.meet_binding.as_ref() {
        super::calendar::claim_meet_binding(&tx, &event.id, binding)?;
    }
    store::cache_canonical(&tx, db::calendar::get_event(&tx, &event.id)?, canonical)?;
    creation.completed = true;
    creation.save(&tx)?;
    tx.commit()?;
    Ok(creation.result())
}

fn verify_created(
    canonical: &CalendarEventSet,
    desired: &CalendarEventSet,
    remote: &str,
    protocol: &str,
) -> Result<()> {
    canonical.validate()?;
    let native = canonical
        .native
        .as_ref()
        .ok_or_else(|| invalid("creation returned no native identity"))?;
    if native.protocol != protocol
        || native.calendar_id != remote
        || native.event_id.is_empty()
        || canonical.event.remote_id.as_deref() != Some(native.event_id.as_str())
        || canonical
            .overrides
            .iter()
            .filter_map(|item| item.native.as_ref())
            .any(|resource| {
                resource.protocol != protocol
                    || resource.calendar_id != remote
                    || resource.event_id.is_empty()
            })
        || !semantic_eq(canonical, desired)
    {
        return Err(invalid(
            "destination has not been verified as the complete requested event set",
        ));
    }
    Ok(())
}

#[tauri::command]
pub async fn execute_calendar_action(
    state: State<'_, AppState>,
    operation_id: String,
    confirmations: CalendarConfirmations,
) -> Result<CalendarActionResult> {
    let creation = store::creation_data(&state.db.reader(), &operation_id)?;
    if let Some(data) = creation {
        let creation: Creation = store::decode(&data)?;
        return create_completed(&state, store::decode(&creation.request)?, &operation_id).await;
    }
    execute(&state, &operation_id, &confirmations).await
}

async fn checkpoint(state: &AppState, operation: &store::Operation) -> Result<()> {
    let conn = state.db.writer().await;
    store::save_operation(&conn, operation)
}

async fn execute(
    state: &AppState,
    id: &str,
    confirmations: &CalendarConfirmations,
) -> Result<CalendarActionResult> {
    execute_with_backends(state, id, confirmations, None).await
}

fn backend_for<'a>(
    account: &db::accounts::AccountFull,
    overrides: Option<&[&'a dyn providers::CalendarBackend]>,
) -> Result<&'a dyn providers::CalendarBackend> {
    match overrides {
        Some(backends) => backends
            .iter()
            .copied()
            .find(|backend| backend.protocol() == account.calendar_protocol_str()),
        None => providers::for_account(account),
    }
    .ok_or_else(|| invalid("calendar provider unavailable"))
}

async fn execute_with_backends(
    state: &AppState,
    id: &str,
    confirmations: &CalendarConfirmations,
    backends: Option<&[&dyn providers::CalendarBackend]>,
) -> Result<CalendarActionResult> {
    let initial = store::load_operation(&state.db.reader(), id)?;
    let _guards = account_guards(
        state,
        &initial.source.anchor.account_id,
        initial
            .destination
            .as_ref()
            .map(|target| target.account_id.as_str()),
    )
    .await;
    let mut operation = store::load_operation(&state.db.reader(), id)?;
    if operation.stage == CalendarActionStage::Completed {
        return Ok(result(&operation));
    }
    if operation.input.reset_exceptions
        && !operation.source.set.overrides.is_empty()
        && !confirmations.reset_exceptions
    {
        return Err(invalid("exception reset confirmation is required"));
    }
    if operation.destination.is_some()
        && (operation.source.set.event.organizer_email.is_some()
            || operation.source.set.event.attendees_json.is_some())
        && !confirmations.replacement_meeting_identity
    {
        return Err(invalid(
            "replacement meeting identity confirmation is required",
        ));
    }
    {
        let mut conn = state.db.writer().await;
        let tx = conn.transaction()?;
        store::ensure_operation_current(&tx, &operation)?;
        if let Some(target) = &operation.destination {
            ensure_destination(&tx, target)?;
        }
        if operation.stage != CalendarActionStage::Planned {
            store::claim_operation(&tx, &operation)?;
        }
        tx.commit()?;
    }
    let account =
        db::accounts::get_account_full(&state.db.reader(), &operation.source.anchor.account_id)?;
    let source_backend = operation
        .source
        .remote_calendar_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|_| backend_for(&account, backends))
        .transpose()?;
    let was_planned = operation.stage == CalendarActionStage::Planned;
    if operation.native_move && operation.canonical.is_none() {
        if let (Some(backend), Some(remote)) = (
            source_backend,
            operation
                .destination
                .as_ref()
                .and_then(|target| target.remote_calendar_id.as_deref()),
        ) {
            if let Ok(current) = backend
                .fetch_event_set(
                    &context(state),
                    &account,
                    &operation.source.set.event,
                    remote,
                )
                .await
            {
                if current.native.as_ref().is_some_and(|native| {
                    native.calendar_id == remote && native.protocol == backend.protocol()
                }) && semantic_eq(&current, &operation.desired)
                {
                    operation.canonical = Some(current);
                    checkpoint(state, &operation).await?;
                }
            }
        }
        if operation.canonical.is_none() {
            operation.stage = CalendarActionStage::Reconciling;
            checkpoint(state, &operation).await?;
            return Ok(result(&operation));
        }
    }
    if operation.canonical.is_none() {
        if let Some(backend) = source_backend {
            let current = backend
                .fetch_event_set(
                    &context(state),
                    &account,
                    &operation.source.set.event,
                    operation
                        .source
                        .remote_calendar_id
                        .as_deref()
                        .ok_or_else(|| invalid("missing source calendar"))?,
                )
                .await?;
            if operation.destination.is_none()
                && !was_planned
                && semantic_eq(&current, &operation.desired)
            {
                operation.canonical = Some(current);
            } else if current != operation.source.set {
                return Err(invalid(
                    "provider series changed; refresh and re-plan before retrying",
                ));
            }
        }
    }
    if operation.canonical.is_none() {
        operation.stage = CalendarActionStage::Applying;
        {
            let mut conn = state.db.writer().await;
            let tx = conn.transaction()?;
            store::ensure_operation_current(&tx, &operation)?;
            store::claim_operation(&tx, &operation)?;
            store::save_operation(&tx, &operation)?;
            tx.commit()?;
        }
        if let Some(target) = operation.destination.clone() {
            let target_account =
                db::accounts::get_account_full(&state.db.reader(), &target.account_id)?;
            let desired = transfer_set(&operation, &state.db.reader())?;
            let native_eligible = operation.input.scope == RecurrenceMutationScope::EntireSeries
                && target.account_id == operation.source.anchor.account_id
                && semantic_eq(&operation.source.set, &operation.desired);
            if native_eligible && was_planned {
                if let (Some(backend), Some(remote)) =
                    (source_backend, target.remote_calendar_id.as_deref())
                {
                    // Checkpoint identifies a possibly committed native move on restart.
                    operation.native_move = true;
                    checkpoint(state, &operation).await?;
                    match backend
                        .move_event_set_native(
                            &context(state),
                            &account,
                            &operation.source.set,
                            remote,
                        )
                        .await?
                    {
                        CalendarCapability::Supported(set) => {
                            verify_created(&set, &operation.desired, remote, backend.protocol())?;
                            operation.canonical = Some(set);
                        }
                        CalendarCapability::Unsupported => {
                            operation.native_move = false;
                            checkpoint(state, &operation).await?;
                        }
                    }
                }
            }
            if operation.native_move && operation.canonical.is_none() {
                operation.stage = CalendarActionStage::Reconciling;
                checkpoint(state, &operation).await?;
                return Ok(result(&operation));
            }
            if operation.canonical.is_none() {
                operation.canonical = Some(if let Some(remote) = &target.remote_calendar_id {
                    let backend = backend_for(&target_account, backends)?;
                    let canonical = backend
                        .create_event_set(
                            &context(state),
                            &target_account,
                            remote,
                            &desired,
                            &operation.id,
                        )
                        .await?;
                    verify_created(&canonical, &desired, remote, backend.protocol())?;
                    canonical
                } else {
                    desired
                });
            }
            operation
                .canonical
                .as_ref()
                .ok_or_else(|| invalid("destination result missing"))?
                .validate()?;
            operation.stage = CalendarActionStage::DestinationVerified;
            checkpoint(state, &operation).await?;
        } else {
            operation.canonical = Some(match source_backend {
                Some(backend) => {
                    backend
                        .update_event_set(
                            &context(state),
                            &account,
                            &operation.source.set,
                            &operation.desired,
                        )
                        .await?
                }
                None => operation.desired.clone(),
            });
            operation
                .canonical
                .as_ref()
                .ok_or_else(|| invalid("update result missing"))?
                .validate()?;
            checkpoint(state, &operation).await?;
        }
    }
    if operation.destination.is_some() && !operation.native_move {
        let target = operation
            .destination
            .as_ref()
            .ok_or_else(|| invalid("missing destination"))?;
        if let Some(remote) = target.remote_calendar_id.as_deref() {
            let target_account =
                db::accounts::get_account_full(&state.db.reader(), &target.account_id)?;
            let backend = backend_for(&target_account, backends)?;
            let canonical = operation
                .canonical
                .as_ref()
                .ok_or_else(|| invalid("destination has no verified resource"))?;
            let current = backend
                .fetch_event_set(&context(state), &target_account, &canonical.event, remote)
                .await?;
            if current != *canonical {
                return Err(invalid(
                    "verified destination changed before source removal; source retained",
                ));
            }
        }
        operation.stage = CalendarActionStage::SourceRemovalPending;
        checkpoint(state, &operation).await?;
        if operation.input.scope == RecurrenceMutationScope::ThisOccurrence
            && operation.source.set.event.recurrence_kind == RecurrenceKind::Series
        {
            if operation.source_after.is_none() {
                let mut remaining = operation.source.set.clone();
                let key = operation
                    .input
                    .selection
                    .original_start
                    .clone()
                    .ok_or_else(|| invalid("missing source position"))?;
                remaining
                    .overrides
                    .retain(|item| item.original_start != key);
                remaining.overrides.push(CalendarOverride {
                    original_start: key,
                    event: None,
                    native: None,
                });
                operation.source_after = Some(match source_backend {
                    Some(backend) => {
                        let current = backend
                            .fetch_event_set(
                                &context(state),
                                &account,
                                &operation.source.set.event,
                                operation
                                    .source
                                    .remote_calendar_id
                                    .as_deref()
                                    .ok_or_else(|| invalid("source calendar missing"))?,
                            )
                            .await?;
                        if semantic_eq(&current, &remaining) {
                            current
                        } else {
                            if current != operation.source.set {
                                return Err(invalid("source changed before occurrence exclusion"));
                            }
                            backend
                                .update_event_set(
                                    &context(state),
                                    &account,
                                    &operation.source.set,
                                    &remaining,
                                )
                                .await?
                        }
                    }
                    None => remaining,
                });
                checkpoint(state, &operation).await?;
            }
        } else if operation.source_after.is_none() {
            if let Some(backend) = source_backend {
                backend
                    .delete_event_set(&context(state), &account, &operation.source.set)
                    .await?;
            }
            // Durable marker: source removal was acknowledged. A lost DELETE
            // response remains pending; adapters must conditionally reconcile it.
            operation.source_after = Some(operation.source.set.clone());
            checkpoint(state, &operation).await?;
        }
    }
    let commit = commit_operation(state, &mut operation).await;
    if let Err(error) = commit {
        operation.stage = CalendarActionStage::Reconciling;
        checkpoint(state, &operation).await?;
        return Err(crate::error::Error::Sync(format!(
            "Calendar action {} needs local reconciliation: {error}",
            operation.id
        )));
    }
    Ok(result(&operation))
}

fn transfer_set(
    operation: &store::Operation,
    conn: &rusqlite::Connection,
) -> Result<CalendarEventSet> {
    let target = operation
        .destination
        .as_ref()
        .ok_or_else(|| invalid("missing destination"))?;
    let mut desired = if operation.input.scope == RecurrenceMutationScope::ThisOccurrence
        && operation.desired.event.recurrence_kind == RecurrenceKind::Series
    {
        let key = operation
            .input
            .selection
            .original_start
            .as_deref()
            .ok_or_else(|| invalid("missing source position"))?;
        let mut set = operation.desired.standalone_at(key)?;
        set.event.uid = Some(format!("{}@chithi", operation.id));
        set
    } else {
        operation.desired.clone()
    };
    desired.capture_content()?;
    if !operation.native_move
        && target.remote_calendar_id.is_some()
        && (desired.event.organizer_email.is_some() || desired.event.attendees_json.is_some())
    {
        let account = db::accounts::get_account_full(conn, &target.account_id)?;
        desired.prepare_new_meeting(&account.email)?;
    }
    for event in std::iter::once(&mut desired.event).chain(
        desired
            .overrides
            .iter_mut()
            .filter_map(|item| item.event.as_mut()),
    ) {
        event.id = operation.destination_event_id.clone();
        event.account_id = target.account_id.clone();
        event.calendar_id = target.calendar_id.clone();
        event.remote_id = None;
        event.etag = None;
        event.ical_data = None;
    }
    if target.remote_calendar_id.is_none() {
        desired.native = None;
        for exception in &mut desired.overrides {
            exception.native = None;
        }
    }
    // Retain source native content for adapter-owned fidelity, but it must never
    // be interpreted as destination addressing (create_event_set's contract).
    Ok(desired)
}

async fn commit_operation(state: &AppState, operation: &mut store::Operation) -> Result<()> {
    let canonical = operation
        .canonical
        .as_ref()
        .ok_or_else(|| invalid("missing canonical result"))?;
    let expected = if operation.destination.is_some() {
        transfer_set(operation, &state.db.reader())?
    } else {
        operation.desired.clone()
    };
    if !semantic_eq(canonical, &expected) {
        return Err(invalid(
            "provider result differs from the complete intended set",
        ));
    }
    if operation.destination.is_none() && !same_event_identity(&operation.source.set, canonical) {
        return Err(invalid(
            "provider update replaced the meeting identity without confirmation",
        ));
    }
    let retain_source = operation.destination.is_some()
        && operation.input.scope == RecurrenceMutationScope::ThisOccurrence
        && operation.source.set.event.recurrence_kind == RecurrenceKind::Series;
    if retain_source {
        let source_after = operation
            .source_after
            .as_ref()
            .ok_or_else(|| invalid("missing source exclusion"))?;
        let key = operation
            .input
            .selection
            .original_start
            .clone()
            .ok_or_else(|| invalid("missing original position"))?;
        let mut expected_source = operation.source.set.clone();
        expected_source
            .overrides
            .retain(|item| item.original_start != key);
        expected_source.overrides.push(CalendarOverride {
            original_start: key,
            event: None,
            native: None,
        });
        if !semantic_eq(source_after, &expected_source)
            || !same_event_identity(&operation.source.set, source_after)
        {
            return Err(invalid(
                "source exclusion changed content outside the selected scope",
            ));
        }
    }
    let mut conn = state.db.writer().await;
    let tx = conn.transaction()?;
    store::ensure_operation_current(&tx, operation)?;
    if let Some(target) = &operation.destination {
        ensure_destination(&tx, target)?;
        let mut event = canonical.event.clone();
        event.id = operation.destination_event_id.clone();
        event.account_id = target.account_id.clone();
        event.calendar_id = target.calendar_id.clone();
        event.source_message_id = operation.source.anchor.source_message_id.clone();
        db::calendar::insert_event(&tx, &event)?;
        store::persist_embedded(&tx, &event, canonical)?;
        copy_references(
            &tx,
            &operation.source.anchor.id,
            &event.id,
            !retain_source && operation.source.members.len() == 1,
        )?;
        if operation.input.scope == RecurrenceMutationScope::ThisOccurrence
            && operation.source.set.event.recurrence_kind == RecurrenceKind::Series
        {
            let owner = store::persist_source(
                &tx,
                &operation.source,
                operation
                    .source_after
                    .as_ref()
                    .ok_or_else(|| invalid("missing source exclusion"))?,
            )?;
            store::cache_canonical(
                &tx,
                owner,
                operation
                    .source_after
                    .as_ref()
                    .ok_or_else(|| invalid("missing source exclusion"))?,
            )?;
        } else {
            store::retire_source(&tx, &operation.source, &event)?;
        }
        store::cache_canonical(&tx, db::calendar::get_event(&tx, &event.id)?, canonical)?;
    } else {
        let owner = store::persist_source(&tx, &operation.source, canonical)?;
        store::cache_canonical(&tx, owner, canonical)?;
    }
    operation.stage = CalendarActionStage::Completed;
    store::save_operation(&tx, operation)?;
    tx.commit()?;
    Ok(())
}

fn same_event_identity(before: &CalendarEventSet, after: &CalendarEventSet) -> bool {
    before.event.uid == after.event.uid
        && before.event.remote_id == after.event.remote_id
        && match (&before.native, &after.native) {
            (Some(before), Some(after)) => {
                before.protocol == after.protocol
                    && before.calendar_id == after.calendar_id
                    && before.event_id == after.event_id
            }
            (None, None) => true,
            _ => false,
        }
}

fn copy_references(
    conn: &rusqlite::Connection,
    source: &str,
    target: &str,
    transfer: bool,
) -> Result<()> {
    if let Some(mut binding) = db::meet_meetings::get(conn, source)? {
        binding.event_id = target.to_string();
        db::meet_meetings::upsert(conn, &binding)?;
    }
    if transfer {
        conn.execute(
            "UPDATE calendar_invitation_sources SET event_id = ?1 WHERE event_id = ?2",
            rusqlite::params![target, source],
        )?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "calendar_actions_tests.rs"]
mod tests;
