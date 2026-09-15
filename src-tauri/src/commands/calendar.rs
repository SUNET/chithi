use serde::{Deserialize, Serialize};
use tauri::State;

use crate::backend::calendar::{
    AttendeeResponseUpdate, CalendarBackend, CalendarBackendCtx, CalendarCapability,
    EventCreationTarget, InviteReplyDelivery, InviteResponse, ParticipantSchedule,
    ParticipantScheduleRequest, RecurringImportFidelity, RemoteOccurrenceUpdate,
    RemoteOccurrenceUpdateOutcome, RemoteRsvpPolicy, RemoteRsvpRequest, RoomAvailability,
    RoomAvailabilityRequest, RoomSuggestion,
};
use crate::calendar::ical::{self, ParsedInvite};
use crate::calendar::recurrence_identity::{
    OccurrenceFields, RecurrenceIdentity, RecurrenceMutationPlan, RecurrenceMutationScope,
    RecurrenceObjectKind, RecurrenceObjectSummary, UpdateOccurrenceInput, UpdatedOccurrence,
};
use crate::calendar::{Attendee, CalendarEvent, RecurrenceKind};
use crate::commands::sync_cmd::try_acquire_sync_guard;
use crate::db;
use crate::db::calendar::{Calendar, Invite, NewCalendar};
use crate::error::Result;
use crate::meet;
use crate::message::BodyLocation;
use crate::state::AppState;

fn calendar_backend_ctx(state: &AppState) -> CalendarBackendCtx<'_> {
    CalendarBackendCtx {
        db: &state.db,
        services: &state.providers,
    }
}

fn meet_provider_ctx(state: &AppState) -> meet::MeetProviderCtx<'_> {
    meet::MeetProviderCtx {
        services: &state.providers,
    }
}

/// Compute the duration in whole minutes between two ISO-8601
/// timestamps. Returns 60 (Zoom's API default) when either input
/// fails to parse or the range is non-positive, so a malformed
/// event time can't poison the reschedule call.
fn duration_minutes_between(start: &str, end: &str) -> u32 {
    let parse = |s: &str| {
        chrono::DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&chrono::Utc))
            .ok()
    };
    match (parse(start), parse(end)) {
        (Some(s), Some(e)) => {
            let minutes = (e - s).num_minutes();
            if minutes > 0 {
                minutes as u32
            } else {
                60
            }
        }
        _ => 60,
    }
}

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct NewEventInput {
    pub account_id: String,
    pub calendar_id: String,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_time: String,
    pub end_time: String,
    pub all_day: bool,
    pub timezone: Option<String>,
    pub recurrence_rule: Option<String>,
    pub attendees: Vec<Attendee>,
    /// Meet binding to persist alongside the event row (#148).
    /// `None` when the user didn't add a video link in this form.
    /// Frontend obtains this from the `meet_create_url` response.
    #[serde(default)]
    pub meet_binding: Option<MeetBindingInput>,
}

#[derive(Debug, Default, Deserialize)]
pub struct UpdateEventInput {
    pub calendar_id: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub all_day: Option<bool>,
    pub timezone: Option<String>,
    pub recurrence_rule: Option<String>,
    pub attendees: Option<Vec<Attendee>>,
    /// New meet binding to attach to this event. `None` means "leave
    /// existing binding alone." Always replaces any prior binding for
    /// the same event (one binding per event).
    #[serde(default)]
    pub meet_binding: Option<MeetBindingInput>,
}

/// Frontend-supplied copy of metadata returned from `meet_create_url`.
/// The backend accepts it only when it exactly matches a pending lifecycle.
#[derive(Debug, Clone, Deserialize)]
pub struct MeetBindingInput {
    pub lifecycle_id: String,
    pub account_id: String,
    pub protocol: String,
    pub meeting_id: String,
    pub join_url: String,
}

/// Defence-in-depth check on a client-supplied meet binding before
/// it touches the keyring or any provider API. The frontend always
/// sends bindings that round-tripped through `meet_create_url`, but
/// the Tauri command surface is also reachable from a compromised
/// renderer; rejecting bindings whose `protocol` doesn't match the
/// account's resolved meet provider stops an attacker from forging
/// e.g. a Zoom protocol entry against a Talk account and causing
/// Chithi to PATCH/DELETE arbitrary meetings.
fn claim_meet_binding(
    conn: &rusqlite::Connection,
    event_id: &str,
    b: &MeetBindingInput,
) -> Result<()> {
    let pending = matching_pending_meeting(conn, b)?;
    validate_pending_provider(conn, &pending)?;
    transfer_pending_meeting(conn, event_id, b, pending)
}

fn validate_pending_provider(
    conn: &rusqlite::Connection,
    pending: &db::meet_pending_meetings::PendingMeeting,
) -> Result<()> {
    let account = db::accounts::get_account_full(conn, &pending.account_id).map_err(|_| {
        crate::error::Error::Other(format!(
            "meet binding: unknown account {}",
            pending.account_id
        ))
    })?;
    let provider = meet::provider_for(&account).ok_or_else(|| {
        crate::error::Error::Other(format!(
            "meet binding: account {} has no meet provider",
            pending.account_id
        ))
    })?;
    if provider.protocol() != pending.protocol {
        return Err(crate::error::Error::Other(format!(
            "meet binding: protocol '{}' doesn't match account's resolved provider '{}'",
            pending.protocol,
            provider.protocol()
        )));
    }
    Ok(())
}

fn matching_pending_meeting(
    conn: &rusqlite::Connection,
    binding: &MeetBindingInput,
) -> Result<db::meet_pending_meetings::PendingMeeting> {
    if binding.meeting_id.trim().is_empty() || binding.join_url.trim().is_empty() {
        return Err(crate::error::Error::Other(
            "meet binding: meeting_id and join_url must be non-empty".into(),
        ));
    }
    let pending =
        db::meet_pending_meetings::get(conn, &binding.lifecycle_id)?.ok_or_else(|| {
            crate::error::Error::Other(format!(
                "meet binding: lifecycle {} is not pending",
                binding.lifecycle_id
            ))
        })?;
    if !pending_matches_binding(&pending, binding) {
        return Err(crate::error::Error::Other(format!(
            "meet binding: metadata does not match lifecycle {}",
            binding.lifecycle_id
        )));
    }
    Ok(pending)
}

fn transfer_pending_meeting(
    conn: &rusqlite::Connection,
    event_id: &str,
    binding: &MeetBindingInput,
    pending: db::meet_pending_meetings::PendingMeeting,
) -> Result<()> {
    db::meet_meetings::upsert(
        conn,
        &db::meet_meetings::MeetMeeting {
            event_id: event_id.to_string(),
            account_id: pending.account_id,
            protocol: pending.protocol,
            meeting_id: pending.meeting_id,
            join_url: pending.join_url,
        },
    )?;
    if !db::meet_pending_meetings::delete(conn, &binding.lifecycle_id)? {
        return Err(crate::error::Error::Other(format!(
            "meet binding: lifecycle {} disappeared during claim",
            binding.lifecycle_id
        )));
    }
    Ok(())
}

fn replace_meet_binding(
    conn: &rusqlite::Connection,
    event_id: &str,
    binding: &MeetBindingInput,
) -> Result<Option<String>> {
    let pending = matching_pending_meeting(conn, binding)?;
    validate_pending_provider(conn, &pending)?;
    replace_meet_binding_ownership(conn, event_id, binding)
}

fn replace_meet_binding_ownership(
    conn: &rusqlite::Connection,
    event_id: &str,
    binding: &MeetBindingInput,
) -> Result<Option<String>> {
    let pending = matching_pending_meeting(conn, binding)?;
    let cleanup_lifecycle_id = match db::meet_meetings::get(conn, event_id)? {
        Some(old)
            if old.account_id != binding.account_id
                || old.protocol != binding.protocol
                || old.meeting_id != binding.meeting_id =>
        {
            Some(queue_meeting_cleanup(conn, old)?)
        }
        _ => None,
    };
    transfer_pending_meeting(conn, event_id, binding, pending)?;
    Ok(cleanup_lifecycle_id)
}

fn queue_meeting_cleanup(
    conn: &rusqlite::Connection,
    binding: db::meet_meetings::MeetMeeting,
) -> Result<String> {
    let lifecycle_id = uuid::Uuid::new_v4().to_string();
    db::meet_pending_meetings::insert(
        conn,
        &db::meet_pending_meetings::PendingMeeting {
            lifecycle_id: lifecycle_id.clone(),
            account_id: binding.account_id,
            protocol: binding.protocol,
            meeting_id: binding.meeting_id,
            join_url: binding.join_url,
            created_at: chrono::Utc::now().to_rfc3339(),
            cleanup_requested: true,
        },
    )?;
    Ok(lifecycle_id)
}

fn pending_matches_binding(
    pending: &db::meet_pending_meetings::PendingMeeting,
    binding: &MeetBindingInput,
) -> bool {
    pending.lifecycle_id == binding.lifecycle_id
        && pending.account_id == binding.account_id
        && pending.protocol == binding.protocol
        && pending.meeting_id == binding.meeting_id
        && pending.join_url == binding.join_url
}

// ---------------------------------------------------------------------------
// Calendar management commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_calendars(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Vec<Calendar>> {
    log::debug!("list_calendars: account={}", account_id);
    let conn = state.db.reader();
    let calendars = db::calendar::list_calendars(&conn, &account_id)?;
    log::debug!("list_calendars: found {} calendars", calendars.len());
    Ok(calendars)
}

#[tauri::command]
pub async fn create_calendar(state: State<'_, AppState>, calendar: NewCalendar) -> Result<String> {
    log::info!(
        "create_calendar: account={} name='{}'",
        calendar.account_id,
        calendar.name
    );
    let id = uuid::Uuid::new_v4().to_string();
    let conn = state.db.writer().await;
    db::calendar::insert_calendar(&conn, &id, &calendar)?;
    log::info!("create_calendar: created calendar id={}", id);
    Ok(id)
}

#[tauri::command]
pub async fn update_calendar(
    state: State<'_, AppState>,
    calendar_id: String,
    name: String,
    color: String,
) -> Result<()> {
    log::info!(
        "update_calendar: id={} name='{}' color='{}'",
        calendar_id,
        name,
        color
    );

    // Load current calendar + account so we know whether the name changed
    // and, if so, which protocol to push the rename through. Drop the
    // reader before any await so the backend stays non-blocking.
    let (existing, account) = {
        let conn = state.db.reader();
        let cal = db::calendar::get_calendar(&conn, &calendar_id)?;
        let acct = db::accounts::get_account_full(&conn, &cal.account_id)?;
        (cal, acct)
    };

    let name_changed = existing.name != name;
    let color_changed = existing.color != color;
    let remote_id = existing.remote_id.clone().filter(|r| !r.is_empty());
    if name_changed {
        if let Some(ref rid) = remote_id {
            push_calendar_rename(&state, &account, rid, &name).await?;
        } else {
            log::info!(
                "update_calendar: skipping remote rename (no remote_id, local-only calendar)"
            );
        }
    }
    if color_changed {
        if let Some(ref rid) = remote_id {
            // CalDAV / JMAP propagate failures: a server reject rolls
            // back the local DB write below and surfaces an error.
            // Graph / Google swallow failures internally and log —
            // system calendars and read-only subscriptions return a
            // generic 500/403 rather than a structured error, and
            // refusing to apply *any* local color change for those
            // accounts would be worse than letting the local pick
            // stick.
            push_calendar_color(&state, &account, rid, &color).await?;
        } else {
            log::info!(
                "update_calendar: skipping remote color push (no remote_id, local-only calendar)"
            );
        }
    }

    let conn = state.db.writer().await;
    db::calendar::update_calendar(&conn, &calendar_id, &name, &color)?;
    Ok(())
}

/// Push a calendar color change to the account's remote server.
/// Per-provider swallow-vs-propagate semantics live in each
/// [`crate::backend::calendar::CalendarBackend`] impl. Local-only
/// calendars (no `remote_id`) never reach this function — caller
/// short-circuits.
async fn push_calendar_color(
    state: &AppState,
    account: &db::accounts::AccountFull,
    remote_id: &str,
    new_color: &str,
) -> Result<()> {
    match crate::backend::calendar::for_account(account) {
        Some(backend) => {
            let ctx = calendar_backend_ctx(state);
            backend
                .push_calendar_color(&ctx, account, remote_id, new_color)
                .await
        }
        None => {
            log::info!(
                "update_calendar: no remote color-push path for protocol '{}', keeping color local-only for {}",
                account.calendar_protocol_str(),
                account.id
            );
            Ok(())
        }
    }
}

/// Push a calendar rename to the account's remote server. Errors here
/// must propagate so the command leaves the local DB unchanged on
/// remote failure.
async fn push_calendar_rename(
    state: &AppState,
    account: &db::accounts::AccountFull,
    remote_id: &str,
    new_name: &str,
) -> Result<()> {
    match crate::backend::calendar::for_account(account) {
        Some(backend) => {
            let ctx = calendar_backend_ctx(state);
            backend
                .push_calendar_rename(&ctx, account, remote_id, new_name)
                .await
        }
        None => Err(crate::error::Error::Other(format!(
            "No remote rename path configured for account {} (calendar_protocol={})",
            account.id,
            account.calendar_protocol_str()
        ))),
    }
}

