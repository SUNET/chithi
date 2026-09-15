//! JMAP calendar domain: `Calendar/*` and `CalendarEvent/*` methods
//! (RFC 8984 JSCalendar).

use crate::calendar::recurrence::{faithful_local_recurrence_rules, valid_local_datetime};
use crate::calendar::recurrence_identity::{
    OccurrenceFields, RecurrenceIdentitySeed, RecurrenceObjectKind, RecurrenceValueType,
    UpdateOccurrenceInput,
};
use crate::calendar::RecurrenceKind;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use super::{JmapConfig, JmapConnection};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JmapCalendar {
    pub id: String,
    pub name: String,
    pub color: Option<String>,
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JmapCalendarEvent {
    pub id: String,
    pub calendar_id: String,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start: String, // ISO 8601
    pub end: String,
    pub all_day: bool,
    pub timezone: Option<String>,
    pub recurrence_rule: Option<String>,
    #[serde(default)]
    pub recurrence_kind: RecurrenceKind,
    /// Only known local creation can prove that RRULE is all recurrence data.
    #[serde(skip)]
    recurrence_rule_is_complete: bool,
    pub uid: Option<String>,
    pub organizer_email: Option<String>,
    pub attendees_json: Option<String>,
    #[serde(default)]
    pub native_json: Option<serde_json::Value>,
    #[serde(default)]
    pub response_state: Option<String>,
    #[serde(skip)]
    pub(crate) recurrence_seeds: Option<Vec<RecurrenceIdentitySeed>>,
}

impl JmapCalendarEvent {
    pub(crate) fn for_local_creation(
        event: &crate::calendar::CalendarEvent,
        remote_calendar_id: &str,
    ) -> Result<Self> {
        let wire = Self {
            id: String::new(),
            calendar_id: remote_calendar_id.to_string(),
            title: event.title.clone(),
            description: event.description.clone(),
            location: event.location.clone(),
            start: event.start_time.clone(),
            end: event.end_time.clone(),
            all_day: event.all_day,
            timezone: event.timezone.clone(),
            recurrence_rule: event.recurrence_rule.clone(),
            recurrence_kind: event.recurrence_kind,
            recurrence_rule_is_complete: event.recurrence_kind == RecurrenceKind::Series
                && ((event.ical_data.is_none() && event.source_message_id.is_none())
                    || event
                        .ical_data
                        .as_deref()
                        .is_some_and(crate::calendar::ical::is_rrule_only_series)),
            uid: event.uid.clone(),
            organizer_email: event.organizer_email.clone(),
            attendees_json: event.attendees_json.clone(),
            native_json: None,
            response_state: None,
            recurrence_seeds: None,
        };
        wire.creation_recurrence_rules()?;
        Ok(wire)
    }

    /// Validate before transport I/O; never drop an unsupported rule or instance id.
    fn creation_recurrence_rules(&self) -> Result<Option<serde_json::Value>> {
        let unsupported = |reason: &str| {
            Error::Other(format!(
            "Cannot create JMAP event '{}': {reason}. Keep this event local and manage its recurrence in the source calendar",
            self.title
        ))
        };
        let rule = self
            .recurrence_rule
            .as_deref()
            .filter(|rule| !rule.is_empty());
        match self.recurrence_kind {
            RecurrenceKind::Unknown => Err(unsupported("recurrence classification is unknown")),
            RecurrenceKind::Occurrence => {
                Err(unsupported("detached occurrence creation is unsupported"))
            }
            RecurrenceKind::Standalone if rule.is_none() => Ok(None),
            RecurrenceKind::Standalone => Err(unsupported(
                "standalone classification conflicts with a recurrence rule",
            )),
            RecurrenceKind::Series if !self.recurrence_rule_is_complete => Err(unsupported(
                "source recurrence data cannot be represented by the available RRULE alone",
            )),
            RecurrenceKind::Series => {
                let rule =
                    rule.ok_or_else(|| unsupported("a complete recurrence rule is missing"))?;
                let rules = faithful_local_recurrence_rules(rule, self.timezone.as_deref())
                    .ok_or_else(|| {
                        unsupported("the recurrence rule cannot be represented faithfully")
                    })?;
                Ok(Some(rules))
            }
        }
    }

    /// Build an authoritative patch for a personal copy. Scheduling data is
    /// always cleared, including participants left on an older remote copy.
    pub(crate) fn personal_copy_update_patch(
        event: &crate::calendar::CalendarEvent,
    ) -> Result<serde_json::Value> {
        let wire = Self::for_local_creation(event, "")?;
        let recurrence_rules = wire.creation_recurrence_rules()?;
        let start = if event.all_day {
            if event.start_time.contains('T') {
                event.start_time.trim_end_matches('Z').to_string()
            } else {
                format!("{}T00:00:00", event.start_time)
            }
        } else if let Some(timezone) = event.timezone.as_deref() {
            crate::mail::caldav::utc_to_local(&event.start_time, timezone)
        } else {
            event.start_time.trim_end_matches('Z').to_string()
        };
        let locations = event
            .location
            .as_deref()
            .filter(|location| !location.is_empty())
            .map(|location| {
                serde_json::json!({
                    "loc1": {"@type": "Location", "name": location}
                })
            })
            .unwrap_or_else(|| serde_json::json!({}));

        Ok(serde_json::json!({
            "title": event.title,
            "description": event.description,
            "locations": locations,
            "start": start,
            "duration": compute_duration(&event.start_time, &event.end_time),
            "showWithoutTime": event.all_day,
            "timeZone": event.timezone,
            "participants": {},
            "recurrenceRules": recurrence_rules,
            "recurrenceRule": null,
            "excludedRecurrenceRules": null,
            "recurrenceOverrides": null,
        }))
    }

    pub(crate) fn occurrence_update_patch(
        recurrence_id: Option<&str>,
        changed: &UpdateOccurrenceInput,
        desired: &OccurrenceFields,
    ) -> serde_json::Value {
        occurrence_update_patch(recurrence_id, changed, desired)
    }
}

/// Fetch the provider's complete native JSCalendar Event representation.
/// JMAP Calendars §5.7 defines omitted `properties` to return stored properties;
/// listing names from different schema versions risks RFC 8620 §5.1 rejection.
const CALENDAR_EVENT_QUERY_PAGE_SIZE: usize = 500;
const CALENDAR_EVENT_MAX_PAGES: usize = 100;
const CALENDAR_EVENT_MAX_IDS: usize = 50_000;
const CALENDAR_EVENT_MAX_GET_CHUNK: usize = 500;

fn calendar_event_query_request(
    account_id: &str,
    position: usize,
    limit: usize,
    call_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:calendars"],
        "methodCalls": [["CalendarEvent/query", {
            "accountId": account_id,
            "position": position,
            "limit": limit,
            "calculateTotal": true
        }, call_id]]
    })
}

fn calendar_events_get_request(account_id: &str, ids: &[String]) -> serde_json::Value {
    serde_json::json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:calendars"],
        "methodCalls": [["CalendarEvent/get", {
            "accountId": account_id,
            "ids": ids
        }, "g1"]]
    })
}

fn method_response<'a>(
    response: &'a serde_json::Value,
    method_name: &str,
    call_id: &str,
) -> Result<&'a serde_json::Map<String, serde_json::Value>> {
    let responses = response["methodResponses"].as_array().ok_or_else(|| {
        Error::Sync(format!(
            "JMAP {method_name} response omitted methodResponses"
        ))
    })?;
    if responses.len() != 1 {
        return Err(Error::Sync(format!(
            "JMAP {method_name} returned an unexpected number of method responses"
        )));
    }
    let response = responses[0]
        .as_array()
        .filter(|response| response.len() == 3);
    let response = response.ok_or_else(|| {
        Error::Sync(format!(
            "JMAP {method_name} returned a malformed method response"
        ))
    })?;
    if response[0].as_str() != Some(method_name) || response[2].as_str() != Some(call_id) {
        return Err(Error::Sync(format!(
            "JMAP {method_name} response correlation failed"
        )));
    }
    response[1]
        .as_object()
        .ok_or_else(|| Error::Sync(format!("JMAP {method_name} response body is malformed")))
}

fn valid_identifier(value: &str) -> bool {
    !value.trim().is_empty() && !value.chars().any(char::is_control)
}

fn event_calendar_ids(event: &serde_json::Value) -> Result<Vec<String>> {
    let memberships = event["calendarIds"]
        .as_object()
        .ok_or_else(|| Error::Sync("JMAP CalendarEvent object has malformed calendarIds".into()))?;
    let mut ids = Vec::new();
    for (id, member) in memberships {
        if !valid_identifier(id) || !member.is_boolean() {
            return Err(Error::Sync(
                "JMAP CalendarEvent object has malformed calendarIds".into(),
            ));
        }
        if member.as_bool() == Some(true) {
            ids.push(id.clone());
        }
    }
    if ids.is_empty() {
        return Err(Error::Sync(
            "JMAP CalendarEvent object has no calendar membership".into(),
        ));
    }
    Ok(ids)
}

fn validate_event_object(event: &serde_json::Value) -> Result<String> {
    if !event.is_object() || event["@type"].as_str() != Some("Event") {
        return Err(Error::Sync(
            "JMAP CalendarEvent/get returned a malformed event object".into(),
        ));
    }
    let id = event["id"]
        .as_str()
        .filter(|id| valid_identifier(id))
        .ok_or_else(|| Error::Sync("JMAP CalendarEvent object has no valid id".into()))?;
    if !event["uid"].as_str().is_some_and(valid_identifier)
        || !event["title"].is_string()
        || event_calendar_ids(event).is_err()
        || occurrence_fields(event, None, None).is_none()
    {
        return Err(Error::Sync(format!(
            "JMAP CalendarEvent object {id} is malformed"
        )));
    }
    Ok(id.to_string())
}

