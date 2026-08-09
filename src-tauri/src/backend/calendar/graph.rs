//! Microsoft Graph calendar backend (O365 / Exchange Online).

use async_trait::async_trait;

use crate::db;
use crate::db::accounts::AccountFull;
use crate::db::calendar::{CalendarEvent, NewCalendar};
use crate::db::pool::DbPool;
use crate::error::Result;
use crate::mail::graph::{
    event_patch_to_graph_json, event_to_graph_json, get_graph_token, GraphClient,
};

use super::{
    BusyPeriod, CalendarBackend, CalendarCapability, InviteReplyDelivery, ParticipantSchedule,
    ParticipantScheduleRequest, PushedEvent, RemoteRsvpOutcome, RemoteRsvpPolicy,
    RemoteRsvpRequest, RoomAvailability, RoomAvailabilityRequest, RoomSuggestion,
};

pub struct GraphCalendarBackend;

#[async_trait]
impl CalendarBackend for GraphCalendarBackend {
    fn protocol(&self) -> &'static str {
        "graph"
    }

    fn invite_reply_delivery(&self) -> InviteReplyDelivery {
        InviteReplyDelivery::Provider
    }

    fn remote_rsvp_policy(&self) -> RemoteRsvpPolicy {
        RemoteRsvpPolicy::RequiredBeforeLocal
    }

    async fn apply_remote_rsvp(
        &self,
        account: &AccountFull,
        request: &RemoteRsvpRequest,
    ) -> Result<CalendarCapability<RemoteRsvpOutcome>> {
        let token = get_graph_token(&account.id).await?;
        let client = GraphClient::new(&token);
        let event_id = client
            .find_event_by_ical_uid(&request.uid)
            .await?
            .ok_or_else(|| {
                crate::error::Error::Other(
                    "This invitation isn't on your Outlook calendar yet. \
                     Sync the calendar and try again."
                        .into(),
                )
            })?;
        client
            .rsvp_event(&event_id, request.response.as_str(), "")
            .await?;
        Ok(CalendarCapability::Supported(RemoteRsvpOutcome {
            remote_id: Some(event_id),
        }))
    }

    async fn list_room_suggestions(
        &self,
        account: &AccountFull,
    ) -> Result<CalendarCapability<Vec<RoomSuggestion>>> {
        let token = crate::mail::graph::get_graph_token_for_rooms(&account.id).await?;
        let rooms = GraphClient::new(&token).list_rooms().await?;
        Ok(CalendarCapability::Supported(
            rooms
                .into_iter()
                .map(|room| RoomSuggestion {
                    name: room.name,
                    address: room.address,
                })
                .collect(),
        ))
    }

    async fn check_room_availability(
        &self,
        account: &AccountFull,
        request: &RoomAvailabilityRequest,
    ) -> Result<CalendarCapability<RoomAvailability>> {
        let token = crate::mail::graph::get_graph_token_for_rooms(&account.id).await?;
        let availability = GraphClient::new(&token)
            .get_room_availability(
                &request.room_address,
                &request.start_time,
                &request.end_time,
            )
            .await?;
        Ok(CalendarCapability::Supported(RoomAvailability {
            state: availability.state,
            busy_start: availability.busy_start,
            busy_end: availability.busy_end,
        }))
    }

    async fn get_participant_schedules(
        &self,
        account: &AccountFull,
        request: &ParticipantScheduleRequest,
    ) -> Result<CalendarCapability<Vec<ParticipantSchedule>>> {
        let token = get_graph_token(&account.id).await?;
        let schedules = GraphClient::new(&token)
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

    async fn sync(&self, db: &DbPool, account: &AccountFull) -> Result<()> {
        let account_id = account.id.as_str();
        log::info!("sync_calendars_graph: starting for account {}", account_id);

        let token = match get_graph_token(account_id).await {
            Ok(t) => t,
            Err(e) => {
                log::error!("sync_calendars_graph: failed to get token: {}", e);
                return Err(e);
            }
        };
        let client = GraphClient::new(&token);

        // 1. List Graph calendars and upsert each into the local table.
        // Multi-calendar support (#47): we keep a remote_id -> (local_id,
        // is_subscribed) map so the per-calendar event sync below can map
        // events to the right local calendar AND skip calendars the user
        // has unsubscribed from.
        let graph_calendars = match client.list_calendars().await {
            Ok(c) => c,
            Err(e) => {
                log::error!("sync_calendars_graph: list_calendars failed: {}", e);
                return Err(e);
            }
        };
        log::info!(
            "sync_calendars_graph: fetched {} calendars",
            graph_calendars.len()
        );

        let mut remote_to_local: std::collections::HashMap<String, (String, bool)> =
            std::collections::HashMap::new();

        {
            let conn = db.writer().await;
            for gc in &graph_calendars {
                // Look up existing row to preserve the user's is_subscribed
                // setting; if absent, we insert and default-subscribe.
                let existing: Option<(String, bool)> = conn
                    .query_row(
                        "SELECT id, is_subscribed FROM calendars WHERE account_id = ?1 AND remote_id = ?2",
                        rusqlite::params![account_id, gc.id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .ok();

                let (local_id, subscribed) = match existing {
                    Some((local_id, subscribed)) => {
                        // Preserve the locally stored color: it may have been set
                        // via the sidebar's color picker, and Graph sometimes
                        // refuses the PATCH (shared / system calendars return 500
                        // ISE), so the *only* place that pick exists is locally.
                        // Stomping it with `gc.color` here resets shared calendars
                        // back to their Graph default on every resync (#132).
                        conn.execute(
                            "UPDATE calendars SET name = ?1 WHERE id = ?2",
                            rusqlite::params![gc.name, local_id],
                        )
                        .ok();
                        (local_id, subscribed)
                    }
                    None => {
                        let cal_id = uuid::Uuid::new_v4().to_string();
                        let cal = NewCalendar {
                            account_id: account_id.to_string(),
                            name: gc.name.clone(),
                            color: gc.color.clone(),
                            is_default: gc.is_default,
                        };
                        db::calendar::insert_calendar(&conn, &cal_id, &cal)?;
                        conn.execute(
                            "UPDATE calendars SET remote_id = ?1 WHERE id = ?2",
                            rusqlite::params![gc.id, cal_id],
                        )
                        .ok();
                        log::info!(
                            "sync_calendars_graph: created calendar '{}' ({})",
                            gc.name,
                            gc.id
                        );
                        (cal_id, true)
                    }
                };
                remote_to_local.insert(gc.id.clone(), (local_id, subscribed));
            }
        }

        // 2. Fetch events for each subscribed calendar individually
        // (`/me/calendars/{id}/calendarView`) — the previous all-account
        // `/me/calendarView` collapsed every calendar's events onto the
        // default calendar.
        let now = chrono::Utc::now();
        let start =
            (now - chrono::Duration::days(90)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let end =
            (now + chrono::Duration::days(90)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        for gc in &graph_calendars {
            let Some((local_cal_id, subscribed)) = remote_to_local.get(&gc.id) else {
                continue;
            };
            if !subscribed {
                log::debug!(
                    "sync_calendars_graph: skipping unsubscribed calendar '{}'",
                    gc.name
                );
                continue;
            }

            let calendar_events = match client.list_events_for_calendar(&gc.id, &start, &end).await
            {
                Ok(e) => e,
                Err(e) => {
                    log::error!(
                        "sync_calendars_graph: list_events_for_calendar('{}') failed: {}",
                        gc.name,
                        e
                    );
                    continue;
                }
            };
            log::info!(
                "sync_calendars_graph: fetched {} events for calendar '{}'",
                calendar_events.len(),
                gc.name
            );

            let conn = db.writer().await;
            let server_ids: std::collections::HashSet<String> =
                calendar_events.iter().map(|e| e.id.clone()).collect();

            for ge in &calendar_events {
                let existing = conn.query_row(
                    "SELECT id FROM calendar_events WHERE account_id = ?1 AND remote_id = ?2",
                    rusqlite::params![account_id, ge.id],
                    |row| row.get::<_, String>(0),
                );

                match existing {
                    Ok(local_id) => {
                        // Update in place. Also re-pin calendar_id in case
                        // the event moved between calendars on the server.
                        conn.execute(
                            "UPDATE calendar_events SET title = ?1, start_time = ?2, end_time = ?3,
                             all_day = ?4, location = ?5, organizer_email = ?6, attendees_json = ?7,
                             description = ?8, timezone = ?9, my_status = ?10, calendar_id = ?11
                             WHERE id = ?12",
                            rusqlite::params![
                                ge.subject,
                                ge.start,
                                ge.end,
                                ge.all_day,
                                ge.location,
                                ge.organizer_email,
                                ge.attendees_json,
                                ge.body_preview,
                                ge.timezone,
                                ge.my_status,
                                local_cal_id,
                                local_id,
                            ],
                        )
                        .ok();
                    }
                    Err(_) => {
                        let event = CalendarEvent {
                            id: uuid::Uuid::new_v4().to_string(),
                            account_id: account_id.to_string(),
                            calendar_id: local_cal_id.clone(),
                            uid: ge.ical_uid.clone(),
                            title: ge.subject.clone(),
                            description: ge.body_preview.clone(),
                            location: ge.location.clone(),
                            start_time: ge.start.clone(),
                            end_time: ge.end.clone(),
                            all_day: ge.all_day,
                            timezone: ge.timezone.clone(),
                            recurrence_rule: None,
                            organizer_email: ge.organizer_email.clone(),
                            attendees_json: ge.attendees_json.clone(),
                            my_status: ge.my_status.clone(),
                            source_message_id: None,
                            ical_data: None,
                            remote_id: Some(ge.id.clone()),
                            etag: None,
                        };
                        db::calendar::insert_event(&conn, &event)?;
                    }
                }
            }

            // Per-calendar reconciliation: drop events that this calendar
            // used to carry but that the server no longer returns. Scoped
            // to calendar_id so a deletion in one calendar doesn't wipe
            // events still present in another.
            let local_events: Vec<(String, String)> = conn
                .prepare(
                    "SELECT id, remote_id FROM calendar_events
                     WHERE account_id = ?1 AND calendar_id = ?2
                       AND remote_id IS NOT NULL AND remote_id != ''",
                )?
                .query_map(rusqlite::params![account_id, local_cal_id], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .filter_map(|r| r.ok())
                .collect();

            let mut deleted = 0;
            for (local_id, remote_id) in &local_events {
                if !server_ids.contains(remote_id) {
                    db::calendar::delete_event(&conn, local_id)?;
                    deleted += 1;
                }
            }
            if deleted > 0 {
                log::info!(
                    "sync_calendars_graph: removed {} server-deleted events from '{}'",
                    deleted,
                    gc.name
                );
            }
        }

        log::info!("sync_calendars_graph: completed for account {}", account_id);
        Ok(())
    }

    /// Created on the account's default calendar (Graph resolves it);
    /// `remote_calendar_id` is ignored. Graph sends invite emails
    /// automatically when attendees are present.
    async fn push_created_event(
        &self,
        account: &AccountFull,
        event: &CalendarEvent,
        _remote_calendar_id: &str,
    ) -> Result<Option<PushedEvent>> {
        let token = get_graph_token(&account.id).await?;
        let client = GraphClient::new(&token);
        let graph_event = event_to_graph_json(event);
        if let Some(atts) = graph_event["attendees"].as_array() {
            log::info!("create_event: O365 event with {} attendees", atts.len());
        }
        log::debug!(
            "create_event: O365 graph_event JSON: {}",
            serde_json::to_string_pretty(&graph_event).unwrap_or_default()
        );
        let (remote_id, ical_uid) = client.create_event(&graph_event).await?;
        Ok(Some(PushedEvent {
            remote_id,
            canonical_uid: ical_uid,
        }))
    }

    async fn push_updated_event(
        &self,
        account: &AccountFull,
        remote_id: &str,
        event: &CalendarEvent,
    ) -> Result<()> {
        let token = get_graph_token(&account.id).await?;
        let client = GraphClient::new(&token);
        let patch = event_patch_to_graph_json(event);
        client.update_event(remote_id, &patch).await
    }

    async fn push_deleted_event(
        &self,
        account: &AccountFull,
        remote_id: &str,
        _remote_calendar_id: &str,
    ) -> Result<()> {
        let token = get_graph_token(&account.id).await?;
        let client = GraphClient::new(&token);
        client.delete_event(remote_id).await
    }

    async fn push_calendar_rename(
        &self,
        account: &AccountFull,
        remote_id: &str,
        name: &str,
    ) -> Result<()> {
        let token = get_graph_token(&account.id).await?;
        let client = GraphClient::new(&token);
        client.rename_calendar(remote_id, name).await
    }

    /// Microsoft Graph wants a constrained `calendarColor` enum.
    /// The hex-to-nearest-named lookup lives in graph.rs so we can
    /// round-trip our own palette consistently. Some calendars
    /// (system / shared / read-only series like "Birthdays" and
    /// holiday subscriptions) reject color writes with a generic
    /// 500 ISE rather than a structured error, so we degrade to
    /// local-only on any Graph-side failure rather than rolling
    /// back the user's pick. Local DB keeps the user's exact hex
    /// so the sidebar shows what they picked.
    async fn push_calendar_color(
        &self,
        account: &AccountFull,
        remote_id: &str,
        color: &str,
    ) -> Result<()> {
        let token = get_graph_token(&account.id).await?;
        let client = GraphClient::new(&token);
        if let Err(e) = client.set_calendar_color(remote_id, color).await {
            log::warn!(
                "update_calendar: Graph color push failed (calendar may be read-only or shared), keeping local-only: {}",
                e
            );
        }
        Ok(())
    }
}
