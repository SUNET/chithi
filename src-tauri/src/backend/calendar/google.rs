//! Google calendar backend (Calendar API v3 with OAuth2).

use async_trait::async_trait;

use crate::db;
use crate::db::accounts::AccountFull;
use crate::db::calendar::CalendarEvent;
use crate::error::{Error, Result};
use crate::mail::google::{
    event_patch_to_google_json, event_to_google_json, send_updates_for, EventsPage,
};

use super::{
    BusyPeriod, CalendarBackend, CalendarBackendCtx, CalendarCapability, ParticipantSchedule,
    ParticipantScheduleRequest, PushedEvent, RemoteRsvpOutcome, RemoteRsvpPolicy,
    RemoteRsvpRequest,
};

pub struct GoogleCalendarBackend;

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

    let mut remote_to_local: std::collections::HashMap<String, String> =
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
                remote_to_local.insert(cal_id.to_string(), local_id);
            }
        }
    }

    // Step 2: Fetch events for each calendar (with syncToken for incremental sync)
    for (remote_cal_id, local_cal_id) in &remote_to_local {
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

        let page = if let Some(ref token) = existing_token {
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

        let conn = db.writer().await;
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
                    let deleted = conn
                        .execute(
                            "DELETE FROM calendar_events WHERE account_id = ?1 AND remote_id = ?2",
                            rusqlite::params![account_id, event_id_remote],
                        )
                        .unwrap_or(0);
                    // Also delete by iCalUID for events created locally via respond_to_invite
                    if let Some(ical_uid) = ev["iCalUID"].as_str() {
                        conn.execute(
                            "DELETE FROM calendar_events WHERE account_id = ?1 AND uid = ?2 AND remote_id IS NULL",
                            rusqlite::params![account_id, ical_uid],
                        ).ok();
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

                let organizer_email = ev["organizer"]["email"].as_str().map(|s| s.to_string());
                let uid = ev["iCalUID"].as_str().map(|s| s.to_string());

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
                    organizer_email,
                    attendees_json: None,
                    my_status: None,
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
        if existing_token.is_none() && !server_event_ids.is_empty() {
            let conn = db.writer().await;
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

            let mut deleted = 0;
            for (local_id, remote_id) in &local_events {
                if !server_event_ids.contains(remote_id) {
                    db::calendar::delete_event(&conn, local_id).ok();
                    deleted += 1;
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
                        db::calendar::delete_event(&conn, local_id).ok();
                        deleted += 1;
                    }
                }
            }
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
        let mut event_id = client
            .find_event_by_ical_uid("primary", &request.uid)
            .await
            .ok()
            .flatten();

        if event_id.is_none() {
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

        if let Some(remote_id) = event_id.as_deref().filter(|id| !id.is_empty()) {
            let attendees_patch = serde_json::json!({
                "attendees": [{
                    "email": account.email,
                    "responseStatus": request.response.as_str(),
                    "self": true,
                }]
            });
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

    /// Google events are always created on the primary calendar (the
    /// pre-trait behaviour); `remote_calendar_id` is ignored.
    async fn push_created_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        event: &CalendarEvent,
        _remote_calendar_id: &str,
    ) -> Result<Option<PushedEvent>> {
        let client = ctx.services.google_client(&account.id).await?;
        let google_event = event_to_google_json(event);
        let send_updates = send_updates_for(event.attendees_json.as_deref());
        let (remote_id, canonical_uid) = client
            .create_event("primary", &google_event, send_updates)
            .await?;
        Ok(Some(PushedEvent {
            remote_id,
            canonical_uid,
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
        "attendees": [{
            "email": account.email,
            "responseStatus": request.response.as_str(),
            "self": true,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::{google_rsvp_import_event, readable_foreground};
    use crate::backend::calendar::{InviteResponse, RemoteRsvpRequest};
    use crate::backend::testutil::account;

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
}
