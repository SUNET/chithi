//! JMAP calendar backend (RFC 8984 JSCalendar via `CalendarEvent/*`).

use async_trait::async_trait;

use crate::calendar::recurrence_identity::RecurrenceObjectKind;
use crate::calendar::{attendee_status_from_json, CalendarEvent, RecurrenceKind};
use crate::db;
use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::mail::jmap::{JmapCalendarEvent, JmapConfig, JmapConnection};

use super::{
    get_unpushed_events, AttendeeResponseUpdate, CalendarBackend, CalendarBackendCtx,
    CalendarCapability, InviteReplyDelivery, PushedEvent, RemoteOccurrenceUpdate,
    RemoteOccurrenceUpdateOutcome,
};

pub struct JmapCalendarBackend;

async fn connect(
    ctx: &CalendarBackendCtx<'_>,
    account: &AccountFull,
) -> Result<(JmapConfig, JmapConnection)> {
    let (config, connection) = ctx.services.jmap_client(account).await?;
    Ok((config, connection))
}

/// Build the wire event from a local row. `id` is empty for creates —
/// the server assigns one.
fn to_jmap_event(event: &CalendarEvent, remote_calendar_id: &str) -> Result<JmapCalendarEvent> {
    JmapCalendarEvent::for_local_creation(event, remote_calendar_id)
}

fn canonical_occurrence_outcome(
    request: &RemoteOccurrenceUpdate,
    new_state: &str,
    native_event: &serde_json::Value,
    mut recurrence_seeds: Vec<crate::calendar::recurrence_identity::RecurrenceIdentitySeed>,
) -> Result<RemoteOccurrenceUpdateOutcome> {
    if native_event["id"].as_str() != Some(request.target_id.as_str()) {
        return Err(Error::Sync(
            "Canonical JMAP event has a different identity; reconciliation required".into(),
        ));
    }
    let recurrence_id = request
        .trusted_identity
        .recurrence_id
        .as_deref()
        .ok_or_else(|| Error::Sync("JMAP occurrence identity is incomplete".into()))?;
    let detached_id = request.trusted_identity.provider_occurrence_id.as_deref();
    let native = serde_json::to_string(native_event).map_err(|error| {
        Error::Sync(format!(
            "Canonical JMAP event could not be stored; reconciliation required: {error}"
        ))
    })?;
    for seed in &mut recurrence_seeds {
        seed.local_series_event_id = request.trusted_identity.local_series_event_id.clone();
        seed.provider_native_data = Some(native.clone());
        seed.provider_revision = Some(new_state.to_string());
        seed.validate().map_err(|error| {
            Error::Sync(format!(
                "Canonical JMAP recurrence object is invalid; reconciliation required: {error}"
            ))
        })?;
    }
    let mut matches = recurrence_seeds.iter().filter(|seed| {
        seed.recurrence_id.as_deref() == Some(recurrence_id)
            && seed.provider_occurrence_id.as_deref() == detached_id
    });
    let replacement = matches.next().ok_or_else(|| {
        Error::Sync(
            "Canonical JMAP response omitted the edited recurrence identity; reconciliation required"
                .into(),
        )
    })?;
    if matches.next().is_some()
        || replacement.provider_calendar_id != request.trusted_identity.provider_calendar_id
        || replacement.provider_series_id != request.trusted_identity.provider_series_id
        || replacement.provider_occurrence_id != request.trusted_identity.provider_occurrence_id
        || replacement.recurrence_id != request.trusted_identity.recurrence_id
        || replacement.recurrence_timezone != request.trusted_identity.recurrence_timezone
        || replacement.recurrence_value_type != request.trusted_identity.recurrence_value_type
        || replacement.provider_revision.as_deref() != Some(new_state)
    {
        return Err(Error::Sync(
            "Provider returned a different immutable JMAP recurrence identity; reconciliation required"
                .into(),
        ));
    }
    if detached_id.is_none()
        && !matches!(
            replacement.kind,
            RecurrenceObjectKind::Occurrence | RecurrenceObjectKind::Exception
        )
    {
        return Err(Error::Sync(
            "Canonical JMAP override is not mutable; reconciliation required".into(),
        ));
    }
    if detached_id.is_some()
        && !matches!(
            replacement.kind,
            RecurrenceObjectKind::Occurrence | RecurrenceObjectKind::Exception
        )
    {
        return Err(Error::Sync(
            "Canonical detached JMAP occurrence changed classification; reconciliation required"
                .into(),
        ));
    }
    let replacement = replacement.clone();
    let occurrence = replacement.occurrence.clone();
    let canonical_event = detached_id.map(|_| {
        let mut event = request.current_event.clone();
        event.title = occurrence.title.clone();
        event.description = occurrence.description.clone();
        event.location = occurrence.location.clone();
        event.start_time = occurrence.start_time.clone();
        event.end_time = occurrence.end_time.clone();
        event.all_day = occurrence.all_day;
        event.timezone = occurrence.timezone.clone();
        event.recurrence_kind = RecurrenceKind::Occurrence;
        event
    });
    Ok(RemoteOccurrenceUpdateOutcome {
        replacement_identity: replacement,
        occurrence,
        canonical_event,
        canonical_recurrence_objects: detached_id.is_none().then_some(recurrence_seeds),
    })
}

