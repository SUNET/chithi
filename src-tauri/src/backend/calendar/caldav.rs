//! CalDAV calendar backend (RFC 4791).

use async_trait::async_trait;

mod event_sets;
mod resource;
mod timezones;

#[cfg(test)]
mod tests;

use crate::calendar::event_set::{CalendarEventSet, NativeCalendarResource};

use crate::calendar::ical;
use crate::calendar::recurrence_identity::{
    OccurrenceFields, RecurrenceIdentitySeed, RecurrenceObjectKind, RecurrenceValueType,
};
use crate::calendar::{attendee_status_for_email, CalendarEvent, RecurrenceKind};
use crate::db;
use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::mail::caldav::{CalDavClient, CalDavConfig, CalDavEvent};

use super::{
    CalendarBackend, CalendarBackendCtx, PushedEvent, RemoteOccurrenceUpdate,
    RemoteOccurrenceUpdateOutcome,
};

pub struct CalDavCalendarBackend;

/// Preserve source recurrence verbatim; generated ICS must never erase it.
fn creation_ical_data(event: &CalendarEvent, uid: &str) -> Result<String> {
    let unsupported = || {
        Error::Other(format!(
        "Cannot create CalDAV event '{}': complete recurrence data is unavailable. Keep this event local and manage its recurrence in the source calendar",
        event.title
    ))
    };
    if event.recurrence_kind == RecurrenceKind::Unknown {
        return Err(unsupported());
    }
    if let Some(raw) = &event.ical_data {
        if ical::parse_ical_data(raw)
            .iter()
            .any(|invite| invite.recurrence_kind == event.recurrence_kind)
        {
            if event.source_message_id.is_some()
                && event.organizer_email.is_none()
                && event.attendees_json.is_none()
            {
                return personal_copy_ical_data(event, uid);
            }
            return Ok(raw.clone());
        }
        return Err(unsupported());
    }
    let rule = event
        .recurrence_rule
        .as_deref()
        .filter(|rule| !rule.is_empty());
    let rule = match (event.recurrence_kind, rule) {
        (RecurrenceKind::Standalone, None) => None,
        (RecurrenceKind::Series, Some(rule))
            if event.source_message_id.is_none()
                && rule.is_ascii()
                && !rule.contains(['\r', '\n'])
                && crate::calendar::recurrence::rrule_to_jscalendar(
                    rule,
                    event.timezone.as_deref(),
                )
                .is_some() =>
        {
            Some(rule.trim().strip_prefix("RRULE:").unwrap_or(rule.trim()))
        }
        _ => return Err(unsupported()),
    };
    let mut raw = crate::mail::caldav::generate_ical_event(
        uid,
        &event.title,
        event.description.as_deref(),
        event.location.as_deref(),
        &event.start_time,
        &event.end_time,
        event.all_day,
        event.timezone.as_deref(),
    );
    if let Some(rule) = rule {
        raw = raw.replace("\r\nEND:VEVENT", &format!("\r\nRRULE:{rule}\r\nEND:VEVENT"));
    }
    Ok(raw)
}

/// Produce a scheduling-free resource while preserving the source's complete
/// recurrence set whenever raw iCalendar is available.
fn personal_copy_ical_data(event: &CalendarEvent, uid: &str) -> Result<String> {
    if let Some(raw) = event.ical_data.as_deref() {
        let groups = ical::parse_ical_event_groups(raw).map_err(Error::Other)?;
        let group = groups
            .into_iter()
            .find(|group| group.representative.uid == uid)
            .ok_or_else(|| {
                Error::Other("The personal copy UID is missing from iCalendar".into())
            })?;
        if group.representative.recurrence_kind != event.recurrence_kind {
            return Err(Error::Other(
                "The personal copy recurrence does not match its iCalendar source".into(),
            ));
        }
        return Ok(overlay_personal_fields(&group.ical_raw, event, uid));
    }
    creation_ical_data(event, uid)
}

/// Replace only the representative VEVENT's editable fields. Recurrence
/// properties, exceptions, timezone definitions, alarms, and vendor data stay
/// byte-for-byte equivalent after the shared parser's unfolding.
fn overlay_personal_fields(raw: &str, event: &CalendarEvent, uid: &str) -> String {
    let generated = crate::mail::caldav::generate_ical_event(
        uid,
        &event.title,
        event.description.as_deref(),
        event.location.as_deref(),
        &event.start_time,
        &event.end_time,
        event.all_day,
        event.timezone.as_deref(),
    );
    let replacement: Vec<&str> = generated
        .lines()
        .filter(|line| is_personal_editable_property(line))
        .collect();
    let lines: Vec<&str> = raw.lines().collect();
    let mut event_ranges = Vec::new();
    let mut start = None;
    for (index, line) in lines.iter().enumerate() {
        if line.trim().eq_ignore_ascii_case("BEGIN:VEVENT") {
            start = Some(index);
        } else if line.trim().eq_ignore_ascii_case("END:VEVENT") {
            if let Some(start) = start.take() {
                event_ranges.push((start, index));
            }
        }
    }
    let selected = event_ranges
        .iter()
        .copied()
        .find(|(start, end)| {
            !lines[start + 1..*end]
                .iter()
                .any(|line| ical_property_name(line) == Some("RECURRENCE-ID"))
        })
        .or_else(|| event_ranges.first().copied());
    let Some((selected_start, selected_end)) = selected else {
        return raw.to_string();
    };

    let mut output = Vec::with_capacity(lines.len() + replacement.len());
    let mut nested_depth = 0usize;
    for (index, line) in lines.iter().enumerate() {
        let inside_selected = index > selected_start && index < selected_end;
        let replaceable =
            inside_selected && nested_depth == 0 && is_personal_editable_property(line);
        if replaceable {
            continue;
        }
        if index == selected_end {
            output.extend(replacement.iter().copied());
        }
        output.push(*line);
        if inside_selected && line.trim().starts_with("BEGIN:") {
            nested_depth += 1;
        } else if inside_selected && line.trim().starts_with("END:") {
            nested_depth = nested_depth.saturating_sub(1);
        }
    }
    format!("{}\r\n", output.join("\r\n"))
}

fn ical_property_name(line: &str) -> Option<&str> {
    let end = line.find([';', ':'])?;
    Some(&line[..end])
}

fn is_personal_editable_property(line: &str) -> bool {
    ical_property_name(line).is_some_and(|name| {
        ["DTSTART", "DTEND", "SUMMARY", "DESCRIPTION", "LOCATION"]
            .iter()
            .any(|expected| name.eq_ignore_ascii_case(expected))
    })
}

struct ParsedCalDavResource {
    event: CalendarEvent,
    /// `None` means recurrence identity was not trustworthy and must not
    /// replace identity learned by an earlier provider read.
    #[cfg(test)]
    recurrence_seeds: Option<Vec<RecurrenceIdentitySeed>>,
}

/// Map a complete CalDAV resource and derive recurrence identity only from
/// explicit, validated provider properties.
fn parse_caldav_resource(
    event: &CalDavEvent,
    account: &AccountFull,
    calendar_id: &str,
    provider_calendar_id: &str,
) -> Option<ParsedCalDavResource> {
    let parsed = ical::parse_ical_data_with_recurrence(&event.ical_data);
    let representative = parsed
        .iter()
        .find(|component| component.invite.recurrence_kind == RecurrenceKind::Series)
        .or_else(|| parsed.first())?;
    let invite = &representative.invite;
    let attendees_json = if invite.attendees.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&invite.attendees).unwrap_or_else(|_| "[]".to_string()))
    };

    let cal_event = CalendarEvent {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: account.id.clone(),
        calendar_id: calendar_id.to_string(),
        uid: Some(event.uid.clone()),
        title: invite
            .summary
            .clone()
            .unwrap_or_else(|| "(No title)".to_string()),
        description: invite.description.clone(),
        location: invite.location.clone(),
        start_time: invite.dtstart.clone(),
        end_time: invite.dtend.clone(),
        all_day: invite.all_day,
        timezone: invite.timezone.clone(),
        recurrence_rule: invite.recurrence_rule.clone(),
        recurrence_kind: invite.recurrence_kind,
        organizer_email: invite.organizer_email.clone(),
        attendees_json,
        my_status: attendee_status_for_email(&invite.attendees, &account.email),
        source_message_id: None,
        ical_data: Some(event.ical_data.clone()),
        remote_id: Some(event.href.clone()),
        etag: Some(event.etag.clone()),
    };
    #[cfg(test)]
    let recurrence_seeds = recurrence_seeds(event, provider_calendar_id, &parsed);
    #[cfg(not(test))]
    let _ = provider_calendar_id;
    Some(ParsedCalDavResource {
        event: cal_event,
        #[cfg(test)]
        recurrence_seeds,
    })
}

/// Map only the display event for callers that do not ingest recurrence rows.
#[cfg(test)]
fn from_caldav_event(
    event: &CalDavEvent,
    account: &AccountFull,
    calendar_id: &str,
    provider_calendar_id: &str,
) -> Option<CalendarEvent> {
    parse_caldav_resource(event, account, calendar_id, provider_calendar_id)
        .map(|parsed| parsed.event)
}

#[cfg(test)]
fn recurrence_seeds(
    resource: &CalDavEvent,
    provider_calendar_id: &str,
    components: &[ical::ParsedIcalEvent],
) -> Option<Vec<RecurrenceIdentitySeed>> {
    recurrence_seeds_from_parts(
        &resource.ical_data,
        Some(&resource.etag),
        &resource.href,
        provider_calendar_id,
        components,
    )
}

fn recurrence_seeds_from_parts(
    ical_data: &str,
    provider_revision: Option<&str>,
    provider_series_id: &str,
    provider_calendar_id: &str,
    components: &[ical::ParsedIcalEvent],
) -> Option<Vec<RecurrenceIdentitySeed>> {
    if components.len() == 1 && components[0].invite.recurrence_kind == RecurrenceKind::Standalone {
        return Some(Vec::new());
    }

    let uid = &components.first()?.invite.uid;
    if components.iter().any(|component| {
        component.invite.uid != *uid
            || matches!(
                component.invite.recurrence_kind,
                RecurrenceKind::Standalone | RecurrenceKind::Unknown
            )
    }) {
        return None;
    }

    let masters = components
        .iter()
        .filter(|component| component.invite.recurrence_kind == RecurrenceKind::Series)
        .count();
    if masters > 1 {
        return None;
    }

    let mut seeds = Vec::with_capacity(components.len());
    for component in components {
        let occurrence = occurrence_fields(&component.invite)?;
        let seed = match component.invite.recurrence_kind {
            RecurrenceKind::Series => RecurrenceIdentitySeed {
                local_series_event_id: None,
                provider_calendar_id: Some(provider_calendar_id.to_string()),
                provider_series_id: Some(provider_series_id.to_string()),
                provider_occurrence_id: None,
                recurrence_id: None,
                recurrence_timezone: None,
                recurrence_value_type: None,
                occurrence,
                provider_native_data: Some(ical_data.to_string()),
                provider_revision: provider_revision.map(str::to_owned),
                kind: RecurrenceObjectKind::Master,
            },
            RecurrenceKind::Occurrence => {
                let recurrence_id = component.recurrence_id.as_ref()?;
                RecurrenceIdentitySeed {
                    local_series_event_id: None,
                    provider_calendar_id: Some(provider_calendar_id.to_string()),
                    provider_series_id: Some(provider_series_id.to_string()),
                    provider_occurrence_id: None,
                    recurrence_id: Some(recurrence_id.raw_value.clone()),
                    recurrence_timezone: recurrence_id.timezone.clone(),
                    recurrence_value_type: Some(recurrence_id.value_type),
                    occurrence,
                    provider_native_data: Some(ical_data.to_string()),
                    provider_revision: provider_revision.map(str::to_owned),
                    kind: if component.explicitly_cancelled {
                        RecurrenceObjectKind::Exclusion
                    } else {
                        RecurrenceObjectKind::Exception
                    },
                }
            }
            RecurrenceKind::Standalone | RecurrenceKind::Unknown => return None,
        };
        if seed.validate().is_err() {
            return None;
        }
        seeds.push(seed);
    }
    Some(seeds)
}

