//! Google calendar backend (Calendar API v3 with OAuth2).

use async_trait::async_trait;
use rusqlite::OptionalExtension;

use crate::calendar::recurrence_identity::{
    OccurrenceFields, RecurrenceIdentitySeed, RecurrenceObjectKind, RecurrenceValueType,
};
use crate::calendar::{Attendee, CalendarEvent, RecurrenceKind};
use crate::db;
use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::mail::google::{
    event_patch_to_google_json, event_to_google_json, google_recurrence_kind,
    invitation_copy_patch_to_google_json, occurrence_patch_to_google_json, send_updates_for,
    EventsPage, GoogleClient,
};

use super::{
    BusyPeriod, CalendarBackend, CalendarBackendCtx, CalendarCapability, ParticipantSchedule,
    ParticipantScheduleRequest, PushedEvent, RemoteOccurrenceUpdate, RemoteOccurrenceUpdateOutcome,
    RemoteRsvpOutcome, RemoteRsvpPolicy, RemoteRsvpRequest,
};

pub struct GoogleCalendarBackend;

/// Incremental tokens cannot refresh unchanged rows created before recurrence
/// classification existed. Local-only rows do not need provider metadata reads.
fn needs_recurrence_refresh(
    conn: &rusqlite::Connection,
    account_id: &str,
    calendar_id: &str,
) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM calendar_events
         WHERE account_id = ?1 AND calendar_id = ?2
           AND remote_id IS NOT NULL AND remote_id != ''
           AND recurrence_kind = 'unknown')",
        rusqlite::params![account_id, calendar_id],
        |row| row.get(0),
    )?)
}

/// Supplement incremental sync with one bounded metadata read. Only positively
/// classified cached unknown rows are updated; cursors, event contents, and
/// deletions belong exclusively to the normal sync stream.
async fn refresh_recurrence(
    db: &db::pool::DbPool,
    client: &GoogleClient,
    account_id: &str,
    calendar_id: &str,
    remote_calendar_id: &str,
) -> Result<()> {
    let now = chrono::Utc::now();
    let time_min = (now - chrono::Duration::days(30)).to_rfc3339();
    let time_max = (now + chrono::Duration::days(180)).to_rfc3339();
    let data = match client
        .list_events_full(remote_calendar_id, &time_min, &time_max)
        .await?
    {
        EventsPage::Events(data) => data,
        EventsPage::SyncTokenExpired => {
            return Err(Error::Other(
                "Google recurrence metadata read returned HTTP 410 without a sync token".into(),
            ));
        }
    };
    let conn = db.writer().await;
    for event in &data.items {
        // A cancelled resource may contain only its ID. Its missing recurrence
        // fields are not positive standalone evidence, nor is this a deletion
        // stream: the normal incremental read applies cancellation tombstones.
        if event["status"].as_str() == Some("cancelled") {
            continue;
        }
        let kind = google_recurrence_kind(event);
        if kind == RecurrenceKind::Unknown {
            continue;
        }
        let Some(remote_id) = event["id"].as_str() else {
            continue;
        };
        let local_id: Option<String> = conn
            .query_row(
                "SELECT id FROM calendar_events
                 WHERE account_id = ?1 AND calendar_id = ?2 AND remote_id = ?3
                   AND recurrence_kind = 'unknown'",
                rusqlite::params![account_id, calendar_id, remote_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(local_id) = local_id else {
            continue;
        };
        let mut cached = db::calendar::get_event(&conn, &local_id)?;
        cached.recurrence_kind = kind;
        upsert_google_event(&conn, event, &cached, remote_calendar_id)?;
    }
    Ok(())
}

fn google_response_status(status: Option<&str>) -> String {
    match status {
        Some("accepted") => "accepted",
        Some("tentative") => "tentative",
        Some("declined") => "declined",
        _ => "needs-action",
    }
    .to_string()
}

fn google_recurrence_seed(
    source: &serde_json::Value,
    event: &CalendarEvent,
    provider_calendar_id: &str,
) -> Option<RecurrenceIdentitySeed> {
    let native_data = serde_json::to_string(source).ok()?;
    let revision = source["etag"].as_str().map(str::to_string);
    let seed = match event.recurrence_kind {
        RecurrenceKind::Series => RecurrenceIdentitySeed {
            local_series_event_id: None,
            provider_calendar_id: Some(provider_calendar_id.to_string()),
            provider_series_id: Some(source["id"].as_str()?.to_string()),
            provider_occurrence_id: None,
            recurrence_id: None,
            recurrence_timezone: None,
            recurrence_value_type: None,
            occurrence: occurrence_fields(event),
            provider_native_data: Some(native_data),
            provider_revision: revision,
            kind: RecurrenceObjectKind::Master,
        },
        RecurrenceKind::Occurrence => {
            let (recurrence_id, value_type, recurrence_timezone) = google_original_start(source)?;
            RecurrenceIdentitySeed {
                local_series_event_id: None,
                provider_calendar_id: Some(provider_calendar_id.to_string()),
                provider_series_id: Some(source["recurringEventId"].as_str()?.to_string()),
                provider_occurrence_id: Some(source["id"].as_str()?.to_string()),
                recurrence_id: Some(recurrence_id),
                recurrence_timezone,
                recurrence_value_type: Some(value_type),
                occurrence: occurrence_fields(event),
                provider_native_data: Some(native_data),
                provider_revision: revision,
                kind: RecurrenceObjectKind::Occurrence,
            }
        }
        RecurrenceKind::Unknown | RecurrenceKind::Standalone => return None,
    };
    seed.validate().ok()?;
    Some(seed)
}

fn occurrence_fields(event: &CalendarEvent) -> OccurrenceFields {
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

fn google_original_start(
    source: &serde_json::Value,
) -> Option<(String, RecurrenceValueType, Option<String>)> {
    let original = source.get("originalStartTime")?.as_object()?;
    let (recurrence_id, value_type) = match (
        original.get("date").and_then(serde_json::Value::as_str),
        original.get("dateTime").and_then(serde_json::Value::as_str),
    ) {
        (Some(date), None) if !date.is_empty() => (date.into(), RecurrenceValueType::Date),
        (None, Some(datetime)) if !datetime.is_empty() => {
            (datetime.into(), RecurrenceValueType::DateTime)
        }
        _ => return None,
    };
    let timezone = match original.get("timeZone") {
        None => None,
        Some(serde_json::Value::String(value)) if !value.is_empty() => Some(value.clone()),
        _ => return None,
    };
    Some((recurrence_id, value_type, timezone))
}

fn canonical_google_occurrence(
    source: &serde_json::Value,
    account: &AccountFull,
    current: &CalendarEvent,
) -> Result<CalendarEvent> {
    if google_recurrence_kind(source) != RecurrenceKind::Occurrence {
        return Err(Error::Sync(
            "Google returned an unclassified canonical occurrence; reconciliation required".into(),
        ));
    }
    let title = source["summary"].as_str().ok_or_else(|| {
        Error::Sync(
            "Google canonical occurrence omitted its summary; reconciliation required".into(),
        )
    })?;
    let start_timezone = source["start"]["timeZone"].as_str().map(str::to_string);
    let (start_time, all_day) = match (
        source["start"]["date"].as_str(),
        source["start"]["dateTime"].as_str(),
    ) {
        (Some(date), None) => (date.to_string(), true),
        (None, Some(datetime)) => (
            crate::calendar::timezone::to_utc(datetime, start_timezone.as_deref().unwrap_or("")),
            false,
        ),
        _ => {
            return Err(Error::Sync(
                "Google canonical occurrence has an invalid start; reconciliation required".into(),
            ));
        }
    };
    let end_time = match (
        source["end"]["date"].as_str(),
        source["end"]["dateTime"].as_str(),
    ) {
        (Some(date), None) if all_day => date.to_string(),
        (None, Some(datetime)) if !all_day => crate::calendar::timezone::to_utc(
            datetime,
            source["end"]["timeZone"].as_str().unwrap_or(""),
        ),
        _ => {
            return Err(Error::Sync(
                "Google canonical occurrence has an invalid end; reconciliation required".into(),
            ));
        }
    };
    let (attendees_json, my_status) = if source.get("attendees").is_some() {
        parse_google_attendees(source, &account.email, true)
    } else {
        (current.attendees_json.clone(), current.my_status.clone())
    };
    let organizer_email = if source.get("organizer").is_some() {
        parse_google_organizer(source, &account.email, true)
    } else {
        current.organizer_email.clone()
    };
    let optional_string = |name: &str| -> Result<Option<String>> {
        match source.get(name) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err(Error::Sync(format!(
                "Google canonical occurrence has an invalid {name}; reconciliation required"
            ))),
        }
    };

    Ok(CalendarEvent {
        id: current.id.clone(),
        account_id: current.account_id.clone(),
        calendar_id: current.calendar_id.clone(),
        uid: if source.get("iCalUID").is_some() {
            optional_string("iCalUID")?
        } else {
            current.uid.clone()
        },
        title: title.into(),
        description: optional_string("description")?,
        location: optional_string("location")?,
        start_time,
        end_time,
        all_day,
        timezone: start_timezone,
        recurrence_rule: current.recurrence_rule.clone(),
        recurrence_kind: RecurrenceKind::Occurrence,
        organizer_email,
        attendees_json,
        my_status,
        source_message_id: current.source_message_id.clone(),
        ical_data: current.ical_data.clone(),
        remote_id: Some(
            source["id"]
                .as_str()
                .ok_or_else(|| {
                    Error::Sync(
                        "Google canonical occurrence omitted its ID; reconciliation required"
                            .into(),
                    )
                })?
                .into(),
        ),
        etag: Some(
            source["etag"]
                .as_str()
                .ok_or_else(|| {
                    Error::Sync(
                        "Google canonical occurrence omitted its ETag; reconciliation required"
                            .into(),
                    )
                })?
                .into(),
        ),
    })
}

fn upsert_google_event(
    conn: &rusqlite::Connection,
    source: &serde_json::Value,
    event: &CalendarEvent,
    provider_calendar_id: &str,
) -> Result<()> {
    match event.recurrence_kind {
        RecurrenceKind::Standalone => {
            db::calendar::upsert_event_by_remote_id_with_recurrence(conn, event, &[]).map(|_| ())
        }
        RecurrenceKind::Series | RecurrenceKind::Occurrence => {
            if let Some(seed) = google_recurrence_seed(source, event, provider_calendar_id) {
                db::calendar::upsert_event_by_remote_id_with_recurrence(conn, event, &[seed])
                    .map(|_| ())
            } else {
                log::warn!(
                    "sync_calendars_google: recurrence identity extraction failed for {}",
                    source["id"].as_str().unwrap_or("<unknown>")
                );
                db::calendar::upsert_event_by_remote_id(conn, event)
            }
        }
        RecurrenceKind::Unknown => db::calendar::upsert_event_by_remote_id(conn, event),
    }
}

/// Convert Google attendees to the provider-neutral representation and pick
/// the signed-in account's authoritative response status.
fn parse_google_attendees(
    event: &serde_json::Value,
    account_email: &str,
    allow_self_fallback: bool,
) -> (Option<String>, Option<String>) {
    let mut attendees = Vec::new();
    let mut self_status = None;
    let mut email_status = None;

    if let Some(values) = event["attendees"].as_array() {
        for value in values {
            let Some(email) = value["email"].as_str() else {
                continue;
            };
            let status = google_response_status(value["responseStatus"].as_str());
            let google_self = value["self"].as_bool().unwrap_or(false);
            let is_self =
                email.eq_ignore_ascii_case(account_email) || (allow_self_fallback && google_self);
            if email.eq_ignore_ascii_case(account_email) && email_status.is_none() {
                email_status = Some(status.clone());
            } else if is_self && self_status.is_none() {
                self_status = Some(status.clone());
            }
            attendees.push(Attendee {
                email: email.to_string(),
                name: value["displayName"].as_str().map(str::to_string),
                status,
                is_self: Some(is_self),
            });
        }
    }

    let attendees_json = if attendees.is_empty() {
        None
    } else {
        serde_json::to_string(&attendees).ok()
    };
    (attendees_json, email_status.or(self_status))
}

fn parse_google_organizer(
    event: &serde_json::Value,
    account_email: &str,
    is_primary_calendar: bool,
) -> Option<String> {
    if is_primary_calendar && event["organizer"]["self"].as_bool().unwrap_or(false) {
        Some(account_email.to_string())
    } else {
        event["organizer"]["email"].as_str().map(str::to_string)
    }
}

/// Pick a readable foreground color for the given background hex.
/// Used when pushing a color to Google Calendar — the API takes a
/// foreground/background pair and omitting the foreground leaves it
/// at a default that can be unreadable on a dark background. The
/// rule is the standard W3C luminance threshold: backgrounds with
/// relative luminance > 0.5 get black text, darker ones get white.
fn readable_foreground(bg_hex: &str) -> &'static str {
    fn channel(hex: &str, lo: usize, hi: usize) -> Option<f64> {
        let v = u8::from_str_radix(hex.get(lo..hi)?, 16).ok()? as f64;
        Some(v / 255.0)
    }
    let h = bg_hex.trim().trim_start_matches('#');
    if h.len() != 6 {
        return "#000000";
    }
    // Quick sRGB luminance — fine for picking black vs. white text.
    let r = channel(h, 0, 2).unwrap_or(0.0);
    let g = channel(h, 2, 4).unwrap_or(0.0);
    let b = channel(h, 4, 6).unwrap_or(0.0);
    let lum = 0.299 * r + 0.587 * g + 0.114 * b;
    if lum > 0.5 {
        "#000000"
    } else {
        "#ffffff"
    }
}

/// The account-level sync body. Split out so `sync` can wrap it with
/// the CalDAV fallback without recursing through the trait object.
async fn sync_google(ctx: &CalendarBackendCtx<'_>, account: &AccountFull) -> Result<()> {
    let db = ctx.db;
    let account_id = account.id.as_str();
    let client = ctx.services.google_client(account_id).await?;

    // Step 1: List calendars via Google Calendar API
    let data = client.list_calendar_list().await?;
    let items = data["items"].as_array();
    log::info!(
        "sync_calendars_google: fetched {} calendars",
        items.map(|i| i.len()).unwrap_or(0)
    );

    let mut remote_to_local: std::collections::HashMap<String, (String, bool)> =
        std::collections::HashMap::new();

    {
        let conn = db.writer().await;
        if let Some(calendars) = items {
            for cal in calendars {
                let cal_id = cal["id"].as_str().unwrap_or_default();
                let name = cal["summary"].as_str().unwrap_or("Calendar");
                let color = cal["backgroundColor"].as_str().unwrap_or("#4285f4");
                let is_primary = cal["primary"].as_bool().unwrap_or(false);

                let local_id = db::calendar::upsert_calendar_by_remote_id(
                    &conn, account_id, cal_id, name, color, is_primary,
                )?;
                remote_to_local.insert(cal_id.to_string(), (local_id, is_primary));
            }
        }
    }

    // Step 2: Fetch events for each calendar (with syncToken for incremental sync)
    for (remote_cal_id, (local_cal_id, is_primary)) in &remote_to_local {
        let sync_key = format!("google_sync_token_{}_{}", account_id, remote_cal_id);

        // Check for existing syncToken
        let existing_token: Option<String> = {
            let conn = db.reader();
            conn.query_row(
                "SELECT value FROM app_metadata WHERE key = ?1",
                rusqlite::params![sync_key],
                |row| row.get(0),
            )
            .ok()
        };

        let has_unknown_events = {
            let conn = db.reader();
            needs_recurrence_refresh(&conn, account_id, local_cal_id)?
        };
        // The initial normal full read already provides classification. With
        // an existing cursor, recovery supplements rather than replaces delta
        // catch-up, including when an old unknown row is outside the window.
        if has_unknown_events && existing_token.is_some() {
            if let Err(error) =
                refresh_recurrence(db, &client, account_id, local_cal_id, remote_cal_id).await
            {
                log::warn!(
                    "sync_calendars_google: recurrence refresh failed for {}: {}",
                    remote_cal_id,
                    error
                );
            }
        }
        let page = if let Some(token) = existing_token.as_deref() {
            // Incremental sync
            log::debug!(
                "sync_calendars_google: incremental sync for calendar {}",
                remote_cal_id
            );
            client.list_events_incremental(remote_cal_id, token).await
        } else {
            // Full sync
            let now = chrono::Utc::now();
            let time_min = (now - chrono::Duration::days(30)).to_rfc3339();
            let time_max = (now + chrono::Duration::days(180)).to_rfc3339();
            client
                .list_events_full(remote_cal_id, &time_min, &time_max)
                .await
        };

        let events_data = match page {
            Ok(EventsPage::Events(data)) => data,
            Ok(EventsPage::SyncTokenExpired) => {
                // syncToken expired — clear it and retry with full sync on next cycle
                log::info!(
                    "sync_calendars_google: syncToken expired for {}, will full sync next time",
                    remote_cal_id
                );
                let conn = db.writer().await;
                conn.execute(
                    "DELETE FROM app_metadata WHERE key = ?1",
                    rusqlite::params![sync_key],
                )
                .ok();
                continue;
            }
            Err(e) => {
                log::error!(
                    "sync_calendars_google: events fetch failed for {}: {}",
                    remote_cal_id,
                    e
                );
                continue;
            }
        };

        let count = events_data.items.len();
        log::info!(
            "sync_calendars_google: fetched {} events for calendar {}",
            count,
            remote_cal_id
        );

        let mut conn = db.writer().await;
        {
            for ev in &events_data.items {
                let event_id_remote = ev["id"].as_str().unwrap_or_default();

                // Incremental sync: cancelled events should be deleted locally
                if ev["status"].as_str() == Some("cancelled") {
                    let transaction = match conn.transaction() {
                        Ok(transaction) => transaction,
                        Err(_) => continue,
                    };
                    let deleted = match db::calendar_event_deletion::delete_events_by_remote_id(
                        &transaction,
                        account_id,
                        event_id_remote,
                    ) {
                        Ok(result) => result.deleted,
                        Err(_) => continue,
                    };
                    // Also delete by iCalUID for events created locally via respond_to_invite
                    if let Some(ical_uid) = ev["iCalUID"].as_str() {
                        if db::calendar_event_deletion::delete_unpushed_events_by_uid(
                            &transaction,
                            account_id,
                            ical_uid,
                        )
                        .is_err()
                        {
                            continue;
                        }
                    }
                    if transaction.commit().is_err() {
                        continue;
                    }
                    if deleted > 0 {
                        log::info!(
                            "sync_calendars_google: deleted cancelled event '{}'",
                            event_id_remote
                        );
                    }
                    continue;
                }

                let title = ev["summary"].as_str().unwrap_or("(No title)");
                let description = ev["description"].as_str().map(|s| s.to_string());
                let location = ev["location"].as_str().map(|s| s.to_string());

                // Parse start/end — can be date (all-day) or dateTime
                let start_tz = ev["start"]["timeZone"].as_str().map(|s| s.to_string());
                let (start_time, all_day) = if let Some(dt) = ev["start"]["dateTime"].as_str() {
                    (
                        crate::calendar::timezone::to_utc(dt, start_tz.as_deref().unwrap_or("")),
                        false,
                    )
                } else if let Some(d) = ev["start"]["date"].as_str() {
                    (d.to_string(), true)
                } else {
                    continue;
                };

                let end_time = if let Some(dt) = ev["end"]["dateTime"].as_str() {
                    let end_tz = ev["end"]["timeZone"].as_str().unwrap_or("");
                    crate::calendar::timezone::to_utc(dt, end_tz)
                } else if let Some(d) = ev["end"]["date"].as_str() {
                    d.to_string()
                } else {
                    start_time.clone()
                };

                let organizer_email = parse_google_organizer(ev, &account.email, *is_primary);
                let uid = ev["iCalUID"].as_str().map(|s| s.to_string());
                let (attendees_json, my_status) =
                    parse_google_attendees(ev, &account.email, *is_primary);

                let cal_event = CalendarEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    account_id: account_id.to_string(),
                    calendar_id: local_cal_id.clone(),
                    uid,
                    title: title.to_string(),
                    description,
                    location,
                    start_time,
                    end_time,
                    all_day,
                    timezone: start_tz,
                    recurrence_rule: None,
                    recurrence_kind: google_recurrence_kind(ev),
                    organizer_email,
                    attendees_json,
                    my_status,
                    source_message_id: None,
                    ical_data: None,
                    remote_id: Some(event_id_remote.to_string()),
                    etag: ev["etag"].as_str().map(|s| s.to_string()),
                };

                if let Err(e) = upsert_google_event(&conn, ev, &cal_event, remote_cal_id) {
                    log::error!("sync_calendars_google: upsert event failed: {}", e);
                }
            }
        }

        // Drop the conn lock before acquiring again for syncToken
        drop(conn);

        // Save nextSyncToken for incremental sync next time
        let conn = db.writer().await;
        conn.execute(
            "INSERT OR REPLACE INTO app_metadata (key, value) VALUES (?1, ?2)",
            rusqlite::params![sync_key, events_data.next_sync_token],
        )
        .ok();
        log::debug!(
            "sync_calendars_google: saved syncToken for calendar {}",
            remote_cal_id
        );

        // The initial list is time-bounded, so absence cannot prove deletion.
        // Incremental sync supplies explicit cancelled tombstones above.
    }

    log::info!(
        "sync_calendars_google: completed for account {}",
        account_id
    );
    Ok(())
}

