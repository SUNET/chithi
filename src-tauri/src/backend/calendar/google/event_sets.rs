//! Private Google master/exception snapshots and conditional scoped mutations.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{parse_google_attendees, CalendarBackendCtx, CalendarCapability, GoogleClient};
use crate::calendar::event_set::{
    canonical_position, CalendarEventSet, CalendarOverride, NativeCalendarResource,
};
use crate::calendar::{simple_recurrence, CalendarEvent, RecurrenceKind};
use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::mail::google::{event_set_content, google_recurrence_kind};

#[cfg(test)]
mod tests;

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::Sync(format!("Google calendar event set: {message}"))
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| invalid(format!("missing {key}")))
}

pub(super) fn revision(native: &NativeCalendarResource) -> Result<&str> {
    native
        .revision
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| invalid("missing ETag"))
}

fn native(value: &Value, calendar: &str) -> Result<NativeCalendarResource> {
    Ok(NativeCalendarResource {
        protocol: "google".into(),
        calendar_id: calendar.into(),
        event_id: text(value, "id")?.into(),
        revision: value["etag"].as_str().map(str::to_owned),
        data: serde_json::to_string(value).map_err(invalid)?,
    })
}

fn native_value(resource: &NativeCalendarResource) -> Result<Value> {
    let value: Value = serde_json::from_str(&resource.data).map_err(invalid)?;
    if resource.protocol != "google"
        || resource.calendar_id.is_empty()
        || value["id"].as_str() != Some(resource.event_id.as_str())
        || value["etag"].as_str() != resource.revision.as_deref()
    {
        return Err(invalid("native identity/revision mismatch"));
    }
    Ok(value)
}

pub(super) fn validate_source<'a>(
    set: &'a CalendarEventSet,
    account: &AccountFull,
) -> Result<&'a NativeCalendarResource> {
    set.validate()?;
    let resource = set
        .native
        .as_ref()
        .ok_or_else(|| invalid("missing native master"))?;
    let value = native_value(resource)?;
    revision(resource)?;
    if set.event.account_id != account.id
        || set.event.remote_id.as_deref() != Some(resource.event_id.as_str())
        || set.event.etag != resource.revision
        || value.get("recurringEventId").is_some()
        || value["iCalUID"].as_str() != set.event.uid.as_deref()
    {
        return Err(invalid("source account/master identity mismatch"));
    }
    if !common_equal(&parse_event(&value, account, &set.event)?, &set.event)? {
        return Err(invalid("source content does not match its native snapshot"));
    }
    for item in &set.overrides {
        if let Some(n) = &item.native {
            let v = native_value(n)?;
            if n.calendar_id != resource.calendar_id
                || v["recurringEventId"].as_str() != Some(resource.event_id.as_str())
                || original(&v, &set.event)? != item.original_start
            {
                return Err(invalid("exception identity mismatch"));
            }
            match &item.event {
                Some(event) if v["status"].as_str() != Some("cancelled") => {
                    if !common_equal(&parse_event(&v, account, event)?, event)? {
                        return Err(invalid("exception content mismatch"));
                    }
                }
                None if v["status"].as_str() == Some("cancelled") => {}
                _ => return Err(invalid("exception cancellation mismatch")),
            }
        }
    }
    Ok(resource)
}

pub(super) fn stored_calendar(
    ctx: &CalendarBackendCtx<'_>,
    account: &AccountFull,
    event: &CalendarEvent,
    remote_id: &str,
) -> Result<String> {
    let conn = ctx.db.reader();
    let calendar: String = conn.query_row(
        "SELECT c.remote_id FROM calendar_events e JOIN calendars c ON c.id = e.calendar_id
         WHERE e.id = ?1 AND e.account_id = ?2 AND c.account_id = ?2 AND e.remote_id = ?3",
        rusqlite::params![event.id, account.id, remote_id],
        |row| row.get(0),
    )?;
    if calendar.is_empty() || event.account_id != account.id {
        return Err(invalid("stored source calendar is missing"));
    }
    Ok(calendar)
}