#[tauri::command]
pub async fn delete_calendar(state: State<'_, AppState>, calendar_id: String) -> Result<()> {
    log::info!("delete_calendar: id={}", calendar_id);
    let cleanup_ids = {
        let mut conn = state.db.writer().await;
        let transaction = conn.transaction()?;
        let cleanup_ids =
            db::calendar_event_deletion::delete_calendar_events(&transaction, &calendar_id)?
                .cleanup_lifecycle_ids;
        db::calendar::delete_calendar_row(&transaction, &calendar_id)?;
        transaction.commit()?;
        cleanup_ids
    };
    crate::commands::meet::sweep_pending(&state, cleanup_ids).await;
    log::info!("delete_calendar: deleted calendar {}", calendar_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Event management commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn unsubscribe_calendar(state: State<'_, AppState>, calendar_id: String) -> Result<()> {
    log::info!("unsubscribe_calendar: id={}", calendar_id);
    let deletion = {
        let mut conn = state.db.writer().await;
        let transaction = conn.transaction()?;
        db::calendar::set_calendar_subscribed(&transaction, &calendar_id, false)?;
        let deletion =
            db::calendar_event_deletion::delete_calendar_events(&transaction, &calendar_id)?;
        transaction.commit()?;
        deletion
    };
    let deleted = deletion.deleted;
    crate::commands::meet::sweep_pending(&state, deletion.cleanup_lifecycle_ids).await;
    log::info!(
        "unsubscribe_calendar: deleted {} events for calendar {}",
        deleted,
        calendar_id
    );
    Ok(())
}

// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn get_events(
    state: State<'_, AppState>,
    account_id: String,
    start: String,
    end: String,
    calendar_id: Option<String>,
) -> Result<Vec<CalendarEvent>> {
    log::debug!(
        "get_events: account={} range={}..{} calendar={:?}",
        account_id,
        start,
        end,
        calendar_id
    );
    let conn = state.db.reader();
    let events =
        db::calendar::list_events(&conn, &account_id, calendar_id.as_deref(), &start, &end)?;
    log::debug!("get_events: found {} events", events.len());
    Ok(events)
}

/// Refresh one selected event independently of the current displayed date range.
#[tauri::command]
pub fn get_calendar_event(state: State<'_, AppState>, event_id: String) -> Result<CalendarEvent> {
    db::calendar::get_event(&state.db.reader(), &event_id)
}

#[tauri::command]
pub async fn get_event_recurrence_objects(
    state: State<'_, AppState>,
    event_id: String,
) -> Result<Vec<RecurrenceObjectSummary>> {
    get_event_recurrence_objects_inner(&state, &event_id).await
}

async fn get_event_recurrence_objects_inner(
    state: &AppState,
    event_id: &str,
) -> Result<Vec<RecurrenceObjectSummary>> {
    let initial_account_id = db::calendar::get_event(&state.db.reader(), event_id)?.account_id;
    let account_lock = state.account_lifecycle.acquire(&initial_account_id);
    let _account_guard = account_lock.lock().await;

    let mut conn = state.db.writer().await;
    let transaction = conn.transaction()?;
    let event = db::calendar::get_event(&transaction, event_id)?;
    if event.account_id != initial_account_id {
        return Err(crate::error::Error::Other(
            "Event ownership changed while waiting to discover recurrence objects".into(),
        ));
    }
    let summaries = recurrence_object_summaries(&transaction, &event)?;
    transaction.commit()?;
    Ok(summaries)
}

fn recurrence_object_summaries(
    conn: &rusqlite::Connection,
    event: &CalendarEvent,
) -> Result<Vec<RecurrenceObjectSummary>> {
    if conn.is_autocommit() {
        return Err(crate::error::Error::Other(
            "Recurrence discovery requires a database transaction".into(),
        ));
    }
    let mut identities = db::calendar_recurrence::get_by_event_id(conn, &event.id)?;
    if identities.is_empty() {
        return Ok(Vec::new());
    }
    if matches!(
        event.recurrence_kind,
        RecurrenceKind::Standalone | RecurrenceKind::Unknown
    ) {
        return Err(crate::error::Error::Other(
            "Event and recurrence object classifications are contradictory".into(),
        ));
    }
    for identity in &identities {
        if identity.event_id != event.id || identity.account_id != event.account_id {
            return Err(crate::error::Error::Other(
                "Recurrence object does not belong to the selected event and account".into(),
            ));
        }
        if event.recurrence_kind == RecurrenceKind::Occurrence
            && !matches!(
                identity.kind,
                RecurrenceObjectKind::Occurrence | RecurrenceObjectKind::Exception
            )
        {
            return Err(crate::error::Error::Other(
                "Event and recurrence object classifications are contradictory".into(),
            ));
        }
    }

    identities.sort_by(|left, right| {
        recurrence_discovery_kind_rank(left.kind)
            .cmp(&recurrence_discovery_kind_rank(right.kind))
            .then_with(|| left.occurrence.start_time.cmp(&right.occurrence.start_time))
            .then_with(|| left.recurrence_id.cmp(&right.recurrence_id))
            .then_with(|| left.object_id.cmp(&right.object_id))
    });
    Ok(identities
        .into_iter()
        .map(|identity| RecurrenceObjectSummary::from_identity(identity, &event.calendar_id))
        .collect())
}

fn recurrence_discovery_kind_rank(kind: RecurrenceObjectKind) -> u8 {
    match kind {
        RecurrenceObjectKind::Master => 0,
        RecurrenceObjectKind::Occurrence
        | RecurrenceObjectKind::Exception
        | RecurrenceObjectKind::Exclusion => 1,
    }
}

#[tauri::command]
pub async fn plan_event_recurrence_mutation(
    state: State<'_, AppState>,
    event_id: String,
    recurrence_object_id: String,
    scope: RecurrenceMutationScope,
) -> Result<RecurrenceMutationPlan> {
    plan_event_recurrence_mutation_inner(&state, &event_id, &recurrence_object_id, scope).await
}

async fn plan_event_recurrence_mutation_inner(
    state: &AppState,
    event_id: &str,
    recurrence_object_id: &str,
    scope: RecurrenceMutationScope,
) -> Result<RecurrenceMutationPlan> {
    let initial_account_id = db::calendar::get_event(&state.db.reader(), event_id)?.account_id;
    let account_lock = state.account_lifecycle.acquire(&initial_account_id);
    let _account_guard = account_lock.lock().await;

    let mut conn = state.db.writer().await;
    let transaction = conn.transaction()?;
    let event = db::calendar::get_event(&transaction, event_id)?;
    if event.account_id != initial_account_id {
        return Err(crate::error::Error::Other(
            "Event ownership changed while waiting to plan recurrence mutation".into(),
        ));
    }
    let backend =
        calendar_backend_for_account(&transaction, &event.account_id)?.ok_or_else(|| {
            crate::error::Error::Other("Account has no enabled calendar backend binding".into())
        })?;
    let plan = build_recurrence_mutation_plan(
        &transaction,
        event,
        recurrence_object_id,
        scope,
        backend.protocol(),
    )?;
    transaction.commit()?;
    Ok(plan)
}

/// Build a plan inside the caller's transaction. This helper never locks or
/// commits, so write commands can revalidate without recursively locking.
fn build_recurrence_mutation_plan(
    conn: &rusqlite::Connection,
    event: CalendarEvent,
    recurrence_object_id: &str,
    scope: RecurrenceMutationScope,
    protocol: &str,
) -> Result<RecurrenceMutationPlan> {
    if conn.is_autocommit() {
        return Err(crate::error::Error::Other(
            "Recurrence mutation planning requires a database transaction".into(),
        ));
    }
    let identity = db::calendar_recurrence::get_by_object_id(conn, recurrence_object_id)?
        .ok_or_else(|| {
            crate::error::Error::Other(
                "Recurrence object is missing or belongs to a legacy unlinked event".into(),
            )
        })?;
    if identity.event_id != event.id || identity.account_id != event.account_id {
        return Err(crate::error::Error::Other(
            "Recurrence object does not belong to the selected event and account".into(),
        ));
    }
    validate_recurrence_plan_classification(&event, &identity, protocol, scope)?;

    let expected_local_revision = db::calendar_revision::get(conn, &event.id)?;
    let (remote_target_id, expected_provider_revision) = match scope {
        RecurrenceMutationScope::ThisOccurrence => {
            let target = match nonempty(identity.provider_occurrence_id.as_deref()) {
                Some(target) => target.to_owned(),
                None if matches!(protocol, "caldav" | "jmap") => {
                    nonempty(event.remote_id.as_deref())
                        .ok_or_else(|| {
                            crate::error::Error::Other(
                                "Embedded recurrence resource has no remote identity".into(),
                            )
                        })?
                        .to_owned()
                }
                None => {
                    return Err(crate::error::Error::Other(
                        "Recurrence occurrence has no provider occurrence identity".into(),
                    ));
                }
            };
            (target, identity.provider_revision.clone())
        }
        RecurrenceMutationScope::EntireSeries => {
            let (target, master) = resolve_series_plan_target(conn, &event, &identity)?;
            (target, master.provider_revision)
        }
    };

    if remote_target_id.chars().any(char::is_control) {
        return Err(crate::error::Error::Other(
            "Remote recurrence target must contain no control characters".into(),
        ));
    }
    Ok(RecurrenceMutationPlan {
        scope,
        recurrence_object_id: identity.object_id,
        event_id: event.id,
        account_id: event.account_id,
        calendar_id: event.calendar_id,
        object_kind: identity.kind,
        local_series_event_id: identity.local_series_event_id,
        provider_calendar_id: identity.provider_calendar_id,
        provider_series_id: identity.provider_series_id,
        provider_occurrence_id: identity.provider_occurrence_id,
        recurrence_id: identity.recurrence_id,
        recurrence_timezone: identity.recurrence_timezone,
        recurrence_value_type: identity.recurrence_value_type,
        occurrence: identity.occurrence,
        expected_provider_revision,
        expected_local_revision,
        backend_protocol: protocol.to_owned(),
        remote_target_id,
    })
}

#[tauri::command]
pub async fn update_event_recurrence_occurrence(
    state: State<'_, AppState>,
    event_id: String,
    recurrence_object_id: String,
    expected_local_revision: i64,
    expected_provider_revision: Option<String>,
    expected_backend_protocol: String,
    expected_remote_target_id: String,
    update: UpdateOccurrenceInput,
) -> Result<UpdatedOccurrence> {
    update_event_recurrence_occurrence_inner(
        &state,
        event_id,
        recurrence_object_id,
        expected_local_revision,
        expected_provider_revision,
        expected_backend_protocol,
        expected_remote_target_id,
        update,
        None,
    )
    .await
}

#[derive(Debug, Clone)]
struct OccurrenceUpdateSnapshot {
    event: CalendarEvent,
    identity: RecurrenceIdentity,
    owned_recurrence_objects: Vec<RecurrenceIdentity>,
    local_revision: i64,
}

async fn update_event_recurrence_occurrence_inner(
    state: &AppState,
    event_id: String,
    recurrence_object_id: String,
    expected_local_revision: i64,
    expected_provider_revision: Option<String>,
    expected_backend_protocol: String,
    expected_remote_target_id: String,
    update: UpdateOccurrenceInput,
    backend_override: Option<&dyn CalendarBackend>,
) -> Result<UpdatedOccurrence> {
    let initial_account_id = db::calendar::get_event(&state.db.reader(), &event_id)?.account_id;
    let account_lock = state.account_lifecycle.acquire(&initial_account_id);
    let _account_guard = account_lock.lock().await;

    let (snapshot, account, request, backend) = {
        let mut conn = state.db.writer().await;
        let transaction = conn.transaction()?;
        let event = db::calendar::get_event(&transaction, &event_id)?;
        if event.account_id != initial_account_id {
            return Err(crate::error::Error::Other(
                "Event ownership changed while waiting to update its occurrence".into(),
            ));
        }
        let backend = match backend_override {
            Some(backend) => backend,
            None => {
                calendar_backend_for_account(&transaction, &event.account_id)?.ok_or_else(|| {
                    crate::error::Error::Other(
                        "Account has no enabled calendar backend binding".into(),
                    )
                })?
            }
        };
        let plan = build_recurrence_mutation_plan(
            &transaction,
            event.clone(),
            &recurrence_object_id,
            RecurrenceMutationScope::ThisOccurrence,
            backend.protocol(),
        )?;
        if plan.expected_local_revision != expected_local_revision
            || plan.expected_provider_revision != expected_provider_revision
            || plan.backend_protocol != expected_backend_protocol
            || plan.remote_target_id != expected_remote_target_id
        {
            return Err(crate::error::Error::Other(
                "Recurrence mutation plan is stale; refresh before trying again".into(),
            ));
        }
        let identity =
            db::calendar_recurrence::get_by_object_id(&transaction, &recurrence_object_id)?
                .ok_or_else(|| {
                    crate::error::Error::Other("Recurrence object disappeared".into())
                })?;
        let desired = apply_occurrence_update(&identity, update.clone())?;
        let account = db::accounts::get_account_full(&transaction, &event.account_id)?;
        let snapshot = OccurrenceUpdateSnapshot {
            event: event.clone(),
            identity: identity.clone(),
            owned_recurrence_objects: db::calendar_recurrence::get_by_event_id(
                &transaction,
                &event.id,
            )?,
            local_revision: plan.expected_local_revision,
        };
        let mut current_occurrence = event;
        apply_occurrence_fields_to_event(&mut current_occurrence, &identity.occurrence);
        let request = RemoteOccurrenceUpdate {
            target_id: plan.remote_target_id,
            expected_provider_revision: plan.expected_provider_revision,
            trusted_identity: identity,
            current_event: current_occurrence,
            patch: update,
            desired,
        };
        transaction.commit()?;
        (snapshot, account, request, backend)
    };

    let outcome = backend
        .update_recurrence_occurrence(&calendar_backend_ctx(state), &account, &request)
        .await?;
    persist_remote_occurrence_update(state, &snapshot, outcome)
        .await
        .map_err(|error| {
            crate::error::Error::Sync(format!(
                "Remote occurrence update succeeded but local persistence failed; reconciliation required: {error}"
            ))
        })
}

fn apply_occurrence_update(
    identity: &RecurrenceIdentity,
    update: UpdateOccurrenceInput,
) -> Result<OccurrenceFields> {
    if update
        .title
        .as_deref()
        .is_some_and(|title| title.trim().is_empty())
    {
        return Err(crate::error::Error::Other(
            "occurrence title must be non-empty".into(),
        ));
    }
    let fields = OccurrenceFields {
        title: update
            .title
            .unwrap_or_else(|| identity.occurrence.title.clone()),
        description: update
            .description
            .or_else(|| identity.occurrence.description.clone()),
        location: update
            .location
            .or_else(|| identity.occurrence.location.clone()),
        start_time: update
            .start_time
            .unwrap_or_else(|| identity.occurrence.start_time.clone()),
        end_time: update
            .end_time
            .unwrap_or_else(|| identity.occurrence.end_time.clone()),
        all_day: update.all_day.unwrap_or(identity.occurrence.all_day),
        timezone: update
            .timezone
            .or_else(|| identity.occurrence.timezone.clone()),
    };
    fields.validate()?;
    Ok(fields)
}

async fn persist_remote_occurrence_update(
    state: &AppState,
    snapshot: &OccurrenceUpdateSnapshot,
    outcome: RemoteOccurrenceUpdateOutcome,
) -> Result<UpdatedOccurrence> {
    outcome.replacement_identity.validate()?;
    outcome.occurrence.validate()?;
    validate_replacement_identity(&snapshot.identity, &outcome)?;

    let detached = snapshot.event.recurrence_kind == RecurrenceKind::Occurrence;
    let (mut canonical, canonical_recurrence_objects) = match (
        detached,
        outcome.canonical_event,
        outcome.canonical_recurrence_objects,
    ) {
        (true, Some(event), None) => (Some(event), None),
        (true, None, _) => {
            return Err(crate::error::Error::Other(
                "Detached occurrence update did not return a canonical event".into(),
            ));
        }
        (true, Some(_), Some(_)) => {
            return Err(crate::error::Error::Other(
                "Detached occurrence update must not return embedded recurrence objects".into(),
            ));
        }
        (false, Some(_), _) => {
            return Err(crate::error::Error::Other(
                "Embedded occurrence update must not replace the series event".into(),
            ));
        }
        (false, None, Some(seeds)) if !seeds.is_empty() => {
            validate_canonical_recurrence_objects(
                &snapshot.identity,
                &outcome.replacement_identity,
                &seeds,
            )?;
            (None, Some(seeds))
        }
        (false, None, _) => {
            return Err(crate::error::Error::Other(
                "Embedded occurrence update requires a complete canonical recurrence object set"
                    .into(),
            ));
        }
    };
    if let Some(event) = canonical.as_ref() {
        validate_canonical_occurrence(&snapshot.event, event, &outcome.occurrence)?;
    }

    let mut conn = state.db.writer().await;
    let transaction = conn.transaction()?;
    let current_event = db::calendar::get_event(&transaction, &snapshot.event.id)?;
    let current_identity =
        db::calendar_recurrence::get_by_object_id(&transaction, &snapshot.identity.object_id)?;
    let current_recurrence_objects =
        db::calendar_recurrence::get_by_event_id(&transaction, &snapshot.event.id)?;
    if current_event != snapshot.event
        || db::calendar_revision::get(&transaction, &snapshot.event.id)? != snapshot.local_revision
        || current_identity.as_ref() != Some(&snapshot.identity)
        || current_recurrence_objects != snapshot.owned_recurrence_objects
    {
        return Err(crate::error::Error::Other(
            "Calendar occurrence changed after the remote update".into(),
        ));
    }

    if let Some(provider_event) = canonical.as_mut() {
        provider_event.id = snapshot.event.id.clone();
        provider_event.account_id = snapshot.event.account_id.clone();
        provider_event.calendar_id = snapshot.event.calendar_id.clone();
        db::calendar::update_event(&transaction, provider_event)?;
    }
    let replacement = if let Some(seeds) = canonical_recurrence_objects.as_deref() {
        db::calendar::replace_event_recurrence_objects(
            &transaction,
            &snapshot.identity.account_id,
            &snapshot.identity.event_id,
            seeds,
        )?;
        db::calendar_recurrence::get_by_object_id(&transaction, &snapshot.identity.object_id)?
            .ok_or_else(|| {
                crate::error::Error::Other(
                    "Canonical recurrence replacement lost the selected occurrence".into(),
                )
            })?
    } else {
        let replacement = outcome.replacement_identity.bind(
            &snapshot.identity.account_id,
            &snapshot.identity.event_id,
            &snapshot.identity.object_id,
        )?;
        db::calendar_recurrence::upsert(&transaction, &replacement)?;
        replacement
    };
    let local_revision = db::calendar_revision::get(&transaction, &snapshot.event.id)?;
    transaction.commit()?;

    let fields = replacement.occurrence.clone();
    Ok(UpdatedOccurrence {
        event_id: snapshot.event.id.clone(),
        recurrence_object_id: replacement.object_id,
        fields,
        local_revision,
        kind: replacement.kind,
        provider_revision: replacement.provider_revision,
    })
}

fn validate_canonical_recurrence_objects(
    selected: &RecurrenceIdentity,
    replacement: &crate::calendar::recurrence_identity::RecurrenceIdentitySeed,
    seeds: &[crate::calendar::recurrence_identity::RecurrenceIdentitySeed],
) -> Result<()> {
    let mut positions = std::collections::HashSet::new();
    let mut occurrence_ids = std::collections::HashSet::new();
    let mut selected_matches = 0;

    for seed in seeds {
        seed.validate()?;
        if seed.local_series_event_id != selected.local_series_event_id
            || seed.provider_calendar_id != selected.provider_calendar_id
            || seed.provider_series_id != selected.provider_series_id
        {
            return Err(crate::error::Error::Other(
                "Canonical recurrence set contains an unrelated object".into(),
            ));
        }

        let position = (seed.recurrence_value_type, seed.recurrence_id.as_deref());
        if !positions.insert(position) {
            return Err(crate::error::Error::Other(
                "Canonical recurrence set contains a duplicate immutable position".into(),
            ));
        }
        if let Some(occurrence_id) = seed.provider_occurrence_id.as_deref() {
            if !occurrence_ids.insert(occurrence_id) {
                return Err(crate::error::Error::Other(
                    "Canonical recurrence set contains a duplicate provider occurrence ID".into(),
                ));
            }
        }

        if seed_matches_immutable_identity(seed, selected) {
            selected_matches += 1;
            if seed != replacement {
                return Err(crate::error::Error::Other(
                    "Selected canonical recurrence object contradicts its replacement".into(),
                ));
            }
        }
    }

    if selected_matches != 1 {
        return Err(crate::error::Error::Other(
            "Canonical recurrence set must contain exactly one selected occurrence".into(),
        ));
    }
    Ok(())
}

fn seed_matches_immutable_identity(
    seed: &crate::calendar::recurrence_identity::RecurrenceIdentitySeed,
    identity: &RecurrenceIdentity,
) -> bool {
    seed.local_series_event_id == identity.local_series_event_id
        && seed.provider_calendar_id == identity.provider_calendar_id
        && seed.provider_series_id == identity.provider_series_id
        && seed.provider_occurrence_id == identity.provider_occurrence_id
        && seed.recurrence_id == identity.recurrence_id
        && seed.recurrence_timezone == identity.recurrence_timezone
        && seed.recurrence_value_type == identity.recurrence_value_type
}

fn validate_replacement_identity(
    current: &RecurrenceIdentity,
    outcome: &RemoteOccurrenceUpdateOutcome,
) -> Result<()> {
    let replacement = &outcome.replacement_identity;
    let allowed_kind = matches!(
        (current.kind, replacement.kind),
        (
            RecurrenceObjectKind::Occurrence,
            RecurrenceObjectKind::Occurrence
        ) | (
            RecurrenceObjectKind::Occurrence,
            RecurrenceObjectKind::Exception
        ) | (
            RecurrenceObjectKind::Exception,
            RecurrenceObjectKind::Occurrence
        ) | (
            RecurrenceObjectKind::Exception,
            RecurrenceObjectKind::Exception
        )
    );
    if !allowed_kind
        || replacement.local_series_event_id != current.local_series_event_id
        || replacement.provider_calendar_id != current.provider_calendar_id
        || replacement.provider_series_id != current.provider_series_id
        || replacement.provider_occurrence_id != current.provider_occurrence_id
        || replacement.recurrence_id != current.recurrence_id
        || replacement.recurrence_timezone != current.recurrence_timezone
        || replacement.recurrence_value_type != current.recurrence_value_type
    {
        return Err(crate::error::Error::Other(
            "Provider returned a different immutable recurrence identity".into(),
        ));
    }
    if replacement.occurrence != outcome.occurrence {
        return Err(crate::error::Error::Other(
            "Provider recurrence content does not match its occurrence projection".into(),
        ));
    }
    Ok(())
}

fn validate_canonical_occurrence(
    current: &CalendarEvent,
    canonical: &CalendarEvent,
    occurrence: &OccurrenceFields,
) -> Result<()> {
    if canonical.id != current.id
        || canonical.account_id != current.account_id
        || canonical.calendar_id != current.calendar_id
        || canonical.uid != current.uid
        || canonical.recurrence_rule != current.recurrence_rule
        || canonical.recurrence_kind != RecurrenceKind::Occurrence
        || canonical.organizer_email != current.organizer_email
        || canonical.attendees_json != current.attendees_json
        || canonical.my_status != current.my_status
        || canonical.source_message_id != current.source_message_id
        || canonical.remote_id != current.remote_id
        || occurrence_fields_from_event(canonical) != *occurrence
    {
        return Err(crate::error::Error::Other(
            "Provider returned a contradictory canonical occurrence".into(),
        ));
    }
    occurrence.validate()
}

fn occurrence_fields_from_event(event: &CalendarEvent) -> OccurrenceFields {
    OccurrenceFields {
        title: event.title.clone(),
        description: event.description.clone(),
        location: event.location.clone(),
        start_time: event.start_time.clone(),
        end_time: event.end_time.clone(),
        all_day: event.all_day,
        timezone: event.timezone.clone(),
    }
}

fn apply_occurrence_fields_to_event(event: &mut CalendarEvent, fields: &OccurrenceFields) {
    event.title = fields.title.clone();
    event.description = fields.description.clone();
    event.location = fields.location.clone();
    event.start_time = fields.start_time.clone();
    event.end_time = fields.end_time.clone();
    event.all_day = fields.all_day;
    event.timezone = fields.timezone.clone();
}

fn validate_recurrence_plan_classification(
    event: &CalendarEvent,
    identity: &crate::calendar::recurrence_identity::RecurrenceIdentity,
    protocol: &str,
    scope: RecurrenceMutationScope,
) -> Result<()> {
    if identity.kind == RecurrenceObjectKind::Exclusion {
        return Err(crate::error::Error::Other(
            "Excluded recurrence instances cannot be mutation targets".into(),
        ));
    }
    if identity.kind == RecurrenceObjectKind::Master
        && scope != RecurrenceMutationScope::EntireSeries
    {
        return Err(crate::error::Error::Other(
            "A recurrence master can only target the entire series".into(),
        ));
    }
    if identity.kind != RecurrenceObjectKind::Master
        && (identity.recurrence_id.is_none() || identity.recurrence_value_type.is_none())
    {
        return Err(crate::error::Error::Other(
            "Occurrence mutation requires an immutable recurrence ID and value type".into(),
        ));
    }

    let detached_occurrence = matches!(
        identity.kind,
        RecurrenceObjectKind::Occurrence | RecurrenceObjectKind::Exception
    ) && event.recurrence_kind == RecurrenceKind::Occurrence;
    let embedded_occurrence = matches!(
        identity.kind,
        RecurrenceObjectKind::Occurrence | RecurrenceObjectKind::Exception
    ) && event.recurrence_kind == RecurrenceKind::Series
        && identity.provider_occurrence_id.is_none()
        && matches!(protocol, "caldav" | "jmap");
    let master = identity.kind == RecurrenceObjectKind::Master
        && event.recurrence_kind == RecurrenceKind::Series;
    if !master && !detached_occurrence && !embedded_occurrence {
        return Err(crate::error::Error::Other(
            "Event and recurrence object classifications are contradictory".into(),
        ));
    }
    Ok(())
}

fn resolve_series_plan_target(
    conn: &rusqlite::Connection,
    event: &CalendarEvent,
    identity: &crate::calendar::recurrence_identity::RecurrenceIdentity,
) -> Result<(
    String,
    crate::calendar::recurrence_identity::RecurrenceIdentity,
)> {
    let local_series_event_id = if identity.kind == RecurrenceObjectKind::Master {
        Some(identity.event_id.as_str())
    } else {
        identity.local_series_event_id.as_deref()
    };
    if let Some(local_id) = local_series_event_id {
        let local_event = db::calendar::get_event(conn, local_id)?;
        if local_event.account_id != event.account_id {
            return Err(crate::error::Error::Other(
                "Local recurrence series belongs to another account".into(),
            ));
        }
    }
    let master = db::calendar_recurrence::resolve_master(
        conn,
        &event.account_id,
        local_series_event_id,
        identity.provider_calendar_id.as_deref(),
        identity.provider_series_id.as_deref(),
    )?
    .ok_or_else(|| {
        crate::error::Error::Other("No exact recurrence series master could be resolved".into())
    })?;
    let master_event = db::calendar::get_event(conn, &master.event_id)?;
    if master_event.account_id != event.account_id
        || master_event.recurrence_kind != RecurrenceKind::Series
    {
        return Err(crate::error::Error::Other(
            "Resolved recurrence master is not a series in the selected account".into(),
        ));
    }

    let provider_target = nonempty(identity.provider_series_id.as_deref());
    let local_target = nonempty(master_event.remote_id.as_deref());
    if local_series_event_id.is_some() && local_target.is_none() {
        return Err(crate::error::Error::Other(
            "Local recurrence series event has no remote identity".into(),
        ));
    }
    if let (Some(provider), Some(local)) = (provider_target, local_target) {
        if provider != local {
            return Err(crate::error::Error::Other(
                "Local and provider series identities resolve to different remote targets".into(),
            ));
        }
    }
    let target = provider_target.or(local_target).ok_or_else(|| {
        crate::error::Error::Other("Recurrence series has no remote identity".into())
    })?;
    Ok((target.to_owned(), master))
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

/// List all calendar invites for an account — events where the account is
/// an attendee but not the organizer. Backs the dedicated Invites view.
/// The recent-past window is fixed at 7 days; recurring invites always pass.
#[tauri::command]
pub async fn list_invites(state: State<'_, AppState>, account_id: String) -> Result<Vec<Invite>> {
    let since = (chrono::Utc::now() - chrono::Duration::days(7)).to_rfc3339();
    let conn = state.db.reader();
    let account = db::accounts::get_account_full(&conn, &account_id)?;
    let invites = db::calendar::list_invites(&conn, &account_id, &account.email, &since)?;
    log::debug!(
        "list_invites: account={} found {} invites",
        account_id,
        invites.len()
    );
    Ok(invites)
}

/// Mark a stored invitation as handled locally without sending an RSVP.
#[tauri::command]
pub async fn mark_invite_managed(
    state: State<'_, AppState>,
    account_id: String,
    event_id: String,
) -> Result<()> {
    let account_lock = state.account_lifecycle.acquire(&account_id);
    let _account_guard = account_lock.lock().await;
    let conn = state.db.writer().await;
    db::calendar::mark_invite_managed(&conn, &account_id, &event_id)
}

#[tauri::command]
pub async fn create_event(state: State<'_, AppState>, event: NewEventInput) -> Result<String> {
    create_event_inner(&state, event, None).await
}

async fn create_event_inner(
    state: &AppState,
    event: NewEventInput,
    move_source: Option<&MoveSourceSnapshot>,
) -> Result<String> {
    Ok(create_event_with_receipt(state, event, move_source, false)
        .await?
        .event
        .id)
}

/// The receipt is captured at insertion, not after provider I/O, so a move
/// cannot accept a different destination row that appeared while publishing.
#[derive(Debug)]
struct CreatedEventReceipt {
    event: CalendarEvent,
    revision: i64,
    calendar: db::calendar::Calendar,
}

impl CreatedEventReceipt {
    fn ensure_current(&self, conn: &rusqlite::Connection) -> Result<()> {
        if conn.is_autocommit() {
            return Err(crate::error::Error::Other(
                "Creation receipt validation requires a database transaction.".into(),
            ));
        }
        let event = db::calendar::get_event(conn, &self.event.id)?;
        let calendar = db::calendar::get_calendar(conn, &self.calendar.id)?;
        if event != self.event
            || db::calendar_revision::get(conn, &self.event.id)? != self.revision
            || calendar.account_id != self.calendar.account_id
            || calendar.remote_id != self.calendar.remote_id
            || calendar.is_subscribed != self.calendar.is_subscribed
        {
            return Err(crate::error::Error::Other(
                "Created event or destination calendar changed. Refresh before trying again."
                    .into(),
            ));
        }
        Ok(())
    }
}

/// Attach only this operation's provider identity and advance its receipt in
/// one transaction. Never bless intervening writes by taking a fresh snapshot.
async fn attach_created_event_identity(
    state: &AppState,
    created: &mut CreatedEventReceipt,
    pushed: crate::backend::calendar::PushedEvent,
) -> Result<()> {
    let mut conn = state.db.writer().await;
    let transaction = conn.transaction()?;
    created.ensure_current(&transaction)?;
    let mut event = created.event.clone();
    event.remote_id = Some(pushed.remote_id);
    event.etag = pushed.etag;
    if let Some(uid) = pushed.canonical_uid {
        event.uid = Some(uid);
    }
    transaction.execute(
        "UPDATE calendar_events SET remote_id = ?1, uid = ?2, etag = ?3 WHERE id = ?4",
        rusqlite::params![event.remote_id, event.uid, event.etag, event.id],
    )?;
    let revision = db::calendar_revision::get(&transaction, &event.id)?;
    transaction.commit()?;
    created.event = event;
    created.revision = revision;
    Ok(())
}

/// Preserve enough transport identity to reconcile or retry when persisting a
/// provider-canonical UID fails after remote creation.
async fn attach_created_event_transport_identity(
    state: &AppState,
    created: &mut CreatedEventReceipt,
    remote_id: String,
    etag: Option<String>,
) -> Result<()> {
    let mut conn = state.db.writer().await;
    let transaction = conn.transaction()?;
    created.ensure_current(&transaction)?;
    let updated = transaction.execute(
        "UPDATE calendar_events SET remote_id = ?1, etag = ?2
         WHERE id = ?3 AND (remote_id IS NULL OR remote_id = '')",
        rusqlite::params![remote_id, etag, created.event.id],
    )?;
    if updated != 1 {
        return Err(crate::error::Error::Other(
            "The created event is unavailable for transport identity recovery".into(),
        ));
    }
    let event = db::calendar::get_event(&transaction, &created.event.id)?;
    let revision = db::calendar_revision::get(&transaction, &created.event.id)?;
    transaction.commit()?;
    created.event = event;
    created.revision = revision;
    Ok(())
}

async fn create_event_with_receipt(
    state: &AppState,
    event: NewEventInput,
    move_source: Option<&MoveSourceSnapshot>,
    account_already_locked: bool,
) -> Result<CreatedEventReceipt> {
    create_event_with_metadata(state, event, move_source, None, account_already_locked).await
}

#[derive(Debug)]
struct ImportedEventMetadata {
    uid: String,
    recurrence_kind: RecurrenceKind,
    ical_data: String,
    source_message_id: String,
    organizer_email: Option<String>,
    attendees_json: Option<String>,
    my_status: Option<String>,
    invitation_source: Option<db::calendar_invitation_source::InvitationSource>,
    personal_copy: bool,
    require_remote_creation: bool,
}

async fn create_event_with_metadata(
    state: &AppState,
    event: NewEventInput,
    move_source: Option<&MoveSourceSnapshot>,
    imported: Option<ImportedEventMetadata>,
    account_already_locked: bool,
) -> Result<CreatedEventReceipt> {
    log::info!(
        "create_event: account={} calendar={} title='{}' attendees={}",
        event.account_id,
        event.calendar_id,
        event.title,
        event.attendees.len()
    );
    let id = uuid::Uuid::new_v4().to_string();

    let recurrence_rule = if imported.is_some() {
        event.recurrence_rule.clone()
    } else {
        event
            .recurrence_rule
            .as_deref()
            .filter(|rule| !rule.is_empty())
            .map(|rule| {
                crate::calendar::recurrence::normalize_invitation_rrule(
                    rule,
                    event.timezone.as_deref(),
                )
                .ok_or_else(|| {
                    crate::error::Error::Other(
                        "The recurrence rule cannot be represented safely in a new invitation."
                            .into(),
                    )
                })
            })
            .transpose()?
    };

    let attendees_json = if event.attendees.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&event.attendees).unwrap_or_else(|_| "[]".to_string()))
    };

    let recurrence_kind = imported
        .as_ref()
        .map(|metadata| metadata.recurrence_kind)
        .unwrap_or_else(|| RecurrenceKind::from_rule(recurrence_rule.as_deref()));
    let mut cal_event = CalendarEvent {
        id: id.clone(),
        account_id: event.account_id,
        calendar_id: event.calendar_id,
        uid: Some(
            imported
                .as_ref()
                .map(|metadata| metadata.uid.clone())
                .unwrap_or_else(|| format!("{}@chithi", uuid::Uuid::new_v4())),
        ),
        title: event.title,
        description: event.description,
        location: event.location,
        start_time: event.start_time,
        end_time: event.end_time,
        all_day: event.all_day,
        timezone: event.timezone,
        recurrence_kind,
        recurrence_rule,
        organizer_email: None,
        attendees_json: imported
            .as_ref()
            .and_then(|metadata| metadata.attendees_json.clone())
            .or(attendees_json),
        my_status: imported
            .as_ref()
            .and_then(|metadata| metadata.my_status.clone()),
        source_message_id: imported
            .as_ref()
            .map(|metadata| metadata.source_message_id.clone()),
        ical_data: imported.as_ref().map(|metadata| metadata.ical_data.clone()),
        remote_id: None,
        etag: None,
    };

    let meet_binding = event.meet_binding;

    let lifecycle_lock = match meet_binding.as_ref() {
        Some(binding) => Some(state.meet_lifecycle.acquire(&binding.lifecycle_id)?),
        None => None,
    };
    let _lifecycle_guard = match lifecycle_lock.as_ref() {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };
    let account_lock =
        (!account_already_locked).then(|| state.account_lifecycle.acquire(&cal_event.account_id));
    let account_guard = match account_lock.as_ref() {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };

    // Insert the event and transfer meeting ownership in one transaction.
    let (account, mut created) = {
        let mut conn = state.db.writer().await;
        let transaction = conn.transaction()?;
        if let Some(source) = move_source {
            checked_mutation_target(&transaction, &source.event.id, Some(source))?;
        }
        let account = db::accounts::get_account_full(&transaction, &cal_event.account_id)?;
        cal_event.organizer_email = imported
            .as_ref()
            .and_then(|metadata| metadata.organizer_email.clone())
            .or_else(|| Some(account.email.clone()));

        // The event's calendar's remote handle — the JMAP backend
        // creates the event on that specific calendar; Google/Graph
        // write to their default calendar and ignore it.
        check_target_calendar(&transaction, &cal_event.calendar_id, &cal_event.account_id)?;
        let calendar = db::calendar::get_calendar(&transaction, &cal_event.calendar_id)?;
        let remote_cal_id = calendar.remote_id.as_deref().unwrap_or_default();
        let backend = crate::backend::calendar::for_account(&account);
        if imported
            .as_ref()
            .is_some_and(|metadata| metadata.require_remote_creation)
            && backend.is_none()
        {
            return Err(crate::error::Error::Other(
                "The invitation destination has no calendar provider".into(),
            ));
        }
        if let Some(backend) = backend {
            backend.validate_event_creation(&cal_event, remote_cal_id)?;
        }
        db::calendar::insert_event(&transaction, &cal_event)?;
        if imported.is_none() && cal_event.recurrence_kind == RecurrenceKind::Series {
            db::calendar_invitation::record_local_series(&transaction, &cal_event)?;
        }
        if let Some(source) = imported
            .as_ref()
            .and_then(|metadata| metadata.invitation_source.as_ref())
        {
            db::calendar_invitation_source::record(&transaction, &cal_event.id, source)?;
        }
        if let Some(ref binding) = meet_binding {
            claim_meet_binding(&transaction, &id, binding)?;
        }
        let created = CreatedEventReceipt {
            event: cal_event.clone(),
            revision: db::calendar_revision::get(&transaction, &id)?,
            calendar,
        };
        transaction.commit()?;
        (account, created)
    };
    drop(_lifecycle_guard);

    if let Some(backend) = crate::backend::calendar::for_account(&account) {
        let remote_cal_id = created.calendar.remote_id.clone().unwrap_or_default();
        if remote_cal_id.is_empty() {
            log::warn!(
                "create_event: no remote calendar ID for local calendar '{}'",
                cal_event.calendar_id
            );
        }
        // Best-effort: a failed push does not roll back the local insert.
        // Moves still require their unchanged local copy before source deletion.
        let ctx = calendar_backend_ctx(state);
        let mut provider_event = cal_event.clone();
        if imported
            .as_ref()
            .is_some_and(|metadata| metadata.personal_copy)
        {
            provider_event.organizer_email = None;
            provider_event.attendees_json = None;
        }
        match backend
            .push_created_event(&ctx, &account, &provider_event, &remote_cal_id)
            .await
        {
            Ok(Some(pushed)) => {
                let pushed_remote_id = pushed.remote_id.clone();
                let pushed_etag = pushed.etag.clone();
                log::info!(
                    "create_event: pushed via {}, remote_id={}",
                    backend.protocol(),
                    pushed.remote_id
                );
                if let Err(error) = attach_created_event_identity(state, &mut created, pushed).await
                {
                    log::error!(
                        "create_event: remote creation succeeded but identity attachment failed for {id}: {error}"
                    );
                    let recovery = attach_created_event_transport_identity(
                        state,
                        &mut created,
                        pushed_remote_id.clone(),
                        pushed_etag,
                    )
                    .await;
                    if let Err(recovery_error) = recovery {
                        let cleanup = backend
                            .push_deleted_event(&ctx, &account, &pushed_remote_id, &remote_cal_id)
                            .await;
                        if let Err(cleanup_error) = cleanup {
                            return Err(crate::error::Error::Other(format!(
                                "Remote event creation succeeded, but identity persistence, transport recovery, and remote rollback all failed: identity: {error}; recovery: {recovery_error}; rollback: {cleanup_error}"
                            )));
                        }
                        let mut conn = state.db.writer().await;
                        let local_cleanup = (|| -> Result<()> {
                            let transaction = conn.transaction()?;
                            created.ensure_current(&transaction)?;
                            db::calendar_event_deletion::delete_event(&transaction, &id)?;
                            transaction.commit()?;
                            Ok(())
                        })();
                        return Err(crate::error::Error::Other(match local_cleanup {
                            Ok(()) => format!(
                                "Remote event creation was rolled back because its identity could not be persisted; the unchanged local copy was also removed: {error}; recovery: {recovery_error}"
                            ),
                            Err(local_error) => format!(
                                "Remote event creation was rolled back because its identity could not be persisted; the changed local copy was retained without a remote identity: {error}; recovery: {recovery_error}; local cleanup: {local_error}"
                            ),
                        }));
                    }
                    log::warn!(
                        "create_event: retained transport identity for {id}; canonical UID will reconcile on sync"
                    );
                }
            }
            Ok(None) => {
                if imported
                    .as_ref()
                    .is_some_and(|metadata| metadata.require_remote_creation)
                {
                    let conn = state.db.writer().await;
                    conn.execute(
                        "DELETE FROM calendar_events WHERE id = ?1",
                        rusqlite::params![id],
                    )?;
                    return Err(crate::error::Error::UnsupportedCapability {
                        protocol: backend.protocol(),
                        capability: "confirmed remote invitation copy creation",
                    });
                }
            }
            Err(error) => {
                log::error!(
                    "create_event: {} push failed: {}",
                    backend.protocol(),
                    error
                );
                if imported
                    .as_ref()
                    .is_some_and(|metadata| metadata.require_remote_creation)
                {
                    let conn = state.db.writer().await;
                    conn.execute(
                        "DELETE FROM calendar_events WHERE id = ?1",
                        rusqlite::params![id],
                    )?;
                    return Err(error);
                }
            }
        }
    }

    drop(account_guard);

    // Re-apply the event title to the meet provider's meeting topic.
    // The frontend creates the meeting at "Add video link" time, when
    // the title input is often still empty, so the remote room ends
    // up named "Meeting" until we sync the final title here.
    if let Some(ref b) = meet_binding {
        sync_meet_topic(state, b, &cal_event.title).await;
    }

    log::info!("create_event: created event id={}", id);
    Ok(created)
}