#[async_trait]
impl CalendarBackend for GoogleCalendarBackend {
    fn protocol(&self) -> &'static str {
        "google"
    }

    fn event_creation_target(&self) -> super::EventCreationTarget {
        super::EventCreationTarget::AccountDefault
    }

    fn remote_rsvp_policy(&self) -> RemoteRsvpPolicy {
        RemoteRsvpPolicy::BestEffortAfterLocal
    }

    async fn apply_remote_rsvp(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        request: &RemoteRsvpRequest,
    ) -> Result<CalendarCapability<RemoteRsvpOutcome>> {
        let client = ctx.services.google_client(&account.id).await?;
        let existing_event = client
            .find_event_by_ical_uid("primary", &request.uid)
            .await
            .ok()
            .flatten();
        let mut event_id = existing_event
            .as_ref()
            .and_then(|event| event["id"].as_str())
            .map(str::to_string);

        if let Some((event, remote_id)) = existing_event.as_ref().and_then(|event| {
            event["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(|id| (event, id))
        }) {
            let attendees_patch =
                google_rsvp_attendees_patch(event, &account.email, request.response.as_str());
            match client
                .patch_event("primary", remote_id, &attendees_patch, "none")
                .await
            {
                Ok(()) => log::info!(
                    "apply_invite_response: updated Google Calendar response to {}",
                    request.response.as_str()
                ),
                Err(error) => log::warn!(
                    "apply_invite_response: Google Calendar PATCH failed: {}",
                    error
                ),
            }
        } else {
            let import_event = google_rsvp_import_event(account, request);
            match client.import_event("primary", &import_event).await {
                Ok(imported_id) => {
                    event_id = imported_id;
                    log::info!("apply_invite_response: imported event to Google Calendar");
                }
                Err(error) => log::warn!(
                    "apply_invite_response: Google Calendar import failed: {}",
                    error
                ),
            }
        }

        Ok(CalendarCapability::Supported(RemoteRsvpOutcome {
            remote_id: event_id.filter(|id| !id.is_empty()),
        }))
    }

    async fn get_participant_schedules(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        request: &ParticipantScheduleRequest,
    ) -> Result<CalendarCapability<Vec<ParticipantSchedule>>> {
        let schedules = ctx
            .services
            .google_client(&account.id)
            .await?
            .get_schedules(&request.emails, &request.start_time, &request.end_time)
            .await?;
        Ok(CalendarCapability::Supported(
            schedules
                .into_iter()
                .map(|schedule| ParticipantSchedule {
                    email: schedule.email,
                    available: schedule.available,
                    busy: schedule
                        .busy
                        .into_iter()
                        .map(|period| BusyPeriod {
                            start: period.start,
                            end: period.end,
                        })
                        .collect(),
                })
                .collect(),
        ))
    }

    async fn update_recurrence_occurrence(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        request: &RemoteOccurrenceUpdate,
    ) -> Result<RemoteOccurrenceUpdateOutcome> {
        request.desired.validate()?;
        let provider_calendar_id = request
            .trusted_identity
            .provider_calendar_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                Error::Other(
                    "Google occurrence update requires a trusted provider calendar ID".into(),
                )
            })?;
        request.trusted_identity.validate()?;
        if !matches!(
            request.trusted_identity.kind,
            RecurrenceObjectKind::Occurrence | RecurrenceObjectKind::Exception
        ) || request.trusted_identity.account_id != account.id
            || request.trusted_identity.event_id != request.current_event.id
            || request.current_event.account_id != account.id
            || request.current_event.recurrence_kind != RecurrenceKind::Occurrence
            || request.current_event.remote_id.as_deref() != Some(request.target_id.as_str())
            || request.trusted_identity.provider_occurrence_id.as_deref()
                != Some(request.target_id.as_str())
        {
            return Err(Error::Other(
                "Google occurrence update requires a trusted detached occurrence identity".into(),
            ));
        }
        let expected_etag = request
            .expected_provider_revision
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                Error::Sync(
                    "Google occurrence update requires an ETag; sync before retrying".into(),
                )
            })?;
        if request.trusted_identity.provider_revision.as_deref() != Some(expected_etag) {
            return Err(Error::Sync(
                "Google occurrence ETag is stale; sync before retrying".into(),
            ));
        }
        let native: serde_json::Value = serde_json::from_str(
            request
                .trusted_identity
                .provider_native_data
                .as_deref()
                .ok_or_else(|| {
                    Error::Other("Google occurrence identity has no provider native data".into())
                })?,
        )
        .map_err(|_| Error::Other("Google occurrence provider native data is invalid".into()))?;
        let native_original = google_original_start(&native).ok_or_else(|| {
            Error::Other("Google occurrence provider native identity is incomplete".into())
        })?;
        if google_recurrence_kind(&native) != RecurrenceKind::Occurrence
            || native["id"].as_str() != Some(request.target_id.as_str())
            || native["etag"].as_str() != Some(expected_etag)
            || native["recurringEventId"].as_str()
                != request.trusted_identity.provider_series_id.as_deref()
            || Some(native_original.0.as_str()) != request.trusted_identity.recurrence_id.as_deref()
            || Some(native_original.1) != request.trusted_identity.recurrence_value_type
            || native_original.2 != request.trusted_identity.recurrence_timezone
        {
            return Err(Error::Other(
                "Google occurrence provider native identity does not match the trusted target"
                    .into(),
            ));
        }

        let patch = occurrence_patch_to_google_json(&request.patch, &request.desired, &native)?;
        let canonical_source = ctx
            .services
            .google_client(&account.id)
            .await?
            .patch_recurrence_occurrence(
                provider_calendar_id,
                &request.target_id,
                expected_etag,
                &patch,
            )
            .await?;
        let canonical =
            canonical_google_occurrence(&canonical_source, account, &request.current_event)?;
        let mut replacement =
            google_recurrence_seed(&canonical_source, &canonical, provider_calendar_id)
                .ok_or_else(|| {
                    Error::Sync(
                    "Google returned an invalid recurrence identity after the occurrence update; \
                 reconciliation required"
                        .into(),
                )
                })?;
        if replacement.provider_calendar_id != request.trusted_identity.provider_calendar_id
            || replacement.provider_series_id != request.trusted_identity.provider_series_id
            || replacement.provider_occurrence_id != request.trusted_identity.provider_occurrence_id
            || replacement.recurrence_id != request.trusted_identity.recurrence_id
            || replacement.recurrence_timezone != request.trusted_identity.recurrence_timezone
            || replacement.recurrence_value_type != request.trusted_identity.recurrence_value_type
        {
            return Err(Error::Sync(
                "Google changed immutable recurrence identity during the occurrence update; \
                 reconciliation required"
                    .into(),
            ));
        }
        replacement.local_series_event_id = request.trusted_identity.local_series_event_id.clone();
        replacement.kind = RecurrenceObjectKind::Exception;
        replacement.validate()?;
        let occurrence = replacement.occurrence.clone();
        Ok(RemoteOccurrenceUpdateOutcome {
            replacement_identity: replacement,
            occurrence,
            canonical_event: Some(canonical),
            canonical_recurrence_objects: None,
        })
    }

    /// REST sync with a CalDAV fallback: accounts configured before
    /// OAuth (or with a broken token) keep syncing through their
    /// `caldav_url` instead of failing outright.
    async fn sync(&self, ctx: &CalendarBackendCtx<'_>, account: &AccountFull) -> Result<()> {
        match sync_google(ctx, account).await {
            Ok(()) => Ok(()),
            Err(e) => {
                log::warn!(
                    "sync_calendars: Gmail CalDAV sync failed (OAuth may not be configured): {}",
                    e
                );
                if !account.caldav_url.is_empty() {
                    super::caldav::CalDavCalendarBackend
                        .sync(ctx, account)
                        .await
                } else {
                    Err(e)
                }
            }
        }
    }

    fn validate_event_creation(&self, event: &CalendarEvent, _: &str) -> Result<()> {
        event_to_google_json(event).map(|_| ())
    }

    /// Google events are always created on the primary calendar (the
    /// pre-trait behaviour); `remote_calendar_id` is ignored.
    async fn push_created_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        event: &CalendarEvent,
        _remote_calendar_id: &str,
    ) -> Result<Option<PushedEvent>> {
        let google_event = event_to_google_json(event)?;
        let client = ctx.services.google_client(&account.id).await?;
        let send_updates = send_updates_for(event.attendees_json.as_deref());
        let (remote_id, canonical_uid) = client
            .create_event("primary", &google_event, send_updates)
            .await?;
        Ok(Some(PushedEvent {
            remote_id,
            canonical_uid,
            etag: None,
        }))
    }

    async fn push_updated_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        event: &CalendarEvent,
    ) -> Result<()> {
        let client = ctx.services.google_client(&account.id).await?;
        let patch = event_patch_to_google_json(event);
        let send_updates = send_updates_for(event.attendees_json.as_deref());
        client
            .patch_event("primary", remote_id, &patch, send_updates)
            .await
    }

    async fn push_updated_invitation_copy(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        event: &CalendarEvent,
    ) -> Result<Option<String>> {
        let client = ctx.services.google_client(&account.id).await?;
        let patch = invitation_copy_patch_to_google_json(event)?;
        client
            .patch_event("primary", remote_id, &patch, "none")
            .await?;
        Ok(None)
    }

    async fn push_deleted_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        remote_calendar_id: &str,
    ) -> Result<()> {
        let client = ctx.services.google_client(&account.id).await?;
        client
            .delete_event(remote_calendar_id, remote_id, "all")
            .await
    }

    /// Prefer the Google Calendar REST endpoint; fall back to CalDAV
    /// PROPPATCH if REST fails (OAuth not configured, or remote_id is
    /// actually a CalDAV href).
    async fn push_calendar_rename(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        name: &str,
    ) -> Result<()> {
        if let Ok(client) = ctx.services.google_client(&account.id).await {
            match client.rename_calendar(remote_id, name).await {
                Ok(()) => return Ok(()),
                Err(e) => log::warn!(
                    "update_calendar: Google REST rename failed ({}), falling back to CalDAV",
                    e
                ),
            }
        }
        if !account.caldav_url.is_empty() {
            return super::caldav::CalDavCalendarBackend
                .push_calendar_rename(ctx, account, remote_id, name)
                .await;
        }
        Err(Error::Other(format!(
            "No remote rename path configured for account {} (calendar_protocol={})",
            account.id,
            account.calendar_protocol_str()
        )))
    }

    /// Google Calendar accepts arbitrary RGB on calendarList.patch when
    /// `colorRgbFormat=true` is set. Failures (including a missing
    /// OAuth token) are swallowed — the local color pick sticks.
    async fn push_calendar_color(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        color: &str,
    ) -> Result<()> {
        match ctx.services.google_client(&account.id).await {
            Ok(client) => {
                let fg = readable_foreground(color);
                if let Err(e) = client.set_calendar_color(remote_id, color, fg).await {
                    log::warn!(
                        "update_calendar: Google color push failed (keeping local-only): {}",
                        e
                    );
                }
            }
            Err(e) => log::warn!(
                "update_calendar: Google color push skipped — no OAuth token: {}",
                e
            ),
        }
        Ok(())
    }
}