fn boundary(value: &Value) -> Result<(String, bool, Option<String>)> {
    let zone = match value.get("timeZone") {
        None => None,
        Some(v) => {
            let name = v.as_str().ok_or_else(|| invalid("invalid timezone"))?;
            name.parse::<chrono_tz::Tz>()
                .map_err(|_| invalid("unknown IANA timezone"))?;
            Some(name.to_owned())
        }
    };
    match (value["date"].as_str(), value["dateTime"].as_str()) {
        (Some(date), None) => Ok((
            NaiveDate::parse_from_str(date, "%Y-%m-%d")
                .map_err(invalid)?
                .to_string(),
            true,
            zone,
        )),
        (None, Some(time)) => Ok((
            DateTime::parse_from_rfc3339(time)
                .map_err(invalid)?
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::AutoSi, true),
            false,
            zone,
        )),
        _ => Err(invalid("invalid date boundary")),
    }
}

fn original(value: &Value, master: &CalendarEvent) -> Result<String> {
    let (value, all_day, _) = boundary(&value["originalStartTime"])?;
    if all_day != master.all_day {
        return Err(invalid("original start type mismatch"));
    }
    canonical_position(master, &value)
}

fn parse_event(
    value: &Value,
    account: &AccountFull,
    local: &CalendarEvent,
) -> Result<CalendarEvent> {
    if value["attendeesOmitted"].as_bool() == Some(true) {
        return Err(invalid("incomplete attendee list"));
    }
    let (start_time, all_day, timezone) = boundary(&value["start"])?;
    let (end_time, end_all_day, _) = boundary(&value["end"])?;
    if all_day != end_all_day {
        return Err(invalid("mixed boundary types"));
    }
    let recurrence_kind = google_recurrence_kind(value);
    if recurrence_kind == RecurrenceKind::Series && !all_day && timezone.is_none() {
        return Err(invalid(
            "timed recurring master omitted its expansion timezone",
        ));
    }
    let recurrence_rule = if recurrence_kind == RecurrenceKind::Series {
        let lines = value["recurrence"]
            .as_array()
            .ok_or_else(|| invalid("missing recurrence"))?;
        if lines.len() != 1 {
            return Err(invalid(
                "only one RRULE is supported; additional recurrence properties cannot be discarded",
            ));
        }
        Some(
            lines[0]
                .as_str()
                .and_then(|s| s.strip_prefix("RRULE:"))
                .ok_or_else(|| invalid("unsupported recurrence property"))?
                .to_owned(),
        )
    } else {
        None
    };
    let optional = |key: &str| -> Result<Option<String>> {
        match value.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(v)) => Ok(Some(v.clone())),
            _ => Err(invalid(format!("invalid {key}"))),
        }
    };
    if let Some(attendees) = value.get("attendees") {
        for a in attendees
            .as_array()
            .ok_or_else(|| invalid("invalid attendees"))?
        {
            text(a, "email")?;
            if a.get("responseStatus").is_some_and(|status| {
                !matches!(
                    status.as_str(),
                    Some("accepted" | "tentative" | "declined" | "needsAction")
                )
            }) {
                return Err(invalid("invalid attendee response status"));
            }
        }
    }
    // Google's `self` denotes this calendar's copy, which may be a shared
    // calendar rather than the authenticated account. Preserve that flag in
    // native data and identify the account by its actual address here.
    let (attendees_json, my_status) = parse_google_attendees(value, &account.email, false);
    Ok(CalendarEvent {
        id: local.id.clone(),
        account_id: account.id.clone(),
        calendar_id: local.calendar_id.clone(),
        uid: Some(text(value, "iCalUID")?.into()),
        title: optional("summary")?.unwrap_or_default(),
        description: optional("description")?,
        location: optional("location")?,
        start_time,
        end_time,
        all_day,
        timezone,
        recurrence_rule,
        recurrence_kind,
        organizer_email: value["organizer"]["email"].as_str().map(str::to_owned),
        attendees_json,
        my_status,
        source_message_id: local.source_message_id.clone(),
        ical_data: None,
        remote_id: Some(text(value, "id")?.into()),
        etag: Some(text(value, "etag")?.into()),
    })
}

