//! Complete Graph snapshots and conditional mutations. IDs are opaque; only
//! `originalStart` and the documented MAPI recurrence property identify slots.

mod recurrence_blob;
#[cfg(test)]
pub(crate) mod tests;
mod writes;

use std::collections::{BTreeMap, HashSet};

use chrono::{
    DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Timelike, Utc,
};
use reqwest::Method;
use serde_json::{json, Value};

use super::{GraphClient, CALENDAR_EVENT_SELECT};
use crate::calendar::event_set::{CalendarEventSet, CalendarOverride, NativeCalendarResource};
use crate::calendar::{simple_recurrence, CalendarEvent, RecurrenceKind};
use crate::error::{Error, Result};

const RECUR_PROPERTY: &str = "Binary {00062002-0000-0000-C000-000000000046} Id 0x8216";
const OP_PROPERTY: &str =
    "String {77f84316-463d-49ec-ae0c-247e05f13645} Name ChithiCalendarOperation";
const EXTRA_SELECT: &str = "originalStartTimeZone,originalEndTimeZone,occurrenceId,transactionId,categories,importance,sensitivity,showAs,isReminderOn,reminderMinutesBeforeStart,responseRequested,hideAttendees,locations,isOnlineMeeting,onlineMeeting";

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::Sync(format!(
        "Graph event set: {message}; reconciliation required"
    ))
}

pub(crate) fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|s| !s.trim().is_empty() && !s.chars().any(char::is_control))
        .ok_or_else(|| invalid(format!("missing or invalid {key}")))
}

pub(crate) fn collection_path(calendar: &str) -> Result<String> {
    if calendar.trim().is_empty()
        || matches!(calendar, "." | "..")
        || calendar.chars().any(char::is_control)
    {
        return Err(invalid("selected calendar ID is required"));
    }
    Ok(format!(
        "/me/calendars/{}/events",
        urlencoding::encode(calendar)
    ))
}

fn event_path(calendar: &str, id: &str) -> Result<String> {
    if id.trim().is_empty() || matches!(id, "." | "..") || id.chars().any(char::is_control) {
        return Err(invalid("event ID is required"));
    }
    Ok(format!(
        "{}/{}",
        collection_path(calendar)?,
        urlencoding::encode(id)
    ))
}

pub(crate) fn zone(name: &str) -> Result<chrono_tz::Tz> {
    if matches!(name, "UTC" | "Etc/UTC" | "Etc/GMT" | "GMT") {
        return Ok(chrono_tz::UTC);
    }
    if name.trim() != name || name.is_empty() {
        return Err(invalid("invalid timezone"));
    }
    crate::calendar::timezone::windows_to_iana(name)
        .unwrap_or(name)
        .parse()
        .map_err(|_| invalid(format!("unknown timezone {name:?}")))
}

fn instant(value: &str) -> Result<DateTime<Utc>> {
    let time = DateTime::parse_from_rfc3339(value)
        .map_err(|_| invalid("invalid timestamp"))?
        .with_timezone(&Utc);
    if time.nanosecond() >= 1_000_000_000 {
        return Err(invalid("leap second is not representable"));
    }
    Ok(time)
}

