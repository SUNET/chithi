//! Lossless component editing for complete DAV resources. Only changed properties
//! are replaced; unknown properties, parameter lists and nested components survive.

use std::collections::{BTreeMap, HashSet};

use crate::calendar::event_set::{CalendarEventSet, CalendarOverride, NativeCalendarResource};
use crate::calendar::{Attendee, CalendarEvent, RecurrenceKind};
use crate::error::{Error, Result};
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};

fn invalid(message: &str) -> Error {
    Error::Other(format!("CalDAV resource: {message}"))
}

#[derive(Clone, Debug)]
enum Entry {
    Property(String),
    Component(Component),
}

#[derive(Clone, Debug)]
struct Component {
    name: String,
    entries: Vec<Entry>,
}

fn split(line: &str) -> Result<(&str, &str)> {
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        if c == '"' {
            quoted = !quoted;
        }
        if c == ':' && !quoted {
            return Ok((&line[..i], &line[i + 1..]));
        }
    }
    Err(invalid("malformed content line"))
}

fn name(line: &str) -> &str {
    line.split([';', ':']).next().unwrap_or("")
}

fn parameter<'a>(line: &'a str, key: &str) -> Result<Option<&'a str>> {
    let (head, _) = split(line)?;
    let mut quoted = false;
    let mut start = 0;
    let mut parts = Vec::new();
    for (i, c) in head.char_indices() {
        if c == '"' {
            quoted = !quoted;
        }
        if c == ';' && !quoted {
            parts.push(&head[start..i]);
            start = i + 1;
        }
    }
    parts.push(&head[start..]);
    let mut found = None;
    for part in parts.into_iter().skip(1) {
        let (k, v) = part
            .split_once('=')
            .ok_or_else(|| invalid("malformed parameter"))?;
        if k.eq_ignore_ascii_case(key) {
            if found.is_some() {
                return Err(invalid("duplicate parameter"));
            }
            found = Some(v.trim_matches('"'));
        }
    }
    Ok(found)
}

impl Component {
    fn parse(raw: &str) -> Result<Self> {
        let mut lines: Vec<String> = Vec::new();
        for line in raw.lines() {
            if line.starts_with([' ', '\t']) {
                lines
                    .last_mut()
                    .ok_or_else(|| invalid("orphan folded line"))?
                    .push_str(&line[1..]);
            } else if !line.is_empty() {
                lines.push(line.to_owned());
            }
        }
        let mut stack: Vec<Component> = Vec::new();
        let mut root = None;
        for line in lines {
            let (head, value) = split(&line)?;
            if head.eq_ignore_ascii_case("BEGIN") {
                if stack.len() > 16 {
                    return Err(invalid("component nesting too deep"));
                }
                stack.push(Self {
                    name: value.to_owned(),
                    entries: Vec::new(),
                });
            } else if head.eq_ignore_ascii_case("END") {
                let component = stack.pop().ok_or_else(|| invalid("unmatched END"))?;
                if !component.name.eq_ignore_ascii_case(value) {
                    return Err(invalid("mismatched END"));
                }
                if let Some(parent) = stack.last_mut() {
                    parent.entries.push(Entry::Component(component));
                } else if root.replace(component).is_some() {
                    return Err(invalid("multiple calendars"));
                }
            } else {
                stack
                    .last_mut()
                    .ok_or_else(|| invalid("property outside component"))?
                    .entries
                    .push(Entry::Property(line));
            }
        }
        if !stack.is_empty() {
            return Err(invalid("incomplete calendar"));
        }
        let root = root.ok_or_else(|| invalid("missing calendar"))?;
        if !root.name.eq_ignore_ascii_case("VCALENDAR") || root.value("VERSION")? != Some("2.0") {
            return Err(invalid("expected VCALENDAR version 2.0"));
        }
        Ok(root)
    }

    fn properties<'a>(&'a self, key: &str) -> impl Iterator<Item = &'a str> {
        let key = key.to_owned();
        self.entries.iter().filter_map(move |entry| match entry {
            Entry::Property(line) if name(line).eq_ignore_ascii_case(&key) => Some(line.as_str()),
            _ => None,
        })
    }

    fn property(&self, key: &str) -> Result<Option<&str>> {
        let mut matches = self.properties(key);
        let first = matches.next();
        if matches.next().is_some() {
            return Err(invalid(&format!("duplicate {key}")));
        }
        Ok(first)
    }

    fn value(&self, key: &str) -> Result<Option<&str>> {
        self.property(key)?
            .map(|line| split(line).map(|(_, v)| v))
            .transpose()
    }

    fn remove(&mut self, key: &str) {
        self.entries.retain(
            |entry| !matches!(entry, Entry::Property(line) if name(line).eq_ignore_ascii_case(key)),
        );
    }

    fn set(&mut self, key: &str, value: Option<String>) {
        self.remove(key);
        if let Some(line) = value {
            self.entries.push(Entry::Property(line));
        }
    }

    fn events(&self) -> impl Iterator<Item = &Component> {
        self.entries.iter().filter_map(|entry| match entry {
            Entry::Component(c) if c.name.eq_ignore_ascii_case("VEVENT") => Some(c),
            _ => None,
        })
    }

    fn render(&self) -> String {
        let mut output = String::new();
        fold(&format!("BEGIN:{}", self.name), &mut output);
        for entry in &self.entries {
            match entry {
                Entry::Property(line) => fold(line, &mut output),
                Entry::Component(c) => output.push_str(&c.render()),
            }
        }
        fold(&format!("END:{}", self.name), &mut output);
        output
    }
}