/// Classify the native provider object before lossy DTO/RRULE conversion.
/// Optional absent/null properties and empty recurrence sets mean no recurrence.
/// RFC 8984 §4.3 and JSCalendar-bis §4.3 define the two rule representations;
/// JMAP Calendars §5 defines detached instances and synthetic `baseEventId`.
fn classify_recurrence(event: &serde_json::Value) -> RecurrenceKind {
    use serde_json::Value;

    if event["@type"].as_str() != Some("Event") {
        return RecurrenceKind::Unknown;
    }
    let present = |name: &str| event.get(name).filter(|value| !value.is_null());
    let recurrence_id = match present("recurrenceId") {
        Some(Value::String(value)) if valid_local_datetime(value) => true,
        Some(_) => return RecurrenceKind::Unknown,
        None => false,
    };
    if let Some(timezone) = present("recurrenceIdTimeZone") {
        if !recurrence_id || !timezone.as_str().is_some_and(|s| !s.is_empty()) {
            return RecurrenceKind::Unknown;
        }
    }
    if let Some(base_id) = present("baseEventId") {
        if !recurrence_id || !base_id.as_str().is_some_and(|s| !s.is_empty()) {
            return RecurrenceKind::Unknown;
        }
    }
    if let Some(excluded) = present("excluded") {
        if !excluded.is_boolean() || (excluded == &Value::Bool(true) && !recurrence_id) {
            return RecurrenceKind::Unknown;
        }
    }

    let mut recurring = false;
    for name in ["recurrenceRules", "excludedRecurrenceRules"] {
        if let Some(value) = present(name) {
            let Some(rules) = value.as_array() else {
                return RecurrenceKind::Unknown;
            };
            if !rules.iter().all(valid_recurrence_rule) {
                return RecurrenceKind::Unknown;
            }
            recurring |= !rules.is_empty();
        }
    }
    if let Some(rule) = present("recurrenceRule") {
        if !valid_recurrence_rule(rule) {
            return RecurrenceKind::Unknown;
        }
        recurring = true;
    }
    if let Some(value) = present("recurrenceOverrides") {
        let Some(overrides) = value.as_object() else {
            return RecurrenceKind::Unknown;
        };
        for (id, patch) in overrides {
            let Some(patch) = patch.as_object() else {
                return RecurrenceKind::Unknown;
            };
            // An empty patch adds a date; it is not an empty recurrence set.
            if !valid_local_datetime(id)
                || patch
                    .get("@type")
                    .is_some_and(|value| value.as_str() != Some("Event"))
                || patch
                    .get("excluded")
                    .is_some_and(|value| !value.is_boolean())
            {
                return RecurrenceKind::Unknown;
            }
        }
        recurring |= !overrides.is_empty();
    }
    if recurrence_id {
        return if recurring {
            RecurrenceKind::Unknown
        } else {
            RecurrenceKind::Occurrence
        };
    }

    // "This and future" splits link the series with first/next relations.
    if let Some(value) = present("relatedTo") {
        let Some(relations) = value.as_object() else {
            return RecurrenceKind::Unknown;
        };
        for relation in relations.values() {
            if !relation.is_object()
                || relation
                    .get("@type")
                    .is_some_and(|value| value.as_str() != Some("Relation"))
            {
                return RecurrenceKind::Unknown;
            }
            if let Some(value) = relation.get("relation") {
                let Some(kinds) = value.as_object() else {
                    return RecurrenceKind::Unknown;
                };
                if kinds.values().any(|value| value.as_bool() != Some(true)) {
                    return RecurrenceKind::Unknown;
                }
                recurring |= kinds.contains_key("first") || kinds.contains_key("next");
            }
        }
    }
    if recurring {
        return RecurrenceKind::Series;
    }

    // A native event can omit legitimate optional fields, but not its event
    // identity/start. Do not authorize mutation from malformed DTO fallbacks.
    if !["id", "uid"]
        .iter()
        .all(|name| event[*name].as_str().is_some_and(|s| !s.trim().is_empty()))
        || !event["start"].as_str().is_some_and(valid_local_datetime)
        || !event["calendarIds"].as_object().is_some_and(|ids| {
            !ids.is_empty()
                && ids
                    .iter()
                    .all(|(id, value)| !id.is_empty() && value.as_bool() == Some(true))
        })
        || ["title", "description", "timeZone", "duration"]
            .iter()
            .any(|name| present(name).is_some_and(|value| !value.is_string()))
        || present("showWithoutTime").is_some_and(|value| !value.is_boolean())
        || [("locations", "Location"), ("participants", "Participant")]
            .iter()
            .any(|(name, expected_type)| {
                present(name).is_some_and(|value| {
                    !value.as_object().is_some_and(|objects| {
                        objects.values().all(|object| {
                            object.is_object()
                                && object
                                    .get("@type")
                                    .is_none_or(|value| value.as_str() == Some(*expected_type))
                        })
                    })
                })
            })
    {
        return RecurrenceKind::Unknown;
    }
    RecurrenceKind::Standalone
}

/// Validate rule identity without restricting recurrence to the local expander.
fn valid_recurrence_rule(rule: &serde_json::Value) -> bool {
    rule.is_object()
        && rule
            .get("@type")
            .is_none_or(|value| value.as_str() == Some("RecurrenceRule"))
        && matches!(
            rule["frequency"].as_str(),
            Some("yearly" | "monthly" | "weekly" | "daily" | "hourly" | "minutely" | "secondly")
        )
}

#[derive(Clone, Copy)]
struct EventDuration {
    days: i64,
    seconds: i64,
    nanoseconds: i64,
}

fn parse_event_duration(value: &str) -> Option<EventDuration> {
    let value = value.strip_prefix('P')?;
    if value.is_empty() || !value.is_ascii() {
        return None;
    }
    let mut duration = EventDuration {
        days: 0,
        seconds: 0,
        nanoseconds: 0,
    };
    let mut number = String::new();
    let mut in_time = false;
    let mut saw_value = false;
    let mut saw_time_value = false;
    let mut last_rank = 0;
    for character in value.chars() {
        if character.is_ascii_digit() || character == '.' {
            number.push(character);
            continue;
        }
        if character == 'T' {
            if in_time || !number.is_empty() {
                return None;
            }
            in_time = true;
            continue;
        }
        if number.is_empty() {
            return None;
        }
        let rank = match (in_time, character) {
            (false, 'W') => 0,
            (false, 'D') => 1,
            (true, 'H') => 2,
            (true, 'M') => 3,
            (true, 'S') => 4,
            _ => return None,
        };
        if saw_value && rank <= last_rank {
            return None;
        }
        last_rank = rank;
        saw_value = true;
        saw_time_value |= in_time;
        if character == 'S' && number.contains('.') {
            let (whole, fraction) = number.split_once('.')?;
            if whole.is_empty()
                || fraction.is_empty()
                || fraction.len() > 9
                || fraction.ends_with('0')
            {
                return None;
            }
            duration.seconds = duration.seconds.checked_add(whole.parse().ok()?)?;
            let nanos = format!("{fraction:0<9}").parse::<i64>().ok()?;
            duration.nanoseconds = nanos;
        } else {
            if number.contains('.') {
                return None;
            }
            let amount = number.parse::<i64>().ok()?;
            match character {
                'W' => duration.days = duration.days.checked_add(amount.checked_mul(7)?)?,
                'D' => duration.days = duration.days.checked_add(amount)?,
                'H' => {
                    duration.seconds = duration.seconds.checked_add(amount.checked_mul(3600)?)?
                }
                'M' => duration.seconds = duration.seconds.checked_add(amount.checked_mul(60)?)?,
                'S' => duration.seconds = duration.seconds.checked_add(amount)?,
                _ => unreachable!(),
            }
        }
        number.clear();
    }
    if !saw_value || !number.is_empty() || (in_time && !saw_time_value) {
        return None;
    }
    Some(duration)
}

fn effective_range(
    start: &str,
    timezone: Option<&str>,
    duration: &str,
) -> Option<(String, String)> {
    use chrono::{SecondsFormat, TimeZone};

    if !valid_local_datetime(start) {
        return None;
    }
    let duration = parse_event_duration(duration)?;
    let local = chrono::NaiveDateTime::parse_from_str(start, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    let end_local_date = local.checked_add_signed(chrono::Duration::try_days(duration.days)?)?;
    let to_utc = |local| match timezone {
        Some(timezone) => {
            let timezone = timezone.parse::<chrono_tz::Tz>().ok()?;
            match timezone.from_local_datetime(&local) {
                chrono::LocalResult::Single(value) => Some(value.with_timezone(&chrono::Utc)),
                _ => None,
            }
        }
        None => Some(chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
            local,
            chrono::Utc,
        )),
    };
    let start = to_utc(local)?;
    let end = to_utc(end_local_date)?
        .checked_add_signed(chrono::Duration::try_seconds(duration.seconds)?)?
        .checked_add_signed(chrono::Duration::nanoseconds(duration.nanoseconds))?;
    if end <= start {
        return None;
    }
    Some((
        start.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        end.to_rfc3339_opts(SecondsFormat::AutoSi, true),
    ))
}

fn patched_string<'a>(
    base: Option<&'a str>,
    patch: &'a serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Option<Option<&'a str>> {
    match patch.get(name) {
        None => Some(base),
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::String(value)) => Some(Some(value)),
        Some(_) => None,
    }
}

fn location_name(value: Option<&serde_json::Value>) -> Option<Option<String>> {
    match value {
        None | Some(serde_json::Value::Null) => Some(None),
        Some(value) => {
            let locations = value.as_object()?;
            let name = locations
                .values()
                .find_map(|location| location.get("name").and_then(serde_json::Value::as_str));
            Some(name.map(str::to_string))
        }
    }
}