fn occurrence_fields(invite: &ical::ParsedInvite) -> Option<OccurrenceFields> {
    if invite.all_day {
        let start = chrono::NaiveDate::parse_from_str(&invite.dtstart, "%Y-%m-%d").ok()?;
        let mut end = chrono::NaiveDate::parse_from_str(&invite.dtend, "%Y-%m-%d").ok()?;
        if end == start {
            end = start.succ_opt()?;
        }
        if end <= start {
            return None;
        }
        return Some(OccurrenceFields {
            title: invite
                .summary
                .clone()
                .unwrap_or_else(|| "(No title)".to_string()),
            description: invite.description.clone(),
            location: invite.location.clone(),
            start_time: start.format("%Y-%m-%d").to_string(),
            end_time: end.format("%Y-%m-%d").to_string(),
            all_day: true,
            timezone: invite.timezone.clone(),
        });
    }

    let start = chrono::DateTime::parse_from_rfc3339(&invite.dtstart).ok()?;
    let end = chrono::DateTime::parse_from_rfc3339(&invite.dtend).ok()?;
    if end <= start {
        return None;
    }
    Some(OccurrenceFields {
        title: invite
            .summary
            .clone()
            .unwrap_or_else(|| "(No title)".to_string()),
        description: invite.description.clone(),
        location: invite.location.clone(),
        start_time: start
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        end_time: end
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        all_day: false,
        timezone: invite.timezone.clone(),
    })
}

fn validate_occurrence_update<'a>(
    account: &AccountFull,
    request: &'a RemoteOccurrenceUpdate,
) -> Result<(
    &'a str,
    &'a str,
    RecurrenceValueType,
    Option<&'a str>,
    &'a str,
)> {
    request.trusted_identity.validate()?;
    request.desired.validate()?;
    let identity = &request.trusted_identity;
    if !matches!(
        identity.kind,
        RecurrenceObjectKind::Occurrence | RecurrenceObjectKind::Exception
    ) || identity.account_id != account.id
        || identity.event_id != request.current_event.id
        || request.current_event.account_id != account.id
        || request.current_event.recurrence_kind != RecurrenceKind::Series
        || request.current_event.remote_id.as_deref() != Some(request.target_id.as_str())
        || identity.provider_series_id.as_deref() != Some(request.target_id.as_str())
        || identity.provider_occurrence_id.is_some()
    {
        return Err(Error::Other(
            "CalDAV THIS-OCCURRENCE requires a trusted embedded exception resource".into(),
        ));
    }
    let etag = request
        .expected_provider_revision
        .as_deref()
        .filter(|value| !value.trim().is_empty() && !value.chars().any(char::is_control))
        .ok_or_else(|| {
            Error::Sync("CalDAV occurrence has no expected ETag; sync before retrying".into())
        })?;
    if identity.provider_revision.as_deref() != Some(etag) {
        return Err(Error::Sync(
            "CalDAV occurrence ETag contradicts its trusted identity; reconciliation required"
                .into(),
        ));
    }
    let uid = request
        .current_event
        .uid
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Other("CalDAV occurrence resource has no UID".into()))?;
    let recurrence_id = identity
        .recurrence_id
        .as_deref()
        .ok_or_else(|| Error::Other("CalDAV occurrence has no RECURRENCE-ID".into()))?;
    let value_type = identity
        .recurrence_value_type
        .ok_or_else(|| Error::Other("CalDAV occurrence has no recurrence value type".into()))?;
    let native = identity
        .provider_native_data
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Error::Other("CalDAV occurrence has no complete native VCALENDAR".into()))?;
    Ok((
        uid,
        recurrence_id,
        value_type,
        identity.recurrence_timezone.as_deref(),
        native,
    ))
}

/// Decode a complete REPORT for display, independently of occurrence-edit
/// classification. A bad resource replaces its stale projection with an error.
fn mirrored_resources(
    client: &CalDavClient,
    calendar: &str,
    account: &AccountFull,
    local_calendar: &str,
    events: &[CalDavEvent],
) -> Result<Vec<db::calendar_sync::Resource>> {
    let native = |event: &CalDavEvent| NativeCalendarResource {
        protocol: "caldav".into(),
        calendar_id: calendar.into(),
        event_id: event.href.clone(),
        revision: Some(event.etag.clone()),
        data: event.ical_data.clone(),
    };
    let mut groups = std::collections::BTreeMap::<String, Vec<&CalDavEvent>>::new();
    let mut output = Vec::new();
    for event in events {
        client.validate_resource_href(calendar, &event.href)?;
        match resource::resource_uid(&event.ical_data) {
            Ok(uid) => groups.entry(uid).or_default().push(event),
            Err(error) => output.push(db::calendar_sync::Resource::unsupported(
                native(event),
                error,
            )),
        }
    }
    for (uid, group) in groups {
        let resources = group.iter().map(|event| native(event)).collect::<Vec<_>>();
        let decoded = (|| -> Result<CalendarEventSet> {
            let mut template = group
                .iter()
                .find_map(|event| {
                    parse_caldav_resource(event, account, local_calendar, calendar)
                        .map(|parsed| parsed.event)
                })
                .ok_or_else(|| {
                    Error::Sync("CalDAV resource has no readable event fields".into())
                })?;
            template.uid = Some(uid);
            let mut set = resource::combine_display(&resources, &template)?;
            set.event.my_status = crate::calendar::attendee_status_from_json(
                set.event.attendees_json.as_deref(),
                &account.email,
            );
            for item in &mut set.overrides {
                if let Some(event) = &mut item.event {
                    event.my_status = crate::calendar::attendee_status_from_json(
                        event.attendees_json.as_deref(),
                        &account.email,
                    );
                }
            }
            Ok(set)
        })();
        match decoded {
            Ok(set) => {
                let master = set
                    .native
                    .as_ref()
                    .expect("CalDAV decoded resource")
                    .event_id
                    .clone();
                output.push(db::calendar_sync::Resource::readable(set)?);
                output.extend(
                    resources
                        .into_iter()
                        .filter(|native| native.event_id != master)
                        .map(|native| db::calendar_sync::Resource {
                            native,
                            set: None,
                            error: None,
                        }),
                );
            }
            Err(error) => output.extend(
                resources
                    .into_iter()
                    .map(|native| db::calendar_sync::Resource::unsupported(native, &error)),
            ),
        }
    }
    Ok(output)
}

/// Connect with the account's DAV coordinates.
pub(super) async fn connect(
    ctx: &CalendarBackendCtx<'_>,
    account: &AccountFull,
) -> Result<CalDavClient> {
    let caldav_config = CalDavConfig {
        caldav_url: account.caldav_url.clone(),
        username: account.username.clone(),
        password: account.password.clone(),
        email: account.email.clone(),
    };
    ctx.services.caldav_client(&caldav_config).await
}