fn google_rsvp_import_event(
    account: &AccountFull,
    request: &RemoteRsvpRequest,
) -> serde_json::Value {
    let mut attendees: Vec<serde_json::Value> = request
        .attendees
        .iter()
        .map(|attendee| {
            let is_account = attendee.is_self == Some(true)
                || attendee.email.eq_ignore_ascii_case(&account.email);
            let mut value = serde_json::json!({
                "email": attendee.email,
                "responseStatus": if is_account {
                    request.response.as_str()
                } else if attendee.status == "needs-action" {
                    "needsAction"
                } else {
                    attendee.status.as_str()
                },
            });
            if let Some(name) = attendee.name.as_deref() {
                value["displayName"] = serde_json::json!(name);
            }
            if is_account {
                value["self"] = serde_json::json!(true);
            }
            value
        })
        .collect();
    if !attendees.iter().any(|attendee| {
        attendee["self"].as_bool() == Some(true)
            || attendee["email"]
                .as_str()
                .is_some_and(|email| email.eq_ignore_ascii_case(&account.email))
    }) {
        attendees.push(serde_json::json!({
            "email": account.email,
            "responseStatus": request.response.as_str(),
            "self": true,
        }));
    }

    serde_json::json!({
        "iCalUID": request.uid,
        "summary": request.summary,
        "start": if request.all_day {
            serde_json::json!({
                "date": request.start_time.split('T').next().unwrap_or_default()
            })
        } else {
            serde_json::json!({"dateTime": request.start_time})
        },
        "end": if request.all_day {
            serde_json::json!({
                "date": request.end_time.split('T').next().unwrap_or_default()
            })
        } else {
            serde_json::json!({"dateTime": request.end_time})
        },
        "description": request.description,
        "location": request.location,
        "organizer": {"email": request.organizer_email},
        "attendees": attendees,
    })
}

fn google_rsvp_attendees_patch(
    event: &serde_json::Value,
    account_email: &str,
    response: &str,
) -> serde_json::Value {
    let mut attendees = event["attendees"].as_array().cloned().unwrap_or_default();
    let account_attendee = attendees
        .iter()
        .position(|attendee| attendee["self"].as_bool() == Some(true))
        .or_else(|| {
            attendees.iter().position(|attendee| {
                attendee["email"]
                    .as_str()
                    .is_some_and(|email| email.eq_ignore_ascii_case(account_email))
            })
        });

    if let Some(attendee) = account_attendee.and_then(|index| attendees.get_mut(index)) {
        attendee["responseStatus"] = serde_json::json!(response);
    } else {
        attendees.push(serde_json::json!({
            "email": account_email,
            "responseStatus": response,
            "self": true,
        }));
    }

    serde_json::json!({"attendees": attendees})
}

/// Shared Google/Graph sync fixtures use real HTTP clients and the real schema.
#[cfg(test)]
pub(super) mod sync_testutil {
    use std::sync::Arc;

    use async_trait::async_trait;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::db::{self, pool::DbPool};
    use crate::error::Result;
    use crate::provider::{
        GraphTokenPurpose, MailCredentials, OAuthTokenStore, ProviderCredentials, ProviderServices,
        ProviderTransports, TokenEndpointClient,
    };

    struct TestDependencies {
        allow_calendar_credentials: bool,
    }