fn occurrence_fields(
    event: &serde_json::Value,
    patch: Option<&serde_json::Map<String, serde_json::Value>>,
    recurrence_id: Option<&str>,
) -> Option<OccurrenceFields> {
    let empty = serde_json::Map::new();
    let patch = patch.unwrap_or(&empty);
    let title = patched_string(
        event.get("title").and_then(serde_json::Value::as_str),
        patch,
        "title",
    )??;
    let description = patched_string(
        event.get("description").and_then(serde_json::Value::as_str),
        patch,
        "description",
    )?
    .filter(|description| !description.is_empty())
    .map(str::to_string);
    let location = match patch.get("locations") {
        Some(value) => location_name(Some(value))?,
        None => location_name(event.get("locations"))?,
    };
    let start = patched_string(
        recurrence_id.or_else(|| event.get("start").and_then(serde_json::Value::as_str)),
        patch,
        "start",
    )??;
    let timezone = patched_string(
        event.get("timeZone").and_then(serde_json::Value::as_str),
        patch,
        "timeZone",
    )?;
    let duration = patched_string(
        event.get("duration").and_then(serde_json::Value::as_str),
        patch,
        "duration",
    )??;
    let all_day = match patch.get("showWithoutTime") {
        Some(value) => value.as_bool()?,
        None => event
            .get("showWithoutTime")
            .map_or(Some(false), serde_json::Value::as_bool)?,
    };
    let (start_time, end_time) = if all_day {
        let duration = parse_event_duration(duration)?;
        if duration.seconds != 0 || duration.nanoseconds != 0 || duration.days <= 0 {
            return None;
        }
        let start = chrono::NaiveDateTime::parse_from_str(start, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
        let end = start.checked_add_signed(chrono::Duration::try_days(duration.days)?)?;
        (
            start.date().format("%Y-%m-%d").to_string(),
            end.date().format("%Y-%m-%d").to_string(),
        )
    } else {
        effective_range(start, timezone, duration)?
    };
    let fields = OccurrenceFields {
        title: title.to_string(),
        description,
        location,
        start_time,
        end_time,
        all_day,
        timezone: timezone.map(str::to_string),
    };
    fields.validate().ok()?;
    Some(fields)
}

fn sole_calendar_id(event: &serde_json::Value) -> Option<&str> {
    let calendar_ids = event.get("calendarIds")?.as_object()?;
    let mut membership = None;
    for (id, value) in calendar_ids {
        if id.trim().is_empty() || id.chars().any(char::is_control) {
            return None;
        }
        match value.as_bool()? {
            true if membership.is_some() => return None,
            true => membership = Some(id.as_str()),
            false => {}
        }
    }
    membership
}

fn recurrence_seeds(
    event: &serde_json::Value,
    kind: RecurrenceKind,
    native: &str,
    state: Option<&str>,
) -> Option<Vec<RecurrenceIdentitySeed>> {
    let revision = state.map(str::to_string);
    if kind == RecurrenceKind::Unknown {
        return None;
    }
    let provider_calendar_id = sole_calendar_id(event)?.to_string();
    if kind == RecurrenceKind::Standalone {
        return Some(Vec::new());
    }

    let event_id = event["id"].as_str()?;
    let event_start = event["start"].as_str()?;
    let event_timezone = match event.get("timeZone") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(value.as_str()?),
    };
    let event_occurrence = occurrence_fields(event, None, None)?;

    if kind == RecurrenceKind::Occurrence {
        let provider_series_id = event["baseEventId"].as_str()?;
        let recurrence_id = event["recurrenceId"].as_str()?;
        let recurrence_timezone = match event.get("recurrenceIdTimeZone") {
            None => event_timezone,
            Some(serde_json::Value::Null) => None,
            Some(value) => Some(value.as_str()?),
        };
        let object_kind = if event["excluded"].as_bool() == Some(true) {
            RecurrenceObjectKind::Exclusion
        } else if recurrence_id != event_start || recurrence_timezone != event_timezone {
            RecurrenceObjectKind::Exception
        } else {
            RecurrenceObjectKind::Occurrence
        };
        let seed = RecurrenceIdentitySeed {
            local_series_event_id: None,
            provider_calendar_id: Some(provider_calendar_id),
            provider_series_id: Some(provider_series_id.to_string()),
            provider_occurrence_id: Some(event_id.to_string()),
            recurrence_id: Some(recurrence_id.to_string()),
            recurrence_timezone: recurrence_timezone.map(str::to_string),
            recurrence_value_type: Some(RecurrenceValueType::DateTime),
            occurrence: event_occurrence,
            provider_native_data: Some(native.to_string()),
            provider_revision: revision,
            kind: object_kind,
        };
        seed.validate().ok()?;
        return Some(vec![seed]);
    }

    let mut seeds = vec![RecurrenceIdentitySeed {
        local_series_event_id: None,
        provider_calendar_id: Some(provider_calendar_id.clone()),
        provider_series_id: Some(event_id.to_string()),
        provider_occurrence_id: None,
        recurrence_id: None,
        recurrence_timezone: None,
        recurrence_value_type: None,
        occurrence: event_occurrence,
        provider_native_data: Some(native.to_string()),
        provider_revision: revision.clone(),
        kind: RecurrenceObjectKind::Master,
    }];
    if let Some(overrides) = event
        .get("recurrenceOverrides")
        .filter(|value| !value.is_null())
    {
        for (recurrence_id, value) in overrides.as_object()? {
            let patch = value.as_object()?;
            let recurrence_timezone = match patch.get("recurrenceIdTimeZone") {
                None => event_timezone,
                Some(serde_json::Value::Null) => None,
                Some(value) => Some(value.as_str()?),
            };
            let seed = RecurrenceIdentitySeed {
                local_series_event_id: None,
                provider_calendar_id: Some(provider_calendar_id.clone()),
                provider_series_id: Some(event_id.to_string()),
                provider_occurrence_id: None,
                recurrence_id: Some(recurrence_id.clone()),
                recurrence_timezone: recurrence_timezone.map(str::to_string),
                recurrence_value_type: Some(RecurrenceValueType::DateTime),
                occurrence: occurrence_fields(event, Some(patch), Some(recurrence_id))?,
                provider_native_data: Some(native.to_string()),
                provider_revision: revision.clone(),
                kind: if patch.get("excluded").and_then(serde_json::Value::as_bool) == Some(true) {
                    RecurrenceObjectKind::Exclusion
                } else if patch.is_empty() {
                    RecurrenceObjectKind::Occurrence
                } else {
                    RecurrenceObjectKind::Exception
                },
            };
            seed.validate().ok()?;
            seeds.push(seed);
        }
    }
    seeds.first()?.validate().ok()?;
    Some(seeds)
}

fn patch_path_segment(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

pub(crate) fn occurrence_update_patch(
    recurrence_id: Option<&str>,
    changed: &UpdateOccurrenceInput,
    desired: &OccurrenceFields,
) -> serde_json::Value {
    let embedded = recurrence_id.is_some();
    let prefix = recurrence_id
        .map(|id| format!("recurrenceOverrides/{}/", patch_path_segment(id)))
        .unwrap_or_default();
    let mut patch = serde_json::Map::new();
    let mut insert = |name: &str, value| {
        patch.insert(format!("{prefix}{name}"), value);
    };
    if changed.title.is_some() {
        insert("title", serde_json::json!(desired.title));
    }
    if changed.description.is_some() {
        insert(
            "description",
            desired
                .description
                .as_deref()
                .filter(|description| !description.is_empty())
                .map_or_else(
                    || {
                        if embedded {
                            serde_json::json!("")
                        } else {
                            serde_json::Value::Null
                        }
                    },
                    |value| serde_json::json!(value),
                ),
        );
    }
    if changed.location.is_some() {
        let locations = desired
            .location
            .as_deref()
            .filter(|location| !location.is_empty())
            .map(|location| {
                serde_json::json!({
                    "loc1": {"@type": "Location", "name": location}
                })
            })
            .unwrap_or_else(|| serde_json::json!({}));
        insert("locations", locations);
    }

    let all_day_changed = changed.all_day.is_some();
    let start_changed =
        changed.start_time.is_some() || changed.timezone.is_some() || all_day_changed;
    let duration_changed =
        changed.start_time.is_some() || changed.end_time.is_some() || all_day_changed;
    if start_changed {
        let start = if desired.all_day {
            format!("{}T00:00:00", desired.start_time)
        } else if let Some(timezone) = desired.timezone.as_deref() {
            crate::mail::caldav::utc_to_local(&desired.start_time, timezone)
        } else {
            desired.start_time.trim_end_matches('Z').to_string()
        };
        insert("start", serde_json::json!(start));
    }
    if duration_changed {
        insert(
            "duration",
            serde_json::json!(compute_duration(&desired.start_time, &desired.end_time)),
        );
    }
    if changed.timezone.is_some() || all_day_changed {
        insert("timeZone", serde_json::json!(desired.timezone));
    }
    if all_day_changed {
        insert("showWithoutTime", serde_json::json!(desired.all_day));
    }
    serde_json::Value::Object(patch)
}

fn occurrence_update_request(
    account_id: &str,
    event_id: &str,
    expected_state: &str,
    patch: &serde_json::Value,
) -> serde_json::Value {
    let mut update = serde_json::Map::new();
    update.insert(event_id.to_string(), patch.clone());
    serde_json::json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:calendars"],
        "methodCalls": [["CalendarEvent/set", {
            "accountId": account_id,
            "ifInState": expected_state,
            "sendSchedulingMessages": false,
            "update": update
        }, "u1"]]
    })
}

fn calendar_event_get_request(account_id: &str, event_id: &str) -> serde_json::Value {
    serde_json::json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:calendars"],
        "methodCalls": [["CalendarEvent/get", {
            "accountId": account_id,
            "ids": [event_id]
        }, "g1"]]
    })
}

fn occurrence_update_state(response: &serde_json::Value, event_id: &str) -> Result<String> {
    let method = &response["methodResponses"][0];
    if method[0].as_str() == Some("error") && method[1]["type"].as_str() == Some("stateMismatch") {
        return Err(Error::Sync(
            "JMAP CalendarEvent state changed; reconciliation required".into(),
        ));
    }
    if method[0].as_str() != Some("CalendarEvent/set") {
        return Err(Error::Sync(
            "Invalid JMAP CalendarEvent/set response; reconciliation required".into(),
        ));
    }
    if let Some(error) = method[1]["notUpdated"][event_id].as_object() {
        let error_type = error
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let description = error
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Unknown error");
        if error_type == "stateMismatch" {
            return Err(Error::Sync(
                "JMAP CalendarEvent state changed; reconciliation required".into(),
            ));
        }
        return Err(Error::Other(format!(
            "JMAP update calendar occurrence failed: {description}"
        )));
    }
    if method[1]["updated"].get(event_id).is_none() {
        return Err(Error::Sync(
            "JMAP did not confirm the occurrence update; reconciliation required".into(),
        ));
    }
    method[1]["newState"]
        .as_str()
        .filter(|state| !state.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            Error::Sync("JMAP CalendarEvent/set omitted newState; reconciliation required".into())
        })
}