#[async_trait]
impl CalendarBackend for CalDavCalendarBackend {
    fn protocol(&self) -> &'static str {
        "caldav"
    }

    fn recurring_import_fidelity(&self) -> super::RecurringImportFidelity {
        super::RecurringImportFidelity::RawIcalendar
    }

    async fn fetch_event_set(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        event: &CalendarEvent,
        calendar: &str,
    ) -> Result<CalendarEventSet> {
        event_sets::fetch(ctx, account, event, calendar).await
    }

    async fn update_event_set(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        before: &CalendarEventSet,
        desired: &CalendarEventSet,
    ) -> Result<CalendarEventSet> {
        event_sets::update(ctx, account, before, desired).await
    }

    async fn create_event_set(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        calendar: &str,
        desired: &CalendarEventSet,
        operation: &str,
    ) -> Result<CalendarEventSet> {
        event_sets::create(ctx, account, calendar, desired, operation).await
    }

    async fn delete_event_set(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        before: &CalendarEventSet,
    ) -> Result<()> {
        event_sets::delete(ctx, account, before).await
    }

    async fn move_event_set_native(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        before: &CalendarEventSet,
        calendar: &str,
    ) -> Result<super::CalendarCapability<CalendarEventSet>> {
        event_sets::move_native(ctx, account, before, calendar).await
    }

    async fn push_updated_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        event: &CalendarEvent,
    ) -> Result<()> {
        event_sets::ordinary_update(ctx, account, remote_id, event).await
    }

    async fn sync(&self, ctx: &CalendarBackendCtx<'_>, account: &AccountFull) -> Result<()> {
        let db = ctx.db;
        let account_id = account.id.as_str();
        let client = connect(ctx, account).await?;

        // Step 1: List calendars from server
        let caldav_calendars = client.list_calendars().await?;
        log::info!(
            "sync_calendars: fetched {} calendars from CalDAV for account {}",
            caldav_calendars.len(),
            account_id
        );

        // Build a mapping from remote calendar href to (local id, is_subscribed)
        // — same shape as the Graph sync (#47). The is_subscribed flag
        // is preserved across re-syncs by upsert_calendar_by_remote_id, so
        // we read it back from the DB after upserting and use it below to
        // skip event sync for calendars the user has unsubscribed from.
        // Without this skip, unsubscribing a calendar drops its events
        // (via unsubscribe_calendar) but the very next sync re-pulls them
        // and they show up as "ghost" events.
        let mut remote_to_local: std::collections::HashMap<String, (String, bool)> =
            std::collections::HashMap::new();

        {
            let conn = db.writer().await;
            for (idx, cal) in caldav_calendars.iter().enumerate() {
                let color = cal.color.as_deref().unwrap_or("#4285f4");
                let is_default = idx == 0; // First calendar is default
                let local_id = db::calendar::upsert_calendar_by_remote_id(
                    &conn, account_id, &cal.href, &cal.name, color, is_default,
                )?;
                // Propagate the read error rather than swallowing it as
                // `subscribed = true`: the row was just upserted above so a
                // failure here means the DB itself is misbehaving and the
                // sync should abort rather than blindly re-pull events the
                // user has unsubscribed from.
                let subscribed: bool = conn.query_row(
                    "SELECT is_subscribed FROM calendars WHERE id = ?1",
                    rusqlite::params![local_id],
                    |row| row.get(0),
                )?;
                remote_to_local.insert(cal.href.clone(), (local_id, subscribed));
            }
        }

        // Step 2: For each subscribed calendar, fetch events and upsert into local DB
        let mut sync_failures = Vec::new();
        for cal in &caldav_calendars {
            let Some((local_cal_id, subscribed)) = remote_to_local.get(&cal.href) else {
                continue;
            };
            if !subscribed {
                log::debug!(
                    "sync_calendars_caldav: skipping unsubscribed calendar '{}'",
                    cal.name
                );
                continue;
            }
            let caldav_events = match client.fetch_events(&cal.href).await {
                Ok(evts) => evts,
                Err(e) => {
                    log::error!(
                        "sync_calendars: failed to fetch CalDAV events for calendar '{}': {}",
                        cal.name,
                        e
                    );
                    sync_failures.push(format!("{}: {e}", cal.name));
                    continue;
                }
            };

            log::info!(
                "sync_calendars: fetched {} events from CalDAV calendar '{}'",
                caldav_events.len(),
                cal.name
            );

            let resources =
                mirrored_resources(&client, &cal.href, account, local_cal_id, &caldav_events)?;
            for resource in &resources {
                if let Some(error) = &resource.error {
                    sync_failures.push(format!(
                        "{}: resource {} cannot be displayed: {error}",
                        cal.name, resource.native.event_id
                    ));
                }
            }
            let mut conn = db.writer().await;
            let tx = conn.transaction()?;
            db::calendar_sync::replace_calendar(
                &tx,
                account_id,
                local_cal_id,
                "caldav",
                &resources,
            )?;
            tx.commit()?;
        }

        {
            let mut conn = db.writer().await;
            let tx = conn.transaction()?;
            db::calendar_sync::replace_inventory(
                &tx,
                account_id,
                &remote_to_local.keys().map(String::as_str).collect(),
            )?;
            tx.commit()?;
        }

        if sync_failures.is_empty() {
            Ok(())
        } else {
            Err(Error::Sync(format!(
                "CalDAV mirror updated, but {} calendar/resource read(s) failed: {}",
                sync_failures.len(),
                sync_failures.join("; ")
            )))
        }
    }

    fn validate_event_creation(&self, event: &CalendarEvent, _: &str) -> Result<()> {
        creation_ical_data(event, event.uid.as_deref().unwrap_or(&event.id)).map(|_| ())
    }

    async fn push_created_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        event: &CalendarEvent,
        remote_calendar_id: &str,
    ) -> Result<Option<PushedEvent>> {
        let uid = event
            .uid
            .clone()
            .unwrap_or_else(|| format!("{}@chithi", uuid::Uuid::new_v4()));
        let data = if event.source_message_id.is_some()
            && event.organizer_email.is_none()
            && event.attendees_json.is_none()
        {
            personal_copy_ical_data(event, &uid)?
        } else {
            creation_ical_data(event, &uid)?
        };
        let client = connect(ctx, account).await?;
        let pushed = client.put_event(remote_calendar_id, &uid, &data).await?;
        Ok(Some(PushedEvent {
            remote_id: pushed.href,
            canonical_uid: Some(pushed.uid),
            etag: pushed.etag,
        }))
    }

    async fn update_recurrence_occurrence(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        request: &RemoteOccurrenceUpdate,
    ) -> Result<RemoteOccurrenceUpdateOutcome> {
        let (uid, recurrence_id, value_type, recurrence_timezone, _native) =
            validate_occurrence_update(account, request)?;
        let rewrite_request = request.clone();
        let updated =
            tokio::task::spawn_blocking(move || resource::rewrite_occurrence(&rewrite_request))
                .await
                .map_err(|e| Error::Other(format!("CalDAV resource preparation failed: {e}")))??;
        let etag = request
            .expected_provider_revision
            .as_deref()
            .ok_or_else(|| Error::Sync("Missing CalDAV validator".into()))?;
        let client = connect(ctx, account).await?;
        let etag = client
            .strong_revision_for(&request.target_id, Some(etag), Some(_native))
            .await?;
        let put_etag = client
            .put_event_at_href(&request.target_id, &updated, Some(&etag))
            .await?;
        let canonical = client
            .get_event_at_href(&request.target_id)
            .await
            .map_err(|error| {
                Error::Sync(format!(
                    "CalDAV accepted the occurrence update but canonical GET failed; reconciliation required: {error}"
                ))
            })?;
        ical::select_recurrence_occurrence(
            &canonical.ical_data,
            uid,
            recurrence_id,
            value_type,
            recurrence_timezone,
        )
        .map_err(|error| {
            Error::Sync(format!(
                "CalDAV accepted the occurrence update but its canonical resource cannot be reconciled: {error}"
            ))
        })?;
        let components = ical::parse_ical_data_with_recurrence(&canonical.ical_data);
        let provider_calendar_id = request
            .trusted_identity
            .provider_calendar_id
            .as_deref()
            .ok_or_else(|| {
                Error::Sync(
                    "CalDAV canonical occurrence has no provider calendar identity; reconciliation required"
                        .into(),
                )
            })?;
        let mut canonical_recurrence_objects = recurrence_seeds_from_parts(
            &canonical.ical_data,
            canonical.etag.as_deref().or(put_etag.as_deref()),
            &request.target_id,
            provider_calendar_id,
            &components,
        )
        .filter(|seeds| !seeds.is_empty())
        .ok_or_else(|| {
            Error::Sync(
                "CalDAV accepted the occurrence update but its complete canonical recurrence set is invalid; reconciliation required"
                    .into(),
            )
        })?;
        for seed in &mut canonical_recurrence_objects {
            seed.local_series_event_id = request.trusted_identity.local_series_event_id.clone();
        }
        let matches: Vec<&RecurrenceIdentitySeed> = canonical_recurrence_objects
            .iter()
            .filter(|seed| {
                seed.local_series_event_id == request.trusted_identity.local_series_event_id
                    && seed.provider_calendar_id == request.trusted_identity.provider_calendar_id
                    && seed.provider_series_id == request.trusted_identity.provider_series_id
                    && seed.provider_occurrence_id
                        == request.trusted_identity.provider_occurrence_id
                    && seed.recurrence_id == request.trusted_identity.recurrence_id
                    && seed.recurrence_timezone == request.trusted_identity.recurrence_timezone
                    && seed.recurrence_value_type == request.trusted_identity.recurrence_value_type
            })
            .collect();
        let [replacement_identity] = matches.as_slice() else {
            return Err(Error::Sync(
                "CalDAV canonical recurrence set must contain exactly one matching immutable occurrence identity; reconciliation required"
                    .into(),
            ));
        };
        if replacement_identity.kind != RecurrenceObjectKind::Exception {
            return Err(Error::Sync(
                "CalDAV canonical selected occurrence is not an exception; reconciliation required"
                    .into(),
            ));
        }
        let replacement_identity = (*replacement_identity).clone();
        let occurrence = replacement_identity.occurrence.clone();
        replacement_identity.validate()?;
        Ok(RemoteOccurrenceUpdateOutcome {
            replacement_identity,
            occurrence,
            canonical_event: None,
            canonical_recurrence_objects: Some(canonical_recurrence_objects),
        })
    }

    async fn push_updated_invitation_copy(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        event: &CalendarEvent,
    ) -> Result<Option<String>> {
        let uid = event
            .uid
            .as_deref()
            .ok_or_else(|| Error::Other("The personal copy has no UID".into()))?;
        let data = personal_copy_ical_data(event, uid)?;
        let client = connect(ctx, account).await?;
        let revision = client
            .strong_revision_for(remote_id, event.etag.as_deref(), event.ical_data.as_deref())
            .await?;
        client
            .put_event_at_href(remote_id, &data, Some(&revision))
            .await
    }

    async fn push_deleted_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        _remote_calendar_id: &str,
    ) -> Result<()> {
        let client = connect(ctx, account).await?;
        client.delete_event(remote_id).await
    }

    async fn push_calendar_rename(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        name: &str,
    ) -> Result<()> {
        let client = connect(ctx, account).await?;
        client.rename_calendar(remote_id, name).await
    }

    async fn push_calendar_color(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        color: &str,
    ) -> Result<()> {
        let client = connect(ctx, account).await?;
        client.set_calendar_color(remote_id, color).await
    }
}

#[cfg(test)]
mod recurrence_tests {
    use super::*;
    use crate::backend::testutil::{account, event, temp_pool};
    use crate::calendar::recurrence_identity::{RecurrenceIdentity, UpdateOccurrenceInput};
    use crate::calendar::RecurrenceKind;
    use crate::db;
    use crate::db::pool::DbPool;
    use crate::mail::caldav::CalDavEvent;
    use crate::provider::ProviderServices;
    use rusqlite::Connection;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn component(properties: &str) -> String {
        format!(
            "BEGIN:VEVENT\nUID:shared\nDTSTAMP:20260901T120000Z\n\
             DTSTART:20260913T100000Z\nDTEND:20260913T110000Z\n\
             SUMMARY:Visible event\n{properties}END:VEVENT\n"
        )
    }

    fn resource(components: &str) -> CalDavEvent {
        CalDavEvent {
            href: "/calendar/event.ics".into(),
            etag: "unchanged-etag".into(),
            uid: "shared".into(),
            ical_data: format!(
                "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//Chithi//EN\n\
                 {components}END:VCALENDAR\n"
            ),
        }
    }

    async fn upload_db() -> (tempfile::TempDir, DbPool) {
        let (dir, db) = temp_pool();
        {
            let conn = db.writer().await;
            db::schema::initialize(&conn).unwrap();
            conn.execute_batch(
                "INSERT INTO accounts (id, display_name, email, username)
                 VALUES ('acc1', 'Test', 'u@example.com', 'u@example.com');
                 INSERT INTO calendars (id, account_id, name, remote_id)
                 VALUES ('cal1', 'acc1', 'Calendar', '/calendar/');",
            )
            .unwrap();
        }
        (dir, db)
    }

    fn local_series() -> CalendarEvent {
        CalendarEvent {
            recurrence_kind: RecurrenceKind::Series,
            recurrence_rule: Some("FREQ=WEEKLY;COUNT=4".into()),
            ..event()
        }
    }

    fn insert_proven_series(conn: &mut Connection, event: &CalendarEvent) {
        let transaction = conn.transaction().unwrap();
        db::calendar::insert_event(&transaction, event).unwrap();
        db::calendar_invitation::record_local_series(&transaction, event).unwrap();
        transaction.commit().unwrap();
        assert!(db::calendar_invitation::validated_series_rule(conn, event).is_ok());
    }