fn ensure_timezones(root: &mut Component) -> Result<()> {
    let mut needed = HashSet::new();
    for event in root.events() {
        for entry in &event.entries {
            if let Entry::Property(line) = entry {
                if let Some(tzid) = parameter(line, "TZID")? {
                    needed.insert(tzid.to_owned());
                }
            }
        }
    }
    for tzid in needed {
        if root.entries.iter().any(|entry| matches!(entry, Entry::Component(c)
            if c.name.eq_ignore_ascii_case("VTIMEZONE") && c.value("TZID").ok().flatten() == Some(tzid.as_str()))) {
            continue;
        }
        let definition = super::timezones::definition(&tzid)?;
        let calendar = Component::parse(&format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{definition}END:VCALENDAR\r\n"
        ))?;
        root.entries.extend(
            calendar
                .entries
                .into_iter()
                .filter(|entry| matches!(entry, Entry::Component(_))),
        );
    }
    Ok(())
}

fn fold(line: &str, output: &mut String) {
    let mut width = 0;
    for c in line.chars() {
        if width + c.len_utf8() > 75 {
            output.push_str("\r\n ");
            width = 1;
        }
        output.push(c);
        width += c.len_utf8();
    }
    output.push_str("\r\n");
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\\n")
        .replace(';', "\\;")
        .replace(',', "\\,")
}

fn unescape(value: &str) -> String {
    let mut output = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n' | 'N') => output.push('\n'),
                Some(c) => output.push(c),
                None => output.push('\\'),
            }
        } else {
            output.push(c);
        }
    }
    output
}