#[tauri::command]
pub async fn list_room_suggestions(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Vec<RoomSuggestion>> {
    let account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    let Some(backend) = crate::backend::calendar::for_account(&account) else {
        return Ok(Vec::new());
    };
    let ctx = calendar_backend_ctx(&state);
    match backend.list_room_suggestions(&ctx, &account).await? {
        CalendarCapability::Supported(rooms) => Ok(rooms),
        CalendarCapability::Unsupported => {
            log::debug!(
                "list_room_suggestions: {} backend does not support room lookup",
                backend.protocol()
            );
            Ok(Vec::new())
        }
    }
}

#[tauri::command]
pub async fn check_room_availability(
    state: State<'_, AppState>,
    account_id: String,
    room_address: String,
    start_time: String,
    end_time: String,
) -> Result<RoomAvailability> {
    let account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    let Some(backend) = crate::backend::calendar::for_account(&account) else {
        return Ok(RoomAvailability {
            state: "unknown".into(),
            busy_start: None,
            busy_end: None,
        });
    };
    let request = RoomAvailabilityRequest {
        room_address,
        start_time,
        end_time,
    };
    let ctx = calendar_backend_ctx(&state);
    match backend
        .check_room_availability(&ctx, &account, &request)
        .await?
    {
        CalendarCapability::Supported(availability) => Ok(availability),
        CalendarCapability::Unsupported => Ok(RoomAvailability {
            state: "unknown".into(),
            busy_start: None,
            busy_end: None,
        }),
    }
}

#[tauri::command]
pub async fn get_participant_schedules(
    state: State<'_, AppState>,
    account_id: String,
    emails: Vec<String>,
    start_time: String,
    end_time: String,
) -> Result<Vec<ParticipantSchedule>> {
    let request = build_participant_schedule_request(emails, start_time, end_time)?;

    let account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };
    let Some(backend) = crate::backend::calendar::for_account(&account) else {
        return Ok(Vec::new());
    };
    let ctx = calendar_backend_ctx(&state);
    match backend
        .get_participant_schedules(&ctx, &account, &request)
        .await?
    {
        CalendarCapability::Supported(schedules) => Ok(schedules),
        CalendarCapability::Unsupported => Ok(Vec::new()),
    }
}

fn build_participant_schedule_request(
    emails: Vec<String>,
    start_time: String,
    end_time: String,
) -> Result<ParticipantScheduleRequest> {
    let mut emails: Vec<String> = emails
        .into_iter()
        .map(|email| email.trim().to_ascii_lowercase())
        .filter(|email| !email.is_empty())
        .collect();
    emails.sort();
    emails.dedup();
    if emails.len() > 50 {
        return Err(crate::error::Error::Other(
            "Scheduling assistant supports at most 50 participants".into(),
        ));
    }

    let start = chrono::DateTime::parse_from_rfc3339(&start_time)
        .map_err(|e| crate::error::Error::Other(format!("Invalid schedule start: {}", e)))?;
    let end = chrono::DateTime::parse_from_rfc3339(&end_time)
        .map_err(|e| crate::error::Error::Other(format!("Invalid schedule end: {}", e)))?;
    if end <= start || end - start > chrono::Duration::days(31) {
        return Err(crate::error::Error::Other(
            "Scheduling range must be positive and no longer than 31 days".into(),
        ));
    }

    Ok(ParticipantScheduleRequest {
        emails,
        start_time,
        end_time,
    })
}

/// Push the event title back to the meet provider as the meeting's
/// topic. Best-effort: a provider failure logs but doesn't abort the
/// event save. Lives here so both `create_event` and `update_event`
/// share one rename code path.
async fn sync_meet_topic(state: &AppState, binding: &MeetBindingInput, title: &str) {
    let meet_account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &binding.account_id).ok()
    };
    let Some(acc) = meet_account else {
        return;
    };
    let Some(provider) = meet::provider_for(&acc) else {
        return;
    };
    log::info!(
        "sync_meet_topic: {} meeting {} -> '{}'",
        binding.protocol,
        binding.meeting_id,
        title,
    );
    if let Err(e) = provider
        .update_topic(&meet_provider_ctx(state), &acc, &binding.meeting_id, title)
        .await
    {
        log::warn!("sync_meet_topic: provider update_topic failed: {}", e);
    }
}

#[tauri::command]
pub async fn update_event(
    state: State<'_, AppState>,
    event_id: String,
    event: UpdateEventInput,
) -> Result<()> {
    update_event_inner(&state, event_id, event).await
}

/// Move tokens are captured with event data in one database snapshot. A durable
/// revision also tracks hidden RSVP/management state and meeting ownership,
/// including changes that return all visible fields to their original values.
struct MoveSourceSnapshot {
    event: CalendarEvent,
    revision: i64,
    invitation_source: Option<db::calendar_invitation_source::InvitationSource>,
}

fn capture_move_source(conn: &rusqlite::Connection, event_id: &str) -> Result<MoveSourceSnapshot> {
    if conn.is_autocommit() {
        return Err(crate::error::Error::Other(
            "Move snapshot requires a database transaction.".into(),
        ));
    }
    let event = checked_mutation_target(conn, event_id, None)?;
    let revision = db::calendar_revision::get(conn, event_id)?;
    let invitation_source = db::calendar_invitation_source::get(conn, event_id)?;
    Ok(MoveSourceSnapshot {
        event,
        revision,
        invitation_source,
    })
}

/// Revalidation belongs inside the transaction that commits the copy/deletion.
fn checked_mutation_target(
    conn: &rusqlite::Connection,
    event_id: &str,
    expected: Option<&MoveSourceSnapshot>,
) -> Result<CalendarEvent> {
    let event = db::calendar::get_event(conn, event_id)?;
    event.ensure_mutable()?;
    if let Some(expected) = expected {
        if conn.is_autocommit() {
            return Err(crate::error::Error::Other(
                "Move revalidation requires a database transaction.".into(),
            ));
        }
        if expected.event != event
            || expected.revision != db::calendar_revision::get(conn, event_id)?
            || expected.invitation_source != db::calendar_invitation_source::get(conn, event_id)?
        {
            return Err(crate::error::Error::Other(
                "Calendar event changed during the move. Refresh before trying again.".into(),
            ));
        }
    }
    Ok(event)
}

fn check_target_calendar(
    conn: &rusqlite::Connection,
    calendar_id: &str,
    account_id: &str,
) -> Result<()> {
    let calendar = db::calendar::get_calendar(conn, calendar_id)?;
    if calendar.account_id != account_id {
        return Err(crate::error::Error::Other(
            "Target calendar does not belong to the selected account.".into(),
        ));
    }
    Ok(())
}

async fn update_event_inner(
    state: &AppState,
    event_id: String,
    event: UpdateEventInput,
) -> Result<()> {
    log::info!("update_event: id={}", event_id);
    // Reject unsupported targets before even resolving meeting ownership.
    let initial_account_id =
        checked_mutation_target(&state.db.reader(), &event_id, None)?.account_id;
    if event
        .recurrence_rule
        .as_deref()
        .is_some_and(|rule| !rule.is_empty())
    {
        return Err(crate::error::CalendarMutationBlockReason::Recurring.into());
    }
    let meet_binding = event.meet_binding;
    let lifecycle_lock = match meet_binding.as_ref() {
        Some(binding) => Some(state.meet_lifecycle.acquire(&binding.lifecycle_id)?),
        None => None,
    };
    let _lifecycle_guard = match lifecycle_lock.as_ref() {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };
    let account_lock = state.account_lifecycle.acquire(&initial_account_id);
    let account_guard = account_lock.lock().await;
    let (existing, prev_title, cleanup_lifecycle_id, reschedule_with, account) = {
        let mut conn = state.db.writer().await;
        let transaction = conn.transaction()?;

        // Load existing event, apply updates
        let mut existing = checked_mutation_target(&transaction, &event_id, None)?;
        if existing.account_id != initial_account_id {
            return Err(crate::error::Error::Other(
                "Event ownership changed while waiting to update it".into(),
            ));
        }
        let prev_start = existing.start_time.clone();
        let prev_end = existing.end_time.clone();
        let prev_title = existing.title.clone();

        if let Some(calendar_id) = event.calendar_id {
            check_target_calendar(&transaction, &calendar_id, &existing.account_id)?;
            existing.calendar_id = calendar_id;
        }
        if let Some(title) = event.title {
            existing.title = title;
        }
        if let Some(description) = event.description {
            existing.description = Some(description);
        }
        if let Some(location) = event.location {
            existing.location = Some(location);
        }
        if let Some(start_time) = event.start_time {
            existing.start_time = start_time;
        }
        if let Some(end_time) = event.end_time {
            existing.end_time = end_time;
        }
        if let Some(all_day) = event.all_day {
            existing.all_day = all_day;
        }
        if let Some(timezone) = event.timezone {
            existing.timezone = Some(timezone);
        }
        if let Some(recurrence_rule) = event.recurrence_rule {
            existing.recurrence_rule = Some(recurrence_rule);
        }
        if let Some(attendees) = event.attendees {
            existing.attendees_json = if attendees.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&attendees).unwrap_or_else(|_| "[]".to_string()))
            };
        }

        db::calendar::update_event(&transaction, &existing)?;
        log::info!("update_event: updated event {}", event_id);

        // Queue the old room and claim the new lifecycle in this transaction.
        let cleanup_lifecycle_id = match meet_binding.as_ref() {
            Some(binding) => replace_meet_binding(&transaction, &event_id, binding)?,
            None => None,
        };

        // If start/end changed and the event has a meet binding, ask the
        // provider to move the remote meeting to the new slot. Best-effort:
        // a provider failure logs but doesn't block the local update, since
        // the user can still open the room via the saved join URL.
        let time_changed = prev_start != existing.start_time || prev_end != existing.end_time;
        let reschedule_with = if time_changed && !existing.all_day {
            match db::meet_meetings::get(&transaction, &event_id)? {
                Some(b) => db::accounts::get_account_full(&transaction, &b.account_id)
                    .ok()
                    .map(|acc| (b, acc)),
                None => None,
            }
        } else {
            None
        };

        let account = db::accounts::get_account_full(&transaction, &existing.account_id)?;
        transaction.commit()?;
        (
            existing,
            prev_title,
            cleanup_lifecycle_id,
            reschedule_with,
            account,
        )
    };
    drop(_lifecycle_guard);

    // Keep the account stable through the remote calendar write. Meeting
    // cleanup acquires lifecycle then account, so release this guard first.
    if let Some(remote_id) = existing.remote_id.as_ref().filter(|r| !r.is_empty()) {
        if let Some(backend) = crate::backend::calendar::for_account(&account) {
            match backend
                .push_updated_event(&calendar_backend_ctx(state), &account, remote_id, &existing)
                .await
            {
                Ok(()) => log::info!("update_event: pushed via {}", backend.protocol()),
                Err(e) => log::error!("update_event: {} push failed: {}", backend.protocol(), e),
            }
        }
    }
    drop(account_guard);

    if let Some(lifecycle_id) = cleanup_lifecycle_id {
        if let Err(error) = crate::commands::meet::discard_pending(state, &lifecycle_id).await {
            log::warn!(
                "update_event: retained replaced meeting {} for retry: {}",
                lifecycle_id,
                error
            );
        }
    }

    if let Some((binding, meet_account)) = reschedule_with {
        if let Some(provider) = meet::provider_for(&meet_account) {
            let duration_minutes =
                duration_minutes_between(&existing.start_time, &existing.end_time);
            log::info!(
                "update_event: rescheduling {} meeting {} to {} ({}m)",
                binding.protocol,
                binding.meeting_id,
                existing.start_time,
                duration_minutes,
            );
            if let Err(e) = provider
                .reschedule_meeting(
                    &meet_provider_ctx(state),
                    &meet_account,
                    &binding.meeting_id,
                    &existing.start_time,
                    duration_minutes,
                )
                .await
            {
                log::warn!("update_event: meet reschedule failed: {}", e);
            }
        }
    }

    // Sync title to meet provider when the title changed, or a new
    // binding was just attached (the user may have clicked "Add
    // video link" with an empty title). Cheaper to look the binding
    // up fresh than to pipe one through from the writer scope above.
    let title_changed = prev_title != existing.title;
    let just_attached = meet_binding.is_some();
    if title_changed || just_attached {
        let binding = {
            let conn = state.db.reader();
            db::meet_meetings::get(&conn, &event_id)?
        };
        if let Some(b) = binding {
            let input = MeetBindingInput {
                lifecycle_id: String::new(),
                account_id: b.account_id,
                protocol: b.protocol,
                meeting_id: b.meeting_id,
                join_url: b.join_url,
            };
            sync_meet_topic(state, &input, &existing.title).await;
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn delete_event(state: State<'_, AppState>, event_id: String) -> Result<()> {
    delete_event_inner(&state, event_id, None).await
}

async fn delete_event_inner(
    state: &AppState,
    event_id: String,
    expected: Option<&MoveSourceSnapshot>,
) -> Result<()> {
    delete_event_with_destination(state, event_id, expected, None).await
}

async fn delete_event_with_destination(
    state: &AppState,
    event_id: String,
    expected: Option<&MoveSourceSnapshot>,
    destination: Option<&CreatedEventReceipt>,
) -> Result<()> {
    log::info!("delete_event: id={}", event_id);
    let initial_account_id =
        checked_mutation_target(&state.db.reader(), &event_id, None)?.account_id;
    let account_lock = state.account_lifecycle.acquire(&initial_account_id);
    let account_guard = account_lock.lock().await;

    // Check recurrence and capture remote targets in the same transaction as
    // local deletion and meeting-cleanup ownership. Sync cannot race the check.
    let (event, account, cal_remote_id, cleanup_lifecycle_id) = {
        let mut conn = state.db.writer().await;
        let transaction = conn.transaction()?;
        let evt = checked_mutation_target(&transaction, &event_id, expected)?;
        if evt.account_id != initial_account_id {
            return Err(crate::error::Error::Other(
                "Event ownership changed while waiting to delete it".into(),
            ));
        }
        if let Some(destination) = destination {
            destination.ensure_current(&transaction)?;
        }
        let acc = db::accounts::get_account_full(&transaction, &evt.account_id)?;
        let cal = db::calendar::get_calendar(&transaction, &evt.calendar_id).ok();
        let cal_rid = cal
            .and_then(|c| c.remote_id)
            .unwrap_or_else(|| "primary".to_string());
        let mut cleanup = db::calendar_event_deletion::delete_event(&transaction, &event_id)?
            .cleanup_lifecycle_ids;
        transaction.commit()?;
        (evt, acc, cal_rid, cleanup.pop())
    };

    // Delete from the calendar server if the event has a remote_id.
    // Best-effort: the committed local deletion stands if this fails.
    if let Some(ref remote_id) = event.remote_id {
        if !remote_id.is_empty() {
            if let Some(backend) = crate::backend::calendar::for_account(&account) {
                match backend
                    .push_deleted_event(
                        &calendar_backend_ctx(state),
                        &account,
                        remote_id,
                        &cal_remote_id,
                    )
                    .await
                {
                    Ok(()) => log::info!(
                        "delete_event: deleted from server via {}",
                        backend.protocol()
                    ),
                    Err(e) => log::error!(
                        "delete_event: {} server delete failed: {}",
                        backend.protocol(),
                        e
                    ),
                }
            }
        }
    }
    drop(account_guard);

    if let Some(lifecycle_id) = cleanup_lifecycle_id {
        if let Err(error) = crate::commands::meet::discard_pending(state, &lifecycle_id).await {
            log::warn!(
                "delete_event: retained meeting {} for cleanup retry: {}",
                lifecycle_id,
                error
            );
        }
    }

    log::info!("delete_event: deleted event {}", event_id);
    Ok(())
}

async fn delete_remote_event_required(state: &AppState, event: &CalendarEvent) -> Result<()> {
    let remote_id = event
        .remote_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            crate::error::Error::Other("The event has no confirmed remote identity".into())
        })?;
    let (account, remote_calendar_id) = {
        let conn = state.db.reader();
        let account = db::accounts::get_account_full(&conn, &event.account_id)?;
        let remote_calendar_id = db::calendar::get_calendar(&conn, &event.calendar_id)?
            .remote_id
            .unwrap_or_else(|| "primary".into());
        (account, remote_calendar_id)
    };
    let backend = crate::backend::calendar::for_account(&account).ok_or_else(|| {
        crate::error::Error::Other("The event account has no calendar provider".into())
    })?;
    backend
        .push_deleted_event(
            &calendar_backend_ctx(state),
            &account,
            remote_id,
            &remote_calendar_id,
        )
        .await
}

async fn discard_created_invitation_copy(
    state: &AppState,
    copied: &CreatedEventReceipt,
) -> Result<()> {
    {
        let conn = state.db.reader();
        let transaction = conn.unchecked_transaction()?;
        copied.ensure_current(&transaction)?;
        transaction.commit()?;
    }
    delete_remote_event_required(state, &copied.event).await?;
    let mut conn = state.db.writer().await;
    let transaction = conn.transaction()?;
    copied.ensure_current(&transaction)?;
    db::calendar_event_deletion::delete_event(&transaction, &copied.event.id)?;
    transaction.commit()?;
    Ok(())
}

fn commit_invitation_copy_move(
    conn: &mut rusqlite::Connection,
    source_event_id: &str,
    snapshot: &MoveSourceSnapshot,
    copied: &CreatedEventReceipt,
) -> Result<Vec<String>> {
    let transaction = conn.transaction()?;
    checked_mutation_target(&transaction, source_event_id, Some(snapshot))?;
    copied.ensure_current(&transaction)?;
    let rows = transaction.execute(
        "UPDATE calendar_invitation_sources SET event_id = ?1 WHERE event_id = ?2",
        rusqlite::params![copied.event.id, source_event_id],
    )?;
    if rows != 1 {
        return Err(crate::error::Error::Other(
            "The invitation provenance could not be transferred".into(),
        ));
    }
    let cleanup_ids = db::calendar_event_deletion::delete_event(&transaction, source_event_id)?
        .cleanup_lifecycle_ids;
    transaction.commit()?;
    Ok(cleanup_ids)
}

async fn restore_invitation_move_source(
    state: &AppState,
    snapshot: &MoveSourceSnapshot,
    copied: &CreatedEventReceipt,
) -> Result<()> {
    let provenance = snapshot.invitation_source.as_ref().ok_or_else(|| {
        crate::error::Error::Other("The invitation move has no source provenance".into())
    })?;
    let (account, remote_calendar_id) = {
        let conn = state.db.reader();
        (
            db::accounts::get_account_full(&conn, &snapshot.event.account_id)?,
            db::calendar::get_calendar(&conn, &snapshot.event.calendar_id)?
                .remote_id
                .unwrap_or_default(),
        )
    };
    let backend = crate::backend::calendar::for_account(&account).ok_or_else(|| {
        crate::error::Error::Other("The source account has no calendar provider".into())
    })?;
    let mut provider_event = snapshot.event.clone();
    provider_event.uid = Some(provenance.invitation_uid.clone());
    provider_event.remote_id = None;
    provider_event.etag = None;
    provider_event.organizer_email = None;
    provider_event.attendees_json = None;
    let pushed = backend
        .push_created_event(
            &calendar_backend_ctx(state),
            &account,
            &provider_event,
            &remote_calendar_id,
        )
        .await?
        .ok_or_else(|| crate::error::Error::UnsupportedCapability {
            protocol: backend.protocol(),
            capability: "confirmed invitation move restoration",
        })?;
    let restored_remote_id = pushed.remote_id.clone();

    let attach_result = {
        let mut conn = state.db.writer().await;
        (|| -> Result<()> {
            let transaction = conn.transaction()?;
            let mut source =
                checked_mutation_target(&transaction, &snapshot.event.id, Some(snapshot))?;
            copied.ensure_current(&transaction)?;
            source.remote_id = Some(pushed.remote_id);
            source.etag = pushed.etag;
            if let Some(uid) = pushed.canonical_uid {
                source.uid = Some(uid);
            }
            transaction.execute(
                "UPDATE calendar_events SET remote_id = ?1, uid = ?2, etag = ?3 WHERE id = ?4",
                rusqlite::params![source.remote_id, source.uid, source.etag, source.id],
            )?;
            transaction.commit()?;
            Ok(())
        })()
    };
    if let Err(attach_error) = attach_result {
        let remote_cleanup = backend
            .push_deleted_event(
                &calendar_backend_ctx(state),
                &account,
                &restored_remote_id,
                &remote_calendar_id,
            )
            .await;
        return Err(crate::error::Error::Other(match remote_cleanup {
            Ok(()) => format!(
                "The source copy was recreated but could not be attached locally; the recreation was rolled back: {attach_error}"
            ),
            Err(cleanup_error) => format!(
                "The source copy was recreated but could not be attached locally, and its rollback failed: {attach_error}; rollback: {cleanup_error}"
            ),
        }));
    }
    discard_created_invitation_copy(state, copied).await
}

/// Move a confirmed standalone event using authoritative source data, rather
/// than allowing a renderer to create a copy before validating its source.
#[tauri::command]
pub async fn move_event_to_calendar(
    state: State<'_, AppState>,
    event_id: String,
    target_calendar_id: String,
    target_account_id: String,
) -> Result<String> {
    move_event_to_calendar_inner(&state, event_id, target_calendar_id, target_account_id).await
}