    #[async_trait]
    impl ProviderCredentials for TestDependencies {
        async fn google_access_token(&self, _: &str) -> Result<String> {
            assert!(
                self.allow_calendar_credentials,
                "unexpected Google credential access"
            );
            Ok("test-token".into())
        }

        async fn graph_access_token(&self, _: &str, _: GraphTokenPurpose) -> Result<String> {
            assert!(
                self.allow_calendar_credentials,
                "unexpected Graph credential access"
            );
            Ok("test-token".into())
        }

        async fn mail_credentials_for(
            &self,
            _: &crate::account::MailAccountConfig,
        ) -> Result<MailCredentials> {
            panic!("unexpected mail credentials")
        }

        async fn jmap_config_for(
            &self,
            _: &crate::account::MailAccountConfig,
        ) -> Result<crate::mail::jmap::JmapConfig> {
            panic!("unexpected JMAP credentials")
        }

        async fn jmap_push_access_token(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<Option<String>> {
            panic!("unexpected JMAP push credentials")
        }

        async fn zoom_access_token(&self, _: &str) -> Result<String> {
            panic!("unexpected Zoom credentials")
        }

        async fn matrix_access_token(&self, _: &str) -> Result<String> {
            panic!("unexpected Matrix credentials")
        }

        async fn talk_app_password(&self, _: &str) -> Result<String> {
            panic!("unexpected Talk credentials")
        }
    }

    impl OAuthTokenStore for TestDependencies {
        fn load(&self, _: &str) -> Result<Option<crate::oauth::OAuthTokens>> {
            panic!("unexpected token load")
        }

        fn store(&self, _: &str, _: &crate::oauth::OAuthTokens) -> Result<()> {
            panic!("unexpected token store")
        }

        fn delete(&self, _: &str) -> Result<()> {
            panic!("unexpected token deletion")
        }
    }

    #[async_trait]
    impl TokenEndpointClient for TestDependencies {
        async fn exchange_code(
            &self,
            _: &crate::oauth::OAuthProvider,
            _: &str,
            _: u16,
            _: Option<&str>,
        ) -> Result<crate::oauth::OAuthTokens> {
            panic!("unexpected token exchange")
        }

        async fn refresh(
            &self,
            _: &crate::oauth::OAuthProvider,
            _: &str,
        ) -> Result<crate::oauth::OAuthTokens> {
            panic!("unexpected token refresh")
        }

        async fn refresh_scoped(
            &self,
            _: &crate::oauth::OAuthProvider,
            _: &str,
            _: &str,
        ) -> Result<crate::oauth::OAuthTokens> {
            panic!("unexpected scoped token refresh")
        }

        async fn refresh_dynamic(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<crate::oauth::OAuthTokens> {
            panic!("unexpected dynamic token refresh")
        }
    }

    pub(crate) fn services(root: &str) -> ProviderServices {
        services_with_credentials(root, true)
    }

    pub(super) fn services_with_credentials(
        root: &str,
        allow_calendar_credentials: bool,
    ) -> ProviderServices {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut transports = ProviderTransports::production().unwrap();
        transports.google_http = http.clone();
        transports.graph_http = http;
        transports.google_endpoints.calendar_api_root = root.into();
        transports.graph_endpoints.v1_api_root = root.into();
        let dependencies = Arc::new(TestDependencies {
            allow_calendar_credentials,
        });
        ProviderServices::new(
            dependencies.clone(),
            dependencies.clone(),
            dependencies,
            transports,
        )
    }

    pub(crate) async fn setup_db() -> (tempfile::TempDir, DbPool) {
        let (dir, db) = crate::backend::testutil::temp_pool();
        {
            let conn = db.writer().await;
            db::schema::initialize(&conn).unwrap();
            conn.execute(
                "INSERT INTO accounts (id, display_name, email, username)
                 VALUES ('acc1', 'Test', 'u@example.com', 'u@example.com')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO calendars (id, account_id, name, remote_id)
                 VALUES ('cal1', 'acc1', 'Calendar', 'primary')",
                [],
            )
            .unwrap();
        }
        (dir, db)
    }

    pub(crate) fn cache_event(conn: &rusqlite::Connection, id: &str, remote_id: Option<&str>) {
        // Omit classification to exercise the migrated legacy-row default.
        conn.execute(
            "INSERT INTO calendar_events
             (id, account_id, calendar_id, title, start_time, end_time, remote_id, uid)
             VALUES (?1, 'acc1', 'cal1', 'Cached event',
                     '2026-09-14T09:00:00Z', '2026-09-14T10:00:00Z', ?2, ?3)",
            rusqlite::params![id, remote_id, format!("uid-{id}@example.test")],
        )
        .unwrap();
    }

    pub(crate) async fn serve_responses(
        responses: Vec<(u16, serde_json::Value)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        serve_requests("GET", responses).await
    }

    pub(crate) async fn serve_create_response(
        response: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        serve_requests("POST", vec![(200, response)]).await
    }

    pub(crate) async fn serve_patch_response(
        response: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        serve_requests("PATCH", vec![(200, response)]).await
    }

    pub(crate) async fn serve_occurrence_responses(
        responses: Vec<(&'static str, u16, serde_json::Value)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        serve_method_responses(responses).await
    }

    async fn serve_requests(
        method: &'static str,
        responses: Vec<(u16, serde_json::Value)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        serve_method_responses(
            responses
                .into_iter()
                .map(|(status, body)| (method, status, body))
                .collect(),
        )
        .await
    }

    async fn serve_method_responses(
        responses: Vec<(&'static str, u16, serde_json::Value)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}/calendar-api", listener.local_addr().unwrap());
        let captured = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (method, status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0, "request ended before its headers and body");
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(header_end) =
                        bytes.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let headers = String::from_utf8_lossy(&bytes[..header_end]);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|value| value.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= header_end + 4 + content_length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8(bytes).unwrap();
                assert_eq!(request.split_whitespace().next(), Some(method), "{request}");
                requests.push(request);
                let body = body.to_string();
                let reason = if status == 200 { "OK" } else { "Error" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (root, captured)
    }
}

#[cfg(test)]
pub(super) mod creation_testutil {
    use super::sync_testutil::{
        serve_create_response, services, services_with_credentials, setup_db,
    };
    use crate::backend::calendar::{CalendarBackend, CalendarBackendCtx};
    use crate::backend::testutil::{account, event};
    use crate::calendar::RecurrenceKind;
    use crate::db;
    use crate::error::Error;

    pub(crate) async fn assert_rejected_before_io(backend: &dyn CalendarBackend) {
        let (_dir, db) = setup_db().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let services = services_with_credentials(&root, false);
        let ctx = CalendarBackendCtx {
            db: &db,
            services: &services,
        };
        let account = account("calendar", backend.protocol());
        for kind in [
            RecurrenceKind::Unknown,
            RecurrenceKind::Series,
            RecurrenceKind::Occurrence,
            RecurrenceKind::Standalone,
        ] {
            for rule in [None, Some(""), Some("FREQ=WEEKLY"), Some(" ")] {
                if kind == RecurrenceKind::Standalone && rule.is_none_or(str::is_empty) {
                    continue;
                }
                let mut event = event();
                event.id = uuid::Uuid::new_v4().to_string();
                event.recurrence_kind = kind;
                event.recurrence_rule = rule.map(str::to_string);
                {
                    let conn = db.writer().await;
                    db::calendar::insert_event(&conn, &event).unwrap();
                }
                let error = backend
                    .push_created_event(&ctx, &account, &event, "primary")
                    .await
                    .err()
                    .expect("lossy event creation must be rejected");
                assert!(
                    matches!(
                        error,
                        Error::UnsupportedCapability {
                            protocol,
                            capability: "recurring or unclassified event creation",
                        } if protocol == backend.protocol()
                    ),
                    "{kind:?}, {rule:?}: {error}"
                );
                let after = db::calendar::get_event(&db.reader(), &event.id).unwrap();
                assert_eq!(
                    serde_json::to_value(after).unwrap(),
                    serde_json::to_value(event).unwrap()
                );
            }
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), listener.accept())
                .await
                .is_err(),
            "rejected creation attempted HTTP"
        );
    }

    pub(crate) async fn assert_standalone_creation(
        backend: &dyn CalendarBackend,
        response: serde_json::Value,
        target: &str,
        title_field: &str,
    ) {
        for rule in [None, Some("")] {
            let (_dir, db) = setup_db().await;
            let (root, captured) = serve_create_response(response.clone()).await;
            let services = services(&root);
            let mut event = event();
            event.recurrence_rule = rule.map(str::to_string);
            let pushed = backend
                .push_created_event(
                    &CalendarBackendCtx {
                        db: &db,
                        services: &services,
                    },
                    &account("calendar", backend.protocol()),
                    &event,
                    "primary",
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(pushed.remote_id, "created-event");
            assert_eq!(
                pushed.canonical_uid.as_deref(),
                Some("canonical@example.test")
            );
            let requests = captured.await.unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with(&format!("POST {target} HTTP/1.1\r\n")));
            let (_, body) = requests[0].split_once("\r\n\r\n").unwrap();
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(body[title_field], event.title);
            assert!(body.get("recurrence").is_none());
        }
    }
}

#[cfg(test)]
mod creation_tests {
    use super::creation_testutil::{assert_rejected_before_io, assert_standalone_creation};
    use super::sync_testutil::{serve_patch_response, services, setup_db};
    use super::GoogleCalendarBackend;
    use crate::backend::calendar::{CalendarBackend, CalendarBackendCtx};
    use crate::backend::testutil::{account, event};
    use crate::calendar::RecurrenceKind;

    #[tokio::test]
    async fn rejects_lossy_creation_before_credentials_and_preserves_local_event() {
        assert_rejected_before_io(&GoogleCalendarBackend).await;
    }

    #[tokio::test]
    async fn publishes_confirmed_standalone_creation() {
        assert_standalone_creation(
            &GoogleCalendarBackend,
            serde_json::json!({"id": "created-event", "iCalUID": "canonical@example.test"}),
            "/calendar-api/calendars/primary/events?sendUpdates=none",
            "summary",
        )
        .await;
    }

    #[tokio::test]
    async fn personal_copy_update_is_authoritative_without_scheduling_guests() {
        let (_directory, db) = setup_db().await;
        let (root, captured) = serve_patch_response(serde_json::json!({})).await;
        let mut event = event();
        event.title = "Changed".into();
        event.description = None;
        event.location = None;
        event.start_time = "2026-09-14T09:00:00Z".into();
        event.end_time = "2026-09-14T10:00:00Z".into();
        event.timezone = Some("Europe/Stockholm".into());
        event.recurrence_kind = RecurrenceKind::Series;
        event.recurrence_rule = Some("FREQ=WEEKLY;BYDAY=MO;COUNT=3".into());
        event.organizer_email = Some("organizer@example.test".into());
        event.attendees_json = Some(
            serde_json::json!([{"email": "guest@example.test", "status": "accepted"}]).to_string(),
        );
        event.ical_data = Some(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:series@example.test\r\n\
             RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=3\r\nEND:VEVENT\r\n\
             END:VCALENDAR\r\n"
                .into(),
        );
        let provider_services = services(&root);

        GoogleCalendarBackend
            .push_updated_invitation_copy(
                &CalendarBackendCtx {
                    db: &db,
                    services: &provider_services,
                },
                &account("calendar", "google"),
                "remote-series",
                &event,
            )
            .await
            .unwrap();
        let requests = captured.await.unwrap();
        assert!(requests[0].starts_with(
            "PATCH /calendar-api/calendars/primary/events/remote-series?sendUpdates=none "
        ));
        let payload: serde_json::Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(payload["summary"], "Changed");
        assert!(payload["description"].is_null());
        assert!(payload["location"].is_null());
        assert_eq!(payload["start"]["timeZone"], "Europe/Stockholm");
        assert_eq!(
            payload["recurrence"][0],
            "RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=3"
        );
        assert!(payload.get("organizer").is_none());
        assert!(payload.get("attendees").is_none());
    }