/// Decode original positions without applying the effective DTSTART or host zone.
pub(super) fn position(line: &str) -> Result<String> {
    if parameter(line, "RANGE")?.is_some() {
        return Err(invalid(
            "RANGE recurrence is structurally unsupported; source must remain intact",
        ));
    }
    let (_, value) = split(line)?;
    let kind = parameter(line, "VALUE")?.unwrap_or("DATE-TIME");
    if kind.eq_ignore_ascii_case("DATE") {
        if parameter(line, "TZID")?.is_some() {
            return Err(invalid("DATE cannot have TZID"));
        }
        return NaiveDate::parse_from_str(value, "%Y%m%d")
            .map(|d| d.format("%Y-%m-%d").to_string())
            .map_err(|_| invalid("invalid DATE"));
    }
    if !kind.eq_ignore_ascii_case("DATE-TIME") {
        return Err(invalid(
            "unsupported recurrence value type (including PERIOD)",
        ));
    }
    let utc = value.ends_with('Z');
    let dt = NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y%m%dT%H%M%S")
        .map_err(|_| invalid("invalid DATE-TIME"))?;
    let timezone = parameter(line, "TZID")?;
    let instant = if utc && timezone.is_none() {
        dt.and_utc()
    } else if !utc {
        let tz: chrono_tz::Tz = timezone
            .ok_or_else(|| invalid("floating time has no authoritative timezone"))?
            .parse()
            .map_err(|_| invalid("unknown native timezone"))?;
        tz.from_local_datetime(&dt)
            .earliest()
            .ok_or_else(|| invalid("nonexistent local time"))?
            .with_timezone(&Utc)
    } else {
        return Err(invalid("UTC value with TZID"));
    };
    Ok(instant.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn date_property(key: &str, value: &str, all_day: bool, timezone: Option<&str>) -> Result<String> {
    if all_day {
        let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map_err(|_| invalid("invalid ISO date"))?;
        if !(1..=9999).contains(&date.year()) {
            return Err(invalid("date exceeds supported years 0001..9999"));
        }
        return Ok(format!("{key};VALUE=DATE:{}", date.format("%Y%m%d")));
    }
    let dt = DateTime::parse_from_rfc3339(value).map_err(|_| invalid("invalid RFC3339 time"))?;
    if !(1..=9999).contains(&dt.year()) {
        return Err(invalid("date exceeds supported years 0001..9999"));
    }
    if dt.timestamp_subsec_nanos() != 0 {
        return Err(invalid("iCalendar cannot represent fractional seconds"));
    }
    if let Some(timezone) = timezone.filter(|v| !v.is_empty()) {
        let tz: chrono_tz::Tz = timezone
            .parse()
            .map_err(|_| invalid("unknown IANA timezone"))?;
        let line = format!(
            "{key};TZID={timezone}:{}",
            dt.with_timezone(&tz).format("%Y%m%dT%H%M%S")
        );
        if position(&line)?
            != dt
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        {
            if matches!(key, "RECURRENCE-ID" | "EXDATE" | "RDATE" | "DTEND") {
                return Ok(format!(
                    "{key}:{}",
                    dt.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ")
                ));
            }
            return Err(invalid(
                "named DTSTART in the second DST fold cannot preserve the supplied instant",
            ));
        }
        Ok(line)
    } else {
        Ok(format!(
            "{key}:{}",
            dt.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ")
        ))
    }
}

fn component_event(component: &Component, template: &CalendarEvent) -> Result<CalendarEvent> {
    let raw = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Chithi//EN\r\n{}END:VCALENDAR\r\n",
        component.render()
    );
    let parsed = crate::calendar::ical::parse_ical_data(&raw);
    let [invite] = parsed.as_slice() else {
        return Err(invalid("cannot decode event fields"));
    };
    let mut event = template.clone();
    event.uid = Some(unescape(
        component
            .value("UID")?
            .ok_or_else(|| invalid("missing UID"))?,
    ));
    event.title = component
        .value("SUMMARY")?
        .map(unescape)
        .unwrap_or_default();
    event.description = component.value("DESCRIPTION")?.map(unescape);
    event.location = component.value("LOCATION")?.map(unescape);
    let start = component
        .property("DTSTART")?
        .ok_or_else(|| invalid("missing DTSTART"))?;
    event.start_time = position(start)?;
    event.all_day = parameter(start, "VALUE")?.is_some_and(|v| v.eq_ignore_ascii_case("DATE"));
    event.timezone = parameter(start, "TZID")?.map(str::to_owned);
    event.end_time = match component.property("DTEND")? {
        Some(end) => {
            if component.property("DURATION")?.is_some() {
                return Err(invalid("DTEND and DURATION coexist"));
            }
            position(end)?
        }
        None if event.all_day && component.property("DURATION")?.is_none() => {
            NaiveDate::parse_from_str(&event.start_time, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.succ_opt())
                .ok_or_else(|| invalid("date overflow"))?
                .format("%Y-%m-%d")
                .to_string()
        }
        None => invite.dtend.clone(),
    };
    event.recurrence_rule = component.value("RRULE")?.map(str::to_owned);
    event.recurrence_kind = if component.property("RECURRENCE-ID")?.is_some() {
        RecurrenceKind::Occurrence
    } else if event.recurrence_rule.is_some() || component.properties("RDATE").next().is_some() {
        RecurrenceKind::Series
    } else {
        RecurrenceKind::Standalone
    };
    event.organizer_email = invite.organizer_email.clone();
    event.attendees_json = if invite.attendees.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&invite.attendees).map_err(|e| invalid(&e.to_string()))?)
    };
    event.source_message_id = None;
    crate::calendar::event_set::event_fields(&event).validate()?;
    Ok(event)
}

fn recurrence_dates(component: &Component, key: &str) -> Result<Vec<String>> {
    let mut result = Vec::new();
    for line in component.properties(key) {
        let (head, values) = split(line)?;
        for value in values.split(',') {
            result.push(position(&format!("{head}:{value}"))?);
        }
    }
    Ok(result)
}

fn at_position(master: &CalendarEvent, original: &str) -> Result<CalendarEvent> {
    let mut event = master.clone();
    if master.all_day {
        let parse =
            |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| invalid("invalid date"));
        let duration = parse(&master.end_time)? - parse(&master.start_time)?;
        event.end_time = parse(original)?
            .checked_add_signed(duration)
            .ok_or_else(|| invalid("date overflow"))?
            .to_string();
    } else {
        let parse =
            |s: &str| DateTime::parse_from_rfc3339(s).map_err(|_| invalid("invalid timestamp"));
        let duration = parse(&master.end_time)? - parse(&master.start_time)?;
        event.end_time = parse(original)?
            .checked_add_signed(duration)
            .ok_or_else(|| invalid("date overflow"))?
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Secs, true);
    }
    event.start_time = original.to_owned();
    event.recurrence_rule = None;
    event.recurrence_kind = RecurrenceKind::Occurrence;
    Ok(event)
}

