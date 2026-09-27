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
    RoomAvailability, RoomAvailabilityRequest, RoomSuggestion, VerifiedOccurrenceMembership,
};

pub struct GraphCalendarBackend;

/// A fallback cache match is possible only for identities absent from the
/// complete inventory and bounded views collected in this sync pass.
struct GraphSyncIdentityScope {
    calendar_ids: std::collections::HashSet<String>,
    event_ids: std::collections::HashSet<String>,
}

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

    async fn fetch_event_set_for_repair(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        event: &CalendarEvent,
        remote_calendar_id: &str,
    ) -> Result<(CalendarEventSet, Option<VerifiedOccurrenceMembership>)> {
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
            .fetch_calendar_event_set_for_repair(remote_calendar_id, id, event)
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
        // A complete inventory is only authoritative together with its event
        // views. Do not publish a new calendar before the whole account can be
        // reconciled; otherwise a protected legacy row leaves two visible
        // calendars on every failed sync.
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

        let mut remote_to_local: std::collections::HashMap<String, (String, bool, bool)> =
            std::collections::HashMap::new();

        {
            let conn = db.reader();
            for gc in &graph_calendars {
                let existing: Option<(String, bool)> = conn
                    .query_row(
                        "SELECT id, is_subscribed FROM calendars WHERE account_id = ?1 AND remote_id = ?2",
                        rusqlite::params![account_id, gc.id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;

                let (local_id, subscribed, is_new) = match existing {
                    Some((local_id, subscribed)) => (local_id, subscribed, false),
                    None => (uuid::Uuid::new_v4().to_string(), true, true),
                };
                remote_to_local.insert(gc.id.clone(), (local_id, subscribed, is_new));
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
        let mut views = Vec::new();

        for gc in &graph_calendars {
            let Some((_, subscribed, _)) = remote_to_local.get(&gc.id) else {
                continue;
            };
            if !subscribed {
                log::debug!(
                    "sync_calendars_graph: skipping unsubscribed calendar '{}'",
                    gc.name
                );
                continue;
            }

            let calendar_events = client
                .list_events_for_calendar(&gc.id, &start, &end)
                .await
                .map_err(|error| match error {
                    Error::GraphThrottled { .. } => error,
                    other => Error::Sync(format!(
                        "Graph calendar '{}' could not fetch a complete view: {other}",
                        gc.name
                    )),
                })?;
            log::info!(
                "sync_calendars_graph: fetched {} events for calendar '{}'",
                calendar_events.len(),
                gc.name
            );
            views.push((gc, calendar_events));
        }

        let mut observed_owners = std::collections::HashMap::new();
        for (calendar, items) in &views {
            for item in items {
                let id = match item {
                    GraphCalendarItem::Live(event) => event.id.as_str(),
                    GraphCalendarItem::Cancelled(tombstone) => tombstone.remote_id(),
                };
                if let Some(previous) = observed_owners.insert(id.to_string(), &calendar.id) {
                    if previous != &calendar.id {
                        return Err(Error::Sync(format!(
                            "Graph event {id:?} appeared in two current calendars during one sync"
                        )));
                    }
                }
            }
        }
        let scope = GraphSyncIdentityScope {
            calendar_ids: graph_calendars
                .iter()
                .map(|calendar| calendar.id.clone())
                .collect(),
            event_ids: observed_owners.into_keys().collect(),
        };
        // A locally unpublished row cannot be assigned to one of two current
        // provider copies with the same invite identity. Resolve this before
        // any calendar or event writes, rather than picking by response order.
        let mut identity_counts = std::collections::HashMap::new();
        for (calendar, items) in &views {
            for item in items {
                let GraphCalendarItem::Live(event) = item else {
                    continue;
                };
                let Some(uid) = event.ical_uid.as_deref().filter(|uid| !uid.is_empty()) else {
                    continue;
                };
                let key = (calendar.id.as_str(), uid, event.start.as_str());
                *identity_counts.entry(key).or_insert(0usize) += 1;
            }
        }
        {
            let conn = db.reader();
            for ((provider_calendar_id, uid, start), count) in identity_counts {
                if count < 2 {
                    continue;
                }
                let (local_cal_id, _, _) = remote_to_local
                    .get(provider_calendar_id)
                    .ok_or_else(|| Error::Sync("Graph inventory mapping disappeared".into()))?;
                let unpushed: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM calendar_events event
                     WHERE event.account_id = ?1 AND event.calendar_id = ?2
                       AND event.uid = ?3 AND event.start_time = ?4
                       AND (event.remote_id IS NULL OR trim(event.remote_id) = '')
                       AND NOT EXISTS (SELECT 1 FROM calendar_action_members member
                                       WHERE member.event_id = event.id)
                       AND NOT EXISTS (SELECT 1 FROM calendar_action_sets owned
                                       WHERE owned.event_id = event.id))",
                    rusqlite::params![account_id, local_cal_id, uid, start],
                    |row| row.get(0),
                )?;
                if unpushed {
                    return Err(Error::Sync(format!(
                        "Graph calendar {provider_calendar_id:?} has multiple current events for an unpublished local invite; refusing an ambiguous cache merge"
                    )));
                }
            }
        }
        let mut conn = db.writer().await;
        let tx = conn.transaction()?;
        let mut created = Vec::new();
        for gc in &graph_calendars {
            let (local_id, subscribed, is_new) = remote_to_local
                .get(&gc.id)
                .ok_or_else(|| Error::Sync("Graph inventory mapping disappeared".into()))?;
            if *is_new {
                let cal = NewCalendar {
                    account_id: account_id.to_string(),
                    name: gc.name.clone(),
                    color: gc.color.clone(),
                    is_default: gc.is_default,
                };
                db::calendar::insert_calendar(&tx, local_id, &cal)?;
                tx.execute(
                    "UPDATE calendars SET remote_id = ?1 WHERE id = ?2",
                    rusqlite::params![gc.id, local_id],
                )?;
                created.push((gc.name.as_str(), gc.id.as_str()));
            } else {
                let current: Option<bool> = tx
                    .query_row(
                        "SELECT is_subscribed FROM calendars
                     WHERE account_id = ?1 AND id = ?2 AND remote_id = ?3",
                        rusqlite::params![account_id, local_id, gc.id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if current != Some(*subscribed) {
                    return Err(Error::Sync(format!(
                        "Graph calendar '{}' changed during inventory read; retry sync",
                        gc.name
                    )));
                }
                // The locally chosen color must survive a provider refresh.
                tx.execute(
                    "UPDATE calendars SET name = ?1, is_archived = 0,
                     archived_acknowledged_at = NULL WHERE id = ?2",
                    rusqlite::params![gc.name, local_id],
                )?;
            }
        }
        let mut complete_views = Vec::new();
        for (gc, items) in views {
            let (local_cal_id, _, _) = remote_to_local
                .get(&gc.id)
                .ok_or_else(|| Error::Sync("Graph inventory mapping disappeared".into()))?;
            let observed = items
                .iter()
                .map(|item| match item {
                    GraphCalendarItem::Live(event) => event.id.clone(),
                    GraphCalendarItem::Cancelled(tombstone) => tombstone.remote_id().to_owned(),
                })
                .collect();
            reconcile_calendar_events(
                &tx,
                account_id,
                &account.email,
                local_cal_id,
                &gc.id,
                &scope,
                items,
            )
            .map_err(|error| {
                Error::Sync(format!(
                    "Graph calendar '{}' reconciliation failed: {error}",
                    gc.name
                ))
            })?;
            complete_views.push((local_cal_id, observed));
        }
        // A legacy event may be rehomed to a later calendar in the same
        // inventory. Only delete absent instances after all calendars import.
        for (local_cal_id, observed) in complete_views {
            retire_absent_graph_view_rows(&tx, account_id, local_cal_id, &start, &end, &observed)?;
        }
        let authoritative_calendars = graph_calendars
            .iter()
            .map(|calendar| {
                remote_to_local
                    .get(&calendar.id)
                    .map(|(local_id, _, _)| (calendar, local_id.as_str()))
                    .ok_or_else(|| Error::Sync("Graph inventory mapping disappeared".into()))
            })
            .collect::<Result<Vec<_>>>()?;
        let retired = retire_stale_graph_calendars(&tx, account_id, &authoritative_calendars)?;
        tx.commit()?;
        for (name, remote_id) in created {
            log::info!(
                "sync_calendars_graph: created calendar '{}' ({})",
                name,
                remote_id
            );
        }
        for retirement in retired {
            log::info!(
                "sync_calendars_graph: {} stale calendar '{}' ({}) after deleting {} provider cache events and retaining {} cached events read-only",
                if retirement.archived { "archived" } else { "retired" },
                retirement.name,
                retirement.remote_id,
                retirement.deleted_events,
                retirement.retained_events
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

/// The caller owns the all-or-nothing account transaction. No provider I/O is
/// performed while it is held.
fn reconcile_calendar_events(
    transaction: &rusqlite::Transaction<'_>,
    account_id: &str,
    account_email: &str,
    local_calendar_id: &str,
    provider_calendar_id: &str,
    scope: &GraphSyncIdentityScope,
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

    for tombstone in cancelled {
        if let Some(owner) = db::calendar_actions::ingest_owned_identity(
            transaction,
            account_id,
            local_calendar_id,
            Some(tombstone.remote_id()),
            &[],
        )? {
            // A legacy row can still retain this exact remote ID independently
            // of its canonical owner. Do not leave it visible after cancellation.
            retire_action_owned_graph_cache_rows(
                transaction,
                account_id,
                local_calendar_id,
                &owner,
                tombstone.remote_id(),
                None,
                None,
                None,
                None,
                None,
                false,
                None,
                None,
                scope,
            )?;
            log::info!(
                "Graph tombstone invalidated action-owned series {} for {}",
                owner,
                tombstone.remote_id()
            );
            continue;
        }
        reconcile_graph_event_identity(
            transaction,
            account_id,
            local_calendar_id,
            provider_calendar_id,
            tombstone.remote_id(),
            None,
            None,
            None,
            None,
            None,
            false,
            scope,
        )?;
        db::calendar_event_deletion::delete_calendar_events_by_remote_id(
            transaction,
            account_id,
            local_calendar_id,
            tombstone.remote_id(),
        )?;
    }
    for ge in live {
        let recurrence_seeds = ge.recurrence_seeds.as_deref().unwrap_or(&[]);
        let provider_invite_visible = visible_invite_fields(
            account_email,
            ge.organizer_email.as_deref(),
            ge.attendees_json.as_deref(),
            ge.my_status.as_deref(),
        );
        if let Some(owner_id) = db::calendar_actions::ingest_owned_identity(
            transaction,
            account_id,
            local_calendar_id,
            Some(&ge.id),
            recurrence_seeds,
        )? {
            if provider_invite_visible {
                refresh_graph_owned_member_from_seed(
                    transaction,
                    account_id,
                    local_calendar_id,
                    &owner_id,
                    &ge,
                    recurrence_seeds,
                )?;
            }
            retire_action_owned_graph_cache_rows(
                transaction,
                account_id,
                local_calendar_id,
                &owner_id,
                &ge.id,
                ge.ical_uid.as_deref(),
                Some(&ge.start),
                Some(&ge.end),
                Some(ge.recurrence_kind),
                ge.organizer_email.as_deref(),
                provider_invite_visible,
                ge.attendees_json.as_deref(),
                ge.my_status.as_deref(),
                scope,
            )?;
            continue;
        }
        reconcile_graph_event_identity(
            transaction,
            account_id,
            local_calendar_id,
            provider_calendar_id,
            &ge.id,
            ge.ical_uid.as_deref(),
            Some(&ge.start),
            Some(&ge.end),
            Some(ge.recurrence_kind),
            ge.organizer_email.as_deref(),
            provider_invite_visible,
            scope,
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
                    transaction,
                    &event,
                    seeds,
                )?;
            }
            None => {
                db::calendar::upsert_event_by_remote_id_in_transaction(transaction, &event)?;
            }
        }
    }

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
        ensure_graph_absent_cache_retirable(conn, &event_id)?;
        db::calendar_event_deletion::delete_event(conn, &event_id)?;
        log::info!(
            "Graph sync retired event {} absent from complete bounded view",
            event_id
        );
    }
    Ok(())
}

struct StaleGraphCalendarRetirement {
    name: String,
    remote_id: String,
    deleted_events: usize,
    retained_events: i64,
    archived: bool,
}

/// Retire disposable provider cache absent from Graph's complete inventory;
/// archive action-owned calendars without changing their cached identities.
/// Names/default status cannot prove a destination for unpublished local work.
fn retire_stale_graph_calendars<'a>(
    transaction: &rusqlite::Transaction<'_>,
    account_id: &str,
    authoritative_calendars: &[(&'a GraphCalendar, &'a str)],
) -> Result<Vec<StaleGraphCalendarRetirement>> {
    let authoritative_ids = authoritative_calendars
        .iter()
        .map(|(calendar, _)| calendar.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let stale = {
        let mut statement = transaction.prepare(
            "SELECT id, name, remote_id FROM calendars
              WHERE account_id = ?1 AND is_archived = 0
                AND remote_id IS NOT NULL AND remote_id != ''
             ORDER BY id",
        )?;
        let rows = statement
            .query_map([account_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|(_, _, remote_id)| !authoritative_ids.contains(remote_id.as_str()))
            .collect::<Vec<_>>();
        rows
    };
    if stale.is_empty() {
        return Ok(Vec::new());
    }

    let mut retired = Vec::with_capacity(stale.len());
    for (calendar_id, name, remote_id) in stale {
        let action_addresses: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM calendar_action_addresses
             WHERE account_id = ?1 AND calendar_id = ?2",
            rusqlite::params![account_id, calendar_id],
            |row| row.get(0),
        )?;
        let unpushed_count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM calendar_events
                 WHERE account_id = ?1 AND calendar_id = ?2
                   AND (remote_id IS NULL OR trim(remote_id) = '')
                   AND NOT EXISTS (
                       SELECT 1 FROM calendar_action_members member
                       WHERE member.event_id = calendar_events.id
                         AND member.owner_event_id != calendar_events.id
                   )",
            rusqlite::params![account_id, calendar_id],
            |row| row.get(0),
        )?;
        if unpushed_count > 0 {
            return Err(Error::Sync(format!(
                "Stale Graph calendar {calendar_id:?} has {unpushed_count} unpublished local events; a destination cannot be inferred from its name"
            )));
        }

        let pending_actions: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM calendar_action_claims claim
                 JOIN calendar_events event ON event.id = claim.event_id
                 WHERE event.account_id = ?1 AND event.calendar_id = ?2
                 UNION ALL SELECT 1 FROM calendar_action_operations operation
                 WHERE operation.account_id = ?1 AND operation.completed = 0
                   AND (operation.event_id IN
                        (SELECT id FROM calendar_events WHERE calendar_id = ?2)
                        OR json_extract(operation.data, '$.source.anchor.calendar_id') = ?2
                        OR json_extract(operation.data, '$.destination.calendar_id') = ?2)
                 UNION ALL SELECT 1 FROM calendar_action_creations creation
                 WHERE creation.account_id = ?1 AND creation.completed = 0
                   AND (creation.event_id IN
                        (SELECT id FROM calendar_events WHERE calendar_id = ?2)
                        OR json_extract(creation.data, '$.destination.calendar_id') = ?2)
             )",
            rusqlite::params![account_id, calendar_id],
            |row| row.get(0),
        )?;
        if pending_actions {
            return Err(Error::Sync(format!(
                "Stale Graph calendar {calendar_id:?} has pending calendar actions; cannot retire it read-only"
            )));
        }

        let action_owned: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM calendar_events event
             WHERE event.account_id = ?1 AND event.calendar_id = ?2
               AND (EXISTS (SELECT 1 FROM calendar_action_sets owned
                            WHERE owned.event_id = event.id)
                    OR EXISTS (SELECT 1 FROM calendar_action_members member
                               WHERE member.event_id = event.id
                                  OR member.owner_event_id = event.id)
                    OR EXISTS (SELECT 1 FROM calendar_action_addresses address
                               WHERE address.owner_event_id = event.id)))",
            rusqlite::params![account_id, calendar_id],
            |row| row.get(0),
        )?;
        if action_owned {
            let cross_calendar_members: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM calendar_action_members member
                 JOIN calendar_events child ON child.id = member.event_id
                 JOIN calendar_events owner ON owner.id = member.owner_event_id
                 WHERE (child.calendar_id = ?1 AND owner.calendar_id != ?1)
                    OR (owner.calendar_id = ?1 AND child.calendar_id != ?1))",
                [&calendar_id],
                |row| row.get(0),
            )?;
            let active_cross_calendar_addresses: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM calendar_action_addresses address
                 JOIN calendar_events owner ON owner.id = address.owner_event_id
                 WHERE owner.calendar_id = ?1 AND address.calendar_id != ?1)",
                [&calendar_id],
                |row| row.get(0),
            )?;
            if cross_calendar_members || active_cross_calendar_addresses {
                return Err(Error::Sync(format!(
                    "Stale Graph calendar {calendar_id:?} has action ownership across calendars; cannot archive it independently"
                )));
            }
            let retained: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM calendar_events
                 WHERE account_id = ?1 AND calendar_id = ?2",
                rusqlite::params![account_id, calendar_id],
                |row| row.get(0),
            )?;
            transaction.execute(
                "UPDATE calendars SET is_archived = 1,
                     archived_acknowledged_at = NULL
                 WHERE id = ?1 AND account_id = ?2 AND remote_id = ?3",
                rusqlite::params![calendar_id, account_id, remote_id],
            )?;
            retired.push(StaleGraphCalendarRetirement {
                name,
                remote_id,
                deleted_events: 0,
                retained_events: retained,
                archived: true,
            });
            continue;
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
            ensure_graph_absent_cache_retirable(transaction, event_id)?;
        }
        let deleted =
            db::calendar_event_deletion::delete_calendar_events(transaction, &calendar_id)?;
        if action_addresses > 0 {
            transaction.execute(
                "UPDATE calendars SET is_archived = 1,
                     archived_acknowledged_at = NULL
                 WHERE id = ?1 AND account_id = ?2 AND remote_id = ?3",
                rusqlite::params![calendar_id, account_id, remote_id],
            )?;
        } else {
            db::calendar::delete_calendar_row(transaction, &calendar_id)?;
        }
        retired.push(StaleGraphCalendarRetirement {
            name,
            remote_id,
            deleted_events: deleted.deleted,
            retained_events: 0,
            archived: action_addresses > 0,
        });
    }
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
    organizer_email: Option<&str>,
    provider_invite_visible: bool,
    scope: &GraphSyncIdentityScope,
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
        scope,
    )?;
    let Some((survivor_id, survivor_calendar_id, survivor_remote_id)) = rows.first() else {
        return Ok(());
    };
    ensure_marked_graph_identity(
        conn,
        survivor_id,
        remote_id,
        ical_uid,
        start,
        end,
        recurrence_kind,
        organizer_email,
        provider_invite_visible,
    )?;

    for (duplicate_id, _, _) in rows.iter().skip(1) {
        prepare_graph_duplicate_for_retirement(
            conn,
            duplicate_id,
            survivor_id,
            organizer_email,
            provider_invite_visible,
        )?;
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
    scope: &GraphSyncIdentityScope,
) -> Result<Vec<(String, String, String)>> {
    let ical_uid = ical_uid.filter(|uid| !uid.is_empty());
    let mut stmt = conn.prepare(
        "SELECT event.id, event.calendar_id, event.remote_id
         FROM calendar_events event
         JOIN calendars calendar ON calendar.id = event.calendar_id
         WHERE event.account_id = ?1 AND calendar.is_archived = 0 AND (
             event.remote_id = ?2 OR (
                 ?4 IS NOT NULL AND event.uid = ?4
                 AND event.start_time = ?5 AND event.end_time = ?6
                 AND event.recurrence_kind = ?7
                 AND event.remote_id IS NOT NULL AND event.remote_id != ''
             )
         )
         ORDER BY CASE WHEN event.remote_id = ?2 THEN 0 ELSE 1 END,
                  CASE WHEN event.calendar_id = ?3 THEN 0 ELSE 1 END,
                  event.updated_at DESC, event.id",
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
    let mut matched = Vec::new();
    for row in rows {
        if row.2 != remote_id && scope.event_ids.contains(&row.2) {
            continue;
        }
        if row.2 == remote_id || row.1 == local_calendar_id {
            matched.push(row);
            continue;
        }
        let provider_id: Option<String> = conn
            .query_row(
                "SELECT remote_id FROM calendars
                 WHERE id = ?1 AND account_id = ?2",
                rusqlite::params![row.1, account_id],
                |result| result.get(0),
            )
            .optional()?
            .flatten();
        if provider_id
            .as_ref()
            .is_some_and(|id| !scope.calendar_ids.contains(id))
        {
            matched.push(row);
        }
    }
    Ok(matched)
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
    organizer_email: Option<&str>,
    provider_invite_visible: bool,
    provider_attendees: Option<&str>,
    provider_status: Option<&str>,
    scope: &GraphSyncIdentityScope,
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
        scope,
    )?;
    for (event_id, _, _) in rows {
        if db::calendar_actions::owner_id(conn, &event_id)? == owner_id {
            continue;
        }
        let managed: bool = conn.query_row(
            "SELECT manually_managed_at IS NOT NULL FROM calendar_events WHERE id = ?1",
            [&event_id],
            |row| row.get(0),
        )?;
        if managed {
            ensure_marked_graph_identity(
                conn,
                &event_id,
                remote_id,
                ical_uid,
                start,
                end,
                recurrence_kind,
                organizer_email,
                provider_invite_visible,
            )?;
            let mut stmt = conn.prepare(
                "SELECT member.event_id FROM calendar_action_members member
                 JOIN calendar_events event ON event.id = member.event_id
                 JOIN calendar_events source ON source.id = ?2
                 WHERE member.owner_event_id = ?1 AND member.event_id != ?2
                   AND event.account_id = source.account_id
                   AND event.uid = source.uid
                   AND event.start_time = source.start_time
                   AND event.end_time = source.end_time
                   AND event.recurrence_kind = source.recurrence_kind
                   AND event.organizer_email = source.organizer_email",
            )?;
            let successors = stmt
                .query_map(rusqlite::params![owner_id, event_id], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let [successor] = successors.as_slice() else {
                return Err(Error::Sync(format!(
                    "Graph event {event_id:?} has a local invite acknowledgement but no unique verified series member to preserve it"
                )));
            };
            if provider_invite_visible {
                refresh_graph_member_participants(
                    conn,
                    account_id,
                    successor,
                    provider_attendees,
                    provider_status,
                )?;
            }
            prepare_graph_duplicate_for_retirement(
                conn,
                &event_id,
                successor,
                organizer_email,
                provider_invite_visible,
            )?;
        } else {
            ensure_disposable_graph_duplicate(conn, &event_id)?;
        }
        db::calendar_event_deletion::delete_event(conn, &event_id)?;
        log::info!(
            "Graph sync retired action-owned cache duplicate {} for event {}",
            event_id,
            remote_id
        );
    }
    Ok(())
}

fn refresh_graph_member_participants(
    conn: &rusqlite::Connection,
    account_id: &str,
    event_id: &str,
    attendees: Option<&str>,
    status: Option<&str>,
) -> Result<()> {
    let pending: Option<String> = conn.query_row(
        "SELECT pending_rsvp_status FROM calendar_events
         WHERE id = ?1 AND account_id = ?2",
        rusqlite::params![event_id, account_id],
        |row| row.get(0),
    )?;
    if pending.is_none() || pending.as_deref() == status {
        conn.execute(
            "UPDATE calendar_events
             SET attendees_json = COALESCE(?1, attendees_json),
                 my_status = ?2, pending_rsvp_status = NULL
             WHERE id = ?3 AND account_id = ?4",
            rusqlite::params![attendees, status, event_id, account_id],
        )?;
        db::calendar_invitation::invalidate(conn, event_id)?;
    }
    Ok(())
}

/// Only a uniquely bound original slot proves which cached child can receive
/// Graph's current RSVP. A no-seed calendarView row cannot update a member.
fn refresh_graph_owned_member_from_seed(
    conn: &rusqlite::Connection,
    account_id: &str,
    calendar_id: &str,
    owner_id: &str,
    event: &crate::mail::graph::GraphCalendarEvent,
    seeds: &[crate::calendar::recurrence_identity::RecurrenceIdentitySeed],
) -> Result<()> {
    let Some(seed) = seeds.iter().find(|seed| {
        matches!(
            seed.kind,
            RecurrenceObjectKind::Occurrence | RecurrenceObjectKind::Exception
        )
    }) else {
        return Ok(());
    };
    let original = db::calendar_actions::identity_position(
        seed.recurrence_value_type == Some(RecurrenceValueType::Date),
        seed.recurrence_id
            .as_deref()
            .ok_or_else(|| Error::Sync("Graph occurrence seed has no original position".into()))?,
        seed.recurrence_timezone
            .as_deref()
            .or(event.timezone.as_deref()),
    )?;
    let mut stmt = conn.prepare(
        "SELECT member.event_id FROM calendar_action_members member
         JOIN calendar_events cached ON cached.id = member.event_id
         WHERE member.owner_event_id = ?1 AND member.original_start = ?2
           AND cached.account_id = ?3 AND cached.calendar_id = ?4
           AND cached.uid = ?5",
    )?;
    let rows = stmt
        .query_map(
            rusqlite::params![owner_id, original, account_id, calendar_id, event.ical_uid],
            |row| row.get::<_, String>(0),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    match rows.as_slice() {
        [member] => refresh_graph_member_participants(
            conn,
            account_id,
            member,
            event.attendees_json.as_deref(),
            event.my_status.as_deref(),
        ),
        [] => Ok(()),
        _ => Err(Error::Sync(
            "Graph occurrence matches multiple owned members".into(),
        )),
    }
}

/// An acknowledgement is local invitation intent, not disposable cache data.
/// Transfer it only to one already-bound event with the same exact invitation
/// identity, inside the caller's reconciliation transaction.
fn prepare_graph_duplicate_for_retirement(
    conn: &rusqlite::Connection,
    source_id: &str,
    successor_id: &str,
    provider_organizer: Option<&str>,
    provider_invite_visible: bool,
) -> Result<()> {
    let source: (
        String,
        Option<String>,
        Option<String>,
        String,
        String,
        String,
        Option<String>,
    ) = conn.query_row(
        "SELECT account_id, uid, organizer_email, start_time, end_time,
                    recurrence_kind, manually_managed_at
             FROM calendar_events WHERE id = ?1",
        [source_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )?;
    if let Some(marked_at) = &source.6 {
        let target: (
            String,
            Option<String>,
            Option<String>,
            String,
            String,
            String,
        ) = conn.query_row(
            "SELECT account_id, uid, organizer_email, start_time, end_time,
                        recurrence_kind
                 FROM calendar_events WHERE id = ?1",
            [successor_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        if source_id == successor_id
            || source.0 != target.0
            || source.1.as_deref().is_none_or(str::is_empty)
            || source.1 != target.1
            || source.2.as_deref().is_none_or(str::is_empty)
            || !source
                .2
                .as_ref()
                .zip(target.2.as_ref())
                .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
            || !provider_organizer.is_some_and(|organizer| {
                target
                    .2
                    .as_ref()
                    .is_some_and(|value| value.eq_ignore_ascii_case(organizer))
            })
            || source.3 != target.3
            || source.4 != target.4
            || source.5 != target.5
            || !provider_invite_visible
            || !is_visible_invitation(conn, source_id)?
            || !is_visible_invitation(conn, successor_id)?
        {
            return Err(Error::Sync(format!(
                "Graph event {source_id:?} has a local invite acknowledgement but no matching verified successor"
            )));
        }
        conn.execute(
            "UPDATE calendar_events
             SET manually_managed_at = COALESCE(manually_managed_at, ?1)
             WHERE id = ?2 AND account_id = ?3",
            rusqlite::params![marked_at, successor_id, source.0],
        )?;
        conn.execute(
            "UPDATE calendar_events SET manually_managed_at = NULL WHERE id = ?1",
            [source_id],
        )?;
    }
    ensure_disposable_graph_duplicate(conn, source_id)
}

fn is_visible_invitation(conn: &rusqlite::Connection, event_id: &str) -> Result<bool> {
    let (attendees, status, organizer, account_email): (
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    ) = conn.query_row(
        "SELECT event.attendees_json, event.my_status, event.organizer_email,
                account.email
         FROM calendar_events event
         JOIN accounts account ON account.id = event.account_id
         WHERE event.id = ?1",
        [event_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    Ok(visible_invite_fields(
        &account_email,
        organizer.as_deref(),
        attendees.as_deref(),
        status.as_deref(),
    ))
}

fn visible_invite_fields(
    account_email: &str,
    organizer: Option<&str>,
    attendees: Option<&str>,
    status: Option<&str>,
) -> bool {
    if organizer.is_none_or(str::is_empty)
        || organizer.is_some_and(|value| value.eq_ignore_ascii_case(account_email))
    {
        return false;
    }
    if status.is_some() {
        return true;
    }
    attendees
        .and_then(|json| serde_json::from_str::<Vec<crate::calendar::Attendee>>(json).ok())
        .is_some_and(|people| {
            people
                .iter()
                .any(|person| person.email.eq_ignore_ascii_case(account_email))
        })
}

/// A provider rewrite cannot carry a local acknowledgement to a different
/// invitation merely because an opaque event ID, UID, or interval matched.
fn ensure_marked_graph_identity(
    conn: &rusqlite::Connection,
    event_id: &str,
    remote_id: &str,
    uid: Option<&str>,
    start: Option<&str>,
    end: Option<&str>,
    kind: Option<RecurrenceKind>,
    organizer: Option<&str>,
    provider_invite_visible: bool,
) -> Result<()> {
    let row: (
        Option<String>,
        Option<String>,
        String,
        String,
        String,
        bool,
        Option<String>,
    ) = conn.query_row(
        "SELECT uid, organizer_email, start_time, end_time,
                recurrence_kind, manually_managed_at IS NOT NULL, remote_id
         FROM calendar_events WHERE id = ?1",
        [event_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )?;
    if row.5
        && (!provider_invite_visible
            || row.0.as_deref().is_none_or(str::is_empty)
            || row.0.as_deref() != uid
            || row.1.as_deref().is_none_or(str::is_empty)
            || !row
                .1
                .as_deref()
                .zip(organizer)
                .is_some_and(|(stored, provider)| stored.eq_ignore_ascii_case(provider))
            || (row.6.as_deref() != Some(remote_id)
                && (Some(row.2.as_str()) != start || Some(row.3.as_str()) != end))
            || kind.map(RecurrenceKind::as_str) != Some(row.4.as_str()))
    {
        return Err(Error::Sync(format!(
            "Graph event {event_id:?} has a local invite acknowledgement but the provider did not prove the same invitation"
        )));
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
    ensure_graph_cache_retirable(conn, event_id, false)
}

/// A complete Graph view/inventory can retire provider cache rows absent
/// from that source. Only the local "handled" flag may be discarded here;
/// all other locally owned state still blocks retirement.
fn ensure_graph_absent_cache_retirable(conn: &rusqlite::Connection, event_id: &str) -> Result<()> {
    ensure_graph_cache_retirable(conn, event_id, true)
}

fn ensure_graph_cache_retirable(
    conn: &rusqlite::Connection,
    event_id: &str,
    discard_handled_flag: bool,
) -> Result<()> {
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
                        OR (?2 = 0 AND manually_managed_at IS NOT NULL)
                      OR source_message_id IS NOT NULL
                      OR ical_data IS NOT NULL
                 )
         )",
        rusqlite::params![event_id, discard_handled_flag],
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
    use super::{
        graph_event_identity_rows, CalendarBackend, CalendarBackendCtx, GraphCalendarBackend,
        GraphSyncIdentityScope,
    };
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
    async fn failed_calendar_prevents_any_calendar_from_publishing() {
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
        assert_eq!(captured.await.unwrap().len(), 2);
        let conn = db.reader();
        assert!(db::calendar::get_event(&conn, "cancelled").is_ok());
        assert_eq!(pending_cleanup_count(&conn), 0);
        assert_eq!(
            db::calendar::list_calendars(&conn, "acc1").unwrap().len(),
            1
        );
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
    async fn stale_graph_calendar_with_only_a_handled_cache_event_can_retire() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            conn.execute(
                "INSERT INTO calendars
                    (id, account_id, name, remote_id, is_subscribed)
                 VALUES ('old-calendar', 'acc1', 'Old', 'old-provider-id', 1)",
                [],
            )
            .unwrap();
            cache_event(&conn, "old-cache", Some("old-event-id"));
            conn.execute(
                "UPDATE calendar_events
                 SET calendar_id = 'old-calendar',
                     recurrence_kind = 'standalone',
                     manually_managed_at = '2026-09-22 13:40:20'
                 WHERE id = 'old-cache'",
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
        assert!(db::calendar::get_calendar(&conn, "old-calendar").is_err());
        assert!(db::calendar::get_event(&conn, "old-cache").is_err());
        assert_eq!(
            db::calendar::list_calendars(&conn, "acc1").unwrap().len(),
            1
        );
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

    fn stale_owned_calendar(conn: &rusqlite::Connection) {
        conn.execute(
            "INSERT INTO calendars
                (id, account_id, name, remote_id, is_subscribed)
             VALUES ('old-calendar', 'acc1', 'Calendar', 'legacy-id', 1)",
            [],
        )
        .unwrap();
        cache_event(conn, "old-owner", Some("legacy-master"));
        cache_event(conn, "old-member", None);
        conn.execute(
            "UPDATE calendar_events SET calendar_id = 'old-calendar',
                    recurrence_kind = 'series', recurrence_rule = 'FREQ=DAILY'
             WHERE id = 'old-owner'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE calendar_events SET calendar_id = 'old-calendar',
                    recurrence_kind = 'occurrence',
                    start_time = '2026-06-29T12:15:00Z',
                    end_time = '2026-06-29T12:45:00Z'
             WHERE id = 'old-member'",
            [],
        )
        .unwrap();
        let revision = db::calendar_revision::get(conn, "old-owner").unwrap();
        conn.execute(
            "INSERT INTO calendar_action_sets(event_id, data, revision)
             VALUES ('old-owner', '{}', ?1)",
            [revision],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendar_action_members(event_id, owner_event_id)
             VALUES ('old-member', 'old-owner')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendar_action_addresses
                (account_id, calendar_id, remote_id, owner_event_id)
             VALUES ('acc1', 'old-calendar', 'legacy-master', 'old-owner')",
            [],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn stale_action_owned_calendar_archives_cached_series_without_rebinding() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            stale_owned_calendar(&conn);
            conn.execute(
                "INSERT INTO service_bindings
                    (id, account_id, service, protocol)
                 VALUES ('graph-calendar', 'acc1', 'calendar', 'graph')",
                [],
            )
            .unwrap();
            cache_event(&conn, "current-copy", Some("immutable-occurrence"));
            conn.execute(
                "UPDATE calendar_events SET uid = 'uid-old-member@example.test',
                        recurrence_kind = 'occurrence',
                        start_time = '2026-06-29T12:15:00Z',
                        end_time = '2026-06-29T12:45:00Z'
                 WHERE id = 'current-copy'",
                [],
            )
            .unwrap();
        }
        let current_occurrence = remote_event(
            "immutable-occurrence",
            &json!({
                "iCalUId": "uid-old-member@example.test",
                "type": "occurrence",
                "seriesMasterId": "current-master",
                "recurrence": null,
                "originalStart": "2026-06-29T12:15:00Z",
                "start": {"dateTime": "2026-06-29T12:15:00", "timeZone": "UTC"},
                "end": {"dateTime": "2026-06-29T12:45:00", "timeZone": "UTC"}
            }),
        );
        let (root, captured) = serve_responses(vec![
            (200, primary_calendar()),
            (200, json!({"value": [current_occurrence.clone()]})),
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
        assert!(db::calendar::get_calendar(&conn, "old-calendar").is_err());
        assert_eq!(
            db::calendar::list_calendars(&conn, "acc1").unwrap().len(),
            1
        );
        assert!(db::calendar::get_event(&conn, "old-owner").is_ok());
        assert!(db::calendar::get_event(&conn, "old-member").is_ok());
        assert!(db::calendar::get_event(&conn, "current-copy").is_ok());
        let owner: String = conn
            .query_row(
                "SELECT owner_event_id FROM calendar_action_members
                 WHERE event_id = 'old-member'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner, "old-owner");
        let data: String = conn
            .query_row(
                "SELECT data FROM calendar_action_sets WHERE event_id = 'old-owner'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(data, "{}");
        let archived = db::calendar::list_archived_graph_calendars(&conn, "acc1").unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].id, "old-calendar");
        assert_eq!(archived[0].retained_event_count, 2);
        assert_eq!(archived[0].replay_address_count, 1);
        let scope = GraphSyncIdentityScope {
            calendar_ids: ["primary".to_string()].into(),
            event_ids: Default::default(),
        };
        let candidates = graph_event_identity_rows(
            &conn,
            "acc1",
            "cal1",
            "new-master",
            Some("uid-old-owner@example.test"),
            Some("2026-09-14T09:00:00Z"),
            Some("2026-09-14T10:00:00Z"),
            Some(RecurrenceKind::Series),
            &scope,
        )
        .unwrap();
        assert!(
            candidates.is_empty(),
            "archived owner is not a migration candidate"
        );
        drop(conn);

        let (root, captured) = serve_responses(vec![
            (200, primary_calendar()),
            (200, json!({"value": [current_occurrence]})),
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
        assert!(db::calendar::get_event(&db.reader(), "old-member").is_ok());
    }

    #[tokio::test]
    async fn stale_action_owned_calendar_with_pending_operation_stays_visible() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            stale_owned_calendar(&conn);
            conn.execute(
                "INSERT INTO calendar_action_operations
                    (operation_id, account_id, event_id, data)
                 VALUES ('pending', 'acc1', 'old-owner', '{}')",
                [],
            )
            .unwrap();
        }
        let (root, _) =
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
        assert!(error.to_string().contains("pending calendar actions"));
        assert!(db::calendar::get_calendar(&db.reader(), "old-calendar").is_ok());
    }

    #[tokio::test]
    async fn stale_calendar_with_unpublished_action_creation_stays_visible() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            conn.execute(
                "INSERT INTO calendars (id, account_id, name, remote_id)
                 VALUES ('old-calendar', 'acc1', 'Old', 'legacy-id')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO calendar_action_creations
                    (operation_id, account_id, event_id, data)
                 VALUES ('pending', 'acc1', 'not-published',
                         '{\"destination\":{\"calendar_id\":\"old-calendar\"}}')",
                [],
            )
            .unwrap();
        }
        let (root, _) =
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
        assert!(error.to_string().contains("pending calendar actions"));
        assert!(db::calendar::get_calendar(&db.reader(), "old-calendar").is_ok());
    }

    #[tokio::test]
    async fn stale_action_owner_with_current_calendar_member_fails_closed() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            stale_owned_calendar(&conn);
            conn.execute(
                "UPDATE calendar_events SET calendar_id = 'cal1'
                 WHERE id = 'old-member'",
                [],
            )
            .unwrap();
        }
        let (root, _) =
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
        assert!(error.to_string().contains("ownership across calendars"));
        assert!(db::calendar::get_calendar(&db.reader(), "old-calendar").is_ok());
    }

    #[tokio::test]
    async fn stale_empty_calendar_archives_action_addresses_and_reactivates_by_exact_id() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            conn.execute(
                "INSERT INTO calendars
                    (id, account_id, name, remote_id, is_subscribed, color)
                 VALUES ('legacy-calendar', 'acc1', 'Old calendar', 'legacy-id', 1, '#123456')",
                [],
            )
            .unwrap();
            cache_event(&conn, "current-owner", Some("current-owner"));
            conn.execute(
                "UPDATE calendar_events
                 SET recurrence_kind = 'series', recurrence_rule = 'FREQ=DAILY'
                 WHERE id = 'current-owner'",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO calendar_action_addresses
                    (account_id, calendar_id, remote_id, owner_event_id, retired)
                 VALUES ('acc1', 'legacy-calendar', 'old-resource', 'current-owner', 1)",
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
        {
            let conn = db.reader();
            assert!(db::calendar::get_calendar(&conn, "legacy-calendar").is_err());
            assert_eq!(
                db::calendar::list_calendars(&conn, "acc1").unwrap().len(),
                1
            );
            let retired: bool = conn
                .query_row(
                    "SELECT retired FROM calendar_action_addresses
                 WHERE calendar_id = 'legacy-calendar' AND remote_id = 'old-resource'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(retired);
        }

        {
            let conn = db.writer().await;
            conn.execute(
                "UPDATE calendars SET archived_acknowledged_at = CURRENT_TIMESTAMP
                 WHERE id = 'legacy-calendar'",
                [],
            )
            .unwrap();
        }

        let (root, captured) = serve_responses(vec![
            (
                200,
                json!({"value": [
                    {"id":"primary", "name":"Calendar", "isDefaultCalendar":true},
                    {"id":"legacy-id", "name":"Old calendar"}
                ]}),
            ),
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
        let restored = db::calendar::get_calendar(&conn, "legacy-calendar").unwrap();
        assert_eq!(restored.remote_id.as_deref(), Some("legacy-id"));
        assert_eq!(restored.color, "#123456");
        assert!(restored.is_subscribed);
        let acknowledged_at: Option<String> = conn
            .query_row(
                "SELECT archived_acknowledged_at FROM calendars
                 WHERE id = 'legacy-calendar'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(acknowledged_at.is_none());
        assert_eq!(
            db::calendar::list_calendars(&conn, "acc1").unwrap().len(),
            2
        );
        assert!(conn
            .query_row(
                "SELECT retired FROM calendar_action_addresses
             WHERE calendar_id = 'legacy-calendar' AND remote_id = 'old-resource'",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap());
    }

    #[tokio::test]
    async fn unpublished_local_event_is_not_moved_by_matching_calendar_name() {
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

        assert!(error.to_string().contains("destination cannot be inferred"));
        assert_eq!(captured.await.unwrap().len(), 2);
        let conn = db.reader();
        assert!(db::calendar::get_calendar(&conn, "legacy-calendar").is_ok());
        let local = db::calendar::get_event(&conn, "local-draft").unwrap();
        assert_eq!(local.calendar_id, "legacy-calendar");
        assert_eq!(local.source_message_id.as_deref(), Some("source-message"));
        assert_eq!(local.ical_data.as_deref(), Some("BEGIN:VCALENDAR"));
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

        assert!(error.to_string().contains("destination cannot be inferred"));
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

        assert!(error.to_string().contains("destination cannot be inferred"));
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
    async fn different_current_calendar_cannot_rehome_a_cached_uid_match() {
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
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, secondary);
        assert_ne!(rows[0].0, "stale-local-row");
        assert!(db::calendar::get_event(&conn, "stale-local-row").is_err());
        assert!(db::calendar::get_event(&conn, "same-looking-other").is_ok());
    }

    #[tokio::test]
    async fn same_invite_in_two_current_calendars_is_not_a_cache_duplicate() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            conn.execute(
                "INSERT INTO calendars
                    (id, account_id, name, remote_id, is_subscribed)
                 VALUES ('second-local', 'acc1', 'Second', 'secondary', 1)",
                [],
            )
            .unwrap();
            cache_event(&conn, "first-copy", Some("provider-first"));
            conn.execute(
                "UPDATE calendar_events
                 SET uid = 'shared-invite@example.test',
                     recurrence_kind = 'standalone'
                 WHERE id = 'first-copy'",
                [],
            )
            .unwrap();
        }
        let mut first = remote_event(
            "provider-first",
            &json!({
                "type":"singleInstance", "seriesMasterId":null, "recurrence":null,
            }),
        );
        first["iCalUId"] = json!("shared-invite@example.test");
        let mut second = remote_event(
            "provider-second",
            &json!({
                "type":"singleInstance", "seriesMasterId":null, "recurrence":null,
            }),
        );
        second["iCalUId"] = json!("shared-invite@example.test");
        let (root, captured) = serve_responses(vec![
            (
                200,
                json!({"value":[
                    {"id":"primary", "name":"Calendar"},
                    {"id":"secondary", "name":"Second"}
                ]}),
            ),
            (200, json!({"value":[first]})),
            (200, json!({"value":[second]})),
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
        let rows: Vec<(String, String)> = conn
            .prepare(
                "SELECT id, calendar_id FROM calendar_events
             WHERE account_id = 'acc1' AND uid = 'shared-invite@example.test'
             ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .any(|(id, cal)| id == "first-copy" && cal == "cal1"));
        assert!(rows.iter().any(|(_, cal)| cal == "second-local"));
    }

    #[tokio::test]
    async fn two_current_graph_events_in_one_calendar_do_not_consume_each_other() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_event(&conn, "first-copy", Some("provider-first"));
            conn.execute(
                "UPDATE calendar_events
                 SET uid = 'shared-invite@example.test',
                     recurrence_kind = 'standalone'
                 WHERE id = 'first-copy'",
                [],
            )
            .unwrap();
        }
        let standalone = json!({
            "type":"singleInstance", "seriesMasterId":null, "recurrence":null
        });
        let mut first = remote_event("provider-first", &standalone);
        first["iCalUId"] = json!("shared-invite@example.test");
        let mut second = remote_event("provider-second", &standalone);
        second["iCalUId"] = json!("shared-invite@example.test");
        let (root, captured) = serve_responses(vec![
            (200, primary_calendar()),
            (200, json!({"value":[first, second]})),
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
        let ids: Vec<String> = conn
            .prepare(
                "SELECT remote_id FROM calendar_events
             WHERE account_id = 'acc1' AND uid = 'shared-invite@example.test'
             ORDER BY remote_id",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, ["provider-first", "provider-second"]);
    }

    #[tokio::test]
    async fn ambiguous_current_copies_cannot_claim_one_unpublished_local_invite() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_event(&conn, "local-copy", None);
            conn.execute(
                "UPDATE calendar_events
                 SET uid = 'shared-invite@example.test',
                     recurrence_kind = 'standalone'
                 WHERE id = 'local-copy'",
                [],
            )
            .unwrap();
        }
        let standalone = json!({
            "type":"singleInstance", "seriesMasterId":null, "recurrence":null
        });
        let mut first = remote_event("provider-first", &standalone);
        first["iCalUId"] = json!("shared-invite@example.test");
        let mut second = remote_event("provider-second", &standalone);
        second["iCalUId"] = json!("shared-invite@example.test");
        second["end"]["dateTime"] = json!("2026-09-14T11:00:00");
        let (root, captured) = serve_responses(vec![
            (200, primary_calendar()),
            (200, json!({"value":[first, second]})),
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
        assert!(error.to_string().contains("ambiguous cache merge"));
        assert_eq!(captured.await.unwrap().len(), 2);
        let conn = db.reader();
        assert!(db::calendar::get_event(&conn, "local-copy")
            .unwrap()
            .remote_id
            .is_none());
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM calendar_events WHERE remote_id IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn same_immutable_id_in_two_current_calendars_rejects_inventory() {
        let (_dir, db) = setup_db().await;
        let standalone = json!({
            "type":"singleInstance", "seriesMasterId":null, "recurrence":null
        });
        let (root, captured) = serve_responses(vec![
            (
                200,
                json!({"value":[
                    {"id":"primary", "name":"Calendar"},
                    {"id":"secondary", "name":"Second"}
                ]}),
            ),
            (200, json!({"value":[remote_event("same-id", &standalone)]})),
            (200, json!({"value":[remote_event("same-id", &standalone)]})),
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
        assert!(error.to_string().contains("two current calendars"));
        assert_eq!(captured.await.unwrap().len(), 3);
        let conn = db.reader();
        assert!(db::calendar::list_calendars(&conn, "acc1").unwrap().len() == 1);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM calendar_events WHERE remote_id = 'same-id'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
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
    async fn action_owned_child_refreshes_rsvp_without_overwriting_local_pending_intent() {
        for (pending, expected) in [(None, "accepted"), (Some("declined"), "declined")] {
            let (_dir, db) = setup_db().await;
            {
                let conn = db.writer().await;
                cache_action_owned_occurrence(&conn, false);
                conn.execute(
                    "UPDATE calendar_events
                     SET organizer_email = 'organizer@example.test',
                         my_status = ?1, pending_rsvp_status = ?2
                     WHERE id = 'projected-member'",
                    rusqlite::params![
                        if pending.is_some() {
                            "declined"
                        } else {
                            "needs-action"
                        },
                        pending
                    ],
                )
                .unwrap();
            }
            let mut metadata = occurrence_metadata();
            metadata["organizer"] = json!({
                "emailAddress":{"address":"organizer@example.test"}
            });
            metadata["responseStatus"] = json!({"response":"accepted"});
            let (root, captured) = serve_responses(vec![
                (200, primary_calendar()),
                (
                    200,
                    json!({"value": [remote_event(
                        "immutable-occurrence", &metadata
                    )]}),
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
            let saved: (Option<String>, Option<String>) = conn
                .query_row(
                    "SELECT my_status, pending_rsvp_status FROM calendar_events
                 WHERE id = 'projected-member'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(saved.0.as_deref(), Some(expected));
            assert_eq!(saved.1.as_deref(), pending);
        }
    }

    #[tokio::test]
    async fn tombstone_for_action_owned_child_invalidates_owner_without_deleting_member() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_action_owned_occurrence(&conn, false);
            conn.execute(
                "UPDATE calendar_events SET remote_id = 'immutable-occurrence'
                 WHERE id = 'stale-provider-cache'",
                [],
            )
            .unwrap();
        }
        let (root, captured) = serve_responses(vec![
            (200, primary_calendar()),
            (
                200,
                json!({"value":[
                    {"id":"immutable-occurrence", "isCancelled":true}
                ]}),
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
        assert!(db::calendar::get_event(&conn, "series-owner").is_ok());
        assert!(db::calendar::get_event(&conn, "projected-member").is_ok());
        assert!(db::calendar::get_event(&conn, "stale-provider-cache").is_err());
        assert!(conn
            .query_row(
                "SELECT dirty FROM calendar_action_sets WHERE event_id = 'series-owner'",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap());
    }

    #[tokio::test]
    async fn protected_action_owned_tombstone_rolls_back_without_losing_acknowledgement() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_action_owned_occurrence(&conn, false);
            conn.execute(
                "UPDATE calendar_events
                 SET remote_id = 'immutable-occurrence',
                     organizer_email = 'organizer@example.test',
                     manually_managed_at = '2026-09-22 13:40:07'
                 WHERE id = 'stale-provider-cache'",
                [],
            )
            .unwrap();
        }
        let (root, captured) = serve_responses(vec![
            (200, primary_calendar()),
            (
                200,
                json!({"value":[
                    {"id":"immutable-occurrence", "isCancelled":true}
                ]}),
            ),
        ])
        .await;
        assert!(GraphCalendarBackend
            .sync(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services(&root)
                },
                &account("calendar", "graph"),
            )
            .await
            .is_err());
        assert_eq!(captured.await.unwrap().len(), 2);
        let conn = db.reader();
        assert_eq!(
            conn.query_row(
                "SELECT manually_managed_at FROM calendar_events
             WHERE id = 'stale-provider-cache'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "2026-09-22 13:40:07"
        );
        assert!(!conn
            .query_row(
                "SELECT dirty FROM calendar_action_sets WHERE event_id = 'series-owner'",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap());
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
    async fn manually_handled_action_owned_duplicate_preserves_its_acknowledgement() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_action_owned_occurrence(&conn, false);
            conn.execute(
                "UPDATE calendar_events
                 SET manually_managed_at = '2026-09-22 13:40:07',
                     organizer_email = 'organizer@example.test',
                     my_status = 'needs-action'
                 WHERE id = 'stale-provider-cache'",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE calendar_events
                 SET organizer_email = 'organizer@example.test',
                     my_status = 'needs-action'
                 WHERE id = 'projected-member'",
                [],
            )
            .unwrap();
        }
        let mut occurrence = occurrence_metadata();
        occurrence["organizer"] = json!({
            "emailAddress": {"address": "organizer@example.test"}
        });
        occurrence["responseStatus"] = json!({"response":"accepted"});
        let (root, captured) = serve_responses(vec![
            (200, primary_calendar()),
            (
                200,
                json!({"value": [remote_event(
                    "immutable-occurrence", &occurrence
                )]}),
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
        let marked: String = conn
            .query_row(
                "SELECT manually_managed_at FROM calendar_events
                 WHERE id = 'projected-member'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marked, "2026-09-22 13:40:07");
        assert_eq!(
            db::calendar::get_event(&conn, "projected-member")
                .unwrap()
                .my_status
                .as_deref(),
            Some("accepted")
        );
        let invites =
            db::calendar::list_invites(&conn, "acc1", "test@example.com", "2026-09-01").unwrap();
        assert!(invites.iter().any(|invite| {
            invite.event.id == "projected-member" && invite.manually_managed_at.is_some()
        }));
    }

    #[tokio::test]
    async fn marked_child_without_same_organizer_cannot_be_retired() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_action_owned_occurrence(&conn, false);
            conn.execute(
                "UPDATE calendar_events
                 SET manually_managed_at = '2026-09-22 13:40:07',
                     organizer_email = 'first@example.test',
                     my_status = 'needs-action'
                 WHERE id = 'stale-provider-cache'",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE calendar_events
                 SET organizer_email = 'another@example.test',
                     my_status = 'needs-action'
                 WHERE id = 'projected-member'",
                [],
            )
            .unwrap();
        }
        let mut metadata = occurrence_metadata();
        metadata["organizer"] = json!({
            "emailAddress": {"address": "first@example.test"}
        });
        metadata["responseStatus"] = json!({"response":"accepted"});
        let (root, captured) = serve_responses(vec![
            (200, primary_calendar()),
            (
                200,
                json!({"value": [remote_event(
                    "immutable-occurrence", &metadata
                )]}),
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
        assert!(error
            .to_string()
            .contains("no unique verified series member"));
        assert_eq!(captured.await.unwrap().len(), 2);
        let conn = db.reader();
        let marker: String = conn
            .query_row(
                "SELECT manually_managed_at FROM calendar_events
             WHERE id = 'stale-provider-cache'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker, "2026-09-22 13:40:07");
        assert!(conn
            .query_row(
                "SELECT manually_managed_at FROM calendar_events
             WHERE id = 'projected-member'",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap()
            .is_none());
        assert!(!conn
            .query_row(
                "SELECT dirty FROM calendar_action_sets WHERE event_id = 'series-owner'",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap());
    }

    #[tokio::test]
    async fn marked_action_owned_child_rejects_changed_provider_uid_at_same_remote_id() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_action_owned_occurrence(&conn, false);
            conn.execute(
                "UPDATE calendar_events
                 SET remote_id = 'immutable-occurrence',
                      organizer_email = 'organizer@example.test',
                      my_status = 'needs-action',
                      manually_managed_at = '2026-09-22 13:40:07'
                 WHERE id = 'stale-provider-cache'",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE calendar_events
                 SET organizer_email = 'organizer@example.test',
                     my_status = 'needs-action'
                 WHERE id = 'projected-member'",
                [],
            )
            .unwrap();
        }
        let mut event = remote_event("immutable-occurrence", &occurrence_metadata());
        event["iCalUId"] = json!("another-instance@example.test");
        event["organizer"] = json!({
            "emailAddress": {"address": "organizer@example.test"}
        });
        event["responseStatus"] = json!({"response":"accepted"});
        let (root, captured) = serve_responses(vec![
            (200, primary_calendar()),
            (200, json!({"value":[event]})),
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
            .contains("did not prove the same invitation"));
        assert_eq!(captured.await.unwrap().len(), 2);
        let conn = db.reader();
        assert_eq!(
            conn.query_row(
                "SELECT manually_managed_at FROM calendar_events
             WHERE id = 'stale-provider-cache'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "2026-09-22 13:40:07"
        );
    }

    #[tokio::test]
    async fn marked_sole_survivor_cannot_change_organizer_on_provider_rehome() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_event(&conn, "legacy", Some("legacy-event-id"));
            conn.execute(
                "UPDATE calendar_events
                 SET uid = 'uid-immutable-event@example.test',
                      recurrence_kind = 'standalone',
                      organizer_email = 'old@example.test',
                      my_status = 'needs-action',
                      manually_managed_at = '2026-09-22 13:40:07'
                 WHERE id = 'legacy'",
                [],
            )
            .unwrap();
        }
        let (root, captured) = serve_responses(vec![
            (200, primary_calendar()),
            (
                200,
                json!({"value": [remote_event(
                    "immutable-event", &json!({
                        "type":"singleInstance", "seriesMasterId":null,
                         "recurrence":null,
                         "responseStatus":{"response":"accepted"},
                         "organizer":{"emailAddress":{"address":"new@example.test"}}
                    })
                )]}),
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
        assert!(error
            .to_string()
            .contains("did not prove the same invitation"));
        assert_eq!(captured.await.unwrap().len(), 2);
        let conn = db.reader();
        let protected: (String, String, String) = conn
            .query_row(
                "SELECT remote_id, organizer_email, manually_managed_at
             FROM calendar_events WHERE id = 'legacy'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(protected.0, "legacy-event-id");
        assert_eq!(protected.1, "old@example.test");
        assert_eq!(protected.2, "2026-09-22 13:40:07");
    }

    #[tokio::test]
    async fn same_immutable_id_can_reschedule_an_acknowledged_invite() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_event(&conn, "marked", Some("immutable-event"));
            conn.execute(
                "UPDATE calendar_events
                 SET uid = 'uid-immutable-event@example.test',
                     recurrence_kind = 'standalone',
                     organizer_email = 'organizer@example.test',
                     my_status = 'needs-action',
                     manually_managed_at = '2026-09-22 13:40:07'
                 WHERE id = 'marked'",
                [],
            )
            .unwrap();
        }
        let mut moved = remote_event(
            "immutable-event",
            &json!({
                "type":"singleInstance", "seriesMasterId":null,
                "recurrence":null,
                "organizer":{"emailAddress":{"address":"organizer@example.test"}},
                "responseStatus":{"response":"accepted"}
            }),
        );
        moved["start"]["dateTime"] = json!("2026-09-15T09:00:00");
        moved["end"]["dateTime"] = json!("2026-09-15T10:00:00");
        let (root, captured) = serve_responses(vec![
            (200, primary_calendar()),
            (200, json!({"value":[moved]})),
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
        let stored: (String, String) = conn
            .query_row(
                "SELECT start_time, manually_managed_at
             FROM calendar_events WHERE id = 'marked'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored.0, "2026-09-15T09:00:00Z");
        assert_eq!(stored.1, "2026-09-22 13:40:07");
    }

    #[tokio::test]
    async fn marked_legacy_cache_retires_with_its_calendar_after_complete_sync() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            conn.execute(
                "INSERT INTO calendars (id, account_id, name, remote_id, is_subscribed)
                 VALUES ('legacy-calendar', 'acc1', 'Calendar', 'legacy-id', 1)",
                [],
            )
            .unwrap();
            cache_event(&conn, "old-marked", Some("legacy-event-id"));
            cache_event(&conn, "current", Some("immutable-event"));
            conn.execute(
                "UPDATE calendar_events SET
                    calendar_id = 'legacy-calendar',
                     uid = 'uid-immutable-event@example.test',
                      recurrence_kind = 'standalone',
                      organizer_email = 'organizer@example.test',
                      my_status = 'needs-action',
                      manually_managed_at = '2026-09-22 13:40:07'
                 WHERE id = 'old-marked'",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE calendar_events SET
                    uid = 'uid-immutable-event@example.test',
                    organizer_email = 'organizer@example.test',
                    my_status = 'needs-action',
                    recurrence_kind = 'standalone'
                 WHERE id = 'current'",
                [],
            )
            .unwrap();
        }
        let (root, captured) = serve_responses(vec![
            (200, primary_calendar()),
            (
                200,
                json!({"value": [remote_event(
                    "immutable-event",
                    &json!({"type":"singleInstance", "seriesMasterId":null,
                             "recurrence":null,
                             "responseStatus":{"response":"accepted"},
                             "organizer":{"emailAddress":{
                                "address":"organizer@example.test"}}})
                )]}),
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
        assert!(db::calendar::get_calendar(&conn, "legacy-calendar").is_err());
        assert!(db::calendar::get_event(&conn, "old-marked").is_err());
        assert!(db::calendar::get_event(&conn, "current").is_ok());
        assert_eq!(
            conn.query_row(
                "SELECT manually_managed_at FROM calendar_events
                 WHERE id = 'current'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "2026-09-22 13:40:07"
        );
    }

    #[tokio::test]
    async fn protected_cache_failure_does_not_publish_unfamiliar_graph_calendars() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_action_owned_occurrence(&conn, true);
        }
        let (root, captured) = serve_responses(vec![
            (
                200,
                json!({"value": [
                    {"id":"primary", "name":"Calendar"},
                    {"id":"new-calendar", "name":"Calendar"}
                ]}),
            ),
            (
                200,
                json!({"value": [remote_event(
                    "immutable-occurrence", &occurrence_metadata()
                )]}),
            ),
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
        assert!(error.to_string().contains("local state"));
        assert_eq!(captured.await.unwrap().len(), 3);
        let conn = db.reader();
        assert_eq!(
            db::calendar::list_calendars(&conn, "acc1").unwrap().len(),
            1
        );
        assert!(db::calendar::get_event(&conn, "stale-provider-cache").is_ok());
        assert!(db::meet_meetings::get(&conn, "stale-provider-cache")
            .unwrap()
            .is_some());
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
                None,
                false,
                &super::GraphSyncIdentityScope {
                    calendar_ids: std::collections::HashSet::from(["secondary".to_string()]),
                    event_ids: std::collections::HashSet::from(["immutable-event".to_string()]),
                },
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
    async fn complete_view_can_retire_an_absent_cache_with_only_a_handled_flag() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_event(&conn, "handled-cache", Some("missing-provider-id"));
            conn.execute(
                "UPDATE calendar_events
                 SET recurrence_kind = 'standalone',
                     manually_managed_at = '2026-09-22 13:40:20'
                 WHERE id = 'handled-cache'",
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
        assert!(db::calendar::get_event(&db.reader(), "handled-cache").is_err());
    }

    #[tokio::test]
    async fn handled_cache_with_message_provenance_still_blocks_absence_cleanup() {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_event(&conn, "handled-cache", Some("missing-provider-id"));
            conn.execute(
                "UPDATE calendar_events
                 SET recurrence_kind = 'standalone',
                     manually_managed_at = '2026-09-22 13:40:20',
                     source_message_id = 'message-1'
                 WHERE id = 'handled-cache'",
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
        assert_eq!(
            conn.query_row(
                "SELECT manually_managed_at FROM calendar_events
             WHERE id = 'handled-cache'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "2026-09-22 13:40:20"
        );
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
