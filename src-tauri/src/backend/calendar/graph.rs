//! Microsoft Graph calendar backend (O365 / Exchange Online).

use async_trait::async_trait;
use rusqlite::OptionalExtension;

use crate::calendar::event_set::CalendarEventSet;
use crate::calendar::recurrence_identity::{RecurrenceObjectKind, RecurrenceValueType};
use crate::calendar::{CalendarEvent, RecurrenceKind};
use crate::db;
use crate::db::accounts::AccountFull;
use crate::db::calendar::NewCalendar;
use crate::error::{Error, Result};
use crate::mail::graph::{
    event_patch_to_graph_json, event_to_graph_json, invitation_copy_patch_to_graph_json,
    GraphCalendar, GraphCalendarItem,
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
        super::EventCreationTarget::SelectedCalendar
    }

    async fn fetch_event_set(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        event: &CalendarEvent,
        remote_calendar_id: &str,
    ) -> Result<CalendarEventSet> {
        if event.account_id != account.id {
            return Err(Error::Other(
                "Graph event belongs to another account".into(),
            ));
        }
        let id = event
            .remote_id
            .as_deref()
            .ok_or_else(|| Error::Other("Graph event has no remote identity".into()))?;
        ctx.services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?
            .fetch_calendar_event_set(remote_calendar_id, id, event)
            .await
    }

    async fn update_event_set(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        before: &CalendarEventSet,
        desired: &CalendarEventSet,
    ) -> Result<CalendarEventSet> {
        ctx.services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?
            .update_calendar_event_set(&account.id, before, desired)
            .await
    }

    async fn create_event_set(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_calendar_id: &str,
        desired: &CalendarEventSet,
        operation_id: &str,
    ) -> Result<CalendarEventSet> {
        ctx.services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?
            .create_calendar_event_set(&account.id, remote_calendar_id, desired, operation_id)
            .await
    }

    async fn delete_event_set(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        before: &CalendarEventSet,
    ) -> Result<()> {
        ctx.services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?
            .delete_calendar_event_set(&account.id, before)
            .await
    }

    async fn move_event_set_native(
        &self,
        _ctx: &CalendarBackendCtx<'_>,
        _account: &AccountFull,
        _before: &CalendarEventSet,
        _remote_calendar_id: &str,
    ) -> Result<CalendarCapability<CalendarEventSet>> {
        // The Graph v1.0 event resource has no move action.
        Ok(CalendarCapability::Unsupported)
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
                    .optional()?;

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
                        )?;
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
                        )?;
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
        let mut failures = Vec::new();
        let mut complete_views = Vec::new();

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
                    failures.push(format!("{}: {e}", gc.name));
                    continue;
                }
            };
            log::info!(
                "sync_calendars_graph: fetched {} events for calendar '{}'",
                calendar_events.len(),
                gc.name
            );
            let observed_remote_ids = calendar_events
                .iter()
                .map(|item| match item {
                    GraphCalendarItem::Live(event) => event.id.clone(),
                    GraphCalendarItem::Cancelled(tombstone) => tombstone.remote_id().to_owned(),
                })
                .collect();

            let mut conn = db.writer().await;
            if let Err(error) = reconcile_calendar_events(
                &mut conn,
                account_id,
                local_cal_id,
                &gc.id,
                calendar_events,
            ) {
                log::error!(
                    "sync_calendars_graph: reconciliation for '{}' failed: {}",
                    gc.name,
                    error
                );
                failures.push(format!("{}: {error}", gc.name));
            } else {
                complete_views.push((local_cal_id.clone(), observed_remote_ids));
            }
        }
        if !failures.is_empty() {
            return Err(Error::Sync(format!(
                "Graph calendar sync failed for {} calendar(s): {}",
                failures.len(),
                failures.join("; ")
            )));
        }

        {
            let mut conn = db.writer().await;
            let transaction = conn.transaction()?;
            for (local_calendar_id, observed_remote_ids) in &complete_views {
                retire_absent_graph_view_rows(
                    &transaction,
                    account_id,
                    local_calendar_id,
                    &start,
                    &end,
                    observed_remote_ids,
                )?;
            }
            transaction.commit()?;
        }

        let authoritative_calendars = graph_calendars
            .iter()
            .map(|calendar| {
                remote_to_local
                    .get(&calendar.id)
                    .map(|(local_id, _)| (calendar, local_id.as_str()))
                    .ok_or_else(|| {
                        Error::Sync(format!(
                            "Graph calendar {:?} has no local mapping",
                            calendar.id
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let retired = {
            let mut conn = db.writer().await;
            retire_stale_graph_calendars(&mut conn, account_id, &authoritative_calendars)?
        };
        for (name, remote_id, migrated_events, deleted_events) in retired {
            log::info!(
                "sync_calendars_graph: retired stale calendar '{}' ({}) after migrating {} local events and deleting {} cached events",
                name,
                remote_id,
                migrated_events,
                deleted_events
            );
        }

        log::info!("sync_calendars_graph: completed for account {}", account_id);
        Ok(())
    }

    fn validate_event_creation(&self, event: &CalendarEvent, calendar: &str) -> Result<()> {
        crate::mail::graph::event_set::collection_path(calendar)?;
        event_to_graph_json(event).map(|_| ())
    }

    /// Graph sends invitations itself, in the exact selected calendar.
    async fn push_created_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        event: &CalendarEvent,
        remote_calendar_id: &str,
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
        let (remote_id, ical_uid) = client
            .create_event(remote_calendar_id, &graph_event)
            .await?;
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

/// The client has validated every page and rejected conflicting duplicate IDs
/// before this function acquires a transaction for the complete calendar batch.
fn reconcile_calendar_events(
    conn: &mut rusqlite::Connection,
    account_id: &str,
    local_calendar_id: &str,
    provider_calendar_id: &str,
    items: Vec<GraphCalendarItem>,
) -> Result<()> {
    let mut live = Vec::new();
    let mut cancelled = Vec::new();
    for item in items {
        match item {
            GraphCalendarItem::Live(event) => live.push(event),
            GraphCalendarItem::Cancelled(tombstone) => cancelled.push(tombstone),
        }
    }

    let transaction = conn.transaction()?;
    for tombstone in cancelled {
        reconcile_graph_event_identity(
            &transaction,
            account_id,
            local_calendar_id,
            provider_calendar_id,
            tombstone.remote_id(),
            None,
            None,
            None,
            None,
        )?;
        db::calendar_event_deletion::delete_calendar_events_by_remote_id(
            &transaction,
            account_id,
            local_calendar_id,
            tombstone.remote_id(),
        )?;
    }
    for ge in live {
        let recurrence_seeds = ge.recurrence_seeds.as_deref().unwrap_or(&[]);
        if let Some(owner_id) = db::calendar_actions::ingest_owned_identity(
            &transaction,
            account_id,
            local_calendar_id,
            Some(&ge.id),
            recurrence_seeds,
        )? {
            retire_action_owned_graph_cache_rows(
                &transaction,
                account_id,
                local_calendar_id,
                &owner_id,
                &ge.id,
                ge.ical_uid.as_deref(),
                Some(&ge.start),
                Some(&ge.end),
                Some(ge.recurrence_kind),
            )?;
            continue;
        }
        reconcile_graph_event_identity(
            &transaction,
            account_id,
            local_calendar_id,
            provider_calendar_id,
            &ge.id,
            ge.ical_uid.as_deref(),
            Some(&ge.start),
            Some(&ge.end),
            Some(ge.recurrence_kind),
        )?;
        let recurrence_rule = if ge
            .recurrence_seeds
            .as_ref()
            .is_some_and(|seeds| seeds.is_empty())
        {
            None
        } else {
            // The bounded view supplies no RFC 5545 rule. Preserve trusted series
            // evidence until Graph authoritatively classifies an object as standalone.
            transaction
                .query_row(
                    "SELECT recurrence_rule FROM calendar_events
                     WHERE account_id = ?1 AND calendar_id = ?2 AND remote_id = ?3",
                    rusqlite::params![account_id, local_calendar_id, ge.id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten()
        };
        let event = CalendarEvent {
            id: uuid::Uuid::new_v4().to_string(),
            account_id: account_id.to_owned(),
            calendar_id: local_calendar_id.to_owned(),
            uid: ge.ical_uid,
            title: ge.subject,
            description: ge.body_preview,
            location: ge.location,
            start_time: ge.start,
            end_time: ge.end,
            all_day: ge.all_day,
            timezone: ge.timezone,
            recurrence_rule,
            recurrence_kind: ge.recurrence_kind,
            organizer_email: ge.organizer_email,
            attendees_json: ge.attendees_json,
            my_status: ge.my_status,
            source_message_id: None,
            ical_data: None,
            remote_id: Some(ge.id),
            etag: None,
        };
        match &ge.recurrence_seeds {
            Some(seeds) => {
                db::calendar::upsert_event_by_remote_id_with_recurrence_in_transaction(
                    &transaction,
                    &event,
                    seeds,
                )?;
            }
            None => {
                db::calendar::upsert_event_by_remote_id_in_transaction(&transaction, &event)?;
            }
        }
    }

    transaction.commit()?;
    Ok(())
}

/// A complete calendarView is authoritative only for concrete instances whose
/// persisted intervals overlap its bounds. Series masters, unknown legacy
/// classifications, local-only rows, and events outside the view are retained.
fn retire_absent_graph_view_rows(
    conn: &rusqlite::Connection,
    account_id: &str,
    local_calendar_id: &str,
    view_start: &str,
    view_end: &str,
    observed_remote_ids: &std::collections::HashSet<String>,
) -> Result<()> {
    let candidates = {
        let mut statement = conn.prepare(
            "SELECT id, remote_id FROM calendar_events
             WHERE account_id = ?1 AND calendar_id = ?2
               AND remote_id IS NOT NULL AND trim(remote_id) != ''
               AND recurrence_kind IN ('standalone', 'occurrence')
               AND julianday(start_time) < julianday(?4)
               AND julianday(end_time) > julianday(?3)
             ORDER BY id",
        )?;
        let rows = statement
            .query_map(
                rusqlite::params![account_id, local_calendar_id, view_start, view_end],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };
    for (event_id, remote_id) in candidates {
        if observed_remote_ids.contains(&remote_id) {
            continue;
        }
        ensure_disposable_graph_duplicate(conn, &event_id)?;
        db::calendar_event_deletion::delete_event(conn, &event_id)?;
        log::info!(
            "Graph sync retired event {} absent from complete bounded view",
            event_id
        );
    }
    Ok(())
}

/// Retire provider-backed calendar caches absent from Graph's complete
/// inventory. Unpushed local events are moved only when name and default status
/// identify exactly one current destination; provider identities are never
/// inferred from display metadata.
fn retire_stale_graph_calendars<'a>(
    conn: &mut rusqlite::Connection,
    account_id: &str,
    authoritative_calendars: &[(&'a GraphCalendar, &'a str)],
) -> Result<Vec<(String, String, usize, usize)>> {
    let authoritative_ids = authoritative_calendars
        .iter()
        .map(|(calendar, _)| calendar.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let stale = {
        let mut statement = conn.prepare(
            "SELECT id, name, is_default, remote_id FROM calendars
             WHERE account_id = ?1 AND remote_id IS NOT NULL AND remote_id != ''
             ORDER BY id",
        )?;
        let rows = statement
            .query_map([account_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|(_, _, _, remote_id)| !authoritative_ids.contains(remote_id.as_str()))
            .collect::<Vec<_>>();
        rows
    };
    if stale.is_empty() {
        return Ok(Vec::new());
    }

    let transaction = conn.transaction()?;
    let mut retired = Vec::with_capacity(stale.len());
    for (calendar_id, name, is_default, remote_id) in stale {
        let owns_calendar_action_state: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM calendar_action_addresses
                 WHERE account_id = ?1 AND calendar_id = ?2
             )",
            rusqlite::params![account_id, calendar_id],
            |row| row.get(0),
        )?;
        if owns_calendar_action_state {
            return Err(Error::Sync(format!(
                "Stale Graph calendar {calendar_id:?} has calendar action state and cannot be retired automatically"
            )));
        }

        let unpushed_event_ids = {
            let mut statement = transaction.prepare(
                "SELECT id FROM calendar_events
                 WHERE account_id = ?1 AND calendar_id = ?2
                   AND (remote_id IS NULL OR trim(remote_id) = '')
                 ORDER BY id",
            )?;
            let rows = statement
                .query_map(rusqlite::params![account_id, calendar_id], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let migrated_events = unpushed_event_ids.len();
        if !unpushed_event_ids.is_empty() {
            let destinations = authoritative_calendars
                .iter()
                .filter(|(calendar, _)| {
                    calendar.name.as_str() == name.as_str() && calendar.is_default == is_default
                })
                .map(|(_, local_id)| *local_id)
                .collect::<Vec<_>>();
            let [destination_calendar_id] = destinations.as_slice() else {
                return Err(Error::Sync(format!(
                    "Stale Graph calendar {calendar_id:?} has {} local events but {} exact current calendar matches",
                    unpushed_event_ids.len(),
                    destinations.len()
                )));
            };
            for event_id in &unpushed_event_ids {
                ensure_graph_event_can_move(&transaction, event_id)?;
                let updated = transaction.execute(
                    "UPDATE calendar_events SET calendar_id = ?1
                     WHERE id = ?2 AND account_id = ?3 AND calendar_id = ?4",
                    rusqlite::params![destination_calendar_id, event_id, account_id, calendar_id],
                )?;
                if updated != 1 {
                    return Err(Error::Sync(format!(
                        "Local event {event_id:?} changed during stale Graph calendar migration"
                    )));
                }
            }
        }

        let event_ids = {
            let mut statement = transaction
                .prepare("SELECT id FROM calendar_events WHERE calendar_id = ?1 ORDER BY id")?;
            let rows = statement
                .query_map([&calendar_id], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        for event_id in &event_ids {
            ensure_disposable_graph_duplicate(&transaction, event_id)?;
        }
        let deleted =
            db::calendar_event_deletion::delete_calendar_events(&transaction, &calendar_id)?;
        db::calendar::delete_calendar_row(&transaction, &calendar_id)?;
        retired.push((name, remote_id, migrated_events, deleted.deleted));
    }
    transaction.commit()?;
    Ok(retired)
}

/// Older Chithi versions assigned account-wide calendarView results to the
/// default calendar and persisted a different Graph ID format. Prefer the
/// current immutable ID, then use Graph's cross-calendar iCalUId together with
/// the exact occurrence interval and kind to identify that historical row.
/// Existing duplicates are retired only when they have no local ownership
/// state that would be unsafe to discard.
fn reconcile_graph_event_identity(
    conn: &rusqlite::Connection,
    account_id: &str,
    local_calendar_id: &str,
    provider_calendar_id: &str,
    remote_id: &str,
    ical_uid: Option<&str>,
    start: Option<&str>,
    end: Option<&str>,
    recurrence_kind: Option<RecurrenceKind>,
) -> Result<()> {
    let rows = graph_event_identity_rows(
        conn,
        account_id,
        local_calendar_id,
        remote_id,
        ical_uid,
        start,
        end,
        recurrence_kind,
    )?;
    let Some((survivor_id, survivor_calendar_id, survivor_remote_id)) = rows.first() else {
        return Ok(());
    };

    for (duplicate_id, _, _) in rows.iter().skip(1) {
        ensure_disposable_graph_duplicate(conn, duplicate_id)?;
        db::calendar_event_deletion::delete_event(conn, duplicate_id)?;
        log::info!(
            "Graph sync reconciled stale duplicate {} for immutable event {}",
            duplicate_id,
            remote_id
        );
    }

    if survivor_calendar_id != local_calendar_id || survivor_remote_id != remote_id {
        ensure_graph_event_can_move(conn, survivor_id)?;
        conn.execute(
            "UPDATE calendar_events SET calendar_id = ?1, remote_id = ?2 WHERE id = ?3",
            rusqlite::params![local_calendar_id, remote_id, survivor_id],
        )?;
        conn.execute(
            "UPDATE calendar_recurrence_objects
             SET provider_calendar_id = ?1,
                 provider_occurrence_id = CASE
                     WHEN provider_occurrence_id = ?2 THEN ?3
                     ELSE provider_occurrence_id
                 END,
                 provider_series_id = CASE
                     WHEN provider_series_id = ?2 THEN ?3
                     ELSE provider_series_id
                 END
             WHERE event_id = ?4",
            rusqlite::params![
                provider_calendar_id,
                survivor_remote_id,
                remote_id,
                survivor_id
            ],
        )?;
        log::info!(
            "Graph sync reconciled event {} to its authoritative ID and calendar",
            remote_id
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn graph_event_identity_rows(
    conn: &rusqlite::Connection,
    account_id: &str,
    local_calendar_id: &str,
    remote_id: &str,
    ical_uid: Option<&str>,
    start: Option<&str>,
    end: Option<&str>,
    recurrence_kind: Option<RecurrenceKind>,
) -> Result<Vec<(String, String, String)>> {
    let ical_uid = ical_uid.filter(|uid| !uid.is_empty());
    let mut stmt = conn.prepare(
        "SELECT id, calendar_id, remote_id FROM calendar_events
         WHERE account_id = ?1 AND (
             remote_id = ?2 OR (
                 ?4 IS NOT NULL AND uid = ?4 AND start_time = ?5 AND end_time = ?6
                 AND recurrence_kind = ?7
                 AND remote_id IS NOT NULL AND remote_id != ''
             )
         )
         ORDER BY CASE WHEN remote_id = ?2 THEN 0 ELSE 1 END,
                  CASE WHEN calendar_id = ?3 THEN 0 ELSE 1 END,
                  updated_at DESC, id",
    )?;
    let rows = stmt
        .query_map(
            rusqlite::params![
                account_id,
                remote_id,
                local_calendar_id,
                ical_uid,
                start,
                end,
                recurrence_kind.map(RecurrenceKind::as_str),
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
fn retire_action_owned_graph_cache_rows(
    conn: &rusqlite::Connection,
    account_id: &str,
    local_calendar_id: &str,
    owner_id: &str,
    remote_id: &str,
    ical_uid: Option<&str>,
    start: Option<&str>,
    end: Option<&str>,
    recurrence_kind: Option<RecurrenceKind>,
) -> Result<()> {
    let rows = graph_event_identity_rows(
        conn,
        account_id,
        local_calendar_id,
        remote_id,
        ical_uid,
        start,
        end,
        recurrence_kind,
    )?;
    for (event_id, _, _) in rows {
        if db::calendar_actions::owner_id(conn, &event_id)? == owner_id {
            continue;
        }
        ensure_disposable_graph_duplicate(conn, &event_id)?;
        db::calendar_event_deletion::delete_event(conn, &event_id)?;
        log::info!(
            "Graph sync retired action-owned cache duplicate {} for event {}",
            event_id,
            remote_id
        );
    }
    Ok(())
}

fn ensure_graph_event_can_move(conn: &rusqlite::Connection, event_id: &str) -> Result<()> {
    let owns_action_state: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM calendar_action_sets WHERE event_id = ?1
             UNION ALL SELECT 1 FROM calendar_action_members
                 WHERE event_id = ?1 OR owner_event_id = ?1
             UNION ALL SELECT 1 FROM calendar_action_addresses WHERE owner_event_id = ?1
             UNION ALL SELECT 1 FROM calendar_action_claims WHERE event_id = ?1
             UNION ALL SELECT 1 FROM calendar_action_operations
                 WHERE event_id = ?1 AND completed = 0
             UNION ALL SELECT 1 FROM calendar_action_creations
                 WHERE event_id = ?1 AND completed = 0
         )",
        [event_id],
        |row| row.get(0),
    )?;
    if owns_action_state {
        return Err(Error::Sync(format!(
            "Graph event {event_id:?} changed calendars while calendar action state owns it"
        )));
    }
    Ok(())
}

fn ensure_disposable_graph_duplicate(conn: &rusqlite::Connection, event_id: &str) -> Result<()> {
    ensure_graph_event_can_move(conn, event_id)?;
    let owns_local_state: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM calendar_invitation_sources WHERE event_id = ?1
             UNION ALL SELECT 1 FROM meet_meetings WHERE event_id = ?1
             UNION ALL SELECT 1 FROM calendar_action_operations
                 WHERE event_id = ?1 AND completed = 0
             UNION ALL SELECT 1 FROM calendar_action_creations
                 WHERE event_id = ?1 AND completed = 0
             UNION ALL SELECT 1 FROM calendar_events
                  WHERE id = ?1 AND (
                      remote_id IS NULL
                      OR trim(remote_id) = ''
                      OR pending_rsvp_status IS NOT NULL
                      OR manually_managed_at IS NOT NULL
                      OR source_message_id IS NOT NULL
                      OR ical_data IS NOT NULL
                 )
         )",
        [event_id],
        |row| row.get(0),
    )?;
    if owns_local_state {
        return Err(Error::Sync(format!(
            "Graph event {event_id:?} has local state and cannot be retired automatically"
        )));
    }
    Ok(())
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
    use crate::backend::calendar::google::sync_testutil::{
        serve_create_response, serve_patch_response, services, setup_db,
    };
    use crate::backend::calendar::{CalendarBackend, CalendarBackendCtx};
    use crate::backend::testutil::{account, event};
    use crate::calendar::RecurrenceKind;

    #[tokio::test]
    async fn publishes_confirmed_standalone_creation() {
        for rule in [None, Some("")] {
            let (_directory, db) = setup_db().await;
            let (root, captured) = serve_create_response(
                serde_json::json!({"id": "created-event", "iCalUId": "canonical@example.test"}),
            )
            .await;
            let services = services(&root);
            let mut event = event();
            event.start_time = "2026-09-14T09:00:00Z".into();
            event.end_time = "2026-09-14T10:00:00Z".into();
            event.recurrence_rule = rule.map(str::to_owned);
            let created = GraphCalendarBackend
                .push_created_event(
                    &CalendarBackendCtx {
                        db: &db,
                        services: &services,
                    },
                    &account("calendar", "graph"),
                    &event,
                    "primary",
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(created.remote_id, "created-event");
            assert_eq!(
                created.canonical_uid.as_deref(),
                Some("canonical@example.test")
            );
            let requests = captured.await.unwrap();
            assert!(requests[0]
                .starts_with("POST /calendar-api/me/calendars/primary/events HTTP/1.1\r\n"));
            let payload: serde_json::Value =
                serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
            assert!(payload.get("recurrence").is_none());
            assert_eq!(payload["subject"], event.title);
        }
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
            "isCancelled": false,
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

    fn cache_bound_occurrence(conn: &rusqlite::Connection, id: &str) {
        cache_event(conn, id, Some(id));
        let event = db::calendar::get_event(conn, id).unwrap();
        let seed = RecurrenceIdentitySeed {
            local_series_event_id: None,
            provider_calendar_id: Some("primary".into()),
            provider_series_id: Some(format!("master-{id}")),
            provider_occurrence_id: Some(id.into()),
            recurrence_id: Some(event.start_time.clone()),
            recurrence_timezone: Some("UTC".into()),
            recurrence_value_type: Some(RecurrenceValueType::DateTime),
            occurrence: crate::calendar::recurrence_identity::OccurrenceFields {
                title: event.title,
                description: event.description,
                location: event.location,
                start_time: event.start_time,
                end_time: event.end_time,
                all_day: event.all_day,
                timezone: Some("UTC".into()),
            },
            provider_native_data: Some(json!({"id": id, "isCancelled": false}).to_string()),
            provider_revision: Some("cached-revision".into()),
            kind: RecurrenceObjectKind::Occurrence,
        };
        db::calendar_recurrence::upsert(
            conn,
            &seed.bind("acc1", id, format!("identity-{id}")).unwrap(),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meet_meetings (event_id, account_id, protocol, meeting_id, join_url)
             VALUES (?1, 'acc1', 'zoom', ?1, 'https://example.test/join')",
            [id],
        )
        .unwrap();
    }

    fn primary_calendar() -> serde_json::Value {
        json!({"value": [{"id": "primary", "name": "Calendar", "isDefaultCalendar": true}]})
    }

    fn pending_cleanup_count(conn: &rusqlite::Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM meet_pending_meetings", [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    fn cache_action_owned_occurrence(conn: &rusqlite::Connection, protected: bool) {
        cache_event(conn, "series-owner", Some("immutable-master"));
        cache_event(conn, "projected-member", None);
        cache_event(conn, "stale-provider-cache", Some("legacy-occurrence"));
        conn.execute(
            "UPDATE calendar_events
             SET uid = 'uid-immutable-occurrence@example.test',
                 recurrence_kind = 'occurrence'
             WHERE id IN ('projected-member', 'stale-provider-cache')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE calendar_events
             SET uid = 'series@example.test', recurrence_kind = 'series',
                 recurrence_rule = 'FREQ=DAILY'
             WHERE id = 'series-owner'",
            [],
        )
        .unwrap();
        let revision = db::calendar_revision::get(conn, "series-owner").unwrap();
        conn.execute(
            "INSERT INTO calendar_action_sets(event_id, data, revision, dirty)
             VALUES ('series-owner', '{}', ?1, 0)",
            [revision],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendar_action_members
                 (event_id, owner_event_id, original_start)
             VALUES (
                 'projected-member', 'series-owner',
                 '2026-09-14T09:00:00Z'
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendar_action_addresses
                 (account_id, calendar_id, remote_id, owner_event_id, retired)
             VALUES (
                 'acc1', 'cal1', 'immutable-occurrence', 'series-owner', 0
             )",
            [],
        )
        .unwrap();
        if protected {
            conn.execute(
                "INSERT INTO meet_meetings
                     (event_id, account_id, protocol, meeting_id, join_url)
                 VALUES (
                     'stale-provider-cache', 'acc1', 'zoom', 'protected',
                     'https://example.test/join'
                 )",
                [],
            )
            .unwrap();
        }
    }

    fn occurrence_metadata() -> serde_json::Value {
        json!({
            "type": "occurrence",
            "seriesMasterId": "immutable-master",
            "originalStart": "2026-09-14T09:00:00Z",
            "recurrence": null
        })
    }

    #[tokio::test]
    async fn complete_paginated_batch_reconciles_cancellations_and_live_events_together() {
        for (status, last_page, succeeds) in [
            (200, json!({"value": []}), true),
            (500, json!({"error": "injected page failure"}), false),
            (200, json!({"value": {}}), false),
            (
                200,
                json!({"value": [{"id": "invalid", "isCancelled": null}]}),
                false,
            ),
            (
                200,
                json!({"value": [{"id": "", "isCancelled": true}]}),
                false,
            ),
            (
                200,
                json!({"value": [remote_event("cancelled", &json!({}))]}),
                false,
            ),
        ] {
            let (_dir, db) = setup_db().await;
            let before = {
                let conn = db.writer().await;
                cache_bound_occurrence(&conn, "cancelled");
                cache_event(&conn, "live", Some("live"));
                cache_event(&conn, "outside-window", Some("outside-window"));
                cache_event(&conn, "unpushed", None);
                conn.execute(
                    "UPDATE calendar_events SET uid = 'cancelled-uid@example.test'
                     WHERE id IN ('cancelled', 'unpushed', 'outside-window')",
                    [],
                )
                .unwrap();
                serde_json::to_value(
                    db::calendar_recurrence::get_by_event_id(&conn, "cancelled").unwrap(),
                )
                .unwrap()
            };
            let (root, captured) = crate::mail::graph::event_set::tests::serve_json(|root| vec![
                (200, primary_calendar()),
                (200, json!({
                    "value": [
                        {"id": "cancelled", "isCancelled": true, "iCalUId": "cancelled-uid@example.test"},
                        remote_event("live", &json!({"type": "singleInstance", "seriesMasterId": null, "recurrence": null}))
                    ],
                    "@odata.nextLink": format!("{root}/me/calendars/primary/calendarView?$skip=1")
                })),
                (status, last_page),
            ])
            .await;
            let result = GraphCalendarBackend
                .sync(
                    &CalendarBackendCtx {
                        db: &db,
                        services: &services(&root),
                    },
                    &account("calendar", "graph"),
                )
                .await;

            assert_eq!(result.is_ok(), succeeds, "{result:?}");
            if let Err(error) = result {
                assert!(matches!(error, crate::error::Error::Sync(_)));
                assert!(error.to_string().contains("Calendar"));
            }
            assert_eq!(captured.await.unwrap().len(), 3);
            let conn = db.reader();
            assert_eq!(
                db::calendar::get_event(&conn, "cancelled").is_err(),
                succeeds
            );
            assert_eq!(pending_cleanup_count(&conn), i64::from(succeeds));
            let identities = db::calendar_recurrence::get_by_event_id(&conn, "cancelled").unwrap();
            if succeeds {
                assert!(identities.is_empty());
                assert_eq!(
                    db::meet_pending_meetings::list_cleanup_requested(&conn).unwrap()[0].meeting_id,
                    "cancelled"
                );
            } else {
                assert_eq!(serde_json::to_value(identities).unwrap(), before);
                assert!(conn
                    .query_row(
                        "SELECT 1 FROM meet_meetings WHERE event_id = 'cancelled'",
                        [],
                        |_| Ok(())
                    )
                    .is_ok());
            }
            assert_eq!(
                db::calendar::get_event(&conn, "live").unwrap().title,
                if succeeds {
                    "Refreshed event"
                } else {
                    "Cached event"
                }
            );
            assert!(db::calendar::get_event(&conn, "outside-window").is_ok());
            assert!(db::calendar::get_event(&conn, "unpushed").is_ok());
        }
    }

    #[tokio::test]
    async fn reconciliation_failure_rolls_back_deletions_meetings_recurrence_and_live_writes() {
        for trigger in [
            "CREATE TRIGGER injected_failure BEFORE INSERT ON meet_pending_meetings
             WHEN NEW.meeting_id = 'second'
             BEGIN SELECT RAISE(ABORT, 'injected queue failure'); END;",
            "CREATE TRIGGER injected_failure BEFORE DELETE ON calendar_events
             WHEN OLD.id = 'second'
             BEGIN SELECT RAISE(ABORT, 'injected deletion failure'); END;",
            "CREATE TRIGGER injected_failure BEFORE UPDATE ON calendar_events
             WHEN OLD.id = 'live-second'
             BEGIN SELECT RAISE(ABORT, 'injected live write failure'); END;",
        ] {
            let (_dir, db) = setup_db().await;
            let before = {
                let conn = db.writer().await;
                cache_bound_occurrence(&conn, "first");
                cache_bound_occurrence(&conn, "second");
                cache_event(&conn, "live-first", Some("live-first"));
                cache_event(&conn, "live-second", Some("live-second"));
                conn.execute_batch(trigger).unwrap();
                ["first", "second", "live-first", "live-second"].map(|id| {
                    serde_json::to_value(db::calendar::get_event(&conn, id).unwrap()).unwrap()
                })
            };
            let (root, captured) = serve_responses(vec![
                (200, primary_calendar()),
                (200, json!({"value": [
                    {"id": "first", "isCancelled": true},
                    remote_event("live-first", &json!({})),
                    remote_event("new", &json!({
                        "type": "occurrence", "seriesMasterId": "master-new",
                        "originalStart": "2026-09-14T09:00:00Z", "recurrence": null
                    })),
                    {"id": "second", "isCancelled": true},
                    remote_event("live-second", &json!({"type": "singleInstance", "seriesMasterId": null, "recurrence": null}))
                ]})),
            ])
            .await;
            let error = GraphCalendarBackend
                .sync(
                    &CalendarBackendCtx {
                        db: &db,
                        services: &services(&root),
                    },
                    &account("calendar", "graph"),
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("injected"), "{error}");
            assert_eq!(captured.await.unwrap().len(), 2);

            let conn = db.reader();
            let after = ["first", "second", "live-first", "live-second"].map(|id| {
                serde_json::to_value(db::calendar::get_event(&conn, id).unwrap()).unwrap()
            });
            assert_eq!(after, before);
            assert_eq!(pending_cleanup_count(&conn), 0);
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM meet_meetings", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                2
            );
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM calendar_events", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                4
            );
            for id in ["first", "second"] {
                let identities = db::calendar_recurrence::get_by_event_id(&conn, id).unwrap();
                assert_eq!(identities.len(), 1);
                assert_eq!(identities[0].object_id, format!("identity-{id}"));
                assert_eq!(
                    identities[0].provider_revision.as_deref(),
                    Some("cached-revision")
                );
            }
            assert_eq!(
                conn.query_row(
                    "SELECT COUNT(*) FROM calendar_recurrence_objects",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                2
            );
        }
    }

    #[tokio::test]
    async fn failed_calendar_is_reported_while_other_calendars_still_reconcile() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_bound_occurrence(&conn, "cancelled");
        }
        let (root, captured) = serve_responses(vec![
            (
                200,
                json!({"value": [
                    {"id": "failed-calendar", "name": "Failed calendar"},
                    {"id": "primary", "name": "Calendar"}
                ]}),
            ),
            (500, json!({"error": "injected failure"})),
            (
                200,
                json!({"value": [{"id": "cancelled", "isCancelled": true}]}),
            ),
        ])
        .await;
        let error = GraphCalendarBackend
            .sync(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "graph"),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("Failed calendar"));
        assert_eq!(captured.await.unwrap().len(), 3);
        let conn = db.reader();
        assert!(db::calendar::get_event(&conn, "cancelled").is_err());
        assert_eq!(pending_cleanup_count(&conn), 1);
    }

    #[tokio::test]
    async fn complete_inventory_retires_legacy_calendar_and_its_series_master() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            conn.execute(
                "INSERT INTO calendars
                    (id, account_id, name, remote_id, is_subscribed)
                 VALUES ('legacy-calendar', 'acc1', 'Calendar', 'legacy-id', 1),
                        ('local-calendar', 'acc1', 'Local only', NULL, 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO calendar_events
                    (id, account_id, calendar_id, uid, title, start_time, end_time,
                     recurrence_rule, recurrence_kind, remote_id)
                 VALUES (
                    'legacy-series', 'acc1', 'legacy-calendar', 'series@example.test',
                    'Lunch', '2026-09-14T12:00:00Z', '2026-09-14T13:00:00Z',
                    'FREQ=DAILY', 'series', 'legacy-series-id'
                 )",
                [],
            )
            .unwrap();
        }
        let calendars = json!({
            "value": [
                {"id": "primary", "name": "Calendar", "isDefaultCalendar": true},
                {"id": "second-live", "name": "Calendar", "isDefaultCalendar": false}
            ]
        });
        let (root, captured) = serve_responses(vec![
            (200, calendars),
            (200, json!({"value": []})),
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

        assert_eq!(captured.await.unwrap().len(), 3);
        let conn = db.reader();
        let calendars: Vec<(String, Option<String>)> = conn
            .prepare(
                "SELECT name, remote_id FROM calendars WHERE account_id = 'acc1'
                 ORDER BY remote_id",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            calendars,
            vec![
                ("Local only".into(), None),
                ("Calendar".into(), Some("primary".into())),
                ("Calendar".into(), Some("second-live".into())),
            ]
        );
        assert!(db::calendar::get_event(&conn, "legacy-series").is_err());
    }

    #[tokio::test]
    async fn incomplete_refresh_never_retires_absent_calendars() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            conn.execute(
                "INSERT INTO calendars
                    (id, account_id, name, remote_id, is_subscribed)
                 VALUES ('legacy-calendar', 'acc1', 'Legacy', 'legacy-id', 1)",
                [],
            )
            .unwrap();
        }
        let (root, captured) = serve_responses(vec![
            (200, primary_calendar()),
            (500, json!({"error": "injected failure"})),
        ])
        .await;

        let result = GraphCalendarBackend
            .sync(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "graph"),
            )
            .await;

        assert!(result.is_err());
        assert_eq!(captured.await.unwrap().len(), 2);
        assert!(db::calendar::get_calendar(&db.reader(), "legacy-calendar").is_ok());
    }

    #[tokio::test]
    async fn protected_stale_calendar_fails_closed_without_partial_cleanup() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            conn.execute(
                "INSERT INTO calendars
                    (id, account_id, name, remote_id, is_subscribed)
                 VALUES
                    ('a-disposable-calendar', 'acc1', 'Disposable', 'old-a', 1),
                    ('z-protected-calendar', 'acc1', 'Protected', 'old-z', 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO calendar_events
                    (id, account_id, calendar_id, title, start_time, end_time, remote_id)
                 VALUES
                    ('disposable', 'acc1', 'a-disposable-calendar', 'Disposable',
                     '2026-09-14T08:00:00Z', '2026-09-14T09:00:00Z', 'old-a-event'),
                    ('protected', 'acc1', 'z-protected-calendar', 'Protected',
                     '2026-09-14T09:00:00Z', '2026-09-14T10:00:00Z', 'old-z-event')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO meet_meetings
                    (event_id, account_id, protocol, meeting_id, join_url)
                 VALUES (
                    'protected', 'acc1', 'zoom', 'meeting',
                    'https://example.test/join'
                 )",
                [],
            )
            .unwrap();
        }
        let (root, captured) =
            serve_responses(vec![(200, primary_calendar()), (200, json!({"value": []}))]).await;

        let error = GraphCalendarBackend
            .sync(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "graph"),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("local state"));
        assert_eq!(captured.await.unwrap().len(), 2);
        let conn = db.reader();
        assert!(db::calendar::get_calendar(&conn, "a-disposable-calendar").is_ok());
        assert!(db::calendar::get_calendar(&conn, "z-protected-calendar").is_ok());
        assert!(db::calendar::get_event(&conn, "disposable").is_ok());
        assert!(db::calendar::get_event(&conn, "protected").is_ok());
        assert!(db::meet_meetings::get(&conn, "protected")
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn unpushed_event_moves_to_unique_current_calendar_before_retirement() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            conn.execute(
                "INSERT INTO calendars
                    (id, account_id, name, is_default, remote_id, is_subscribed)
                 VALUES ('legacy-calendar', 'acc1', 'Calendar', 1, 'legacy-id', 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO calendar_events
                    (id, account_id, calendar_id, title, start_time, end_time,
                     source_message_id, ical_data)
                 VALUES (
                    'local-draft', 'acc1', 'legacy-calendar', 'Local draft',
                    '2026-09-14T09:00:00Z', '2026-09-14T10:00:00Z',
                    'source-message', 'BEGIN:VCALENDAR'
                 )",
                [],
            )
            .unwrap();
        }
        let (root, captured) =
            serve_responses(vec![(200, primary_calendar()), (200, json!({"value": []}))]).await;

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
        assert!(db::calendar::get_calendar(&conn, "legacy-calendar").is_err());
        let migrated = db::calendar::get_event(&conn, "local-draft").unwrap();
        assert_eq!(migrated.calendar_id, "cal1");
        assert_eq!(
            migrated.source_message_id.as_deref(),
            Some("source-message")
        );
        assert_eq!(migrated.ical_data.as_deref(), Some("BEGIN:VCALENDAR"));
    }

    #[tokio::test]
    async fn ambiguous_current_calendar_match_keeps_stale_local_events() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            conn.execute(
                "INSERT INTO calendars
                    (id, account_id, name, is_default, remote_id, is_subscribed)
                 VALUES ('legacy-calendar', 'acc1', 'Shared', 0, 'legacy-id', 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO calendar_events
                    (id, account_id, calendar_id, title, start_time, end_time)
                 VALUES (
                    'local-draft', 'acc1', 'legacy-calendar', 'Local draft',
                    '2026-09-14T09:00:00Z', '2026-09-14T10:00:00Z'
                 )",
                [],
            )
            .unwrap();
        }
        let calendars = json!({"value": [
            {"id": "primary", "name": "Calendar", "isDefaultCalendar": true},
            {"id": "shared-a", "name": "Shared", "isDefaultCalendar": false},
            {"id": "shared-b", "name": "Shared", "isDefaultCalendar": false}
        ]});
        let (root, captured) = serve_responses(vec![
            (200, calendars),
            (200, json!({"value": []})),
            (200, json!({"value": []})),
            (200, json!({"value": []})),
        ])
        .await;

        let error = GraphCalendarBackend
            .sync(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "graph"),
            )
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("2 exact current calendar matches"));
        assert_eq!(captured.await.unwrap().len(), 4);
        let conn = db.reader();
        assert!(db::calendar::get_calendar(&conn, "legacy-calendar").is_ok());
        assert_eq!(
            db::calendar::get_event(&conn, "local-draft")
                .unwrap()
                .calendar_id,
            "legacy-calendar"
        );
    }

    #[tokio::test]
    async fn pending_action_prevents_local_event_calendar_migration() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            conn.execute(
                "INSERT INTO calendars
                    (id, account_id, name, is_default, remote_id, is_subscribed)
                 VALUES ('legacy-calendar', 'acc1', 'Calendar', 1, 'legacy-id', 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO calendar_events
                    (id, account_id, calendar_id, title, start_time, end_time)
                 VALUES (
                    'local-draft', 'acc1', 'legacy-calendar', 'Local draft',
                    '2026-09-14T09:00:00Z', '2026-09-14T10:00:00Z'
                 )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO calendar_action_operations
                    (operation_id, account_id, event_id, data, completed)
                 VALUES ('operation', 'acc1', 'local-draft', '{}', 0)",
                [],
            )
            .unwrap();
        }
        let (root, captured) =
            serve_responses(vec![(200, primary_calendar()), (200, json!({"value": []}))]).await;

        let error = GraphCalendarBackend
            .sync(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "graph"),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("calendar action state"));
        assert_eq!(captured.await.unwrap().len(), 2);
        let conn = db.reader();
        assert!(db::calendar::get_calendar(&conn, "legacy-calendar").is_ok());
        assert_eq!(
            db::calendar::get_event(&conn, "local-draft")
                .unwrap()
                .calendar_id,
            "legacy-calendar"
        );
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
            assert_eq!(
                result.is_err(),
                response_status != 200 || reject_invalidation
            );
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
            .unwrap_err();
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
    async fn immutable_event_is_rehomed_instead_of_duplicated() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_event(&conn, "stale-local-row", Some("legacy-event-id"));
            conn.execute(
                "UPDATE calendar_events
                 SET uid = 'uid-immutable-event@example.test',
                     recurrence_kind = 'standalone'
                 WHERE id = 'stale-local-row'",
                [],
            )
            .unwrap();
            cache_event(&conn, "same-looking-other", Some("other-event-id"));
            conn.execute(
                "UPDATE calendar_events
                 SET title = 'Refreshed event', recurrence_kind = 'standalone'
                 WHERE id = 'same-looking-other'",
                [],
            )
            .unwrap();
        }
        let calendars = json!({
            "value": [
                {"id": "primary", "name": "Primary", "isDefaultCalendar": true},
                {"id": "secondary", "name": "Secondary", "isDefaultCalendar": false}
            ]
        });
        let standalone = json!({
            "type": "singleInstance", "seriesMasterId": null, "recurrence": null
        });
        let (root, captured) = serve_responses(vec![
            (200, calendars),
            (
                200,
                json!({"value": [remote_event("other-event-id", &standalone)]}),
            ),
            (
                200,
                json!({"value": [remote_event("immutable-event", &standalone)]}),
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
        let secondary: String = conn
            .query_row(
                "SELECT id FROM calendars WHERE account_id = 'acc1' AND remote_id = 'secondary'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let rows: Vec<(String, String)> = conn
            .prepare(
                "SELECT id, calendar_id FROM calendar_events WHERE account_id = 'acc1'
                 AND uid = 'uid-immutable-event@example.test'",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows, vec![("stale-local-row".into(), secondary)]);
        assert_eq!(
            db::calendar::get_event(&conn, "stale-local-row")
                .unwrap()
                .remote_id
                .as_deref(),
            Some("immutable-event")
        );
        assert!(db::calendar::get_event(&conn, "same-looking-other").is_ok());
    }

    #[tokio::test]
    async fn existing_immutable_event_duplicates_are_reconciled() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            conn.execute(
                "INSERT INTO calendars
                    (id, account_id, name, remote_id, is_subscribed)
                 VALUES ('secondary-local', 'acc1', 'Secondary', 'secondary', 1)",
                [],
            )
            .unwrap();
            cache_event(&conn, "stale-local-row", Some("immutable-event"));
            cache_event(&conn, "authoritative-local-row", Some("immutable-event"));
            conn.execute(
                "UPDATE calendar_events SET calendar_id = 'secondary-local'
                 WHERE id = 'authoritative-local-row'",
                [],
            )
            .unwrap();
        }
        let calendars = json!({
            "value": [
                {"id": "primary", "name": "Primary", "isDefaultCalendar": true},
                {"id": "secondary", "name": "Secondary", "isDefaultCalendar": false}
            ]
        });
        let standalone = json!({
            "type": "singleInstance", "seriesMasterId": null, "recurrence": null
        });
        let (root, captured) = serve_responses(vec![
            (200, calendars),
            (200, json!({"value": []})),
            (
                200,
                json!({"value": [remote_event("immutable-event", &standalone)]}),
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
        let rows: Vec<String> = conn
            .prepare(
                "SELECT id FROM calendar_events
                 WHERE account_id = 'acc1' AND remote_id = 'immutable-event'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows, vec!["authoritative-local-row"]);
        assert_eq!(
            db::calendar::get_event(&conn, "authoritative-local-row")
                .unwrap()
                .title,
            "Refreshed event"
        );
    }

    #[tokio::test]
    async fn action_owned_event_retires_stale_provider_cache_before_reconciliation() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_action_owned_occurrence(&conn, false);
        }
        let (root, captured) = serve_responses(vec![
            (200, primary_calendar()),
            (
                200,
                json!({
                    "value": [remote_event(
                        "immutable-occurrence",
                        &occurrence_metadata()
                    )]
                }),
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

        assert_eq!(captured.await.unwrap().len(), 2);
        let conn = db.reader();
        assert!(db::calendar::get_event(&conn, "stale-provider-cache").is_err());
        assert!(db::calendar::get_event(&conn, "series-owner").is_ok());
        assert!(db::calendar::get_event(&conn, "projected-member").is_ok());
        assert_eq!(
            db::calendar::get_event(&conn, "projected-member")
                .unwrap()
                .remote_id,
            None
        );
        assert!(conn
            .query_row(
                "SELECT dirty FROM calendar_action_sets
                 WHERE event_id = 'series-owner'",
                [],
                |row| row.get::<_, bool>(0)
            )
            .unwrap());
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM calendar_events
                 WHERE remote_id IN ('legacy-occurrence', 'immutable-occurrence')",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn protected_action_owned_cache_duplicate_rolls_back_routing() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_action_owned_occurrence(&conn, true);
        }
        let (root, captured) = serve_responses(vec![
            (200, primary_calendar()),
            (
                200,
                json!({
                    "value": [remote_event(
                        "immutable-occurrence",
                        &occurrence_metadata()
                    )]
                }),
            ),
        ])
        .await;

        let error = GraphCalendarBackend
            .sync(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root),
                },
                &account("calendar", "graph"),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("local state"), "{error}");
        assert_eq!(captured.await.unwrap().len(), 2);
        let conn = db.reader();
        assert!(db::calendar::get_event(&conn, "stale-provider-cache").is_ok());
        assert!(db::meet_meetings::get(&conn, "stale-provider-cache")
            .unwrap()
            .is_some());
        assert!(!conn
            .query_row(
                "SELECT dirty FROM calendar_action_sets
                 WHERE event_id = 'series-owner'",
                [],
                |row| row.get::<_, bool>(0)
            )
            .unwrap());
    }

    #[tokio::test]
    async fn protected_duplicate_rolls_back_prior_cleanup() {
        let (_dir, db) = setup_db().await;
        let mut conn = db.writer().await;
        conn.execute(
            "INSERT INTO calendars
                (id, account_id, name, remote_id, is_subscribed)
             VALUES ('secondary-local', 'acc1', 'Secondary', 'secondary', 1)",
            [],
        )
        .unwrap();
        for id in ["authoritative", "a-disposable", "z-protected"] {
            cache_event(&conn, id, Some("immutable-event"));
        }
        conn.execute(
            "UPDATE calendar_events SET calendar_id = 'secondary-local'
             WHERE id = 'authoritative'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meet_meetings
                (event_id, account_id, protocol, meeting_id, join_url)
             VALUES (
                'z-protected', 'acc1', 'zoom', 'meeting',
                'https://example.test/join'
             )",
            [],
        )
        .unwrap();

        let result = {
            let transaction = conn.transaction().unwrap();
            let result = super::reconcile_graph_event_identity(
                &transaction,
                "acc1",
                "secondary-local",
                "secondary",
                "immutable-event",
                None,
                None,
                None,
                None,
            );
            drop(transaction);
            result
        };

        assert!(result.unwrap_err().to_string().contains("local state"));
        let ids: Vec<String> = conn
            .prepare(
                "SELECT id FROM calendar_events
                 WHERE account_id = 'acc1' AND remote_id = 'immutable-event'
                 ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec!["a-disposable", "authoritative", "z-protected"]);
        assert!(db::meet_meetings::get(&conn, "z-protected")
            .unwrap()
            .is_some());
        assert_eq!(pending_cleanup_count(&conn), 0);
    }

    #[tokio::test]
    async fn bounded_view_absence_retires_only_authoritative_instances() {
        let (_dir, db) = setup_db().await;
        let mut conn = db.writer().await;
        for (id, remote_id, kind) in [
            ("seen", Some("seen-remote"), "occurrence"),
            ("absent-occurrence", Some("absent-occurrence"), "occurrence"),
            ("absent-standalone", Some("absent-standalone"), "standalone"),
            ("series-master", Some("series-master"), "series"),
            ("unknown", Some("unknown"), "unknown"),
            ("outside-window", Some("outside-window"), "occurrence"),
            ("local-only", None, "occurrence"),
        ] {
            cache_event(&conn, id, remote_id);
            conn.execute(
                "UPDATE calendar_events SET recurrence_kind = ?1 WHERE id = ?2",
                rusqlite::params![kind, id],
            )
            .unwrap();
        }
        conn.execute(
            "UPDATE calendar_events
             SET start_time = '2025-01-01T09:00:00Z',
                 end_time = '2025-01-01T10:00:00Z'
             WHERE id = 'outside-window'",
            [],
        )
        .unwrap();
        let observed = std::collections::HashSet::from(["seen-remote".to_owned()]);
        let transaction = conn.transaction().unwrap();

        super::retire_absent_graph_view_rows(
            &transaction,
            "acc1",
            "cal1",
            "2026-09-01T00:00:00Z",
            "2026-10-01T00:00:00Z",
            &observed,
        )
        .unwrap();
        transaction.commit().unwrap();

        for id in ["absent-occurrence", "absent-standalone"] {
            assert!(db::calendar::get_event(&conn, id).is_err(), "{id}");
        }
        for id in [
            "seen",
            "series-master",
            "unknown",
            "outside-window",
            "local-only",
        ] {
            assert!(db::calendar::get_event(&conn, id).is_ok(), "{id}");
        }
    }

    #[tokio::test]
    async fn protected_bounded_view_absence_rolls_back_prior_writes() {
        let (_dir, db) = setup_db().await;
        let mut conn = db.writer().await;
        cache_event(&conn, "stale-protected", Some("stale-remote"));
        cache_event(&conn, "prior-write", Some("seen-remote"));
        conn.execute(
            "UPDATE calendar_events SET recurrence_kind = 'occurrence'
             WHERE id = 'stale-protected'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meet_meetings
                 (event_id, account_id, protocol, meeting_id, join_url)
             VALUES (
                 'stale-protected', 'acc1', 'zoom', 'protected',
                 'https://example.test/join'
             )",
            [],
        )
        .unwrap();
        let observed = std::collections::HashSet::from(["seen-remote".to_owned()]);
        let transaction = conn.transaction().unwrap();
        transaction
            .execute(
                "UPDATE calendar_events SET title = 'Changed'
                 WHERE id = 'prior-write'",
                [],
            )
            .unwrap();

        let result = super::retire_absent_graph_view_rows(
            &transaction,
            "acc1",
            "cal1",
            "2026-09-01T00:00:00Z",
            "2026-10-01T00:00:00Z",
            &observed,
        );
        drop(transaction);

        assert!(result.unwrap_err().to_string().contains("local state"));
        assert_eq!(
            db::calendar::get_event(&conn, "prior-write").unwrap().title,
            "Cached event"
        );
        assert!(db::calendar::get_event(&conn, "stale-protected").is_ok());
        assert!(db::meet_meetings::get(&conn, "stale-protected")
            .unwrap()
            .is_some());
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