impl JmapConnection {
    /// List all JMAP calendars for the account.
    pub async fn list_jmap_calendars(&self, config: &JmapConfig) -> Result<Vec<JmapCalendar>> {
        log::debug!("JMAP listing calendars");
        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["Calendar/get", {
                    "accountId": self.account_id,
                    "properties": ["id", "name", "color", "isDefault"]
                }, "c1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        let calendars_json = resp["methodResponses"][0][1]["list"]
            .as_array()
            .ok_or_else(|| Error::Other("Invalid Calendar/get response".into()))?;

        let mut calendars = Vec::new();
        for cal in calendars_json {
            let id = cal["id"].as_str().unwrap_or("").to_string();
            let name = cal["name"].as_str().unwrap_or("Untitled").to_string();
            let color = cal["color"].as_str().map(|s| s.to_string());
            let is_default = cal["isDefault"].as_bool().unwrap_or(false);

            log::debug!("  calendar: {} ({}) default={}", name, id, is_default);
            calendars.push(JmapCalendar {
                id,
                name,
                color,
                is_default,
            });
        }
        log::info!("JMAP found {} calendars", calendars.len());
        Ok(calendars)
    }

    /// Update the JMAP `color` property on a calendar via
    /// `Calendar/set`. JMAP calendars (RFC 8984 / "JSCalendar") store
    /// color as a CSS-format string, conventionally a `#RRGGBB` hex.
    /// Stalwart and Cyrus both honor the property; servers that
    /// don't will surface the rejection in `notUpdated` and we
    /// return that as an error so the caller can roll back.
    pub async fn set_calendar_color(
        &self,
        config: &JmapConfig,
        calendar_id: &str,
        hex: &str,
    ) -> Result<()> {
        log::info!("JMAP set color for calendar {} -> {}", calendar_id, hex);

        let mut update = serde_json::Map::new();
        update.insert(calendar_id.to_string(), serde_json::json!({ "color": hex }));

        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["Calendar/set", {
                    "accountId": self.account_id,
                    "update": update
                }, "c1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notUpdated"][calendar_id].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP Calendar/set rejected color update: {}",
                desc
            )));
        }

        log::info!("JMAP color set for calendar {}", calendar_id);
        Ok(())
    }

    /// Rename a JMAP calendar via `Calendar/set` with an update
    /// entry whose `name` field carries the new display name.
    pub async fn rename_calendar(
        &self,
        config: &JmapConfig,
        calendar_id: &str,
        new_name: &str,
    ) -> Result<()> {
        log::info!("JMAP rename calendar: id={} -> {}", calendar_id, new_name);

        // Build the update map by hand: `serde_json::json!({ calendar_id: ... })`
        // would emit the literal key "calendar_id", not the id's value.
        let mut update = serde_json::Map::new();
        update.insert(
            calendar_id.to_string(),
            serde_json::json!({ "name": new_name }),
        );

        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["Calendar/set", {
                    "accountId": self.account_id,
                    "update": update
                }, "c1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notUpdated"][calendar_id].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP Calendar/set rejected rename: {}",
                desc
            )));
        }

        log::info!("JMAP renamed calendar {}", calendar_id);
        Ok(())
    }

    /// Fetch a complete, stable account-wide CalendarEvent snapshot.
    ///
    /// The second and third tuple members identify calendars and remote events
    /// for which absence-based reconciliation is unsafe because the local row
    /// model cannot represent multiple calendar memberships.
    pub async fn fetch_calendar_events(
        &self,
        config: &JmapConfig,
    ) -> Result<(Vec<JmapCalendarEvent>, HashSet<String>, HashSet<String>)> {
        log::debug!("JMAP fetching complete calendar event snapshot");

        let mut ids = Vec::new();
        let mut seen_ids = HashSet::new();
        let mut query_state = None;
        let mut expected_total = None;
        for page in 0..CALENDAR_EVENT_MAX_PAGES {
            let position = ids.len();
            let request = calendar_event_query_request(
                &self.account_id,
                position,
                CALENDAR_EVENT_QUERY_PAGE_SIZE,
                "q1",
            );
            let response = self.api_request(&request, config).await?;
            let body = method_response(&response, "CalendarEvent/query", "q1")?;
            if body.get("accountId").and_then(serde_json::Value::as_str)
                != Some(self.account_id.as_str())
            {
                return Err(Error::Sync(
                    "JMAP CalendarEvent/query returned the wrong accountId".into(),
                ));
            }
            let state = body
                .get("queryState")
                .and_then(serde_json::Value::as_str)
                .filter(|state| !state.is_empty())
                .ok_or_else(|| Error::Sync("JMAP CalendarEvent/query omitted queryState".into()))?;
            if query_state
                .as_deref()
                .is_some_and(|expected| expected != state)
            {
                return Err(Error::Sync(
                    "JMAP CalendarEvent query state changed while paging".into(),
                ));
            }
            query_state.get_or_insert_with(|| state.to_string());
            let response_position = body
                .get("position")
                .and_then(serde_json::Value::as_u64)
                .and_then(|position| usize::try_from(position).ok())
                .ok_or_else(|| {
                    Error::Sync("JMAP CalendarEvent/query omitted a valid position".into())
                })?;
            if response_position != position {
                return Err(Error::Sync(
                    "JMAP CalendarEvent/query returned a repeated or unexpected position".into(),
                ));
            }
            let total = body
                .get("total")
                .and_then(serde_json::Value::as_u64)
                .and_then(|total| usize::try_from(total).ok())
                .ok_or_else(|| {
                    Error::Sync("JMAP CalendarEvent/query omitted a valid total".into())
                })?;
            if total > CALENDAR_EVENT_MAX_IDS {
                return Err(Error::Sync(format!(
                    "JMAP CalendarEvent snapshot exceeds the {CALENDAR_EVENT_MAX_IDS} object limit"
                )));
            }
            if expected_total.is_some_and(|expected| expected != total) {
                return Err(Error::Sync(
                    "JMAP CalendarEvent query total changed while paging".into(),
                ));
            }
            expected_total.get_or_insert(total);
            let page_ids = body
                .get("ids")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| Error::Sync("JMAP CalendarEvent/query omitted ids".into()))?;
            if page_ids.len() > CALENDAR_EVENT_QUERY_PAGE_SIZE {
                return Err(Error::Sync(
                    "JMAP CalendarEvent/query exceeded the requested page limit".into(),
                ));
            }
            if page_ids.is_empty() && ids.len() < total {
                return Err(Error::Sync(
                    "JMAP CalendarEvent/query returned a nonadvancing page".into(),
                ));
            }
            for id in page_ids {
                let id = id
                    .as_str()
                    .filter(|id| valid_identifier(id))
                    .ok_or_else(|| {
                        Error::Sync("JMAP CalendarEvent/query returned a malformed id".into())
                    })?;
                if !seen_ids.insert(id.to_string()) {
                    return Err(Error::Sync(
                        "JMAP CalendarEvent/query repeated an event id".into(),
                    ));
                }
                ids.push(id.to_string());
            }
            if ids.len() > total {
                return Err(Error::Sync(
                    "JMAP CalendarEvent/query returned more ids than total".into(),
                ));
            }
            if ids.len() == total {
                break;
            }
            if page + 1 == CALENDAR_EVENT_MAX_PAGES {
                return Err(Error::Sync(
                    "JMAP CalendarEvent/query exceeded the page limit".into(),
                ));
            }
        }
        let query_state = query_state.ok_or_else(|| {
            Error::Sync("JMAP CalendarEvent/query produced no snapshot state".into())
        })?;

        let advertised_get_limit = if self.max_objects_in_get == 0 {
            CALENDAR_EVENT_MAX_GET_CHUNK
        } else {
            self.max_objects_in_get
        };
        let get_chunk_size = advertised_get_limit.min(CALENDAR_EVENT_MAX_GET_CHUNK);
        let mut events_json = Vec::with_capacity(ids.len());
        let mut returned_ids = HashSet::new();
        let mut get_state = None;
        for chunk in ids.chunks(get_chunk_size) {
            let request = calendar_events_get_request(&self.account_id, chunk);
            let response = self.api_request(&request, config).await?;
            let body = method_response(&response, "CalendarEvent/get", "g1")?;
            if body.get("accountId").and_then(serde_json::Value::as_str)
                != Some(self.account_id.as_str())
            {
                return Err(Error::Sync(
                    "JMAP CalendarEvent/get returned the wrong accountId".into(),
                ));
            }
            let state = body
                .get("state")
                .and_then(serde_json::Value::as_str)
                .filter(|state| !state.is_empty())
                .ok_or_else(|| Error::Sync("JMAP CalendarEvent/get omitted state".into()))?;
            if get_state
                .as_deref()
                .is_some_and(|expected| expected != state)
            {
                return Err(Error::Sync(
                    "JMAP CalendarEvent state changed while reading the snapshot".into(),
                ));
            }
            get_state.get_or_insert_with(|| state.to_string());
            let not_found = body
                .get("notFound")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| Error::Sync("JMAP CalendarEvent/get omitted notFound".into()))?;
            if !not_found.is_empty() {
                return Err(Error::Sync(
                    "JMAP CalendarEvent/get reported missing queried objects".into(),
                ));
            }
            let list = body
                .get("list")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| Error::Sync("JMAP CalendarEvent/get omitted list".into()))?;
            if list.len() != chunk.len() {
                return Err(Error::Sync(
                    "JMAP CalendarEvent/get omitted queried objects".into(),
                ));
            }
            let expected: HashSet<&str> = chunk.iter().map(String::as_str).collect();
            for event in list {
                let id = validate_event_object(event)?;
                if !expected.contains(id.as_str()) || !returned_ids.insert(id) {
                    return Err(Error::Sync(
                        "JMAP CalendarEvent/get returned duplicate or unexpected objects".into(),
                    ));
                }
                events_json.push(event.clone());
            }
        }
        if returned_ids.len() != ids.len() {
            return Err(Error::Sync(
                "JMAP CalendarEvent/get did not complete the queried snapshot".into(),
            ));
        }
        let recheck_request = calendar_event_query_request(&self.account_id, 0, 0, "q2");
        let recheck_response = self.api_request(&recheck_request, config).await?;
        let recheck = method_response(&recheck_response, "CalendarEvent/query", "q2")?;
        if recheck.get("accountId").and_then(serde_json::Value::as_str)
            != Some(self.account_id.as_str())
            || recheck
                .get("queryState")
                .and_then(serde_json::Value::as_str)
                != Some(query_state.as_str())
            || recheck.get("position").and_then(serde_json::Value::as_u64) != Some(0)
            || recheck.get("total").and_then(serde_json::Value::as_u64)
                != expected_total.map(|total| total as u64)
            || !recheck
                .get("ids")
                .and_then(serde_json::Value::as_array)
                .is_some_and(Vec::is_empty)
        {
            return Err(Error::Sync(
                "JMAP CalendarEvent query changed before snapshot completion".into(),
            ));
        }
        let response_state = get_state.as_deref();

        let mut events = Vec::new();
        let mut ambiguous_calendars = HashSet::new();
        let mut ambiguous_events = HashSet::new();
        for ev in events_json {
            let memberships = event_calendar_ids(&ev)?;
            if memberships.len() != 1 {
                ambiguous_calendars.extend(memberships);
                ambiguous_events.insert(ev["id"].as_str().expect("validated id").to_string());
                continue;
            }
            let recurrence_kind = classify_recurrence(&ev);
            let native_data = serde_json::to_string(&ev)
                .map_err(|error| Error::Other(format!("Invalid CalendarEvent JSON: {error}")))?;
            let recurrence_seeds =
                recurrence_seeds(&ev, recurrence_kind, &native_data, response_state);
            let id = ev["id"].as_str().expect("validated id").to_string();
            let title = ev["title"].as_str().expect("validated title").to_string();
            let description = ev["description"].as_str().map(|s| s.to_string());
            let uid = ev["uid"].as_str().map(|s| s.to_string());

            let cal_id = memberships[0].clone();

            // Location: JSCalendar uses "locations" as a map { id: { name: "..." } }
            let location = ev["locations"]
                .as_object()
                .and_then(|m| m.values().next())
                .and_then(|loc| loc["name"].as_str())
                .map(|s| s.to_string());

            // Start datetime — JSCalendar uses "start" as local time + "timeZone" as IANA id.
            let raw_start = ev["start"].as_str().unwrap_or("").to_string();
            let event_tz = ev["timeZone"].as_str().unwrap_or("").to_string();
            let start = if raw_start.is_empty() {
                raw_start.clone()
            } else {
                crate::calendar::timezone::to_utc(&raw_start, &event_tz)
            };

            let all_day = ev["showWithoutTime"].as_bool().unwrap_or(false);

            let duration_str = ev["duration"].as_str().unwrap_or("PT1H");
            let end = {
                let e = compute_end_from_duration(start.trim_end_matches('Z'), duration_str);
                if start.ends_with('Z') && !e.ends_with('Z') {
                    format!("{}Z", e)
                } else {
                    e
                }
            };

            let event_tz_opt = if event_tz.is_empty() {
                None
            } else {
                Some(event_tz.clone())
            };

            // Recurrence: JSCalendar carries an array of RecurrenceRule
            // objects — convert to the app's canonical iCal RRULE string
            // so the local DB, the frontend expander and the other
            // backends all agree on one format.
            let recurrence_rules = ev["recurrenceRules"]
                .as_array()
                .filter(|rules| !rules.is_empty())
                .map(Vec::as_slice)
                .or_else(|| {
                    ev.get("recurrenceRule")
                        .filter(|rule| rule.is_object())
                        .map(std::slice::from_ref)
                });
            let recurrence_rule = recurrence_rules
                .and_then(|rules| {
                    crate::calendar::recurrence::jscalendar_to_rrule(rules, Some(&event_tz))
                        .or_else(|| {
                            log::warn!(
                                "JMAP event {} has a recurrence rule not supported by the local expander",
                                id
                            );
                            serde_json::to_string(rules).ok()
                        })
                });

            // Participants: supports both JSCalendar-bis (calendarAddress) and old format (sendTo.imip)
            let mut organizer_email = None;
            let mut attendees: Vec<serde_json::Value> = Vec::new();
            if let Some(participants) = ev["participants"].as_object() {
                for (_pid, p) in participants {
                    // Try calendarAddress (JSCalendar-bis), then sendTo.imip (old), then email
                    let email = p["calendarAddress"]
                        .as_str()
                        .map(|s| s.trim_start_matches("mailto:").to_string())
                        .or_else(|| {
                            p["sendTo"]
                                .as_object()
                                .and_then(|s| s.get("imip"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.trim_start_matches("mailto:").to_string())
                        })
                        .or_else(|| p["email"].as_str().map(|s| s.to_string()));
                    let name = p["name"].as_str().map(|s| s.to_string());
                    let mut status = p["participationStatus"]
                        .as_str()
                        .unwrap_or("needs-action")
                        .to_string();
                    let roles = p["roles"].as_object();
                    let is_owner = roles.map(|r| r.contains_key("owner")).unwrap_or(false);

                    if is_owner {
                        organizer_email = email.clone();
                        // Organizer is implicitly "accepted" — they created the event
                        if status == "needs-action" {
                            status = "accepted".to_string();
                        }
                    }
                    if let Some(ref em) = email {
                        attendees.push(serde_json::json!({
                            "email": em,
                            "name": name,
                            "status": status,
                        }));
                    }
                }
            }
            let attendees_json = if attendees.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&attendees).unwrap_or_default())
            };

            log::debug!(
                "  event: {} ({}) start={} end={} attendees={}",
                title,
                id,
                start,
                end,
                attendees.len()
            );
            events.push(JmapCalendarEvent {
                id,
                calendar_id: cal_id,
                title,
                description,
                location,
                start,
                end,
                all_day,
                timezone: event_tz_opt,
                recurrence_rule,
                recurrence_kind,
                recurrence_rule_is_complete: false,
                uid,
                organizer_email,
                attendees_json,
                native_json: Some(ev),
                response_state: response_state.map(str::to_string),
                recurrence_seeds,
            });
        }

        log::info!("JMAP fetched {} unambiguous calendar events", events.len());
        Ok((events, ambiguous_calendars, ambiguous_events))
    }

    /// Create a calendar event on the server via CalendarEvent/set.
    /// Returns the server-assigned event ID.
    pub async fn create_calendar_event(
        &self,
        config: &JmapConfig,
        event: &JmapCalendarEvent,
    ) -> Result<String> {
        let recurrence_rules = event.creation_recurrence_rules()?;
        log::info!(
            "JMAP creating calendar event: '{}' organizer={:?} attendees={:?}",
            event.title,
            event.organizer_email,
            event.attendees_json
        );

        let uid = event
            .uid
            .clone()
            .unwrap_or_else(|| format!("{}@chithi", uuid::Uuid::new_v4()));

        let duration = compute_duration(&event.start, &event.end);

        let mut event_obj = serde_json::json!({
            "@type": "Event",
            "calendarIds": { &event.calendar_id: true },
            "title": event.title,
            "start": event.start,
            "duration": duration,
            "showWithoutTime": event.all_day,
            "uid": uid,
        });

        if let Some(ref desc) = event.description {
            event_obj["description"] = serde_json::json!(desc);
        }
        if let Some(ref loc) = event.location {
            event_obj["locations"] = serde_json::json!({
                "loc1": { "@type": "Location", "name": loc }
            });
        }
        if let Some(rules) = recurrence_rules {
            event_obj["recurrenceRules"] = rules;
        }

        // Add participants (organizer + attendees)
        // Uses JSCalendar-bis format (draft-ietf-calext-jscalendarbis-14):
        // - "calendarAddress" instead of "sendTo"
        // - No "replyTo" on the event
        let mut participants = serde_json::Map::new();
        if let Some(ref org_email) = event.organizer_email {
            if !org_email.is_empty() {
                participants.insert(
                    "organizer".to_string(),
                    serde_json::json!({
                        "@type": "Participant",
                        "calendarAddress": format!("mailto:{}", org_email),
                        "roles": {"owner": true, "attendee": true},
                        "participationStatus": "accepted",
                        "expectReply": false,
                    }),
                );
            }
        }
        if let Some(ref att_json) = event.attendees_json {
            if let Ok(attendees) = serde_json::from_str::<Vec<serde_json::Value>>(att_json) {
                for (i, att) in attendees.iter().enumerate() {
                    let email = att["email"].as_str().unwrap_or_default();
                    if !email.is_empty() {
                        let status = att["status"].as_str().unwrap_or("needs-action");
                        participants.insert(
                            format!("att{}", i),
                            serde_json::json!({
                                "@type": "Participant",
                                "calendarAddress": format!("mailto:{}", email),
                                "roles": {"attendee": true},
                                "participationStatus": status,
                                "expectReply": true,
                            }),
                        );
                    }
                }
            }
        }
        if !participants.is_empty() {
            event_obj["participants"] = serde_json::Value::Object(participants);
        }

        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["CalendarEvent/set", {
                    "accountId": self.account_id,
                    "create": {
                        "new1": event_obj
                    }
                }, "s1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        // Check for creation errors
        if let Some(err) = resp["methodResponses"][0][1]["notCreated"]["new1"].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP create calendar event failed: {}",
                desc
            )));
        }

        let created_id = resp["methodResponses"][0][1]["created"]["new1"]["id"]
            .as_str()
            .ok_or_else(|| Error::Other("No id in CalendarEvent/set create response".into()))?
            .to_string();

        log::info!("JMAP created calendar event id={}", created_id);
        Ok(created_id)
    }

    /// Apply a caller-built JSCalendar patch through `CalendarEvent/set`.
    pub async fn update_calendar_event(
        &self,
        config: &JmapConfig,
        event_id: &str,
        patch: &serde_json::Value,
    ) -> Result<()> {
        let mut update = serde_json::Map::new();
        update.insert(event_id.to_string(), patch.clone());
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:calendars"],
            "methodCalls": [["CalendarEvent/set", {
                "accountId": self.account_id,
                "sendSchedulingMessages": false,
                "update": update
            }, "u1"]]
        });
        let response = self.api_request(&request, config).await?;
        if let Some(error) = response["methodResponses"][0][1]["notUpdated"][event_id].as_object() {
            let description = error
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP update calendar event failed: {description}"
            )));
        }
        Ok(())
    }

    /// Atomically update an event at a known state and read its complete
    /// canonical JSCalendar object after the successful write.
    pub(crate) async fn update_calendar_occurrence(
        &self,
        config: &JmapConfig,
        event_id: &str,
        expected_state: &str,
        patch: &serde_json::Value,
    ) -> Result<(String, serde_json::Value, Vec<RecurrenceIdentitySeed>)> {
        let request = occurrence_update_request(&self.account_id, event_id, expected_state, patch);
        let response = self.api_request(&request, config).await?;
        let new_state = occurrence_update_state(&response, event_id)?;

        let get = calendar_event_get_request(&self.account_id, event_id);
        let response = self.api_request(&get, config).await?;
        let method = &response["methodResponses"][0];
        if method[0].as_str() != Some("CalendarEvent/get") {
            return Err(Error::Sync(
                "Canonical JMAP CalendarEvent/get failed; reconciliation required".into(),
            ));
        }
        if method[1]["state"].as_str() != Some(new_state.as_str()) {
            return Err(Error::Sync(
                "JMAP CalendarEvent state changed before canonical read; reconciliation required"
                    .into(),
            ));
        }
        let list = method[1]["list"].as_array().ok_or_else(|| {
            Error::Sync(
                "Canonical JMAP CalendarEvent/get omitted its list; reconciliation required".into(),
            )
        })?;
        if list.len() != 1 || list[0]["id"].as_str() != Some(event_id) {
            return Err(Error::Sync(
                "Canonical JMAP event identity changed; reconciliation required".into(),
            ));
        }
        let native_event = list[0].clone();
        let native = serde_json::to_string(&native_event).map_err(|error| {
            Error::Sync(format!(
                "Canonical JMAP event could not be stored; reconciliation required: {error}"
            ))
        })?;
        let kind = classify_recurrence(&native_event);
        let recurrence_seeds = recurrence_seeds(&native_event, kind, &native, Some(&new_state))
            .ok_or_else(|| {
                Error::Sync(
                    "Canonical JMAP recurrence data is incomplete; reconciliation required".into(),
                )
            })?;
        Ok((new_state, native_event, recurrence_seeds))
    }

    /// Update a participant's status on a calendar event via JMAP patch.
    /// Uses the JSCalendar-bis path syntax: participants/<id>/participationStatus
    pub async fn update_participant_status(
        &self,
        config: &JmapConfig,
        event_id: &str,
        participant_key: &str,
        status: &str,
    ) -> Result<()> {
        log::info!(
            "JMAP updating participant {} status to {} on event {}",
            participant_key,
            status,
            event_id
        );

        let patch_key = format!("participants/{}/participationStatus", participant_key);
        let mut patch = serde_json::Map::new();
        patch.insert(patch_key, serde_json::json!(status));

        let mut update = serde_json::Map::new();
        update.insert(event_id.to_string(), serde_json::Value::Object(patch));

        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:calendars"],
            "methodCalls": [
                ["CalendarEvent/set", {
                    "accountId": self.account_id,
                    "update": update
                }, "u1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notUpdated"][event_id].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP update participant failed: {}",
                desc
            )));
        }

        log::info!("JMAP updated participant status on event {}", event_id);
        Ok(())
    }

    /// Delete a calendar event on the server via CalendarEvent/set.
    pub async fn delete_calendar_event(&self, config: &JmapConfig, event_id: &str) -> Result<()> {
        log::info!("JMAP deleting calendar event: id={}", event_id);

        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["CalendarEvent/set", {
                    "accountId": self.account_id,
                    "destroy": [event_id]
                }, "d1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notDestroyed"][event_id].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP delete calendar event failed: {}",
                desc
            )));
        }

        log::info!("JMAP deleted calendar event id={}", event_id);
        Ok(())
    }
}