pub(super) fn snapshot(
    native: NativeCalendarResource,
    template: &CalendarEvent,
) -> Result<CalendarEventSet> {
    let root = Component::parse(&native.data)?;
    let mut masters = Vec::new();
    let mut uid = None;
    for component in root.events() {
        let current = component
            .value("UID")?
            .ok_or_else(|| invalid("missing UID"))?;
        if current.is_empty() || uid.is_some_and(|uid| uid != current) {
            return Err(invalid("mixed resource UIDs"));
        }
        uid = Some(current);
        if component.property("RECURRENCE-ID")?.is_none() {
            masters.push(component);
        }
    }
    let [master] = masters.as_slice() else {
        return Err(invalid("expected exactly one master or standalone event"));
    };
    let mut event = component_event(master, template)?;
    event.remote_id = Some(native.event_id.clone());
    event.etag = native.revision.clone();
    event.ical_data = Some(native.data.clone());
    let mut overrides = BTreeMap::new();
    for original in recurrence_dates(master, "RDATE")? {
        overrides.insert(
            original.clone(),
            CalendarOverride {
                original_start: original.clone(),
                event: Some(at_position(&event, &original)?),
                native: None,
            },
        );
    }
    for original in recurrence_dates(master, "EXDATE")? {
        overrides.insert(
            original.clone(),
            CalendarOverride {
                original_start: original,
                event: None,
                native: None,
            },
        );
    }
    let mut seen = HashSet::new();
    for component in root.events() {
        let Some(rid) = component.property("RECURRENCE-ID")? else {
            continue;
        };
        let original = position(rid)?;
        if !seen.insert(original.clone()) {
            return Err(invalid("duplicate original occurrence"));
        }
        let cancelled = component
            .value("STATUS")?
            .is_some_and(|v| v.eq_ignore_ascii_case("CANCELLED"));
        let live = if cancelled {
            None
        } else {
            Some(component_event(component, &event)?)
        };
        if overrides.get(&original).is_some_and(|v| v.event.is_none()) && live.is_some() {
            return Err(invalid("live override contradicts EXDATE"));
        }
        overrides.insert(
            original.clone(),
            CalendarOverride {
                original_start: original,
                event: live,
                native: Some(native.clone()),
            },
        );
    }
    let set = CalendarEventSet {
        event,
        overrides: overrides.into_values().collect(),
        native: Some(native),
        content: None,
    };
    if set.event.recurrence_kind == RecurrenceKind::Standalone && !set.overrides.is_empty() {
        return Err(invalid(
            "standalone event cannot contain recurrence exceptions",
        ));
    }
    // RDATE-only and opaque rules remain readable; structural writes validate the UI subset.
    for exception in &set.overrides {
        crate::calendar::event_set::canonical_position(&set.event, &exception.original_start)?;
    }
    Ok(set)
}

fn attendee_lines(event: &CalendarEvent) -> Result<Vec<String>> {
    let attendees: Vec<Attendee> = event
        .attendees_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| invalid(&format!("invalid attendees: {e}")))?
        .unwrap_or_default();
    let mut lines = Vec::new();
    for attendee in attendees {
        if attendee.email.chars().any(char::is_control) || attendee.email.contains([',', ';', ' '])
        {
            return Err(invalid("invalid attendee address"));
        }
        let status = match attendee.status.to_ascii_lowercase().as_str() {
            "accepted" => "ACCEPTED",
            "tentative" => "TENTATIVE",
            "declined" => "DECLINED",
            _ => "NEEDS-ACTION",
        };
        let cn = attendee
            .name
            .map(|v| {
                format!(
                    ";CN=\"{}\"",
                    v.replace('^', "^^")
                        .replace('"', "^'")
                        .replace(['\r', '\n'], "^n")
                )
            })
            .unwrap_or_default();
        lines.push(format!(
            "ATTENDEE{cn};PARTSTAT={status}:mailto:{}",
            attendee.email
        ));
    }
    Ok(lines)
}

fn overlay(
    component: &mut Component,
    before: Option<&CalendarEvent>,
    desired: &CalendarEvent,
) -> Result<()> {
    crate::calendar::event_set::event_fields(desired).validate()?;
    for text in [
        Some(desired.title.as_str()),
        desired.description.as_deref(),
        desired.location.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
        {
            return Err(invalid(
                "text contains an invalid iCalendar control character",
            ));
        }
    }
    for (key, value, old) in [
        (
            "SUMMARY",
            Some(desired.title.as_str()),
            before.map(|b| b.title.as_str()),
        ),
        (
            "DESCRIPTION",
            desired.description.as_deref(),
            before.and_then(|b| b.description.as_deref()),
        ),
        (
            "LOCATION",
            desired.location.as_deref(),
            before.and_then(|b| b.location.as_deref()),
        ),
    ] {
        if before.is_none() || old != value {
            component.set(key, value.map(|v| format!("{key}:{}", escape(v))));
        }
    }
    if before.is_none_or(|b| {
        b.start_time != desired.start_time
            || b.end_time != desired.end_time
            || b.all_day != desired.all_day
            || b.timezone != desired.timezone
    }) {
        component.set(
            "DTSTART",
            Some(date_property(
                "DTSTART",
                &desired.start_time,
                desired.all_day,
                desired.timezone.as_deref(),
            )?),
        );
        component.set(
            "DTEND",
            Some(date_property(
                "DTEND",
                &desired.end_time,
                desired.all_day,
                desired.timezone.as_deref(),
            )?),
        );
        component.remove("DURATION");
    }
    if before.is_none_or(|b| b.recurrence_rule != desired.recurrence_rule) {
        if let Some(old) = before.and_then(|b| b.recurrence_rule.as_deref()) {
            crate::calendar::simple_recurrence::normalize_rule(
                old,
                before.ok_or_else(|| invalid("missing before"))?,
            )?;
        }
        let rule = desired
            .recurrence_rule
            .as_deref()
            .filter(|v| !v.is_empty())
            .map(|rule| wire_rule(rule, desired))
            .transpose()?;
        component.set("RRULE", rule.map(|rule| format!("RRULE:{rule}")));
    }
    // Existing scheduling parameters belong to the server, so unchanged participant
    // lists are retained verbatim. Replacing native participants requires a richer API.
    if before.is_some_and(|b| {
        b.attendees_json != desired.attendees_json || b.organizer_email != desired.organizer_email
    }) {
        return Err(invalid(
            "native participant editing requires scheduling-aware parameters",
        ));
    }
    if before.is_none() {
        if let Some(email) = &desired.organizer_email {
            if email.chars().any(char::is_control) || email.contains([' ', ';', ',']) {
                return Err(invalid("invalid organizer"));
            }
            component.set("ORGANIZER", Some(format!("ORGANIZER:mailto:{email}")));
        }
        for line in attendee_lines(desired)? {
            component.entries.push(Entry::Property(line));
        }
    }
    Ok(())
}