fn utc(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn native_time(value: &Value) -> Result<DateTime<Utc>> {
    let name = required_string(value, "timeZone")?;
    let tz = zone(name)?;
    let text = required_string(value, "dateTime")?;
    if let Ok(time) = instant(text) {
        return Ok(time);
    }
    let local = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
        .map_err(|_| invalid("invalid native wall time"))?;
    tz.from_local_datetime(&local)
        .single()
        .map(|t| t.with_timezone(&Utc))
        .ok_or_else(|| invalid("ambiguous or nonexistent native wall time"))
}

fn original(master: &CalendarEvent, value: &Value) -> Result<String> {
    let time = instant(required_string(value, "originalStart")?)?;
    if master.all_day {
        Ok(time
            .with_timezone(&zone(master.timezone.as_deref().unwrap_or("UTC"))?)
            .date_naive()
            .to_string())
    } else {
        Ok(utc(time))
    }
}

fn native(value: &Value, calendar: &str) -> Result<NativeCalendarResource> {
    if required_string(value, "@odata.etag")? == "*" {
        return Err(invalid("wildcard is not an event revision"));
    }
    Ok(NativeCalendarResource {
        protocol: "graph".into(),
        calendar_id: calendar.into(),
        event_id: required_string(value, "id")?.into(),
        revision: Some(required_string(value, "@odata.etag")?.into()),
        data: value.to_string(),
    })
}

/// Strict full-content projection. The UTC response preference is undone for
/// all-day dates using the original zone, never by truncating a UTC timestamp.
fn canonical(value: &Value, template: &CalendarEvent, calendar: &str) -> Result<CalendarEvent> {
    let kind = super::graph_recurrence_kind(value);
    if kind == RecurrenceKind::Unknown {
        return Err(invalid("incomplete recurrence classification"));
    }
    let all_day = value["isAllDay"]
        .as_bool()
        .ok_or_else(|| invalid("isAllDay must be boolean"))?;
    if value["isCancelled"].as_bool() != Some(false) {
        return Err(invalid("isCancelled must be false"));
    }
    for key in [
        "subject",
        "iCalUId",
        "@odata.etag",
        "originalStartTimeZone",
        "originalEndTimeZone",
    ] {
        required_string(value, key)?;
    }
    if !matches!(value["body"]["contentType"].as_str(), Some("html" | "text"))
        || !value["body"]["content"].is_string()
        || !value["location"]["displayName"].is_string()
        || !value["organizer"]["emailAddress"]["address"].is_string()
        || !value["responseStatus"]["response"].is_string()
    {
        return Err(invalid(
            "incomplete body, location, organizer or responseStatus",
        ));
    }
    let attendees = value["attendees"]
        .as_array()
        .ok_or_else(|| invalid("attendees must be an array"))?;
    for attendee in attendees {
        required_string(&attendee["emailAddress"], "address")?;
        if !matches!(
            attendee["type"].as_str(),
            Some("required" | "optional" | "resource")
        ) {
            return Err(invalid("invalid attendee type"));
        }
        required_string(&attendee["status"], "response")?;
    }
    let start = native_time(&value["start"])?;
    let end = native_time(&value["end"])?;
    let timezone = if kind == RecurrenceKind::Series {
        value["recurrence"]["range"]["recurrenceTimeZone"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(required_string(value, "originalStartTimeZone")?)
    } else {
        required_string(value, "originalStartTimeZone")?
    };
    let tz = zone(timezone)?;
    let (start_text, end_text) = if all_day {
        let end_tz = zone(required_string(value, "originalEndTimeZone")?)?;
        let a = start.with_timezone(&tz);
        let b = end.with_timezone(&end_tz);
        if tz != end_tz || a.time() != chrono::NaiveTime::MIN || b.time() != chrono::NaiveTime::MIN
        {
            return Err(invalid("all-day boundaries must be midnight in one zone"));
        }
        (a.date_naive().to_string(), b.date_naive().to_string())
    } else {
        (utc(start), utc(end))
    };
    // Reuse the existing attendee/status projection after strict validation.
    let mut normalized = value.clone();
    normalized["start"] = json!({"dateTime": if all_day { format!("{start_text}T00:00:00") } else { start_text.clone() }, "timeZone": "UTC"});
    normalized["end"] = json!({"dateTime": if all_day { format!("{end_text}T00:00:00") } else { end_text.clone() }, "timeZone": "UTC"});
    let parsed = super::parse_graph_event(&normalized, calendar)?.into_live()?;
    let mut event = template.clone();
    event.remote_id = Some(required_string(value, "id")?.into());
    event.etag = Some(required_string(value, "@odata.etag")?.into());
    event.uid = parsed.ical_uid;
    event.title = parsed.subject;
    event.description = parsed.body_preview;
    event.location = parsed.location;
    event.start_time = start_text;
    event.end_time = end_text;
    event.all_day = all_day;
    event.timezone = Some(tz.name().into());
    event.attendees_json = parsed.attendees_json;
    event.organizer_email = parsed.organizer_email;
    event.my_status = parsed.my_status;
    event.recurrence_kind = kind;
    event.ical_data = None;
    event.recurrence_rule = if kind == RecurrenceKind::Series {
        Some(read_rule(&value["recurrence"], &event)?)
    } else {
        None
    };
    crate::calendar::event_set::event_fields(&event).validate()?;
    Ok(event)
}

const DAYS: [(&str, &str); 7] = [
    ("MO", "monday"),
    ("TU", "tuesday"),
    ("WE", "wednesday"),
    ("TH", "thursday"),
    ("FR", "friday"),
    ("SA", "saturday"),
    ("SU", "sunday"),
];

fn read_rule(recurrence: &Value, event: &CalendarEvent) -> Result<String> {
    let p = &recurrence["pattern"];
    let range = &recurrence["range"];
    let (_, local) =
        super::recurring_graph_time(&event.start_time, event.all_day, event.timezone.as_deref())?;
    let freq = match p["type"].as_str() {
        Some("daily") => "DAILY",
        Some("weekly") => "WEEKLY",
        Some("absoluteMonthly") if p["dayOfMonth"].as_u64() == Some(local.day().into()) => {
            "MONTHLY"
        }
        Some("absoluteYearly")
            if p["dayOfMonth"].as_u64() == Some(local.day().into())
                && p["month"].as_u64() == Some(local.month().into()) =>
        {
            "YEARLY"
        }
        _ => {
            return Err(invalid(
                "recurrence pattern is outside the editor's lossless subset",
            ))
        }
    };
    let interval = p["interval"]
        .as_u64()
        .filter(|n| (1..=99).contains(n))
        .ok_or_else(|| invalid("invalid recurrence interval"))?;
    let mut rule = format!("FREQ={freq};INTERVAL={interval}");
    if freq == "WEEKLY" {
        let days = p["daysOfWeek"]
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| invalid("weekly days are missing"))?;
        let codes = days
            .iter()
            .map(|d| {
                DAYS.iter()
                    .find(|(_, name)| Some(*name) == d.as_str())
                    .map(|(code, _)| *code)
                    .ok_or_else(|| invalid("invalid weekday"))
            })
            .collect::<Result<Vec<_>>>()?;
        rule.push_str(&format!(";BYDAY={}", codes.join(",")));
        let first = required_string(p, "firstDayOfWeek")?;
        let code = DAYS
            .iter()
            .find(|(_, name)| *name == first)
            .ok_or_else(|| invalid("invalid week start"))?
            .0;
        rule.push_str(&format!(";WKST={code}"));
    }
    if required_string(range, "startDate")? != local.date().to_string() {
        return Err(invalid("recurrence startDate differs from DTSTART"));
    }
    match range["type"].as_str() {
        Some("noEnd") => {}
        Some("numbered") => {
            let count = range["numberOfOccurrences"]
                .as_u64()
                .filter(|n| *n > 0 && *n <= i32::MAX as u64)
                .ok_or_else(|| invalid("invalid recurrence count"))?;
            rule.push_str(&format!(";COUNT={count}"));
        }
        Some("endDate") => {
            let date = NaiveDate::parse_from_str(required_string(range, "endDate")?, "%Y-%m-%d")
                .map_err(|_| invalid("invalid recurrence endDate"))?;
            rule.push_str(&format!(";UNTIL={}", date.format("%Y%m%d")));
        }
        _ => return Err(invalid("invalid recurrence range")),
    }
    simple_recurrence::normalize_rule(&rule, event)
}

pub(super) fn recurrence_json(
    event: &CalendarEvent,
    local: NaiveDateTime,
    rule: &str,
) -> Result<Value> {
    let fields: BTreeMap<_, _> = rule.split(';').filter_map(|s| s.split_once('=')).collect();
    let interval = fields
        .get("INTERVAL")
        .map(|s| s.parse::<u32>())
        .transpose()
        .map_err(invalid)?
        .unwrap_or(1);
    let mut pattern = json!({"interval": interval});
    match fields.get("FREQ").copied() {
        Some("DAILY") => pattern["type"] = json!("daily"),
        Some("WEEKLY") => {
            let days = fields
                .get("BYDAY")
                .map(|s| {
                    s.split(',')
                        .map(|code| {
                            DAYS.iter()
                                .find(|(c, _)| *c == code)
                                .map(|(_, name)| *name)
                                .ok_or_else(|| invalid("invalid weekday"))
                        })
                        .collect::<Result<Vec<_>>>()
                })
                .transpose()?
                .unwrap_or_else(|| vec![super::graph_weekday(local.weekday())]);
            let first = fields.get("WKST").copied().unwrap_or("MO");
            pattern["type"] = json!("weekly");
            pattern["daysOfWeek"] = json!(days);
            pattern["firstDayOfWeek"] = json!(
                DAYS.iter()
                    .find(|(code, _)| *code == first)
                    .ok_or_else(|| invalid("invalid WKST"))?
                    .1
            );
        }
        Some("MONTHLY") => {
            pattern["type"] = json!("absoluteMonthly");
            pattern["dayOfMonth"] = json!(local.day());
        }
        Some("YEARLY") => {
            pattern["type"] = json!("absoluteYearly");
            pattern["dayOfMonth"] = json!(local.day());
            pattern["month"] = json!(local.month());
        }
        _ => return Err(invalid("unsupported recurrence frequency")),
    }
    let mut range = json!({"startDate": local.date().to_string(), "recurrenceTimeZone": event.timezone.as_deref().unwrap_or("UTC")});
    if let Some(count) = fields.get("COUNT") {
        let count = count.parse::<i32>().map_err(invalid)?;
        range["type"] = json!("numbered");
        range["numberOfOccurrences"] = json!(count);
    } else if let Some(until) = fields.get("UNTIL") {
        let end = if until.len() == 8 {
            NaiveDate::parse_from_str(until, "%Y%m%d").map_err(invalid)?
        } else {
            let time = NaiveDateTime::parse_from_str(until, "%Y%m%dT%H%M%SZ")
                .map_err(invalid)?
                .and_utc()
                .with_timezone(&zone(event.timezone.as_deref().unwrap_or("UTC"))?);
            if time.time() < local.time() {
                time.date_naive()
                    .pred_opt()
                    .ok_or_else(|| invalid("UNTIL underflow"))?
            } else {
                time.date_naive()
            }
        };
        range["type"] = json!("endDate");
        range["endDate"] = json!(end.to_string());
    } else {
        range["type"] = json!("noEnd");
    }
    Ok(json!({"pattern": pattern, "range": range}))
}

impl GraphClient {
    /// Validate before attaching credentials, including every continuation page.
    pub(super) fn calendar_set_url(&self, path: &str) -> Result<reqwest::Url> {
        let root = reqwest::Url::parse(&self.endpoints.v1_api_root).map_err(invalid)?;
        let url = if path.starts_with('/') {
            reqwest::Url::parse(&self.endpoints.v1_url(path))
        } else {
            reqwest::Url::parse(path)
        }
        .map_err(invalid)?;
        if url.origin() != root.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !url
                .path()
                .starts_with(&format!("{}/", root.path().trim_end_matches('/')))
        {
            return Err(invalid("untrusted calendar continuation URL"));
        }
        Ok(url)
    }

    pub(super) async fn calendar_set_request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
        etag: Option<&str>,
    ) -> Result<Value> {
        let url = self.calendar_set_url(path)?;
        let response = self
            .send_with_retry(
                || {
                    let mut request = self
                        .http
                        .request(method.clone(), url.clone())
                        .bearer_auth(&self.access_token)
                        .header("Prefer", "outlook.timezone=\"UTC\", IdType=\"ImmutableId\"")
                        .query(query);
                    if let Some(etag) = etag {
                        request = request.header("If-Match", etag);
                    }
                    if let Some(body) = body {
                        request = request.json(body);
                    }
                    request
                },
                "calendar event set",
                method == Method::GET,
            )
            .await?;
        let status = response.status();
        let text = response.text().await.map_err(invalid)?;
        if !status.is_success() {
            return Err(invalid(format!(
                "{method} returned {status}: {}",
                super::truncate(&text, 1000)
            )));
        }
        if text.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(invalid)
    }

    async fn set_pages(&self, mut page: Value, scope: &str) -> Result<Vec<Value>> {
        let mut items = BTreeMap::new();
        let mut visited = HashSet::new();
        loop {
            for value in page["value"]
                .as_array()
                .ok_or_else(|| invalid("collection value must be an array"))?
            {
                let id = required_string(value, "id")?.to_owned();
                if let Some(previous) = items.insert(id, value.clone()) {
                    if previous != *value {
                        return Err(invalid("conflicting duplicate event ID"));
                    }
                }
            }
            match page.get("@odata.nextLink") {
                None | Some(Value::Null) => break,
                Some(Value::String(next)) if !next.trim().is_empty() => {
                    let url = self.calendar_set_url(next)?;
                    let expected = self.calendar_set_url(scope)?;
                    if url.path() != expected.path() {
                        return Err(invalid("continuation left selected calendar collection"));
                    }
                    if !visited.insert(next.clone()) || visited.len() > 10_000 {
                        return Err(invalid("repeated or excessive pagination link"));
                    }
                    page = self
                        .calendar_set_request(Method::GET, next, &[], None, None)
                        .await?;
                }
                _ => return Err(invalid("invalid @odata.nextLink")),
            }
        }
        Ok(items.into_values().collect())
    }

    async fn set_get(&self, calendar: &str, id: &str, complete: bool) -> Result<Value> {
        let mut select = format!("{CALENDAR_EVENT_SELECT},{EXTRA_SELECT}");
        let expand = format!(
            "exceptionOccurrences,singleValueExtendedProperties($filter=id eq '{RECUR_PROPERTY}')"
        );
        let mut query = Vec::new();
        if complete {
            select.push_str(",exceptionOccurrences,cancelledOccurrences");
            query.push(("$expand", expand.as_str()));
        }
        query.push(("$select", select.as_str()));
        let value = self
            .calendar_set_request(Method::GET, &event_path(calendar, id)?, &query, None, None)
            .await?;
        if required_string(&value, "id")? != id {
            return Err(invalid("GET returned a different immutable ID"));
        }
        Ok(value)
    }

    pub(crate) async fn fetch_calendar_event_set(
        &self,
        calendar: &str,
        id: &str,
        template: &CalendarEvent,
    ) -> Result<CalendarEventSet> {
        let first = self.set_get(calendar, id, false).await?;
        let kind = super::graph_recurrence_kind(&first);
        let master_id = if kind == RecurrenceKind::Occurrence {
            required_string(&first, "seriesMasterId")?
        } else {
            id
        };
        let master = if kind == RecurrenceKind::Standalone {
            first.clone()
        } else {
            self.set_get(calendar, master_id, true).await?
        };
        let event = canonical(&master, template, calendar)?;
        if !matches!(
            event.recurrence_kind,
            RecurrenceKind::Standalone | RecurrenceKind::Series
        ) {
            return Err(invalid("expected master or standalone"));
        }
        let mut result = CalendarEventSet {
            event,
            overrides: Vec::new(),
            native: Some(native(&master, calendar)?),
            content: None,
        };
        if result.event.recurrence_kind == RecurrenceKind::Series {
            let exceptions = master["exceptionOccurrences"]
                .as_array()
                .ok_or_else(|| invalid("missing authoritative exceptionOccurrences"))?;
            let page = json!({"value": exceptions, "@odata.nextLink": master.get("exceptionOccurrences@odata.nextLink").cloned().unwrap_or(Value::Null)});
            let scope = format!("{}/exceptionOccurrences", event_path(calendar, master_id)?);
            for reference in self.set_pages(page, &scope).await? {
                let value = self
                    .set_get(calendar, required_string(&reference, "id")?, false)
                    .await?;
                if value["type"] != "exception" || value["seriesMasterId"] != master_id {
                    return Err(invalid("exception belongs to a different master"));
                }
                result.overrides.push(CalendarOverride {
                    original_start: original(&result.event, &value)?,
                    event: Some(canonical(&value, template, calendar)?),
                    native: Some(native(&value, calendar)?),
                });
            }
            let cancelled = master["cancelledOccurrences"]
                .as_array()
                .ok_or_else(|| invalid("missing authoritative cancelledOccurrences"))?;
            let mut ids = HashSet::new();
            for id in cancelled {
                let id = id
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| invalid("malformed cancelled occurrence ID"))?;
                if !ids.insert(id) {
                    return Err(invalid("duplicate cancelled occurrence ID"));
                }
            }
            if !cancelled.is_empty() {
                let properties = master["singleValueExtendedProperties"].as_array().ok_or_else(|| invalid("cancelled slots require PidLidAppointmentRecur; Graph returned no legacy property"))?;
                let matches: Vec<_> = properties
                    .iter()
                    .filter(|p| {
                        p["id"]
                            .as_str()
                            .is_some_and(|id| id.eq_ignore_ascii_case(RECUR_PROPERTY))
                    })
                    .collect();
                if matches.len() != 1 {
                    return Err(invalid(
                        "cancelled slots require exactly one PidLidAppointmentRecur",
                    ));
                }
                let positions = recurrence_blob::deleted_positions(
                    required_string(matches[0], "value")?,
                    &result.event,
                    &result.overrides,
                )?;
                if positions.len() != cancelled.len() {
                    return Err(invalid(
                        "legacy deleted slots disagree with cancelledOccurrences count",
                    ));
                }
                result
                    .overrides
                    .extend(
                        positions
                            .into_iter()
                            .map(|original_start| CalendarOverride {
                                original_start,
                                event: None,
                                native: None,
                            }),
                    );
            }
            // Graph has no snapshot transaction across expanded exception pages.
            // A changed master revision invalidates this entire read.
            let after = self.set_get(calendar, master_id, true).await?;
            if after["@odata.etag"] != master["@odata.etag"]
                || after["cancelledOccurrences"] != master["cancelledOccurrences"]
                || after["exceptionOccurrences"] != master["exceptionOccurrences"]
            {
                return Err(invalid("master changed while reading its exceptions"));
            }
        }
        result
            .overrides
            .sort_by(|a, b| a.original_start.cmp(&b.original_start));
        result.validate()?;
        Ok(result)
    }
}