    #[tokio::test]
    async fn personal_copy_update_explicitly_removes_recurrence() {
        let (_directory, db) = setup_db().await;
        let (root, captured) = serve_patch_response(serde_json::json!({})).await;
        let mut event = event();
        event.recurrence_kind = RecurrenceKind::Standalone;
        event.recurrence_rule = None;
        event.attendees_json =
            Some(serde_json::json!([{"email": "guest@example.test"}]).to_string());
        let provider_services = services(&root);

        GoogleCalendarBackend
            .push_updated_invitation_copy(
                &CalendarBackendCtx {
                    db: &db,
                    services: &provider_services,
                },
                &account("calendar", "google"),
                "remote-event",
                &event,
            )
            .await
            .unwrap();
        let requests = captured.await.unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(payload["recurrence"], serde_json::json!([]));
        assert!(payload.get("attendees").is_none());
    }
}

#[cfg(test)]
mod occurrence_update_tests {
    use super::sync_testutil::{
        serve_occurrence_responses, services, services_with_credentials, setup_db,
    };
    use super::{CalendarBackend, CalendarBackendCtx, GoogleCalendarBackend};
    use crate::backend::calendar::RemoteOccurrenceUpdate;
    use crate::backend::testutil::{account, event};
    use crate::calendar::recurrence_identity::{
        OccurrenceFields, RecurrenceIdentity, RecurrenceObjectKind, RecurrenceValueType,
        UpdateOccurrenceInput,
    };
    use crate::calendar::{Attendee, RecurrenceKind};
    use crate::error::Error;
    use serde_json::json;

    fn provider_event(etag: &str, fields: &OccurrenceFields) -> serde_json::Value {
        let boundary = |value: &str| {
            if fields.all_day {
                json!({"date": value})
            } else {
                json!({"dateTime": value, "timeZone": "UTC"})
            }
        };
        json!({
            "id": "remote-occurrence",
            "etag": etag,
            "iCalUID": "series@example.test",
            "summary": fields.title,
            "description": fields.description,
            "location": fields.location,
            "start": boundary(&fields.start_time),
            "end": boundary(&fields.end_time),
            "recurringEventId": "remote-series",
            "originalStartTime": if fields.all_day {
                json!({"date": "2026-09-15", "timeZone": "UTC"})
            } else {
                json!({"dateTime": "2026-09-15T09:00:00Z", "timeZone": "UTC"})
            },
            "organizer": {"email": "u@example.com", "self": true},
            "attendees": [{
                "email": "u@example.com", "self": true, "responseStatus": "accepted"
            }]
        })
    }

    fn request(all_day: bool) -> RemoteOccurrenceUpdate {
        let current_fields = OccurrenceFields {
            title: "Original".into(),
            description: Some("Original description".into()),
            location: Some("Original room".into()),
            start_time: if all_day {
                "2026-09-15".into()
            } else {
                "2026-09-15T09:00:00Z".into()
            },
            end_time: if all_day {
                "2026-09-16".into()
            } else {
                "2026-09-15T10:00:00Z".into()
            },
            all_day,
            timezone: (!all_day).then(|| "UTC".into()),
        };
        let native = provider_event("old-etag", &current_fields);
        let mut current = event();
        current.uid = Some("series@example.test".into());
        current.title = current_fields.title.clone();
        current.description = current_fields.description.clone();
        current.location = current_fields.location.clone();
        current.start_time = current_fields.start_time.clone();
        current.end_time = current_fields.end_time.clone();
        current.all_day = all_day;
        current.timezone = current_fields.timezone.clone();
        current.recurrence_kind = RecurrenceKind::Occurrence;
        current.organizer_email = Some("u@example.com".into());
        current.attendees_json = Some(
            serde_json::to_string(&[Attendee {
                email: "u@example.com".into(),
                name: None,
                status: "accepted".into(),
                is_self: Some(true),
            }])
            .unwrap(),
        );
        current.my_status = Some("accepted".into());
        current.remote_id = Some("remote-occurrence".into());
        current.etag = Some("old-etag".into());
        let desired = OccurrenceFields {
            title: "Canonical override".into(),
            description: Some(String::new()),
            location: Some(String::new()),
            start_time: if all_day {
                "2026-09-17".into()
            } else {
                "2026-09-15T11:00:00Z".into()
            },
            end_time: if all_day {
                "2026-09-18".into()
            } else {
                "2026-09-15T12:00:00Z".into()
            },
            all_day,
            timezone: (!all_day).then(|| "UTC".into()),
        };
        RemoteOccurrenceUpdate {
            target_id: "remote-occurrence".into(),
            expected_provider_revision: Some("old-etag".into()),
            trusted_identity: RecurrenceIdentity {
                object_id: "recurrence-object".into(),
                account_id: "acc1".into(),
                event_id: current.id.clone(),
                local_series_event_id: Some("local-series".into()),
                provider_calendar_id: Some("team/calendar@example.com".into()),
                provider_series_id: Some("remote-series".into()),
                provider_occurrence_id: Some("remote-occurrence".into()),
                recurrence_id: Some(if all_day {
                    "2026-09-15".into()
                } else {
                    "2026-09-15T09:00:00Z".into()
                }),
                recurrence_timezone: Some("UTC".into()),
                recurrence_value_type: Some(if all_day {
                    RecurrenceValueType::Date
                } else {
                    RecurrenceValueType::DateTime
                }),
                occurrence: current_fields,
                provider_native_data: Some(native.to_string()),
                provider_revision: Some("old-etag".into()),
                kind: RecurrenceObjectKind::Occurrence,
            },
            current_event: current,
            patch: UpdateOccurrenceInput {
                title: Some(desired.title.clone()),
                description: desired.description.clone(),
                location: desired.location.clone(),
                start_time: Some(desired.start_time.clone()),
                end_time: Some(desired.end_time.clone()),
                all_day: None,
                timezone: None,
            },
            desired,
        }
    }

    #[tokio::test]
    async fn secondary_calendar_patch_is_encoded_and_returns_canonical_exception() {
        let (_directory, db) = setup_db().await;
        let request = request(false);
        let canonical = provider_event("new-etag", &request.desired);
        let (root, captured) =
            serve_occurrence_responses(vec![("PATCH", 200, canonical.clone())]).await;
        let outcome = GoogleCalendarBackend
            .update_recurrence_occurrence(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "google"),
                &request,
            )
            .await
            .unwrap();

        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), 1, "complete PATCH response must avoid GET");
        assert!(requests[0].starts_with(
            "PATCH /calendar-api/calendars/team%2Fcalendar%40example.com/events/remote-occurrence?sendUpdates=none "
        ));
        let (headers, body) = requests[0].split_once("\r\n\r\n").unwrap();
        assert!(headers
            .to_ascii_lowercase()
            .contains("if-match: old-etag\r\n"));
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["description"], "");
        assert_eq!(body["location"], "");
        assert_eq!(body["start"]["dateTime"], "2026-09-15T11:00:00Z");
        for forbidden in [
            "attendees",
            "recurrence",
            "organizer",
            "id",
            "iCalUID",
            "recurringEventId",
            "originalStartTime",
        ] {
            assert!(body.get(forbidden).is_none(), "{forbidden}");
        }
        assert_eq!(
            outcome.replacement_identity.kind,
            RecurrenceObjectKind::Exception
        );
        assert_eq!(
            outcome.replacement_identity.provider_revision.as_deref(),
            Some("new-etag")
        );
        assert_eq!(
            outcome.replacement_identity.provider_calendar_id.as_deref(),
            Some("team/calendar@example.com")
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                outcome
                    .replacement_identity
                    .provider_native_data
                    .as_deref()
                    .unwrap()
            )
            .unwrap(),
            canonical
        );
        assert_eq!(outcome.occurrence, request.desired);
        assert!(outcome.canonical_recurrence_objects.is_none());
        let event = outcome.canonical_event.unwrap();
        assert_eq!(event.id, request.current_event.id);
        assert_eq!(event.calendar_id, "cal1");
        assert_eq!(event.remote_id.as_deref(), Some("remote-occurrence"));
        assert_eq!(event.etag.as_deref(), Some("new-etag"));
        assert_eq!(event.uid, request.current_event.uid);
        assert_eq!(event.organizer_email, request.current_event.organizer_email);
        assert_eq!(event.attendees_json, request.current_event.attendees_json);
    }

    #[tokio::test]
    async fn incomplete_patch_response_gets_canonical_all_day_event_from_same_target() {
        let (_directory, db) = setup_db().await;
        let request = request(true);
        let canonical = provider_event("new-etag", &request.desired);
        let (root, captured) = serve_occurrence_responses(vec![
            ("PATCH", 200, json!({"id": "remote-occurrence"})),
            ("GET", 200, canonical),
        ])
        .await;
        let outcome = GoogleCalendarBackend
            .update_recurrence_occurrence(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "google"),
                &request,
            )
            .await
            .unwrap();
        let requests = captured.await.unwrap();
        assert!(requests[1].starts_with(
            "GET /calendar-api/calendars/team%2Fcalendar%40example.com/events/remote-occurrence HTTP/1.1"
        ));
        assert_eq!(outcome.occurrence.start_time, "2026-09-17");
        assert_eq!(outcome.occurrence.end_time, "2026-09-18");
        assert!(outcome.occurrence.all_day);
    }

    #[tokio::test]
    async fn time_only_patch_omits_text_and_preserves_native_timezones() {
        let (_directory, db) = setup_db().await;
        let mut request = request(false);
        request.desired.title = request.trusted_identity.occurrence.title.clone();
        request.desired.description = request.trusted_identity.occurrence.description.clone();
        request.desired.location = request.trusted_identity.occurrence.location.clone();
        request.patch = UpdateOccurrenceInput {
            start_time: Some(request.desired.start_time.clone()),
            end_time: Some(request.desired.end_time.clone()),
            ..Default::default()
        };
        let mut native: serde_json::Value = serde_json::from_str(
            request
                .trusted_identity
                .provider_native_data
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        native["start"]["timeZone"] = json!("Europe/Stockholm");
        native["end"]["timeZone"] = json!("Europe/Stockholm");
        request.trusted_identity.provider_native_data = Some(native.to_string());
        let canonical = provider_event("new-etag", &request.desired);
        let (root, captured) = serve_occurrence_responses(vec![("PATCH", 200, canonical)]).await;

        GoogleCalendarBackend
            .update_recurrence_occurrence(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "google"),
                &request,
            )
            .await
            .unwrap();

        let requests = captured.await.unwrap();
        let body: serde_json::Value =
            serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            body,
            json!({
                "start": {
                    "dateTime": "2026-09-15T13:00:00+02:00",
                    "timeZone": "Europe/Stockholm"
                },
                "end": {
                    "dateTime": "2026-09-15T14:00:00+02:00",
                    "timeZone": "Europe/Stockholm"
                }
            })
        );
    }

    #[tokio::test]
    async fn title_only_patch_does_not_rewrite_boundaries() {
        let (_directory, db) = setup_db().await;
        let mut request = request(false);
        request.desired = request.trusted_identity.occurrence.clone();
        request.desired.title = "Renamed occurrence".into();
        request.patch = UpdateOccurrenceInput {
            title: Some(request.desired.title.clone()),
            ..Default::default()
        };
        let canonical = provider_event("new-etag", &request.desired);
        let (root, captured) = serve_occurrence_responses(vec![("PATCH", 200, canonical)]).await;

        GoogleCalendarBackend
            .update_recurrence_occurrence(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "google"),
                &request,
            )
            .await
            .unwrap();

        let requests = captured.await.unwrap();
        let body: serde_json::Value =
            serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body, json!({"summary": "Renamed occurrence"}));
    }

    #[tokio::test]
    async fn invalid_timezone_edits_fail_before_credentials_or_http() {
        let (_directory, db) = setup_db().await;
        for source in ["start", "end", "requested", "rewrite"] {
            for with_credentials in [false, true] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let root = format!("http://{}", listener.local_addr().unwrap());
                let mut request = request(false);
                request.patch = UpdateOccurrenceInput::default();
                match source {
                    "requested" => {
                        request.desired.timezone = Some("Unsupported/Requested".into());
                        request.patch.timezone = request.desired.timezone.clone();
                    }
                    "rewrite" => {
                        request.desired.timezone = Some("Unsupported/Retained".into());
                        request.patch.all_day = Some(false);
                    }
                    name => {
                        if name == "start" {
                            request.patch.start_time = Some(request.desired.start_time.clone());
                        } else {
                            request.patch.end_time = Some(request.desired.end_time.clone());
                        }
                        let mut native: serde_json::Value = serde_json::from_str(
                            request
                                .trusted_identity
                                .provider_native_data
                                .as_deref()
                                .unwrap(),
                        )
                        .unwrap();
                        native[name]["timeZone"] = json!("Unsupported/Retained");
                        request.trusted_identity.provider_native_data = Some(native.to_string());
                    }
                }
                let provider_services = services_with_credentials(&root, with_credentials);
                let error = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    GoogleCalendarBackend.update_recurrence_occurrence(
                        &CalendarBackendCtx {
                            db: &db,
                            services: &provider_services,
                        },
                        &account("calendar", "google"),
                        &request,
                    ),
                )
                .await
                .expect("timezone validation must precede HTTP")
                .unwrap_err();
                assert!(
                    error.to_string().contains("timezone is unsupported"),
                    "source={source}, credentials={with_credentials}: {error}"
                );
                assert!(tokio::time::timeout(
                    std::time::Duration::from_millis(25),
                    listener.accept()
                )
                .await
                .is_err());
            }
        }
    }

    #[tokio::test]
    async fn stale_patch_and_untrusted_native_identity_fail_without_local_mutation() {
        let (_directory, db) = setup_db().await;
        let stale_request = request(false);
        let (root, captured) =
            serve_occurrence_responses(vec![("PATCH", 412, json!({"error": "stale"}))]).await;
        let error = GoogleCalendarBackend
            .update_recurrence_occurrence(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "google"),
                &stale_request,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Sync(message) if message.contains("sync before retrying")));
        assert_eq!(captured.await.unwrap().len(), 1);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let mut mismatched = stale_request;
        let mut native: serde_json::Value = serde_json::from_str(
            mismatched
                .trusted_identity
                .provider_native_data
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        native["recurringEventId"] = json!("different-series");
        mismatched.trusted_identity.provider_native_data = Some(native.to_string());
        let error = GoogleCalendarBackend
            .update_recurrence_occurrence(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services_with_credentials(&root, false),
                },
                &account("calendar", "google"),
                &mismatched,
            )
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("does not match the trusted target"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), listener.accept())
                .await
                .is_err()
        );

        let request = request(false);
        let mut changed_identity = provider_event("new-etag", &request.desired);
        changed_identity["originalStartTime"]["dateTime"] = json!("2026-09-22T09:00:00Z");
        let (root, captured) =
            serve_occurrence_responses(vec![("PATCH", 200, changed_identity)]).await;
        let error = GoogleCalendarBackend
            .update_recurrence_occurrence(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "google"),
                &request,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Sync(message) if message.contains("immutable recurrence identity")
        ));
        assert_eq!(captured.await.unwrap().len(), 1);
    }
}