fn wire_rule(rule: &str, event: &CalendarEvent) -> Result<String> {
    let rule = crate::calendar::simple_recurrence::normalize_rule(rule, event)?;
    let mut parts = Vec::new();
    for part in rule.split(';') {
        if let Some(value) = part.strip_prefix("UNTIL=") {
            if event.all_day {
                if value.len() != 8 {
                    return Err(invalid("all-day UNTIL must be DATE"));
                }
            } else if value.len() == 8 {
                let date = NaiveDate::parse_from_str(value, "%Y%m%d")
                    .map_err(|_| invalid("invalid UNTIL"))?;
                let tz: chrono_tz::Tz = event
                    .timezone
                    .as_deref()
                    .unwrap_or("UTC")
                    .parse()
                    .map_err(|_| invalid("invalid timezone"))?;
                let local = date
                    .and_hms_opt(23, 59, 59)
                    .ok_or_else(|| invalid("invalid UNTIL"))?;
                let end = tz
                    .from_local_datetime(&local)
                    .latest()
                    .ok_or_else(|| invalid("invalid UNTIL local time"))?;
                parts.push(format!(
                    "UNTIL={}",
                    end.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ")
                ));
                continue;
            }
        }
        parts.push(part.to_owned());
    }
    Ok(parts.join(";"))
}

fn same_content(a: &CalendarEvent, b: &CalendarEvent) -> bool {
    crate::calendar::event_set::event_fields(a) == crate::calendar::event_set::event_fields(b)
        && a.recurrence_rule == b.recurrence_rule
        && a.attendees_json == b.attendees_json
        && a.organizer_email == b.organizer_email
}

fn same_override(a: Option<&CalendarOverride>, b: Option<&CalendarOverride>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => match (&a.event, &b.event) {
            (None, None) => true,
            (Some(a), Some(b)) => same_content(a, b),
            _ => false,
        },
        _ => false,
    }
}

pub(super) fn same_set(a: &CalendarEventSet, b: &CalendarEventSet) -> bool {
    same_content(&a.event, &b.event)
        && a.overrides.len() == b.overrides.len()
        && a.overrides.iter().all(|item| {
            same_override(
                Some(item),
                b.overrides
                    .iter()
                    .find(|other| other.original_start == item.original_start),
            )
        })
}