#[async_trait]
impl CalendarBackend for JmapCalendarBackend {
    fn protocol(&self) -> &'static str {
        "jmap"
    }

    fn invite_reply_delivery(&self) -> InviteReplyDelivery {
        InviteReplyDelivery::JmapSubmission
    }

    async fn sync(&self, ctx: &CalendarBackendCtx<'_>, account: &AccountFull) -> Result<()> {
        let db = ctx.db;
        let account_id = account.id.as_str();
        let (jmap_config, jmap_conn) = connect(ctx, account).await?;

        // Step 1: Fetch and upsert calendars
        let jmap_calendars = jmap_conn.list_jmap_calendars(&jmap_config).await?;
        log::info!(
            "sync_calendars: fetched {} calendars from JMAP for account {}",
            jmap_calendars.len(),
            account_id
        );

        // Build a mapping from remote calendar ID to local calendar ID
        let mut remote_to_local: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        {
            let conn = db.writer().await;
            for jcal in &jmap_calendars {
                let color = jcal.color.as_deref().unwrap_or("#4285f4");
                let local_id = db::calendar::upsert_calendar_by_remote_id(
                    &conn,
                    account_id,
                    &jcal.id,
                    &jcal.name,
                    color,
                    jcal.is_default,
                )?;
                remote_to_local.insert(jcal.id.clone(), local_id);
            }
        }

        // Step 2: Fetch one complete account snapshot and apply it atomically.
        let (events, ambiguous_calendars, ambiguous_events) =
            jmap_conn.fetch_calendar_events(&jmap_config).await?;
        for event in &events {
            if !remote_to_local.contains_key(&event.calendar_id) {
                return Err(Error::Sync(format!(
                    "JMAP event {} references an unknown calendar; snapshot not applied",
                    event.id
                )));
            }
        }
        {
            let mut conn = db.writer().await;
            for ev in &events {
                let local_cal_id = remote_to_local
                    .get(&ev.calendar_id)
                    .expect("calendar membership validated")
                    .clone();
                let event_id = uuid::Uuid::new_v4().to_string();
                let cal_event = CalendarEvent {
                    id: event_id,
                    account_id: account_id.to_string(),
                    calendar_id: local_cal_id.clone(),
                    uid: ev.uid.clone(),
                    title: ev.title.clone(),
                    description: ev.description.clone(),
                    location: ev.location.clone(),
                    start_time: ev.start.clone(),
                    end_time: ev.end.clone(),
                    all_day: ev.all_day,
                    timezone: ev.timezone.clone(),
                    recurrence_rule: ev.recurrence_rule.clone(),
                    recurrence_kind: ev.recurrence_kind,
                    organizer_email: ev.organizer_email.clone(),
                    attendees_json: ev.attendees_json.clone(),
                    my_status: attendee_status_from_json(
                        ev.attendees_json.as_deref(),
                        &account.email,
                    ),
                    source_message_id: None,
                    ical_data: None,
                    remote_id: Some(ev.id.clone()),
                    etag: None,
                };

                match ev.recurrence_seeds.as_deref() {
                    Some(seeds) => db::calendar::upsert_event_by_remote_id_with_recurrence(
                        &conn, &cal_event, seeds,
                    )
                    .map(|_| ()),
                    None => db::calendar::upsert_event_by_remote_id(&conn, &cal_event),
                }
                .map_err(|error| {
                    Error::Sync(format!(
                        "Failed to apply JMAP event {} atomically: {error}",
                        ev.id
                    ))
                })?;
            }

            let server_ids: std::collections::HashSet<&str> =
                events.iter().map(|event| event.id.as_str()).collect();
            let local_synced: Vec<(String, String, String)> = {
                let mut statement = conn.prepare(
                    "SELECT id, calendar_id, remote_id FROM calendar_events
                     WHERE account_id = ?1 AND remote_id IS NOT NULL AND remote_id != ''",
                )?;
                let rows = statement
                    .query_map(rusqlite::params![account_id], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?
                    .collect::<std::result::Result<_, _>>()?;
                rows
            };
            let protected_local_calendars: std::collections::HashSet<&str> = ambiguous_calendars
                .iter()
                .filter_map(|remote_id| remote_to_local.get(remote_id).map(String::as_str))
                .collect();
            let deleted_ids: Vec<String> = local_synced
                .into_iter()
                .filter(|(_, local_calendar_id, remote_id)| {
                    !server_ids.contains(remote_id.as_str())
                        && !ambiguous_events.contains(remote_id)
                        && !protected_local_calendars.contains(local_calendar_id.as_str())
                })
                .map(|(local_id, _, _)| local_id)
                .collect();
            let deleted = if deleted_ids.is_empty() {
                0
            } else {
                let transaction = conn.transaction()?;
                let deleted =
                    db::calendar_event_deletion::delete_events(&transaction, &deleted_ids)?.deleted;
                transaction.commit()?;
                deleted
            };
            if deleted != 0 {
                log::info!("sync_calendars: removed {deleted} server-deleted JMAP events");
            }
        }

        // Step 3: Push local events (no remote_id) to the JMAP server
        let mut push_failures = Vec::new();
        {
            let conn = db.writer().await;
            let local_events: Vec<CalendarEvent> = get_unpushed_events(&conn, account_id)?;

            if !local_events.is_empty() {
                log::info!(
                    "sync_calendars: pushing {} local events to JMAP",
                    local_events.len()
                );
                drop(conn); // Release lock for async calls

                for ev in &local_events {
                    // Find the remote calendar ID for this event's local calendar
                    let remote_cal_id = remote_to_local
                        .iter()
                        .find(|(_, local_id)| **local_id == ev.calendar_id)
                        .map(|(remote_id, _)| remote_id.clone())
                        .unwrap_or_default();

                    if remote_cal_id.is_empty() {
                        push_failures.push(format!(
                            "{} ({}): destination calendar is unavailable",
                            ev.title, ev.id
                        ));
                        continue;
                    }

                    let jmap_event = match to_jmap_event(ev, &remote_cal_id) {
                        Ok(event) => event,
                        Err(error) => {
                            push_failures.push(format!("{}: {error}", ev.id));
                            continue;
                        }
                    };

                    match jmap_conn
                        .create_calendar_event(&jmap_config, &jmap_event)
                        .await
                    {
                        Ok(remote_id) => {
                            log::info!(
                                "sync_calendars: pushed event '{}' to JMAP, remote_id={}",
                                ev.title,
                                remote_id
                            );
                            let conn = db.writer().await;
                            if let Err(error) = conn.execute(
                                "UPDATE calendar_events SET remote_id = ?1 WHERE id = ?2",
                                rusqlite::params![remote_id, ev.id],
                            ) {
                                push_failures.push(format!(
                                    "{} ({}): failed to save remote id: {error}",
                                    ev.title, ev.id
                                ));
                            }
                        }
                        Err(e) => {
                            push_failures.push(format!("{} ({}): {e}", ev.title, ev.id));
                        }
                    }
                }
            }
        }

        if push_failures.is_empty() {
            Ok(())
        } else {
            Err(Error::Sync(format!(
                "JMAP calendar sync could not create {} local event(s): {}",
                push_failures.len(),
                push_failures.join("; ")
            )))
        }
    }

    fn validate_event_creation(
        &self,
        event: &CalendarEvent,
        remote_calendar_id: &str,
    ) -> Result<()> {
        to_jmap_event(event, remote_calendar_id).map(|_| ())
    }

    async fn push_created_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        event: &CalendarEvent,
        remote_calendar_id: &str,
    ) -> Result<Option<PushedEvent>> {
        let jmap_event = to_jmap_event(event, remote_calendar_id)?;
        let (jmap_config, conn_jmap) = connect(ctx, account).await?;
        let remote_id = conn_jmap
            .create_calendar_event(&jmap_config, &jmap_event)
            .await?;
        Ok(Some(PushedEvent {
            remote_id,
            canonical_uid: None,
            etag: None,
        }))
    }

    async fn push_deleted_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        _remote_calendar_id: &str,
    ) -> Result<()> {
        let (jmap_config, conn_jmap) = connect(ctx, account).await?;
        conn_jmap
            .delete_calendar_event(&jmap_config, remote_id)
            .await
    }

    async fn update_recurrence_occurrence(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        request: &RemoteOccurrenceUpdate,
    ) -> Result<RemoteOccurrenceUpdateOutcome> {
        request.trusted_identity.validate()?;
        request.desired.validate()?;
        let expected_state = request
            .expected_provider_revision
            .as_deref()
            .filter(|state| !state.is_empty())
            .ok_or_else(|| {
                Error::Sync(
                    "JMAP occurrence has no expected CalendarEvent state; reconciliation required"
                        .into(),
                )
            })?;
        if request.trusted_identity.provider_revision.as_deref() != Some(expected_state) {
            return Err(Error::Sync(
                "Expected JMAP state contradicts the trusted recurrence state; reconciliation required"
                    .into(),
            ));
        }
        let recurrence_id = request
            .trusted_identity
            .recurrence_id
            .as_deref()
            .ok_or_else(|| Error::Sync("JMAP occurrence identity is incomplete".into()))?;
        let detached_id = request.trusted_identity.provider_occurrence_id.as_deref();
        let expected_target = detached_id
            .or(request.trusted_identity.provider_series_id.as_deref())
            .ok_or_else(|| Error::Sync("JMAP occurrence has no provider target".into()))?;
        if request.target_id != expected_target {
            return Err(Error::Sync(
                "JMAP occurrence target contradicts its immutable identity".into(),
            ));
        }

        let patch = JmapCalendarEvent::occurrence_update_patch(
            detached_id.is_none().then_some(recurrence_id),
            &request.patch,
            &request.desired,
        );
        let (config, connection) = connect(ctx, account).await?;
        let (new_state, native_event, recurrence_seeds) = connection
            .update_calendar_occurrence(&config, &request.target_id, expected_state, &patch)
            .await?;
        canonical_occurrence_outcome(request, &new_state, &native_event, recurrence_seeds)
    }

    async fn push_updated_invitation_copy(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        event: &CalendarEvent,
    ) -> Result<Option<String>> {
        let patch = JmapCalendarEvent::personal_copy_update_patch(event)?;
        let (jmap_config, connection) = connect(ctx, account).await?;
        connection
            .update_calendar_event(&jmap_config, remote_id, &patch)
            .await?;
        Ok(None)
    }

    async fn push_calendar_rename(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        name: &str,
    ) -> Result<()> {
        let (jmap_config, conn_jmap) = connect(ctx, account).await?;
        conn_jmap
            .rename_calendar(&jmap_config, remote_id, name)
            .await
    }

    async fn push_calendar_color(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        color: &str,
    ) -> Result<()> {
        let (jmap_config, conn_jmap) = connect(ctx, account).await?;
        conn_jmap
            .set_calendar_color(&jmap_config, remote_id, color)
            .await
    }

    async fn push_attendee_responses(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        updates: &[AttendeeResponseUpdate],
    ) -> Result<CalendarCapability<()>> {
        let Ok((jmap_config, connection)) = connect(ctx, account).await else {
            return Ok(CalendarCapability::Supported(()));
        };
        let Ok((events, _, _)) = connection.fetch_calendar_events(&jmap_config).await else {
            return Ok(CalendarCapability::Supported(()));
        };

        for update in updates {
            let Some(event) = events.iter().find(|event| event.id == update.remote_id) else {
                continue;
            };
            let Some(attendees_json) = event.attendees_json.as_deref() else {
                continue;
            };
            let Ok(attendees) = serde_json::from_str::<Vec<serde_json::Value>>(attendees_json)
            else {
                continue;
            };
            for (index, attendee) in attendees.iter().enumerate() {
                if attendee["email"].as_str() == Some(&update.attendee_email) {
                    let participant_key = format!("att{}", index);
                    connection
                        .update_participant_status(
                            &jmap_config,
                            &update.remote_id,
                            &participant_key,
                            &update.response,
                        )
                        .await
                        .ok();
                    break;
                }
            }
        }

        Ok(CalendarCapability::Supported(()))
    }
}