/// Compute end datetime from a start datetime and an ISO 8601 duration string.
/// Handles simple cases like PT1H, PT30M, P1D, PT1H30M, etc.
/// Falls back to start + 1 hour if parsing fails.
fn compute_end_from_duration(start: &str, duration: &str) -> String {
    use chrono::{Duration, NaiveDate, NaiveDateTime};

    let total_seconds = parse_iso8601_duration_seconds(duration);

    // Try parsing as full datetime first, then as date-only
    if let Ok(dt) = NaiveDateTime::parse_from_str(start, "%Y-%m-%dT%H:%M:%S") {
        let end = dt + Duration::seconds(total_seconds);
        return end.format("%Y-%m-%dT%H:%M:%S").to_string();
    }
    if let Ok(d) = NaiveDate::parse_from_str(start, "%Y-%m-%d") {
        let dt = d.and_hms_opt(0, 0, 0).unwrap();
        let end = dt + Duration::seconds(total_seconds);
        if total_seconds % 86400 == 0 {
            return end.format("%Y-%m-%d").to_string();
        }
        return end.format("%Y-%m-%dT%H:%M:%S").to_string();
    }
    // Fallback: return start as-is
    start.to_string()
}

/// Compute an ISO 8601 duration string from start and end datetimes.
/// Returns "P1D" for full-day spans, "PT{n}H" / "PT{n}M" for shorter spans.
fn compute_duration(start: &str, end: &str) -> String {
    use chrono::{NaiveDate, NaiveDateTime};

    let parse_datetime = |value: &str| {
        chrono::DateTime::parse_from_rfc3339(value)
            .map(|value| value.naive_utc())
            .or_else(|_| {
                NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y-%m-%dT%H:%M:%S")
            })
            .or_else(|_| {
                NaiveDate::parse_from_str(value, "%Y-%m-%d")
                    .map(|value| value.and_hms_opt(0, 0, 0).expect("midnight is valid"))
            })
    };
    let start_dt = parse_datetime(start);
    let end_dt = parse_datetime(end);

    if let (Ok(s), Ok(e)) = (start_dt, end_dt) {
        let diff = e - s;
        let total_secs = diff.num_seconds();
        if total_secs <= 0 {
            return "PT1H".to_string();
        }
        let days = total_secs / 86400;
        let remaining = total_secs % 86400;
        let hours = remaining / 3600;
        let minutes = (remaining % 3600) / 60;
        let secs = remaining % 60;

        if remaining == 0 && days > 0 {
            return format!("P{}D", days);
        }
        let mut s = String::from("P");
        if days > 0 {
            s.push_str(&format!("{}D", days));
        }
        s.push('T');
        if hours > 0 {
            s.push_str(&format!("{}H", hours));
        }
        if minutes > 0 {
            s.push_str(&format!("{}M", minutes));
        }
        if secs > 0 {
            s.push_str(&format!("{}S", secs));
        }
        // Ensure we have at least something after 'T'
        if s.ends_with('T') {
            s.push_str("0S");
        }
        return s;
    }
    // Fallback
    "PT1H".to_string()
}

