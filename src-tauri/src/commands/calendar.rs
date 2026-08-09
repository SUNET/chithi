use serde::Deserialize;
use tauri::State;

use crate::backend::calendar::{
    AttendeeResponseUpdate, CalendarBackendCtx, CalendarCapability, InviteReplyDelivery,
    InviteResponse, ParticipantSchedule, ParticipantScheduleRequest, RemoteRsvpPolicy,
    RemoteRsvpRequest, RoomAvailability, RoomAvailabilityRequest, RoomSuggestion,
};
use crate::calendar::ical::{self, ParsedInvite};
use crate::commands::sync_cmd::try_acquire_sync_guard;
use crate::db;
use crate::db::calendar::{Attendee, Calendar, CalendarEvent, Invite, NewCalendar};
use crate::error::Result;
use crate::mail::compat::BodyLocation;
use crate::meet;
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

#[derive(Debug, Deserialize)]
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

/// Frontend-supplied meet binding metadata returned from a previous
/// `meet_create_url` call. Stored verbatim in `meet_meetings` so the
/// later delete / reschedule code knows which remote meeting to act on.
#[derive(Debug, Clone, Deserialize)]
pub struct MeetBindingInput {
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
fn validate_meet_binding(conn: &rusqlite::Connection, b: &MeetBindingInput) -> Result<()> {
    if b.meeting_id.trim().is_empty() || b.join_url.trim().is_empty() {
        return Err(crate::error::Error::Other(
            "meet binding: meeting_id and join_url must be non-empty".into(),
        ));
    }
    let account = db::accounts::get_account_full(conn, &b.account_id).map_err(|_| {
        crate::error::Error::Other(format!("meet binding: unknown account {}", b.account_id))
    })?;
    let provider = meet::provider_for(&account).ok_or_else(|| {
        crate::error::Error::Other(format!(
            "meet binding: account {} has no meet provider",
            b.account_id
        ))
    })?;
    if provider.protocol() != b.protocol {
        return Err(crate::error::Error::Other(format!(
            "meet binding: protocol '{}' doesn't match account's resolved provider '{}'",
            b.protocol,
            provider.protocol()
        )));
    }
    Ok(())
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
    let conn = state.db.writer().await;
    db::calendar::delete_calendar(&conn, &calendar_id)?;
    log::info!("delete_calendar: deleted calendar {}", calendar_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Event management commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn unsubscribe_calendar(state: State<'_, AppState>, calendar_id: String) -> Result<()> {
    log::info!("unsubscribe_calendar: id={}", calendar_id);
    let conn = state.db.writer().await;
    db::calendar::set_calendar_subscribed(&conn, &calendar_id, false)?;
    let deleted = db::calendar::delete_calendar_events(&conn, &calendar_id)?;
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

#[tauri::command]
pub async fn create_event(state: State<'_, AppState>, event: NewEventInput) -> Result<String> {
    log::info!(
        "create_event: account={} calendar={} title='{}' attendees={}",
        event.account_id,
        event.calendar_id,
        event.title,
        event.attendees.len()
    );
    let id = uuid::Uuid::new_v4().to_string();

    let attendees_json = if event.attendees.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&event.attendees).unwrap_or_else(|_| "[]".to_string()))
    };

    // Get organizer email from account
    let organizer_email = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &event.account_id)
            .ok()
            .map(|a| a.email)
    };

    let cal_event = CalendarEvent {
        id: id.clone(),
        account_id: event.account_id,
        calendar_id: event.calendar_id,
        uid: Some(format!("{}@chithi", uuid::Uuid::new_v4())),
        title: event.title,
        description: event.description,
        location: event.location,
        start_time: event.start_time,
        end_time: event.end_time,
        all_day: event.all_day,
        timezone: event.timezone,
        recurrence_rule: event.recurrence_rule,
        organizer_email,
        attendees_json,
        my_status: None,
        source_message_id: None,
        ical_data: None,
        remote_id: None,
        etag: None,
    };

    let meet_binding = event.meet_binding;

    // Insert locally first, then push to server
    {
        let conn = state.db.writer().await;
        if let Some(ref b) = meet_binding {
            validate_meet_binding(&conn, b)?;
        }
        db::calendar::insert_event(&conn, &cal_event)?;
        if let Some(ref b) = meet_binding {
            db::meet_meetings::upsert(
                &conn,
                &db::meet_meetings::MeetMeeting {
                    event_id: id.clone(),
                    account_id: b.account_id.clone(),
                    protocol: b.protocol.clone(),
                    meeting_id: b.meeting_id.clone(),
                    join_url: b.join_url.clone(),
                },
            )?;
        }
        let account = db::accounts::get_account_full(&conn, &cal_event.account_id)?;

        // The event's calendar's remote handle — the JMAP backend
        // creates the event on that specific calendar; Google/Graph
        // write to their default calendar and ignore it.
        let remote_cal_id = db::calendar::get_calendar(&conn, &cal_event.calendar_id)
            .ok()
            .and_then(|c| c.remote_id)
            .unwrap_or_default();
        drop(conn); // Release lock before async push

        if let Some(backend) = crate::backend::calendar::for_account(&account) {
            if remote_cal_id.is_empty() {
                log::warn!(
                    "create_event: no remote calendar ID for local calendar '{}'",
                    cal_event.calendar_id
                );
            }
            // Best-effort: the local insert above always stands; a failed
            // push is logged and the event goes out with a later sync.
            let ctx = calendar_backend_ctx(&state);
            match backend
                .push_created_event(&ctx, &account, &cal_event, &remote_cal_id)
                .await
            {
                Ok(Some(pushed)) => {
                    log::info!(
                        "create_event: pushed via {}, remote_id={}",
                        backend.protocol(),
                        pushed.remote_id
                    );
                    let conn = state.db.writer().await;
                    conn.execute(
                        "UPDATE calendar_events SET remote_id = ?1 WHERE id = ?2",
                        rusqlite::params![pushed.remote_id, id],
                    )
                    .ok();
                    // Update the local UID to the server's canonical UID
                    // (Google iCalUID / Exchange iCalUid) so incoming RSVP
                    // replies can be matched back to the event.
                    if let Some(ref canonical_uid) = pushed.canonical_uid {
                        conn.execute(
                            "UPDATE calendar_events SET uid = ?1 WHERE id = ?2",
                            rusqlite::params![canonical_uid, id],
                        )
                        .ok();
                        log::info!(
                            "create_event: updated local UID to canonical UID={}",
                            canonical_uid
                        );
                    }
                }
                Ok(None) => {} // provider defers the push to its next sync
                Err(e) => log::error!("create_event: {} push failed: {}", backend.protocol(), e),
            }
        }
    }

    // Re-apply the event title to the meet provider's meeting topic.
    // The frontend creates the meeting at "Add video link" time, when
    // the title input is often still empty, so the remote room ends
    // up named "Meeting" until we sync the final title here.
    if let Some(ref b) = meet_binding {
        sync_meet_topic(&state, b, &cal_event.title).await;
    }

    log::info!("create_event: created event id={}", id);
    Ok(id)
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
    log::info!("update_event: id={}", event_id);
    let conn = state.db.writer().await;

    // Load existing event, apply updates
    let mut existing = db::calendar::get_event(&conn, &event_id)?;
    let prev_start = existing.start_time.clone();
    let prev_end = existing.end_time.clone();
    let prev_title = existing.title.clone();

    if let Some(calendar_id) = event.calendar_id {
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

    db::calendar::update_event(&conn, &existing)?;
    log::info!("update_event: updated event {}", event_id);

    // Persist a freshly attached meet binding, if any. Replaces a prior
    // binding for the same event (one per event).
    if let Some(ref b) = event.meet_binding {
        validate_meet_binding(&conn, b)?;
        db::meet_meetings::upsert(
            &conn,
            &db::meet_meetings::MeetMeeting {
                event_id: event_id.clone(),
                account_id: b.account_id.clone(),
                protocol: b.protocol.clone(),
                meeting_id: b.meeting_id.clone(),
                join_url: b.join_url.clone(),
            },
        )?;
    }

    // If start/end changed and the event has a meet binding, ask the
    // provider to move the remote meeting to the new slot. Best-effort:
    // a provider failure logs but doesn't block the local update, since
    // the user can still open the room via the saved join URL.
    let time_changed = prev_start != existing.start_time || prev_end != existing.end_time;
    let reschedule_with = if time_changed && !existing.all_day {
        match db::meet_meetings::get(&conn, &event_id)? {
            Some(b) => db::accounts::get_account_full(&conn, &b.account_id)
                .ok()
                .map(|acc| (b, acc)),
            None => None,
        }
    } else {
        None
    };

    let account = db::accounts::get_account_full(&conn, &existing.account_id)?;
    drop(conn);

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
                    &meet_provider_ctx(&state),
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

    // Push update to server. Best-effort: the local update above stands
    // either way. JMAP and CalDAV backends are deliberate no-ops here
    // (their impls inherit the trait default — see ADR 0050).
    if let Some(remote_id) = existing.remote_id.as_ref().filter(|r| !r.is_empty()) {
        if let Some(backend) = crate::backend::calendar::for_account(&account) {
            match backend
                .push_updated_event(
                    &calendar_backend_ctx(&state),
                    &account,
                    remote_id,
                    &existing,
                )
                .await
            {
                Ok(()) => log::info!("update_event: pushed via {}", backend.protocol()),
                Err(e) => log::error!("update_event: {} push failed: {}", backend.protocol(), e),
            }
        }
    }

    // Sync title to meet provider when the title changed, or a new
    // binding was just attached (the user may have clicked "Add
    // video link" with an empty title). Cheaper to look the binding
    // up fresh than to pipe one through from the writer scope above.
    let title_changed = prev_title != existing.title;
    let just_attached = event.meet_binding.is_some();
    if title_changed || just_attached {
        let binding = {
            let conn = state.db.reader();
            db::meet_meetings::get(&conn, &event_id)?
        };
        if let Some(b) = binding {
            let input = MeetBindingInput {
                account_id: b.account_id,
                protocol: b.protocol,
                meeting_id: b.meeting_id,
                join_url: b.join_url,
            };
            sync_meet_topic(&state, &input, &existing.title).await;
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn delete_event(state: State<'_, AppState>, event_id: String) -> Result<()> {
    log::info!("delete_event: id={}", event_id);

    // Look up the event, account, calendar remote_id, and any meet
    // binding. The meet binding has to be read *before* the event row
    // is deleted, otherwise the CASCADE drops it and we lose track of
    // which remote meeting to cancel.
    let (event, account, cal_remote_id, meet_cleanup) = {
        let conn = state.db.reader();
        let evt = db::calendar::get_event(&conn, &event_id)?;
        let acc = db::accounts::get_account_full(&conn, &evt.account_id)?;
        let cal = db::calendar::get_calendar(&conn, &evt.calendar_id).ok();
        let cal_rid = cal
            .and_then(|c| c.remote_id)
            .unwrap_or_else(|| "primary".to_string());
        let cleanup = match db::meet_meetings::get(&conn, &event_id)? {
            Some(b) => db::accounts::get_account_full(&conn, &b.account_id)
                .ok()
                .map(|meet_acc| (b, meet_acc)),
            None => None,
        };
        (evt, acc, cal_rid, cleanup)
    };

    // Best-effort delete on the meet provider. Failure logs but
    // doesn't block the rest of the event deletion: an undeletable
    // remote room is annoying, an undeletable calendar event is worse.
    if let Some((binding, meet_account)) = meet_cleanup {
        if let Some(provider) = meet::provider_for(&meet_account) {
            log::info!(
                "delete_event: deleting {} meeting {}",
                binding.protocol,
                binding.meeting_id,
            );
            if let Err(e) = provider
                .delete_meeting(
                    &meet_provider_ctx(&state),
                    &meet_account,
                    &binding.meeting_id,
                )
                .await
            {
                log::warn!("delete_event: meet provider delete failed: {}", e);
            }
        }
    }

    // Delete from server if event has a remote_id. Best-effort: the
    // local delete below proceeds even if the remote delete fails (an
    // undeletable remote copy is less bad than a local ghost event).
    if let Some(ref remote_id) = event.remote_id {
        if !remote_id.is_empty() {
            if let Some(backend) = crate::backend::calendar::for_account(&account) {
                match backend
                    .push_deleted_event(
                        &calendar_backend_ctx(&state),
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

    let conn = state.db.writer().await;
    db::calendar::delete_event(&conn, &event_id)?;
    log::info!("delete_event: deleted event {}", event_id);
    Ok(())
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

    let account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    // Gate on the per-account toggle before any side effects. Running the
    // force_full_sync token-clearing below for a disabled account would
    // make the *next* sync after re-enabling do an unnecessary full sync.
    if !account.calendar_sync_enabled {
        log::info!(
            "sync_calendars: skipping account {} (calendar sync disabled)",
            account_id
        );
        return Ok(());
    }

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

    sync_result?;

    log::info!("sync_calendars: completed for account {}", account_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Invite handling commands
// ---------------------------------------------------------------------------

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

    // Step 1: Parse the invite from the email
    let (raw, account) = {
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
        (raw, account)
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

    apply_invite_response(
        &app,
        &state,
        account_id,
        account,
        invite,
        invite_uid,
        response,
        Some(message_id),
    )
    .await
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
) -> Result<()> {
    let response = InviteResponse::try_from(response.as_str())?;
    let response_text = response.as_str();
    let backend = crate::backend::calendar::for_account(&account);
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
    };

    // Step 2: Generate the iTIP REPLY
    let reply_ical = ical::generate_reply(invite, &account.email, response_text);

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
                    organizer_email,
                    &subject,
                    &body_text,
                    &reply_ical,
                )?;

                jmap_conn.send_email(&jmap_config, &raw_message).await?;
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

    // Step 4: Create/update event in local calendar
    let my_status = response_text.to_string();
    let conn = state.db.writer().await;

    // Find the best calendar for this account: prefer default, then any with
    // a remote_id (synced from server), then any existing, finally create one.
    let calendars = db::calendar::list_calendars(&conn, &account_id)?;
    let calendar_id = if let Some(cal) = calendars
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
    let attendees_json = if invite.attendees.is_empty() {
        None
    } else {
        let mut attendees = invite.attendees.clone();
        for att in attendees.iter_mut() {
            if att.email.eq_ignore_ascii_case(&account.email) {
                att.status = my_status.clone();
            }
        }
        Some(serde_json::to_string(&attendees).unwrap_or_else(|_| "[]".to_string()))
    };

    // Check if we already have this event
    if let Some(mut existing) = db::calendar::get_event_by_uid(&conn, &account_id, &invite_uid)? {
        existing.my_status = Some(my_status);
        existing.attendees_json = attendees_json;
        db::calendar::update_event(&conn, &existing)?;
        log::info!(
            "apply_invite_response: updated existing event {} status={}",
            existing.id,
            response_text
        );
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
    }

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
            "UPDATE calendar_events SET remote_id = ?1 WHERE uid = ?2 AND account_id = ?3 AND (remote_id IS NULL OR remote_id = '')",
            rusqlite::params![remote_id, invite_uid, account_id],
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
            "UPDATE calendar_events SET remote_id = ?1 WHERE uid = ?2 AND account_id = ?3",
            rusqlite::params![remote_id, invite_uid, account_id],
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

    let (event, account) = {
        let conn = state.db.reader();
        let event = db::calendar::get_event(&conn, &event_id)?;
        let account = db::accounts::get_account_full(&conn, &account_id)?;
        (event, account)
    };

    // Reject cross-account requests: the event must belong to the account
    // we are about to RSVP as, otherwise the reply would be sent from the
    // wrong identity.
    if event.account_id != account_id {
        return Err(crate::error::Error::Other(
            "Event does not belong to the specified account".to_string(),
        ));
    }

    let uid = event.uid.clone().ok_or_else(|| {
        crate::error::Error::Other("Event has no UID; cannot send an RSVP".to_string())
    })?;

    let invite = event_to_parsed_invite(&event, &uid);
    let source_message_id = event.source_message_id.clone();

    apply_invite_response(
        &app,
        &state,
        account_id,
        account,
        &invite,
        uid,
        response,
        source_message_id,
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
    to: &str,
    subject: &str,
    body_text: &str,
    ical_reply: &str,
) -> Result<Vec<u8>> {
    use lettre::message::{header::ContentType, Mailbox, MultiPart, SinglePart};
    use lettre::Message;

    let from_mailbox: Mailbox = from.parse().map_err(|e| {
        crate::error::Error::Other(format!("Invalid 'from' address '{}': {}", from, e))
    })?;
    let to_mailbox: Mailbox = to
        .parse()
        .map_err(|e| crate::error::Error::Other(format!("Invalid 'to' address '{}': {}", to, e)))?;

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
    use lettre::transport::smtp::authentication::{Credentials, Mechanism};
    use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

    log::info!(
        "send_raw_smtp: from={} to={} via {}:{} (xoauth2={})",
        from,
        to,
        smtp_host,
        smtp_port,
        use_xoauth2,
    );

    let creds = Credentials::new(username.to_string(), password.to_string());
    let auth_mechanisms = if use_xoauth2 {
        vec![Mechanism::Xoauth2]
    } else {
        vec![Mechanism::Plain, Mechanism::Login]
    };

    let transport = if smtp_port == 587 {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(smtp_host)
            .map_err(|e| crate::error::Error::Other(format!("SMTP setup failed: {}", e)))?
            .port(smtp_port)
            .credentials(creds)
            .authentication(auth_mechanisms)
            .build()
    } else if use_tls || smtp_port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(smtp_host)
            .map_err(|e| crate::error::Error::Other(format!("SMTP setup failed: {}", e)))?
            .port(smtp_port)
            .credentials(creds)
            .authentication(auth_mechanisms)
            .build()
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(smtp_host)
            .map_err(|e| crate::error::Error::Other(format!("SMTP setup failed: {}", e)))?
            .port(smtp_port)
            .credentials(creds)
            .authentication(auth_mechanisms)
            .build()
    };

    // Build an envelope from the from/to addresses
    let from_addr: lettre::Address = from
        .parse()
        .map_err(|e| crate::error::Error::Other(format!("Invalid from address: {}", e)))?;
    let to_addr: lettre::Address = to
        .parse()
        .map_err(|e| crate::error::Error::Other(format!("Invalid to address: {}", e)))?;

    let envelope = lettre::address::Envelope::new(Some(from_addr), vec![to_addr])
        .map_err(|e| crate::error::Error::Other(format!("Failed to create envelope: {}", e)))?;

    transport
        .send_raw(&envelope, raw_message)
        .await
        .map_err(|e| {
            log::error!("SMTP send failed: {}", e);
            crate::error::Error::Other(format!("SMTP send failed: {}", e))
        })?;

    log::info!("send_raw_smtp: message sent successfully");
    Ok(())
}

/// Send meeting invite emails to attendees for a calendar event.
#[tauri::command]
pub async fn send_invites(
    state: State<'_, AppState>,
    account_id: String,
    event_id: String,
    attendee_emails: Vec<String>,
) -> Result<()> {
    log::info!(
        "send_invites: account={} event={} attendees={:?}",
        account_id,
        event_id,
        attendee_emails
    );

    let (account, event) = {
        let conn = state.db.writer().await;
        let acc = db::accounts::get_account_full(&conn, &account_id)?;
        let evt = db::calendar::get_event(&conn, &event_id)?;
        (acc, evt)
    };

    // Gmail and O365 handle sending invite emails server-side when
    // events are pushed via Google Calendar API (sendUpdates=all) or
    // Graph API. Sending our own SMTP invite would create duplicates.
    if account.calendar_protocol_str() == "google" || account.calendar_protocol_str() == "graph" {
        log::info!(
            "send_invites: skipping manual send for {} account (server handles invites)",
            account.calendar_protocol_str()
        );
        // Still update attendees in the local DB
        let conn = state.db.writer().await;
        let attendees_json = serde_json::to_string(
            &attendee_emails
                .iter()
                .map(|e| Attendee {
                    email: e.clone(),
                    name: None,
                    status: "needs-action".to_string(),
                })
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default();
        conn.execute(
            "UPDATE calendar_events SET attendees_json = ?1 WHERE id = ?2",
            rusqlite::params![attendees_json, event_id],
        )
        .ok();
        return Ok(());
    }

    let attendees: Vec<Attendee> = attendee_emails
        .iter()
        .map(|email| Attendee {
            email: email.clone(),
            name: None,
            status: "needs-action".to_string(),
        })
        .collect();

    let uid = event.uid.as_deref().unwrap_or(&event_id);
    let ical = ical::generate_invite(
        uid,
        &event.title,
        &event.start_time,
        &event.end_time,
        event.location.as_deref(),
        event.description.as_deref(),
        &account.email,
        None, // Use email as organizer name — display_name is the account label, not a person's name
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
        let raw = build_invite_message(&account.email, attendee_email, &subject, &body_text, &ical);

        if raw.is_empty() {
            log::error!(
                "send_invites: failed to build invite message for {}",
                attendee_email
            );
            continue;
        }

        if account.calendar_protocol_str() == "jmap" {
            let (jmap_config, conn_jmap) = state.providers.jmap_client(&account).await?;
            conn_jmap.send_email(&jmap_config, &raw).await?;
        } else {
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
                attendee_email,
                &raw,
            )
            .await?;
        }
        log::info!("send_invites: sent to {}", attendee_email);
    }

    // Update event's attendees in local DB
    {
        let conn = state.db.writer().await;
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

/// Process a METHOD:CANCEL email — delete the matching local event.
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

    let conn = state.db.writer().await;
    let mut deleted = 0;
    for cancel in &cancels {
        if let Some(event) = db::calendar::get_event_by_uid(&conn, &account_id, &cancel.uid)? {
            // Verify the CANCEL's organizer matches the event's organizer to
            // prevent spoofed CANCEL emails from deleting events.
            if let Some(ref cancel_org) = cancel.organizer_email {
                if let Some(ref event_org) = event.organizer_email {
                    if cancel_org.to_lowercase() != event_org.to_lowercase() {
                        log::warn!(
                            "process_cancelled_invite: organizer mismatch for UID={} (cancel={}, event={}), skipping",
                            cancel.uid, cancel_org, event_org
                        );
                        continue;
                    }
                }
            }
            db::calendar::delete_event(&conn, &event.id)?;
            deleted += 1;
            log::info!(
                "process_cancelled_invite: deleted event '{}' (UID={})",
                event.title,
                cancel.uid
            );
        }
    }

    if deleted > 0 {
        use tauri::Emitter as _;
        app.emit("calendar-changed", account_id.as_str()).ok();
    }

    log::info!(
        "process_cancelled_invite: completed for account {}",
        account_id
    );
    Ok(())
}

/// Auto-process calendar emails (METHOD:REPLY and METHOD:CANCEL) found
/// during mail sync. Called after new messages are synced for an account.
/// This enables Thunderbird-style automatic invite processing without
/// requiring the user to open each reply/cancel email.
pub fn auto_process_calendar_emails(
    app: &tauri::AppHandle,
    db: &std::sync::Arc<crate::db::pool::DbPool>,
    account_id: &str,
    data_dir: &std::path::Path,
    new_message_ids: &[String],
) {
    if new_message_ids.is_empty() {
        return;
    }

    // Phase 1: read-only — gather all invites from new messages.
    // Uses only the reader connection (no writer lock needed yet).
    let mut all_invites: Vec<ParsedInvite> = Vec::new();

    // Canonicalise the base once for the whole loop; individual paths are
    // still validated per-iteration against this canonical base.
    let canonical_data_dir = match std::fs::canonicalize(data_dir) {
        Ok(p) => p,
        Err(e) => {
            log::warn!(
                "auto_process_calendar_emails: cannot canonicalise data dir {}: {}",
                data_dir.display(),
                e
            );
            return;
        }
    };

    for msg_id in new_message_ids {
        let maildir_path = {
            let conn = db.reader();
            match db::messages::get_message_metadata(&conn, account_id, msg_id) {
                Ok((path, _, _, _, _, _, _)) => path,
                Err(_) => continue,
            }
        };

        if BodyLocation::from_persisted(&maildir_path).needs_fetch() {
            continue; // Body not fetched yet
        }

        let full_path = match crate::path_validation::resolve_under_canonical(
            &canonical_data_dir,
            &maildir_path,
        ) {
            Ok(p) => p,
            Err(e) => {
                log::warn!(
                    "auto_process_calendar_emails: rejecting maildir path for msg {}: {}",
                    msg_id,
                    e
                );
                continue;
            }
        };

        let raw = match std::fs::read(&full_path) {
            Ok(data) => data,
            Err(_) => continue,
        };

        let invites = ical::parse_ical_from_email(&raw);
        all_invites.extend(invites);
    }

    if all_invites.is_empty() {
        return;
    }

    // Phase 2: acquire the writer ONCE and batch all calendar updates.
    let conn_w = tokio::runtime::Handle::current().block_on(db.writer());
    let mut calendar_changed = false;

    for invite in &all_invites {
        match invite.method.to_uppercase().as_str() {
            "REPLY" => {
                // Update attendee status on organizer's local event
                if let Some(event) =
                    db::calendar::get_event_by_uid(&conn_w, account_id, &invite.uid)
                        .ok()
                        .flatten()
                {
                    for attendee in &invite.attendees {
                        if let Some(ref att_json) = event.attendees_json {
                            if let Ok(mut attendees) =
                                serde_json::from_str::<Vec<serde_json::Value>>(att_json)
                            {
                                let mut updated = false;
                                for att in attendees.iter_mut() {
                                    if att["email"].as_str() == Some(&attendee.email) {
                                        att["status"] = serde_json::json!(&attendee.status);
                                        updated = true;
                                    }
                                }
                                if updated {
                                    let updated_json =
                                        serde_json::to_string(&attendees).unwrap_or_default();
                                    conn_w.execute(
                                        "UPDATE calendar_events SET attendees_json = ?1 WHERE id = ?2",
                                        rusqlite::params![updated_json, event.id],
                                    ).ok();
                                    log::info!(
                                        "auto_process: {} responded '{}' to event '{}'",
                                        attendee.email,
                                        attendee.status,
                                        event.title
                                    );
                                    calendar_changed = true;
                                }
                            }
                        }
                    }
                }
            }
            "CANCEL" => {
                // Delete cancelled event
                if let Some(event) =
                    db::calendar::get_event_by_uid(&conn_w, account_id, &invite.uid)
                        .ok()
                        .flatten()
                {
                    if db::calendar::delete_event(&conn_w, &event.id).is_ok() {
                        log::info!(
                            "auto_process: deleted cancelled event '{}' (UID={})",
                            event.title,
                            invite.uid
                        );
                        calendar_changed = true;
                    }
                }
            }
            _ => {} // REQUEST etc. handled by user interaction
        }
    }

    // Release writer before emitting events
    drop(conn_w);

    if calendar_changed {
        use tauri::Emitter;
        app.emit("calendar-changed", account_id).ok();
    }
}

fn build_invite_message(
    from: &str,
    to: &str,
    subject: &str,
    body_text: &str,
    ical_data: &str,
) -> Vec<u8> {
    use lettre::message::{header::ContentType, Mailbox, MultiPart, SinglePart};
    use lettre::Message;

    let from_mailbox: Mailbox = match from.parse() {
        Ok(m) => m,
        Err(e) => {
            log::error!("build_invite_message: invalid from '{}': {}", from, e);
            return Vec::new();
        }
    };
    let to_mailbox: Mailbox = match to.parse() {
        Ok(m) => m,
        Err(e) => {
            log::error!("build_invite_message: invalid to '{}': {}", to, e);
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
mod tests {
    use super::*;

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
}