#[cfg(test)]
mod recurrence_sync_tests {
    use super::sync_testutil::{cache_event, serve_responses, services, setup_db};
    use super::{
        google_recurrence_seed, needs_recurrence_refresh, sync_google, CalendarBackendCtx,
    };
    use crate::backend::testutil::{account, event};
    use crate::calendar::recurrence_identity::{
        OccurrenceFields, RecurrenceIdentitySeed, RecurrenceObjectKind, RecurrenceValueType,
    };
    use crate::calendar::RecurrenceKind;
    use crate::db;
    use rusqlite::OptionalExtension;
    use serde_json::json;

    const SYNC_KEY: &str = "google_sync_token_acc1_primary";

    fn assert_events_query(request: &str, sync_token: Option<&str>) {
        let target = request.split_whitespace().nth(1).unwrap();
        let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(url.path(), "/calendar-api/calendars/primary/events");
        assert_eq!(
            query.get("singleEvents").map(|value| value.as_ref()),
            Some("true")
        );
        assert_eq!(
            query.get("maxResults").map(|value| value.as_ref()),
            Some("500")
        );
        assert_eq!(
            query.get("syncToken").map(|value| value.as_ref()),
            sync_token
        );
        for bound in ["timeMin", "timeMax"] {
            assert_eq!(
                query.contains_key(bound),
                sync_token.is_none(),
                "{bound}: {request}"
            );
        }
    }

    fn remote_event(id: &str) -> serde_json::Value {
        json!({
            "id": id,
            "kind": "calendar#event",
            "iCalUID": format!("uid-{id}@example.test"),
            "summary": "Refreshed event",
            "start": {"dateTime": "2026-09-14T09:00:00Z"},
            "end": {"dateTime": "2026-09-14T10:00:00Z"}
        })
    }

    fn trusted_seed(provider_occurrence_id: &str) -> RecurrenceIdentitySeed {
        RecurrenceIdentitySeed {
            local_series_event_id: None,
            provider_calendar_id: Some("primary".into()),
            provider_series_id: Some(format!("trusted-series-{provider_occurrence_id}")),
            provider_occurrence_id: Some(provider_occurrence_id.into()),
            recurrence_id: Some("2026-09-14T09:00:00Z".into()),
            recurrence_timezone: Some("UTC".into()),
            recurrence_value_type: Some(RecurrenceValueType::DateTime),
            occurrence: OccurrenceFields {
                title: "Cached event".into(),
                description: None,
                location: None,
                start_time: "2026-09-14T09:00:00Z".into(),
                end_time: "2026-09-14T10:00:00Z".into(),
                all_day: false,
                timezone: Some("UTC".into()),
            },
            provider_native_data: Some("trusted provider data".into()),
            provider_revision: Some("trusted revision".into()),
            kind: RecurrenceObjectKind::Occurrence,
        }
    }

    #[test]
    fn recurrence_seed_preserves_google_identity_values() {
        let mut normalized = event();
        normalized.start_time = "2026-09-15".into();
        normalized.end_time = "2026-09-16".into();
        normalized.all_day = true;
        normalized.recurrence_kind = RecurrenceKind::Occurrence;
        let all_day = json!({
            "id": "all-day-occurrence",
            "recurringEventId": "all-day-master",
            "originalStartTime": {
                "date": "2026-09-08",
                "timeZone": "Europe/Stockholm"
            },
            "etag": "all-day-etag"
        });
        let seed = google_recurrence_seed(&all_day, &normalized, "primary").unwrap();
        assert_eq!(seed.provider_calendar_id.as_deref(), Some("primary"));
        assert_eq!(seed.recurrence_id.as_deref(), Some("2026-09-08"));
        assert_eq!(seed.recurrence_value_type, Some(RecurrenceValueType::Date));
        assert_eq!(
            seed.recurrence_timezone.as_deref(),
            Some("Europe/Stockholm")
        );
        assert_eq!(seed.occurrence.start_time, "2026-09-15");
        assert_eq!(seed.occurrence.end_time, "2026-09-16");

        normalized.start_time = "2026-09-15T07:30:00Z".into();
        normalized.end_time = "2026-09-15T08:30:00Z".into();
        normalized.all_day = false;
        let timed = json!({
            "id": "timed-occurrence",
            "recurringEventId": "timed-master",
            "originalStartTime": {
                "dateTime": "2026-09-08T09:30:00+02:00",
                "timeZone": "Europe/Stockholm"
            }
        });
        let seed = google_recurrence_seed(&timed, &normalized, "primary").unwrap();
        assert_eq!(
            seed.recurrence_id.as_deref(),
            Some("2026-09-08T09:30:00+02:00")
        );
        assert_eq!(
            seed.recurrence_value_type,
            Some(RecurrenceValueType::DateTime)
        );

        normalized.recurrence_kind = RecurrenceKind::Series;
        let master = json!({
            "id": "master",
            "recurrence": ["RRULE:FREQ=WEEKLY"],
            "etag": "master-etag",
            "providerOnly": {"preserved": true}
        });
        let seed = google_recurrence_seed(&master, &normalized, "primary").unwrap();
        assert_eq!(seed.kind, RecurrenceObjectKind::Master);
        assert_eq!(seed.provider_calendar_id.as_deref(), Some("primary"));
        assert_eq!(seed.provider_series_id.as_deref(), Some("master"));
        assert!(seed.recurrence_id.is_none());
        assert_eq!(seed.provider_revision.as_deref(), Some("master-etag"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                seed.provider_native_data.as_deref().unwrap()
            )
            .unwrap(),
            master
        );

        normalized.recurrence_kind = RecurrenceKind::Occurrence;
        assert!(google_recurrence_seed(
            &json!({"id": "partial", "recurringEventId": "master"}),
            &normalized,
            "primary"
        )
        .is_none());
    }