/// Parse a simple ISO 8601 duration like "P1D", "PT1H30M", "PT45M" into total seconds.
fn parse_iso8601_duration_seconds(dur: &str) -> i64 {
    let mut total: i64 = 0;
    let mut num_buf = String::new();
    let mut in_time = false;

    for ch in dur.chars() {
        match ch {
            'P' => {}
            'T' => {
                in_time = true;
            }
            '0'..='9' => {
                num_buf.push(ch);
            }
            'D' => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 86400;
                }
                num_buf.clear();
            }
            'H' if in_time => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 3600;
                }
                num_buf.clear();
            }
            'M' if in_time => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 60;
                }
                num_buf.clear();
            }
            'S' if in_time => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n;
                }
                num_buf.clear();
            }
            'W' => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 604800;
                }
                num_buf.clear();
            }
            _ => {
                num_buf.clear();
            }
        }
    }

    if total == 0 {
        3600
    } else {
        total
    } // default 1 hour
}

#[cfg(test)]
mod recurrence_tests {
    use super::{
        calendar_event_query_request, calendar_events_get_request, classify_recurrence,
        faithful_local_recurrence_rules, occurrence_update_patch, occurrence_update_request,
        occurrence_update_state, recurrence_seeds, JmapCalendarEvent,
    };
    use crate::calendar::recurrence_identity::{
        OccurrenceFields, RecurrenceObjectKind, RecurrenceValueType, UpdateOccurrenceInput,
    };
    use crate::calendar::RecurrenceKind;
    use serde_json::{json, Value};

    fn event() -> Value {
        json!({
            "@type": "Event",
            "id": "event-1",
            "uid": "uid-1",
            "calendarIds": {"calendar-1": true},
            "title": "Master title",
            "start": "2026-09-13T10:00:00",
            "duration": "PT1H"
        })
    }

    fn seeds(
        event: &Value,
        state: &str,
    ) -> Option<Vec<crate::calendar::recurrence_identity::RecurrenceIdentitySeed>> {
        let native = serde_json::to_string(event).unwrap();
        recurrence_seeds(event, classify_recurrence(event), &native, Some(state))
    }