/// Edit one whole resource; unmodified exception siblings are never regenerated.
pub(super) fn rewrite(before: &CalendarEventSet, desired: &CalendarEventSet) -> Result<String> {
    let native = before
        .native
        .as_ref()
        .ok_or_else(|| invalid("missing complete native resource"))?;
    let canonical = snapshot(native.clone(), &before.event)?;
    if canonical.event.uid != before.event.uid {
        return Err(invalid("source UID contradicts native resource"));
    }
    let mut root = Component::parse(&native.data)?;
    let mut positions = HashSet::new();
    for item in &desired.overrides {
        if !positions.insert(&item.original_start) {
            return Err(invalid("duplicate desired occurrence"));
        }
        crate::calendar::event_set::canonical_position(&desired.event, &item.original_start)?;
    }
    let mut master = root
        .events()
        .find(|c| c.property("RECURRENCE-ID").is_ok_and(|p| p.is_none()))
        .cloned()
        .ok_or_else(|| invalid("missing master"))?;
    let structural = before.event.recurrence_rule != desired.event.recurrence_rule
        || before.event.all_day != desired.event.all_day
        || before.event.timezone != desired.event.timezone
        || before.event.start_time != desired.event.start_time;
    if structural
        && (master.properties("EXRULE").next().is_some()
            || before.event.recurrence_rule.as_deref().is_some_and(|r| {
                crate::calendar::simple_recurrence::normalize_rule(r, &before.event).is_err()
            }))
    {
        return Err(invalid(
            "structural changes to imported recurrence are unsupported",
        ));
    }
    overlay(&mut master, Some(&before.event), &desired.event)?;
    let mut changed = BTreeMap::new();
    for item in before.overrides.iter().chain(&desired.overrides) {
        let original = &item.original_start;
        let old = before
            .overrides
            .iter()
            .find(|v| v.original_start == *original);
        let new = desired
            .overrides
            .iter()
            .find(|v| v.original_start == *original);
        if !same_override(old, new) {
            changed.insert(original.clone(), (old, new));
        }
    }
    let mut generated = Vec::new();
    for (original, (old, new)) in &changed {
        crate::calendar::event_set::canonical_position(&desired.event, original)?;
        let selected = root.events().find(|c| {
            c.property("RECURRENCE-ID")
                .ok()
                .flatten()
                .and_then(|p| position(p).ok())
                .as_deref()
                == Some(original)
        });
        for key in ["EXDATE", "RDATE"] {
            if key == "RDATE" && new.is_some() {
                continue;
            }
            let lines = master
                .properties(key)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            master.remove(key);
            for line in lines {
                let (head, values) = split(&line)?;
                let mut retained = Vec::new();
                for value in values.split(',') {
                    if position(&format!("{head}:{value}"))? != *original {
                        retained.push(value);
                    }
                }
                if !retained.is_empty() {
                    master
                        .entries
                        .push(Entry::Property(format!("{head}:{}", retained.join(","))));
                }
            }
        }
        let Some(new) = new else {
            continue;
        };
        let Some(event) = &new.event else {
            master.entries.push(Entry::Property(date_property(
                "EXDATE",
                original,
                desired.event.all_day,
                desired.event.timezone.as_deref(),
            )?));
            continue;
        };
        let mut component = selected.cloned().unwrap_or_else(|| master.clone());
        for key in ["RRULE", "RDATE", "EXDATE", "EXRULE"] {
            component.remove(key);
        }
        if selected.is_none() {
            component.set(
                "RECURRENCE-ID",
                Some(date_property(
                    "RECURRENCE-ID",
                    original,
                    before.event.all_day,
                    before.event.timezone.as_deref(),
                )?),
            );
        }
        let inherited = at_position(&before.event, original)?;
        let previous = old.and_then(|o| o.event.as_ref()).unwrap_or(&inherited);
        // A cloned master still carries the master's DTSTART, even when the effective
        // generated occurrence has the same fields as the desired exception.
        if selected.is_none() {
            component.set(
                "DTSTART",
                Some(date_property(
                    "DTSTART",
                    &event.start_time,
                    event.all_day,
                    event.timezone.as_deref(),
                )?),
            );
            component.set(
                "DTEND",
                Some(date_property(
                    "DTEND",
                    &event.end_time,
                    event.all_day,
                    event.timezone.as_deref(),
                )?),
            );
            component.remove("DURATION");
        }
        if component
            .value("STATUS")?
            .is_some_and(|s| s.eq_ignore_ascii_case("CANCELLED"))
        {
            component.remove("STATUS");
        }
        overlay(&mut component, Some(previous), event)?;
        generated.push(component);
    }
    let mut entries = Vec::new();
    for entry in root.entries {
        if let Entry::Component(c) = &entry {
            if c.name.eq_ignore_ascii_case("VEVENT") {
                match c.property("RECURRENCE-ID")? {
                    None => {
                        entries.push(Entry::Component(master.clone()));
                        continue;
                    }
                    Some(line) if changed.contains_key(&position(line)?) => continue,
                    _ => {}
                }
            }
        }
        entries.push(entry);
    }
    entries.extend(generated.into_iter().map(Entry::Component));
    root.entries = entries;
    ensure_timezones(&mut root)?;
    let data = root.render();
    snapshot(
        NativeCalendarResource {
            data: data.clone(),
            ..native.clone()
        },
        &before.event,
    )?;
    Ok(data)
}