async fn move_event_to_calendar_inner(
    state: &AppState,
    event_id: String,
    target_calendar_id: String,
    target_account_id: String,
) -> Result<String> {
    let snapshot = {
        let conn = state.db.reader();
        let transaction = conn.unchecked_transaction()?;
        let snapshot = capture_move_source(&transaction, &event_id)?;
        check_target_calendar(&transaction, &target_calendar_id, &target_account_id)?;
        transaction.commit()?;
        snapshot
    };
    if snapshot.event.calendar_id == target_calendar_id {
        return Ok(snapshot.event.id.clone());
    }
    if snapshot.event.account_id == target_account_id {
        update_event_inner(
            state,
            event_id.clone(),
            UpdateEventInput {
                calendar_id: Some(target_calendar_id),
                ..Default::default()
            },
        )
        .await?;
        return Ok(event_id);
    }
    let provenance_move = snapshot.invitation_source.is_some();
    let (first_account_id, second_account_id) = if snapshot.event.account_id < target_account_id {
        (
            snapshot.event.account_id.clone(),
            Some(target_account_id.clone()),
        )
    } else {
        (
            target_account_id.clone(),
            Some(snapshot.event.account_id.clone()),
        )
    };
    let first_account_lock =
        provenance_move.then(|| state.account_lifecycle.acquire(&first_account_id));
    let second_account_lock = provenance_move.then(|| {
        state
            .account_lifecycle
            .acquire(second_account_id.as_deref().unwrap())
    });
    let first_account_guard = match first_account_lock.as_ref() {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };
    let second_account_guard = match second_account_lock.as_ref() {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };
    if provenance_move {
        let conn = state.db.reader();
        let transaction = conn.unchecked_transaction()?;
        checked_mutation_target(&transaction, &event_id, Some(&snapshot))?;
        check_target_calendar(&transaction, &target_calendar_id, &target_account_id)?;
        transaction.commit()?;
    }
    let source = &snapshot.event;
    let attendees = source
        .attendees_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|_| {
            crate::error::Error::Other("Cannot move event with malformed attendees.".into())
        })?
        .unwrap_or_default();
    let copied_input = NewEventInput {
        account_id: target_account_id,
        calendar_id: target_calendar_id,
        title: source.title.clone(),
        description: source.description.clone(),
        location: source.location.clone(),
        start_time: source.start_time.clone(),
        end_time: source.end_time.clone(),
        all_day: source.all_day,
        timezone: source.timezone.clone(),
        recurrence_rule: None,
        attendees: if snapshot.invitation_source.is_some() {
            vec![]
        } else {
            attendees
        },
        meet_binding: None,
    };
    let copied = if let Some(provenance) = snapshot.invitation_source.as_ref() {
        create_event_with_metadata(
            state,
            copied_input,
            Some(&snapshot),
            Some(ImportedEventMetadata {
                uid: provenance.invitation_uid.clone(),
                recurrence_kind: source.recurrence_kind,
                ical_data: source.ical_data.clone().ok_or_else(|| {
                    crate::error::Error::Other(
                        "The invitation copy has no source calendar data".into(),
                    )
                })?,
                source_message_id: provenance.source_message_id.clone(),
                organizer_email: source.organizer_email.clone(),
                attendees_json: source.attendees_json.clone(),
                my_status: source.my_status.clone(),
                invitation_source: None,
                personal_copy: true,
                require_remote_creation: true,
            }),
            true,
        )
        .await?
    } else {
        create_event_with_receipt(state, copied_input, Some(&snapshot), false).await?
    };
    let copied_id = &copied.event.id;
    if snapshot.invitation_source.is_some() {
        let validation = {
            let conn = state.db.reader();
            let transaction = conn.unchecked_transaction()?;
            let result = checked_mutation_target(&transaction, &event_id, Some(&snapshot))
                .and_then(|_| copied.ensure_current(&transaction));
            transaction.commit()?;
            result
        };
        if let Err(validation_error) = validation {
            let compensation = discard_created_invitation_copy(state, &copied).await;
            return Err(crate::error::Error::Other(match compensation {
                Ok(()) => format!(
                    "The invitation changed while it was being moved; the destination copy was rolled back: {validation_error}"
                ),
                Err(compensation_error) => format!(
                    "The invitation changed while it was being moved, and destination rollback also failed: {validation_error}; rollback: {compensation_error}"
                ),
            }));
        }
        if let Err(source_error) = delete_remote_event_required(state, source).await {
            let compensation = discard_created_invitation_copy(state, &copied).await;
            return Err(crate::error::Error::Other(match compensation {
                Ok(()) => format!(
                    "The invitation move could not delete its source copy; the destination copy was rolled back: {source_error}"
                ),
                Err(compensation_error) => format!(
                    "The invitation move could not delete its source copy, and destination rollback also failed: {source_error}; rollback: {compensation_error}"
                ),
            }));
        }
        let commit_result = {
            let mut conn = state.db.writer().await;
            commit_invitation_copy_move(&mut conn, &event_id, &snapshot, &copied)
        };
        let cleanup_ids = match commit_result {
            Ok(cleanup_ids) => cleanup_ids,
            Err(commit_error) => {
                let compensation = restore_invitation_move_source(state, &snapshot, &copied).await;
                return Err(crate::error::Error::Other(match compensation {
                    Ok(()) => format!(
                        "The invitation move could not be committed after source deletion; the source was restored and destination removed: {commit_error}"
                    ),
                    Err(compensation_error) => format!(
                        "The invitation move could not be committed after source deletion, and compensation was incomplete: {commit_error}; compensation: {compensation_error}"
                    ),
                }));
            }
        };
        drop(second_account_guard);
        drop(first_account_guard);
        crate::commands::meet::sweep_pending(state, cleanup_ids).await;
        return Ok(copied.event.id);
    }
    if let Err(error) =
        delete_event_with_destination(state, event_id, Some(&snapshot), Some(&copied)).await
    {
        return Err(crate::error::Error::Other(format!(
            "Event copy {copied_id} was created, but the move could not be completed; the source was not removed: {error}"
        )));
    }
    Ok(copied.event.id)
}