    #[test]
    fn native_master_and_override_seeds_preserve_identity_state_and_complete_event() {
        let mut event = event();
        event["timeZone"] = json!("Europe/Stockholm");
        event["duration"] = json!("PT1H");
        event["description"] = json!("Master description");
        event["locations"] = json!({"master": {"name": "Master room"}});
        event["recurrenceRules"] = json!([{"frequency": "weekly"}]);
        event["recurrenceOverrides"] = json!({
            "2026-09-20T10:00:00": {
                "title": "Override title",
                "description": "Override description",
                "locations": {"override": {"name": "Override room"}},
                "start": "2026-09-20T12:00:00",
                "duration": "PT30M"
            },
            "2026-09-27T10:00:00": {"excluded": true},
            "2026-10-04T10:00:00": {}
        });

        let seeds = seeds(&event, "state-7").unwrap();
        assert_eq!(seeds.len(), 4);
        assert!(seeds
            .iter()
            .all(|seed| seed.provider_calendar_id.as_deref() == Some("calendar-1")));
        let master = &seeds[0];
        assert_eq!(master.kind, RecurrenceObjectKind::Master);
        assert_eq!(master.provider_calendar_id.as_deref(), Some("calendar-1"));
        assert_eq!(master.provider_series_id.as_deref(), Some("event-1"));
        assert_eq!(master.provider_revision.as_deref(), Some("state-7"));
        assert_eq!(
            serde_json::from_str::<Value>(master.provider_native_data.as_deref().unwrap()).unwrap(),
            event
        );

        let moved = &seeds[1];
        assert_eq!(moved.kind, RecurrenceObjectKind::Exception);
        assert_eq!(moved.provider_calendar_id.as_deref(), Some("calendar-1"));
        assert_eq!(moved.provider_occurrence_id, None);
        assert_eq!(moved.recurrence_id.as_deref(), Some("2026-09-20T10:00:00"));
        assert_eq!(
            moved.recurrence_timezone.as_deref(),
            Some("Europe/Stockholm")
        );
        assert_eq!(
            moved.recurrence_value_type,
            Some(RecurrenceValueType::DateTime)
        );
        assert_eq!(master.occurrence.title, "Master title");
        assert_eq!(
            master.occurrence.description.as_deref(),
            Some("Master description")
        );
        assert_eq!(master.occurrence.location.as_deref(), Some("Master room"));
        assert_eq!(moved.occurrence.title, "Override title");
        assert_eq!(
            moved.occurrence.description.as_deref(),
            Some("Override description")
        );
        assert_eq!(moved.occurrence.location.as_deref(), Some("Override room"));
        assert_eq!(moved.occurrence.start_time, "2026-09-20T10:00:00Z");
        assert_eq!(moved.occurrence.end_time, "2026-09-20T10:30:00Z");
        assert_eq!(
            moved.occurrence.timezone.as_deref(),
            Some("Europe/Stockholm")
        );
        assert_eq!(
            serde_json::from_str::<Value>(moved.provider_native_data.as_deref().unwrap()).unwrap(),
            event
        );
        assert!(seeds.iter().all(|seed| {
            serde_json::from_str::<Value>(seed.provider_native_data.as_deref().unwrap()).unwrap()
                == event
        }));
        assert_eq!(seeds[2].kind, RecurrenceObjectKind::Exclusion);
        assert_eq!(seeds[2].occurrence.start_time, "2026-09-27T08:00:00Z");
        assert_eq!(seeds[3].kind, RecurrenceObjectKind::Occurrence);
        assert_eq!(seeds[3].occurrence.start_time, "2026-10-04T08:00:00Z");
    }

    #[test]
    fn occurrence_patch_escapes_identity_and_excludes_immutable_and_scheduling_fields() {
        let fields = OccurrenceFields {
            title: "Changed".into(),
            description: Some(String::new()),
            location: Some(String::new()),
            start_time: "2026-09-20T10:00:00Z".into(),
            end_time: "2026-09-20T11:30:00Z".into(),
            all_day: false,
            timezone: Some("Europe/Stockholm".into()),
        };
        let changed = UpdateOccurrenceInput {
            title: Some(fields.title.clone()),
            description: Some(String::new()),
            location: Some(String::new()),
            start_time: Some(fields.start_time.clone()),
            end_time: Some(fields.end_time.clone()),
            all_day: Some(fields.all_day),
            timezone: Some(fields.timezone.clone().unwrap()),
        };
        let patch = occurrence_update_patch(Some("id/with~marker"), &changed, &fields);
        let object = patch.as_object().unwrap();
        let prefix = "recurrenceOverrides/id~1with~0marker/";
        assert_eq!(object[&format!("{prefix}title")], "Changed");
        assert_eq!(object[&format!("{prefix}description")], "");
        assert_eq!(object[&format!("{prefix}locations")], json!({}));
        assert_eq!(object[&format!("{prefix}start")], "2026-09-20T12:00:00");
        assert_eq!(object[&format!("{prefix}duration")], "PT1H30M");
        assert!(object.keys().all(|key| {
            !key.contains("participants")
                && !key.contains("recurrenceRules")
                && !key.contains("excludedRecurrenceRules")
                && !key.ends_with("uid")
                && !key.contains("relatedTo")
                && !key.contains("baseEventId")
                && !key.contains("recurrenceIdTimeZone")
        }));

        let detached = occurrence_update_patch(None, &changed, &fields);
        assert!(detached["description"].is_null());
        assert_eq!(detached["locations"], json!({}));
    }

    #[test]
    fn sparse_occurrence_patch_preserves_unmodified_native_fields() {
        let desired = OccurrenceFields {
            title: "Changed".into(),
            description: Some("Preserved description".into()),
            location: Some("Flattened projection must not replace native locations".into()),
            start_time: "2026-09-20T08:00:00Z".into(),
            end_time: "2026-09-20T09:00:00Z".into(),
            all_day: false,
            timezone: Some("Europe/Stockholm".into()),
        };
        let changed = UpdateOccurrenceInput {
            title: Some("Changed".into()),
            ..UpdateOccurrenceInput::default()
        };
        assert_eq!(
            occurrence_update_patch(Some("occurrence"), &changed, &desired),
            json!({"recurrenceOverrides/occurrence/title": "Changed"})
        );

        let changed = UpdateOccurrenceInput {
            end_time: Some(desired.end_time.clone()),
            ..UpdateOccurrenceInput::default()
        };
        assert_eq!(
            occurrence_update_patch(None, &changed, &desired),
            json!({"duration": "PT1H"})
        );
    }

    #[test]
    fn occurrence_set_uses_expected_state_and_requires_new_state() {
        let request = occurrence_update_request(
            "account",
            "event",
            "expected-state",
            &json!({"title": "Changed"}),
        );
        let set = &request["methodCalls"][0][1];
        assert_eq!(set["ifInState"], "expected-state");
        assert_eq!(set["sendSchedulingMessages"], false);
        assert_eq!(set["update"]["event"]["title"], "Changed");

        let response = json!({"methodResponses": [["CalendarEvent/set", {
            "updated": {"event": null}, "newState": "new-state"
        }, "u1"]]});
        assert_eq!(
            occurrence_update_state(&response, "event").unwrap(),
            "new-state"
        );
        let missing = json!({"methodResponses": [["CalendarEvent/set", {
            "updated": {"event": null}
        }, "u1"]]});
        assert!(matches!(
            occurrence_update_state(&missing, "event"),
            Err(crate::error::Error::Sync(_))
        ));
    }

    #[test]
    fn occurrence_set_state_mismatch_requires_reconciliation() {
        for response in [
            json!({"methodResponses": [["error", {
                "type": "stateMismatch"
            }, "u1"]]}),
            json!({"methodResponses": [["CalendarEvent/set", {
                "notUpdated": {"event": {"type": "stateMismatch"}}
            }, "u1"]]}),
        ] {
            let error = occurrence_update_state(&response, "event").unwrap_err();
            assert!(matches!(error, crate::error::Error::Sync(_)));
            assert!(error.to_string().contains("reconciliation"));
        }
    }

    #[test]
    fn detached_seed_uses_immutable_provider_identity_and_native_semantics() {
        let mut event = event();
        event["id"] = json!("projected-9");
        event["baseEventId"] = json!("series-3");
        event["recurrenceId"] = json!("2026-09-20T10:00:00");
        event["recurrenceIdTimeZone"] = json!("Europe/Stockholm");
        event["timeZone"] = json!("Europe/Stockholm");
        event["start"] = json!("2026-09-20T11:00:00");
        event["duration"] = json!("PT1H");

        let detached_seeds = seeds(&event, "state-8").unwrap();
        assert_eq!(detached_seeds.len(), 1);
        let seed = &detached_seeds[0];
        assert_eq!(seed.kind, RecurrenceObjectKind::Exception);
        assert_eq!(seed.provider_calendar_id.as_deref(), Some("calendar-1"));
        assert_eq!(seed.provider_series_id.as_deref(), Some("series-3"));
        assert_eq!(seed.provider_occurrence_id.as_deref(), Some("projected-9"));
        assert_eq!(seed.recurrence_id.as_deref(), Some("2026-09-20T10:00:00"));
        assert_eq!(seed.provider_revision.as_deref(), Some("state-8"));
        assert_eq!(
            serde_json::from_str::<Value>(seed.provider_native_data.as_deref().unwrap()).unwrap(),
            event
        );

        event["start"] = event["recurrenceId"].clone();
        assert_eq!(
            seeds(&event, "state-9").unwrap()[0].kind,
            RecurrenceObjectKind::Occurrence
        );
        event["excluded"] = json!(true);
        assert_eq!(
            seeds(&event, "state-10").unwrap()[0].kind,
            RecurrenceObjectKind::Exclusion
        );
    }

    #[test]
    fn malformed_ranges_fail_closed_and_positive_standalone_clears() {
        let standalone = event();
        assert_eq!(seeds(&standalone, "state").unwrap(), Vec::new());

        let mut malformed = event();
        malformed["recurrenceRules"] = json!([{"frequency": "weekly"}]);
        malformed["duration"] = json!("not-a-duration");
        assert_eq!(classify_recurrence(&malformed), RecurrenceKind::Series);
        assert!(seeds(&malformed, "state").is_none());

        malformed["duration"] = json!("P9223372036854775807D");
        assert!(seeds(&malformed, "state").is_none());

        malformed["duration"] = json!("PT1H");
        malformed["recurrenceOverrides"] = json!({
            "2026-09-20T10:00:00": {"timeZone": "Not/AZone"}
        });
        assert!(seeds(&malformed, "state").is_none());
    }