/// Merge nonstandard detached resources for a finite provider-neutral read while
/// retaining each original href, validator, and complete VCALENDAR independently.
pub(super) fn combine(
    resources: &[NativeCalendarResource],
    template: &CalendarEvent,
) -> Result<CalendarEventSet> {
    let mut master_native = None;
    let mut roots = Vec::new();
    let mut owners = BTreeMap::new();
    for native in resources {
        let root = Component::parse(&native.data)?;
        for event in root.events() {
            if event.value("UID")?.map(unescape) != template.uid {
                return Err(invalid("detached resource UID mismatch"));
            }
            if let Some(rid) = event.property("RECURRENCE-ID")? {
                if owners.insert(position(rid)?, native.clone()).is_some() {
                    return Err(invalid("duplicate detached position"));
                }
            } else if master_native.replace(native.clone()).is_some() {
                return Err(invalid("multiple masters"));
            }
        }
        roots.push(root);
    }
    let master_native = master_native
        .ok_or_else(|| invalid("detached resource has no master in the selected calendar"))?;
    let mut merged = Component::parse(&master_native.data)?;
    for (root, native) in roots.into_iter().zip(resources) {
        if native.event_id == master_native.event_id {
            continue;
        }
        for entry in root.entries {
            if let Entry::Component(component) = entry {
                if component.name.eq_ignore_ascii_case("VEVENT") {
                    merged.entries.push(Entry::Component(component));
                } else if component.name.eq_ignore_ascii_case("VTIMEZONE") {
                    let tzid = component.value("TZID")?;
                    let existing = merged.entries.iter().find_map(|entry| match entry {
                        Entry::Component(c)
                            if c.name.eq_ignore_ascii_case("VTIMEZONE")
                                && c.value("TZID").ok().flatten() == tzid =>
                        {
                            Some(c)
                        }
                        _ => None,
                    });
                    if let Some(existing) = existing {
                        if existing.render() != component.render() {
                            return Err(invalid("conflicting detached timezone definitions"));
                        }
                    } else {
                        merged.entries.push(Entry::Component(component));
                    }
                }
            }
        }
    }
    let mut set = snapshot(
        NativeCalendarResource {
            data: merged.render(),
            ..master_native.clone()
        },
        template,
    )?;
    for item in &mut set.overrides {
        if let Some(owner) = owners.get(&item.original_start) {
            item.native = Some(owner.clone());
        }
    }
    set.event.ical_data = Some(master_native.data.clone());
    set.native = Some(master_native);
    Ok(set)
}

pub(super) fn detached_write(
    native: &NativeCalendarResource,
    before: &CalendarEventSet,
    desired: &CalendarEventSet,
) -> Result<Option<String>> {
    let mut root = Component::parse(&native.data)?;
    let mut changed = false;
    for entry in &mut root.entries {
        let Entry::Component(component) = entry else {
            continue;
        };
        if !component.name.eq_ignore_ascii_case("VEVENT") {
            continue;
        }
        let rid = component
            .property("RECURRENCE-ID")?
            .ok_or_else(|| invalid("detached resource contains a master"))?;
        let original = position(rid)?;
        let old = before
            .overrides
            .iter()
            .find(|o| o.original_start == original);
        let new = desired
            .overrides
            .iter()
            .find(|o| o.original_start == original);
        if same_override(old, new) {
            continue;
        }
        let new =
            new.ok_or_else(|| invalid("removing a detached override requires explicit exclusion"))?;
        if let Some(event) = &new.event {
            let inherited = at_position(&before.event, &original)?;
            overlay(
                component,
                Some(old.and_then(|o| o.event.as_ref()).unwrap_or(&inherited)),
                event,
            )?;
            if component
                .value("STATUS")?
                .is_some_and(|s| s.eq_ignore_ascii_case("CANCELLED"))
            {
                component.remove("STATUS");
            }
        } else {
            component.set("STATUS", Some("STATUS:CANCELLED".into()));
        }
        changed = true;
    }
    Ok(changed.then(|| root.render()))
}

pub(super) fn create(desired: &CalendarEventSet, uid: &str, marker: &str) -> Result<String> {
    if desired
        .native
        .as_ref()
        .is_none_or(|n| n.protocol != "caldav")
    {
        desired.validate()?;
    }
    let mut root = if let Some(native) = desired.native.as_ref().filter(|n| n.protocol == "caldav")
    {
        let mut sources = vec![native.clone()];
        for item in &desired.overrides {
            if let Some(n) = item.native.as_ref().filter(|n| n.protocol == "caldav") {
                if !sources.iter().any(|s| s.event_id == n.event_id) {
                    sources.push(n.clone());
                }
            }
        }
        let mut current = combine(&sources, &desired.event)?;
        // The export target is one resource. Merge complete native exceptions first,
        // then apply only semantic changes, retaining detached alarms and extensions.
        let mut merged = Component::parse(&native.data)?;
        for source in sources.iter().skip(1) {
            for entry in Component::parse(&source.data)?.entries {
                match &entry {
                    Entry::Component(c) if c.name.eq_ignore_ascii_case("VEVENT") => merged.entries.push(entry),
                    Entry::Component(c) if c.name.eq_ignore_ascii_case("VTIMEZONE")
                        && !merged.entries.iter().any(|e| matches!(e, Entry::Component(other) if other.render() == c.render())) => { merged.entries.push(entry); }
                    _ => {},
                }
            }
        }
        current
            .native
            .as_mut()
            .ok_or_else(|| invalid("missing combined source"))?
            .data = merged.render();
        Component::parse(&rewrite(&current, desired)?)?
    } else {
        let mut root = Component::parse(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Chithi//EN\r\nEND:VCALENDAR\r\n",
        )?;
        let mut master = Component {
            name: "VEVENT".into(),
            entries: Vec::new(),
        };
        master.set("UID", Some(format!("UID:{}", escape(uid))));
        master.set(
            "DTSTAMP",
            Some(format!("DTSTAMP:{}", Utc::now().format("%Y%m%dT%H%M%SZ"))),
        );
        overlay(&mut master, None, &desired.event)?;
        let stamp = master.property("DTSTAMP")?.map(str::to_owned);
        for item in &desired.overrides {
            let Some(event) = &item.event else {
                master.entries.push(Entry::Property(date_property(
                    "EXDATE",
                    &item.original_start,
                    desired.event.all_day,
                    desired.event.timezone.as_deref(),
                )?));
                continue;
            };
            if crate::calendar::simple_recurrence::resolve(&desired.event, &item.original_start)
                .is_err()
            {
                master.entries.push(Entry::Property(date_property(
                    "RDATE",
                    &item.original_start,
                    desired.event.all_day,
                    desired.event.timezone.as_deref(),
                )?));
            }
            let mut component = Component {
                name: "VEVENT".into(),
                entries: Vec::new(),
            };
            component.set("UID", Some(format!("UID:{}", escape(uid))));
            component.set("DTSTAMP", stamp.clone());
            component.set(
                "RECURRENCE-ID",
                Some(date_property(
                    "RECURRENCE-ID",
                    &item.original_start,
                    desired.event.all_day,
                    desired.event.timezone.as_deref(),
                )?),
            );
            overlay(&mut component, None, event)?;
            root.entries.push(Entry::Component(component));
        }
        root.entries.push(Entry::Component(master));
        root
    };
    root.remove("METHOD");
    for entry in &mut root.entries {
        if let Entry::Component(c) = entry {
            if c.name.eq_ignore_ascii_case("VEVENT") {
                c.set("UID", Some(format!("UID:{}", escape(uid))));
                c.set(
                    "X-CHITHI-OPERATION-ID",
                    Some(format!("X-CHITHI-OPERATION-ID:{marker}")),
                );
            }
        }
    }
    ensure_timezones(&mut root)?;
    Ok(root.render())
}