#[cfg(test)]
mod payload_tests {
    use super::{canonical_occurrence_outcome, to_jmap_event};
    use crate::backend::calendar::RemoteOccurrenceUpdate;
    use crate::backend::testutil::event;
    use crate::calendar::recurrence_identity::{
        OccurrenceFields, RecurrenceIdentity, RecurrenceIdentitySeed, RecurrenceObjectKind,
        RecurrenceValueType, UpdateOccurrenceInput,
    };
    use crate::calendar::RecurrenceKind;
    use serde_json::json;

    fn occurrence_request(detached: bool) -> RemoteOccurrenceUpdate {
        let mut current_event = event();
        current_event.remote_id = Some(if detached { "instance" } else { "series" }.into());
        current_event.recurrence_kind = if detached {
            RecurrenceKind::Occurrence
        } else {
            RecurrenceKind::Series
        };
        let occurrence = OccurrenceFields {
            title: "Before".into(),
            description: Some("Before description".into()),
            location: Some("Before room".into()),
            start_time: "2026-09-20T08:00:00Z".into(),
            end_time: "2026-09-20T09:00:00Z".into(),
            all_day: false,
            timezone: Some("Europe/Stockholm".into()),
        };
        RemoteOccurrenceUpdate {
            target_id: if detached { "instance" } else { "series" }.into(),
            expected_provider_revision: Some("old-state".into()),
            trusted_identity: RecurrenceIdentity {
                object_id: "object".into(),
                account_id: "acc1".into(),
                event_id: current_event.id.clone(),
                local_series_event_id: None,
                provider_calendar_id: Some("calendar-1".into()),
                provider_series_id: Some("series".into()),
                provider_occurrence_id: detached.then(|| "instance".into()),
                recurrence_id: Some("2026-09-20T10:00:00".into()),
                recurrence_timezone: Some("Europe/Stockholm".into()),
                recurrence_value_type: Some(RecurrenceValueType::DateTime),
                occurrence: occurrence.clone(),
                provider_native_data: Some("{}".into()),
                provider_revision: Some("old-state".into()),
                kind: RecurrenceObjectKind::Occurrence,
            },
            current_event,
            patch: UpdateOccurrenceInput {
                title: Some("Canonical".into()),
                ..UpdateOccurrenceInput::default()
            },
            desired: occurrence,
        }
    }