pub(super) async fn fetch(
    client: &GoogleClient,
    account: &AccountFull,
    event: &CalendarEvent,
    calendar: &str,
) -> Result<CalendarEventSet> {
    if calendar.is_empty() {
        return Err(invalid("empty calendar ID"));
    }
    let requested = event
        .remote_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| invalid("missing requested event ID"))?;
    let mut value = client.get_set_event(calendar, requested).await?;
    if let Some(master) = value["recurringEventId"].as_str() {
        value = client.get_set_event(calendar, master).await?;
    }
    if value["status"].as_str() == Some("cancelled") {
        return Err(invalid("master is cancelled"));
    }
    let master = parse_event(&value, account, event)?;
    let resource = native(&value, calendar)?;
    let mut overrides = Vec::new();
    if master.recurrence_kind == RecurrenceKind::Series {
        for item in client.list_set_resources(calendar).await? {
            if item["recurringEventId"].as_str() != Some(resource.event_id.as_str()) {
                continue;
            }
            overrides.push(CalendarOverride {
                original_start: original(&item, &master)?,
                event: if item["status"].as_str() == Some("cancelled") {
                    None
                } else {
                    Some(parse_event(&item, account, event)?)
                },
                native: Some(native(&item, calendar)?),
            });
        }
        // Detect a concurrent master edit across the complete paginated read.
        let current = client.get_set_event(calendar, &resource.event_id).await?;
        if current["etag"] != value["etag"] {
            return Err(invalid("master changed during complete read"));
        }
    }
    overrides.sort_by(|a, b| a.original_start.cmp(&b.original_start));
    let set = CalendarEventSet {
        event: master,
        overrides,
        native: Some(resource),
        content: None,
    };
    set.validate()?;
    Ok(set)
}