// ---------------------------------------------------------------------------
// Calendar sync command
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn sync_calendars(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    #[allow(unused_variables)] force_full_sync: Option<bool>,
) -> Result<()> {
    log::info!("sync_calendars: account={}", account_id);

    // Serialize calendar sync per account. The frontend can trigger this
    // command from multiple sources (toolbar button, 5-minute periodic tick,
    // context menu); without this guard, overlapping runs would race on DB
    // writes and emit out-of-order "calendar-sync-*" events for the same
    // account (e.g. an early "calendar-sync-error" from one run would
    // overwrite the "running" state of another). Mirrors the mail-sync
    // pattern in `trigger_sync` / `sync_folder`.
    let Some(_guard) = try_acquire_sync_guard(
        &state.calendar_sync_in_progress,
        &account_id,
        "Calendar sync",
    ) else {
        // A sync for this account is already running; skip silently with no
        // event emission so the in-progress run's events stay coherent.
        return Ok(());
    };

    let account_lock = state.account_lifecycle.acquire(&account_id);
    let account_guard = account_lock.lock().await;
    let account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    // Gate on the per-account toggle after acquiring the lifecycle lock so an
    // account mutation cannot make the decision stale before provider I/O.
    if !account.calendar_sync_enabled {
        log::info!(
            "sync_calendars: skipping account {} (calendar sync disabled)",
            account_id
        );
        return Ok(());
    }

    // When force_full_sync is true (manual Sync button), clear Google/O365
    // sync tokens to force a full sync that reconciles server-side deletions.
    if force_full_sync.unwrap_or(false) {
        let conn = state.db.writer().await;
        // Escape SQL LIKE metacharacters in account_id to prevent
        // unintended pattern matching if the id contains % or _.
        let escaped_id = account_id.replace('%', "\\%").replace('_', "\\_");
        conn.execute(
            "DELETE FROM app_metadata WHERE key LIKE ?1 ESCAPE '\\'",
            rusqlite::params![format!("google_sync_token_{escaped_id}_%")],
        )
        .ok();
        log::info!(
            "sync_calendars: cleared sync tokens for full sync (account={})",
            account_id
        );
    }

    // Tell the frontend a calendar sync has started so the activity panel
    // and the StatusBar Sync button show the spinning indicator — same
    // contract as the mail "sync-started" event.
    use tauri::Emitter;
    app.emit("calendar-sync-started", account_id.as_str()).ok();

    // Per-provider sync (incl. Google's internal CalDAV fallback) lives
    // in the backend impls; see backend/calendar/.
    let sync_result: Result<()> = match crate::backend::calendar::for_account(&account) {
        Some(backend) => backend.sync(&calendar_backend_ctx(&state), &account).await,
        None => {
            log::debug!(
                "sync_calendars: skipping account {} (no calendar backend configured)",
                account_id
            );
            Ok(())
        }
    };

    // "calendar-changed" is emitted in BOTH branches because the lower-level
    // sync helpers can mutate the DB before an error propagates (e.g.
    // sync_calendars_caldav upserts calendars, then a later query errors).
    // Subscribers (invites store, calendar list) would otherwise hold stale
    // data after a partial-write failure. Spinner state is carried by the
    // dedicated "calendar-sync-complete"/"calendar-sync-error" events so
    // "calendar-changed" no longer conflates "sync finished" with "data
    // changed" — and so an invite-response or push-processing emission of
    // "calendar-changed" can't prematurely complete the spinner.
    app.emit("calendar-changed", account_id.as_str()).ok();
    match &sync_result {
        Ok(()) => {
            app.emit("calendar-sync-complete", account_id.as_str()).ok();
        }
        Err(e) => {
            app.emit(
                "calendar-sync-error",
                serde_json::json!({
                    "account_id": account_id.as_str(),
                    "error": e.to_string(),
                }),
            )
            .ok();
        }
    }
    drop(account_guard);

    // Backends queue cleanup inside their reconciliation transactions. Run
    // after backend writers are released and never mask the sync result.
    crate::commands::meet::sweep_cleanup_requested(&state).await;

    sync_result?;

    log::info!("sync_calendars: completed for account {}", account_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Invite handling commands
// ---------------------------------------------------------------------------

const MAX_CALENDAR_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
const MAX_CALENDAR_IMPORT_EVENTS: usize = 1_000;

#[derive(Debug, Serialize)]
pub struct CalendarImportPreview {
    pub uid: String,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_time: String,
    pub end_time: String,
    pub all_day: bool,
    pub timezone: Option<String>,
    pub method: String,
    pub recurrence_kind: RecurrenceKind,
    pub component_count: usize,
    pub organizer_email: Option<String>,
    pub attendee_count: usize,
    pub importable: bool,
    pub import_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CalendarImportResult {
    pub imported: usize,
    pub skipped_existing: usize,
}

fn read_calendar_attachment_groups(
    state: &AppState,
    source_account_id: &str,
    message_id: &str,
    attachment_index: u32,
) -> Result<Vec<ical::IcalEventGroup>> {
    let contents = crate::commands::mail::read_attachment_contents(
        state,
        source_account_id,
        message_id,
        attachment_index,
    )?;
    if contents.len() > MAX_CALENDAR_ATTACHMENT_BYTES {
        return Err(crate::error::Error::Other(format!(
            "Calendar attachments larger than {} MiB cannot be imported",
            MAX_CALENDAR_ATTACHMENT_BYTES / 1024 / 1024
        )));
    }
    let text = String::from_utf8(contents).map_err(|_| {
        crate::error::Error::Other("The calendar attachment is not valid UTF-8".into())
    })?;
    let groups = ical::parse_ical_event_groups(&text).map_err(crate::error::Error::Other)?;
    if groups.len() > MAX_CALENDAR_IMPORT_EVENTS {
        return Err(crate::error::Error::Other(format!(
            "Calendar attachments containing more than {MAX_CALENDAR_IMPORT_EVENTS} events cannot be imported"
        )));
    }
    Ok(groups)
}

fn calendar_backend_for_account(
    conn: &rusqlite::Connection,
    account_id: &str,
) -> Result<Option<&'static dyn CalendarBackend>> {
    let binding = db::service_bindings::list_for_account(conn, account_id)?
        .into_iter()
        .find(|binding| binding.service == "calendar" && binding.enabled);
    let Some(binding) = binding else {
        return Ok(None);
    };
    crate::backend::calendar::for_protocol(&binding.protocol)
        .map(Some)
        .ok_or_else(|| crate::error::Error::UnsupportedCapability {
            protocol: "calendar",
            capability: "configured calendar protocol",
        })
}

fn import_target_error(
    calendar: &db::calendar::Calendar,
    backend: Option<&dyn CalendarBackend>,
) -> Option<String> {
    if !calendar.is_subscribed {
        return Some("The calendar is not subscribed".into());
    }
    match backend {
        Some(backend)
            if backend.event_creation_target() == EventCreationTarget::AccountDefault
                && !calendar.is_default =>
        {
            Some(format!(
                "{} currently supports importing only into its default calendar",
                backend.protocol()
            ))
        }
        Some(backend)
            if backend.event_creation_target() == EventCreationTarget::SelectedCalendar
                && calendar.remote_id.as_deref().is_none_or(str::is_empty) =>
        {
            Some("The calendar has no writable remote identity".into())
        }
        _ => None,
    }
}

fn checked_calendar_import_target(
    conn: &rusqlite::Connection,
    calendar_id: &str,
) -> Result<(db::calendar::Calendar, Option<&'static dyn CalendarBackend>)> {
    let calendar = db::calendar::get_calendar(conn, calendar_id)?;
    if !db::accounts::get_account_full(conn, &calendar.account_id)?.enabled {
        return Err(crate::error::Error::Other(
            "Events cannot be imported into a disabled account".into(),
        ));
    }
    let backend = calendar_backend_for_account(conn, &calendar.account_id)?;
    if let Some(error) = import_target_error(&calendar, backend) {
        return Err(crate::error::Error::Other(error));
    }
    Ok((calendar, backend))
}

fn configured_invite_destination(
    conn: &rusqlite::Connection,
    source_account_id: &str,
) -> Result<Option<(Calendar, db::accounts::AccountFull)>> {
    let Some(calendar_id) =
        db::service_bindings::get_default_import_calendar(conn, source_account_id)?
    else {
        return Ok(None);
    };
    let (calendar, _) = checked_calendar_import_target(conn, &calendar_id)?;
    let account = db::accounts::get_account_full(conn, &calendar.account_id)?;
    Ok(Some((calendar, account)))
}

fn imported_event_for_validation(
    group: &ical::IcalEventGroup,
    calendar: &db::calendar::Calendar,
    source_message_id: &str,
) -> CalendarEvent {
    let event = &group.representative;
    CalendarEvent {
        id: "calendar-import-preview".into(),
        account_id: calendar.account_id.clone(),
        calendar_id: calendar.id.clone(),
        uid: Some(event.uid.clone()),
        title: event.summary.clone().unwrap_or_else(|| "(No title)".into()),
        description: event.description.clone(),
        location: event.location.clone(),
        start_time: event.dtstart.clone(),
        end_time: event.dtend.clone(),
        all_day: event.all_day,
        timezone: event.timezone.clone(),
        recurrence_rule: event.recurrence_rule.clone(),
        recurrence_kind: event.recurrence_kind,
        organizer_email: None,
        attendees_json: None,
        my_status: None,
        source_message_id: Some(source_message_id.into()),
        ical_data: Some(group.ical_raw.clone()),
        remote_id: None,
        etag: None,
    }
}

fn import_group_error(
    group: &ical::IcalEventGroup,
    calendar: &db::calendar::Calendar,
    backend: Option<&dyn CalendarBackend>,
    source_message_id: &str,
) -> Option<String> {
    let method = group.representative.method.to_ascii_uppercase();
    if matches!(method.as_str(), "REPLY" | "CANCEL") {
        return Some(format!(
            "Calendar {method} messages cannot be imported as events"
        ));
    }
    if group.representative.recurrence_kind != RecurrenceKind::Standalone {
        match backend.map(CalendarBackend::recurring_import_fidelity) {
            Some(RecurringImportFidelity::Unsupported) => {
                return Some("This provider cannot preserve recurring imports".into());
            }
            Some(RecurringImportFidelity::PatternedRecurrence)
                if !ical::is_rrule_only_series(&group.ical_raw) =>
            {
                return Some(
                    "This provider cannot preserve recurrence exceptions or additional dates"
                        .into(),
                );
            }
            _ => {}
        }
    }
    let event = imported_event_for_validation(group, calendar, source_message_id);
    backend.and_then(|backend| {
        backend
            .validate_event_creation(&event, calendar.remote_id.as_deref().unwrap_or_default())
            .err()
            .map(|error| error.to_string())
    })
}

#[tauri::command]
pub fn list_calendar_import_targets(
    state: State<'_, AppState>,
) -> Result<Vec<db::calendar::Calendar>> {
    let conn = state.db.reader();
    let mut targets = Vec::new();
    for account in db::accounts::list_accounts(&conn)?
        .into_iter()
        .filter(|account| account.enabled)
    {
        let backend = calendar_backend_for_account(&conn, &account.id)?;
        targets.extend(
            db::calendar::list_calendars(&conn, &account.id)?
                .into_iter()
                .filter(|calendar| import_target_error(calendar, backend).is_none()),
        );
    }
    Ok(targets)
}

#[tauri::command]
pub fn get_default_import_calendar(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Option<String>> {
    let conn = state.db.reader();
    db::service_bindings::get_default_import_calendar(&conn, &account_id)
}

#[tauri::command]
pub async fn set_default_import_calendar(
    state: State<'_, AppState>,
    account_id: String,
    calendar_id: Option<String>,
) -> Result<()> {
    let conn = state.db.writer().await;
    let account = db::accounts::get_account_full(&conn, &account_id)?;
    if !account.enabled || account.mail_binding().is_none() {
        return Err(crate::error::Error::Other(
            "A default import calendar requires an enabled mail account".into(),
        ));
    }
    if let Some(calendar_id) = calendar_id.as_deref() {
        checked_calendar_import_target(&conn, calendar_id)?;
    }
    db::service_bindings::set_default_import_calendar(&conn, &account_id, calendar_id.as_deref())
}

#[tauri::command]
pub async fn preview_calendar_attachment(
    state: State<'_, AppState>,
    source_account_id: String,
    message_id: String,
    attachment_index: u32,
    calendar_id: String,
) -> Result<Vec<CalendarImportPreview>> {
    let (calendar, backend) = {
        let conn = state.db.reader();
        checked_calendar_import_target(&conn, &calendar_id)?
    };
    let groups =
        read_calendar_attachment_groups(&state, &source_account_id, &message_id, attachment_index)?;
    Ok(groups
        .into_iter()
        .map(|group| {
            let import_error = import_group_error(&group, &calendar, backend, &message_id);
            let event = group.representative;
            let method = event.method.to_ascii_uppercase();
            CalendarImportPreview {
                uid: event.uid,
                title: event.summary.unwrap_or_else(|| "(No title)".into()),
                description: event.description,
                location: event.location,
                start_time: event.dtstart,
                end_time: event.dtend,
                all_day: event.all_day,
                timezone: event.timezone,
                importable: import_error.is_none(),
                import_error,
                method,
                recurrence_kind: event.recurrence_kind,
                component_count: group.component_count,
                organizer_email: event.organizer_email,
                attendee_count: event.attendees.len(),
            }
        })
        .collect())
}

#[tauri::command]
pub async fn import_calendar_attachment(
    state: State<'_, AppState>,
    source_account_id: String,
    message_id: String,
    attachment_index: u32,
    calendar_id: String,
    selected_uids: Vec<String>,
) -> Result<CalendarImportResult> {
    let groups =
        read_calendar_attachment_groups(&state, &source_account_id, &message_id, attachment_index)?;
    import_calendar_groups_inner(&state, &message_id, &calendar_id, selected_uids, groups).await
}

async fn import_calendar_groups_inner(
    state: &AppState,
    message_id: &str,
    calendar_id: &str,
    selected_uids: Vec<String>,
    groups: Vec<ical::IcalEventGroup>,
) -> Result<CalendarImportResult> {
    if selected_uids.is_empty() {
        return Err(crate::error::Error::Other(
            "Select at least one event to import".into(),
        ));
    }
    let selected: std::collections::HashSet<&str> =
        selected_uids.iter().map(String::as_str).collect();
    if selected.len() != selected_uids.len() {
        return Err(crate::error::Error::Other(
            "The import selection contains duplicate event UIDs".into(),
        ));
    }

    if selected
        .iter()
        .any(|uid| !groups.iter().any(|group| group.representative.uid == *uid))
    {
        return Err(crate::error::Error::Other(
            "The calendar attachment changed or the selection is invalid".into(),
        ));
    }

    let target_account_id = {
        let conn = state.db.reader();
        db::calendar::get_calendar(&conn, calendar_id)?.account_id
    };
    let account_lock = state.account_lifecycle.acquire(&target_account_id);
    let _account_guard = account_lock.lock().await;

    let (target, backend) = {
        let conn = state.db.reader();
        let (calendar, backend) = checked_calendar_import_target(&conn, calendar_id)?;
        if calendar.account_id != target_account_id {
            return Err(crate::error::Error::Other(
                "The destination calendar changed accounts while waiting to import".into(),
            ));
        }
        (calendar, backend)
    };

    for group in groups
        .iter()
        .filter(|group| selected.contains(group.representative.uid.as_str()))
    {
        if let Some(error) = import_group_error(group, &target, backend, message_id) {
            return Err(crate::error::Error::Other(format!(
                "Cannot import '{}': {error}",
                group
                    .representative
                    .summary
                    .as_deref()
                    .unwrap_or("(No title)")
            )));
        }
    }

    let mut result = CalendarImportResult {
        imported: 0,
        skipped_existing: 0,
    };
    let mut existing_uids = {
        let conn = state.db.reader();
        db::calendar::event_identity_uids(&conn, &target.account_id)?
    };
    for group in groups
        .into_iter()
        .filter(|group| selected.contains(group.representative.uid.as_str()))
    {
        let event = group.representative;
        if existing_uids.contains(&event.uid) {
            result.skipped_existing += 1;
            continue;
        }

        let input = NewEventInput {
            account_id: target.account_id.clone(),
            calendar_id: target.id.clone(),
            title: event.summary.unwrap_or_else(|| "(No title)".into()),
            description: event.description,
            location: event.location,
            start_time: event.dtstart,
            end_time: event.dtend,
            all_day: event.all_day,
            timezone: event.timezone,
            recurrence_rule: event.recurrence_rule,
            // Import is a personal copy, not an RSVP or a new invitation.
            attendees: Vec::new(),
            meet_binding: None,
        };
        let imported_uid = event.uid;
        let metadata = ImportedEventMetadata {
            uid: imported_uid.clone(),
            recurrence_kind: event.recurrence_kind,
            ical_data: group.ical_raw,
            source_message_id: message_id.to_string(),
            organizer_email: None,
            attendees_json: None,
            my_status: None,
            invitation_source: None,
            personal_copy: true,
            require_remote_creation: false,
        };
        if let Err(error) =
            create_event_with_metadata(state, input, None, Some(metadata), true).await
        {
            if result.imported > 0 || result.skipped_existing > 0 {
                return Err(crate::error::Error::Other(format!(
                    "Import stopped after {} event(s) were added and {} existing event(s) were skipped: {}",
                    result.imported, result.skipped_existing, error
                )));
            }
            return Err(error);
        }
        result.imported += 1;
        existing_uids.insert(imported_uid);
    }

    Ok(result)
}

#[tauri::command]
pub async fn get_email_invites(
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
) -> Result<Vec<ParsedInvite>> {
    log::info!(
        "get_email_invites: account={} message={}",
        account_id,
        message_id
    );
    let conn = state.db.reader();

    // Look up the message to get its maildir path
    let (maildir_path, _from_email, _to, _cc, _flags, _encrypted, _signed) =
        db::messages::get_message_metadata(&conn, &account_id, &message_id)?;

    if BodyLocation::from_persisted(&maildir_path).needs_fetch() {
        log::debug!("get_email_invites: message body not fetched yet");
        return Ok(vec![]);
    }

    // Read the raw message from disk (maildir_path is relative to data_dir)
    let full_path = crate::path_validation::resolve_under(&state.data_dir, &maildir_path)?;
    log::debug!("get_email_invites: reading from {}", full_path.display());
    let raw = std::fs::read(&full_path).map_err(|e| {
        crate::error::Error::Other(format!(
            "Failed to read message file '{}': {}",
            full_path.display(),
            e
        ))
    })?;

    let invites = ical::parse_ical_from_email(&raw);
    log::info!(
        "get_email_invites: found {} invites in message {}",
        invites.len(),
        message_id
    );
    Ok(invites)
}

#[tauri::command]
pub async fn get_invite_status(
    state: State<'_, AppState>,
    account_id: String,
    invite_uid: String,
) -> Result<Option<String>> {
    let conn = state.db.reader();
    if let Some(event_id) =
        db::calendar_invitation_source::event_id(&conn, &account_id, &invite_uid)?
    {
        return Ok(db::calendar::get_event(&conn, &event_id)?.my_status);
    }
    let event = db::calendar::get_event_by_uid(&conn, &account_id, &invite_uid)?;
    Ok(event.and_then(|e| e.my_status))
}

#[tauri::command]
pub async fn respond_to_invite(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
    invite_uid: String,
    response: String,
) -> Result<()> {
    log::info!(
        "respond_to_invite: account={} message={} uid={} response={}",
        account_id,
        message_id,
        invite_uid,
        response
    );

    let initial_destination = {
        let conn = state.db.reader();
        configured_invite_destination(&conn, &account_id)?
    };
    let destination_account_id = initial_destination
        .as_ref()
        .map(|(_, account)| account.id.clone())
        .unwrap_or_else(|| account_id.clone());
    let (first_account_id, second_account_id) = if account_id == destination_account_id {
        (account_id.clone(), None)
    } else if account_id < destination_account_id {
        (account_id.clone(), Some(destination_account_id.clone()))
    } else {
        (destination_account_id.clone(), Some(account_id.clone()))
    };
    let first_account_lock = state.account_lifecycle.acquire(&first_account_id);
    let second_account_lock = second_account_id
        .as_deref()
        .map(|id| state.account_lifecycle.acquire(id));
    let _first_account_guard = first_account_lock.lock().await;
    let _second_account_guard = match second_account_lock.as_ref() {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };

    // Re-read every account-owned input after acquiring both lifecycle locks.
    let (raw, account, destination) = {
        let conn = state.db.writer().await;
        let (maildir_path, _from_email, _to, _cc, _flags, _encrypted, _signed) =
            db::messages::get_message_metadata(&conn, &account_id, &message_id)?;

        if BodyLocation::from_persisted(&maildir_path).needs_fetch() {
            return Err(crate::error::Error::Other(
                "Message body not fetched yet".to_string(),
            ));
        }

        let full_path = crate::path_validation::resolve_under(&state.data_dir, &maildir_path)?;
        let raw = std::fs::read(&full_path).map_err(|e| {
            crate::error::Error::Other(format!(
                "Failed to read message file '{}': {}",
                full_path.display(),
                e
            ))
        })?;

        let account = db::accounts::get_account_full(&conn, &account_id)?;
        if !account.enabled || account.mail_binding().is_none() {
            return Err(crate::error::Error::Other(
                "Invitation responses require an enabled source mail account".into(),
            ));
        }
        let destination = configured_invite_destination(&conn, &account_id)?;
        let current_destination_id = destination
            .as_ref()
            .map(|(calendar, _)| calendar.id.as_str());
        let initial_destination_id = initial_destination
            .as_ref()
            .map(|(calendar, _)| calendar.id.as_str());
        if current_destination_id != initial_destination_id {
            return Err(crate::error::Error::Other(
                "The default invitation calendar changed while preparing the response".into(),
            ));
        }
        if destination
            .as_ref()
            .is_some_and(|(_, destination_account)| {
                destination_account.id != destination_account_id
            })
        {
            return Err(crate::error::Error::Other(
                "The invitation calendar changed accounts while preparing the response".into(),
            ));
        }
        (raw, account, destination)
    };

    let invites = ical::parse_ical_from_email(&raw);
    let invite = invites
        .iter()
        .find(|inv| inv.uid == invite_uid)
        .ok_or_else(|| {
            crate::error::Error::Other(format!(
                "Invite with UID '{}' not found in message",
                invite_uid
            ))
        })?;

    if let Some((calendar, destination_account)) = destination.as_ref() {
        let group = ical::parse_ical_event_groups(&invite.ical_raw)
            .map_err(crate::error::Error::Other)?
            .into_iter()
            .find(|group| group.representative.uid == invite_uid)
            .ok_or_else(|| {
                crate::error::Error::Other(
                    "The invitation recurrence group could not be resolved safely".into(),
                )
            })?;
        let backend = crate::backend::calendar::for_account(destination_account);
        if let Some(error) = import_group_error(&group, calendar, backend, &message_id) {
            return Err(crate::error::Error::Other(format!(
                "Cannot add this invitation to the configured calendar: {error}"
            )));
        }
    }

    apply_invite_response(
        &app,
        &state,
        account_id,
        account,
        invite,
        invite_uid,
        response,
        Some(message_id),
        destination,
    )
    .await
}

/// Persist RSVP and conservative recurrence evidence in one transaction.
fn persist_existing_invite_response(
    conn: &rusqlite::Connection,
    existing: &mut CalendarEvent,
    invite: &ParsedInvite,
    status: String,
    attendees_json: Option<String>,
) -> Result<()> {
    // An email may be stale: it can revoke standalone certainty, never restore it.
    // Keep already-protected recurrence evidence until an authoritative refresh.
    if existing.recurrence_kind == RecurrenceKind::Standalone
        && invite.recurrence_kind != RecurrenceKind::Standalone
    {
        existing.recurrence_kind = invite.recurrence_kind;
        existing.recurrence_rule = invite.recurrence_rule.clone();
        existing.ical_data = Some(invite.ical_raw.clone());
    }
    existing.my_status = Some(status);
    existing.attendees_json = attendees_json;

    let transaction = conn.unchecked_transaction()?;
    db::calendar::update_event(&transaction, existing)?;
    // Incoming invitations cannot attest to the completeness of a local series.
    db::calendar_invitation::invalidate(&transaction, &existing.id)?;
    transaction.commit()?;
    Ok(())
}

fn responded_attendees_json(
    invite: &ParsedInvite,
    respondent_email: &str,
    status: &str,
) -> Option<String> {
    if invite.attendees.is_empty() {
        return None;
    }
    let mut attendees = invite.attendees.clone();
    let respondent = attendees
        .iter()
        .position(|attendee| attendee.is_self == Some(true))
        .or_else(|| {
            attendees
                .iter()
                .position(|attendee| attendee.email.eq_ignore_ascii_case(respondent_email))
        });
    if let Some(attendee) = respondent.and_then(|index| attendees.get_mut(index)) {
        attendee.status = status.into();
    }
    serde_json::to_string(&attendees).ok()
}

fn invitation_organizers_match(existing: Option<&str>, incoming: Option<&str>) -> bool {
    match (existing, incoming) {
        (Some(existing), Some(incoming))
            if !existing.trim().is_empty() && !incoming.trim().is_empty() =>
        {
            existing.trim().eq_ignore_ascii_case(incoming.trim())
        }
        _ => false,
    }
}

fn stored_invitation_sequence(event: &CalendarEvent, invitation_uid: &str) -> Result<u32> {
    let raw = event.ical_data.as_deref().ok_or_else(|| {
        crate::error::Error::Other(
            "The existing invitation copy has no source calendar data".into(),
        )
    })?;
    ical::parse_ical_event_groups(raw)
        .map_err(crate::error::Error::Other)?
        .into_iter()
        .find(|group| group.representative.uid == invitation_uid)
        .map(|group| group.representative.sequence)
        .ok_or_else(|| {
            crate::error::Error::Other(
                "The existing invitation copy has mismatched source identity".into(),
            )
        })
}

fn invitation_payload_changed(existing: &CalendarEvent, refreshed: &CalendarEvent) -> bool {
    existing.title != refreshed.title
        || existing.description != refreshed.description
        || existing.location != refreshed.location
        || existing.start_time != refreshed.start_time
        || existing.end_time != refreshed.end_time
        || existing.all_day != refreshed.all_day
        || existing.timezone != refreshed.timezone
        || existing.recurrence_rule != refreshed.recurrence_rule
        || existing.recurrence_kind != refreshed.recurrence_kind
        || existing.organizer_email != refreshed.organizer_email
        || existing.attendees_json != refreshed.attendees_json
        || existing.ical_data != refreshed.ical_data
}

fn refreshed_invitation_copy(
    existing: &CalendarEvent,
    invite_uid: &str,
    invite: &ParsedInvite,
    respondent_email: &str,
    source_message_id: Option<&str>,
) -> Result<CalendarEvent> {
    if !invitation_organizers_match(
        existing.organizer_email.as_deref(),
        invite.organizer_email.as_deref(),
    ) {
        return Err(crate::error::Error::Other(
            "The updated invitation organizer does not match the existing copy".into(),
        ));
    }
    if invite.sequence < stored_invitation_sequence(existing, invite_uid)? {
        return Err(crate::error::Error::Other(
            "The invitation update is older than the existing calendar copy".into(),
        ));
    }

    let mut refreshed = existing.clone();
    refreshed.title = invite
        .summary
        .clone()
        .unwrap_or_else(|| "(No title)".into());
    refreshed.description = invite.description.clone();
    refreshed.location = invite.location.clone();
    refreshed.start_time = invite.dtstart.clone();
    refreshed.end_time = invite.dtend.clone();
    refreshed.all_day = invite.all_day;
    refreshed.timezone = invite.timezone.clone();
    refreshed.recurrence_rule = invite.recurrence_rule.clone();
    refreshed.recurrence_kind = invite.recurrence_kind;
    refreshed.organizer_email = invite.organizer_email.clone();
    refreshed.attendees_json = existing
        .my_status
        .as_deref()
        .map(|status| responded_attendees_json(invite, respondent_email, status))
        .unwrap_or_else(|| serde_json::to_string(&invite.attendees).ok());
    refreshed.source_message_id = source_message_id.map(str::to_owned);
    refreshed.ical_data = Some(invite.ical_raw.clone());
    Ok(refreshed)
}

async fn ensure_cross_account_invitation_copy(
    state: &AppState,
    source_account_id: &str,
    respondent_email: &str,
    source_message_id: Option<&str>,
    invite_uid: &str,
    invite: &ParsedInvite,
    calendar: &Calendar,
    destination_account: &db::accounts::AccountFull,
) -> Result<String> {
    if invite
        .organizer_email
        .as_deref()
        .is_none_or(|organizer| organizer.trim().is_empty())
    {
        return Err(crate::error::Error::Other(
            "A cross-account invitation requires an organizer identity".into(),
        ));
    }
    let existing_event_id = {
        let conn = state.db.reader();
        db::calendar_invitation_source::event_id(&conn, source_account_id, invite_uid)?
    };
    if let Some(event_id) = existing_event_id {
        let (existing, revision, provenance) = {
            let conn = state.db.reader();
            (
                db::calendar::get_event(&conn, &event_id)?,
                db::calendar_revision::get(&conn, &event_id)?,
                db::calendar_invitation_source::get(&conn, &event_id)?.ok_or_else(|| {
                    crate::error::Error::Other(
                        "The existing invitation copy lost its source provenance".into(),
                    )
                })?,
            )
        };
        if existing.account_id != destination_account.id || existing.calendar_id != calendar.id {
            return Err(crate::error::Error::Other(
                "The existing invitation copy no longer matches the configured calendar".into(),
            ));
        }
        if existing
            .remote_id
            .as_deref()
            .is_none_or(|remote_id| remote_id.is_empty())
        {
            return Err(crate::error::Error::Other(
                "The existing invitation copy has no confirmed remote identity".into(),
            ));
        }
        let mut refreshed = refreshed_invitation_copy(
            &existing,
            invite_uid,
            invite,
            respondent_email,
            source_message_id,
        )?;

        if invitation_payload_changed(&existing, &refreshed) {
            let remote_id = existing.remote_id.as_deref().ok_or_else(|| {
                crate::error::Error::Other(
                    "The invitation copy has no remote identity for applying its update".into(),
                )
            })?;
            let backend =
                crate::backend::calendar::for_account(destination_account).ok_or_else(|| {
                    crate::error::Error::Other(
                        "The invitation destination has no calendar provider".into(),
                    )
                })?;
            backend.validate_event_creation(
                &refreshed,
                calendar.remote_id.as_deref().unwrap_or_default(),
            )?;
            refreshed.etag = backend
                .push_updated_invitation_copy(
                    &calendar_backend_ctx(state),
                    destination_account,
                    remote_id,
                    &refreshed,
                )
                .await?;
        }

        let mut conn = state.db.writer().await;
        let transaction = conn.transaction()?;
        if db::calendar_revision::get(&transaction, &event_id)? != revision
            || db::calendar_invitation_source::get(&transaction, &event_id)?.as_ref()
                != Some(&provenance)
        {
            return Err(crate::error::Error::Other(
                "The invitation copy changed while applying its update".into(),
            ));
        }
        db::calendar::update_event(&transaction, &refreshed)?;
        db::calendar_invitation_source::record(
            &transaction,
            &event_id,
            &db::calendar_invitation_source::InvitationSource {
                source_account_id: source_account_id.into(),
                source_message_id: source_message_id
                    .unwrap_or(&provenance.source_message_id)
                    .into(),
                invitation_uid: invite_uid.into(),
            },
        )?;
        transaction.commit()?;
        return Ok(event_id);
    }

    let message_id = source_message_id.ok_or_else(|| {
        crate::error::Error::Other(
            "Cross-account invitation response is missing message provenance".into(),
        )
    })?;
    let input = NewEventInput {
        account_id: destination_account.id.clone(),
        calendar_id: calendar.id.clone(),
        title: invite
            .summary
            .clone()
            .unwrap_or_else(|| "(No title)".into()),
        description: invite.description.clone(),
        location: invite.location.clone(),
        start_time: invite.dtstart.clone(),
        end_time: invite.dtend.clone(),
        all_day: invite.all_day,
        timezone: invite.timezone.clone(),
        recurrence_rule: invite.recurrence_rule.clone(),
        attendees: vec![],
        meet_binding: None,
    };
    let metadata = ImportedEventMetadata {
        uid: invite_uid.into(),
        recurrence_kind: invite.recurrence_kind,
        ical_data: invite.ical_raw.clone(),
        source_message_id: message_id.into(),
        organizer_email: invite.organizer_email.clone(),
        attendees_json: serde_json::to_string(&invite.attendees).ok(),
        my_status: None,
        invitation_source: Some(db::calendar_invitation_source::InvitationSource {
            source_account_id: source_account_id.into(),
            source_message_id: message_id.into(),
            invitation_uid: invite_uid.into(),
        }),
        personal_copy: true,
        require_remote_creation: true,
    };
    Ok(
        create_event_with_metadata(state, input, None, Some(metadata), true)
            .await?
            .event
            .id,
    )
}

/// Deliver an iTIP REPLY for `invite` and persist the RSVP locally.
///
/// Shared by `respond_to_invite` (invite parsed from an email) and
/// `respond_to_event` (invite rebuilt from a stored calendar row). The
/// per-provider routing — JMAP / Graph RSVP / SMTP plus the Google
/// Calendar API path — is identical for both callers. `source_message_id`
/// is recorded only when a brand-new local event row has to be created.
#[allow(clippy::too_many_arguments)]
async fn apply_invite_response(
    app: &tauri::AppHandle,
    state: &State<'_, AppState>,
    account_id: String,
    account: db::accounts::AccountFull,
    invite: &ParsedInvite,
    invite_uid: String,
    response: String,
    source_message_id: Option<String>,
    destination: Option<(Calendar, db::accounts::AccountFull)>,
) -> Result<()> {
    let response = InviteResponse::try_from(response.as_str())?;
    let response_text = response.as_str();
    let backend = crate::backend::calendar::for_account(&account);
    let track_pending_rsvp = backend
        .map(|provider| provider.remote_rsvp_policy() != RemoteRsvpPolicy::RequiredBeforeLocal)
        .unwrap_or(true);
    let remote_request = RemoteRsvpRequest {
        uid: invite_uid.clone(),
        response,
        summary: invite.summary.clone(),
        start_time: invite.dtstart.clone(),
        end_time: invite.dtend.clone(),
        all_day: invite.all_day,
        description: invite.description.clone(),
        location: invite.location.clone(),
        organizer_email: invite.organizer_email.clone(),
        attendees: invite.attendees.clone(),
    };
    let my_status = response_text.to_string();
    let attendees_json = responded_attendees_json(invite, &account.email, &my_status);
    let cross_account_destination = destination
        .as_ref()
        .filter(|(_, destination_account)| destination_account.id != account_id);
    let cross_account_event_id =
        if let Some((calendar, destination_account)) = cross_account_destination {
            Some(
                ensure_cross_account_invitation_copy(
                    state,
                    &account_id,
                    &account.email,
                    source_message_id.as_deref(),
                    &invite_uid,
                    invite,
                    calendar,
                    destination_account,
                )
                .await?,
            )
        } else {
            None
        };

    // Step 2: Generate the iTIP REPLY
    let reply_ical = ical::generate_reply(
        invite,
        &account.email,
        (!account.sender_name.trim().is_empty()).then_some(account.sender_name.as_str()),
        response_text,
    );

    // Step 3: Send the reply to the organizer
    if let Some(ref organizer_email) = invite.organizer_email {
        let subject = format!(
            "Re: {}",
            invite.summary.as_deref().unwrap_or("Calendar Invite")
        );

        // Build an email with the iCal reply as a text/calendar attachment
        let body_text = format!(
            "This is a {} response to the calendar invitation \"{}\".",
            response_text,
            invite.summary.as_deref().unwrap_or("Calendar Invite")
        );

        match backend
            .map(|provider| provider.invite_reply_delivery())
            .unwrap_or(InviteReplyDelivery::Smtp)
        {
            InviteReplyDelivery::JmapSubmission => {
                log::info!("apply_invite_response: sending reply via JMAP");
                let (jmap_config, jmap_conn) = state.providers.jmap_client(&account).await?;

                let raw_message = build_calendar_reply_message(
                    &account.email,
                    &account.sender_name,
                    organizer_email,
                    &subject,
                    &body_text,
                    &reply_ical,
                )?;

                let envelope = crate::mail::jmap::JmapSubmissionEnvelope::new(
                    &account.email,
                    std::slice::from_ref(organizer_email),
                    &[],
                    &[],
                )?;

                jmap_conn
                    .send_email(&jmap_config, &raw_message, &envelope)
                    .await?;
            }
            InviteReplyDelivery::Provider => {
                // O365/Graph accounts have no SMTP host configured. The reply
                // email to the organizer is sent by Microsoft itself via the
                // Graph API RSVP call (`sendResponse: true`) in Step 3b below.
                log::info!(
                    "apply_invite_response: O365 account — reply delivered via Graph API RSVP (Step 3b)"
                );
            }
            InviteReplyDelivery::Smtp => {
                log::info!("apply_invite_response: sending reply via SMTP");
                let raw_message = build_calendar_reply_message(
                    &account.email,
                    &account.sender_name,
                    organizer_email,
                    &subject,
                    &body_text,
                    &reply_ical,
                )?;

                // For O365: refresh SMTP-scoped OAuth token
                let credentials = state
                    .providers
                    .credentials()
                    .mail_credentials(&account)
                    .await?;

                send_raw_smtp(
                    &account.smtp_host,
                    account.smtp_port,
                    &account.username,
                    &credentials.secret,
                    account.use_tls,
                    credentials.use_xoauth2,
                    &account.email,
                    organizer_email,
                    &raw_message,
                )
                .await?;
            }
        }
    } else {
        log::info!("apply_invite_response: no organizer email, skipping send");
    }

    // Step 3b: For O365/Graph accounts, deliver the RSVP to the organizer
    // *before* the local DB write below. The Graph RSVP (`sendResponse:
    // true`) is the only delivery path for these accounts, and Graph locates
    // the event by UID itself (no organizer address needed). Doing it here
    // keeps the operation atomic: a delivery failure returns an error
    // without having marked the invite answered locally — Step 4 (and the
    // remote_id store in Step 6) only run once delivery has succeeded.
    let required_remote_id = if let Some(provider) = backend
        .filter(|provider| provider.remote_rsvp_policy() == RemoteRsvpPolicy::RequiredBeforeLocal)
    {
        match provider
            .apply_remote_rsvp(&calendar_backend_ctx(state), &account, &remote_request)
            .await?
        {
            CalendarCapability::Supported(outcome) => outcome.remote_id,
            CalendarCapability::Unsupported => {
                return Err(crate::error::Error::UnsupportedCapability {
                    protocol: provider.protocol(),
                    capability: "remote calendar RSVP",
                });
            }
        }
    } else {
        None
    };

    if let (Some(responded_event_id), Some((_, destination_account))) =
        (cross_account_event_id, cross_account_destination)
    {
        let conn = state.db.writer().await;
        let mut existing = db::calendar::get_event(&conn, &responded_event_id)?;
        persist_existing_invite_response(&conn, &mut existing, invite, my_status, attendees_json)?;
        conn.execute(
            "UPDATE calendar_events SET pending_rsvp_status = ?1 WHERE id = ?2",
            rusqlite::params![
                track_pending_rsvp.then_some(response_text),
                responded_event_id
            ],
        )?;
        drop(conn);
        use tauri::Emitter as _;
        app.emit("calendar-changed", destination_account.id.as_str())
            .ok();
        return Ok(());
    }

    // Step 4: Create/update event in local calendar
    let conn = state.db.writer().await;

    // Find the best calendar for this account: prefer default, then any with
    // a remote_id (synced from server), then any existing, finally create one.
    let calendars = db::calendar::list_calendars(&conn, &account_id)?;
    let calendar_id = if let Some((calendar, _)) = destination.as_ref() {
        calendar.id.clone()
    } else if let Some(cal) = calendars
        .iter()
        .find(|c| c.is_default && c.remote_id.is_some())
    {
        cal.id.clone()
    } else if let Some(cal) = calendars.iter().find(|c| c.is_default) {
        cal.id.clone()
    } else if let Some(cal) = calendars.iter().find(|c| c.remote_id.is_some()) {
        cal.id.clone()
    } else if let Some(cal) = calendars.first() {
        cal.id.clone()
    } else {
        // No calendars at all — create a default one
        let cal_id = uuid::Uuid::new_v4().to_string();
        let new_cal = NewCalendar {
            account_id: account_id.clone(),
            name: "Calendar".to_string(),
            color: random_calendar_color(),
            is_default: true,
        };
        db::calendar::insert_calendar(&conn, &cal_id, &new_cal)?;
        log::info!(
            "apply_invite_response: created default calendar id={}",
            cal_id
        );
        cal_id
    };

    // Reflect the user's own RSVP in the attendee list. The invite email
    // carries the original "needs-action" PARTSTAT for every attendee, so
    // without this patch the event popup shows "needs-action" next to the
    // user's own name even though they just responded.
    // Check if we already have this event
    let responded_event_id = if let Some(mut existing) =
        db::calendar::get_event_by_uid_and_start(&conn, &account_id, &invite_uid, &invite.dtstart)?
    {
        persist_existing_invite_response(&conn, &mut existing, invite, my_status, attendees_json)?;
        log::info!(
            "apply_invite_response: updated existing event {} status={}",
            existing.id,
            response_text
        );
        existing.id
    } else {
        let event_id = uuid::Uuid::new_v4().to_string();
        let cal_event = CalendarEvent {
            id: event_id.clone(),
            account_id: account_id.clone(),
            calendar_id,
            uid: Some(invite.uid.clone()),
            title: invite
                .summary
                .clone()
                .unwrap_or_else(|| "(No title)".to_string()),
            description: invite.description.clone(),
            location: invite.location.clone(),
            start_time: invite.dtstart.clone(),
            end_time: invite.dtend.clone(),
            all_day: invite.all_day,
            timezone: invite.timezone.clone(),
            recurrence_rule: invite.recurrence_rule.clone(),
            recurrence_kind: invite.recurrence_kind,
            organizer_email: invite.organizer_email.clone(),
            attendees_json,
            my_status: Some(my_status),
            source_message_id: source_message_id.clone(),
            ical_data: Some(invite.ical_raw.clone()),
            remote_id: None,
            etag: None,
        };
        db::calendar::insert_event(&conn, &cal_event)?;
        log::info!(
            "apply_invite_response: created event {} status={}",
            event_id,
            response_text
        );
        event_id
    };

    conn.execute(
        "UPDATE calendar_events SET pending_rsvp_status = ?1 WHERE id = ?2",
        rusqlite::params![
            track_pending_rsvp.then_some(response_text),
            responded_event_id
        ],
    )?;

    // The DB write lock covers only the local update above. Release it before
    // any network I/O below, so calendar reads aren't blocked and so the
    // Google/Graph steps can re-acquire the writer without deadlocking.
    drop(conn);

    // Step 5: Best-effort provider RSVP after local persistence. Google uses
    // this path; failures must not undo SMTP delivery or the local status.
    let best_effort_remote_id = if let Some(provider) = backend
        .filter(|provider| provider.remote_rsvp_policy() == RemoteRsvpPolicy::BestEffortAfterLocal)
    {
        match provider
            .apply_remote_rsvp(&calendar_backend_ctx(state), &account, &remote_request)
            .await
        {
            Ok(CalendarCapability::Supported(outcome)) => outcome.remote_id,
            Ok(CalendarCapability::Unsupported) => {
                log::warn!(
                    "apply_invite_response: {} advertises remote RSVP but returned unsupported",
                    provider.protocol()
                );
                None
            }
            Err(error) => {
                log::warn!(
                    "apply_invite_response: {} remote RSVP failed: {}",
                    provider.protocol(),
                    error
                );
                None
            }
        }
    } else {
        None
    };

    if let Some(remote_id) = best_effort_remote_id {
        let conn = state.db.writer().await;
        conn.execute(
            "UPDATE calendar_events SET remote_id = ?1
             WHERE id = ?2 AND (remote_id IS NULL OR remote_id = '')",
            rusqlite::params![remote_id, responded_event_id],
        )
        .ok();
    }

    // Step 6: Persist the Graph event id. The RSVP itself was already
    // delivered in Step 3b; this only records remote_id on the now-existing
    // local row so process_invite_reply can locate the event later. It is
    // best-effort (`.ok()`) — a failure here doesn't lose the RSVP.
    if let Some(remote_id) = required_remote_id {
        let conn = state.db.writer().await;
        conn.execute(
            "UPDATE calendar_events SET remote_id = ?1 WHERE id = ?2",
            rusqlite::params![remote_id, responded_event_id],
        )
        .ok();
    }

    // Notify frontend that calendar data changed so the UI refreshes
    use tauri::Emitter as _;
    app.emit("calendar-changed", account_id.as_str()).ok();

    Ok(())
}

/// Rebuild a `ParsedInvite` from a stored calendar event so an iTIP REPLY
/// can be generated without the original invite email. Prefers the stored
/// iCalendar payload (most faithful — keeps SEQUENCE, exact organizer and
/// attendee encoding); falls back to synthesizing one from the row columns.
fn event_to_parsed_invite(event: &CalendarEvent, uid: &str) -> ParsedInvite {
    if let Some(ical) = event.ical_data.as_deref() {
        if !ical.trim().is_empty() {
            if let Some(parsed) = ical::parse_ical_data(ical)
                .into_iter()
                .find(|inv| inv.uid == uid)
            {
                return parsed;
            }
        }
    }

    let attendees: Vec<Attendee> = event
        .attendees_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();

    ParsedInvite {
        method: "REQUEST".to_string(),
        uid: uid.to_string(),
        summary: Some(event.title.clone()),
        description: event.description.clone(),
        location: event.location.clone(),
        dtstart: event.start_time.clone(),
        dtend: event.end_time.clone(),
        all_day: event.all_day,
        timezone: event.timezone.clone(),
        organizer_email: event.organizer_email.clone(),
        organizer_name: None,
        attendees,
        recurrence_rule: event.recurrence_rule.clone(),
        recurrence_kind: event.recurrence_kind,
        sequence: 0,
        ical_raw: event.ical_data.clone().unwrap_or_default(),
    }
}

/// Respond to a calendar invite from a stored event row — backs the
/// dedicated Invites view. Unlike `respond_to_invite`, no original invite
/// email is needed: the iTIP REPLY is rebuilt from the persisted event.
/// RSVP delivery and local persistence are shared via `apply_invite_response`.
#[tauri::command]
pub async fn respond_to_event(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    event_id: String,
    response: String,
) -> Result<()> {
    log::info!(
        "respond_to_event: account={} event={} response={}",
        account_id,
        event_id,
        response
    );

    let initial_source_account_id = {
        let conn = state.db.reader();
        let event = db::calendar::get_event(&conn, &event_id)?;
        if event.account_id != account_id {
            return Err(crate::error::Error::Other(
                "Event does not belong to the specified account".into(),
            ));
        }
        let provenance = db::calendar_invitation_source::get(&conn, &event_id)?;
        provenance
            .as_ref()
            .map(|source| source.source_account_id.clone())
            .unwrap_or_else(|| account_id.clone())
    };
    let (first_account_id, second_account_id) = if account_id == initial_source_account_id {
        (account_id.clone(), None)
    } else if account_id < initial_source_account_id {
        (account_id.clone(), Some(initial_source_account_id.clone()))
    } else {
        (initial_source_account_id.clone(), Some(account_id.clone()))
    };
    let first_account_lock = state.account_lifecycle.acquire(&first_account_id);
    let second_account_lock = second_account_id
        .as_deref()
        .map(|id| state.account_lifecycle.acquire(id));
    let _first_account_guard = first_account_lock.lock().await;
    let _second_account_guard = match second_account_lock.as_ref() {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };

    let (event, source_account, destination, provenance) = {
        let conn = state.db.reader();
        let event = db::calendar::get_event(&conn, &event_id)?;
        if event.account_id != account_id {
            return Err(crate::error::Error::Other(
                "Event ownership changed while preparing the response".into(),
            ));
        }
        let provenance = db::calendar_invitation_source::get(&conn, &event_id)?;
        let source_account_id = provenance
            .as_ref()
            .map(|source| source.source_account_id.as_str())
            .unwrap_or(&account_id);
        if source_account_id != initial_source_account_id {
            return Err(crate::error::Error::Other(
                "Invitation provenance changed while preparing the response".into(),
            ));
        }
        let source_account = db::accounts::get_account_full(&conn, source_account_id)?;
        if !source_account.enabled || source_account.mail_binding().is_none() {
            return Err(crate::error::Error::Other(
                "Invitation responses require an enabled source mail account".into(),
            ));
        }
        let destination_account = db::accounts::get_account_full(&conn, &event.account_id)?;
        let calendar = db::calendar::get_calendar(&conn, &event.calendar_id)?;
        (
            event,
            source_account,
            Some((calendar, destination_account)),
            provenance,
        )
    };

    let uid = provenance
        .as_ref()
        .map(|source| source.invitation_uid.clone())
        .or_else(|| event.uid.clone())
        .ok_or_else(|| {
            crate::error::Error::Other("Event has no UID; cannot send an RSVP".to_string())
        })?;

    let invite = event_to_parsed_invite(&event, &uid);
    let source_message_id = provenance
        .as_ref()
        .map(|source| source.source_message_id.clone())
        .or_else(|| event.source_message_id.clone());
    let source_account_id = source_account.id.clone();

    apply_invite_response(
        &app,
        &state,
        source_account_id,
        source_account,
        &invite,
        uid,
        response,
        source_message_id,
        destination,
    )
    .await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Pick a random color from a curated palette for new calendars.
pub fn random_calendar_color() -> String {
    let colors = [
        "#4285f4", // Google Blue
        "#0b8043", // Green
        "#8e24aa", // Purple
        "#d50000", // Red
        "#f4511e", // Orange
        "#039be5", // Cyan
        "#616161", // Grey
        "#e67c73", // Salmon
        "#f6bf26", // Yellow
        "#33b679", // Teal
    ];
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as usize;
    colors[seed % colors.len()].to_string()
}

/// Build a raw RFC5322 message with a text/calendar MIME part for an iTIP REPLY.
fn build_calendar_reply_message(
    from: &str,
    sender_name: &str,
    to: &str,
    subject: &str,
    body_text: &str,
    ical_reply: &str,
) -> Result<Vec<u8>> {
    use lettre::message::{header::ContentType, MultiPart, SinglePart};
    use lettre::Message;

    let from_mailbox = crate::mail::smtp::sender_mailbox(from, sender_name)
        .map_err(|_| crate::error::Error::Other("Invalid calendar reply sender".into()))?;
    let to_mailbox = crate::mail::smtp::parse_mailbox(to)
        .map_err(|_| crate::error::Error::Other("Invalid calendar reply recipient".into()))?;

    let message = Message::builder()
        .from(from_mailbox)
        .to(to_mailbox)
        .subject(subject)
        .multipart(
            MultiPart::mixed()
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_PLAIN)
                        .body(body_text.to_string()),
                )
                .singlepart(
                    SinglePart::builder()
                        .header(
                            ContentType::parse("text/calendar; method=REPLY; charset=UTF-8")
                                .unwrap_or(ContentType::TEXT_PLAIN),
                        )
                        .body(ical_reply.to_string()),
                ),
        )
        .map_err(|e| {
            crate::error::Error::Other(format!("Failed to build calendar reply message: {}", e))
        })?;

    Ok(message.formatted())
}

/// Send a pre-built raw message via SMTP, with XOAUTH2 support for O365.
async fn send_raw_smtp(
    smtp_host: &str,
    smtp_port: u16,
    username: &str,
    password: &str,
    use_tls: bool,
    use_xoauth2: bool,
    from: &str,
    to: &str,
    raw_message: &[u8],
) -> Result<()> {
    let recipients = [to.to_string()];
    crate::mail::smtp::send_raw(
        smtp_host,
        smtp_port,
        username,
        password,
        use_tls,
        use_xoauth2,
        from,
        &recipients,
        &[],
        &[],
        raw_message,
    )
    .await
}

/// Send meeting invite emails to attendees for a calendar event.
#[tauri::command]
pub async fn send_invites(
    state: State<'_, AppState>,
    account_id: String,
    event_id: String,
    attendee_emails: Vec<String>,
) -> Result<()> {
    send_invites_inner(&state, account_id, event_id, attendee_emails).await
}

/// Notifications attached to ordinary edits/moves never inherit the series
/// creation exemption, even if the renderer is using stale event metadata.
#[tauri::command]
pub async fn notify_calendar_event(state: State<'_, AppState>, event_id: String) -> Result<()> {
    notify_calendar_event_inner(&state, event_id).await
}

#[derive(Clone, Copy)]
enum InvitationPurpose {
    Creation,
    MutationNotification,
}

/// Creation invitations require stored proof that the supported RRULE is the
/// entire recurrence definition, not just a Series classification. Ordinary
/// editing notifications remain standalone-only.
fn checked_invitation_target(
    conn: &rusqlite::Connection,
    account_id: &str,
    event_id: &str,
    purpose: InvitationPurpose,
) -> Result<CalendarEvent> {
    let mut event = db::calendar::get_event(conn, event_id)?;
    if event.account_id != account_id {
        return Err(crate::error::Error::Other(
            "Calendar event belongs to another account.".into(),
        ));
    }
    if matches!(purpose, InvitationPurpose::Creation)
        && event.recurrence_kind == RecurrenceKind::Series
    {
        event.recurrence_rule = Some(db::calendar_invitation::validated_series_rule(
            conn, &event,
        )?);
    } else {
        event.ensure_mutable()?;
    }
    Ok(event)
}

async fn send_invites_inner(
    state: &AppState,
    account_id: String,
    event_id: String,
    attendee_emails: Vec<String>,
) -> Result<()> {
    deliver_invites(
        state,
        event_id,
        Some((account_id, attendee_emails)),
        InvitationPurpose::Creation,
    )
    .await
}

async fn notify_calendar_event_inner(state: &AppState, event_id: String) -> Result<()> {
    deliver_invites(
        state,
        event_id,
        None,
        InvitationPurpose::MutationNotification,
    )
    .await
}

fn checked_delivery_snapshot(
    conn: &rusqlite::Connection,
    expected: &CalendarEvent,
    purpose: InvitationPurpose,
) -> Result<()> {
    let current = checked_invitation_target(conn, &expected.account_id, &expected.id, purpose)?;
    if current != *expected {
        return Err(crate::error::Error::Other(
            "Calendar event changed while preparing the notification. Refresh before trying again."
                .into(),
        ));
    }
    Ok(())
}

/// Recheck after asynchronous credential/session preparation and before each
/// transport submission. Never hold a database reader/writer across network I/O.
async fn prepare_invitation_transport<T>(
    state: &AppState,
    event: &CalendarEvent,
    purpose: InvitationPurpose,
    preparation: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    checked_delivery_snapshot(&state.db.reader(), event, purpose)?;
    let transport = preparation.await?;
    checked_delivery_snapshot(&state.db.reader(), event, purpose)?;
    Ok(transport)
}

fn checked_invitation_snapshot(
    conn: &rusqlite::Connection,
    event_id: &str,
    creation: Option<(String, Vec<String>)>,
    purpose: InvitationPurpose,
) -> Result<(db::accounts::AccountFull, CalendarEvent, Vec<Attendee>)> {
    let stored = db::calendar::get_event(conn, event_id)?;
    let account_id = creation
        .as_ref()
        .map(|(id, _)| id)
        .unwrap_or(&stored.account_id);
    let evt = checked_invitation_target(conn, account_id, event_id, purpose)?;
    let acc = db::accounts::get_account_full(conn, account_id)?;
    let attendees = if let Some((_, emails)) = creation {
        emails
            .into_iter()
            .map(|email| Attendee {
                email,
                name: None,
                status: "needs-action".into(),
                is_self: None,
            })
            .collect::<Vec<_>>()
    } else {
        if !evt
            .organizer_email
            .as_deref()
            .is_some_and(|email| email.eq_ignore_ascii_case(&acc.email))
        {
            return Err(crate::error::Error::Other(
                "Only the event organizer can notify attendees.".into(),
            ));
        }
        serde_json::from_str::<Vec<Attendee>>(evt.attendees_json.as_deref().unwrap_or("[]"))
            .map_err(|error| {
                crate::error::Error::Other(format!("Invalid stored attendees: {error}"))
            })?
    };
    Ok((acc, evt, attendees))
}

async fn deliver_invites(
    state: &AppState,
    event_id: String,
    creation: Option<(String, Vec<String>)>,
    purpose: InvitationPurpose,
) -> Result<()> {
    let (account, event, attendees) =
        checked_invitation_snapshot(&state.db.reader(), &event_id, creation, purpose)?;
    let attendee_emails: Vec<String> = attendees.iter().map(|a| a.email.clone()).collect();

    // Gmail and O365 handle sending invite emails server-side when
    // events are pushed via Google Calendar API (sendUpdates=all) or
    // Graph API. Sending our own SMTP invite would create duplicates.
    if account.calendar_protocol_str() == "google" || account.calendar_protocol_str() == "graph" {
        log::info!(
            "send_invites: skipping manual send for {} account (server handles invites)",
            account.calendar_protocol_str()
        );
        // Provider-delegated ordinary notifications are read-only as well.
        let conn = state.db.writer().await;
        checked_delivery_snapshot(&conn, &event, purpose)?;
        if matches!(purpose, InvitationPurpose::MutationNotification) {
            return Ok(());
        }
        let attendees_json = serde_json::to_string(&attendees).unwrap_or_default();
        conn.execute(
            "UPDATE calendar_events SET attendees_json = ?1 WHERE id = ?2",
            rusqlite::params![attendees_json, event_id],
        )
        .ok();
        return Ok(());
    }

    let uid = event.uid.as_deref().unwrap_or(&event_id);
    let ical = ical::generate_invite(
        uid,
        &event.title,
        &event.start_time,
        &event.end_time,
        event.location.as_deref(),
        event.description.as_deref(),
        &account.email,
        (!account.sender_name.trim().is_empty()).then_some(account.sender_name.as_str()),
        &attendees,
        event.recurrence_rule.as_deref(),
        if event.all_day {
            None
        } else {
            event.timezone.as_deref()
        },
    );

    let subject = format!("Invitation: {}", event.title);
    let body_text = format!(
        "You have been invited to: {}\nWhen: {} - {}\n{}",
        event.title,
        event.start_time,
        event.end_time,
        event
            .location
            .as_deref()
            .map(|l| format!("Where: {}\n", l))
            .unwrap_or_default()
    );

    for attendee_email in &attendee_emails {
        let raw = build_invite_message(
            &account.email,
            &account.sender_name,
            attendee_email,
            &subject,
            &body_text,
            &ical,
        );

        if raw.is_empty() {
            log::error!(
                "send_invites: failed to build invite message for {}",
                attendee_email
            );
            continue;
        }

        if account.calendar_protocol_str() == "jmap" {
            let envelope = crate::mail::jmap::JmapSubmissionEnvelope::new(
                &account.email,
                std::slice::from_ref(attendee_email),
                &[],
                &[],
            )?;
            let (jmap_config, conn_jmap) = prepare_invitation_transport(
                state,
                &event,
                purpose,
                state.providers.jmap_client(&account),
            )
            .await?;
            conn_jmap.send_email(&jmap_config, &raw, &envelope).await?;
        } else {
            let credentials = prepare_invitation_transport(
                state,
                &event,
                purpose,
                state.providers.credentials().mail_credentials(&account),
            )
            .await?;
            send_raw_smtp(
                &account.smtp_host,
                account.smtp_port,
                &account.username,
                &credentials.secret,
                account.use_tls,
                credentials.use_xoauth2,
                &account.email,
                attendee_email,
                &raw,
            )
            .await?;
        }
        log::info!("send_invites: sent to {}", attendee_email);
    }

    // Only explicit creation invitations change stored attendees.
    if matches!(purpose, InvitationPurpose::Creation) {
        let conn = state.db.writer().await;
        checked_delivery_snapshot(&conn, &event, purpose)?;
        let attendees_json = serde_json::to_string(&attendees).unwrap_or_default();
        conn.execute(
            "UPDATE calendar_events SET attendees_json = ?1 WHERE id = ?2",
            rusqlite::params![attendees_json, event_id],
        )
        .ok();
    }

    log::info!("send_invites: all invites sent for event {}", event_id);
    Ok(())
}

/// Process an incoming iTIP REPLY to update attendee status on the organizer's event.
/// Called when the organizer receives a METHOD:REPLY email.
#[tauri::command]
pub async fn process_invite_reply(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
) -> Result<()> {
    log::info!(
        "process_invite_reply: account={} message={}",
        account_id,
        message_id
    );

    let raw = {
        let conn = state.db.writer().await;
        let (maildir_path, _, _, _, _, _, _) =
            db::messages::get_message_metadata(&conn, &account_id, &message_id)?;
        if BodyLocation::from_persisted(&maildir_path).needs_fetch() {
            return Err(crate::error::Error::Other(
                "Message body not fetched yet".to_string(),
            ));
        }
        let full_path = crate::path_validation::resolve_under(&state.data_dir, &maildir_path)?;
        std::fs::read(&full_path)
            .map_err(|e| crate::error::Error::Other(format!("Failed to read message: {}", e)))?
    };

    let replies = ical::parse_ical_from_email(&raw);
    let reply_invites: Vec<_> = replies
        .iter()
        .filter(|inv| inv.method.to_uppercase() == "REPLY")
        .collect();

    if reply_invites.is_empty() {
        log::debug!("process_invite_reply: no METHOD:REPLY found in message");
        return Ok(());
    }

    // Phase 1: update local DB and collect provider-neutral remote updates.
    // The writer guard is dropped before backend network activity.
    let (account, attendee_updates) = {
        let conn = state.db.writer().await;
        let account = db::accounts::get_account_full(&conn, &account_id)?;
        let mut attendee_updates = Vec::new();

        for reply in &reply_invites {
            let event = db::calendar::get_event_by_uid(&conn, &account_id, &reply.uid)?;
            let Some(event) = event else {
                log::debug!("process_invite_reply: no local event for UID {}", reply.uid);
                continue;
            };

            for attendee in &reply.attendees {
                let status = &attendee.status;
                log::info!(
                    "process_invite_reply: {} responded '{}' to event '{}'",
                    attendee.email,
                    status,
                    event.title
                );

                if let Some(ref att_json) = event.attendees_json {
                    if let Ok(mut attendees) =
                        serde_json::from_str::<Vec<serde_json::Value>>(att_json)
                    {
                        for att in attendees.iter_mut() {
                            if att["email"].as_str() == Some(&attendee.email) {
                                att["status"] = serde_json::json!(status);
                            }
                        }
                        let updated_json = serde_json::to_string(&attendees).unwrap_or_default();
                        conn.execute(
                            "UPDATE calendar_events SET attendees_json = ?1 WHERE id = ?2",
                            rusqlite::params![updated_json, event.id],
                        )
                        .ok();
                    }
                }

                if let Some(ref remote_id) = event.remote_id {
                    attendee_updates.push(AttendeeResponseUpdate {
                        remote_id: remote_id.clone(),
                        attendee_email: attendee.email.clone(),
                        response: status.clone(),
                    });
                }
            }
        }
        (account, attendee_updates)
    };

    // Phase 2: let the provider apply any supported remote participant update
    // without holding the DB writer. JMAP preserves its fetch-once behavior;
    // other providers explicitly report this capability as unsupported.
    if !attendee_updates.is_empty() {
        if let Some(backend) = crate::backend::calendar::for_account(&account) {
            if let CalendarCapability::Unsupported = backend
                .push_attendee_responses(&calendar_backend_ctx(&state), &account, &attendee_updates)
                .await?
            {
                log::debug!(
                    "process_invite_reply: {} backend does not push attendee responses",
                    backend.protocol()
                );
            }
        }
    }

    // Notify frontend to refresh calendar UI. Runs regardless of whether
    // a JMAP push happened, since phase 1 always updated the local DB.
    use tauri::Emitter as _;
    app.emit("calendar-changed", account_id.as_str()).ok();

    log::info!("process_invite_reply: completed for account {}", account_id);
    Ok(())
}

#[derive(Debug)]
enum ProvenanceCancellation {
    NotFound,
    Ignored,
    Deleted {
        destination_account_id: String,
        cleanup_ids: Vec<String>,
    },
}

async fn delete_provenance_invitation_copy(
    state: &AppState,
    source_account_id: &str,
    cancel: &ParsedInvite,
) -> Result<ProvenanceCancellation> {
    if matches!(
        cancel.recurrence_kind,
        RecurrenceKind::Occurrence | RecurrenceKind::Unknown
    ) {
        log::warn!(
            "Ignoring unsupported occurrence or unclassified cancellation for UID {}",
            cancel.uid
        );
        return Ok(ProvenanceCancellation::Ignored);
    }
    let (event_id, initial_destination_account_id) = {
        let conn = state.db.reader();
        let Some(event_id) =
            db::calendar_invitation_source::event_id(&conn, source_account_id, &cancel.uid)?
        else {
            return Ok(ProvenanceCancellation::NotFound);
        };
        let event = db::calendar::get_event(&conn, &event_id)?;
        (event_id, event.account_id)
    };
    let (first_account_id, second_account_id) =
        if source_account_id == initial_destination_account_id {
            (source_account_id.to_owned(), None)
        } else if source_account_id < initial_destination_account_id.as_str() {
            (
                source_account_id.to_owned(),
                Some(initial_destination_account_id.clone()),
            )
        } else {
            (
                initial_destination_account_id.clone(),
                Some(source_account_id.to_owned()),
            )
        };
    let first_account_lock = state.account_lifecycle.acquire(&first_account_id);
    let second_account_lock = second_account_id
        .as_deref()
        .map(|id| state.account_lifecycle.acquire(id));
    let _first_account_guard = first_account_lock.lock().await;
    let _second_account_guard = match second_account_lock.as_ref() {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };

    let (event, revision, provenance, destination_account, remote_calendar_id) = {
        let conn = state.db.reader();
        let event = db::calendar::get_event(&conn, &event_id)?;
        let provenance =
            db::calendar_invitation_source::get(&conn, &event_id)?.ok_or_else(|| {
                crate::error::Error::Other(
                    "The invitation copy lost its provenance during cancellation".into(),
                )
            })?;
        if provenance.source_account_id != source_account_id
            || provenance.invitation_uid != cancel.uid
            || event.account_id != initial_destination_account_id
        {
            return Err(crate::error::Error::Other(
                "The invitation copy changed while preparing cancellation".into(),
            ));
        }
        if !invitation_organizers_match(
            event.organizer_email.as_deref(),
            cancel.organizer_email.as_deref(),
        ) {
            log::warn!(
                "Ignoring cancellation with mismatched organizer for UID {}",
                cancel.uid
            );
            return Ok(ProvenanceCancellation::Ignored);
        }
        if cancel.sequence < stored_invitation_sequence(&event, &cancel.uid)? {
            log::warn!("Ignoring stale cancellation for UID {}", cancel.uid);
            return Ok(ProvenanceCancellation::Ignored);
        }
        let destination_account = db::accounts::get_account_full(&conn, &event.account_id)?;
        let remote_calendar_id = db::calendar::get_calendar(&conn, &event.calendar_id)?
            .remote_id
            .unwrap_or_else(|| "primary".into());
        (
            event,
            db::calendar_revision::get(&conn, &event_id)?,
            provenance,
            destination_account,
            remote_calendar_id,
        )
    };
    let remote_id = event.remote_id.as_deref().ok_or_else(|| {
        crate::error::Error::Other(
            "The invitation copy has no remote identity for cancellation".into(),
        )
    })?;
    let backend = crate::backend::calendar::for_account(&destination_account).ok_or_else(|| {
        crate::error::Error::Other("The invitation destination has no calendar provider".into())
    })?;
    backend
        .push_deleted_event(
            &calendar_backend_ctx(state),
            &destination_account,
            remote_id,
            &remote_calendar_id,
        )
        .await?;

    let mut conn = state.db.writer().await;
    let transaction = conn.transaction()?;
    if db::calendar_revision::get(&transaction, &event_id)? != revision
        || db::calendar_invitation_source::get(&transaction, &event_id)?.as_ref()
            != Some(&provenance)
    {
        return Err(crate::error::Error::Other(
            "The invitation copy changed while completing cancellation".into(),
        ));
    }
    let cleanup_ids =
        db::calendar_event_deletion::delete_event(&transaction, &event_id)?.cleanup_lifecycle_ids;
    transaction.commit()?;
    Ok(ProvenanceCancellation::Deleted {
        destination_account_id: destination_account.id,
        cleanup_ids,
    })
}

/// Process a METHOD:CANCEL email and remove its matching calendar copy.
#[tauri::command]
pub async fn process_cancelled_invite(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
) -> Result<()> {
    log::info!(
        "process_cancelled_invite: account={} message={}",
        account_id,
        message_id
    );

    let raw = {
        let conn = state.db.reader();
        let (maildir_path, _, _, _, _, _, _) =
            db::messages::get_message_metadata(&conn, &account_id, &message_id)?;
        if BodyLocation::from_persisted(&maildir_path).needs_fetch() {
            return Err(crate::error::Error::Other(
                "Message body not fetched yet".to_string(),
            ));
        }
        let full_path = crate::path_validation::resolve_under(&state.data_dir, &maildir_path)?;
        std::fs::read(&full_path)
            .map_err(|e| crate::error::Error::Other(format!("Failed to read message: {}", e)))?
    };

    let invites = ical::parse_ical_from_email(&raw);
    let cancels: Vec<_> = invites
        .iter()
        .filter(|inv| inv.method.to_uppercase() == "CANCEL")
        .collect();

    if cancels.is_empty() {
        log::debug!("process_cancelled_invite: no METHOD:CANCEL found");
        return Ok(());
    }

    let mut deleted = 0;
    let mut cleanup_ids = Vec::new();
    let mut changed_accounts = std::collections::HashSet::new();
    for cancel in &cancels {
        match delete_provenance_invitation_copy(&state, &account_id, cancel).await? {
            ProvenanceCancellation::Deleted {
                destination_account_id,
                cleanup_ids: mut event_cleanup_ids,
            } => {
                cleanup_ids.append(&mut event_cleanup_ids);
                changed_accounts.insert(destination_account_id);
                deleted += 1;
                continue;
            }
            ProvenanceCancellation::Ignored => continue,
            ProvenanceCancellation::NotFound => {}
        }
        let mut conn = state.db.writer().await;
        if let Some(event) = db::calendar::get_event_by_uid(&conn, &account_id, &cancel.uid)? {
            // Verify the CANCEL's organizer matches the event's organizer to
            // prevent spoofed CANCEL emails from deleting events.
            if !invitation_organizers_match(
                event.organizer_email.as_deref(),
                cancel.organizer_email.as_deref(),
            ) {
                log::warn!(
                    "process_cancelled_invite: missing or mismatched organizer for UID={}, skipping",
                    cancel.uid
                );
                continue;
            }
            let transaction = conn.transaction()?;
            cleanup_ids.extend(
                db::calendar_event_deletion::delete_event(&transaction, &event.id)?
                    .cleanup_lifecycle_ids,
            );
            transaction.commit()?;
            deleted += 1;
            changed_accounts.insert(event.account_id.clone());
            log::info!(
                "process_cancelled_invite: deleted event '{}' (UID={})",
                event.title,
                cancel.uid
            );
        }
    }
    crate::commands::meet::sweep_pending(&state, cleanup_ids).await;

    if deleted > 0 {
        use tauri::Emitter as _;
        for changed_account in changed_accounts {
            app.emit("calendar-changed", changed_account).ok();
        }
    }

    log::info!(
        "process_cancelled_invite: completed for account {}",
        account_id
    );
    Ok(())
}

fn build_invite_message(
    from: &str,
    sender_name: &str,
    to: &str,
    subject: &str,
    body_text: &str,
    ical_data: &str,
) -> Vec<u8> {
    use lettre::message::{header::ContentType, MultiPart, SinglePart};
    use lettre::Message;

    let from_mailbox = match crate::mail::smtp::sender_mailbox(from, sender_name) {
        Ok(m) => m,
        Err(_) => {
            log::error!("build_invite_message: invalid sender");
            return Vec::new();
        }
    };
    let to_mailbox = match crate::mail::smtp::parse_mailbox(to) {
        Ok(m) => m,
        Err(_) => {
            log::error!("build_invite_message: invalid recipient");
            return Vec::new();
        }
    };

    match Message::builder()
        .from(from_mailbox)
        .to(to_mailbox)
        .subject(subject)
        .multipart(
            MultiPart::mixed()
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_PLAIN)
                        .body(body_text.to_string()),
                )
                .singlepart(
                    SinglePart::builder()
                        .header(
                            ContentType::parse("text/calendar; method=REQUEST; charset=UTF-8")
                                .unwrap_or(ContentType::TEXT_PLAIN),
                        )
                        .body(ical_data.to_string()),
                ),
        ) {
        Ok(msg) => msg.formatted(),
        Err(e) => {
            log::error!("build_invite_message: failed to build message: {}", e);
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Microsoft Graph calendar sync
// ---------------------------------------------------------------------------

/// Return all IANA timezone names from the chrono-tz database.
#[tauri::command]
pub fn list_timezones() -> Vec<String> {
    let mut tzs: Vec<String> = chrono_tz::TZ_VARIANTS
        .iter()
        .map(|tz| tz.name().to_string())
        .collect();
    tzs.sort();
    tzs
}

/// Return the OS timezone, falling back to "UTC".
#[tauri::command]
pub fn get_default_timezone() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string())
}

#[cfg(test)]
mod occurrence_safety_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::recurrence_identity::{
        RecurrenceIdentity, RecurrenceIdentitySeed, RecurrenceValueType,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Copy)]
    enum OccurrenceBackendMode {
        Success,
        SparsePatch,
        Unsupported,
        RemoteFailure,
        ImmutableMismatch,
        LocalRace,
        UnrelatedCanonicalSet,
        DuplicatePositionCanonicalSet,
        DuplicateOccurrenceCanonicalSet,
        MissingSelectedCanonicalSet,
        DetachedCanonicalSet,
    }

    struct MockOccurrenceBackend {
        calls: AtomicUsize,
        mode: OccurrenceBackendMode,
        protocol: &'static str,
    }

    #[async_trait::async_trait]
    impl CalendarBackend for MockOccurrenceBackend {
        fn protocol(&self) -> &'static str {
            self.protocol
        }

        async fn sync(
            &self,
            _ctx: &CalendarBackendCtx<'_>,
            _account: &db::accounts::AccountFull,
        ) -> Result<()> {
            unreachable!()
        }

        fn validate_event_creation(
            &self,
            _event: &CalendarEvent,
            _remote_calendar_id: &str,
        ) -> Result<()> {
            unreachable!()
        }

        async fn push_created_event(
            &self,
            _ctx: &CalendarBackendCtx<'_>,
            _account: &db::accounts::AccountFull,
            _event: &CalendarEvent,
            _remote_calendar_id: &str,
        ) -> Result<Option<crate::backend::calendar::PushedEvent>> {
            unreachable!()
        }

        async fn push_deleted_event(
            &self,
            _ctx: &CalendarBackendCtx<'_>,
            _account: &db::accounts::AccountFull,
            _remote_id: &str,
            _remote_calendar_id: &str,
        ) -> Result<()> {
            unreachable!()
        }

        async fn push_calendar_rename(
            &self,
            _ctx: &CalendarBackendCtx<'_>,
            _account: &db::accounts::AccountFull,
            _remote_id: &str,
            _name: &str,
        ) -> Result<()> {
            unreachable!()
        }

        async fn push_calendar_color(
            &self,
            _ctx: &CalendarBackendCtx<'_>,
            _account: &db::accounts::AccountFull,
            _remote_id: &str,
            _color: &str,
        ) -> Result<()> {
            unreachable!()
        }

        async fn update_recurrence_occurrence(
            &self,
            ctx: &CalendarBackendCtx<'_>,
            _account: &db::accounts::AccountFull,
            request: &RemoteOccurrenceUpdate,
        ) -> Result<RemoteOccurrenceUpdateOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if matches!(self.mode, OccurrenceBackendMode::Unsupported) {
                return Err(crate::error::Error::UnsupportedCapability {
                    protocol: self.protocol(),
                    capability: "THIS-OCCURRENCE update",
                });
            }
            if matches!(self.mode, OccurrenceBackendMode::RemoteFailure) {
                return Err(crate::error::Error::Other("injected remote failure".into()));
            }
            if matches!(self.mode, OccurrenceBackendMode::SparsePatch) {
                assert_eq!(
                    request.patch,
                    UpdateOccurrenceInput {
                        title: Some("Sparse title".into()),
                        timezone: Some("Europe/Paris".into()),
                        ..Default::default()
                    }
                );
            }
            if matches!(self.mode, OccurrenceBackendMode::LocalRace) {
                ctx.db.writer().await.execute(
                    "UPDATE calendar_events SET title = title WHERE id = ?1",
                    [&request.current_event.id],
                )?;
            }
            let identity = &request.trusted_identity;
            let mut replacement = RecurrenceIdentitySeed {
                local_series_event_id: identity.local_series_event_id.clone(),
                provider_calendar_id: identity.provider_calendar_id.clone(),
                provider_series_id: identity.provider_series_id.clone(),
                provider_occurrence_id: identity.provider_occurrence_id.clone(),
                recurrence_id: identity.recurrence_id.clone(),
                recurrence_timezone: identity.recurrence_timezone.clone(),
                recurrence_value_type: identity.recurrence_value_type,
                occurrence: request.desired.clone(),
                provider_native_data: Some("replacement-native-data".into()),
                provider_revision: Some("replacement-revision".into()),
                kind: RecurrenceObjectKind::Exception,
            };
            if matches!(self.mode, OccurrenceBackendMode::ImmutableMismatch) {
                replacement.provider_calendar_id = Some("different-calendar".into());
            }
            let canonical_event =
                (request.current_event.recurrence_kind == RecurrenceKind::Occurrence).then(|| {
                    let mut event = request.current_event.clone();
                    event.title = request.desired.title.clone();
                    event.description = request.desired.description.clone();
                    event.location = request.desired.location.clone();
                    event.start_time = request.desired.start_time.clone();
                    event.end_time = request.desired.end_time.clone();
                    event.all_day = request.desired.all_day;
                    event.timezone = request.desired.timezone.clone();
                    event.etag = Some("replacement-revision".into());
                    event
                });
            let mut canonical_recurrence_objects =
                (request.current_event.recurrence_kind == RecurrenceKind::Series).then(|| {
                    db::calendar_recurrence::get_by_event_id(
                        &ctx.db.reader(),
                        &request.current_event.id,
                    )
                    .unwrap()
                    .into_iter()
                    .filter(|identity| identity.object_id != "removed-object")
                    .map(|identity| {
                        let mut seed = recurrence_identity_seed(&identity);
                        seed.provider_native_data = Some("replacement-native-data".into());
                        seed.provider_revision = Some("replacement-revision".into());
                        if identity.object_id == request.trusted_identity.object_id {
                            replacement.clone()
                        } else {
                            seed
                        }
                    })
                    .collect::<Vec<_>>()
                });
            match self.mode {
                OccurrenceBackendMode::UnrelatedCanonicalSet => {
                    canonical_recurrence_objects.as_mut().unwrap()[0].provider_series_id =
                        Some("unrelated-resource.ics".into());
                }
                OccurrenceBackendMode::DuplicatePositionCanonicalSet => {
                    canonical_recurrence_objects
                        .as_mut()
                        .unwrap()
                        .push(replacement.clone());
                }
                OccurrenceBackendMode::DuplicateOccurrenceCanonicalSet => {
                    let seeds = canonical_recurrence_objects.as_mut().unwrap();
                    seeds[0].provider_occurrence_id = Some("duplicate-occurrence".into());
                    seeds[1].provider_occurrence_id = Some("duplicate-occurrence".into());
                }
                OccurrenceBackendMode::MissingSelectedCanonicalSet => {
                    canonical_recurrence_objects
                        .as_mut()
                        .unwrap()
                        .retain(|seed| !seed_matches_immutable_identity(seed, identity));
                }
                OccurrenceBackendMode::DetachedCanonicalSet => {
                    canonical_recurrence_objects = Some(vec![replacement.clone()]);
                }
                _ => {}
            }
            Ok(RemoteOccurrenceUpdateOutcome {
                replacement_identity: replacement,
                occurrence: request.desired.clone(),
                canonical_event,
                canonical_recurrence_objects,
            })
        }
    }

    fn recurrence_identity_seed(identity: &RecurrenceIdentity) -> RecurrenceIdentitySeed {
        RecurrenceIdentitySeed {
            local_series_event_id: identity.local_series_event_id.clone(),
            provider_calendar_id: identity.provider_calendar_id.clone(),
            provider_series_id: identity.provider_series_id.clone(),
            provider_occurrence_id: identity.provider_occurrence_id.clone(),
            recurrence_id: identity.recurrence_id.clone(),
            recurrence_timezone: identity.recurrence_timezone.clone(),
            recurrence_value_type: identity.recurrence_value_type,
            occurrence: identity.occurrence.clone(),
            provider_native_data: identity.provider_native_data.clone(),
            provider_revision: identity.provider_revision.clone(),
            kind: identity.kind,
        }
    }

    async fn recurrence_plan_state(protocol: &str) -> (tempfile::TempDir, AppState) {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(directory.path().to_path_buf()).unwrap();
        {
            let conn = state.db.writer().await;
            conn.execute_batch(
                "INSERT INTO accounts (id, display_name, email, username)
                 VALUES
                    ('account', 'Account', 'account@example.test', 'account@example.test'),
                    ('other', 'Other', 'other@example.test', 'other@example.test');
                 INSERT INTO calendars (id, account_id, name, remote_id)
                 VALUES
                    ('calendar', 'account', 'Calendar', 'remote-calendar'),
                    ('other-calendar', 'other', 'Other', 'other-calendar');",
            )
            .unwrap();
            db::service_bindings::insert(
                &conn,
                &db::service_bindings::ServiceBinding {
                    id: "calendar-binding".into(),
                    account_id: "account".into(),
                    service: "calendar".into(),
                    protocol: protocol.into(),
                    enabled: true,
                    sync_interval_seconds: None,
                    config_json: "{}".into(),
                },
            )
            .unwrap();
        }
        (directory, state)
    }

    fn recurrence_plan_event(
        id: &str,
        account_id: &str,
        kind: RecurrenceKind,
        remote_id: Option<&str>,
    ) -> CalendarEvent {
        CalendarEvent {
            id: id.into(),
            account_id: account_id.into(),
            calendar_id: if account_id == "account" {
                "calendar".into()
            } else {
                "other-calendar".into()
            },
            uid: Some(format!("{id}@example.test")),
            title: id.into(),
            description: None,
            location: None,
            start_time: "2026-09-15T10:00:00Z".into(),
            end_time: "2026-09-15T11:00:00Z".into(),
            all_day: false,
            timezone: Some("UTC".into()),
            recurrence_rule: (kind == RecurrenceKind::Series).then(|| "FREQ=WEEKLY".into()),
            recurrence_kind: kind,
            organizer_email: None,
            attendees_json: None,
            my_status: None,
            source_message_id: None,
            ical_data: None,
            remote_id: remote_id.map(str::to_owned),
            etag: None,
        }
    }

    fn recurrence_plan_identity(
        object_id: &str,
        event_id: &str,
        kind: RecurrenceObjectKind,
        local_series_event_id: Option<&str>,
        provider_series_id: Option<&str>,
        provider_occurrence_id: Option<&str>,
    ) -> RecurrenceIdentity {
        let occurrence = kind != RecurrenceObjectKind::Master;
        let provider_identity = provider_series_id.is_some() || provider_occurrence_id.is_some();
        RecurrenceIdentity {
            object_id: object_id.into(),
            account_id: "account".into(),
            event_id: event_id.into(),
            local_series_event_id: local_series_event_id.map(str::to_owned),
            provider_calendar_id: provider_identity.then(|| "provider-calendar".into()),
            provider_series_id: provider_series_id.map(str::to_owned),
            provider_occurrence_id: provider_occurrence_id.map(str::to_owned),
            recurrence_id: occurrence.then(|| format!("2026-09-15T10:00:00Z#{object_id}")),
            recurrence_timezone: occurrence.then(|| "UTC".into()),
            recurrence_value_type: occurrence.then_some(RecurrenceValueType::DateTime),
            occurrence: OccurrenceFields {
                title: format!("Effective {object_id}"),
                description: Some("Effective description".into()),
                location: Some("Effective room".into()),
                start_time: "2026-09-15T10:00:00Z".into(),
                end_time: "2026-09-15T11:00:00Z".into(),
                all_day: false,
                timezone: Some("Europe/Helsinki".into()),
            },
            provider_native_data: Some("secret-native-payload".into()),
            provider_revision: Some(format!("revision-{object_id}")),
            kind,
        }
    }

    async fn insert_plan_event(state: &AppState, event: &CalendarEvent) {
        db::calendar::insert_event(&*state.db.writer().await, event).unwrap();
    }

    async fn insert_plan_identity(state: &AppState, identity: &RecurrenceIdentity) {
        db::calendar_recurrence::upsert(&*state.db.writer().await, identity).unwrap();
    }

    fn occurrence_update() -> UpdateOccurrenceInput {
        UpdateOccurrenceInput {
            title: Some("Updated occurrence".into()),
            description: Some(String::new()),
            location: Some("Room 2".into()),
            start_time: Some("2026-09-15T12:00:00Z".into()),
            end_time: Some("2026-09-15T13:00:00Z".into()),
            all_day: None,
            timezone: Some("Europe/Stockholm".into()),
        }
    }

    async fn occurrence_fixture(
        protocol: &str,
        embedded: bool,
    ) -> (tempfile::TempDir, AppState, RecurrenceMutationPlan) {
        let (directory, state) = recurrence_plan_state(protocol).await;
        let event = recurrence_plan_event(
            "occurrence-event",
            "account",
            if embedded {
                RecurrenceKind::Series
            } else {
                RecurrenceKind::Occurrence
            },
            Some(if embedded {
                "resource.ics"
            } else {
                "remote-occurrence"
            }),
        );
        insert_plan_event(&state, &event).await;
        let mut identity = recurrence_plan_identity(
            "occurrence-object",
            "occurrence-event",
            RecurrenceObjectKind::Occurrence,
            None,
            Some(if embedded {
                "resource.ics"
            } else {
                "remote-series"
            }),
            (!embedded).then_some("provider-occurrence"),
        );
        if embedded {
            identity.kind = RecurrenceObjectKind::Exception;
            identity.occurrence.title = "Embedded exception".into();
            identity.occurrence.description = Some("Exception description".into());
            identity.occurrence.location = Some("Exception room".into());
            identity.occurrence.timezone = Some("America/Toronto".into());
        }
        insert_plan_identity(&state, &identity).await;
        if embedded {
            for (object_id, kind) in [
                ("master-object", RecurrenceObjectKind::Master),
                ("sibling-object", RecurrenceObjectKind::Exception),
                ("removed-object", RecurrenceObjectKind::Exclusion),
            ] {
                insert_plan_identity(
                    &state,
                    &recurrence_plan_identity(
                        object_id,
                        "occurrence-event",
                        kind,
                        None,
                        Some("resource.ics"),
                        None,
                    ),
                )
                .await;
            }
        }
        let plan = plan_event_recurrence_mutation_inner(
            &state,
            "occurrence-event",
            "occurrence-object",
            RecurrenceMutationScope::ThisOccurrence,
        )
        .await
        .unwrap();
        (directory, state, plan)
    }

    async fn run_occurrence_update(
        state: &AppState,
        plan: &RecurrenceMutationPlan,
        update: UpdateOccurrenceInput,
        backend: &dyn CalendarBackend,
    ) -> Result<UpdatedOccurrence> {
        update_event_recurrence_occurrence_inner(
            state,
            plan.event_id.clone(),
            plan.recurrence_object_id.clone(),
            plan.expected_local_revision,
            plan.expected_provider_revision.clone(),
            plan.backend_protocol.clone(),
            plan.remote_target_id.clone(),
            update,
            Some(backend),
        )
        .await
    }

    #[tokio::test]
    async fn recurrence_discovery_is_owned_complete_safe_and_deterministic() {
        let (_directory, state) = recurrence_plan_state("caldav").await;
        let series = recurrence_plan_event(
            "series-event",
            "account",
            RecurrenceKind::Series,
            Some("resource.ics"),
        );
        let detached = recurrence_plan_event(
            "detached-event",
            "account",
            RecurrenceKind::Occurrence,
            Some("remote-detached"),
        );
        insert_plan_event(&state, &series).await;
        insert_plan_event(&state, &detached).await;

        let master = recurrence_plan_identity(
            "master-object",
            "series-event",
            RecurrenceObjectKind::Master,
            None,
            Some("resource.ics"),
            None,
        );
        let mut later = recurrence_plan_identity(
            "later-override",
            "series-event",
            RecurrenceObjectKind::Exception,
            None,
            Some("resource.ics"),
            None,
        );
        later.occurrence.start_time = "2026-09-17T10:00:00Z".into();
        later.occurrence.end_time = "2026-09-17T11:00:00Z".into();
        let mut earlier = recurrence_plan_identity(
            "earlier-override",
            "series-event",
            RecurrenceObjectKind::Occurrence,
            None,
            Some("resource.ics"),
            None,
        );
        earlier.occurrence.start_time = "2026-09-16T10:00:00Z".into();
        earlier.occurrence.end_time = "2026-09-16T11:00:00Z".into();
        let sibling = recurrence_plan_identity(
            "detached-object",
            "detached-event",
            RecurrenceObjectKind::Occurrence,
            Some("series-event"),
            Some("resource.ics"),
            Some("remote-detached"),
        );
        for identity in [&later, &sibling, &master, &earlier] {
            insert_plan_identity(&state, identity).await;
        }

        let summaries = get_event_recurrence_objects_inner(&state, "series-event")
            .await
            .unwrap();
        assert_eq!(
            summaries
                .iter()
                .map(|summary| summary.object_id.as_str())
                .collect::<Vec<_>>(),
            vec!["master-object", "earlier-override", "later-override"]
        );
        assert!(summaries.iter().all(|summary| {
            summary.event_id == "series-event"
                && summary.account_id == "account"
                && summary.calendar_id == "calendar"
                && summary.provider_calendar_id.as_deref() == Some("provider-calendar")
                && summary.provider_series_id.as_deref() == Some("resource.ics")
        }));
        assert_eq!(summaries[1].occurrence.title, "Effective earlier-override");
        assert_eq!(
            summaries[1].provider_revision.as_deref(),
            Some("revision-earlier-override")
        );
        let serialized = serde_json::to_value(&summaries).unwrap();
        assert!(!serialized.to_string().contains("secret-native-payload"));
        assert!(serialized
            .as_array()
            .unwrap()
            .iter()
            .all(|summary| summary.get("provider_native_data").is_none()));

        let detached_summaries = get_event_recurrence_objects_inner(&state, "detached-event")
            .await
            .unwrap();
        assert_eq!(detached_summaries.len(), 1);
        assert_eq!(detached_summaries[0].object_id, "detached-object");
    }

    #[tokio::test]
    async fn recurrence_discovery_returns_legacy_empty_and_rejects_contradictions() {
        let (_directory, state) = recurrence_plan_state("google").await;
        for event in [
            recurrence_plan_event(
                "legacy-series",
                "account",
                RecurrenceKind::Series,
                Some("legacy-series"),
            ),
            recurrence_plan_event(
                "standalone",
                "account",
                RecurrenceKind::Standalone,
                Some("standalone"),
            ),
            recurrence_plan_event(
                "unknown",
                "account",
                RecurrenceKind::Unknown,
                Some("unknown"),
            ),
            recurrence_plan_event(
                "owned-series",
                "account",
                RecurrenceKind::Series,
                Some("owned-series"),
            ),
        ] {
            insert_plan_event(&state, &event).await;
        }
        assert!(get_event_recurrence_objects_inner(&state, "legacy-series")
            .await
            .unwrap()
            .is_empty());

        for (event_id, object_id) in [
            ("standalone", "standalone-object"),
            ("unknown", "unknown-object"),
        ] {
            insert_plan_identity(
                &state,
                &recurrence_plan_identity(
                    object_id,
                    event_id,
                    RecurrenceObjectKind::Occurrence,
                    Some("legacy-series"),
                    Some("legacy-series"),
                    Some(object_id),
                ),
            )
            .await;
            assert!(get_event_recurrence_objects_inner(&state, event_id)
                .await
                .is_err());
        }

        insert_plan_identity(
            &state,
            &recurrence_plan_identity(
                "ownership-object",
                "owned-series",
                RecurrenceObjectKind::Master,
                None,
                Some("owned-series"),
                None,
            ),
        )
        .await;
        let conn = state.db.writer().await;
        conn.execute_batch("DROP TRIGGER calendar_recurrence_account_update")
            .unwrap();
        conn.execute(
            "UPDATE calendar_recurrence_objects SET account_id = 'other'
             WHERE object_id = 'ownership-object'",
            [],
        )
        .unwrap();
        drop(conn);
        assert!(get_event_recurrence_objects_inner(&state, "owned-series")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn occurrence_update_rejects_stale_plans_before_provider_io() {
        let (_directory, state, plan) = occurrence_fixture("google", false).await;
        let backend = MockOccurrenceBackend {
            calls: AtomicUsize::new(0),
            mode: OccurrenceBackendMode::Success,
            protocol: "google",
        };
        let mut stale_local = plan.clone();
        stale_local.expected_local_revision += 1;
        assert!(
            run_occurrence_update(&state, &stale_local, occurrence_update(), &backend)
                .await
                .is_err()
        );
        let mut stale_provider = plan.clone();
        stale_provider.expected_provider_revision = None;
        assert!(
            run_occurrence_update(&state, &stale_provider, occurrence_update(), &backend)
                .await
                .is_err()
        );
        let changed_protocol_backend = MockOccurrenceBackend {
            calls: AtomicUsize::new(0),
            mode: OccurrenceBackendMode::Success,
            protocol: "graph",
        };
        assert!(run_occurrence_update(
            &state,
            &plan,
            occurrence_update(),
            &changed_protocol_backend
        )
        .await
        .is_err());
        assert_eq!(changed_protocol_backend.calls.load(Ordering::SeqCst), 0);

        let mut stale_target = plan.clone();
        state
            .db
            .writer()
            .await
            .execute(
                "UPDATE calendar_recurrence_objects
                 SET provider_occurrence_id = 'replacement-target'
                 WHERE object_id = 'occurrence-object'",
                [],
            )
            .unwrap();
        stale_target.expected_local_revision =
            db::calendar_revision::get(&state.db.reader(), &plan.event_id).unwrap();
        assert!(
            run_occurrence_update(&state, &stale_target, occurrence_update(), &backend)
                .await
                .is_err()
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unsupported_or_failed_remote_occurrence_update_does_not_write_locally() {
        for mode in [
            OccurrenceBackendMode::Unsupported,
            OccurrenceBackendMode::RemoteFailure,
        ] {
            let backend = MockOccurrenceBackend {
                calls: AtomicUsize::new(0),
                mode,
                protocol: "google",
            };
            let (_directory, state, plan) = occurrence_fixture("google", false).await;
            let before_event = db::calendar::get_event(&state.db.reader(), &plan.event_id).unwrap();
            let before_identity = db::calendar_recurrence::get_by_object_id(
                &state.db.reader(),
                &plan.recurrence_object_id,
            )
            .unwrap();
            assert!(
                run_occurrence_update(&state, &plan, occurrence_update(), &backend)
                    .await
                    .is_err()
            );
            assert_eq!(
                db::calendar::get_event(&state.db.reader(), &plan.event_id).unwrap(),
                before_event
            );
            assert_eq!(
                db::calendar_recurrence::get_by_object_id(
                    &state.db.reader(),
                    &plan.recurrence_object_id
                )
                .unwrap(),
                before_identity
            );
            assert_eq!(
                db::calendar_revision::get(&state.db.reader(), &plan.event_id).unwrap(),
                plan.expected_local_revision
            );
        }
    }

    #[tokio::test]
    async fn detached_occurrence_success_persists_canonical_event_and_identity() {
        let (_directory, state, plan) = occurrence_fixture("google", false).await;
        let backend = MockOccurrenceBackend {
            calls: AtomicUsize::new(0),
            mode: OccurrenceBackendMode::Success,
            protocol: "google",
        };
        let result = run_occurrence_update(&state, &plan, occurrence_update(), &backend)
            .await
            .unwrap();
        let event = db::calendar::get_event(&state.db.reader(), &plan.event_id).unwrap();
        let identity = db::calendar_recurrence::get_by_object_id(
            &state.db.reader(),
            &plan.recurrence_object_id,
        )
        .unwrap()
        .unwrap();
        assert_eq!(event.title, "Updated occurrence");
        assert_eq!(event.description.as_deref(), Some(""));
        assert_eq!(event.remote_id.as_deref(), Some("remote-occurrence"));
        assert_eq!(identity.object_id, plan.recurrence_object_id);
        assert_eq!(identity.kind, RecurrenceObjectKind::Exception);
        assert_eq!(result.fields, occurrence_fields_from_event(&event));
        assert_eq!(identity.occurrence, result.fields);
        assert_eq!(
            result.local_revision,
            db::calendar_revision::get(&state.db.reader(), &event.id).unwrap()
        );
        assert!(result.local_revision > plan.expected_local_revision);
    }

    #[tokio::test]
    async fn remote_occurrence_update_receives_the_original_sparse_patch() {
        let (_directory, state, plan) = occurrence_fixture("google", false).await;
        let backend = MockOccurrenceBackend {
            calls: AtomicUsize::new(0),
            mode: OccurrenceBackendMode::SparsePatch,
            protocol: "google",
        };
        let patch = UpdateOccurrenceInput {
            title: Some("Sparse title".into()),
            timezone: Some("Europe/Paris".into()),
            ..Default::default()
        };

        let result = run_occurrence_update(&state, &plan, patch, &backend)
            .await
            .unwrap();

        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.fields.title, "Sparse title");
        assert_eq!(result.fields.description, plan.occurrence.description);
        assert_eq!(result.fields.location, plan.occurrence.location);
        assert_eq!(result.fields.start_time, plan.occurrence.start_time);
        assert_eq!(result.fields.end_time, plan.occurrence.end_time);
        assert_eq!(result.fields.all_day, plan.occurrence.all_day);
        assert_eq!(result.fields.timezone.as_deref(), Some("Europe/Paris"));
    }

    #[tokio::test]
    async fn embedded_occurrence_success_does_not_overwrite_master_event() {
        let (_directory, state, plan) = occurrence_fixture("caldav", true).await;
        let before = db::calendar::get_event(&state.db.reader(), &plan.event_id).unwrap();
        let before_objects =
            db::calendar_recurrence::get_by_event_id(&state.db.reader(), &plan.event_id).unwrap();
        let backend = MockOccurrenceBackend {
            calls: AtomicUsize::new(0),
            mode: OccurrenceBackendMode::Success,
            protocol: "caldav",
        };
        let update = UpdateOccurrenceInput {
            start_time: Some("2026-09-15T12:00:00Z".into()),
            end_time: Some("2026-09-15T13:00:00Z".into()),
            ..Default::default()
        };
        let result = run_occurrence_update(&state, &plan, update, &backend)
            .await
            .unwrap();
        assert_eq!(
            db::calendar::get_event(&state.db.reader(), &plan.event_id).unwrap(),
            before
        );
        assert_eq!(result.fields.title, "Embedded exception");
        assert_eq!(
            result.fields.description.as_deref(),
            Some("Exception description")
        );
        assert_eq!(result.fields.location.as_deref(), Some("Exception room"));
        assert_eq!(result.fields.timezone.as_deref(), Some("America/Toronto"));
        assert_eq!(result.fields.start_time, "2026-09-15T12:00:00Z");
        let stored = db::calendar_recurrence::get_by_object_id(
            &state.db.reader(),
            &plan.recurrence_object_id,
        )
        .unwrap()
        .unwrap();
        assert_eq!(stored.occurrence, result.fields);
        assert_eq!(result.kind, RecurrenceObjectKind::Exception);
        assert!(result.local_revision > plan.expected_local_revision);

        let stored_objects =
            db::calendar_recurrence::get_by_event_id(&state.db.reader(), &plan.event_id).unwrap();
        assert_eq!(stored_objects.len(), before_objects.len() - 1);
        assert!(
            db::calendar_recurrence::get_by_object_id(&state.db.reader(), "removed-object")
                .unwrap()
                .is_none()
        );
        for object_id in ["master-object", "sibling-object", "occurrence-object"] {
            let previous = before_objects
                .iter()
                .find(|identity| identity.object_id == object_id)
                .unwrap();
            let refreshed = stored_objects
                .iter()
                .find(|identity| identity.object_id == object_id)
                .unwrap();
            assert_eq!(refreshed.object_id, previous.object_id);
            assert_eq!(
                refreshed.provider_revision.as_deref(),
                Some("replacement-revision")
            );
            assert_eq!(
                refreshed.provider_native_data.as_deref(),
                Some("replacement-native-data")
            );
        }
    }

    #[tokio::test]
    async fn invalid_embedded_canonical_sets_require_reconciliation_without_writes() {
        for mode in [
            OccurrenceBackendMode::UnrelatedCanonicalSet,
            OccurrenceBackendMode::DuplicatePositionCanonicalSet,
            OccurrenceBackendMode::DuplicateOccurrenceCanonicalSet,
            OccurrenceBackendMode::MissingSelectedCanonicalSet,
        ] {
            let (_directory, state, plan) = occurrence_fixture("caldav", true).await;
            let before_event = db::calendar::get_event(&state.db.reader(), &plan.event_id).unwrap();
            let before_objects =
                db::calendar_recurrence::get_by_event_id(&state.db.reader(), &plan.event_id)
                    .unwrap();
            let before_revision =
                db::calendar_revision::get(&state.db.reader(), &plan.event_id).unwrap();
            let backend = MockOccurrenceBackend {
                calls: AtomicUsize::new(0),
                mode,
                protocol: "caldav",
            };

            let error = run_occurrence_update(
                &state,
                &plan,
                UpdateOccurrenceInput {
                    title: Some("Remote success".into()),
                    ..Default::default()
                },
                &backend,
            )
            .await
            .unwrap_err();

            assert!(matches!(error, crate::error::Error::Sync(_)));
            assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                db::calendar::get_event(&state.db.reader(), &plan.event_id).unwrap(),
                before_event
            );
            assert_eq!(
                db::calendar_recurrence::get_by_event_id(&state.db.reader(), &plan.event_id)
                    .unwrap(),
                before_objects
            );
            assert_eq!(
                db::calendar_revision::get(&state.db.reader(), &plan.event_id).unwrap(),
                before_revision
            );
        }
    }

    #[tokio::test]
    async fn detached_occurrence_rejects_a_canonical_recurrence_set_without_writes() {
        let (_directory, state, plan) = occurrence_fixture("google", false).await;
        let before_event = db::calendar::get_event(&state.db.reader(), &plan.event_id).unwrap();
        let before_objects =
            db::calendar_recurrence::get_by_event_id(&state.db.reader(), &plan.event_id).unwrap();
        let before_revision =
            db::calendar_revision::get(&state.db.reader(), &plan.event_id).unwrap();
        let backend = MockOccurrenceBackend {
            calls: AtomicUsize::new(0),
            mode: OccurrenceBackendMode::DetachedCanonicalSet,
            protocol: "google",
        };

        let error = run_occurrence_update(&state, &plan, occurrence_update(), &backend)
            .await
            .unwrap_err();

        assert!(matches!(error, crate::error::Error::Sync(_)));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            db::calendar::get_event(&state.db.reader(), &plan.event_id).unwrap(),
            before_event
        );
        assert_eq!(
            db::calendar_recurrence::get_by_event_id(&state.db.reader(), &plan.event_id).unwrap(),
            before_objects
        );
        assert_eq!(
            db::calendar_revision::get(&state.db.reader(), &plan.event_id).unwrap(),
            before_revision
        );
    }

    #[tokio::test]
    async fn invalid_remote_identity_and_post_remote_race_require_reconciliation() {
        for mode in [
            OccurrenceBackendMode::ImmutableMismatch,
            OccurrenceBackendMode::LocalRace,
        ] {
            let (_directory, state, plan) = occurrence_fixture("google", false).await;
            let before_identity = db::calendar_recurrence::get_by_object_id(
                &state.db.reader(),
                &plan.recurrence_object_id,
            )
            .unwrap();
            let backend = MockOccurrenceBackend {
                calls: AtomicUsize::new(0),
                mode,
                protocol: "google",
            };
            let error = run_occurrence_update(&state, &plan, occurrence_update(), &backend)
                .await
                .unwrap_err();
            assert!(matches!(error, crate::error::Error::Sync(_)));
            assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                db::calendar_recurrence::get_by_object_id(
                    &state.db.reader(),
                    &plan.recurrence_object_id
                )
                .unwrap(),
                before_identity
            );
        }
    }

    #[test]
    fn occurrence_update_input_has_no_series_scope_or_forbidden_fields() {
        let parsed = serde_json::from_value::<UpdateOccurrenceInput>(serde_json::json!({
            "title": "Occurrence",
            "scope": "entire-series",
            "calendar_id": "other",
            "recurrence_rule": "FREQ=DAILY",
            "attendees": []
        }));
        assert!(parsed.is_err());
    }

    #[tokio::test]
    async fn recurrence_plans_enforce_scope_matrix_and_choose_remote_targets() {
        let (_directory, state) = recurrence_plan_state("google").await;
        let master = recurrence_plan_event(
            "master-event",
            "account",
            RecurrenceKind::Series,
            Some("remote-master"),
        );
        let occurrence = recurrence_plan_event(
            "occurrence-event",
            "account",
            RecurrenceKind::Occurrence,
            Some("remote-occurrence"),
        );
        insert_plan_event(&state, &master).await;
        insert_plan_event(&state, &occurrence).await;
        insert_plan_identity(
            &state,
            &recurrence_plan_identity(
                "master-object",
                "master-event",
                RecurrenceObjectKind::Master,
                None,
                Some("remote-master"),
                None,
            ),
        )
        .await;
        insert_plan_identity(
            &state,
            &recurrence_plan_identity(
                "occurrence-object",
                "occurrence-event",
                RecurrenceObjectKind::Occurrence,
                Some("master-event"),
                Some("remote-master"),
                Some("provider-occurrence"),
            ),
        )
        .await;

        assert!(plan_event_recurrence_mutation_inner(
            &state,
            "master-event",
            "master-object",
            RecurrenceMutationScope::ThisOccurrence,
        )
        .await
        .is_err());
        let master_plan = plan_event_recurrence_mutation_inner(
            &state,
            "master-event",
            "master-object",
            RecurrenceMutationScope::EntireSeries,
        )
        .await
        .unwrap();
        assert_eq!(master_plan.remote_target_id, "remote-master");

        let occurrence_plan = plan_event_recurrence_mutation_inner(
            &state,
            "occurrence-event",
            "occurrence-object",
            RecurrenceMutationScope::ThisOccurrence,
        )
        .await
        .unwrap();
        assert_eq!(occurrence_plan.remote_target_id, "provider-occurrence");
        assert_eq!(
            occurrence_plan.provider_calendar_id.as_deref(),
            Some("provider-calendar")
        );
        assert_eq!(occurrence_plan.backend_protocol, "google");
        assert_eq!(
            occurrence_plan.occurrence.title,
            "Effective occurrence-object"
        );
        let first_revision = occurrence_plan.expected_local_revision;
        assert_eq!(
            occurrence_plan.expected_provider_revision.as_deref(),
            Some("revision-occurrence-object")
        );
        assert!(serde_json::to_value(&occurrence_plan)
            .unwrap()
            .get("provider_native_data")
            .is_none());

        let series_plan = plan_event_recurrence_mutation_inner(
            &state,
            "occurrence-event",
            "occurrence-object",
            RecurrenceMutationScope::EntireSeries,
        )
        .await
        .unwrap();
        assert_eq!(series_plan.remote_target_id, "remote-master");
        assert_eq!(
            series_plan.expected_provider_revision.as_deref(),
            Some("revision-master-object")
        );

        state
            .db
            .writer()
            .await
            .execute(
                "UPDATE calendar_events SET title = title WHERE id = 'occurrence-event'",
                [],
            )
            .unwrap();
        let refreshed = plan_event_recurrence_mutation_inner(
            &state,
            "occurrence-event",
            "occurrence-object",
            RecurrenceMutationScope::ThisOccurrence,
        )
        .await
        .unwrap();
        assert!(refreshed.expected_local_revision > first_revision);
    }

    #[tokio::test]
    async fn embedded_occurrence_plan_targets_owning_resource() {
        let (_directory, state) = recurrence_plan_state("caldav").await;
        let event = recurrence_plan_event(
            "embedded-event",
            "account",
            RecurrenceKind::Series,
            Some("resource.ics"),
        );
        insert_plan_event(&state, &event).await;
        for identity in [
            recurrence_plan_identity(
                "embedded-master",
                "embedded-event",
                RecurrenceObjectKind::Master,
                None,
                Some("resource.ics"),
                None,
            ),
            recurrence_plan_identity(
                "embedded-exception",
                "embedded-event",
                RecurrenceObjectKind::Exception,
                None,
                Some("resource.ics"),
                None,
            ),
        ] {
            insert_plan_identity(&state, &identity).await;
        }
        let plan = plan_event_recurrence_mutation_inner(
            &state,
            "embedded-event",
            "embedded-exception",
            RecurrenceMutationScope::ThisOccurrence,
        )
        .await
        .unwrap();
        assert_eq!(plan.remote_target_id, "resource.ics");
        state
            .db
            .writer()
            .await
            .execute(
                "UPDATE calendar_events SET remote_id = NULL WHERE id = 'embedded-event'",
                [],
            )
            .unwrap();
        assert!(plan_event_recurrence_mutation_inner(
            &state,
            "embedded-event",
            "embedded-exception",
            RecurrenceMutationScope::ThisOccurrence,
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn recurrence_planning_rejects_untrusted_or_conflicting_identity() {
        let (_directory, state) = recurrence_plan_state("google").await;
        for event in [
            recurrence_plan_event(
                "master-a",
                "account",
                RecurrenceKind::Series,
                Some("remote-a"),
            ),
            recurrence_plan_event(
                "master-b",
                "account",
                RecurrenceKind::Series,
                Some("remote-b"),
            ),
            recurrence_plan_event(
                "occurrence",
                "account",
                RecurrenceKind::Occurrence,
                Some("remote-occurrence"),
            ),
            recurrence_plan_event(
                "unknown",
                "account",
                RecurrenceKind::Unknown,
                Some("remote-unknown"),
            ),
            recurrence_plan_event(
                "standalone",
                "account",
                RecurrenceKind::Standalone,
                Some("remote-standalone"),
            ),
            recurrence_plan_event(
                "other-event",
                "other",
                RecurrenceKind::Series,
                Some("other-master"),
            ),
        ] {
            insert_plan_event(&state, &event).await;
        }
        for identity in [
            recurrence_plan_identity(
                "master-a-object",
                "master-a",
                RecurrenceObjectKind::Master,
                None,
                Some("remote-a"),
                None,
            ),
            recurrence_plan_identity(
                "master-b-object",
                "master-b",
                RecurrenceObjectKind::Master,
                None,
                Some("remote-b"),
                None,
            ),
            recurrence_plan_identity(
                "conflict",
                "occurrence",
                RecurrenceObjectKind::Occurrence,
                Some("master-a"),
                Some("remote-b"),
                Some("remote-conflict"),
            ),
            recurrence_plan_identity(
                "excluded",
                "occurrence",
                RecurrenceObjectKind::Exclusion,
                Some("master-a"),
                Some("remote-a"),
                Some("remote-excluded"),
            ),
            recurrence_plan_identity(
                "unknown-object",
                "unknown",
                RecurrenceObjectKind::Occurrence,
                Some("master-a"),
                Some("remote-a"),
                Some("remote-unknown"),
            ),
            recurrence_plan_identity(
                "standalone-object",
                "standalone",
                RecurrenceObjectKind::Occurrence,
                Some("master-a"),
                Some("remote-a"),
                Some("remote-standalone"),
            ),
        ] {
            insert_plan_identity(&state, &identity).await;
        }
        state
            .db
            .writer()
            .await
            .execute_batch("DROP TRIGGER calendar_recurrence_account_insert")
            .unwrap();
        insert_plan_identity(
            &state,
            &recurrence_plan_identity(
                "cross-account-series",
                "occurrence",
                RecurrenceObjectKind::Occurrence,
                Some("other-event"),
                Some("remote-a"),
                Some("remote-cross-account"),
            ),
        )
        .await;

        for (event_id, object_id, scope) in [
            (
                "occurrence",
                "conflict",
                RecurrenceMutationScope::EntireSeries,
            ),
            (
                "occurrence",
                "excluded",
                RecurrenceMutationScope::ThisOccurrence,
            ),
            (
                "unknown",
                "unknown-object",
                RecurrenceMutationScope::ThisOccurrence,
            ),
            (
                "standalone",
                "standalone-object",
                RecurrenceMutationScope::ThisOccurrence,
            ),
            (
                "master-a",
                "conflict",
                RecurrenceMutationScope::EntireSeries,
            ),
            (
                "occurrence",
                "cross-account-series",
                RecurrenceMutationScope::EntireSeries,
            ),
            (
                "occurrence",
                "missing",
                RecurrenceMutationScope::ThisOccurrence,
            ),
        ] {
            assert!(
                plan_event_recurrence_mutation_inner(&state, event_id, object_id, scope)
                    .await
                    .is_err()
            );
        }

        state
            .db
            .writer()
            .await
            .execute_batch("DROP TRIGGER calendar_recurrence_account_update")
            .unwrap();
        state
            .db
            .writer()
            .await
            .execute(
                "UPDATE calendar_recurrence_objects SET account_id = 'other'
                 WHERE object_id = 'unknown-object'",
                [],
            )
            .unwrap();
        assert!(plan_event_recurrence_mutation_inner(
            &state,
            "unknown",
            "unknown-object",
            RecurrenceMutationScope::ThisOccurrence,
        )
        .await
        .is_err());

        state
            .db
            .writer()
            .await
            .execute(
                "UPDATE service_bindings SET enabled = 0
                 WHERE id = 'calendar-binding'",
                [],
            )
            .unwrap();
        assert!(plan_event_recurrence_mutation_inner(
            &state,
            "master-a",
            "master-a-object",
            RecurrenceMutationScope::EntireSeries,
        )
        .await
        .is_err());
    }

    fn import_group(ical: &str) -> ical::IcalEventGroup {
        ical::parse_ical_event_groups(ical)
            .expect("parse import fixture")
            .remove(0)
    }

    #[test]
    fn recurring_import_requires_a_raw_series_capable_backend() {
        let group = import_group(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\n\
             UID:series@example.test\r\nSUMMARY:Series\r\n\
             DTSTART:20260914T080000Z\r\nDTEND:20260914T090000Z\r\n\
             RRULE:FREQ=WEEKLY;COUNT=3\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        let calendar = Calendar {
            id: "calendar".into(),
            account_id: "account".into(),
            name: "Calendar".into(),
            color: "#123456".into(),
            is_default: true,
            remote_id: Some("remote-calendar".into()),
            is_subscribed: true,
        };

        assert!(import_group_error(&group, &calendar, None, "message").is_none());
        assert!(import_group_error(
            &group,
            &calendar,
            crate::backend::calendar::for_protocol("caldav"),
            "message"
        )
        .is_none());
        for protocol in ["jmap", "google"] {
            let error = import_group_error(
                &group,
                &calendar,
                crate::backend::calendar::for_protocol(protocol),
                "message",
            );
            assert!(
                error
                    .as_deref()
                    .is_some_and(|message| message.contains("recurring")),
                "{protocol} should reject a recurring import"
            );
        }
        assert!(import_group_error(
            &group,
            &calendar,
            crate::backend::calendar::for_protocol("graph"),
            "message"
        )
        .is_none());
    }

    #[test]
    fn provider_target_rules_reject_unusable_calendars() {
        let mut calendar = Calendar {
            id: "calendar".into(),
            account_id: "account".into(),
            name: "Calendar".into(),
            color: "#123456".into(),
            is_default: false,
            remote_id: Some("remote-calendar".into()),
            is_subscribed: true,
        };

        for protocol in ["google", "graph"] {
            let backend = crate::backend::calendar::for_protocol(protocol);
            assert!(import_target_error(&calendar, backend).is_some());
            calendar.is_default = true;
            assert!(import_target_error(&calendar, backend).is_none());
            calendar.is_default = false;
        }

        let caldav = crate::backend::calendar::for_protocol("caldav");
        assert!(import_target_error(&calendar, caldav).is_none());
        calendar.remote_id = None;
        assert!(import_target_error(&calendar, caldav).is_some());
    }

    #[test]
    fn calendar_messages_use_the_configured_sender_name() {
        use mailparse::MailHeaderMap as _;

        let reply = build_calendar_reply_message(
            "asa@example.com",
            "Åsa Österberg",
            "organizer@example.com",
            "Re: Planning",
            "Accepted",
            "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nEND:VCALENDAR\r\n",
        )
        .unwrap();
        let invite = build_invite_message(
            "asa@example.com",
            "Åsa Österberg",
            "guest@example.com",
            "Planning",
            "Please join",
            "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR\r\n",
        );

        for raw in [&reply, &invite] {
            let parsed = mailparse::parse_mail(raw).expect("parse generated calendar message");
            assert_eq!(
                parsed.headers.get_first_value("From").as_deref(),
                Some("Åsa Österberg <asa@example.com>")
            );
        }
    }

    #[test]
    fn participant_schedule_request_normalizes_addresses() {
        let request = build_participant_schedule_request(
            vec![
                " Bob@Example.com ".into(),
                "alice@example.com".into(),
                "bob@example.com".into(),
                " ".into(),
            ],
            "2026-08-10T09:00:00Z".into(),
            "2026-08-10T10:00:00Z".into(),
        )
        .unwrap();

        assert_eq!(request.emails, vec!["alice@example.com", "bob@example.com"]);
    }

    #[test]
    fn participant_schedule_request_rejects_too_many_addresses() {
        let emails = (0..51)
            .map(|index| format!("person{index}@example.com"))
            .collect();
        assert!(build_participant_schedule_request(
            emails,
            "2026-08-10T09:00:00Z".into(),
            "2026-08-10T10:00:00Z".into(),
        )
        .is_err());
    }

    #[test]
    fn participant_schedule_request_rejects_invalid_ranges() {
        assert!(build_participant_schedule_request(
            vec!["person@example.com".into()],
            "2026-08-10T10:00:00Z".into(),
            "2026-08-10T09:00:00Z".into(),
        )
        .is_err());
        assert!(build_participant_schedule_request(
            vec!["person@example.com".into()],
            "not-a-date".into(),
            "2026-08-10T09:00:00Z".into(),
        )
        .is_err());
    }

    #[test]
    fn pending_claim_requires_exact_backend_metadata() {
        let pending = db::meet_pending_meetings::PendingMeeting {
            lifecycle_id: "lifecycle".into(),
            account_id: "account".into(),
            protocol: "zoom".into(),
            meeting_id: "meeting".into(),
            join_url: "https://example.test/join".into(),
            created_at: "2026-08-11T20:00:00Z".into(),
            cleanup_requested: false,
        };
        let mut binding = MeetBindingInput {
            lifecycle_id: pending.lifecycle_id.clone(),
            account_id: pending.account_id.clone(),
            protocol: pending.protocol.clone(),
            meeting_id: pending.meeting_id.clone(),
            join_url: pending.join_url.clone(),
        };
        assert!(pending_matches_binding(&pending, &binding));

        binding.meeting_id = "forged".into();
        assert!(!pending_matches_binding(&pending, &binding));
    }

    fn pending_claim_connection() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        db::schema::initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('account', 'Account', 'account@example.test',
                     'account@example.test')",
            [],
        )
        .unwrap();
        conn
    }

    fn pending_claim_binding() -> MeetBindingInput {
        MeetBindingInput {
            lifecycle_id: "37b4c2b4-9256-4f42-a453-406aa2f3f0ef".into(),
            account_id: "account".into(),
            protocol: "zoom".into(),
            meeting_id: "meeting".into(),
            join_url: "https://example.test/join".into(),
        }
    }

    fn insert_pending_claim_fixture(conn: &rusqlite::Connection, binding: &MeetBindingInput) {
        db::meet_pending_meetings::insert(
            conn,
            &db::meet_pending_meetings::PendingMeeting {
                lifecycle_id: binding.lifecycle_id.clone(),
                account_id: binding.account_id.clone(),
                protocol: binding.protocol.clone(),
                meeting_id: binding.meeting_id.clone(),
                join_url: binding.join_url.clone(),
                created_at: "2026-08-11T20:00:00Z".into(),
                cleanup_requested: false,
            },
        )
        .unwrap();
    }

    fn insert_claim_event(conn: &rusqlite::Connection, event_id: &str) -> Result<()> {
        conn.execute(
            "INSERT INTO calendar_events
                (id, account_id, calendar_id, title, start_time, end_time)
             VALUES (?1, 'account', 'calendar', 'Event',
                     '2026-08-11T20:00:00Z', '2026-08-11T21:00:00Z')",
            rusqlite::params![event_id],
        )?;
        Ok(())
    }

    fn bind_meeting(conn: &rusqlite::Connection, event_id: &str, meeting_id: &str) {
        db::meet_meetings::upsert(
            conn,
            &db::meet_meetings::MeetMeeting {
                event_id: event_id.into(),
                account_id: "account".into(),
                protocol: "zoom".into(),
                meeting_id: meeting_id.into(),
                join_url: format!("https://example.test/{meeting_id}"),
            },
        )
        .unwrap();
    }

    #[test]
    fn exact_pending_claim_transfers_ownership_transactionally() {
        let mut conn = pending_claim_connection();
        let binding = pending_claim_binding();
        insert_pending_claim_fixture(&conn, &binding);

        let transaction = conn.transaction().unwrap();
        insert_claim_event(&transaction, "event").unwrap();
        let pending = matching_pending_meeting(&transaction, &binding).unwrap();
        transfer_pending_meeting(&transaction, "event", &binding, pending).unwrap();
        transaction.commit().unwrap();

        assert!(db::meet_pending_meetings::get(&conn, &binding.lifecycle_id)
            .unwrap()
            .is_none());
        let bound = db::meet_meetings::get(&conn, "event").unwrap().unwrap();
        assert_eq!(bound.meeting_id, binding.meeting_id);
    }

    #[test]
    fn mismatched_pending_claim_rolls_back_event_and_preserves_ownership() {
        let mut conn = pending_claim_connection();
        let binding = pending_claim_binding();
        insert_pending_claim_fixture(&conn, &binding);
        let mut forged = binding.clone();
        forged.join_url = "https://attacker.test/join".into();

        let transaction = conn.transaction().unwrap();
        insert_claim_event(&transaction, "event").unwrap();
        let result = matching_pending_meeting(&transaction, &forged)
            .and_then(|pending| transfer_pending_meeting(&transaction, "event", &forged, pending));
        assert!(result.is_err());
        transaction.rollback().unwrap();

        assert!(db::meet_pending_meetings::get(&conn, &binding.lifecycle_id)
            .unwrap()
            .is_some());
        assert!(db::meet_meetings::get(&conn, "event").unwrap().is_none());
        assert!(db::calendar::get_event(&conn, "event").is_err());
    }

    #[test]
    fn replacement_queues_old_and_claims_new_atomically() {
        let mut conn = pending_claim_connection();
        insert_claim_event(&conn, "event").unwrap();
        bind_meeting(&conn, "event", "old");
        let binding = pending_claim_binding();
        insert_pending_claim_fixture(&conn, &binding);

        let transaction = conn.transaction().unwrap();
        let cleanup_id = replace_meet_binding_ownership(&transaction, "event", &binding)
            .unwrap()
            .unwrap();
        transaction.commit().unwrap();

        let bound = db::meet_meetings::get(&conn, "event").unwrap().unwrap();
        assert_eq!(bound.meeting_id, "meeting");
        assert!(db::meet_pending_meetings::get(&conn, &binding.lifecycle_id)
            .unwrap()
            .is_none());
        let cleanup = db::meet_pending_meetings::get(&conn, &cleanup_id)
            .unwrap()
            .unwrap();
        assert_eq!(cleanup.meeting_id, "old");
    }

    #[test]
    fn failed_replacement_rolls_back_without_cleanup_row() {
        let mut conn = pending_claim_connection();
        insert_claim_event(&conn, "event").unwrap();
        bind_meeting(&conn, "event", "old");
        let binding = pending_claim_binding();
        insert_pending_claim_fixture(&conn, &binding);
        let mut forged = binding.clone();
        forged.meeting_id = "forged".into();

        let transaction = conn.transaction().unwrap();
        assert!(replace_meet_binding_ownership(&transaction, "event", &forged).is_err());
        transaction.rollback().unwrap();

        let bound = db::meet_meetings::get(&conn, "event").unwrap().unwrap();
        assert_eq!(bound.meeting_id, "old");
        let pending = db::meet_pending_meetings::list(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].lifecycle_id, binding.lifecycle_id);
    }

    #[test]
    fn deletion_queues_bound_meeting_before_cascade() {
        let mut conn = pending_claim_connection();
        insert_claim_event(&conn, "event").unwrap();
        bind_meeting(&conn, "event", "old");

        let transaction = conn.transaction().unwrap();
        let cleanup_id = db::calendar_event_deletion::delete_event(&transaction, "event")
            .unwrap()
            .cleanup_lifecycle_ids
            .pop()
            .unwrap();
        transaction.commit().unwrap();

        assert!(db::calendar::get_event(&conn, "event").is_err());
        assert!(db::meet_meetings::get(&conn, "event").unwrap().is_none());
        let cleanup = db::meet_pending_meetings::get(&conn, &cleanup_id)
            .unwrap()
            .unwrap();
        assert_eq!(cleanup.meeting_id, "old");
    }

    #[test]
    fn deletion_rollback_preserves_event_and_bound_meeting() {
        let mut conn = pending_claim_connection();
        insert_claim_event(&conn, "event").unwrap();
        bind_meeting(&conn, "event", "old");

        let transaction = conn.transaction().unwrap();
        db::calendar_event_deletion::delete_event(&transaction, "event").unwrap();
        transaction.rollback().unwrap();

        assert!(db::calendar::get_event(&conn, "event").is_ok());
        assert_eq!(
            db::meet_meetings::get(&conn, "event")
                .unwrap()
                .unwrap()
                .meeting_id,
            "old"
        );
        assert!(db::meet_pending_meetings::list(&conn).unwrap().is_empty());
    }

    #[test]
    fn duplicate_claim_cannot_bind_two_events() {
        let mut conn = pending_claim_connection();
        let binding = pending_claim_binding();
        insert_pending_claim_fixture(&conn, &binding);

        let first = conn.transaction().unwrap();
        insert_claim_event(&first, "event-one").unwrap();
        let pending = matching_pending_meeting(&first, &binding).unwrap();
        transfer_pending_meeting(&first, "event-one", &binding, pending).unwrap();
        first.commit().unwrap();

        let second = conn.transaction().unwrap();
        insert_claim_event(&second, "event-two").unwrap();
        assert!(matching_pending_meeting(&second, &binding).is_err());
        second.rollback().unwrap();

        assert!(db::meet_meetings::get(&conn, "event-one")
            .unwrap()
            .is_some());
        assert!(db::calendar::get_event(&conn, "event-two").is_err());
        assert!(db::meet_meetings::get(&conn, "event-two")
            .unwrap()
            .is_none());
    }
}
