//! Microsoft Graph calendar operations and an upstream-owned resource mirror.

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
};
use crate::provider::GraphTokenPurpose;

use super::{
    BusyPeriod, CalendarBackend, CalendarBackendCtx, CalendarCapability, InviteReplyDelivery,
    ParticipantSchedule, ParticipantScheduleRequest, PushedEvent, RemoteOccurrenceUpdate,
    RemoteOccurrenceUpdateOutcome, RemoteRsvpOutcome, RemoteRsvpPolicy, RemoteRsvpRequest,
    RoomAvailability, RoomAvailabilityRequest, RoomSuggestion, VerifiedOccurrenceMembership,
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
    fn recurring_import_fidelity(&self) -> super::RecurringImportFidelity {
        super::RecurringImportFidelity::PatternedRecurrence
    }
    fn invite_reply_delivery(&self) -> InviteReplyDelivery {
        InviteReplyDelivery::Provider
    }
    fn remote_rsvp_policy(&self) -> RemoteRsvpPolicy {
        RemoteRsvpPolicy::RequiredBeforeLocal
    }

    async fn fetch_event_set(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        event: &CalendarEvent,
        calendar: &str,
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
            .fetch_calendar_event_set(calendar, id, event)
            .await
    }

    async fn fetch_event_set_for_repair(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        event: &CalendarEvent,
        calendar: &str,
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
            .fetch_calendar_event_set_for_repair(calendar, id, event)
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
        calendar: &str,
        desired: &CalendarEventSet,
        operation: &str,
    ) -> Result<CalendarEventSet> {
        ctx.services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?
            .create_calendar_event_set(&account.id, calendar, desired, operation)
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
        _: &CalendarBackendCtx<'_>,
        _: &AccountFull,
        _: &CalendarEventSet,
        _: &str,
    ) -> Result<CalendarCapability<CalendarEventSet>> {
        // Graph v1.0 has no event move action.
        Ok(CalendarCapability::Unsupported)
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
        let id = client.find_event_by_ical_uid(&request.uid).await?.ok_or_else(|| Error::Other(
            "This invitation isn't on your Outlook calendar yet. Sync the calendar and try again.".into()))?;
        client
            .rsvp_event(&id, request.response.as_str(), "")
            .await?;
        Ok(CalendarCapability::Supported(RemoteRsvpOutcome {
            remote_id: Some(id),
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
                log::debug!("Room credentials unavailable: {error}");
                return Ok(CalendarCapability::Supported(Vec::new()));
            }
        };
        Ok(CalendarCapability::Supported(
            client
                .list_rooms()
                .await?
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
                log::debug!("Room credentials unavailable: {error}");
                return Ok(CalendarCapability::Supported(RoomAvailability {
                    state: "unknown".into(),
                    busy_start: None,
                    busy_end: None,
                }));
            }
        };
        let result = client
            .get_room_availability(
                &request.room_address,
                &request.start_time,
                &request.end_time,
            )
            .await?;
        Ok(CalendarCapability::Supported(RoomAvailability {
            state: result.state,
            busy_start: result.busy_start,
            busy_end: result.busy_end,
        }))
    }

    async fn get_participant_schedules(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        request: &ParticipantScheduleRequest,
    ) -> Result<CalendarCapability<Vec<ParticipantSchedule>>> {
        let result = ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?
            .get_schedules(&request.emails, &request.start_time, &request.end_time)
            .await?;
        Ok(CalendarCapability::Supported(
            result
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
        let client = ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?;
        let calendars = client.list_calendars().await?;
        let mut mapping = std::collections::HashMap::new();
        for calendar in &calendars {
            let existing: Option<(String, bool)> = ctx.db.reader().query_row(
                "SELECT id, is_subscribed FROM calendars WHERE account_id = ?1 AND remote_id = ?2",
                rusqlite::params![account.id, calendar.id], |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            mapping.insert(
                calendar.id.clone(),
                existing.unwrap_or_else(|| (uuid::Uuid::new_v4().to_string(), true)),
            );
        }
        let mut mirrors = std::collections::HashMap::new();
        let mut read_errors = Vec::new();
        for calendar in &calendars {
            let (local, subscribed) = &mapping[&calendar.id];
            if !subscribed {
                continue;
            }
            let mut resources = Vec::new();
            for value in client.list_calendar_resources(&calendar.id).await? {
                let id = crate::mail::graph::event_set::required_string(&value, "id")?;
                if value["isCancelled"].as_bool() == Some(true) {
                    continue;
                }
                let native = crate::calendar::event_set::NativeCalendarResource {
                    protocol: "graph".into(),
                    calendar_id: calendar.id.clone(),
                    event_id: id.to_owned(),
                    revision: value["@odata.etag"].as_str().map(str::to_owned),
                    data: value.to_string(),
                };
                let template = CalendarEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    account_id: account.id.clone(),
                    calendar_id: local.clone(),
                    remote_id: Some(id.to_owned()),
                    uid: None,
                    title: String::new(),
                    description: None,
                    location: None,
                    start_time: String::new(),
                    end_time: String::new(),
                    all_day: false,
                    timezone: None,
                    recurrence_rule: None,
                    recurrence_kind: RecurrenceKind::Unknown,
                    organizer_email: None,
                    attendees_json: None,
                    my_status: None,
                    source_message_id: None,
                    ical_data: None,
                    etag: None,
                };
                let decoded =
                    crate::mail::graph::event_set::display_event(&value, &template, &calendar.id);
                match decoded {
                    Ok(event) if event.recurrence_kind == RecurrenceKind::Standalone => {
                        resources.push(db::calendar_sync::Resource::readable(CalendarEventSet {
                            event,
                            native: Some(native),
                            overrides: Vec::new(),
                            content: None,
                        })?);
                    }
                    Ok(event) if event.recurrence_kind == RecurrenceKind::Series => {
                        match client
                            .fetch_calendar_display_set(&calendar.id, id, &event)
                            .await
                        {
                            Ok(set) => resources.push(db::calendar_sync::Resource::readable(set)?),
                            Err(Error::Sync(error)) => {
                                read_errors.push(format!(
                                    "Graph resource {id} cannot be displayed: {error}"
                                ));
                                resources
                                    .push(db::calendar_sync::Resource::unsupported(native, error));
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    Ok(_) => resources.push(db::calendar_sync::Resource::unsupported(
                        native,
                        "Expanded instance in unexpanded inventory",
                    )),
                    Err(error) => {
                        read_errors
                            .push(format!("Graph resource {id} cannot be displayed: {error}"));
                        resources.push(db::calendar_sync::Resource::unsupported(native, error));
                    }
                }
            }
            mirrors.insert(local.clone(), resources);
        }
        let mut conn = ctx.db.writer().await;
        let tx = conn.transaction()?;
        for calendar in &calendars {
            let (local, _) = &mapping[&calendar.id];
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM calendars WHERE id = ?1)",
                [local],
                |row| row.get(0),
            )?;
            if !exists {
                db::calendar::insert_calendar(
                    &tx,
                    local,
                    &NewCalendar {
                        account_id: account.id.clone(),
                        name: calendar.name.clone(),
                        color: calendar.color.clone(),
                        is_default: calendar.is_default,
                    },
                )?;
                tx.execute(
                    "UPDATE calendars SET remote_id = ?1 WHERE id = ?2",
                    rusqlite::params![calendar.id, local],
                )?;
            } else {
                tx.execute("UPDATE calendars SET name = ?1, is_archived = 0, archived_acknowledged_at = NULL WHERE id = ?2", rusqlite::params![calendar.name, local])?;
            }
        }
        db::calendar_sync::rebind_account_resources(&tx, &account.id, "graph", &mirrors)?;
        for (calendar, resources) in mirrors {
            db::calendar_sync::replace_calendar(&tx, &account.id, &calendar, "graph", &resources)?;
        }
        db::calendar_sync::replace_inventory(
            &tx,
            &account.id,
            &mapping.keys().map(String::as_str).collect(),
        )?;
        tx.commit()?;
        if read_errors.is_empty() {
            Ok(())
        } else {
            Err(Error::Sync(read_errors.join("; ")))
        }
    }

    fn validate_event_creation(&self, event: &CalendarEvent, calendar: &str) -> Result<()> {
        crate::mail::graph::event_set::collection_path(calendar)?;
        event_to_graph_json(event).map(|_| ())
    }

    async fn push_created_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        event: &CalendarEvent,
        calendar: &str,
    ) -> Result<Option<PushedEvent>> {
        let payload = event_to_graph_json(event)?;
        let (remote_id, canonical_uid) = ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?
            .create_event(calendar, &payload)
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
        id: &str,
        event: &CalendarEvent,
    ) -> Result<()> {
        ctx.services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?
            .update_event(id, &event_patch_to_graph_json(event))
            .await
    }

    async fn update_recurrence_occurrence(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        request: &RemoteOccurrenceUpdate,
    ) -> Result<RemoteOccurrenceUpdateOutcome> {
        let (calendar, etag, series, original) = validate_occurrence_update(account, request)?;
        let refreshed = ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?
            .update_recurrence_occurrence(
                &request.target_id,
                calendar,
                etag,
                series,
                original,
                &request.patch,
                &request.desired,
            )
            .await?;
        let mut replacement = refreshed.recurrence_seeds.as_ref().and_then(|seeds| seeds.first()).cloned().ok_or_else(|| Error::Sync("Graph occurrence update returned no replacement identity; reconciliation required".into()))?;
        replacement.provider_calendar_id = request.trusted_identity.provider_calendar_id.clone();
        replacement.local_series_event_id = request.trusted_identity.local_series_event_id.clone();
        replacement.recurrence_timezone = request.trusted_identity.recurrence_timezone.clone();
        replacement.validate()?;
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
            etag: replacement.provider_revision.clone(),
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
        id: &str,
        event: &CalendarEvent,
    ) -> Result<Option<String>> {
        ctx.services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?
            .update_event(id, &invitation_copy_patch_to_graph_json(event)?)
            .await?;
        Ok(None)
    }

    async fn push_deleted_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        id: &str,
        _: &str,
    ) -> Result<()> {
        ctx.services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?
            .delete_event(id)
            .await
    }

    async fn push_calendar_rename(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        id: &str,
        name: &str,
    ) -> Result<()> {
        ctx.services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?
            .rename_calendar(id, name)
            .await
    }

    async fn push_calendar_color(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        id: &str,
        color: &str,
    ) -> Result<()> {
        let client = ctx
            .services
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?;
        if let Err(error) = client.set_calendar_color(id, color).await {
            log::warn!("Graph color push failed (calendar may be read-only or shared), keeping local color: {error}");
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
    let calendar = identity
        .provider_calendar_id
        .as_deref()
        .ok_or_else(|| Error::Other("Graph occurrence has no calendar identity".into()))?;
    let series = identity
        .provider_series_id
        .as_deref()
        .ok_or_else(|| Error::Other("Graph occurrence has no series identity".into()))?;
    let original = identity
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
    let kind = if identity.kind == RecurrenceObjectKind::Occurrence {
        "occurrence"
    } else {
        "exception"
    };
    if native["id"].as_str() != Some(request.target_id.as_str())
        || native["type"].as_str() != Some(kind)
        || native["seriesMasterId"].as_str() != Some(series)
        || native["originalStart"].as_str() != Some(original)
        || native["@odata.etag"].as_str() != Some(etag)
    {
        return Err(Error::Other(
            "Graph occurrence identity does not match its native immutable identity".into(),
        ));
    }
    Ok((calendar, etag, series, original))
}

#[cfg(test)]
mod tests;