    fn proof_count(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM calendar_invitation_recurrence",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn injected_services() -> ProviderServices {
        let mut services = crate::backend::calendar::google::sync_testutil::services("");
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-injected-client",
            reqwest::header::HeaderValue::from_static("caldav-upload-test"),
        );
        services.transports.dav_http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .default_headers(headers.clone())
            .build()
            .unwrap();
        services.transports.dav_resource_http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .default_headers(headers)
            .build()
            .unwrap();
        services
    }

    fn multistatus(body: &str) -> String {
        format!(
            "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">\
             {body}</d:multistatus>"
        )
    }

    fn sync_responses(events: &str) -> Vec<(&'static str, u16, String)> {
        vec![
            (
                "PROPFIND",
                207,
                multistatus(
                    "<d:response><d:propstat><d:prop><d:current-user-principal>\
                 <d:href>/principal/</d:href></d:current-user-principal>\
                 </d:prop></d:propstat></d:response>",
                ),
            ),
            (
                "PROPFIND",
                207,
                multistatus(
                    "<d:response><d:propstat><d:prop><c:calendar-home-set>\
                 <d:href>/home/</d:href></c:calendar-home-set>\
                 </d:prop></d:propstat></d:response>",
                ),
            ),
            (
                "PROPFIND",
                207,
                multistatus(
                    "<d:response><d:href>/calendar/</d:href><d:propstat><d:prop>\
                 <d:displayname>Calendar</d:displayname>\
                 <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>\
                 </d:prop></d:propstat></d:response>",
                ),
            ),
            ("REPORT", 207, multistatus(events)),
        ]
    }

    fn report_events(events: &[CalDavEvent]) -> String {
        events
            .iter()
            .map(|event| {
                format!(
                    "<d:response><d:href>{}</d:href><d:propstat><d:prop>\
             <d:getetag>{}</d:getetag><c:calendar-data><![CDATA[{}]]></c:calendar-data>\
             </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    event.href, event.etag, event.ical_data,
                )
            })
            .collect::<String>()
    }

    fn split_series() -> (CalDavEvent, CalDavEvent) {
        let master = CalDavEvent {
            href: "/calendar/master.ics".into(),
            ..resource(&component("RRULE:FREQ=WEEKLY;COUNT=3\n"))
        };
        let detached = CalDavEvent {
            href: "/calendar/detached.ics".into(),
            ical_data: resource(&component("RECURRENCE-ID:20260920T100000Z\n"))
                .ical_data
                .replace("DTSTART:20260913T100000Z", "DTSTART:20260921T120000Z")
                .replace("DTEND:20260913T110000Z", "DTEND:20260921T130000Z"),
            ..master.clone()
        };
        (master, detached)
    }

    fn projected_series(
        db: &DbPool,
        master_id: &str,
        start: &str,
        end: &str,
    ) -> crate::calendar::actions::CalendarOccurrencePage {
        let conn = db.reader();
        let owner = db::calendar::get_event(&conn, master_id).unwrap();
        let set = db::calendar_sync::displayed_set(&conn, master_id)
            .unwrap()
            .unwrap();
        crate::calendar::actions::project(&set, &owner, "test", start, end, 100).unwrap()
    }

    #[tokio::test]
    async fn display_sync_keeps_readable_resources_when_upstream_content_is_not_editable() {
        let (_dir, db) = upload_db().await;
        let payloads = [
            (
                "untitled",
                component("RRULE:FREQ=DAILY;COUNT=2\n").replace("SUMMARY:Visible event\n", ""),
            ),
            (
                "instant",
                component("").replace("DTEND:20260913T110000Z\n", ""),
            ),
            (
                "redundant",
                component("RRULE:FREQ=DAILY;COUNT=2\nRRULE:FREQ=DAILY;COUNT=2\n")
                    .replace("DTSTART:", "DTSTART;TZID=Europe/Stockholm:"),
            ),
            (
                "unknown",
                component("").replace(
                    "DTSTART:20260913T100000Z",
                    "DTSTART;TZID=Unknown/Zone:20260913T100000",
                ),
            ),
        ];
        let events = payloads
            .into_iter()
            .map(|(id, data)| CalDavEvent {
                href: format!("/calendar/{id}.ics"),
                uid: id.into(),
                ical_data: resource(&data.replace("UID:shared", &format!("UID:{id}"))).ical_data,
                ..resource("")
            })
            .collect::<Vec<_>>();
        let (root, captured) = serve_dav(sync_responses(&report_events(&events))).await;
        let error = sync_at(&db, &root).await.unwrap_err();
        assert!(error.to_string().contains("unknown native timezone"));
        assert_eq!(captured.await.unwrap().len(), 4);
        let conn = db.reader();
        let displayed =
            db::calendar::list_events(&conn, "acc1", None, "2026-09-13", "2026-09-15").unwrap();
        assert_eq!(displayed.len(), 3);
        let mut total = 0;
        for event in displayed {
            let set = db::calendar_sync::displayed_set(&conn, &event.id)
                .unwrap()
                .unwrap();
            let page = crate::calendar::actions::project_display(
                &set,
                &event,
                "",
                "2026-09-13",
                "2026-09-15",
                10,
            )
            .unwrap();
            total += page.occurrences.len();
        }
        assert_eq!(total, 5);
    }

    #[tokio::test]
    async fn split_moved_exception_syncs_as_one_series_and_recovers_after_removal() {
        let (_dir, db) = upload_db().await;
        let (master, detached) = split_series();
        // Simulate the independent rows left by older versions of sync.
        let detached_id = {
            let conn = db.writer().await;
            let account = account("calendar", "caldav");
            let parsed = parse_caldav_resource(&detached, &account, "cal1", "/calendar/").unwrap();
            db::calendar::upsert_event_by_remote_id_with_recurrence(
                &conn,
                &parsed.event,
                parsed.recurrence_seeds.as_deref().unwrap(),
            )
            .unwrap()
        };
        let mut master_id = String::new();
        for _ in 0..2 {
            let (root, captured) = serve_dav(sync_responses(&report_events(&[
                detached.clone(),
                master.clone(),
            ])))
            .await;
            sync_at(&db, &root).await.unwrap();
            assert_eq!(captured.await.unwrap().len(), 4);
            let conn = db.reader();
            let current_master: String = conn
                .query_row(
                    "SELECT id FROM calendar_events WHERE remote_id = '/calendar/master.ics'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            if !master_id.is_empty() {
                assert_eq!(current_master, master_id);
            }
            master_id = current_master;
            assert!(db::calendar::get_event(&conn, &detached_id).is_err());
            assert_eq!(conn.query_row(
                "SELECT COUNT(*) FROM calendar_events WHERE account_id = 'acc1' AND calendar_id = 'cal1'",
                [], |row| row.get::<_, i64>(0),
            ).unwrap(), 1);
            drop(conn);
            let page = projected_series(
                &db,
                &master_id,
                "2026-09-20T00:00:00Z",
                "2026-09-22T00:00:00Z",
            );
            assert_eq!(page.occurrences.len(), 1);
            assert_eq!(page.occurrences[0].event_id, master_id);
            assert_eq!(
                page.occurrences[0].selection.original_start.as_deref(),
                Some("2026-09-20T10:00:00Z")
            );
            assert_eq!(
                page.occurrences[0].fields.start_time,
                "2026-09-21T12:00:00Z"
            );
        }

        let (root, captured) = serve_dav(sync_responses(&report_events(&[master]))).await;
        sync_at(&db, &root).await.unwrap();
        assert_eq!(captured.await.unwrap().len(), 4);
        let page = projected_series(
            &db,
            &master_id,
            "2026-09-20T00:00:00Z",
            "2026-09-22T00:00:00Z",
        );
        assert_eq!(page.occurrences.len(), 1);
        assert_eq!(
            page.occurrences[0].fields.start_time,
            "2026-09-20T10:00:00Z"
        );
        let conn = db.reader();
        assert_eq!(conn.query_row(
            "SELECT COUNT(*) FROM calendar_action_addresses WHERE remote_id = '/calendar/detached.ics'",
            [], |row| row.get::<_, i64>(0),
        ).unwrap(), 0);
    }

    #[tokio::test]
    async fn split_exception_projects_only_at_its_effective_position_across_windows() {
        let (_dir, db) = upload_db().await;
        let (master, detached) = split_series();
        let (root, captured) = serve_dav(sync_responses(&report_events(&[master, detached]))).await;
        sync_at(&db, &root).await.unwrap();
        assert_eq!(captured.await.unwrap().len(), 4);
        let conn = db.reader();
        let master_id: String = conn
            .query_row(
                "SELECT id FROM calendar_events WHERE remote_id = '/calendar/master.ics'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        assert!(projected_series(
            &db,
            &master_id,
            "2026-09-20T00:00:00Z",
            "2026-09-21T00:00:00Z"
        )
        .occurrences
        .is_empty());
        let moved_in = projected_series(
            &db,
            &master_id,
            "2026-09-21T00:00:00Z",
            "2026-09-22T00:00:00Z",
        );
        assert_eq!(moved_in.occurrences.len(), 1);
        assert_eq!(
            moved_in.occurrences[0].selection.original_start.as_deref(),
            Some("2026-09-20T10:00:00Z")
        );
    }

    #[tokio::test]
    async fn embedded_moved_exception_uses_one_owner_without_detached_rows() {
        let (_dir, db) = upload_db().await;
        let (mut master, detached) = split_series();
        let exception = detached
            .ical_data
            .split_once("BEGIN:VEVENT\n")
            .unwrap()
            .1
            .split_once("END:VEVENT\n")
            .unwrap()
            .0;
        master.ical_data = master.ical_data.replace(
            "END:VCALENDAR\n",
            &format!("BEGIN:VEVENT\n{exception}END:VEVENT\nEND:VCALENDAR\n"),
        );
        let (root, captured) = serve_dav(sync_responses(&report_events(&[master]))).await;
        sync_at(&db, &root).await.unwrap();
        assert_eq!(captured.await.unwrap().len(), 4);
        let conn = db.reader();
        let master_id: String = conn
            .query_row(
                "SELECT id FROM calendar_events WHERE remote_id = '/calendar/master.ics'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(conn.query_row(
            "SELECT COUNT(*) FROM calendar_events WHERE account_id = 'acc1' AND calendar_id = 'cal1'",
            [], |row| row.get::<_, i64>(0),
        ).unwrap(), 1);
        drop(conn);
        let page = projected_series(
            &db,
            &master_id,
            "2026-09-20T00:00:00Z",
            "2026-09-22T00:00:00Z",
        );
        assert_eq!(page.occurrences.len(), 1);
        assert_eq!(
            page.occurrences[0].fields.start_time,
            "2026-09-21T12:00:00Z"
        );
    }

    #[tokio::test]
    async fn removed_split_series_does_not_upload_orphaned_detached_members() {
        let (_dir, db) = upload_db().await;
        let (master, detached) = split_series();
        let (root, captured) = serve_dav(sync_responses(&report_events(&[master, detached]))).await;
        sync_at(&db, &root).await.unwrap();
        assert_eq!(captured.await.unwrap().len(), 4);
        let (root, captured) = serve_dav(sync_responses("")).await;
        sync_at(&db, &root).await.unwrap();
        assert_eq!(captured.await.unwrap().len(), 4);
        let conn = db.reader();
        assert_eq!(conn.query_row(
            "SELECT COUNT(*) FROM calendar_events WHERE account_id = 'acc1' AND calendar_id = 'cal1'",
            [], |row| row.get::<_, i64>(0),
        ).unwrap(), 0);
        assert_eq!(conn.query_row(
            "SELECT COUNT(*) FROM calendar_action_addresses WHERE account_id = 'acc1' AND calendar_id = 'cal1' AND retired = 0",
            [], |row| row.get::<_, i64>(0),
        ).unwrap(), 0);
    }

    #[tokio::test]
    async fn invalid_split_group_preserves_last_canonical_set_and_owner() {
        let (_dir, db) = upload_db().await;
        let (master, detached) = split_series();
        let (root, captured) = serve_dav(sync_responses(&report_events(&[
            master.clone(),
            detached.clone(),
        ])))
        .await;
        sync_at(&db, &root).await.unwrap();
        assert_eq!(captured.await.unwrap().len(), 4);
        let conn = db.reader();
        let master_id: String = conn
            .query_row(
                "SELECT id FROM calendar_events WHERE remote_id = '/calendar/master.ics'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        let invalid = [
            vec![detached.clone()],
            vec![
                master.clone(),
                CalDavEvent {
                    href: "/calendar/second-master.ics".into(),
                    ..master.clone()
                },
            ],
            vec![
                master.clone(),
                CalDavEvent {
                    ical_data: detached.ical_data.replace(
                        "RECURRENCE-ID:20260920T100000Z",
                        "RECURRENCE-ID;RANGE=THISANDFUTURE:20260920T100000Z",
                    ),
                    ..detached.clone()
                },
            ],
        ];
        for events in invalid {
            let (root, captured) = serve_dav(sync_responses(&report_events(&events))).await;
            sync_at(&db, &root).await.unwrap_err();
            assert_eq!(captured.await.unwrap().len(), 4);
            let conn = db.reader();
            assert!(db::calendar_sync::displayed_set(&conn, &master_id)
                .unwrap()
                .is_none());
            assert!(db::calendar::get_event(&conn, &master_id).is_err());
            assert!(
                conn.query_row(
                    "SELECT COUNT(*) FROM calendar_remote_resources WHERE error IS NOT NULL",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap()
                    > 0
            );
        }
    }

    #[tokio::test]
    async fn unclassifiable_report_resource_cannot_make_another_uid_look_complete() {
        let (_dir, db) = upload_db().await;
        let (master, detached) = split_series();
        let (root, captured) = serve_dav(sync_responses(&report_events(&[
            master.clone(),
            detached.clone(),
        ])))
        .await;
        sync_at(&db, &root).await.unwrap();
        assert_eq!(captured.await.unwrap().len(), 4);
        let conn = db.reader();
        let master_id: String = conn
            .query_row(
                "SELECT id FROM calendar_events WHERE remote_id = '/calendar/master.ics'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        let changed_master = CalDavEvent {
            ical_data: master
                .ical_data
                .replace("SUMMARY:Visible event", "SUMMARY:Changed"),
            ..master
        };
        let invalid = CalDavEvent {
            href: "/calendar/invalid.ics".into(),
            ical_data: "BEGIN:VCALENDAR\nVERSION:2.0\nEND:VCALENDAR\n".into(),
            ..detached.clone()
        };
        let (root, captured) = serve_dav(sync_responses(&report_events(&[
            changed_master,
            detached,
            invalid,
        ])))
        .await;
        sync_at(&db, &root).await.unwrap_err();
        assert_eq!(captured.await.unwrap().len(), 4);
        let conn = db.reader();
        assert_eq!(
            db::calendar::get_event(&conn, &master_id).unwrap().title,
            "Changed"
        );
        assert_eq!(
            db::calendar_sync::displayed_set(&conn, &master_id)
                .unwrap()
                .unwrap()
                .event
                .title,
            "Changed"
        );
    }

    #[tokio::test]
    async fn claimed_series_cannot_veto_refresh_or_deletion() {
        let (_dir, db) = upload_db().await;
        let (master, detached) = split_series();
        let master_id = {
            let conn = db.writer().await;
            let parsed = parse_caldav_resource(
                &master,
                &account("calendar", "caldav"),
                "cal1",
                "/calendar/",
            )
            .unwrap();
            let id = db::calendar::upsert_event_by_remote_id_with_recurrence(
                &conn,
                &parsed.event,
                parsed.recurrence_seeds.as_deref().unwrap(),
            )
            .unwrap();
            conn.execute(
                "INSERT INTO calendar_action_operations(operation_id, account_id, event_id, data)
                 VALUES ('operation', 'acc1', ?1, '{}')",
                [&id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO calendar_action_claims(event_id, operation_id)
                 VALUES (?1, 'operation')",
                [&id],
            )
            .unwrap();
            id
        };
        let report = report_events(&[master, detached]);
        let (root, captured) = serve_dav(sync_responses(&report)).await;
        sync_at(&db, &root).await.unwrap();
        assert!(db::calendar_sync::displayed_set(&db.reader(), &master_id)
            .unwrap()
            .is_some());
        assert_eq!(captured.await.unwrap().len(), 4);
        let (root, captured) = serve_dav(sync_responses("")).await;
        sync_at(&db, &root).await.unwrap();
        assert_eq!(captured.await.unwrap().len(), 4);
        {
            let conn = db.writer().await;
            assert!(db::calendar_sync::displayed_set(&conn, &master_id)
                .unwrap()
                .is_none());
            assert!(db::calendar::get_event(&conn, &master_id).is_err());
            assert_eq!(conn.query_row(
                "SELECT COUNT(*) FROM calendar_events WHERE account_id = 'acc1' AND calendar_id = 'cal1'",
                [], |row| row.get::<_, i64>(0),
            ).unwrap(), 0);
            conn.execute(
                "DELETE FROM calendar_action_claims WHERE operation_id = 'operation'",
                [],
            )
            .unwrap();
        }
        let (root, captured) = serve_dav(sync_responses(&report)).await;
        sync_at(&db, &root).await.unwrap();
        assert_eq!(captured.await.unwrap().len(), 4);
        let master_id: String = db
            .reader()
            .query_row(
                "SELECT event_id FROM calendar_remote_resources WHERE set_data IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let page = projected_series(
            &db,
            &master_id,
            "2026-09-20T00:00:00Z",
            "2026-09-22T00:00:00Z",
        );
        assert_eq!(page.occurrences.len(), 1);
        assert_eq!(
            page.occurrences[0].fields.start_time,
            "2026-09-21T12:00:00Z"
        );
    }

    async fn serve_dav(
        responses: Vec<(&'static str, u16, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        serve_dav_with_etag(responses, Some("\"uploaded-etag\"")).await
    }

    async fn serve_dav_with_etag(
        responses: Vec<(&'static str, u16, String)>,
        etag: Option<&'static str>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        serve_dav_with_response_etags(
            responses
                .into_iter()
                .map(|(method, status, body)| (method, status, body, etag))
                .collect(),
        )
        .await
    }

    async fn serve_dav_with_response_etags(
        responses: Vec<(&'static str, u16, String, Option<&'static str>)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}/dav/", listener.local_addr().unwrap());
        let captured = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (method, status, body, etag) in responses {
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(5), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0, "request ended before its headers and body");
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8(bytes).unwrap();
                assert_eq!(request.split_whitespace().next(), Some(method), "{request}");
                assert!(request.contains("x-injected-client: caldav-upload-test\r\n"));
                requests.push(request);
                let etag_header = etag
                    .map(|etag| format!("ETag: {etag}\r\n"))
                    .unwrap_or_default();
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/xml\r\n\
                     {etag_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (root, captured)
    }

    async fn sync_at(db: &DbPool, root: &str) -> Result<()> {
        let mut account = account("calendar", "caldav");
        account.caldav_url = root.into();
        CalDavCalendarBackend
            .sync(
                &CalendarBackendCtx {
                    db,
                    services: &injected_services(),
                },
                &account,
            )
            .await
    }

    fn occurrence_update_request(native: String) -> RemoteOccurrenceUpdate {
        let occurrence = OccurrenceFields {
            title: "Visible event".into(),
            description: None,
            location: None,
            start_time: "2026-09-13T10:00:00Z".into(),
            end_time: "2026-09-13T11:00:00Z".into(),
            all_day: false,
            timezone: None,
        };
        let mut current = event();
        current.uid = Some("shared".into());
        current.remote_id = Some("/calendar/event.ics".into());
        current.recurrence_kind = RecurrenceKind::Series;
        current.start_time = occurrence.start_time.clone();
        current.end_time = occurrence.end_time.clone();
        RemoteOccurrenceUpdate {
            target_id: "/calendar/event.ics".into(),
            expected_provider_revision: Some("\"old-etag\"".into()),
            trusted_identity: RecurrenceIdentity {
                object_id: "exception-object".into(),
                account_id: "acc1".into(),
                event_id: current.id.clone(),
                local_series_event_id: Some(current.id.clone()),
                provider_calendar_id: Some("/collections/work/".into()),
                provider_series_id: Some("/calendar/event.ics".into()),
                provider_occurrence_id: None,
                recurrence_id: Some("20260920T100000Z".into()),
                recurrence_timezone: None,
                recurrence_value_type: Some(RecurrenceValueType::DateTime),
                occurrence,
                provider_native_data: Some(native),
                provider_revision: Some("\"old-etag\"".into()),
                kind: RecurrenceObjectKind::Exception,
            },
            current_event: current,
            patch: UpdateOccurrenceInput {
                title: Some("Requested title".into()),
                description: Some(String::new()),
                location: Some(String::new()),
                start_time: Some("2026-09-20T12:00:00Z".into()),
                end_time: Some("2026-09-20T13:30:00Z".into()),
                all_day: None,
                timezone: None,
            },
            desired: OccurrenceFields {
                title: "Requested title".into(),
                description: Some(String::new()),
                location: Some(String::new()),
                start_time: "2026-09-20T12:00:00Z".into(),
                end_time: "2026-09-20T13:30:00Z".into(),
                all_day: false,
                timezone: None,
            },
        }
    }

    fn occurrence_resource(properties: &str) -> String {
        resource(&format!(
            "{}{}",
            component("RRULE:FREQ=WEEKLY\nX-MASTER:keep\n"),
            component(&format!(
                "RECURRENCE-ID:20260920T100000Z\nDESCRIPTION:Old\nLOCATION:Old\n{properties}"
            ))
        ))
        .ical_data
    }

    fn occurrence_sibling() -> String {
        component("RECURRENCE-ID:20260927T100000Z\nX-SIBLING:original\n")
    }

    fn occurrence_resource_with_sibling(properties: &str) -> String {
        resource(&format!(
            "{}{}{}",
            component("RRULE:FREQ=WEEKLY\nX-MASTER:keep\n"),
            component(&format!(
                "RECURRENCE-ID:20260920T100000Z\nDESCRIPTION:Old\nLOCATION:Old\n{properties}"
            )),
            occurrence_sibling(),
        ))
        .ical_data
    }

    #[tokio::test]
    async fn occurrence_update_prefers_canonical_get_etag_over_put() {
        let native =
            occurrence_resource_with_sibling("ATTENDEE:mailto:guest@example.test\nX-KEEP:value\n");
        let canonical = native
            .replace("SUMMARY:Visible event", "SUMMARY:Canonical title")
            .replace("DTSTART:20260913T100000Z", "DTSTART:20260920T121500Z")
            .replace("DTEND:20260913T110000Z", "DTEND:20260920T134500Z")
            .replace("X-SIBLING:original", "X-SIBLING:canonical");
        let (root, captured) = serve_dav_with_response_etags(vec![
            ("PUT", 204, String::new(), Some("\"uploaded-etag\"")),
            ("GET", 200, canonical.clone(), Some("W/\"canonical-etag\"")),
        ])
        .await;
        let mut caldav_account = account("calendar", "caldav");
        caldav_account.caldav_url = root;
        let (_dir, db) = temp_pool();
        let services = injected_services();
        let request = occurrence_update_request(native);
        let outcome = CalDavCalendarBackend
            .update_recurrence_occurrence(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services,
                },
                &caldav_account,
                &request,
            )
            .await
            .unwrap();
        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with("PUT /calendar/event.ics HTTP/1.1\r\n"));
        assert!(requests[0].contains("if-match: \"old-etag\"\r\n"));
        let put_body = requests[0]
            .split_once("\r\n\r\n")
            .unwrap()
            .1
            .replace("\r\n", "\n");
        assert!(put_body.contains("SUMMARY:Requested title\n"));
        assert!(!put_body.contains("DESCRIPTION:Old"));
        assert!(!put_body.contains("LOCATION:Old"));
        assert!(put_body.contains("ATTENDEE:mailto:guest@example.test"));
        assert!(put_body.contains("X-KEEP:value"));
        assert!(put_body.contains("X-MASTER:keep"));
        assert!(requests[1].starts_with("GET /calendar/event.ics HTTP/1.1\r\n"));
        assert_eq!(outcome.occurrence.title, "Canonical title");
        assert_eq!(outcome.occurrence.start_time, "2026-09-20T12:15:00Z");
        assert_eq!(outcome.occurrence.end_time, "2026-09-20T13:45:00Z");
        assert_eq!(
            outcome.replacement_identity.provider_native_data.as_deref(),
            Some(canonical.as_str())
        );
        assert_eq!(
            outcome.replacement_identity.provider_revision.as_deref(),
            Some("W/\"canonical-etag\"")
        );
        assert_eq!(
            outcome.replacement_identity.provider_calendar_id,
            request.trusted_identity.provider_calendar_id
        );
        assert_ne!(
            outcome.replacement_identity.provider_calendar_id,
            Some(request.target_id.clone())
        );
        assert!(outcome.canonical_event.is_none());
        let seeds = outcome.canonical_recurrence_objects.unwrap();
        assert_eq!(seeds.len(), 3);
        assert!(seeds.iter().all(|seed| {
            seed.provider_native_data.as_deref() == Some(canonical.as_str())
                && seed.provider_revision.as_deref() == Some("W/\"canonical-etag\"")
        }));
        assert!(seeds.iter().any(|seed| {
            seed.recurrence_id.as_deref() == Some("20260927T100000Z")
                && seed.occurrence.title == "Canonical title"
        }));
    }

    #[tokio::test]
    async fn canonical_override_removal_and_etag_forms_are_preserved() {
        for etag in ["\"strong-etag\"", "W/\"weak-etag\""] {
            let native = occurrence_resource_with_sibling("");
            let canonical = native.replace(&occurrence_sibling(), "");
            let (root, captured) = serve_dav_with_etag(
                vec![("PUT", 204, String::new()), ("GET", 200, canonical.clone())],
                Some(etag),
            )
            .await;
            let mut caldav_account = account("calendar", "caldav");
            caldav_account.caldav_url = root;
            let (_dir, db) = temp_pool();
            let services = injected_services();
            let mut request = occurrence_update_request(native);
            request.expected_provider_revision = Some("\"old-etag\"".into());
            request.trusted_identity.provider_revision = Some("\"old-etag\"".into());

            let outcome = CalDavCalendarBackend
                .update_recurrence_occurrence(
                    &CalendarBackendCtx {
                        db: &db,
                        services: &services,
                    },
                    &caldav_account,
                    &request,
                )
                .await
                .unwrap();
            let requests = captured.await.unwrap();
            assert!(requests[0].contains("if-match: \"old-etag\"\r\n"));
            assert_eq!(
                outcome.replacement_identity.provider_revision.as_deref(),
                Some(etag)
            );
            let seeds = outcome.canonical_recurrence_objects.unwrap();
            assert_eq!(seeds.len(), 2);
            assert!(seeds.iter().all(|seed| {
                seed.provider_native_data.as_deref() == Some(canonical.as_str())
                    && seed.provider_revision.as_deref() == Some(etag)
                    && seed.recurrence_id.as_deref() != Some("20260927T100000Z")
            }));
        }
    }

    #[tokio::test]
    async fn occurrence_update_retains_put_etag_when_canonical_get_omits_it() {
        for put_etag in ["\"uploaded-etag\""] {
            let native = occurrence_resource_with_sibling("X-KEEP:value\n");
            let canonical = native
                .replace("SUMMARY:Visible event", "SUMMARY:Canonical")
                .replace("X-SIBLING:original", "X-SIBLING:canonical");
            let follow_up_canonical =
                canonical.replace("SUMMARY:Canonical", "SUMMARY:Follow-up canonical");
            let (root, captured) = serve_dav_with_response_etags(vec![
                ("PUT", 204, String::new(), Some(put_etag)),
                ("GET", 200, canonical.clone(), None),
                ("PUT", 204, String::new(), Some("\"next-etag\"")),
                ("GET", 200, follow_up_canonical.clone(), None),
            ])
            .await;
            let mut caldav_account = account("calendar", "caldav");
            caldav_account.caldav_url = root;
            let (_dir, db) = temp_pool();
            let services = injected_services();
            let ctx = CalendarBackendCtx {
                db: &db,
                services: &services,
            };
            let request = occurrence_update_request(native);
            let outcome = CalDavCalendarBackend
                .update_recurrence_occurrence(&ctx, &caldav_account, &request)
                .await
                .unwrap();
            assert_eq!(outcome.occurrence.title, "Canonical");
            assert_eq!(
                outcome.replacement_identity.provider_revision.as_deref(),
                Some(put_etag)
            );
            let seeds = outcome.canonical_recurrence_objects.as_ref().unwrap();
            assert_eq!(seeds.len(), 3);
            assert!(seeds.iter().all(|seed| {
                seed.provider_revision.as_deref() == Some(put_etag)
                    && seed.provider_native_data.as_deref() == Some(canonical.as_str())
            }));

            let mut next = request;
            next.expected_provider_revision =
                outcome.replacement_identity.provider_revision.clone();
            next.trusted_identity = outcome
                .replacement_identity
                .bind("acc1", next.current_event.id.clone(), "exception-object")
                .unwrap();
            next.desired = outcome.occurrence;
            next.desired.title = "Follow-up title".into();
            next.patch = UpdateOccurrenceInput {
                title: Some(next.desired.title.clone()),
                ..Default::default()
            };
            let follow_up = CalDavCalendarBackend
                .update_recurrence_occurrence(&ctx, &caldav_account, &next)
                .await
                .unwrap();
            let requests = captured.await.unwrap();
            assert_eq!(requests.len(), 4);
            assert!(requests[0].contains("if-match: \"old-etag\"\r\n"));
            assert!(requests[2].starts_with("PUT /calendar/event.ics HTTP/1.1\r\n"));
            assert!(requests[2].contains(&format!("if-match: {put_etag}\r\n")));
            assert!(!requests[2].contains("if-match: \"old-etag\"\r\n"));
            let put_body = requests[2]
                .split_once("\r\n\r\n")
                .unwrap()
                .1
                .replace("\r\n", "\n");
            assert!(put_body.contains("SUMMARY:Follow-up title\n"));
            assert!(put_body.contains("DESCRIPTION:Old\nLOCATION:Old\nX-KEEP:value\n"));
            assert!(put_body.contains(
                &component("RRULE:FREQ=WEEKLY\nX-MASTER:keep\n")
                    .replace("SUMMARY:Visible event", "SUMMARY:Canonical")
            ));
            assert!(put_body.contains(
                &occurrence_sibling()
                    .replace("SUMMARY:Visible event", "SUMMARY:Canonical")
                    .replace("X-SIBLING:original", "X-SIBLING:canonical")
            ));
            assert_eq!(follow_up.occurrence.title, "Follow-up canonical");
            assert_eq!(
                follow_up.replacement_identity.provider_revision.as_deref(),
                Some("\"next-etag\"")
            );
            let seeds = follow_up.canonical_recurrence_objects.unwrap();
            assert_eq!(seeds.len(), 3);
            assert!(seeds.iter().all(|seed| {
                seed.provider_revision.as_deref() == Some("\"next-etag\"")
                    && seed.provider_native_data.as_deref() == Some(follow_up_canonical.as_str())
            }));
        }
    }

    #[tokio::test]
    async fn occurrence_update_without_response_etags_never_reuses_old_etag() {
        let native = occurrence_resource_with_sibling("");
        let canonical = native.replace("SUMMARY:Visible event", "SUMMARY:Canonical");
        let (root, captured) = serve_dav_with_etag(
            vec![("PUT", 204, String::new()), ("GET", 200, canonical.clone())],
            None,
        )
        .await;
        let mut caldav_account = account("calendar", "caldav");
        caldav_account.caldav_url = root;
        let (_dir, db) = temp_pool();
        let services = injected_services();
        let request = occurrence_update_request(native);
        let outcome = CalDavCalendarBackend
            .update_recurrence_occurrence(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services,
                },
                &caldav_account,
                &request,
            )
            .await
            .unwrap();
        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("if-match: \"old-etag\"\r\n"));
        assert_eq!(outcome.replacement_identity.provider_revision, None);
        let seeds = outcome.canonical_recurrence_objects.as_ref().unwrap();
        assert_eq!(seeds.len(), 3);
        assert!(seeds.iter().all(|seed| {
            seed.provider_revision.is_none()
                && seed.provider_native_data.as_deref() == Some(canonical.as_str())
        }));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        caldav_account.caldav_url = format!("http://{}/dav/", listener.local_addr().unwrap());
        let mut next = request;
        next.expected_provider_revision = outcome.replacement_identity.provider_revision.clone();
        next.trusted_identity = outcome
            .replacement_identity
            .bind("acc1", next.current_event.id.clone(), "exception-object")
            .unwrap();
        let error = CalDavCalendarBackend
            .update_recurrence_occurrence(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services,
                },
                &caldav_account,
                &next,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Sync(_)));
        assert!(error.to_string().contains("has no expected ETag"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn occurrence_update_maps_conflicts_to_stale_sync_error() {
        let native = occurrence_resource("");
        for status in [409, 412] {
            let (root, captured) = serve_dav(vec![("PUT", status, "stale".into())]).await;
            let mut caldav_account = account("calendar", "caldav");
            caldav_account.caldav_url = root;
            let (_dir, db) = temp_pool();
            let services = injected_services();
            let error = CalDavCalendarBackend
                .update_recurrence_occurrence(
                    &CalendarBackendCtx {
                        db: &db,
                        services: &services,
                    },
                    &caldav_account,
                    &occurrence_update_request(native.clone()),
                )
                .await
                .unwrap_err();
            assert!(matches!(error, Error::Sync(_)));
            assert!(error.to_string().contains("reconciliation required"));
            assert_eq!(captured.await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn malformed_ambiguous_and_range_resources_fail_before_io() {
        for native in [
            occurrence_resource("RECURRENCE-ID:20260920T100000Z\n"),
            occurrence_resource("").replace(
                "RECURRENCE-ID:20260920T100000Z",
                "RECURRENCE-ID;RANGE=THISANDFUTURE:20260920T100000Z",
            ),
            occurrence_resource("")
                .trim_end_matches("END:VCALENDAR\n")
                .into(),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let root = format!("http://{}/dav/", listener.local_addr().unwrap());
            let mut caldav_account = account("calendar", "caldav");
            caldav_account.caldav_url = root;
            let (_dir, db) = temp_pool();
            let services = injected_services();
            assert!(CalDavCalendarBackend
                .update_recurrence_occurrence(
                    &CalendarBackendCtx {
                        db: &db,
                        services: &services,
                    },
                    &caldav_account,
                    &occurrence_update_request(native),
                )
                .await
                .is_err());
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn sync_evicts_unpublished_series_and_rebuilds_from_remote_content_only() {
        let (_dir, db) = upload_db().await;
        let local = local_series();
        {
            let mut conn = db.writer().await;
            insert_proven_series(&mut conn, &local);
        }
        let uploaded = creation_ical_data(&local, "shared").unwrap();
        let (root, captured) = serve_dav(sync_responses("")).await;
        sync_at(&db, &root).await.unwrap();
        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests.iter().all(|request| !request.starts_with("PUT ")));
        let parsed = ical::parse_ical_data(&uploaded);
        assert_eq!(parsed[0].recurrence_kind, RecurrenceKind::Series);
        assert_eq!(parsed[0].recurrence_rule, local.recurrence_rule);
        assert!(db::calendar::get_event(&db.reader(), &local.id).is_err());
        let attached = CalendarEvent {
            uid: Some("shared".into()),
            remote_id: Some("/calendar/shared.ics".into()),
            etag: Some("\"uploaded-etag\"".into()),
            ..local
        };
        assert_eq!(proof_count(&db.reader()), 0);

        // A real REPORT supplies the timezone absent from the legacy floating
        // upload and authoritative source even with the same RRULE/etag.
        let provider_source = uploaded
            .replace("\r\n", "\n")
            .replace("DTSTART:", "DTSTART;TZID=UTC:")
            .replace("DTEND:", "DTEND;TZID=UTC:")
            .trim()
            .to_string();
        let report = format!(
            "<d:response><d:href>{}</d:href><d:propstat><d:prop>\
             <d:getetag>\"uploaded-etag\"</d:getetag>\
             <c:calendar-data><![CDATA[{provider_source}]]></c:calendar-data>\
             </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>\
             </d:response>",
            attached.remote_id.as_ref().unwrap()
        );
        let (root, captured) = serve_dav(sync_responses(&report)).await;
        sync_at(&db, &root).await.unwrap();
        assert_eq!(captured.await.unwrap().len(), 4);
        let conn = db.reader();
        let id: String = conn
            .query_row(
                "SELECT id FROM calendar_events WHERE remote_id = '/calendar/shared.ics'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let refreshed = db::calendar::get_event(&conn, &id).unwrap();
        assert_eq!(refreshed.recurrence_rule, attached.recurrence_rule);
        assert_eq!(
            refreshed.ical_data.as_deref(),
            Some(provider_source.as_str())
        );
        assert_eq!(refreshed.remote_id, attached.remote_id);
        assert_eq!(refreshed.etag.as_deref(), Some("\"uploaded-etag\""));
        assert!(db::calendar_invitation::validated_series_rule(&conn, &refreshed).is_err());
        assert_eq!(proof_count(&conn), 0);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM calendar_events WHERE remote_id IS NULL OR remote_id = ''",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    async fn insert_synced_series(db: &DbPool) -> (CalendarEvent, Vec<RecurrenceIdentity>) {
        let account = account("calendar", "caldav");
        let resource = resource(&format!(
            "{}{}",
            component("RRULE:FREQ=WEEKLY\n"),
            component("RECURRENCE-ID:20260920T100000Z\n")
        ));
        let parsed = parse_caldav_resource(&resource, &account, "cal1", "/calendar/").unwrap();
        let event_id = {
            let conn = db.writer().await;
            db::calendar::upsert_event_by_remote_id_with_recurrence(
                &conn,
                &parsed.event,
                parsed.recurrence_seeds.as_deref().unwrap(),
            )
            .unwrap()
        };
        let conn = db.reader();
        (
            db::calendar::get_event(&conn, &event_id).unwrap(),
            db::calendar_recurrence::get_by_event_id(&conn, &event_id).unwrap(),
        )
    }

    #[tokio::test]
    async fn failed_report_fetch_or_parse_preserves_rows_and_identities() {
        for (status, body) in [
            (500, "server error".to_string()),
            (207, "<d:multistatus xmlns:d=\"DAV:\">".to_string()),
            (207, "<not-multistatus/>".to_string()),
        ] {
            let (_dir, db) = upload_db().await;
            let (before, identities_before) = insert_synced_series(&db).await;
            let mut responses = sync_responses("");
            responses[3] = ("REPORT", status, body);
            let (root, captured) = serve_dav(responses).await;

            sync_at(&db, &root).await.unwrap_err();
            assert_eq!(captured.await.unwrap().len(), 4);
            assert_eq!(
                db::calendar::get_event(&db.reader(), &before.id).unwrap(),
                before
            );
            assert_eq!(
                db::calendar_recurrence::get_by_event_id(&db.reader(), &before.id).unwrap(),
                identities_before
            );
        }
    }

    #[tokio::test]
    async fn complete_empty_report_deletes_absent_rows_and_identities() {
        let (_dir, db) = upload_db().await;
        let (before, identities_before) = insert_synced_series(&db).await;
        assert!(!identities_before.is_empty());
        let (root, captured) = serve_dav(sync_responses("")).await;

        sync_at(&db, &root).await.unwrap();
        assert_eq!(captured.await.unwrap().len(), 4);
        assert!(db::calendar::get_event(&db.reader(), &before.id).is_err());
        assert!(
            db::calendar_recurrence::get_by_event_id(&db.reader(), &before.id)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn sync_never_implicitly_uploads_addressless_rows_even_with_source_data() {
        let (_dir, db) = upload_db().await;
        let mut originals = vec![
            event(),
            CalendarEvent {
                id: "unproven".into(),
                ..local_series()
            },
        ];
        for (id, kind, properties) in [
            ("raw-standalone", RecurrenceKind::Standalone, ""),
            (
                "raw-series",
                RecurrenceKind::Series,
                "RDATE:20260920T100000Z\nEXDATE:20260927T100000Z\n",
            ),
            (
                "raw-occurrence",
                RecurrenceKind::Occurrence,
                "RECURRENCE-ID:20260920T100000Z\n",
            ),
        ] {
            originals.push(CalendarEvent {
                id: id.into(),
                uid: Some(id.into()),
                recurrence_kind: kind,
                ical_data: Some(
                    resource(&component(properties))
                        .ical_data
                        .replace("UID:shared", &format!("UID:{id}")),
                ),
                source_message_id: Some("original-message".into()),
                remote_id: Some(String::new()),
                ..event()
            });
        }
        {
            let conn = db.writer().await;
            for event in &originals {
                db::calendar::insert_event(&conn, event).unwrap();
            }
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM calendar_events", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                5
            );
        }
        let (root, captured) = serve_dav(sync_responses("")).await;
        sync_at(&db, &root).await.unwrap();
        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), 4);
        let conn = db.reader();
        for original in originals {
            assert!(db::calendar::get_event(&conn, &original.id).is_err());
        }
        assert_eq!(proof_count(&conn), 0);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM calendar_events", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn unsafe_unpushed_rows_fail_before_any_upload_http() {
        let (_dir, db) = upload_db().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut invalid = vec![
            CalendarEvent {
                recurrence_kind: RecurrenceKind::Unknown,
                ..event()
            },
            CalendarEvent {
                recurrence_kind: RecurrenceKind::Occurrence,
                ..event()
            },
            CalendarEvent {
                recurrence_kind: RecurrenceKind::Series,
                ..event()
            },
            CalendarEvent {
                source_message_id: Some("message".into()),
                ..local_series()
            },
            CalendarEvent {
                ical_data: Some(String::new()),
                ..local_series()
            },
            CalendarEvent {
                ical_data: Some(resource(&component("")).ical_data),
                ..local_series()
            },
            CalendarEvent {
                recurrence_rule: Some("FREQ=WEEKLY".into()),
                ..event()
            },
        ];
        for rule in [
            "",
            "FREQ=INVALID",
            "FREQ=WEEKLY;BYDAY=MÖ",
            "FREQ=WEEKLY\r\nEXDATE:20260920T100000Z",
        ] {
            invalid.push(CalendarEvent {
                recurrence_rule: Some(rule.into()),
                ..local_series()
            });
        }
        {
            let conn = db.writer().await;
            for (index, event) in invalid.iter_mut().enumerate() {
                event.id = format!("invalid-{index}");
                db::calendar::insert_event(&conn, event).unwrap();
            }
        }
        assert_eq!(invalid.len(), 11);
        for event in invalid {
            let revision = db::calendar_revision::get(&db.reader(), &event.id).unwrap();
            let error = creation_ical_data(&event, "test-uid")
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("complete recurrence data is unavailable"),
                "{error}"
            );
            assert_eq!(
                db::calendar::get_event(&db.reader(), &event.id).unwrap(),
                event
            );
            assert_eq!(
                db::calendar_revision::get(&db.reader(), &event.id).unwrap(),
                revision
            );
        }
        assert_eq!(proof_count(&db.reader()), 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }

    #[test]
    fn deferred_creation_preserves_source_recurrence_and_never_invents_standalone() {
        let mut event = crate::backend::testutil::event();
        for kind in [
            RecurrenceKind::Unknown,
            RecurrenceKind::Occurrence,
            RecurrenceKind::Series,
        ] {
            event.recurrence_kind = kind;
            assert!(creation_ical_data(&event, "uid").is_err());
        }
        event.recurrence_kind = RecurrenceKind::Standalone;
        let raw = creation_ical_data(&event, "uid").unwrap();
        assert_eq!(
            crate::calendar::ical::parse_ical_data(&raw)[0].recurrence_kind,
            RecurrenceKind::Standalone
        );
        event.recurrence_kind = RecurrenceKind::Series;
        event.recurrence_rule = Some("FREQ=WEEKLY;COUNT=4".into());
        let raw = creation_ical_data(&event, "uid").unwrap();
        let parsed = crate::calendar::ical::parse_ical_data(&raw);
        assert_eq!(parsed[0].recurrence_kind, RecurrenceKind::Series);
        assert_eq!(parsed[0].recurrence_rule, event.recurrence_rule);

        for (kind, properties) in [
            (
                RecurrenceKind::Occurrence,
                "RECURRENCE-ID:20260920T100000Z\n",
            ),
            (
                RecurrenceKind::Series,
                "RDATE:20260920T100000Z\nEXDATE:20260927T100000Z\n",
            ),
        ] {
            let raw = resource(&component(properties)).ical_data;
            event.recurrence_kind = kind;
            event.recurrence_rule = None;
            event.ical_data = Some(raw.clone());
            assert_eq!(creation_ical_data(&event, "uid").unwrap(), raw);
            event.recurrence_kind = RecurrenceKind::Unknown;
            assert!(creation_ical_data(&event, "uid").is_err());
        }
    }

    #[test]
    fn selected_row_retains_resource_recurrence_classification() {
        let account = account("calendar", "caldav");
        let master = component("RRULE:FREQ=WEEKLY\n");
        let occurrence = component("RECURRENCE-ID:20260920T100000Z\n");
        let standalone = component("");
        for (components, kind) in [
            (standalone.clone(), RecurrenceKind::Standalone),
            (format!("{master}{occurrence}"), RecurrenceKind::Series),
            (format!("{occurrence}{master}"), RecurrenceKind::Series),
            (format!("{standalone}{occurrence}"), RecurrenceKind::Unknown),
            (format!("{standalone}{standalone}"), RecurrenceKind::Unknown),
        ] {
            let resource = resource(&components);
            let event =
                from_caldav_event(&resource, &account, "calendar-1", "/collections/work/").unwrap();
            assert_eq!(event.recurrence_kind, kind);
            assert_eq!(
                event.ical_data.as_deref(),
                Some(resource.ical_data.as_str())
            );
            assert_eq!(event.remote_id.as_deref(), Some(resource.href.as_str()));
            assert_eq!(event.title, "Visible event");
        }
    }

    #[test]
    fn caldav_recurrence_seeds_preserve_resource_and_raw_positions() {
        let master = component("RRULE:FREQ=WEEKLY\n");
        let utc = component("RECURRENCE-ID:20260920T100000Z\n");
        let floating = component("RECURRENCE-ID:20260927T100000\n");
        let zoned = component(
            "RECURRENCE-ID;TZID=Europe/Stockholm;RANGE=THISANDFUTURE:\
             20261004T100000\nSTATUS:CANCELLED\n",
        );
        let resource = resource(&format!("{master}{utc}{floating}{zoned}"));
        let account = account("calendar", "caldav");
        let parsed =
            parse_caldav_resource(&resource, &account, "calendar-1", "/collections/work/").unwrap();
        let seeds = parsed.recurrence_seeds.unwrap();

        assert_eq!(parsed.event.recurrence_kind, RecurrenceKind::Series);
        assert_eq!(
            parsed.event.remote_id.as_deref(),
            Some(resource.href.as_str())
        );
        assert_eq!(parsed.event.etag.as_deref(), Some(resource.etag.as_str()));
        assert_eq!(
            parsed.event.ical_data.as_deref(),
            Some(resource.ical_data.as_str())
        );
        assert_eq!(seeds.len(), 4);
        assert_eq!(seeds[0].kind, RecurrenceObjectKind::Master);
        assert_eq!(
            seeds[0].provider_series_id.as_deref(),
            Some(resource.href.as_str())
        );
        assert_eq!(seeds[0].occurrence.title, "Visible event");
        assert_eq!(seeds[0].occurrence.start_time, "2026-09-13T10:00:00Z");
        for seed in &seeds {
            assert_eq!(
                seed.provider_calendar_id.as_deref(),
                Some("/collections/work/")
            );
            assert_ne!(seed.provider_calendar_id, seed.provider_series_id);
            assert_eq!(
                seed.provider_native_data.as_deref(),
                Some(resource.ical_data.as_str())
            );
            assert_eq!(
                seed.provider_revision.as_deref(),
                Some(resource.etag.as_str())
            );
            assert_eq!(seed.provider_occurrence_id, None);
        }
        assert_eq!(seeds[1].recurrence_id.as_deref(), Some("20260920T100000Z"));
        assert_eq!(seeds[1].recurrence_timezone, None);
        assert_eq!(seeds[1].kind, RecurrenceObjectKind::Exception);
        assert_eq!(seeds[2].recurrence_id.as_deref(), Some("20260927T100000"));
        assert_eq!(seeds[2].recurrence_timezone, None);
        assert_eq!(seeds[3].recurrence_id.as_deref(), Some("20261004T100000"));
        assert_eq!(
            seeds[3].recurrence_timezone.as_deref(),
            Some("Europe/Stockholm")
        );
        assert_eq!(seeds[3].kind, RecurrenceObjectKind::Exclusion);
        assert!(seeds[3]
            .provider_native_data
            .as_deref()
            .unwrap()
            .contains("RANGE=THISANDFUTURE"));
    }

    #[test]
    fn caldav_date_identity_standalone_clear_and_malformed_fail_closed() {
        let date = CalDavEvent {
            ical_data: "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//Chithi//EN\n\
                        BEGIN:VEVENT\nUID:shared\nDTSTAMP:20260901T120000Z\n\
                        RECURRENCE-ID;VALUE=DATE:20260920\n\
                        DTSTART;VALUE=DATE:20260921\n\
                        SUMMARY:Moved\nEND:VEVENT\nEND:VCALENDAR\n"
                .into(),
            ..resource("")
        };
        let account = account("calendar", "caldav");
        let parsed =
            parse_caldav_resource(&date, &account, "calendar-1", "/collections/work/").unwrap();
        let seeds = parsed.recurrence_seeds.unwrap();
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].recurrence_id.as_deref(), Some("20260920"));
        assert_eq!(
            seeds[0].recurrence_value_type,
            Some(crate::calendar::recurrence_identity::RecurrenceValueType::Date)
        );
        assert_eq!(seeds[0].occurrence.title, "Moved");
        assert_eq!(seeds[0].occurrence.start_time, "2026-09-21");
        assert_eq!(seeds[0].occurrence.end_time, "2026-09-22");
        assert!(seeds[0].occurrence.all_day);
        assert_eq!(seeds[0].kind, RecurrenceObjectKind::Exception);

        let standalone = resource(&component(""));
        assert!(
            parse_caldav_resource(&standalone, &account, "calendar-1", "/collections/work/")
                .unwrap()
                .recurrence_seeds
                .unwrap()
                .is_empty()
        );

        for malformed in [
            "RECURRENCE-ID;VALUE=DATE;VALUE=DATE:20260920\n",
            "RECURRENCE-ID;TZID=:20260920T100000\n",
            "RECURRENCE-ID:20260920T100000Z\nRECURRENCE-ID:20260927T100000Z\n",
        ] {
            let malformed = resource(&component(malformed));
            let parsed =
                parse_caldav_resource(&malformed, &account, "calendar-1", "/collections/work/")
                    .unwrap();
            assert_eq!(parsed.event.recurrence_kind, RecurrenceKind::Unknown);
            assert!(parsed.recurrence_seeds.is_none());
        }
    }

    #[tokio::test]
    async fn caldav_identity_refresh_is_stable_and_only_standalone_clears() {
        let (_directory, db) = upload_db().await;
        let account = account("calendar", "caldav");
        let mut recurring = resource(&format!(
            "{}{}",
            component("RRULE:FREQ=WEEKLY\n"),
            component("RECURRENCE-ID:20260920T100000Z\n")
        ));

        let first =
            parse_caldav_resource(&recurring, &account, "cal1", "/collections/work/").unwrap();
        let first_event_id = {
            let conn = db.writer().await;
            db::calendar::upsert_event_by_remote_id_with_recurrence(
                &conn,
                &first.event,
                first.recurrence_seeds.as_deref().unwrap(),
            )
            .unwrap()
        };
        let original =
            db::calendar_recurrence::get_by_event_id(&db.reader(), &first_event_id).unwrap();
        assert_eq!(original.len(), 2);

        recurring.etag = "refreshed-etag".into();
        let refresh =
            parse_caldav_resource(&recurring, &account, "cal1", "/collections/work/").unwrap();
        {
            let conn = db.writer().await;
            let reconciled = db::calendar::upsert_event_by_remote_id_with_recurrence(
                &conn,
                &refresh.event,
                refresh.recurrence_seeds.as_deref().unwrap(),
            )
            .unwrap();
            assert_eq!(reconciled, first_event_id);
        }
        let refreshed =
            db::calendar_recurrence::get_by_event_id(&db.reader(), &first_event_id).unwrap();
        assert_eq!(
            refreshed
                .iter()
                .map(|identity| &identity.object_id)
                .collect::<Vec<_>>(),
            original
                .iter()
                .map(|identity| &identity.object_id)
                .collect::<Vec<_>>()
        );
        assert!(refreshed
            .iter()
            .all(|identity| identity.provider_revision.as_deref() == Some("refreshed-etag")));

        let malformed = resource(&component(
            "RECURRENCE-ID:20260920T100000Z\nRECURRENCE-ID:20260927T100000Z\n",
        ));
        let malformed =
            parse_caldav_resource(&malformed, &account, "cal1", "/collections/work/").unwrap();
        assert!(malformed.recurrence_seeds.is_none());
        {
            let conn = db.writer().await;
            db::calendar::upsert_event_by_remote_id(&conn, &malformed.event).unwrap();
        }
        assert_eq!(
            db::calendar_recurrence::get_by_event_id(&db.reader(), &first_event_id).unwrap(),
            refreshed
        );

        let standalone = resource(&component(""));
        let standalone =
            parse_caldav_resource(&standalone, &account, "cal1", "/collections/work/").unwrap();
        assert!(standalone.recurrence_seeds.as_ref().unwrap().is_empty());
        {
            let conn = db.writer().await;
            db::calendar::upsert_event_by_remote_id_with_recurrence(
                &conn,
                &standalone.event,
                standalone.recurrence_seeds.as_deref().unwrap(),
            )
            .unwrap();
        }
        assert!(
            db::calendar_recurrence::get_by_event_id(&db.reader(), &first_event_id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn unchanged_etag_refresh_recovers_unknown_classification_on_existing_row() {
        let conn = Connection::open_in_memory().unwrap();
        db::schema::initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('acc1', 'Test', 'u@example.com', 'u@example.com')",
            [],
        )
        .unwrap();
        let account = account("calendar", "caldav");
        let calendar_id = db::calendar::upsert_calendar_by_remote_id(
            &conn,
            &account.id,
            "/calendar/",
            "Calendar",
            "#4285f4",
            true,
        )
        .unwrap();
        let resource = resource(&component(""));
        let mut legacy =
            from_caldav_event(&resource, &account, &calendar_id, "/collections/work/").unwrap();
        legacy.recurrence_kind = RecurrenceKind::Unknown;
        db::calendar::upsert_event_by_remote_id(&conn, &legacy).unwrap();

        let refreshed =
            from_caldav_event(&resource, &account, &calendar_id, "/collections/work/").unwrap();
        assert_eq!(legacy.etag, refreshed.etag);
        db::calendar::upsert_event_by_remote_id(&conn, &refreshed).unwrap();
        let stored: (String, String, String) = conn
            .query_row(
                "SELECT id, recurrence_kind, etag FROM calendar_events WHERE remote_id = ?1",
                [&resource.href],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(stored, (legacy.id, "standalone".into(), resource.etag));
    }

    #[test]
    fn personal_resource_strips_scheduling_and_preserves_recurrence_set() {
        let raw = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\n\
                   BEGIN:VEVENT\r\nUID:series\r\nDTSTART:20260913T100000Z\r\n\
                   DTEND:20260913T110000Z\r\nSUMMARY:Updated title\r\n\
                   DESCRIPTION:Remove me\r\nLOCATION:Remove me\r\n\
                   ORGANIZER:mailto:owner@example.test\r\n\
                   ATTENDEE:mailto:guest@example.test\r\nRRULE:FREQ=WEEKLY\r\n\
                   EXDATE:20260920T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let local = CalendarEvent {
            uid: Some("series".into()),
            title: "Updated title".into(),
            recurrence_kind: RecurrenceKind::Series,
            recurrence_rule: Some("FREQ=WEEKLY".into()),
            source_message_id: Some("message".into()),
            ical_data: Some(raw.into()),
            organizer_email: Some("owner@example.test".into()),
            attendees_json: Some("[]".into()),
            ..event()
        };

        let personal = personal_copy_ical_data(&local, "series").unwrap();
        assert!(!personal.contains("METHOD:"));
        assert!(!personal.contains("ORGANIZER"));
        assert!(!personal.contains("ATTENDEE"));
        assert!(personal.contains("SUMMARY:Updated title\r\n"));
        assert!(!personal.contains("DESCRIPTION:"));
        assert!(!personal.contains("LOCATION:"));
        assert!(personal.contains("RRULE:FREQ=WEEKLY\r\n"));
        assert!(personal.contains("EXDATE:20260920T100000Z\r\n"));
        let parsed = ical::parse_ical_data(&personal);
        assert_eq!(parsed[0].recurrence_kind, RecurrenceKind::Series);
    }

    #[tokio::test]
    async fn direct_creation_returns_confirmed_identity_and_update_uses_existing_href() {
        let (root, created_request) = serve_dav(vec![("PUT", 201, String::new())]).await;
        let (_directory, db) = temp_pool();
        let services = injected_services();
        let mut destination = account("calendar", "caldav");
        destination.caldav_url = root;
        let context = CalendarBackendCtx {
            db: &db,
            services: &services,
        };
        let local = CalendarEvent {
            uid: Some("created-uid".into()),
            ..event()
        };
        let pushed = CalDavCalendarBackend
            .push_created_event(&context, &destination, &local, "/calendar/")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pushed.remote_id, "/calendar/created-uid.ics");
        assert_eq!(pushed.canonical_uid.as_deref(), Some("created-uid"));
        assert_eq!(pushed.etag.as_deref(), Some("\"uploaded-etag\""));
        let requests = created_request.await.unwrap();
        assert!(requests[0].starts_with("PUT /calendar/created-uid.ics HTTP/1.1\r\n"));

        let (root, updated_request) = serve_dav(vec![("PUT", 204, String::new())]).await;
        destination.caldav_url = root;
        let updated = CalendarEvent {
            uid: Some("created-uid".into()),
            etag: Some("\"stored-etag\"".into()),
            title: "Authoritative".into(),
            source_message_id: Some("message".into()),
            organizer_email: Some("owner@example.test".into()),
            attendees_json: Some("[]".into()),
            ical_data: Some(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\n\
                 METHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:created-uid\r\n\
                 DTSTAMP:20260901T120000Z\r\nDTSTART:20260914T100000Z\r\n\
                 DTEND:20260914T110000Z\r\nSUMMARY:Authoritative\r\n\
                 ORGANIZER:mailto:owner@example.test\r\n\
                 ATTENDEE:mailto:guest@example.test\r\nEND:VEVENT\r\n\
                 END:VCALENDAR\r\n"
                    .into(),
            ),
            ..event()
        };
        let revision = CalDavCalendarBackend
            .push_updated_invitation_copy(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services,
                },
                &destination,
                "/calendar/existing-name.ics",
                &updated,
            )
            .await
            .unwrap();
        assert_eq!(revision.as_deref(), Some("\"uploaded-etag\""));
        let requests = updated_request.await.unwrap();
        assert!(requests[0].starts_with("PUT /calendar/existing-name.ics HTTP/1.1\r\n"));
        assert!(requests[0].contains("if-match: \"stored-etag\"\r\n"));
        let body = requests[0].split_once("\r\n\r\n").unwrap().1;
        assert!(body.contains("SUMMARY:Authoritative\r\n"));
        assert!(body.contains("DTSTART:20260716T100000\r\n"));
        assert!(body.contains("DTEND:20260716T103000\r\n"));
        assert!(!body.contains("METHOD:"));
        assert!(!body.contains("ORGANIZER"));
        assert!(!body.contains("ATTENDEE"));

        let (root, created_request) =
            serve_dav_with_etag(vec![("PUT", 201, String::new())], None).await;
        destination.caldav_url = root;
        let pushed = CalDavCalendarBackend
            .push_created_event(&context, &destination, &local, "/calendar/")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pushed.etag, None);
        assert_eq!(created_request.await.unwrap().len(), 1);

        let (root, updated_request) =
            serve_dav_with_etag(vec![("PUT", 204, String::new())], None).await;
        destination.caldav_url = root;
        let revision = CalDavCalendarBackend
            .push_updated_invitation_copy(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services,
                },
                &destination,
                "/calendar/existing-name.ics",
                &updated,
            )
            .await
            .unwrap();
        assert_eq!(revision, None);
        let requests = updated_request.await.unwrap();
        assert!(requests[0].contains("if-match: \"stored-etag\"\r\n"));
    }
}