    fn canonical_seed(detached: bool) -> RecurrenceIdentitySeed {
        RecurrenceIdentitySeed {
            local_series_event_id: None,
            provider_calendar_id: Some("calendar-1".into()),
            provider_series_id: Some("series".into()),
            provider_occurrence_id: detached.then(|| "instance".into()),
            recurrence_id: Some("2026-09-20T10:00:00".into()),
            recurrence_timezone: Some("Europe/Stockholm".into()),
            recurrence_value_type: Some(RecurrenceValueType::DateTime),
            occurrence: OccurrenceFields {
                title: "Canonical".into(),
                description: None,
                location: None,
                start_time: "2026-09-20T10:00:00Z".into(),
                end_time: "2026-09-20T11:30:00Z".into(),
                all_day: false,
                timezone: Some("Europe/Stockholm".into()),
            },
            provider_native_data: Some(r#"{"complete":true}"#.into()),
            provider_revision: Some("new-state".into()),
            kind: RecurrenceObjectKind::Exception,
        }
    }

    fn canonical_master() -> RecurrenceIdentitySeed {
        RecurrenceIdentitySeed {
            local_series_event_id: None,
            provider_calendar_id: Some("calendar-1".into()),
            provider_series_id: Some("series".into()),
            provider_occurrence_id: None,
            recurrence_id: None,
            recurrence_timezone: None,
            recurrence_value_type: None,
            occurrence: OccurrenceFields {
                title: "Master".into(),
                description: None,
                location: None,
                start_time: "2026-09-13T08:00:00Z".into(),
                end_time: "2026-09-13T09:00:00Z".into(),
                all_day: false,
                timezone: Some("Europe/Stockholm".into()),
            },
            provider_native_data: Some(r#"{"stale":true}"#.into()),
            provider_revision: Some("stale-state".into()),
            kind: RecurrenceObjectKind::Master,
        }
    }

    fn canonical_exclusion(recurrence_id: &str) -> RecurrenceIdentitySeed {
        let mut seed = canonical_seed(false);
        seed.recurrence_id = Some(recurrence_id.into());
        seed.kind = RecurrenceObjectKind::Exclusion;
        seed
    }

    #[test]
    fn create_payload_leaves_id_to_server_and_targets_given_calendar() {
        let local = event();
        let wire = to_jmap_event(&local, "remote-cal-7").unwrap();
        assert_eq!(wire.id, "");
        assert_eq!(wire.calendar_id, "remote-cal-7");
        assert_eq!(wire.title, local.title);
        assert_eq!(wire.start, local.start_time);
        assert_eq!(wire.end, local.end_time);
        assert!(!wire.all_day);
        assert_eq!(wire.recurrence_kind, local.recurrence_kind);
    }

    #[test]
    fn all_day_flag_carries_through() {
        let mut local = event();
        local.all_day = true;
        assert!(to_jmap_event(&local, "c").unwrap().all_day);
    }

    #[test]
    fn canonical_embedded_and_detached_results_have_distinct_event_outcomes() {
        let embedded = occurrence_request(false);
        let native = json!({"id": "series", "complete": {"provider": true}});
        let outcome = canonical_occurrence_outcome(
            &embedded,
            "new-state",
            &native,
            vec![
                canonical_master(),
                canonical_seed(false),
                canonical_exclusion("2026-09-27T10:00:00"),
            ],
        )
        .unwrap();
        assert_eq!(outcome.occurrence.title, "Canonical");
        assert_eq!(outcome.occurrence.description, None);
        assert_eq!(outcome.occurrence.location, None);
        assert!(outcome.canonical_event.is_none());
        assert_eq!(
            outcome.replacement_identity.kind,
            RecurrenceObjectKind::Exception
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
            json!({"id": "series", "complete": {"provider": true}})
        );
        assert_eq!(
            outcome.replacement_identity.provider_revision.as_deref(),
            Some("new-state")
        );
        assert_eq!(
            outcome.replacement_identity.provider_calendar_id.as_deref(),
            Some("calendar-1")
        );
        let canonical = outcome.canonical_recurrence_objects.unwrap();
        assert_eq!(canonical.len(), 3);
        assert!(canonical.iter().all(|seed| {
            seed.provider_revision.as_deref() == Some("new-state")
                && serde_json::from_str::<serde_json::Value>(
                    seed.provider_native_data.as_deref().unwrap(),
                )
                .unwrap()
                    == native
        }));
        assert_eq!(
            canonical
                .iter()
                .filter_map(|seed| seed.recurrence_id.as_deref())
                .collect::<Vec<_>>(),
            vec!["2026-09-20T10:00:00", "2026-09-27T10:00:00"]
        );
        assert!(!canonical
            .iter()
            .any(|seed| { seed.recurrence_id.as_deref() == Some("2026-10-04T10:00:00") }));
        assert_eq!(
            canonical
                .iter()
                .find(|seed| { seed.recurrence_id == embedded.trusted_identity.recurrence_id })
                .unwrap(),
            &outcome.replacement_identity
        );

        let detached = occurrence_request(true);
        let outcome = canonical_occurrence_outcome(
            &detached,
            "new-state",
            &json!({"id": "instance"}),
            vec![canonical_seed(true)],
        )
        .unwrap();
        let event = outcome.canonical_event.unwrap();
        assert_eq!(event.title, "Canonical");
        assert_eq!(event.description, None);
        assert_eq!(event.location, None);
        assert_eq!(event.recurrence_kind, RecurrenceKind::Occurrence);
        assert!(outcome.canonical_recurrence_objects.is_none());
    }

    #[test]
    fn canonical_identity_changes_require_reconciliation() {
        let request = occurrence_request(true);
        let mut changed_calendar = canonical_seed(true);
        changed_calendar.provider_calendar_id = Some("calendar-2".into());
        let mut changed_timezone = canonical_seed(true);
        changed_timezone.recurrence_timezone = Some("Europe/Helsinki".into());
        for seed in [changed_calendar, changed_timezone] {
            let error = canonical_occurrence_outcome(
                &request,
                "new-state",
                &json!({"id": "instance"}),
                vec![seed],
            )
            .unwrap_err();
            assert!(matches!(error, crate::error::Error::Sync(_)));
            assert!(error.to_string().contains("immutable"));
        }
    }
}

#[cfg(test)]
mod deferred_creation_tests {
    use super::*;
    use crate::backend::testutil::{account, event, temp_pool};
    use crate::calendar::RecurrenceKind;
    use crate::provider::ProviderServices;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Clone, Copy)]
    enum SnapshotFault {
        RepeatedPosition,
        ChangedQueryState,
        ChangedGetState,
        MethodError,
        MalformedObject,
        NotFound,
        MissingList,
    }