pub(super) fn verify_operation(raw: &str, uid: &str, marker: &str) -> Result<()> {
    let root = Component::parse(raw)?;
    if root.events().next().is_none() {
        return Err(invalid("empty retry resource"));
    }
    for event in root.events() {
        if event.value("UID")? != Some(uid) || event.value("X-CHITHI-OPERATION-ID")? != Some(marker)
        {
            return Err(invalid(
                "create collision: existing UID/operation marker does not match",
            ));
        }
    }
    Ok(())
}

pub(super) fn rewrite_occurrence(
    request: &crate::backend::calendar::RemoteOccurrenceUpdate,
) -> Result<String> {
    let identity = &request.trusted_identity;
    let value_type = if identity.recurrence_value_type
        == Some(crate::calendar::recurrence_identity::RecurrenceValueType::Date)
    {
        ";VALUE=DATE"
    } else {
        ""
    };
    let timezone = identity
        .recurrence_timezone
        .as_ref()
        .map(|tz| format!(";TZID={tz}"))
        .unwrap_or_default();
    let original = position(&format!(
        "RECURRENCE-ID{value_type}{timezone}:{}",
        identity
            .recurrence_id
            .as_deref()
            .ok_or_else(|| invalid("missing original identity"))?
    ))?;
    let native = NativeCalendarResource {
        protocol: "caldav".into(),
        calendar_id: identity.provider_calendar_id.clone().unwrap_or_default(),
        event_id: request.target_id.clone(),
        revision: request.expected_provider_revision.clone(),
        data: identity
            .provider_native_data
            .clone()
            .ok_or_else(|| invalid("missing source"))?,
    };
    let before = snapshot(native, &request.current_event)?;
    // The legacy sparse-patch API retains its established embedded writer. The
    // event-set writer handles materialization, which that API cannot represent.
    if before
        .overrides
        .iter()
        .any(|item| item.original_start == original && item.native.is_some())
    {
        return crate::calendar::ical::rewrite_recurrence_occurrence(
            identity
                .provider_native_data
                .as_deref()
                .ok_or_else(|| invalid("missing source"))?,
            request
                .current_event
                .uid
                .as_deref()
                .ok_or_else(|| invalid("missing UID"))?,
            identity
                .recurrence_id
                .as_deref()
                .ok_or_else(|| invalid("missing original position"))?,
            identity
                .recurrence_value_type
                .ok_or_else(|| invalid("missing original type"))?,
            identity.recurrence_timezone.as_deref(),
            &request.patch,
            &request.desired,
        )
        .map_err(Error::Other);
    }
    let mut desired = before.clone();
    let mut effective = desired
        .overrides
        .iter()
        .find(|o| o.original_start == original)
        .and_then(|o| o.event.clone())
        .unwrap_or(at_position(&before.event, &original)?);
    crate::calendar::event_set::apply_event_fields(&mut effective, &request.desired);
    desired.overrides.retain(|o| o.original_start != original);
    desired.overrides.push(CalendarOverride {
        original_start: original,
        event: Some(effective),
        native: None,
    });
    rewrite(&before, &desired)
}