    #[tokio::test]
    async fn sync_ingests_stable_identity_and_fails_closed_for_malformed_data() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            for id in ["standalone", "malformed"] {
                cache_event(&conn, id, Some(id));
                let cached = db::calendar::get_event(&conn, id).unwrap();
                db::calendar::upsert_event_by_remote_id_with_recurrence(
                    &conn,
                    &cached,
                    &[trusted_seed(&format!("trusted-{id}"))],
                )
                .unwrap();
            }
        }

        let master = json!({
            "id": "master",
            "iCalUID": "master@example.test",
            "summary": "Master",
            "start": {"dateTime": "2026-09-15T09:00:00+02:00"},
            "end": {"dateTime": "2026-09-15T10:00:00+02:00"},
            "recurrence": ["RRULE:FREQ=WEEKLY"],
            "etag": "master-etag",
            "providerOnly": {"preserved": true}
        });
        let all_day = json!({
            "id": "all-day",
            "iCalUID": "master@example.test",
            "summary": "All day occurrence",
            "start": {"date": "2026-09-22"},
            "end": {"date": "2026-09-23"},
            "recurringEventId": "master",
            "originalStartTime": {
                "date": "2026-09-22",
                "timeZone": "Europe/Stockholm"
            },
            "etag": "all-day-etag"
        });
        let timed = json!({
            "id": "timed",
            "iCalUID": "master@example.test",
            "summary": "Timed occurrence",
            "description": "Exception description",
            "location": "Exception room",
            "start": {"dateTime": "2026-09-29T11:00:00+02:00"},
            "end": {"dateTime": "2026-09-29T12:00:00+02:00"},
            "recurringEventId": "master",
            "originalStartTime": {
                "dateTime": "2026-09-29T09:00:00+02:00",
                "timeZone": "Europe/Stockholm"
            },
            "etag": "timed-etag",
            "providerOnly": [1, 2, 3]
        });
        let standalone = remote_event("standalone");
        let mut malformed = remote_event("malformed");
        malformed["recurringEventId"] = json!("master");
        let calendars = json!({
            "items": [{"id": "primary", "summary": "Calendar", "primary": true}]
        });
        let items = vec![
            master.clone(),
            all_day.clone(),
            timed.clone(),
            standalone,
            malformed,
        ];
        let (root, captured) = serve_responses(vec![
            (200, calendars.clone()),
            (
                200,
                json!({"items": items.clone(), "nextSyncToken": "token-1"}),
            ),
            (200, calendars),
            (200, json!({"items": items, "nextSyncToken": "token-2"})),
        ])
        .await;
        let provider_services = services(&root);
        let ctx = CalendarBackendCtx {
            db: &db,
            services: &provider_services,
        };
        let account = account("calendar", "google");
        sync_google(&ctx, &account).await.unwrap();
        let first_object_ids: Vec<String> = {
            let conn = db.reader();
            ["master", "all-day", "timed"]
                .iter()
                .map(|remote_id| {
                    conn.query_row(
                        "SELECT recurrence.object_id
                         FROM calendar_recurrence_objects recurrence
                         JOIN calendar_events event ON event.id = recurrence.event_id
                         WHERE event.remote_id = ?1",
                        [remote_id],
                        |row| row.get(0),
                    )
                    .unwrap()
                })
                .collect()
        };
        sync_google(&ctx, &account).await.unwrap();
        let requests = captured.await.unwrap();
        assert_events_query(&requests[1], None);
        assert_events_query(&requests[3], Some("token-1"));

        let conn = db.reader();
        let identities = ["master", "all-day", "timed"]
            .iter()
            .map(|remote_id| {
                conn.query_row(
                    "SELECT event.id FROM calendar_events event WHERE event.remote_id = ?1",
                    [remote_id],
                    |row| row.get::<_, String>(0),
                )
                .map(|event_id| {
                    db::calendar_recurrence::get_by_event_id(&conn, &event_id)
                        .unwrap()
                        .pop()
                        .unwrap()
                })
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            identities
                .iter()
                .map(|identity| identity.object_id.clone())
                .collect::<Vec<_>>(),
            first_object_ids
        );
        assert_eq!(identities[0].kind, RecurrenceObjectKind::Master);
        assert!(identities
            .iter()
            .all(|identity| identity.provider_calendar_id.as_deref() == Some("primary")));
        assert_eq!(identities[0].occurrence.start_time, "2026-09-15T07:00:00Z");
        assert_eq!(identities[1].recurrence_id.as_deref(), Some("2026-09-22"));
        assert_eq!(
            identities[1].recurrence_value_type,
            Some(RecurrenceValueType::Date)
        );
        assert_eq!(
            identities[2].recurrence_id.as_deref(),
            Some("2026-09-29T09:00:00+02:00")
        );
        assert_eq!(identities[2].occurrence.title, "Timed occurrence");
        assert_eq!(
            identities[2].occurrence.description.as_deref(),
            Some("Exception description")
        );
        assert_eq!(
            identities[2].occurrence.location.as_deref(),
            Some("Exception room")
        );
        assert_eq!(identities[2].occurrence.start_time, "2026-09-29T09:00:00Z");
        assert_eq!(
            identities[2].provider_revision.as_deref(),
            Some("timed-etag")
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                identities[2].provider_native_data.as_deref().unwrap()
            )
            .unwrap(),
            timed
        );
        let standalone_id: String = conn
            .query_row(
                "SELECT id FROM calendar_events WHERE remote_id = 'standalone'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            db::calendar_recurrence::get_by_event_id(&conn, &standalone_id)
                .unwrap()
                .is_empty()
        );
        let malformed_id: String = conn
            .query_row(
                "SELECT id FROM calendar_events WHERE remote_id = 'malformed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let malformed_identity = db::calendar_recurrence::get_by_event_id(&conn, &malformed_id)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            malformed_identity.provider_revision.as_deref(),
            Some("trusted revision")
        );
    }

    #[tokio::test]
    async fn old_unknown_metadata_recovery_never_starves_incremental_deletions() {
        for metadata_status in [200, 404, 410, 500] {
            let (_dir, db) = setup_db().await;
            let old_start = (chrono::Utc::now() - chrono::Duration::days(365)).to_rfc3339();
            {
                let conn = db.writer().await;
                cache_event(&conn, "old-unknown", Some("old-unknown"));
                cache_event(&conn, "upcoming", Some("upcoming"));
                cache_event(&conn, "local-only", None);
                conn.execute(
                    "UPDATE calendar_events SET start_time = ?1, end_time = ?1
                     WHERE id = 'old-unknown'",
                    [&old_start],
                )
                .unwrap();
                conn.execute(
                    "UPDATE calendar_events SET recurrence_kind = 'standalone'
                     WHERE id = 'upcoming'",
                    [],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO app_metadata (key, value) VALUES (?1, 'old-token')",
                    [SYNC_KEY],
                )
                .unwrap();
            }
            let (root, captured) = serve_responses(vec![
                (200, json!({"items": [{"id": "primary", "summary": "Calendar"}]})),
                (metadata_status, json!({"items": [], "nextSyncToken": "metadata-token"})),
                (200, json!({
                    "items": [{"id": "upcoming", "status": "cancelled"}, remote_event("new-event")],
                    "nextSyncToken": "delta-token"
                })),
            ])
            .await;
            sync_google(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "google"),
            )
            .await
            .unwrap();

            let conn = db.reader();
            let remaining: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM calendar_events WHERE id = 'upcoming'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                remaining, 0,
                "metadata HTTP {metadata_status} blocked deletion"
            );
            let old = db::calendar::get_event(&conn, "old-unknown").unwrap();
            assert_eq!(old.start_time, old_start);
            assert_eq!(old.recurrence_kind, RecurrenceKind::Unknown);
            assert_eq!(
                db::calendar::get_event(&conn, "local-only")
                    .unwrap()
                    .recurrence_kind,
                RecurrenceKind::Unknown
            );
            let token: String = conn
                .query_row(
                    "SELECT value FROM app_metadata WHERE key = ?1",
                    [SYNC_KEY],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(token, "delta-token");
            let new_kind: String = conn
                .query_row(
                    "SELECT recurrence_kind FROM calendar_events WHERE remote_id = 'new-event'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(new_kind, "standalone");
            drop(conn);

            let requests = tokio::time::timeout(std::time::Duration::from_secs(2), captured)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(requests.len(), 3);
            assert_events_query(&requests[1], None);
            assert_events_query(&requests[2], Some("old-token"));
        }
    }

    #[tokio::test]
    async fn unknown_cached_rows_are_reclassified_in_place_without_deleting_omitted_rows() {
        for has_token in [false, true] {
            let (_dir, db) = setup_db().await;
            {
                let conn = db.writer().await;
                for id in ["standalone", "occurrence", "malformed", "omitted"] {
                    cache_event(&conn, id, Some(id));
                }
                cache_event(&conn, "local-null", None);
                cache_event(&conn, "local-empty", Some(""));
                if has_token {
                    conn.execute(
                        "INSERT INTO app_metadata (key, value) VALUES (?1, 'old-token')",
                        [SYNC_KEY],
                    )
                    .unwrap();
                }
            }
            let mut occurrence = remote_event("occurrence");
            occurrence["recurringEventId"] = json!("master");
            occurrence["originalStartTime"] = json!({"dateTime": "2026-09-14T09:00:00Z"});
            let mut malformed = remote_event("malformed");
            malformed["recurrence"] = json!(42);
            let mut new_occurrence = occurrence.clone();
            new_occurrence["id"] = json!("new-occurrence");
            new_occurrence["start"] = json!({"dateTime": "2026-09-15T09:00:00Z"});
            new_occurrence["end"] = json!({"dateTime": "2026-09-15T10:00:00Z"});
            new_occurrence["originalStartTime"] = new_occurrence["start"].clone();
            let events = vec![
                remote_event("standalone"),
                occurrence,
                malformed,
                new_occurrence.clone(),
            ];
            let mut responses = vec![(
                200,
                json!({"items": [{"id": "primary", "summary": "Calendar", "primary": true}]}),
            )];
            if has_token {
                responses.push((
                    200,
                    json!({"items": events, "nextSyncToken": "metadata-token"}),
                ));
            }
            responses.push((
                200,
                json!({
                    "items": if has_token { vec![new_occurrence] } else { events },
                    "nextSyncToken": "new-token"
                }),
            ));
            let (root, captured) = serve_responses(responses).await;
            sync_google(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "google"),
            )
            .await
            .unwrap();

            let requests = captured.await.unwrap();
            assert_eq!(requests.len(), if has_token { 3 } else { 2 });
            assert_events_query(&requests[1], None);
            if has_token {
                assert_events_query(&requests[2], Some("old-token"));
            }
            let conn = db.reader();
            for (id, expected) in [
                ("standalone", RecurrenceKind::Standalone),
                ("occurrence", RecurrenceKind::Occurrence),
                ("malformed", RecurrenceKind::Unknown),
                ("omitted", RecurrenceKind::Unknown),
                ("local-null", RecurrenceKind::Unknown),
                ("local-empty", RecurrenceKind::Unknown),
            ] {
                let event = db::calendar::get_event(&conn, id).unwrap();
                assert_eq!(event.id, id);
                assert_eq!(event.recurrence_kind, expected, "{id}");
            }
            let new_kind: String = conn.query_row(
                "SELECT recurrence_kind FROM calendar_events WHERE remote_id = 'new-occurrence'", [], |row| row.get(0)
            ).unwrap();
            assert_eq!(new_kind, "occurrence");
            let token: String = conn
                .query_row(
                    "SELECT value FROM app_metadata WHERE key = ?1",
                    [SYNC_KEY],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(token, "new-token");
            assert!(needs_recurrence_refresh(&conn, "acc1", "cal1").unwrap());
        }
    }

    #[tokio::test]
    async fn token_request_decision_and_failed_refresh_preserve_cached_state() {
        for (remote_id, kind, token, incremental, status) in [
            (
                Some("remote"),
                RecurrenceKind::Unknown,
                Some("old-token"),
                true,
                500,
            ),
            (
                Some("remote"),
                RecurrenceKind::Unknown,
                Some("old-token"),
                true,
                410,
            ),
            (None, RecurrenceKind::Unknown, Some("old-token"), true, 500),
            (
                Some(""),
                RecurrenceKind::Unknown,
                Some("old-token"),
                true,
                500,
            ),
            (
                Some("remote"),
                RecurrenceKind::Standalone,
                Some("old-token"),
                true,
                500,
            ),
            (
                Some("remote"),
                RecurrenceKind::Occurrence,
                Some("old-token"),
                true,
                500,
            ),
            (Some("remote"), RecurrenceKind::Standalone, None, false, 500),
        ] {
            let (_dir, db) = setup_db().await;
            let before = {
                let conn = db.writer().await;
                cache_event(&conn, "cached", remote_id);
                conn.execute(
                    "UPDATE calendar_events SET recurrence_kind = ?1 WHERE id = 'cached'",
                    [kind.as_str()],
                )
                .unwrap();
                if let Some(token) = token {
                    conn.execute(
                        "INSERT INTO app_metadata (key, value) VALUES (?1, ?2)",
                        [SYNC_KEY, token],
                    )
                    .unwrap();
                }
                serde_json::to_value(db::calendar::get_event(&conn, "cached").unwrap()).unwrap()
            };
            let metadata_read = token.is_some()
                && kind == RecurrenceKind::Unknown
                && remote_id.is_some_and(|id| !id.is_empty());
            let mut responses = vec![(
                200,
                json!({"items": [{"id": "primary", "summary": "Calendar"}]}),
            )];
            if metadata_read {
                responses.push((status, json!({"error": "injected metadata read failure"})));
            }
            responses.push((500, json!({"error": "injected normal read failure"})));
            let (root, captured) = serve_responses(responses).await;
            sync_google(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "google"),
            )
            .await
            .unwrap();
            let requests = captured.await.unwrap();
            assert_eq!(requests.len(), if metadata_read { 3 } else { 2 });
            if metadata_read {
                assert_events_query(&requests[1], None);
            }
            let normal_request = requests.last().unwrap();
            assert_events_query(normal_request, incremental.then_some("old-token"));
            let conn = db.reader();
            let after =
                serde_json::to_value(db::calendar::get_event(&conn, "cached").unwrap()).unwrap();
            assert_eq!(after, before);
            let saved: Option<String> = conn
                .query_row(
                    "SELECT value FROM app_metadata WHERE key = ?1",
                    [SYNC_KEY],
                    |row| row.get(0),
                )
                .optional()
                .unwrap();
            assert_eq!(saved.as_deref(), token);
        }
    }

    #[tokio::test]
    async fn later_incremental_page_failure_never_applies_partial_changes() {
        for status in [500, 410] {
            let (_dir, db) = setup_db().await;
            {
                let conn = db.writer().await;
                cache_event(&conn, "cancelled", Some("cancelled"));
                conn.execute(
                    "UPDATE calendar_events SET recurrence_kind = 'standalone'
                     WHERE id = 'cancelled'",
                    [],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO app_metadata (key, value) VALUES (?1, 'old-token')",
                    [SYNC_KEY],
                )
                .unwrap();
            }
            let (root, captured) = serve_responses(vec![
                (
                    200,
                    json!({"items": [{"id": "primary", "summary": "Calendar"}]}),
                ),
                (
                    200,
                    json!({
                        "items": [
                            {"id": "cancelled", "status": "cancelled"},
                            remote_event("partial-new")
                        ],
                        "nextPageToken": "next-page"
                    }),
                ),
                (status, json!({"error": "injected later-page failure"})),
            ])
            .await;

            sync_google(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "google"),
            )
            .await
            .unwrap();

            let requests = captured.await.unwrap();
            assert_eq!(requests.len(), 3);
            assert_events_query(&requests[1], Some("old-token"));
            assert_events_query(&requests[2], Some("old-token"));
            let second_query = url::Url::parse(&format!(
                "http://localhost{}",
                requests[2].split_whitespace().nth(1).unwrap()
            ))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect::<std::collections::HashMap<_, _>>();
            assert_eq!(second_query["pageToken"], "next-page");

            let conn = db.reader();
            assert!(db::calendar::get_event(&conn, "cancelled").is_ok());
            let partial_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM calendar_events
                     WHERE remote_id = 'partial-new'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(partial_count, 0);
            let token: Option<String> = conn
                .query_row(
                    "SELECT value FROM app_metadata WHERE key = ?1",
                    [SYNC_KEY],
                    |row| row.get(0),
                )
                .optional()
                .unwrap();
            assert_eq!(
                token.as_deref(),
                if status == 410 {
                    None
                } else {
                    Some("old-token")
                }
            );
        }
    }

    #[tokio::test]
    async fn successful_recovery_stops_extra_reads_despite_unknown_local_only_rows() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_event(&conn, "cached", Some("remote"));
            cache_event(&conn, "local-only", None);
            conn.execute(
                "INSERT INTO app_metadata (key, value) VALUES (?1, 'old-token')",
                [SYNC_KEY],
            )
            .unwrap();
        }
        let calendars = json!({"items": [{"id": "primary", "summary": "Calendar"}]});
        let (root, captured) = serve_responses(vec![
            (200, calendars.clone()),
            (
                200,
                json!({"items": [remote_event("remote")], "nextSyncToken": "metadata-token"}),
            ),
            (200, json!({"items": [], "nextSyncToken": "delta-token"})),
            (200, calendars),
            (
                200,
                json!({"items": [], "nextSyncToken": "incremental-token"}),
            ),
        ])
        .await;
        let services = services(&root);
        let ctx = CalendarBackendCtx {
            db: &db,
            services: &services,
        };
        let account = account("calendar", "google");
        sync_google(&ctx, &account).await.unwrap();
        assert!(!needs_recurrence_refresh(&db.reader(), "acc1", "cal1").unwrap());
        sync_google(&ctx, &account).await.unwrap();

        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), 5);
        assert_events_query(&requests[1], None);
        assert_events_query(&requests[2], Some("old-token"));
        assert_events_query(&requests[4], Some("delta-token"));
        let conn = db.reader();
        assert_eq!(
            db::calendar::get_event(&conn, "cached")
                .unwrap()
                .recurrence_kind,
            RecurrenceKind::Standalone
        );
        assert_eq!(
            db::calendar::get_event(&conn, "local-only")
                .unwrap()
                .recurrence_kind,
            RecurrenceKind::Unknown
        );
    }

    #[tokio::test]
    async fn metadata_recovery_cannot_advance_cursor_or_apply_content_and_deletions() {
        for (normal_status, expected_token) in [(500, Some("old-token")), (410, None)] {
            let (_dir, db) = setup_db().await;
            {
                let conn = db.writer().await;
                for id in ["unknown", "cancelled", "known"] {
                    cache_event(&conn, id, Some(id));
                }
                cache_event(&conn, "local-only", None);
                conn.execute(
                    "UPDATE calendar_events SET recurrence_kind = 'standalone' WHERE id = 'known'",
                    [],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO app_metadata (key, value) VALUES (?1, 'old-token')",
                    [SYNC_KEY],
                )
                .unwrap();
            }
            let mut known = remote_event("known");
            known["recurringEventId"] = json!("master");
            known["originalStartTime"] = known["start"].clone();
            let (root, captured) = serve_responses(vec![
                (
                    200,
                    json!({"items": [{"id": "primary", "summary": "Calendar"}]}),
                ),
                (
                    200,
                    json!({
                        "items": [remote_event("unknown"), known,
                            {"id": "cancelled", "status": "cancelled"},
                            remote_event("local-only"), remote_event("metadata-only")],
                        "nextSyncToken": "metadata-token"
                    }),
                ),
                (
                    normal_status,
                    json!({"error": "injected normal read failure"}),
                ),
            ])
            .await;
            sync_google(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "google"),
            )
            .await
            .unwrap();
            let requests = captured.await.unwrap();
            assert_eq!(requests.len(), 3);
            assert_events_query(&requests[1], None);
            assert_events_query(&requests[2], Some("old-token"));
            let conn = db.reader();
            let token: Option<String> = conn
                .query_row(
                    "SELECT value FROM app_metadata WHERE key = ?1",
                    [SYNC_KEY],
                    |row| row.get(0),
                )
                .optional()
                .unwrap();
            assert_eq!(token.as_deref(), expected_token);
            for (id, kind) in [
                ("unknown", RecurrenceKind::Standalone),
                ("known", RecurrenceKind::Standalone),
                ("cancelled", RecurrenceKind::Unknown),
                ("local-only", RecurrenceKind::Unknown),
            ] {
                let event = db::calendar::get_event(&conn, id).unwrap();
                assert_eq!(event.recurrence_kind, kind, "{id}");
                assert_eq!(event.title, "Cached event", "{id}");
            }
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM calendar_events", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 4);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        google_rsvp_attendees_patch, google_rsvp_import_event, parse_google_attendees,
        parse_google_organizer, readable_foreground,
    };
    use crate::backend::calendar::{InviteResponse, RemoteRsvpRequest};
    use crate::backend::testutil::account;
    use crate::calendar::Attendee;

    #[test]
    fn light_background_gets_black_text() {
        assert_eq!(readable_foreground("#ffffff"), "#000000");
        assert_eq!(readable_foreground("#fbd75b"), "#000000");
    }

    #[test]
    fn dark_background_gets_white_text() {
        assert_eq!(readable_foreground("#000000"), "#ffffff");
        assert_eq!(readable_foreground("#3f51b5"), "#ffffff");
    }

    #[test]
    fn malformed_hex_defaults_to_black() {
        assert_eq!(readable_foreground("nope"), "#000000");
    }

    #[test]
    fn synced_attendees_include_the_accounts_response() {
        let event = serde_json::json!({
            "attendees": [
                {
                    "email": "organizer@example.com",
                    "displayName": "Organizer",
                    "responseStatus": "accepted"
                },
                {
                    "email": "alias@example.com",
                    "responseStatus": "tentative",
                    "self": true
                }
            ]
        });

        let (json, my_status) = parse_google_attendees(&event, "me@example.com", true);
        let attendees: Vec<crate::calendar::Attendee> =
            serde_json::from_str(json.as_deref().unwrap()).unwrap();

        assert_eq!(my_status.as_deref(), Some("tentative"));
        assert_eq!(attendees.len(), 2);
        assert_eq!(attendees[1].email, "alias@example.com");
        assert_eq!(attendees[1].status, "tentative");
        assert_eq!(attendees[1].is_self, Some(true));
    }

    #[test]
    fn google_needs_action_maps_to_the_canonical_status() {
        let event = serde_json::json!({
            "attendees": [{
                "email": "me@example.com",
                "responseStatus": "needsAction"
            }]
        });

        let (_, my_status) = parse_google_attendees(&event, "ME@example.com", false);
        assert_eq!(my_status.as_deref(), Some("needs-action"));
    }

    #[test]
    fn shared_calendar_self_attendee_is_not_the_account() {
        let event = serde_json::json!({
            "attendees": [{
                "email": "room@example.com",
                "responseStatus": "accepted",
                "self": true
            }]
        });

        let (_, my_status) = parse_google_attendees(&event, "me@example.com", false);
        assert!(my_status.is_none());
    }

    #[test]
    fn primary_calendar_self_organizer_uses_the_account_identity() {
        let event = serde_json::json!({
            "organizer": {
                "email": "alias@example.com",
                "self": true
            }
        });

        assert_eq!(
            parse_google_organizer(&event, "me@example.com", true).as_deref(),
            Some("me@example.com")
        );
        assert_eq!(
            parse_google_organizer(&event, "me@example.com", false).as_deref(),
            Some("alias@example.com")
        );
    }

    #[test]
    fn rsvp_import_payload_preserves_event_and_response_fields() {
        let account = account("calendar", "google");
        let request = RemoteRsvpRequest {
            uid: "event@example.com".into(),
            response: InviteResponse::Tentative,
            summary: Some("Planning".into()),
            start_time: "2026-08-10".into(),
            end_time: "2026-08-11".into(),
            all_day: true,
            description: Some("Agenda".into()),
            location: Some("Room 1".into()),
            organizer_email: Some("organizer@example.com".into()),
            attendees: Vec::new(),
        };

        assert_eq!(
            google_rsvp_import_event(&account, &request),
            serde_json::json!({
                "iCalUID": "event@example.com",
                "summary": "Planning",
                "start": {"date": "2026-08-10"},
                "end": {"date": "2026-08-11"},
                "description": "Agenda",
                "location": "Room 1",
                "organizer": {"email": "organizer@example.com"},
                "attendees": [{
                    "email": "u@example.com",
                    "responseStatus": "tentative",
                    "self": true,
                }],
            })
        );
    }

    #[test]
    fn rsvp_patch_preserves_all_remote_attendees_and_alias_metadata() {
        let event = serde_json::json!({
            "attendees": [
                {
                    "email": "organizer@example.com",
                    "responseStatus": "accepted",
                    "organizer": true,
                    "comment": "preserve me"
                },
                {
                    "email": "alias@example.com",
                    "responseStatus": "accepted",
                    "self": true,
                    "additionalGuests": 2
                },
                {
                    "email": "guest@example.com",
                    "responseStatus": "tentative"
                }
            ]
        });

        let patch = google_rsvp_attendees_patch(&event, "me@example.com", "declined");
        let attendees = patch["attendees"].as_array().unwrap();

        assert_eq!(attendees.len(), 3);
        assert_eq!(attendees[0], event["attendees"][0]);
        assert_eq!(attendees[2], event["attendees"][2]);
        assert_eq!(attendees[1]["email"], "alias@example.com");
        assert_eq!(attendees[1]["responseStatus"], "declined");
        assert_eq!(attendees[1]["additionalGuests"], 2);
    }

    #[test]
    fn rsvp_import_preserves_invite_guest_list() {
        let account = account("calendar", "google");
        let request = RemoteRsvpRequest {
            uid: "event@example.com".into(),
            response: InviteResponse::Declined,
            summary: Some("Planning".into()),
            start_time: "2026-08-10T09:00:00Z".into(),
            end_time: "2026-08-10T10:00:00Z".into(),
            all_day: false,
            description: None,
            location: None,
            organizer_email: Some("organizer@example.com".into()),
            attendees: vec![
                Attendee {
                    email: "guest@example.com".into(),
                    name: Some("Guest".into()),
                    status: "accepted".into(),
                    is_self: None,
                },
                Attendee {
                    email: "alias@example.com".into(),
                    name: None,
                    status: "accepted".into(),
                    is_self: Some(true),
                },
            ],
        };

        let imported = google_rsvp_import_event(&account, &request);
        let attendees = imported["attendees"].as_array().unwrap();
        assert_eq!(attendees.len(), 2);
        assert_eq!(attendees[0]["email"], "guest@example.com");
        assert_eq!(attendees[0]["displayName"], "Guest");
        assert_eq!(attendees[0]["responseStatus"], "accepted");
        assert_eq!(attendees[1]["email"], "alias@example.com");
        assert_eq!(attendees[1]["responseStatus"], "declined");
        assert_eq!(attendees[1]["self"], true);
    }
}