    struct CalendarServer {
        root: String,
        writes: Arc<Mutex<Vec<Value>>>,
        requests: Arc<Mutex<Vec<Value>>>,
        events: Arc<Mutex<Vec<Value>>>,
        fault: Arc<Mutex<Option<SnapshotFault>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for CalendarServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl CalendarServer {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let root = format!("http://{}", listener.local_addr().unwrap());
            let writes = Arc::new(Mutex::new(Vec::new()));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let events = Arc::new(Mutex::new(Vec::<Value>::new()));
            let fault = Arc::new(Mutex::new(None));
            let captured = writes.clone();
            let captured_requests = requests.clone();
            let stored = events.clone();
            let injected_fault = fault.clone();
            let query_count = Arc::new(AtomicUsize::new(0));
            let get_count = Arc::new(AtomicUsize::new(0));
            let query_calls = query_count.clone();
            let get_calls = get_count.clone();
            let base = root.clone();
            let task = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    let mut chunk = [0; 4096];
                    let header_end = loop {
                        let count = stream.read(&mut chunk).await.unwrap();
                        assert!(count > 0);
                        bytes.extend_from_slice(&chunk[..count]);
                        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                            break end + 4;
                        }
                    };
                    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    while bytes.len() < header_end + length {
                        let count = stream.read(&mut chunk).await.unwrap();
                        assert!(count > 0);
                        bytes.extend_from_slice(&chunk[..count]);
                    }
                    let response = if headers.starts_with("GET ") {
                        json!({
                            "apiUrl": format!("{base}/jmap/api"),
                            "downloadUrl": format!("{base}/download/{{blobId}}"),
                            "uploadUrl": format!("{base}/upload/{{accountId}}"),
                            "primaryAccounts": {"urn:ietf:params:jmap:mail": "remote-account"},
                            "accounts": {"remote-account": {"accountCapabilities": {
                                "urn:ietf:params:jmap:mail": {},
                                "urn:ietf:params:jmap:calendars": {}
                            }}},
                            "capabilities": {"urn:ietf:params:jmap:core": {
                                "maxObjectsInGet": 2
                            }}
                        })
                    } else {
                        let request: Value =
                            serde_json::from_slice(&bytes[header_end..header_end + length])
                                .unwrap();
                        captured_requests.lock().unwrap().push(request.clone());
                        let mut responses = Vec::new();
                        for call in request["methodCalls"].as_array().unwrap() {
                            let method = call[0].as_str().unwrap();
                            let fault = *injected_fault.lock().unwrap();
                            let mut response_method = method;
                            let mut body = match method {
                                "Calendar/get" => json!({"list": [{
                                    "id": "remote-cal", "name": "Calendar", "isDefault": true
                                }]}),
                                "CalendarEvent/query" => {
                                    let query_index = query_calls.fetch_add(1, Ordering::SeqCst);
                                    let events = stored.lock().unwrap();
                                    let position = call[1]["position"].as_u64().unwrap() as usize;
                                    let limit = call[1]["limit"].as_u64().unwrap() as usize;
                                    json!({
                                        "accountId": "remote-account",
                                        "queryState": if matches!(fault, Some(SnapshotFault::ChangedQueryState)) && query_index > 0 {
                                            "changed-state"
                                        } else { "query-state" },
                                        "position": if matches!(fault, Some(SnapshotFault::RepeatedPosition)) && position > 0 {
                                            0
                                        } else { position },
                                        "total": events.len(),
                                        "ids": events.iter().skip(position).take(limit)
                                            .map(|event| event["id"].clone()).collect::<Vec<_>>()
                                    })
                                }
                                "CalendarEvent/get" => {
                                    let get_index = get_calls.fetch_add(1, Ordering::SeqCst);
                                    let ids = call[1]["ids"].as_array().unwrap();
                                    json!({
                                        "accountId": "remote-account",
                                        "list": stored.lock().unwrap().iter()
                                            .filter(|event| ids.contains(&event["id"]))
                                            .cloned().collect::<Vec<_>>(),
                                        "state": if matches!(fault, Some(SnapshotFault::ChangedGetState)) && get_index > 0 {
                                            "changed-state"
                                        } else { "event-state" },
                                        "notFound": if matches!(fault, Some(SnapshotFault::NotFound)) {
                                            vec![ids[0].clone()]
                                        } else { Vec::<Value>::new() }
                                    })
                                }
                                "CalendarEvent/set" => {
                                    if let Some(event) = call[1]["create"]["new1"].as_object() {
                                        let mut event = Value::Object(event.clone());
                                        captured.lock().unwrap().push(event.clone());
                                        let mut events = stored.lock().unwrap();
                                        let id = format!("remote-{}", events.len());
                                        event["id"] = json!(id);
                                        events.push(event);
                                        json!({"created": {"new1": {"id": id}}})
                                    } else {
                                        assert_eq!(call[1]["sendSchedulingMessages"], false);
                                        let (id, patch) = call[1]["update"]
                                            .as_object()
                                            .unwrap()
                                            .iter()
                                            .next()
                                            .unwrap();
                                        captured.lock().unwrap().push(patch.clone());
                                        json!({"updated": {id: null}})
                                    }
                                }
                                _ => panic!("unexpected JMAP method {method}"),
                            };
                            if method == "CalendarEvent/query"
                                && matches!(fault, Some(SnapshotFault::MethodError))
                            {
                                response_method = "error";
                                body = json!({"type": "serverFail"});
                            }
                            if method == "CalendarEvent/get"
                                && matches!(fault, Some(SnapshotFault::MalformedObject))
                            {
                                body["list"][0].as_object_mut().unwrap().remove("title");
                            }
                            if method == "CalendarEvent/get"
                                && matches!(fault, Some(SnapshotFault::MissingList))
                            {
                                body.as_object_mut().unwrap().remove("list");
                            }
                            responses.push(json!([response_method, body, call[2]]));
                        }
                        json!({"methodResponses": responses, "sessionState": "state"})
                    };
                    let body = response.to_string();
                    stream.write_all(format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    ).as_bytes()).await.unwrap();
                }
            });
            Self {
                root,
                writes,
                requests,
                events,
                fault,
                task,
            }
        }

        fn services(&self) -> ProviderServices {
            // Basic-auth calendar sync does not consult OAuth or the keyring.
            let mut services = ProviderServices::production().unwrap();
            let http = reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(3))
                .build()
                .unwrap();
            services.transports.jmap_discovery_http = http.clone();
            services.transports.jmap_api_http = http;
            services
        }

        fn account(&self) -> AccountFull {
            let mut account = account("calendar", "jmap");
            account.jmap_url = self.root.clone();
            account.jmap_auth_method = "basic".into();
            account
        }
    }

    fn remote_event(id: &str, calendar_ids: Value) -> Value {
        json!({
            "@type": "Event",
            "id": id,
            "uid": format!("{id}@test"),
            "calendarIds": calendar_ids,
            "title": format!("Event {id}"),
            "start": "2026-09-13T10:00:00",
            "duration": "PT1H"
        })
    }

    #[tokio::test]
    async fn complete_snapshot_paginates_query_and_chunks_get() {
        let server = CalendarServer::start().await;
        server.events.lock().unwrap().extend(
            (0..501)
                .map(|index| remote_event(&format!("event-{index}"), json!({"remote-cal": true}))),
        );
        let services = server.services();
        let (config, connection) = services.jmap_client(&server.account()).await.unwrap();

        let (events, ambiguous_calendars, ambiguous_events) =
            connection.fetch_calendar_events(&config).await.unwrap();

        assert_eq!(events.len(), 501);
        assert!(ambiguous_calendars.is_empty());
        assert!(ambiguous_events.is_empty());
        let requests = server.requests.lock().unwrap();
        let methods: Vec<&str> = requests
            .iter()
            .filter_map(|request| request["methodCalls"][0][0].as_str())
            .collect();
        assert_eq!(
            methods
                .iter()
                .filter(|method| **method == "CalendarEvent/query")
                .count(),
            3
        );
        assert_eq!(
            methods
                .iter()
                .filter(|method| **method == "CalendarEvent/get")
                .count(),
            251
        );
        let second_query = requests
            .iter()
            .filter(|request| request["methodCalls"][0][0] == "CalendarEvent/query")
            .nth(1)
            .unwrap();
        assert_eq!(second_query["methodCalls"][0][1]["position"], 500);
        assert!(requests
            .iter()
            .filter(|request| request["methodCalls"][0][0] == "CalendarEvent/get")
            .all(|request| request["methodCalls"][0][1]["ids"]
                .as_array()
                .unwrap()
                .len()
                <= 2));
    }

    #[tokio::test]
    async fn incomplete_snapshots_never_delete_local_rows() {
        for fault in [
            SnapshotFault::RepeatedPosition,
            SnapshotFault::ChangedQueryState,
            SnapshotFault::ChangedGetState,
            SnapshotFault::MethodError,
            SnapshotFault::MalformedObject,
            SnapshotFault::NotFound,
            SnapshotFault::MissingList,
        ] {
            let server = CalendarServer::start().await;
            let count = if matches!(
                fault,
                SnapshotFault::RepeatedPosition | SnapshotFault::ChangedQueryState
            ) {
                501
            } else if matches!(fault, SnapshotFault::ChangedGetState) {
                3
            } else {
                1
            };
            server.events.lock().unwrap().extend(
                (0..count).map(|index| {
                    remote_event(&format!("event-{index}"), json!({"remote-cal": true}))
                }),
            );
            *server.fault.lock().unwrap() = Some(fault);
            let services = server.services();
            let account = server.account();
            let (_directory, db) = temp_pool();
            {
                let conn = db.writer().await;
                db::schema::initialize(&conn).unwrap();
                conn.execute(
                    "INSERT INTO accounts (id, display_name, email, username)
                     VALUES ('acc1', 'Test', 'u@example.com', 'u@example.com')",
                    [],
                )
                .unwrap();
                let calendar_id = db::calendar::upsert_calendar_by_remote_id(
                    &conn,
                    &account.id,
                    "remote-cal",
                    "Calendar",
                    "#4285f4",
                    true,
                )
                .unwrap();
                let mut stale = event();
                stale.id = "stale-local".into();
                stale.account_id = account.id.clone();
                stale.calendar_id = calendar_id;
                stale.remote_id = Some("stale-remote".into());
                db::calendar::insert_event(&conn, &stale).unwrap();
            }

            assert!(JmapCalendarBackend
                .sync(
                    &CalendarBackendCtx {
                        db: &db,
                        services: &services,
                    },
                    &account,
                )
                .await
                .is_err());
            assert!(db::calendar::get_event(&db.reader(), "stale-local").is_ok());
        }
    }

    #[tokio::test]
    async fn multi_calendar_ambiguity_retains_existing_row_without_moving_it() {
        let server = CalendarServer::start().await;
        server
            .events
            .lock()
            .unwrap()
            .push(remote_event("shared", json!({"remote-cal": true})));
        let services = server.services();
        let account = server.account();
        let (_directory, db) = temp_pool();
        {
            let conn = db.writer().await;
            db::schema::initialize(&conn).unwrap();
            conn.execute(
                "INSERT INTO accounts (id, display_name, email, username)
                 VALUES ('acc1', 'Test', 'u@example.com', 'u@example.com')",
                [],
            )
            .unwrap();
        }
        let context = CalendarBackendCtx {
            db: &db,
            services: &services,
        };
        JmapCalendarBackend.sync(&context, &account).await.unwrap();
        let before: (String, String, String) = db
            .reader()
            .query_row(
                "SELECT id, calendar_id, title FROM calendar_events WHERE remote_id = 'shared'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        server.events.lock().unwrap()[0] = remote_event(
            "shared",
            json!({"aaa-first-in-json-order": true, "remote-cal": true}),
        );
        server.events.lock().unwrap()[0]["title"] = json!("Must not overwrite");
        JmapCalendarBackend.sync(&context, &account).await.unwrap();

        let after: (String, String, String) = db
            .reader()
            .query_row(
                "SELECT id, calendar_id, title FROM calendar_events WHERE remote_id = 'shared'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(after, before);
    }

    #[tokio::test]
    async fn complete_snapshot_deletes_absent_remote_rows() {
        let server = CalendarServer::start().await;
        server
            .events
            .lock()
            .unwrap()
            .push(remote_event("deleted-later", json!({"remote-cal": true})));
        let services = server.services();
        let account = server.account();
        let (_directory, db) = temp_pool();
        {
            let conn = db.writer().await;
            db::schema::initialize(&conn).unwrap();
            conn.execute(
                "INSERT INTO accounts (id, display_name, email, username)
                 VALUES ('acc1', 'Test', 'u@example.com', 'u@example.com')",
                [],
            )
            .unwrap();
        }
        let context = CalendarBackendCtx {
            db: &db,
            services: &services,
        };
        JmapCalendarBackend.sync(&context, &account).await.unwrap();
        assert_eq!(
            db.reader()
                .query_row(
                    "SELECT COUNT(*) FROM calendar_events WHERE remote_id = 'deleted-later'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );

        server.events.lock().unwrap().clear();
        JmapCalendarBackend.sync(&context, &account).await.unwrap();
        assert_eq!(
            db.reader()
                .query_row(
                    "SELECT COUNT(*) FROM calendar_events WHERE remote_id = 'deleted-later'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn deferred_creation_blocks_laundering_but_processes_valid_rows_on_each_sync() {
        let server = CalendarServer::start().await;
        let services = server.services();
        let account = server.account();
        let (_directory, db) = temp_pool();
        let context = CalendarBackendCtx {
            db: &db,
            services: &services,
        };
        let mut blocked = Vec::new();
        let mut allowed = Vec::new();
        {
            let conn = db.writer().await;
            db::schema::initialize(&conn).unwrap();
            conn.execute(
                "INSERT INTO accounts (id, display_name, email, username)
                 VALUES ('acc1', 'Test', 'u@example.com', 'u@example.com')",
                [],
            )
            .unwrap();
            let calendar_id = db::calendar::upsert_calendar_by_remote_id(
                &conn,
                &account.id,
                "remote-cal",
                "Calendar",
                "#4285f4",
                true,
            )
            .unwrap();
            let raw = "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//Test//EN\n\
                BEGIN:VEVENT\nUID:source\nDTSTAMP:20260901T120000Z\n\
                DTSTART:20260913T100000Z\nRRULE:FREQ=WEEKLY\n\
                EXDATE:20260920T100000Z\nEND:VEVENT\nEND:VCALENDAR\n";
            for (id, kind, rule, ical) in [
                ("unknown", RecurrenceKind::Unknown, None, None),
                (
                    "occurrence",
                    RecurrenceKind::Occurrence,
                    None,
                    Some(raw.replace(
                        "RRULE:FREQ=WEEKLY\nEXDATE:20260920T100000Z",
                        "RECURRENCE-ID:20260920T100000Z",
                    )),
                ),
                ("series-no-rule", RecurrenceKind::Series, None, None),
                (
                    "series-invalid",
                    RecurrenceKind::Series,
                    Some("FREQ=WEEKLY;COUNT=bad"),
                    None,
                ),
                (
                    "series-exclusions",
                    RecurrenceKind::Series,
                    Some("FREQ=WEEKLY"),
                    Some(raw.into()),
                ),
                ("valid-standalone", RecurrenceKind::Standalone, None, None),
                (
                    "valid-series",
                    RecurrenceKind::Series,
                    Some("FREQ=WEEKLY;COUNT=4"),
                    None,
                ),
            ] {
                let event = CalendarEvent {
                    id: id.into(),
                    calendar_id: calendar_id.clone(),
                    title: id.into(),
                    uid: Some(format!("{id}@test")),
                    recurrence_kind: kind,
                    recurrence_rule: rule.map(str::to_string),
                    ical_data: ical,
                    // Both legacy representations of an unpushed remote id occur.
                    remote_id: (kind == RecurrenceKind::Occurrence).then(String::new),
                    ..event()
                };
                db::calendar::insert_event(&conn, &event).unwrap();
                if id.starts_with("valid-") {
                    allowed.push(event);
                } else {
                    blocked.push(event);
                }
            }
        }
        for _ in 0..2 {
            let error = JmapCalendarBackend
                .sync(&context, &account)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("could not create 5 local event(s)"),
                "{error}"
            );
            assert!(error.contains("source calendar"), "{error}");
            let conn = db.reader();
            for before in &blocked {
                assert!(error.contains(&before.id), "{error}");
                let after = db::calendar::get_event(&conn, &before.id).unwrap();
                assert_eq!(
                    serde_json::to_value(after).unwrap(),
                    serde_json::to_value(before).unwrap()
                );
            }
            for before in &allowed {
                let after = db::calendar::get_event(&conn, &before.id).unwrap();
                assert_eq!(after.recurrence_kind, before.recurrence_kind);
                assert!(after.remote_id.is_some());
            }
            let writes = server.writes.lock().unwrap();
            assert_eq!(
                writes.len(),
                2,
                "blocked rows must never reach CalendarEvent/set"
            );
            assert!(writes
                .iter()
                .all(|event| event["uid"].as_str().unwrap().starts_with("valid-")));
            let series = writes
                .iter()
                .find(|event| event["title"] == "valid-series")
                .unwrap();
            assert_eq!(series["recurrenceRules"][0]["frequency"], "weekly");
            assert_eq!(series["recurrenceRules"][0]["count"], 4);
        }
    }

    #[tokio::test]
    async fn direct_transport_creation_rejects_unknown_occurrences_and_lossy_provider_dtos() {
        let server = CalendarServer::start().await;
        server.events.lock().unwrap().push(json!({
            "id": "source", "@type": "Event", "uid": "source", "calendarIds": {"remote-cal": true},
            "title": "Source", "start": "2026-09-13T10:00:00", "duration": "PT1H",
            "recurrenceRules": [{"frequency": "weekly"}],
            "recurrenceOverrides": {"2026-09-20T10:00:00": {"excluded": true}}
        }));
        let services = server.services();
        let (config, connection) = services.jmap_client(&server.account()).await.unwrap();
        let valid = to_jmap_event(&event(), "remote-cal").unwrap();
        for kind in [
            RecurrenceKind::Unknown,
            RecurrenceKind::Occurrence,
            RecurrenceKind::Series,
        ] {
            let mut blocked = valid.clone();
            blocked.recurrence_kind = kind;
            blocked.recurrence_rule =
                (kind == RecurrenceKind::Series).then(|| "FREQ=WEEKLY".into());
            let error = connection
                .create_calendar_event(&config, &blocked)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("Cannot create JMAP event"), "{error}");
        }
        let (fetched, _, _) = connection.fetch_calendar_events(&config).await.unwrap();
        assert_eq!(fetched[0].recurrence_kind, RecurrenceKind::Series);
        assert!(fetched[0].recurrence_rule.is_some());
        assert_eq!(fetched[0].response_state.as_deref(), Some("event-state"));
        assert_eq!(
            fetched[0].native_json.as_ref(),
            Some(&server.events.lock().unwrap()[0])
        );
        assert!(connection
            .create_calendar_event(&config, &fetched[0])
            .await
            .is_err());
        assert!(server.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn recurrence_ingestion_is_stable_and_standalone_refresh_clears_objects() {
        let server = CalendarServer::start().await;
        server.events.lock().unwrap().push(json!({
            "id": "series", "@type": "Event", "uid": "series-uid",
            "calendarIds": {"remote-cal": true},
            "title": "Series", "start": "2026-09-13T10:00:00",
            "duration": "PT1H", "timeZone": "Europe/Stockholm",
            "recurrenceRules": [{"frequency": "weekly"}],
            "recurrenceOverrides": {
                "2026-09-20T10:00:00": {"excluded": true}
            }
        }));
        let services = server.services();
        let account = server.account();
        let (_directory, db) = temp_pool();
        {
            let conn = db.writer().await;
            db::schema::initialize(&conn).unwrap();
            conn.execute(
                "INSERT INTO accounts (id, display_name, email, username)
                 VALUES ('acc1', 'Test', 'u@example.com', 'u@example.com')",
                [],
            )
            .unwrap();
        }
        let context = CalendarBackendCtx {
            db: &db,
            services: &services,
        };

        JmapCalendarBackend.sync(&context, &account).await.unwrap();
        let first: Vec<(String, String, Option<String>, Option<String>)> = {
            let conn = db.reader();
            let mut statement = conn
                .prepare(
                    "SELECT object_id, object_kind, provider_native_data,
                            provider_revision
                     FROM calendar_recurrence_objects ORDER BY object_kind",
                )
                .unwrap();
            statement
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .unwrap()
                .map(|row| row.unwrap())
                .collect()
        };
        assert_eq!(first.len(), 2);
        assert!(first.iter().any(|row| row.1 == "master"));
        assert!(first.iter().any(|row| row.1 == "exclusion"));
        assert!(first.iter().all(|row| row.2.is_some()));
        assert!(first
            .iter()
            .all(|row| row.3.as_deref() == Some("event-state")));

        JmapCalendarBackend.sync(&context, &account).await.unwrap();
        let second: Vec<(String, String)> = {
            let conn = db.reader();
            let mut statement = conn
                .prepare(
                    "SELECT object_id, object_kind
                     FROM calendar_recurrence_objects ORDER BY object_kind",
                )
                .unwrap();
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(|row| row.unwrap())
                .collect()
        };
        assert_eq!(
            second,
            first
                .iter()
                .map(|row| (row.0.clone(), row.1.clone()))
                .collect::<Vec<_>>()
        );

        {
            let mut events = server.events.lock().unwrap();
            events[0].as_object_mut().unwrap().remove("recurrenceRules");
            events[0]
                .as_object_mut()
                .unwrap()
                .remove("recurrenceOverrides");
        }
        JmapCalendarBackend.sync(&context, &account).await.unwrap();
        let conn = db.reader();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM calendar_recurrence_objects",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        let event_id: String = conn
            .query_row(
                "SELECT id FROM calendar_events WHERE account_id = 'acc1' AND remote_id = 'series'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let stored = db::calendar::get_event(&conn, &event_id).unwrap();
        assert_eq!(stored.recurrence_kind, RecurrenceKind::Standalone);
    }

    #[tokio::test]
    async fn personal_copy_update_clears_scheduling_and_stale_optional_fields() {
        let server = CalendarServer::start().await;
        let services = server.services();
        let account = server.account();
        let (_directory, db) = temp_pool();
        let mut local = event();
        local.description = None;
        local.location = None;
        local.organizer_email = Some("owner@example.test".into());
        local.attendees_json = Some("[]".into());

        JmapCalendarBackend
            .push_updated_invitation_copy(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services,
                },
                &account,
                "remote-copy",
                &local,
            )
            .await
            .unwrap();

        let writes = server.writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert!(writes[0]["description"].is_null());
        assert_eq!(writes[0]["locations"], json!({}));
        assert_eq!(writes[0]["participants"], json!({}));
        assert!(writes[0]["recurrenceRules"].is_null());
        assert!(writes[0]["recurrenceOverrides"].is_null());
    }
}
