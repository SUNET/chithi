//! Microsoft Graph calendar backend (O365 / Exchange Online).

use async_trait::async_trait;

use crate::calendar::CalendarEvent;
use crate::db;
use crate::db::accounts::AccountFull;
use crate::db::calendar::NewCalendar;
use crate::error::Result;
use crate::mail::graph::{
    event_patch_to_graph_json, event_to_graph_json, invitation_copy_patch_to_graph_json,
};
use crate::provider::GraphTokenPurpose;

use super::{
    BusyPeriod, CalendarBackend, CalendarBackendCtx, CalendarCapability, InviteReplyDelivery,
    ParticipantSchedule, ParticipantScheduleRequest, PushedEvent, RemoteRsvpOutcome,
    RemoteRsvpPolicy, RemoteRsvpRequest, RoomAvailability, RoomAvailabilityRequest, RoomSuggestion,
};

pub struct GraphCalendarBackend;

#[async_trait]
impl CalendarBackend for GraphCalendarBackend {
    fn protocol(&self) -> &'static str {
        "graph"
    }

    fn event_creation_target(&self) -> super::EventCreationTarget {
        super::EventCreationTarget::AccountDefault
    }

    fn recurring_import_fidelity(&self) -> super::RecurringImportFidelity {
        super::RecurringImportFidelity::PatternedRecurrence
    }

    fn invite_reply_delivery(&self) -> InviteReplyDelivery {
        InviteReplyDelivery::Provider
    }

    fn remote_rsvp_policy(&self) -> RemoteRsvpPolicy {
        RemoteRsvpPolicy::RequiredBeforeLocal
    }

    async fn apply_remote_rsvp(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        request: &RemoteRsvpRequest,
    ) -> Result<CalendarCapability<RemoteRsvpOutcome>> {
        let client = ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?;
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
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
    ) -> Result<CalendarCapability<Vec<RoomSuggestion>>> {
        let client = match ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Rooms)
            .await
        {
            Ok(client) => client,
            Err(error) => {
                log::debug!(
                    "list_room_suggestions: room credentials unavailable, allowing free text: {}",
                    error
                );
                return Ok(CalendarCapability::Supported(Vec::new()));
            }
        };
        let rooms = client.list_rooms().await?;
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
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        request: &RoomAvailabilityRequest,
    ) -> Result<CalendarCapability<RoomAvailability>> {
        let client = match ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Rooms)
            .await
        {
            Ok(client) => client,
            Err(error) => {
                log::debug!(
                    "check_room_availability: room credentials unavailable: {}",
                    error
                );
                return Ok(CalendarCapability::Supported(RoomAvailability {
                    state: "unknown".into(),
                    busy_start: None,
                    busy_end: None,
                }));
            }
        };
        let availability = client
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
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        request: &ParticipantScheduleRequest,
    ) -> Result<CalendarCapability<Vec<ParticipantSchedule>>> {
        let schedules = ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
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

    async fn sync(&self, ctx: &CalendarBackendCtx<'_>, account: &AccountFull) -> Result<()> {
        let db = ctx.db;
        let account_id = account.id.as_str();
        log::info!("sync_calendars_graph: starting for account {}", account_id);

        let client = match ctx
            .services
            .graph_client(account_id, GraphTokenPurpose::Baseline)
            .await
        {
            Ok(client) => client,
            Err(e) => {
                log::error!("sync_calendars_graph: failed to get token: {}", e);
                return Err(e);
            }
        };
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

            let mut conn = db.writer().await;
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
                        let transaction = conn.transaction()?;
                        transaction.execute(
                            "UPDATE calendar_events SET title = ?1, start_time = ?2, end_time = ?3,
                             all_day = ?4, location = ?5, organizer_email = ?6, attendees_json = ?7,
                             description = ?8, timezone = ?9, my_status = ?10, calendar_id = ?11,
                             recurrence_kind = ?12
                             WHERE id = ?13",
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
                                ge.recurrence_kind.as_str(),
                                local_id,
                            ],
                        )?;
                        db::calendar_invitation::invalidate(&transaction, &local_id)?;
                        transaction.commit()?;
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
                            recurrence_kind: ge.recurrence_kind,
                            organizer_email: ge.organizer_email.clone(),
                            attendees_json: ge.attendees_json.clone(),
                            my_status: ge.my_status.clone(),
                            source_message_id: None,
                            ical_data: None,
                            remote_id: Some(ge.id.clone()),
                            etag: None,
                        };
                        let transaction = conn.transaction()?;
                        db::calendar::insert_event(&transaction, &event)?;
                        db::calendar_invitation::invalidate(&transaction, &event.id)?;
                        transaction.commit()?;
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

            let deleted_ids: Vec<String> = local_events
                .iter()
                .filter(|(_, remote_id)| !server_ids.contains(remote_id))
                .map(|(local_id, _)| local_id.clone())
                .collect();
            let mut deleted = 0;
            if !deleted_ids.is_empty() {
                let transaction = conn.transaction()?;
                deleted =
                    db::calendar_event_deletion::delete_events(&transaction, &deleted_ids)?.deleted;
                transaction.commit()?;
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

    fn validate_event_creation(&self, event: &CalendarEvent, _: &str) -> Result<()> {
        event_to_graph_json(event).map(|_| ())
    }

    /// Created on the account's default calendar (Graph resolves it);
    /// `remote_calendar_id` is ignored. Graph sends invite emails
    /// automatically when attendees are present.
    async fn push_created_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        event: &CalendarEvent,
        _remote_calendar_id: &str,
    ) -> Result<Option<PushedEvent>> {
        let graph_event = event_to_graph_json(event)?;
        let client = ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?;
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
        let client = ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?;
        let patch = event_patch_to_graph_json(event);
        client.update_event(remote_id, &patch).await
    }

    async fn push_updated_invitation_copy(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        event: &CalendarEvent,
    ) -> Result<Option<String>> {
        let client = ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?;
        client
            .update_event(remote_id, &invitation_copy_patch_to_graph_json(event)?)
            .await?;
        Ok(None)
    }

    async fn push_deleted_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        _remote_calendar_id: &str,
    ) -> Result<()> {
        let client = ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?;
        client.delete_event(remote_id).await
    }

    async fn push_calendar_rename(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        name: &str,
    ) -> Result<()> {
        let client = ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?;
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
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        color: &str,
    ) -> Result<()> {
        let client = ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?;
        if let Err(e) = client.set_calendar_color(remote_id, color).await {
            log::warn!(
                "update_calendar: Graph color push failed (calendar may be read-only or shared), keeping local-only: {}",
                e
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod creation_tests {
    use super::GraphCalendarBackend;
    use crate::backend::calendar::google::{
        creation_testutil::assert_standalone_creation,
        sync_testutil::{serve_create_response, serve_patch_response, services, setup_db},
    };
    use crate::backend::calendar::{CalendarBackend, CalendarBackendCtx};
    use crate::backend::testutil::{account, event};
    use crate::calendar::RecurrenceKind;

    #[tokio::test]
    async fn publishes_confirmed_standalone_creation() {
        assert_standalone_creation(
            &GraphCalendarBackend,
            serde_json::json!({"id": "created-event", "iCalUId": "canonical@example.test"}),
            "/calendar-api/me/events",
            "subject",
        )
        .await;
    }

    #[tokio::test]
    async fn publishes_supported_recurrence_in_the_graph_request() {
        let (_directory, db) = setup_db().await;
        let (root, captured) = serve_create_response(
            serde_json::json!({"id": "created-series", "iCalUId": "series@example.test"}),
        )
        .await;
        let mut event = event();
        event.start_time = "2026-09-14T09:00:00Z".into();
        event.end_time = "2026-09-14T10:00:00Z".into();
        event.timezone = Some("Europe/Stockholm".into());
        event.recurrence_kind = RecurrenceKind::Series;
        event.recurrence_rule = Some("FREQ=WEEKLY;BYDAY=MO;COUNT=3".into());
        event.ical_data = Some(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:series@example.test\r\n\
             RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=3\r\nEND:VEVENT\r\n\
             END:VCALENDAR\r\n"
                .into(),
        );
        let provider_services = services(&root);

        let pushed = GraphCalendarBackend
            .push_created_event(
                &CalendarBackendCtx {
                    db: &db,
                    services: &provider_services,
                },
                &account("calendar", "graph"),
                &event,
                "ignored",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pushed.remote_id, "created-series");
        let requests = captured.await.unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(payload["recurrence"]["pattern"]["type"], "weekly");
        assert_eq!(payload["recurrence"]["range"]["numberOfOccurrences"], 3);
    }

    #[tokio::test]
    async fn personal_copy_update_preserves_recurrence_without_scheduling_guests() {
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

        GraphCalendarBackend
            .push_updated_invitation_copy(
                &CalendarBackendCtx {
                    db: &db,
                    services: &provider_services,
                },
                &account("calendar", "graph"),
                "remote-series",
                &event,
            )
            .await
            .unwrap();
        let requests = captured.await.unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(payload["subject"], "Changed");
        assert_eq!(payload["body"]["content"], "");
        assert_eq!(payload["location"]["displayName"], "");
        assert_eq!(payload["start"]["timeZone"], "Europe/Stockholm");
        assert_eq!(payload["recurrence"]["pattern"]["type"], "weekly");
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

        GraphCalendarBackend
            .push_updated_invitation_copy(
                &CalendarBackendCtx {
                    db: &db,
                    services: &provider_services,
                },
                &account("calendar", "graph"),
                "remote-event",
                &event,
            )
            .await
            .unwrap();
        let requests = captured.await.unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert!(payload["recurrence"].is_null());
        assert!(payload.get("attendees").is_none());
    }
}

#[cfg(test)]
mod recurrence_sync_tests {
    use super::{CalendarBackend, CalendarBackendCtx, GraphCalendarBackend};
    use crate::backend::calendar::google::sync_testutil::{
        cache_event, serve_responses, services, setup_db,
    };
    use crate::backend::testutil::account;
    use crate::calendar::RecurrenceKind;
    use crate::db;
    use serde_json::json;

    fn remote_event(id: &str, metadata: &serde_json::Value) -> serde_json::Value {
        let mut event = json!({
            "id": id,
            "iCalUId": format!("uid-{id}@example.test"),
            "subject": "Refreshed event",
            "start": {"dateTime": "2026-09-14T09:00:00", "timeZone": "UTC"},
            "end": {"dateTime": "2026-09-14T10:00:00", "timeZone": "UTC"}
        });
        event
            .as_object_mut()
            .unwrap()
            .extend(metadata.as_object().unwrap().clone());
        event
    }

    #[tokio::test]
    async fn only_successful_provider_refresh_invalidates_identical_series_proof() {
        for (response_status, reject_invalidation) in [(200, false), (500, false), (200, true)] {
            let (_dir, db) = setup_db().await;
            {
                let mut conn = db.writer().await;
                let transaction = conn.transaction().unwrap();
                cache_event(&transaction, "cached", None);
                let mut local = db::calendar::get_event(&transaction, "cached").unwrap();
                local.recurrence_kind = RecurrenceKind::Series;
                local.recurrence_rule = Some("FREQ=WEEKLY;COUNT=4;BYDAY=MO".into());
                local.timezone = Some("UTC".into());
                db::calendar::update_event(&transaction, &local).unwrap();
                db::calendar_invitation::record_local_series(&transaction, &local).unwrap();
                transaction
                    .execute(
                        "UPDATE calendar_events SET remote_id = 'remote' WHERE id = 'cached'",
                        [],
                    )
                    .unwrap();
                transaction.commit().unwrap();
                let attached = db::calendar::get_event(&conn, "cached").unwrap();
                assert!(db::calendar_invitation::validated_series_rule(&conn, &attached).is_ok());
                if reject_invalidation {
                    conn.execute_batch(
                        "CREATE TRIGGER reject_proof_invalidation
                         BEFORE DELETE ON calendar_invitation_recurrence
                         BEGIN SELECT RAISE(ABORT, 'injected invalidation failure'); END;",
                    )
                    .unwrap();
                }
            }
            let metadata = json!({
                "type": "seriesMaster", "seriesMasterId": null,
                "recurrence": {
                    "pattern": {"type": "weekly", "interval": 1, "daysOfWeek": ["monday"]},
                    "range": {"type": "numbered", "startDate": "2026-09-14", "numberOfOccurrences": 4}
                }
            });
            let (root, captured) = serve_responses(vec![
                (200, json!({"value": [{"id": "primary", "name": "Calendar", "isDefaultCalendar": true}]})),
                (response_status, json!({"value": [remote_event("remote", &metadata)]})),
            ]).await;
            let result = GraphCalendarBackend
                .sync(
                    &CalendarBackendCtx {
                        db: &db,
                        services: &services(&root),
                    },
                    &account("calendar", "graph"),
                )
                .await;
            assert_eq!(result.is_err(), reject_invalidation);
            assert_eq!(captured.await.unwrap().len(), 2);
            let conn = db.reader();
            let refreshed = db::calendar::get_event(&conn, "cached").unwrap();
            assert_eq!(refreshed.recurrence_kind, RecurrenceKind::Series);
            assert_eq!(
                refreshed.recurrence_rule.as_deref(),
                Some("FREQ=WEEKLY;COUNT=4;BYDAY=MO")
            );
            assert_eq!(
                db::calendar_invitation::validated_series_rule(&conn, &refreshed).is_ok(),
                response_status != 200 || reject_invalidation
            );
            if reject_invalidation {
                assert_eq!(refreshed.title, "Cached event");
            }
        }
    }

    #[tokio::test]
    async fn refresh_persists_recurrence_for_existing_and_new_rows() {
        for (metadata, expected) in [
            (
                json!({"type": "singleInstance", "seriesMasterId": null, "recurrence": null}),
                RecurrenceKind::Standalone,
            ),
            (
                json!({"type": "occurrence", "seriesMasterId": "master", "recurrence": null}),
                RecurrenceKind::Occurrence,
            ),
            (
                json!({"type": "exception", "seriesMasterId": "master", "recurrence": null}),
                RecurrenceKind::Occurrence,
            ),
            (
                json!({"seriesMasterId": null, "recurrence": null}),
                RecurrenceKind::Unknown,
            ),
            (
                json!({"type": "singleInstance", "seriesMasterId": "master", "recurrence": null}),
                RecurrenceKind::Unknown,
            ),
        ] {
            let (_dir, db) = setup_db().await;
            {
                let conn = db.writer().await;
                cache_event(&conn, "cached", Some("remote"));
                cache_event(&conn, "local-only", None);
                assert_eq!(
                    db::calendar::get_event(&conn, "cached")
                        .unwrap()
                        .recurrence_kind,
                    RecurrenceKind::Unknown
                );
            }
            let (root, captured) = serve_responses(vec![
                (200, json!({"value": [{"id": "primary", "name": "Calendar", "isDefaultCalendar": true}]})),
                (200, json!({"value": [remote_event("remote", &metadata), remote_event("new", &metadata)]})),
            ]).await;
            GraphCalendarBackend
                .sync(
                    &CalendarBackendCtx {
                        db: &db,
                        services: &services(&root),
                    },
                    &account("calendar", "graph"),
                )
                .await
                .unwrap();
            assert_eq!(captured.await.unwrap().len(), 2);
            let conn = db.reader();
            let cached = db::calendar::get_event(&conn, "cached").unwrap();
            assert_eq!(cached.id, "cached");
            assert_eq!(cached.title, "Refreshed event");
            assert_eq!(cached.recurrence_kind, expected);
            let new_kind: String = conn
                .query_row(
                    "SELECT recurrence_kind FROM calendar_events WHERE remote_id = 'new'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(new_kind, expected.as_str());
            assert_eq!(
                db::calendar::get_event(&conn, "local-only")
                    .unwrap()
                    .recurrence_kind,
                RecurrenceKind::Unknown
            );
        }
    }

    #[tokio::test]
    async fn failed_refresh_keeps_unknown_cached_events_unchanged() {
        let (_dir, db) = setup_db().await;
        let before = {
            let conn = db.writer().await;
            cache_event(&conn, "cached", Some("remote"));
            serde_json::to_value(db::calendar::get_event(&conn, "cached").unwrap()).unwrap()
        };
        let (root, captured) = serve_responses(vec![
            (200, json!({"value": [{"id": "primary", "name": "Calendar", "isDefaultCalendar": true}]})),
            (500, json!({"error": "injected read failure"})),
        ]).await;
        GraphCalendarBackend
            .sync(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "graph"),
            )
            .await
            .unwrap();
        assert_eq!(captured.await.unwrap().len(), 2);
        let after =
            serde_json::to_value(db::calendar::get_event(&db.reader(), "cached").unwrap()).unwrap();
        assert_eq!(after, before);
    }
}