    #[test]
    fn recurrence_seeds_require_exactly_one_true_calendar_membership() {
        let mut recurring = event();
        recurring["recurrenceRules"] = json!([{"frequency": "weekly"}]);

        recurring["calendarIds"] = json!({
            "calendar-1": true,
            "calendar-2": true
        });
        assert!(seeds(&recurring, "state").is_none());

        recurring["calendarIds"] = json!({
            "calendar-1": true,
            "not-a-membership": false
        });
        let authoritative = seeds(&recurring, "state").unwrap();
        assert!(authoritative
            .iter()
            .all(|seed| { seed.provider_calendar_id.as_deref() == Some("calendar-1") }));

        let mut standalone = event();
        standalone["calendarIds"] = json!({
            "calendar-1": true,
            "calendar-2": true
        });
        assert!(seeds(&standalone, "state").is_none());

        standalone["calendarIds"] = json!({" ": true});
        assert!(seeds(&standalone, "state").is_none());
    }

    #[test]
    fn creation_never_drops_malformed_or_unrepresentable_rrule_parts() {
        for rule in [
            "",
            "FREQ=HOURLY",
            "FREQ=WEEKLY;BYSETPOS=1",
            "FREQ=WEEKLY;COUNT=bad",
            "FREQ=WEEKLY;COUNT=0",
            "FREQ=WEEKLY;INTERVAL=bad",
            "FREQ=WEEKLY;INTERVAL=0",
            "FREQ=WEEKLY;UNTIL=bad",
            "FREQ=WEEKLY;UNTIL=20260230",
            "FREQ=WEEKLY;FREQ=DAILY",
            "FREQ=WEEKLY;COUNT=2;UNTIL=20261001",
            "[{\"frequency\":\"weekly\"}]",
        ] {
            assert!(
                faithful_local_recurrence_rules(rule, None).is_none(),
                "{rule}"
            );
        }
        for rule in [
            "FREQ=WEEKLY;COUNT=4",
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO",
            "FREQ=DAILY;UNTIL=20261231",
        ] {
            assert!(
                faithful_local_recurrence_rules(rule, None).is_some(),
                "{rule}"
            );
        }
    }

    #[test]
    fn serialization_cannot_invent_proof_of_complete_local_series_recurrence() {
        let mut local = crate::backend::testutil::event();
        local.recurrence_kind = RecurrenceKind::Series;
        local.recurrence_rule = Some("FREQ=WEEKLY;COUNT=4".into());
        let wire = JmapCalendarEvent::for_local_creation(&local, "calendar").unwrap();
        let restored: JmapCalendarEvent =
            serde_json::from_value(serde_json::to_value(wire).unwrap()).unwrap();
        assert_eq!(restored.recurrence_kind, RecurrenceKind::Series);
        assert!(restored.creation_recurrence_rules().is_err());
    }

    #[test]
    fn personal_copy_patch_is_authoritative_and_has_no_participants() {
        let mut local = crate::backend::testutil::event();
        local.title = "Updated".into();
        local.description = None;
        local.location = None;
        local.start_time = "2026-09-14T07:00:00Z".into();
        local.end_time = "2026-09-14T08:30:00Z".into();
        local.timezone = Some("Europe/Stockholm".into());
        local.recurrence_kind = RecurrenceKind::Series;
        local.recurrence_rule = Some("FREQ=WEEKLY;COUNT=3".into());
        local.ical_data = Some(
            "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:u\r\n\
             DTSTART:20260914T070000Z\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n"
                .into(),
        );
        local.source_message_id = Some("message".into());
        local.organizer_email = Some("owner@example.test".into());
        local.attendees_json = Some("[]".into());

        let patch = JmapCalendarEvent::personal_copy_update_patch(&local).unwrap();
        assert_eq!(patch["title"], "Updated");
        assert!(patch["description"].is_null());
        assert_eq!(patch["locations"], json!({}));
        assert_eq!(patch["participants"], json!({}));
        assert_eq!(patch["start"], "2026-09-14T09:00:00");
        assert_eq!(patch["duration"], "PT1H30M");
        assert_eq!(patch["timeZone"], "Europe/Stockholm");
        assert_eq!(patch["recurrenceRules"][0]["frequency"], "weekly");
        assert_eq!(patch["recurrenceRules"][0]["count"], 3);
        assert!(patch["recurrenceOverrides"].is_null());

        local.recurrence_kind = RecurrenceKind::Standalone;
        local.recurrence_rule = None;
        local.ical_data = None;
        let cleared = JmapCalendarEvent::personal_copy_update_patch(&local).unwrap();
        assert!(cleared["recurrenceRules"].is_null());
    }

    #[test]
    fn get_requests_native_events_without_a_version_specific_property_list() {
        let query = calendar_event_query_request("account-1", 500, 500, "q1");
        assert_eq!(query["methodCalls"][0][0], "CalendarEvent/query");
        assert_eq!(query["methodCalls"][0][1]["position"], 500);
        assert_eq!(query["methodCalls"][0][1]["limit"], 500);
        assert!(query["methodCalls"][0][1]
            .get("expandRecurrences")
            .is_none());
        let request = calendar_events_get_request("account-1", &["event-1".into()]);
        let get = &request["methodCalls"][0][1];
        assert_eq!(get["accountId"], "account-1");
        assert_eq!(get["ids"], json!(["event-1"]));
        assert!(get.get("recurrenceOverridesBefore").is_none());
        assert!(get.get("recurrenceOverridesAfter").is_none());
        assert!(get.get("properties").is_none());
        assert!(get.get("reduceParticipants").is_none());
    }

    #[test]
    fn standalone_accepts_omitted_null_and_empty_optional_recurrence_metadata() {
        let mut event = event();
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Standalone);
        for name in [
            "recurrenceId",
            "recurrenceIdTimeZone",
            "recurrenceRules",
            "recurrenceRule",
            "excludedRecurrenceRules",
            "recurrenceOverrides",
            "baseEventId",
            "relatedTo",
        ] {
            event[name] = Value::Null;
        }
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Standalone);
        event["recurrenceRules"] = json!([]);
        event["excludedRecurrenceRules"] = json!([]);
        event["recurrenceOverrides"] = json!({});
        event["excluded"] = json!(false);
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Standalone);
    }

    #[test]
    fn series_includes_overrides_exclusions_unsupported_rules_and_bis_rule() {
        for (name, value) in [
            (
                "recurrenceRules",
                json!([{"@type": "RecurrenceRule", "frequency": "weekly"}]),
            ),
            (
                "recurrenceRules",
                json!([{"frequency": "hourly"}, {"frequency": "yearly"}]),
            ),
            ("recurrenceRule", json!({"frequency": "daily"})),
            ("excludedRecurrenceRules", json!([{"frequency": "monthly"}])),
            ("recurrenceOverrides", json!({"2026-09-20T10:00:00": {}})),
            (
                "recurrenceOverrides",
                json!({"2026-09-20T10:00:00": {"excluded": true}}),
            ),
            (
                "relatedTo",
                json!({"previous": {"relation": {"first": true}}}),
            ),
        ] {
            let mut event = event();
            event[name] = value;
            assert_eq!(
                classify_recurrence(&event),
                RecurrenceKind::Series,
                "{event}"
            );
        }
    }

    #[test]
    fn detached_and_server_expanded_instances_are_occurrences() {
        let mut event = event();
        event["recurrenceId"] = json!("2026-09-20T10:00:00");
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Occurrence);
        event["recurrenceId"] = json!("2026-09-20T10:00:00.123");
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Occurrence);
        event["recurrenceIdTimeZone"] = json!("Europe/Stockholm");
        event["baseEventId"] = json!("base-1");
        event["recurrenceRules"] = Value::Null;
        event["recurrenceRule"] = Value::Null;
        event["recurrenceOverrides"] = Value::Null;
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Occurrence);
        event["recurrenceId"] = json!("2026-09-20T00:00:00");
        event["showWithoutTime"] = json!(true);
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Occurrence);
        event["recurrenceRules"] = json!([{"frequency": "weekly"}]);
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Unknown);
    }

    #[test]
    fn malformed_or_ambiguous_provider_metadata_cannot_become_standalone() {
        for (name, value) in [
            ("@type", json!("Task")),
            ("@type", json!("VendorEvent")),
            ("@type", Value::Null),
            ("recurrenceId", json!(true)),
            ("recurrenceId", json!("")),
            ("recurrenceId", json!("2026-02-30T10:00:00")),
            ("recurrenceId", json!("2026-09-20T10:00:00Z")),
            ("recurrenceId", json!("2026-09-20T10:00:00.000")),
            ("recurrenceId", json!("2026-09-20")),
            ("recurrenceIdTimeZone", json!("UTC")),
            ("baseEventId", json!("base-1")),
            ("baseEventId", json!(false)),
            ("recurrenceRules", json!({})),
            ("recurrenceRules", json!([null])),
            (
                "recurrenceRules",
                json!([{"@type": "Unknown", "frequency": "weekly"}]),
            ),
            ("recurrenceRules", json!([{"frequency": false}])),
            ("recurrenceRule", json!([])),
            ("recurrenceRule", json!({})),
            ("excludedRecurrenceRules", json!(false)),
            ("recurrenceOverrides", json!([])),
            ("recurrenceOverrides", json!({"invalid": {}})),
            ("recurrenceOverrides", json!({"2026-09-20T10:00:00": null})),
            (
                "recurrenceOverrides",
                json!({"2026-09-20T10:00:00": {"@type": "Task"}}),
            ),
            ("excluded", json!(true)),
            ("excluded", json!("false")),
            ("relatedTo", json!([])),
            ("id", json!("")),
            ("uid", Value::Null),
            ("start", json!(42)),
            ("calendarIds", json!({"calendar-1": false})),
            ("showWithoutTime", json!("false")),
            ("locations", json!({"location-1": {"@type": "Unknown"}})),
            ("participants", json!({"participant-1": false})),
        ] {
            let mut event = event();
            event[name] = value;
            assert_eq!(
                classify_recurrence(&event),
                RecurrenceKind::Unknown,
                "{event}"
            );
        }
    }
}
