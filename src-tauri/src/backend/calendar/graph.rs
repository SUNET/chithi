//! Microsoft Graph calendar backend (O365 / Exchange Online).

use async_trait::async_trait;

use crate::calendar::recurrence_identity::{RecurrenceObjectKind, RecurrenceValueType};
use crate::calendar::{CalendarEvent, RecurrenceKind};
use crate::db;
use crate::db::accounts::AccountFull;
use crate::db::calendar::NewCalendar;
use crate::error::{Error, Result};
use crate::mail::graph::{
    event_patch_to_graph_json, event_to_graph_json, invitation_copy_patch_to_graph_json,
};
use crate::provider::GraphTokenPurpose;

use super::{
    BusyPeriod, CalendarBackend, CalendarBackendCtx, CalendarCapability, InviteReplyDelivery,
    ParticipantSchedule, ParticipantScheduleRequest, PushedEvent, RemoteOccurrenceUpdate,
    RemoteOccurrenceUpdateOutcome, RemoteRsvpOutcome, RemoteRsvpPolicy, RemoteRsvpRequest,
    RoomAvailability, RoomAvailabilityRequest, RoomSuggestion,
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

            let conn = db.writer().await;

            for ge in &calendar_events {
                let recurrence_rule = if ge
                    .recurrence_seeds
                    .as_ref()
                    .is_some_and(|seeds| seeds.is_empty())
                {
                    None
                } else {
                    // The bounded Graph view does not provide an RFC 5545 rule.
                    // Preserve existing trusted series evidence unless Graph has
                    // authoritatively classified the object as standalone.
                    conn.query_row(
                        "SELECT recurrence_rule FROM calendar_events
                         WHERE account_id = ?1 AND remote_id = ?2",
                        rusqlite::params![account_id, ge.id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .ok()
                    .flatten()
                };
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
                    recurrence_rule,
                    recurrence_kind: ge.recurrence_kind,
                    organizer_email: ge.organizer_email.clone(),
                    attendees_json: ge.attendees_json.clone(),
                    my_status: ge.my_status.clone(),
                    source_message_id: None,
                    ical_data: None,
                    remote_id: Some(ge.id.clone()),
                    etag: None,
                };
                match &ge.recurrence_seeds {
                    Some(seeds) => {
                        db::calendar::upsert_event_by_remote_id_with_recurrence(
                            &conn, &event, seeds,
                        )?;
                    }
                    None => db::calendar::upsert_event_by_remote_id(&conn, &event)?,
                }
            }

            // calendarView is bounded to the requested dates. Absence from
            // this response says nothing about events outside that window (or
            // deleted events); only a delta/tombstone feed can prove deletion.
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

    async fn update_recurrence_occurrence(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        request: &RemoteOccurrenceUpdate,
    ) -> Result<RemoteOccurrenceUpdateOutcome> {
        let (provider_calendar_id, etag, series_id, original_start) =
            validate_occurrence_update(account, request)?;
        let refreshed = ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?
            .update_recurrence_occurrence(
                &request.target_id,
                provider_calendar_id,
                etag,
                series_id,
                original_start,
                &request.patch,
                &request.desired,
            )
            .await?;
        let mut replacement = refreshed
            .recurrence_seeds
            .as_ref()
            .and_then(|seeds| seeds.first())
            .cloned()
            .ok_or_else(|| {
                Error::Sync(
                    "Graph occurrence update returned no replacement identity; reconciliation required"
                        .into(),
                )
            })?;
        replacement.provider_calendar_id = request.trusted_identity.provider_calendar_id.clone();
        replacement.local_series_event_id = request.trusted_identity.local_series_event_id.clone();
        replacement.recurrence_timezone = request.trusted_identity.recurrence_timezone.clone();
        replacement.validate()?;
        let replacement_etag = replacement.provider_revision.clone();

        let canonical = CalendarEvent {
            id: request.current_event.id.clone(),
            account_id: request.current_event.account_id.clone(),
            calendar_id: request.current_event.calendar_id.clone(),
            uid: refreshed.ical_uid,
            title: refreshed.subject,
            description: refreshed.body_preview,
            location: refreshed.location,
            start_time: refreshed.start,
            end_time: refreshed.end,
            all_day: refreshed.all_day,
            timezone: refreshed.timezone,
            recurrence_rule: request.current_event.recurrence_rule.clone(),
            recurrence_kind: RecurrenceKind::Occurrence,
            organizer_email: refreshed.organizer_email,
            attendees_json: refreshed.attendees_json,
            my_status: refreshed.my_status,
            source_message_id: request.current_event.source_message_id.clone(),
            ical_data: request.current_event.ical_data.clone(),
            remote_id: Some(request.target_id.clone()),
            etag: replacement_etag,
        };
        Ok(RemoteOccurrenceUpdateOutcome {
            occurrence: replacement.occurrence.clone(),
            replacement_identity: replacement,
            canonical_event: Some(canonical),
            canonical_recurrence_objects: None,
        })
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

fn validate_occurrence_update<'a>(
    account: &AccountFull,
    request: &'a RemoteOccurrenceUpdate,
) -> Result<(&'a str, &'a str, &'a str, &'a str)> {
    let identity = &request.trusted_identity;
    identity.validate()?;
    request.desired.validate()?;
    if !matches!(
        identity.kind,
        RecurrenceObjectKind::Occurrence | RecurrenceObjectKind::Exception
    ) || identity.recurrence_value_type != Some(RecurrenceValueType::DateTime)
        || request.current_event.recurrence_kind != RecurrenceKind::Occurrence
        || identity.account_id != account.id
        || identity.event_id != request.current_event.id
        || request.current_event.account_id != account.id
        || request.current_event.remote_id.as_deref() != Some(request.target_id.as_str())
        || identity.provider_occurrence_id.as_deref() != Some(request.target_id.as_str())
    {
        return Err(Error::Other(
            "Graph THIS-OCCURRENCE target is not a trusted immutable occurrence".into(),
        ));
    }
    let etag = request
        .expected_provider_revision
        .as_deref()
        .filter(|value| !value.trim().is_empty() && !value.chars().any(char::is_control))
        .ok_or(Error::UnsupportedCapability {
            protocol: "graph",
            capability: "THIS-OCCURRENCE update without @odata.etag",
        })?;
    if identity.provider_revision.as_deref() != Some(etag) {
        return Err(Error::Other(
            "Graph occurrence revision does not match its trusted identity".into(),
        ));
    }
    let provider_calendar_id = identity
        .provider_calendar_id
        .as_deref()
        .ok_or_else(|| Error::Other("Graph occurrence has no calendar identity".into()))?;
    let series_id = identity
        .provider_series_id
        .as_deref()
        .ok_or_else(|| Error::Other("Graph occurrence has no series identity".into()))?;
    let original_start = identity
        .recurrence_id
        .as_deref()
        .ok_or_else(|| Error::Other("Graph occurrence has no originalStart identity".into()))?;
    let native: serde_json::Value = serde_json::from_str(
        identity
            .provider_native_data
            .as_deref()
            .ok_or_else(|| Error::Other("Graph occurrence has no native identity data".into()))?,
    )
    .map_err(|error| Error::Other(format!("Invalid Graph occurrence identity data: {error}")))?;
    let expected_type = match identity.kind {
        RecurrenceObjectKind::Occurrence => "occurrence",
        RecurrenceObjectKind::Exception => "exception",
        _ => unreachable!(),
    };
    if native["id"].as_str() != Some(request.target_id.as_str())
        || native["type"].as_str() != Some(expected_type)
        || native["seriesMasterId"].as_str() != Some(series_id)
        || native["originalStart"].as_str() != Some(original_start)
        || native["@odata.etag"].as_str() != Some(etag)
    {
        return Err(Error::Other(
            "Graph occurrence identity does not match its native immutable identity".into(),
        ));
    }
    Ok((provider_calendar_id, etag, series_id, original_start))
}

#[cfg(test)]
mod occurrence_validation_tests {
    use super::validate_occurrence_update;
    use crate::backend::calendar::RemoteOccurrenceUpdate;
    use crate::backend::testutil::{account, event};
    use crate::calendar::recurrence_identity::{
        OccurrenceFields, RecurrenceIdentity, RecurrenceObjectKind, RecurrenceValueType,
        UpdateOccurrenceInput,
    };
    use crate::calendar::RecurrenceKind;

    fn request() -> RemoteOccurrenceUpdate {
        let fields = OccurrenceFields {
            title: "Occurrence".into(),
            description: Some("Description".into()),
            location: Some("Room".into()),
            start_time: "2026-09-15T10:00:00Z".into(),
            end_time: "2026-09-15T11:00:00Z".into(),
            all_day: false,
            timezone: Some("UTC".into()),
        };
        let mut current = event();
        current.remote_id = Some("immutable-occurrence".into());
        current.recurrence_kind = RecurrenceKind::Occurrence;
        RemoteOccurrenceUpdate {
            target_id: "immutable-occurrence".into(),
            expected_provider_revision: Some("etag-1".into()),
            trusted_identity: RecurrenceIdentity {
                object_id: "object".into(),
                account_id: "acc1".into(),
                event_id: current.id.clone(),
                local_series_event_id: None,
                provider_calendar_id: Some("provider-calendar".into()),
                provider_series_id: Some("immutable-master".into()),
                provider_occurrence_id: Some("immutable-occurrence".into()),
                recurrence_id: Some("2026-09-15T09:00:00.0000000Z".into()),
                recurrence_timezone: Some("UTC".into()),
                recurrence_value_type: Some(RecurrenceValueType::DateTime),
                occurrence: fields.clone(),
                provider_native_data: Some(
                    serde_json::json!({
                        "id": "immutable-occurrence",
                        "type": "occurrence",
                        "seriesMasterId": "immutable-master",
                        "originalStart": "2026-09-15T09:00:00.0000000Z",
                        "@odata.etag": "etag-1"
                    })
                    .to_string(),
                ),
                provider_revision: Some("etag-1".into()),
                kind: RecurrenceObjectKind::Occurrence,
            },
            current_event: current,
            patch: UpdateOccurrenceInput::default(),
            desired: fields,
        }
    }

    #[test]
    fn occurrence_write_requires_an_odata_etag() {
        let mut request = request();
        request.expected_provider_revision = None;
        request.trusted_identity.provider_revision = None;

        let error =
            validate_occurrence_update(&account("calendar", "graph"), &request).unwrap_err();

        assert!(matches!(
            error,
            crate::error::Error::UnsupportedCapability { .. }
        ));
    }

    #[test]
    fn occurrence_write_uses_the_provider_calendar_not_the_local_uuid() {
        let mut request = request();
        request.current_event.calendar_id = "local-calendar-uuid".into();

        let (provider_calendar_id, _, _, _) =
            validate_occurrence_update(&account("calendar", "graph"), &request).unwrap();

        assert_eq!(provider_calendar_id, "provider-calendar");
        assert_ne!(provider_calendar_id, request.current_event.calendar_id);
    }

    #[test]
    fn occurrence_write_rejects_any_native_identity_mismatch() {
        for field in [
            "id",
            "type",
            "seriesMasterId",
            "originalStart",
            "@odata.etag",
        ] {
            let mut request = request();
            let mut native: serde_json::Value = serde_json::from_str(
                request
                    .trusted_identity
                    .provider_native_data
                    .as_deref()
                    .unwrap(),
            )
            .unwrap();
            native[field] = serde_json::json!("different");
            request.trusted_identity.provider_native_data = Some(native.to_string());

            assert!(validate_occurrence_update(&account("calendar", "graph"), &request).is_err());
        }
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
    use crate::calendar::{
        recurrence_identity::{RecurrenceIdentitySeed, RecurrenceObjectKind, RecurrenceValueType},
        RecurrenceKind,
    };
    use crate::db;
    use serde_json::json;

    fn remote_event(id: &str, metadata: &serde_json::Value) -> serde_json::Value {
        let mut event = json!({
            "id": id,
            "iCalUId": format!("uid-{id}@example.test"),
            "@odata.etag": "revision-1",
            "changeKey": "native-change-key",
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
                json!({"type": "occurrence", "seriesMasterId": "master", "originalStart": "2026-09-14T09:00:00Z", "recurrence": null}),
                RecurrenceKind::Occurrence,
            ),
            (
                json!({"type": "exception", "seriesMasterId": "master", "originalStart": "2026-09-14T09:00:00Z", "recurrence": null}),
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
                (200, {
                    let mut new_metadata = metadata.clone();
                    if new_metadata.get("originalStart").is_some() {
                        new_metadata["originalStart"] = json!("2026-09-15T09:00:00Z");
                    }
                    json!({"value": [remote_event("remote", &metadata), remote_event("new", &new_metadata)]})
                }),
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

    #[tokio::test]
    async fn recurring_identity_is_stable_across_refreshes() {
        let (_dir, db) = setup_db().await;
        let metadata = json!({
            "type": "exception",
            "seriesMasterId": "immutable-master",
            "originalStart": "2026-09-14T09:00:00.0000000Z",
            "recurrence": null
        });
        let first = remote_event("immutable-exception", &metadata);
        let mut second = first.clone();
        second["@odata.etag"] = json!("revision-2");
        second["subject"] = json!("Second refresh");
        let calendar = json!({
            "value": [{"id": "primary", "name": "Calendar", "isDefaultCalendar": true}]
        });
        let (root, captured) = serve_responses(vec![
            (200, calendar.clone()),
            (200, json!({"value": [first]})),
            (200, calendar),
            (200, json!({"value": [second]})),
        ])
        .await;
        let provider_services = services(&root);
        let ctx = CalendarBackendCtx {
            db: &db,
            services: &provider_services,
        };
        let account = account("calendar", "graph");

        GraphCalendarBackend.sync(&ctx, &account).await.unwrap();
        let first_identity = {
            let conn = db.reader();
            let event = conn
                .query_row(
                    "SELECT id FROM calendar_events WHERE remote_id = 'immutable-exception'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap();
            db::calendar_recurrence::get_by_event_id(&conn, &event)
                .unwrap()
                .remove(0)
        };
        GraphCalendarBackend.sync(&ctx, &account).await.unwrap();

        assert_eq!(captured.await.unwrap().len(), 4);
        let conn = db.reader();
        let identities =
            db::calendar_recurrence::get_by_event_id(&conn, &first_identity.event_id).unwrap();
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0].object_id, first_identity.object_id);
        assert_eq!(
            identities[0].provider_revision.as_deref(),
            Some("revision-2")
        );
        assert_eq!(
            identities[0].recurrence_id.as_deref(),
            Some("2026-09-14T09:00:00.0000000Z")
        );
        let native: serde_json::Value =
            serde_json::from_str(identities[0].provider_native_data.as_deref().unwrap()).unwrap();
        assert_eq!(native["subject"], "Second refresh");
    }

    #[tokio::test]
    async fn same_series_id_in_two_provider_calendars_does_not_collide() {
        let (_dir, db) = setup_db().await;
        let metadata = json!({
            "type": "occurrence",
            "seriesMasterId": "shared-series-id",
            "originalStart": "2026-09-14T09:00:00Z",
            "recurrence": null
        });
        let calendars = json!({
            "value": [
                {"id": "remote-calendar-a", "name": "A", "isDefaultCalendar": true},
                {"id": "remote-calendar-b", "name": "B", "isDefaultCalendar": false}
            ]
        });
        let (root, captured) = serve_responses(vec![
            (200, calendars),
            (
                200,
                json!({"value": [remote_event("occurrence-a", &metadata)]}),
            ),
            (
                200,
                json!({"value": [remote_event("occurrence-b", &metadata)]}),
            ),
        ])
        .await;

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

        assert_eq!(captured.await.unwrap().len(), 3);
        let conn = db.reader();
        for provider_calendar_id in ["remote-calendar-a", "remote-calendar-b"] {
            let identities = db::calendar_recurrence::list_series_objects(
                &conn,
                "acc1",
                None,
                Some(provider_calendar_id),
                Some("shared-series-id"),
            )
            .unwrap();
            assert_eq!(identities.len(), 1);
            assert_eq!(
                identities[0].provider_calendar_id.as_deref(),
                Some(provider_calendar_id)
            );
            let local_calendar_id: String = conn
                .query_row(
                    "SELECT calendar_id FROM calendar_events WHERE id = ?1",
                    [&identities[0].event_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_ne!(local_calendar_id, provider_calendar_id);
        }
    }

    #[tokio::test]
    async fn bounded_calendar_view_absence_does_not_delete_local_events() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_event(&conn, "outside-window", Some("remote-outside-window"));
        }
        let (root, captured) = serve_responses(vec![
            (200, json!({"value": [{"id": "primary", "name": "Calendar", "isDefaultCalendar": true}]})),
            (200, json!({"value": []})),
        ])
        .await;

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
        assert!(db::calendar::get_event(&db.reader(), "outside-window").is_ok());
    }

    #[tokio::test]
    async fn malformed_metadata_preserves_identity_until_proven_standalone() {
        let (_dir, db) = setup_db().await;
        let event_id = {
            let conn = db.writer().await;
            cache_event(&conn, "cached", Some("immutable-event"));
            let event = db::calendar::get_event(&conn, "cached").unwrap();
            let seed = RecurrenceIdentitySeed {
                local_series_event_id: None,
                provider_calendar_id: Some("primary".into()),
                provider_series_id: Some("immutable-master".into()),
                provider_occurrence_id: Some("immutable-event".into()),
                recurrence_id: Some("2026-09-14T09:00:00Z".into()),
                recurrence_timezone: Some("UTC".into()),
                recurrence_value_type: Some(RecurrenceValueType::DateTime),
                occurrence: crate::calendar::recurrence_identity::OccurrenceFields {
                    title: event.title.clone(),
                    description: event.description.clone(),
                    location: event.location.clone(),
                    start_time: event.start_time.clone(),
                    end_time: event.end_time.clone(),
                    all_day: event.all_day,
                    timezone: event.timezone.clone(),
                },
                provider_native_data: Some("{\"trusted\":true}".into()),
                provider_revision: Some("trusted-revision".into()),
                kind: RecurrenceObjectKind::Occurrence,
            };
            db::calendar::upsert_event_by_remote_id_with_recurrence(&conn, &event, &[seed]).unwrap()
        };
        let malformed = json!({
            "type": "exception",
            "seriesMasterId": "immutable-master",
            "recurrence": null
        });
        let standalone = json!({
            "type": "singleInstance",
            "seriesMasterId": null,
            "recurrence": null
        });
        let calendar = json!({
            "value": [{"id": "primary", "name": "Calendar", "isDefaultCalendar": true}]
        });
        let (root, captured) = serve_responses(vec![
            (200, calendar.clone()),
            (
                200,
                json!({"value": [remote_event("immutable-event", &malformed)]}),
            ),
            (200, calendar),
            (
                200,
                json!({"value": [remote_event("immutable-event", &standalone)]}),
            ),
        ])
        .await;
        let provider_services = services(&root);
        let ctx = CalendarBackendCtx {
            db: &db,
            services: &provider_services,
        };
        let account = account("calendar", "graph");

        GraphCalendarBackend.sync(&ctx, &account).await.unwrap();
        let preserved = db::calendar_recurrence::get_by_event_id(&db.reader(), &event_id).unwrap();
        assert_eq!(preserved.len(), 1);
        assert_eq!(
            preserved[0].provider_revision.as_deref(),
            Some("trusted-revision")
        );

        GraphCalendarBackend.sync(&ctx, &account).await.unwrap();
        assert_eq!(captured.await.unwrap().len(), 4);
        assert!(
            db::calendar_recurrence::get_by_event_id(&db.reader(), &event_id)
                .unwrap()
                .is_empty()
        );
    }
}