pub(super) async fn check_current(
    client: &GoogleClient,
    account: &AccountFull,
    before: &CalendarEventSet,
) -> Result<()> {
    let resource = validate_source(before, account)?;
    let current = fetch(client, account, &before.event, &resource.calendar_id).await?;
    let versions = |set: &CalendarEventSet| {
        set.overrides
            .iter()
            .map(|o| (o.original_start.clone(), o.native.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    if current.native != before.native || versions(&current) != versions(before) {
        return Err(invalid("source changed remotely; refresh before retrying"));
    }
    Ok(())
}

fn default_type(value: &Value) -> bool {
    value.get("eventType").is_none() || value["eventType"].as_str() == Some("default")
}

fn attendees_with_native(common: &Value, source: &Value) -> Value {
    let mut attendees = common.clone();
    if let Some(attendees) = attendees.as_array_mut() {
        for attendee in attendees {
            if let Some(original) = source["attendees"].as_array().and_then(|list| {
                list.iter().find(|a| {
                    a["email"]
                        .as_str()
                        .zip(attendee["email"].as_str())
                        .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
                })
            }) {
                for key in ["optional", "resource", "comment", "additionalGuests"] {
                    if let Some(value) = original.get(key) {
                        attendee[key] = value.clone();
                    }
                }
            }
        }
    }
    attendees
}

fn writable_attendees(value: &Value) -> Value {
    let mut attendees = value.clone();
    if let Some(attendees) = attendees.as_array_mut() {
        for attendee in attendees {
            if let Some(object) = attendee.as_object_mut() {
                for key in ["self", "organizer", "id"] {
                    object.remove(key);
                }
            }
        }
    }
    attendees
}

fn content_diff(before: &CalendarEvent, desired: &CalendarEvent, native: &Value) -> Result<Value> {
    let old = event_set_content(before)?;
    let new = event_set_content(desired)?;
    let mut patch = serde_json::Map::new();
    for key in [
        "summary",
        "description",
        "location",
        "start",
        "end",
        "attendees",
        "recurrence",
    ] {
        if old[key] != new[key] {
            let mut value = new[key].clone();
            if key == "attendees" {
                // Google replaces arrays. Retain native attendee-specific fields
                // (optional, resource, comment, additionalGuests) by email.
                value = attendees_with_native(&value, native);
            }
            patch.insert(key.into(), value);
        }
    }
    Ok(Value::Object(patch))
}

async fn instance(
    client: &GoogleClient,
    calendar: &str,
    master: &CalendarEvent,
    position: &str,
) -> Result<Value> {
    let id = master
        .remote_id
        .as_deref()
        .ok_or_else(|| invalid("missing master ID"))?;
    let mut matching = Vec::new();
    for value in client.find_set_instance(calendar, id, position).await? {
        if value["recurringEventId"].as_str() != Some(id) {
            return Err(invalid("instances returned foreign master"));
        }
        if original(&value, master)? == position {
            matching.push(value);
        }
    }
    if matching.len() != 1 {
        return Err(invalid(
            "original start did not resolve to exactly one authoritative instance",
        ));
    }
    matching.pop().ok_or_else(|| invalid("missing instance"))
}

fn validate_desired(desired: &CalendarEventSet) -> Result<()> {
    desired.validate()?;
    event_set_content(&desired.event)?;
    if let Some(n) = desired.native.as_ref().filter(|n| n.protocol == "google") {
        writable_native(&native_value(n)?)?;
    }
    for item in &desired.overrides {
        simple_recurrence::resolve(&desired.event, &item.original_start)?;
        if let Some(event) = &item.event {
            event_set_content(event)?;
        }
        if let Some(n) = item.native.as_ref().filter(|n| n.protocol == "google") {
            writable_native(&native_value(n)?)?;
        }
    }
    Ok(())
}

/// Replay complete effective exceptions after master changes. Lookups use the
/// new master's authoritative original positions, never old provider IDs.
async fn replay(
    client: &GoogleClient,
    account: &AccountFull,
    calendar: &str,
    master: &CalendarEvent,
    desired: &[CalendarOverride],
    expected: Option<&CalendarEventSet>,
) -> Result<()> {
    for item in desired {
        let value = instance(client, calendar, master, &item.original_start).await?;
        let id = text(&value, "id")?;
        if let Some(expected) = expected {
            if let Some(previous) = expected
                .overrides
                .iter()
                .find(|o| o.original_start == item.original_start)
            {
                let previous = previous
                    .native
                    .as_ref()
                    .ok_or_else(|| invalid("missing original exception revision"))?;
                if previous.event_id != id || previous.revision.as_deref() != value["etag"].as_str()
                {
                    return Err(invalid("exception changed after snapshot read"));
                }
            } else {
                let mut generated = expected.event.clone();
                let fields = simple_recurrence::resolve(&expected.event, &item.original_start)?;
                crate::calendar::event_set::apply_event_fields(&mut generated, &fields);
                generated.recurrence_rule = None;
                generated.recurrence_kind = RecurrenceKind::Occurrence;
                if value["status"].as_str() == Some("cancelled")
                    || !common_equal(&parse_event(&value, account, &generated)?, &generated)?
                {
                    return Err(invalid("generated slot acquired a concurrent exception"));
                }
            }
        }
        match &item.event {
            None if value["status"].as_str() != Some("cancelled") => {
                client
                    .delete_set_event(calendar, id, text(&value, "etag")?)
                    .await?
            }
            None => {}
            Some(event) => {
                let mut patch = if value["status"].as_str() == Some("cancelled") {
                    let mut body = event_set_content(event)?;
                    body["status"] = json!("confirmed");
                    body
                } else {
                    content_diff(&parse_event(&value, account, event)?, event, &value)?
                };
                if let Some(resource) = item.native.as_ref().filter(|n| n.protocol == "google") {
                    let source = native_value(resource)?;
                    // Replay native exception details if a rule change regenerated
                    // this slot, and preserve them on Google-to-Google copies.
                    for (key, wanted) in writable_native(&source)? {
                        if value[&key] != wanted {
                            patch[&key] = wanted;
                        }
                    }
                    let attendees =
                        attendees_with_native(&event_set_content(event)?["attendees"], &source);
                    if attendees != writable_attendees(&value["attendees"])
                        && !(attendees == json!([]) && value.get("attendees").is_none())
                    {
                        patch["attendees"] = attendees;
                    }
                }
                if patch.as_object().is_some_and(|p| !p.is_empty()) {
                    client
                        .patch_set_event(calendar, id, text(&value, "etag")?, &patch)
                        .await?;
                }
            }
        }
    }
    Ok(())
}

pub(super) async fn update(
    client: &GoogleClient,
    account: &AccountFull,
    before: &CalendarEventSet,
    desired: &CalendarEventSet,
) -> Result<CalendarEventSet> {
    let resource = validate_source(before, account)?;
    validate_desired(desired)?;
    if desired
        .native
        .as_ref()
        .is_some_and(|n| before.native.as_ref() != Some(n))
    {
        return Err(invalid("desired set supplied an untrusted native master"));
    }
    for item in &desired.overrides {
        if let Some(n) = &item.native {
            if !before
                .overrides
                .iter()
                .any(|old| old.native.as_ref() == Some(n))
            {
                return Err(invalid(
                    "desired override supplied an untrusted native identity",
                ));
            }
        }
    }
    if desired.event.account_id != before.event.account_id
        || desired.event.remote_id != before.event.remote_id
        || desired.event.uid != before.event.uid
        || desired.event.calendar_id != before.event.calendar_id
    {
        return Err(invalid("update changed immutable source identity"));
    }
    let value = native_value(resource)?;
    if !default_type(&value) {
        return Err(invalid("only default eventType supports scoped mutation"));
    }
    let patch = content_diff(&before.event, &desired.event, &value)?;
    let mut replay_items = desired.overrides.clone();
    // Removing an override restores the generated event; DELETE would exclude it.
    for old in &before.overrides {
        if !desired
            .overrides
            .iter()
            .any(|o| o.original_start == old.original_start)
        {
            if let Ok(fields) = simple_recurrence::resolve(&desired.event, &old.original_start) {
                let mut event = desired.event.clone();
                crate::calendar::event_set::apply_event_fields(&mut event, &fields);
                event.recurrence_kind = RecurrenceKind::Occurrence;
                event.recurrence_rule = None;
                replay_items.push(CalendarOverride {
                    original_start: old.original_start.clone(),
                    event: Some(event),
                    native: None,
                });
            }
        }
    }
    check_current(client, account, before).await?;
    let master_changed = patch.as_object().is_some_and(|p| !p.is_empty());
    if master_changed {
        client
            .patch_set_event(
                &resource.calendar_id,
                &resource.event_id,
                revision(resource)?,
                &patch,
            )
            .await?;
    }
    let mut master = desired.event.clone();
    master.remote_id = Some(resource.event_id.clone());
    replay(
        client,
        account,
        &resource.calendar_id,
        &master,
        &replay_items,
        if master_changed { None } else { Some(before) },
    )
    .await?;
    let canonical = fetch(client, account, &master, &resource.calendar_id).await?;
    verify(&canonical, desired)?;
    Ok(canonical)
}

/// Writable non-identity properties. Read-only fields, source organizer, IDs,
/// UID and shared/private operation identities must not become destination IDs.
fn writable_native(value: &Value) -> Result<serde_json::Map<String, Value>> {
    if !default_type(value) {
        return Err(invalid("only default eventType can be copied"));
    }
    let mut result = serde_json::Map::new();
    for key in [
        "reminders",
        "source",
        "conferenceData",
        "extendedProperties",
    ] {
        if value.get(key).is_some_and(|v| !v.is_object()) {
            return Err(invalid(format!("invalid native {key}")));
        }
    }
    for key in ["private", "shared"] {
        if value["extendedProperties"]
            .get(key)
            .is_some_and(|v| !v.is_object())
        {
            return Err(invalid("invalid native extended properties"));
        }
    }
    for key in [
        "colorId",
        "reminders",
        "transparency",
        "visibility",
        "guestsCanInviteOthers",
        "guestsCanModify",
        "guestsCanSeeOtherGuests",
        "attachments",
        "source",
        "conferenceData",
        "extendedProperties",
    ] {
        if let Some(value) = value.get(key) {
            result.insert(key.into(), value.clone());
        }
    }
    if matches!(value["status"].as_str(), Some("confirmed" | "tentative")) {
        result.insert("status".into(), value["status"].clone());
    }
    if let Some(properties) = result.get_mut("extendedProperties") {
        if let Some(private) = properties["private"].as_object_mut() {
            private.remove("chithiOperation");
        }
    }
    if let Some(conference) = result
        .get_mut("conferenceData")
        .and_then(Value::as_object_mut)
    {
        conference.remove("createRequest");
    }
    Ok(result)
}

fn operation_event_id(account: &str, calendar: &str, operation: &str) -> String {
    let mut hash = Sha256::new();
    for part in [account, calendar, operation] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    // Hex is a subset of Google's base32hex alphabet (0-9, a-v).
    format!("chithi{:x}", hash.finalize())
}

pub(super) async fn create(
    client: &GoogleClient,
    account: &AccountFull,
    calendar: &str,
    desired: &CalendarEventSet,
    operation: &str,
) -> Result<CalendarEventSet> {
    if operation.is_empty() || operation.len() > 1024 || calendar.is_empty() {
        return Err(invalid("invalid operation or destination identity"));
    }
    validate_desired(desired)?;
    let mut body = event_set_content(&desired.event)?;
    if let Some(resource) = desired.native.as_ref().filter(|n| n.protocol == "google") {
        let source = native_value(resource)?;
        for (key, value) in writable_native(&source)? {
            body[&key] = value;
        }
        body["attendees"] = attendees_with_native(&body["attendees"], &source);
    }
    let id = operation_event_id(&account.id, calendar, operation);
    body["id"] = json!(id);
    body["extendedProperties"]["private"]["chithiOperation"] = json!(operation);
    let inserted = client.insert_set_event(calendar, &body, operation).await?;
    if !common_equal(
        &parse_event(&inserted, account, &desired.event)?,
        &desired.event,
    )? {
        return Err(invalid(
            "operation already exists with different master content",
        ));
    }
    let mut target = desired.event.clone();
    target.account_id = account.id.clone();
    target.remote_id = Some(id);
    // On retries, already-created exceptions are read and diffed before writing.
    replay(client, account, calendar, &target, &desired.overrides, None).await?;
    let canonical = fetch(client, account, &target, calendar).await?;
    verify(&canonical, desired)?;
    Ok(canonical)
}

fn common_equal(a: &CalendarEvent, b: &CalendarEvent) -> Result<bool> {
    let mut a = event_set_content(a)?;
    let mut b = event_set_content(b)?;
    // Absent and empty optional text have the same Calendar API semantics.
    for key in ["description", "location"] {
        if a[key].is_null() {
            a[key] = json!("");
        }
        if b[key].is_null() {
            b[key] = json!("");
        }
    }
    for value in [&mut a, &mut b] {
        if let Some(attendees) = value["attendees"].as_array_mut() {
            attendees.sort_by(|a, b| a["email"].as_str().cmp(&b["email"].as_str()));
        }
    }
    Ok(a == b)
}

fn verify_native(
    canonical: Option<&NativeCalendarResource>,
    desired: Option<&NativeCalendarResource>,
) -> Result<()> {
    let Some(source) = desired.filter(|n| n.protocol == "google") else {
        return Ok(());
    };
    let source = native_value(source)?;
    let actual =
        native_value(canonical.ok_or_else(|| invalid("missing canonical native resource"))?)?;
    let expected = writable_native(&source)?;
    let actual_properties = writable_native(&actual)?;
    for (key, value) in expected {
        if actual_properties.get(&key) != Some(&value) {
            return Err(invalid(format!(
                "canonical native {key} differs; reconciliation required"
            )));
        }
    }
    if let Some(attendees) = source["attendees"].as_array() {
        for source in attendees {
            if let Some(actual) = actual["attendees"]
                .as_array()
                .and_then(|a| a.iter().find(|a| a["email"] == source["email"]))
            {
                for key in ["optional", "resource", "comment", "additionalGuests"] {
                    if source.get(key).is_some_and(|v| actual.get(key) != Some(v)) {
                        return Err(invalid(
                            "canonical attendee properties differ; reconciliation required",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn verify(canonical: &CalendarEventSet, desired: &CalendarEventSet) -> Result<()> {
    if !common_equal(&canonical.event, &desired.event)? {
        return Err(invalid(
            "canonical master differs from requested content; reconciliation required",
        ));
    }
    verify_native(canonical.native.as_ref(), desired.native.as_ref())?;
    for item in &desired.overrides {
        let actual = canonical
            .overrides
            .iter()
            .find(|o| o.original_start == item.original_start)
            .ok_or_else(|| invalid("canonical set omitted requested exception"))?;
        match (&actual.event, &item.event) {
            (None, None) => {}
            (Some(a), Some(b)) if common_equal(a, b)? => {}
            _ => {
                return Err(invalid(
                    "canonical exception differs; reconciliation required",
                ))
            }
        }
        if item.event.is_some() {
            verify_native(actual.native.as_ref(), item.native.as_ref())?;
        }
    }
    let wanted: HashSet<_> = desired
        .overrides
        .iter()
        .map(|o| &o.original_start)
        .collect();
    for item in &canonical.overrides {
        if wanted.contains(&item.original_start) {
            continue;
        }
        let mut generated = desired.event.clone();
        let fields = simple_recurrence::resolve(&desired.event, &item.original_start)?;
        crate::calendar::event_set::apply_event_fields(&mut generated, &fields);
        generated.recurrence_kind = RecurrenceKind::Occurrence;
        generated.recurrence_rule = None;
        if !item
            .event
            .as_ref()
            .map(|e| common_equal(e, &generated))
            .transpose()?
            .unwrap_or(false)
        {
            return Err(invalid(
                "unexpected canonical exception; reconciliation required",
            ));
        }
    }
    Ok(())
}

pub(super) async fn move_native(
    client: &GoogleClient,
    account: &AccountFull,
    before: &CalendarEventSet,
    destination: &str,
) -> Result<CalendarCapability<CalendarEventSet>> {
    let resource = validate_source(before, account)?;
    if destination.is_empty() {
        return Err(invalid("empty move destination"));
    }
    let value = native_value(resource)?;
    if !default_type(&value) {
        return Ok(CalendarCapability::Unsupported);
    }
    check_current(client, account, before).await?;
    for calendar in [&resource.calendar_id, destination] {
        match client.calendar_access_role(calendar).await?.as_str() {
            "owner" | "writer" => {}
            "reader" | "freeBusyReader" => return Ok(CalendarCapability::Unsupported),
            _ => return Err(invalid("unknown calendar access role")),
        }
    }
    if value["organizer"]["self"].as_bool() != Some(true) {
        return Ok(CalendarCapability::Unsupported);
    }
    client
        .move_set_event(
            &resource.calendar_id,
            &resource.event_id,
            revision(resource)?,
            destination,
        )
        .await?;
    let canonical = fetch(client, account, &before.event, destination).await?;
    if canonical.event.uid != before.event.uid {
        return Err(invalid("move changed UID; reconciliation required"));
    }
    verify(&canonical, before)?;
    Ok(CalendarCapability::Supported(canonical))
}
