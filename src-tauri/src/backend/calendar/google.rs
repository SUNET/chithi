//! Google calendar backend (Calendar API v3 with OAuth2).

use async_trait::async_trait;

use crate::calendar::{Attendee, CalendarEvent, RecurrenceKind};
use crate::db;
use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::mail::google::{
    event_patch_to_google_json, event_to_google_json, google_recurrence_kind,
    invitation_copy_patch_to_google_json, send_updates_for, EventsPage, GoogleClient,
};

use super::{
    BusyPeriod, CalendarBackend, CalendarBackendCtx, CalendarCapability, ParticipantSchedule,
    ParticipantScheduleRequest, PushedEvent, RemoteRsvpOutcome, RemoteRsvpPolicy,
    RemoteRsvpRequest,
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
        EventsPage::Page(data) => data,
        EventsPage::SyncTokenExpired => {
            return Err(Error::Other(
                "Google recurrence metadata read returned HTTP 410 without a sync token".into(),
            ));
        }
    };
    let events = match data.get("items") {
        None => return Ok(()),
        Some(serde_json::Value::Array(events)) => events,
        Some(_) => {
            return Err(Error::Other(
                "Google recurrence metadata items must be an array".into(),
            ))
        }
    };
    let conn = db.writer().await;
    for event in events {
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
        conn.execute(
            "UPDATE calendar_events SET recurrence_kind = ?1
             WHERE account_id = ?2 AND calendar_id = ?3 AND remote_id = ?4
               AND recurrence_kind = 'unknown'",
            rusqlite::params![kind.as_str(), account_id, calendar_id, remote_id],
        )?;
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
            Ok(EventsPage::Page(data)) => data,
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

        let events = events_data["items"].as_array();
        let count = events.map(|e| e.len()).unwrap_or(0);
        log::info!(
            "sync_calendars_google: fetched {} events for calendar {}",
            count,
            remote_cal_id
        );

        let mut conn = db.writer().await;
        let mut server_event_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut server_uids: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Some(events) = events {
            for ev in events {
                let event_id_remote = ev["id"].as_str().unwrap_or_default();
                server_event_ids.insert(event_id_remote.to_string());
                if let Some(uid) = ev["iCalUID"].as_str() {
                    server_uids.insert(uid.to_string());
                }

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

                if let Err(e) = db::calendar::upsert_event_by_remote_id(&conn, &cal_event) {
                    log::error!("sync_calendars_google: upsert event failed: {}", e);
                }
            }
        }

        // Drop the conn lock before acquiring again for syncToken
        drop(conn);

        // Save nextSyncToken for incremental sync next time
        if let Some(next_token) = events_data["nextSyncToken"].as_str() {
            let conn = db.writer().await;
            conn.execute(
                "INSERT OR REPLACE INTO app_metadata (key, value) VALUES (?1, ?2)",
                rusqlite::params![sync_key, next_token],
            )
            .ok();
            log::debug!(
                "sync_calendars_google: saved syncToken for calendar {}",
                remote_cal_id
            );
        }

        // During full sync (no syncToken), reconcile: delete local events
        // whose remote_id no longer appears on the server. Incremental sync
        // handles deletions via "status: cancelled" (see above).
        // Bootstrapping legacy unknown rows remains non-destructive: a bounded
        // initial read can omit them. Incremental cancellations are always
        // consumed above, independently of the metadata recovery pass.
        if !has_unknown_events && existing_token.is_none() && !server_event_ids.is_empty() {
            let mut conn = db.writer().await;
            let local_events: Vec<(String, String)> = conn
                .prepare(
                    "SELECT ce.id, ce.remote_id FROM calendar_events ce
                     JOIN calendars c ON ce.calendar_id = c.id
                     WHERE ce.account_id = ?1 AND ce.remote_id IS NOT NULL AND ce.remote_id != ''
                     AND c.remote_id = ?2",
                )
                .map(|mut stmt| {
                    stmt.query_map(rusqlite::params![account_id, remote_cal_id], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default()
                })
                .unwrap_or_default();

            let mut deleted_ids = Vec::new();
            for (local_id, remote_id) in &local_events {
                if !server_event_ids.contains(remote_id) {
                    deleted_ids.push(local_id.clone());
                }
            }
            // Also remove orphan events (no remote_id) by matching UID
            if !server_uids.is_empty() {
                let orphans: Vec<(String, String)> = conn
                    .prepare(
                        "SELECT ce.id, ce.uid FROM calendar_events ce
                         JOIN calendars c ON ce.calendar_id = c.id
                         WHERE ce.account_id = ?1 AND (ce.remote_id IS NULL OR ce.remote_id = '')
                         AND ce.uid IS NOT NULL AND c.remote_id = ?2",
                    )
                    .map(|mut stmt| {
                        stmt.query_map(rusqlite::params![account_id, remote_cal_id], |row| {
                            Ok((row.get(0)?, row.get(1)?))
                        })
                        .map(|rows| rows.filter_map(|r| r.ok()).collect())
                        .unwrap_or_default()
                    })
                    .unwrap_or_default();
                for (local_id, uid) in &orphans {
                    if !server_uids.contains(uid) {
                        deleted_ids.push(local_id.clone());
                    }
                }
            }
            let deleted = if deleted_ids.is_empty() {
                0
            } else {
                match conn.transaction() {
                    Ok(transaction) => {
                        match db::calendar_event_deletion::delete_events(&transaction, &deleted_ids)
                        {
                            Ok(result) if transaction.commit().is_ok() => result.deleted,
                            _ => 0,
                        }
                    }
                    Err(_) => 0,
                }
            };
            if deleted > 0 {
                log::info!(
                    "sync_calendars_google: removed {} server-deleted events from '{}'",
                    deleted,
                    remote_cal_id
                );
            }
        }
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

    async fn serve_requests(
        method: &'static str,
        responses: Vec<(u16, serde_json::Value)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}/calendar-api", listener.local_addr().unwrap());
        let captured = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
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
mod recurrence_sync_tests {
    use super::sync_testutil::{cache_event, serve_responses, services, setup_db};
    use super::{needs_recurrence_refresh, sync_google, CalendarBackendCtx};
    use crate::backend::testutil::account;
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
