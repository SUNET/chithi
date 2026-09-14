use std::borrow::Cow;

use mail_parser::{MessageParser, MimeHeaders};
use serde::{Deserialize, Serialize};

use super::{Attendee, RecurrenceKind};

/// A parsed calendar invite extracted from an email or raw iCalendar text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedInvite {
    pub method: String, // REQUEST, REPLY, CANCEL
    pub uid: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub dtstart: String, // ISO 8601
    pub dtend: String,   // ISO 8601
    pub all_day: bool,
    pub timezone: Option<String>,
    pub organizer_email: Option<String>,
    pub organizer_name: Option<String>,
    pub attendees: Vec<Attendee>,
    pub recurrence_rule: Option<String>,
    #[serde(default)]
    pub recurrence_kind: RecurrenceKind,
    pub sequence: u32,
    #[serde(skip_serializing)]
    pub ical_raw: String, // Original iCalendar text
}

/// One logical event from an iCalendar resource. A recurring master and all
/// of its RECURRENCE-ID exceptions share a UID and therefore stay together.
#[derive(Debug, Clone)]
pub struct IcalEventGroup {
    pub representative: ParsedInvite,
    pub component_count: usize,
    /// A self-contained VCALENDAR containing this UID and its timezones.
    /// Scheduling properties are removed so importing creates a personal
    /// calendar copy rather than sending invitations to the original guests.
    pub ical_raw: String,
}

/// Whether a calendar resource is one recurring master whose complete
/// recurrence set is represented by exactly one RRULE and no exceptions or
/// additional-date properties.
pub fn is_rrule_only_series(ical_text: &str) -> bool {
    let Ok(parts) = split_calendar_components(ical_text) else {
        return false;
    };
    let Some(event) = parts.events.first().filter(|_| parts.events.len() == 1) else {
        return false;
    };
    let mut depth = 0usize;
    let mut rrules = 0usize;
    for line in event {
        let line = line.trim();
        let current_depth = depth;
        if component_marker(line, "BEGIN").is_some() {
            depth += 1;
        }
        if current_depth == 1 {
            match property_name(line).map(str::to_ascii_uppercase).as_deref() {
                Some("RRULE") => rrules += 1,
                Some("RECURRENCE-ID" | "RDATE" | "EXDATE" | "EXRULE") => return false,
                _ => {}
            }
        }
        if component_marker(line, "END").is_some() {
            depth = depth.saturating_sub(1);
        }
    }
    rrules == 1
}

/// Split an iCalendar resource into logical events grouped by UID.
///
/// The structured parser remains the authority for event identity and field
/// values. A small component splitter is used only to retain each group's raw
/// VEVENTs and shared VTIMEZONE definitions for recurrence fidelity.
pub fn parse_ical_event_groups(ical_text: &str) -> Result<Vec<IcalEventGroup>, String> {
    let invites = parse_ical_data(ical_text);
    if invites.is_empty() {
        return Err("The attachment does not contain any valid events".into());
    }

    let parts = split_calendar_components(ical_text)?;
    if parts.events.len() != invites.len() {
        return Err("The calendar event structure could not be matched safely".into());
    }

    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (index, invite) in invites.iter().enumerate() {
        if let Some((_, indices)) = groups.iter_mut().find(|(uid, _)| uid == &invite.uid) {
            indices.push(index);
        } else {
            groups.push((invite.uid.clone(), vec![index]));
        }
    }

    groups
        .into_iter()
        .map(|(uid, indices)| {
            let representative_index = indices
                .iter()
                .copied()
                .find(|index| invites[*index].recurrence_kind != RecurrenceKind::Occurrence)
                .unwrap_or(indices[0]);
            let raw = build_personal_calendar(&parts, &indices);
            let reparsed = parse_ical_data(&raw);
            if reparsed.len() != indices.len() || reparsed.iter().any(|event| event.uid != uid) {
                return Err(format!(
                    "The calendar event with UID '{uid}' could not be isolated safely"
                ));
            }
            Ok(IcalEventGroup {
                representative: invites[representative_index].clone(),
                component_count: indices.len(),
                ical_raw: raw,
            })
        })
        .collect()
}

#[derive(Debug)]
struct SplitCalendar {
    properties: Vec<String>,
    timezones: Vec<Vec<String>>,
    events: Vec<Vec<String>>,
}

fn split_calendar_components(ical_text: &str) -> Result<SplitCalendar, String> {
    let normalized = ical_text
        .trim_start_matches('\u{feff}')
        .replace("\r\n ", "")
        .replace("\r\n\t", "")
        .replace("\r\n", "\n")
        .replace("\n ", "")
        .replace("\n\t", "");
    let lines: Vec<String> = normalized.lines().map(str::to_string).collect();
    let first = lines
        .iter()
        .position(|line| !line.trim().is_empty())
        .ok_or_else(|| "The calendar attachment is empty".to_string())?;
    if !lines[first].trim().eq_ignore_ascii_case("BEGIN:VCALENDAR") {
        return Err("The attachment is not a VCALENDAR resource".into());
    }

    let mut properties = Vec::new();
    let mut timezones = Vec::new();
    let mut events = Vec::new();
    let mut index = first + 1;
    let mut found_end = false;
    while index < lines.len() {
        let line = lines[index].trim();
        if line.eq_ignore_ascii_case("END:VCALENDAR") {
            found_end = true;
            index += 1;
            break;
        }
        if let Some(name) = component_marker(line, "BEGIN") {
            let (block, next) = take_component(&lines, index)?;
            if name.eq_ignore_ascii_case("VEVENT") {
                events.push(block);
            } else if name.eq_ignore_ascii_case("VTIMEZONE") {
                timezones.push(block);
            }
            index = next;
            continue;
        }
        if property_name(line).is_some_and(|name| !name.eq_ignore_ascii_case("METHOD")) {
            properties.push(lines[index].clone());
        }
        index += 1;
    }

    if !found_end || lines[index..].iter().any(|line| !line.trim().is_empty()) || events.is_empty()
    {
        return Err("The attachment must contain one complete VCALENDAR resource".into());
    }
    Ok(SplitCalendar {
        properties,
        timezones,
        events,
    })
}

fn take_component(lines: &[String], start: usize) -> Result<(Vec<String>, usize), String> {
    let mut depth = 0usize;
    for index in start..lines.len() {
        let line = lines[index].trim();
        if component_marker(line, "BEGIN").is_some() {
            depth += 1;
        } else if component_marker(line, "END").is_some() {
            depth = depth
                .checked_sub(1)
                .ok_or_else(|| "The calendar contains an unmatched END component".to_string())?;
            if depth == 0 {
                return Ok((lines[start..=index].to_vec(), index + 1));
            }
        }
    }
    Err("The calendar contains an unterminated component".into())
}

fn component_marker<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let (prefix, name) = line.split_once(':')?;
    prefix.eq_ignore_ascii_case(marker).then_some(name)
}

fn property_name(line: &str) -> Option<&str> {
    let end = line.find([';', ':'])?;
    Some(&line[..end])
}

fn strip_event_scheduling(block: &[String]) -> Vec<String> {
    let mut depth = 0usize;
    block
        .iter()
        .filter_map(|line| {
            let trimmed = line.trim();
            let current_depth = depth;
            if component_marker(trimmed, "BEGIN").is_some() {
                depth += 1;
            }
            let scheduling_property = current_depth == 1
                && property_name(trimmed).is_some_and(|name| {
                    name.eq_ignore_ascii_case("ORGANIZER") || name.eq_ignore_ascii_case("ATTENDEE")
                });
            if component_marker(trimmed, "END").is_some() {
                depth = depth.saturating_sub(1);
            }
            (!scheduling_property).then(|| line.clone())
        })
        .collect()
}

fn build_personal_calendar(parts: &SplitCalendar, event_indices: &[usize]) -> String {
    let mut lines = vec!["BEGIN:VCALENDAR".to_string()];
    lines.extend(parts.properties.iter().cloned());
    for timezone in &parts.timezones {
        lines.extend(timezone.iter().cloned());
    }
    for index in event_indices {
        lines.extend(strip_event_scheduling(&parts.events[*index]));
    }
    lines.push("END:VCALENDAR".to_string());
    format!("{}\r\n", lines.join("\r\n"))
}

/// Parse calendar invites from a raw RFC 5322 email message.
///
/// Scans all MIME parts for `text/calendar` content type, then parses each
/// iCalendar body to extract VEVENT components.
pub fn parse_ical_from_email(raw_message: &[u8]) -> Vec<ParsedInvite> {
    let Some(parsed) = MessageParser::default().parse(raw_message) else {
        log::debug!("parse_ical_from_email: failed to parse message");
        return vec![];
    };

    let mut invites = Vec::new();

    // Walk all MIME parts looking for text/calendar
    for (idx, part) in parsed.parts.iter().enumerate() {
        let content_type = part.content_type();
        let is_calendar = content_type
            .map(|ct| {
                ct.ctype() == "text" && ct.subtype().map(|s| s == "calendar").unwrap_or(false)
            })
            .unwrap_or(false);

        if !is_calendar {
            continue;
        }

        // Extract the text body of this part
        let body_text = match &part.body {
            mail_parser::PartType::Text(text) => text.to_string(),
            mail_parser::PartType::Binary(bin) | mail_parser::PartType::InlineBinary(bin) => {
                String::from_utf8_lossy(bin.as_ref()).to_string()
            }
            _ => {
                log::debug!(
                    "parse_ical_from_email: part {} has calendar content-type but unexpected body type",
                    idx
                );
                continue;
            }
        };

        log::debug!(
            "parse_ical_from_email: found text/calendar part ({} bytes)",
            body_text.len()
        );

        let mut parsed_invites = parse_ical_data(&body_text);
        invites.append(&mut parsed_invites);
    }

    invites
}

/// Parse raw iCalendar text and extract all VEVENT components as `ParsedInvite`s.
pub fn parse_ical_data(ical_text: &str) -> Vec<ParsedInvite> {
    let mut invites = Vec::new();

    // Normalize line endings and unfold continuation lines.
    // Microsoft Exchange sends \r\n (CRLF) with RFC 5545 line folding
    // (long lines split with CRLF + space/tab). Radicale and many
    // Thunderbird-exported calendars emit LF-only line endings with
    // LF-based folds. The icalendar parser handles neither, so we
    // unfold both forms — CRLF folds first, then anything remaining
    // after CRLF→LF normalization is treated as LF folding.
    let normalized = ical_text
        .replace("\r\n ", "") // Unfold CRLF + space
        .replace("\r\n\t", "") // Unfold CRLF + tab
        .replace("\r\n", "\n") // Normalize remaining CRLF to LF
        .replace("\n ", "") // Unfold LF + space
        .replace("\n\t", ""); // Unfold LF + tab

    // Exchange/Outlook emit DESCRIPTION;ALTREP="data:text/html,...":plain text
    // where the quoted value contains raw unescaped " chars (RFC-violating but
    // common in the wild). The strict icalendar parser fails with
    // "Satisfy at: BEGIN:VEVENT" when it encounters this. We don't use ALTREP,
    // so strip the parameter entirely before parsing.
    let sanitized = strip_altrep_params(&normalized);
    let trustworthy_syntax = sanitized == normalized
        || icalendar::parser::read_calendar_simple(&normalized)
            .is_ok_and(|components| !components.is_empty());
    let normalized = sanitized;

    // Use the icalendar parser to get structured components
    let components = match icalendar::parser::read_calendar_simple(&normalized) {
        Ok(components) => components,
        Err(e) => {
            log::error!("parse_ical_data: failed to parse iCalendar: {}", e);
            return invites;
        }
    };

    // The top-level component is VCALENDAR; METHOD is a property on it.
    // read_calendar_simple returns a Vec<Component>, each typically being a VCALENDAR.
    for vcal in &components {
        if vcal.name.as_str() != "VCALENDAR" {
            continue;
        }

        let method = find_property_value(vcal, "METHOD").unwrap_or_else(|| "REQUEST".to_string());
        // Every row retains the entire resource, including sibling components.
        // A row without an RRULE is not necessarily independently mutable.
        let single_resource = components.len() == 1
            && trustworthy_syntax
            && normalized
                .lines()
                .all(|line| line.is_empty() || property_value_separator(line).is_some())
            && single_property(vcal, "VERSION").is_some_and(|p| p.val.as_str() == "2.0")
            && single_property(vcal, "PRODID").is_some_and(|p| !p.val.as_str().trim().is_empty())
            && vcal.components.iter().all(|c| {
                c.name.as_str() == "VEVENT"
                    || (c.name.as_str() == "VTIMEZONE"
                        && c.components.iter().all(|observance| {
                            matches!(observance.name.as_str(), "STANDARD" | "DAYLIGHT")
                                && observance.components.is_empty()
                        }))
            });
        let event_count = vcal
            .components
            .iter()
            .filter(|c| c.name.as_str() == "VEVENT")
            .count();

        // Look for VEVENT sub-components
        for vevent in &vcal.components {
            if vevent.name.as_str() != "VEVENT" {
                continue;
            }

            let uid = match find_property_value(vevent, "UID") {
                Some(uid) => uid,
                None => {
                    log::debug!("parse_ical_data: VEVENT missing UID, skipping");
                    continue;
                }
            };

            let summary = find_property_value(vevent, "SUMMARY");
            let description = find_property_value(vevent, "DESCRIPTION");
            let location = find_property_value(vevent, "LOCATION");
            let recurrence_rule = find_property_value(vevent, "RRULE");

            let sequence_str = find_property_value(vevent, "SEQUENCE");
            let sequence: u32 = sequence_str.and_then(|s| s.parse().ok()).unwrap_or(0);

            // Parse DTSTART and DTEND
            let (dtstart, all_day, timezone) = parse_dt_property(vevent, "DTSTART");
            let (dtend, _, _) = parse_dt_property(vevent, "DTEND");

            // If DTEND is missing, try DURATION and compute dtend from dtstart
            let dtend = if dtend.is_empty() {
                if let Some(duration_str) = find_property_value(vevent, "DURATION") {
                    compute_end_from_duration(&dtstart, &duration_str)
                } else {
                    dtstart.clone()
                }
            } else {
                dtend
            };

            // Parse ORGANIZER
            let (organizer_email, organizer_name) = parse_organizer(vevent);

            // Parse ATTENDEEs
            let attendees = parse_attendees(vevent);

            invites.push(ParsedInvite {
                method: method.clone(),
                uid,
                summary,
                description,
                location,
                dtstart,
                dtend,
                all_day,
                timezone,
                organizer_email,
                organizer_name,
                attendees,
                recurrence_rule,
                recurrence_kind: classify_vevent(vevent, single_resource && event_count == 1),
                sequence,
                ical_raw: ical_text.to_string(),
            });
        }
    }

    invites
}

/// Find exactly one property, including case variants (RFC 5545 §3.1).
fn single_property<'a, 'i>(
    component: &'a icalendar::parser::Component<'i>,
    name: &str,
) -> Option<&'a icalendar::parser::Property<'i>> {
    let mut properties = component
        .properties
        .iter()
        .filter(|p| p.name.as_str().eq_ignore_ascii_case(name));
    let property = properties.next()?;
    if properties.next().is_some() {
        None
    } else {
        Some(property)
    }
}

fn has_property(component: &icalendar::parser::Component<'_>, name: &str) -> bool {
    component
        .properties
        .iter()
        .any(|p| p.name.as_str().eq_ignore_ascii_case(name))
}

/// Recognize recurrence evidence without claiming the local expander supports it.
fn classify_vevent(
    event: &icalendar::parser::Component<'_>,
    single_event_resource: bool,
) -> RecurrenceKind {
    if has_property(event, "RECURRENCE-ID") {
        return match single_property(event, "RECURRENCE-ID") {
            Some(property) if valid_ical_date_property(property) => RecurrenceKind::Occurrence,
            _ => RecurrenceKind::Unknown,
        };
    }

    // EXRULE is obsolete but is still recurrence evidence in older resources.
    let mut recurring = false;
    for property in &event.properties {
        let name = property.name.as_str().to_ascii_uppercase();
        if matches!(name.as_str(), "RRULE" | "RDATE" | "EXDATE" | "EXRULE") {
            if property.val.as_str().trim().is_empty()
                || property
                    .params
                    .iter()
                    .any(|p| p.val.as_ref().is_none_or(|v| v.as_str().is_empty()))
            {
                return RecurrenceKind::Unknown;
            }
            recurring = true;
        }
    }
    if recurring {
        return RecurrenceKind::Series;
    }

    // Missing mandatory metadata is not proof of a complete, standalone event.
    // VTIMEZONE and VALARM are metadata, not additional independently stored events.
    let complete_event = single_property(event, "UID")
        .is_some_and(|p| !p.val.as_str().trim().is_empty())
        && single_property(event, "DTSTART").is_some_and(valid_ical_date_property)
        && single_property(event, "DTSTAMP")
            .is_some_and(|p| p.val.as_str().ends_with('Z') && valid_ical_date_property(p))
        && (!has_property(event, "DTEND")
            || single_property(event, "DTEND").is_some_and(valid_ical_date_property))
        && !(has_property(event, "DTEND") && has_property(event, "DURATION"))
        && (!has_property(event, "DURATION")
            || single_property(event, "DURATION")
                .is_some_and(|p| valid_duration_value(p.val.as_str())))
        && event
            .components
            .iter()
            .all(|c| c.name.as_str() == "VALARM" && c.components.is_empty());
    if single_event_resource && complete_event {
        RecurrenceKind::Standalone
    } else {
        RecurrenceKind::Unknown
    }
}

/// Validate DATE/DATE-TIME syntax and parameters before trusting an instance id.
/// Omitted VALUE means DATE-TIME; a date-only value must explicitly say DATE.
fn valid_ical_date_property(property: &icalendar::parser::Property<'_>) -> bool {
    let mut value_type = None;
    let mut timezone = None;
    let mut range = None;
    for parameter in &property.params {
        let Some(value) = parameter
            .val
            .as_ref()
            .map(|v| v.as_str())
            .filter(|v| !v.is_empty())
        else {
            return false;
        };
        let slot = match parameter.key.as_str().to_ascii_uppercase().as_str() {
            "VALUE" => &mut value_type,
            "TZID" => &mut timezone,
            "RANGE" => &mut range,
            _ => continue,
        };
        if slot.replace(value).is_some() {
            return false;
        }
    }
    if range.is_some_and(|value| !value.eq_ignore_ascii_case("THISANDFUTURE")) {
        return false;
    }
    let value = property.val.as_str();
    if !value.is_ascii() {
        return false;
    }
    match value_type
        .unwrap_or("DATE-TIME")
        .to_ascii_uppercase()
        .as_str()
    {
        "DATE" => {
            timezone.is_none()
                && value.len() == 8
                && value.bytes().all(|byte| byte.is_ascii_digit())
                && chrono::NaiveDate::parse_from_str(value, "%Y%m%d").is_ok()
        }
        "DATE-TIME" => {
            let local = value.strip_suffix('Z').unwrap_or(value);
            !(value.ends_with('Z') && timezone.is_some())
                && local.len() == 15
                && local.bytes().enumerate().all(|(i, byte)| {
                    if i == 8 {
                        byte == b'T'
                    } else {
                        byte.is_ascii_digit()
                    }
                })
                && chrono::NaiveDateTime::parse_from_str(local, "%Y%m%dT%H%M%S").is_ok()
        }
        _ => false,
    }
}

/// Validate a positive RFC 5545 duration without relying on the tolerant viewer.
fn valid_duration_value(value: &str) -> bool {
    let Some(mut rest) = value.strip_prefix('+').unwrap_or(value).strip_prefix('P') else {
        return false;
    };
    if let Some(weeks) = rest.strip_suffix('W') {
        return !weeks.is_empty() && weeks.bytes().all(|byte| byte.is_ascii_digit());
    }
    let mut any = false;
    for unit in ['D', 'T', 'H', 'M', 'S'] {
        if unit == 'T' {
            if rest.is_empty() {
                return any;
            }
            let Some(time) = rest.strip_prefix('T') else {
                return false;
            };
            rest = time;
            any = false;
            continue;
        }
        let digits = rest
            .bytes()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits > 0 && rest.as_bytes().get(digits) == Some(&(unit as u8)) {
            rest = &rest[digits + 1..];
            any = true;
        }
    }
    any && rest.is_empty()
}

/// Generate an iTIP REPLY iCalendar for responding to an invite.
///
/// `response` should be one of: "ACCEPTED", "TENTATIVE", "DECLINED".
pub fn generate_reply(
    invite: &ParsedInvite,
    user_email: &str,
    user_name: Option<&str>,
    response: &str,
) -> String {
    let partstat = response.to_uppercase();
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");

    // Build the REPLY iCalendar manually for full control over iTIP format
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//Chithi//EN".to_string(),
        "METHOD:REPLY".to_string(),
    ];
    lines.push("BEGIN:VEVENT".to_string());

    // Preserve organizer from original invite
    if let Some(ref org_email) = invite.organizer_email {
        if let Some(ref org_name) = invite.organizer_name {
            lines.push(format!(
                "ORGANIZER;CN={}:mailto:{}",
                ical_parameter_value(org_name),
                org_email
            ));
        } else {
            lines.push(format!("ORGANIZER:mailto:{}", org_email));
        }
    }

    // Add the replying attendee with their response
    let attendee_name = user_name
        .filter(|name| !name.trim().is_empty())
        .map(|name| format!(";CN={}", ical_parameter_value(name.trim())))
        .unwrap_or_default();
    lines.push(format!(
        "ATTENDEE;PARTSTAT={};RSVP=FALSE{}:mailto:{}",
        partstat, attendee_name, user_email
    ));

    lines.push(format!("UID:{}", invite.uid));
    if let Some(ref summary) = invite.summary {
        lines.push(format!("SUMMARY:{}", summary));
    }
    if !invite.all_day {
        if let Some(ref tz) = invite.timezone {
            let local_start = crate::mail::caldav::utc_to_local(&invite.dtstart, tz);
            let local_end = crate::mail::caldav::utc_to_local(&invite.dtend, tz);
            lines.push(format!(
                "DTSTART;TZID={}:{}",
                tz,
                to_ical_datetime(&local_start)
            ));
            lines.push(format!(
                "DTEND;TZID={}:{}",
                tz,
                to_ical_datetime(&local_end)
            ));
        } else {
            lines.push(format!("DTSTART:{}", to_ical_datetime(&invite.dtstart)));
            lines.push(format!("DTEND:{}", to_ical_datetime(&invite.dtend)));
        }
    } else {
        lines.push(format!(
            "DTSTART;VALUE=DATE:{}",
            invite.dtstart.split('T').next().unwrap_or(&invite.dtstart)
        ));
        lines.push(format!(
            "DTEND;VALUE=DATE:{}",
            invite.dtend.split('T').next().unwrap_or(&invite.dtend)
        ));
    }
    lines.push(format!("SEQUENCE:{}", invite.sequence));
    lines.push(format!("DTSTAMP:{}", now));
    lines.push("END:VEVENT".to_string());
    lines.push("END:VCALENDAR".to_string());

    lines.join("\r\n")
}

/// Generate a METHOD:REQUEST iCalendar for inviting attendees to an event.
pub fn generate_invite(
    uid: &str,
    summary: &str,
    dtstart: &str,
    dtend: &str,
    location: Option<&str>,
    description: Option<&str>,
    organizer_email: &str,
    organizer_name: Option<&str>,
    attendees: &[Attendee],
    recurrence_rule: Option<&str>,
    timezone: Option<&str>,
) -> String {
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");

    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//Chithi//EN".to_string(),
        "METHOD:REQUEST".to_string(),
        "BEGIN:VEVENT".to_string(),
    ];

    // Organizer
    if let Some(name) = organizer_name {
        lines.push(format!(
            "ORGANIZER;CN={}:mailto:{}",
            ical_parameter_value(name),
            organizer_email
        ));
    } else {
        lines.push(format!("ORGANIZER:mailto:{}", organizer_email));
    }

    // Attendees
    for attendee in attendees {
        let cn = attendee
            .name
            .as_ref()
            .map(|name| format!(";CN={}", ical_parameter_value(name)))
            .unwrap_or_default();
        lines.push(format!(
            "ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE{}:mailto:{}",
            cn, attendee.email
        ));
    }

    lines.push(format!("UID:{}", uid));
    lines.push(format!("SUMMARY:{}", summary));
    if let Some(tz) = timezone {
        let local_start = crate::mail::caldav::utc_to_local(dtstart, tz);
        let local_end = crate::mail::caldav::utc_to_local(dtend, tz);
        lines.push(format!(
            "DTSTART;TZID={}:{}",
            tz,
            to_ical_datetime(&local_start)
        ));
        lines.push(format!(
            "DTEND;TZID={}:{}",
            tz,
            to_ical_datetime(&local_end)
        ));
    } else {
        lines.push(format!("DTSTART:{}", to_ical_datetime(dtstart)));
        lines.push(format!("DTEND:{}", to_ical_datetime(dtend)));
    }

    if let Some(loc) = location {
        lines.push(format!("LOCATION:{}", loc));
    }
    if let Some(desc) = description {
        lines.push(format!("DESCRIPTION:{}", desc));
    }
    if let Some(rrule) = recurrence_rule {
        lines.push(format!("RRULE:{}", rrule));
    }

    lines.push("SEQUENCE:0".to_string());
    lines.push(format!("DTSTAMP:{}", now));
    lines.push("STATUS:CONFIRMED".to_string());
    lines.push("END:VEVENT".to_string());
    lines.push("END:VCALENDAR".to_string());

    lines.join("\r\n")
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Encode an RFC 5545 parameter value as a quoted string. RFC 6868 caret
/// encoding keeps quotes, carets, and newlines inside the value instead of
/// allowing a user-provided display name to inject another iCalendar property.
fn ical_parameter_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() + 2);
    let mut chars = value.trim().chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '^' => encoded.push_str("^^"),
            '"' => encoded.push_str("^'"),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                encoded.push_str("^n");
            }
            '\n' => encoded.push_str("^n"),
            ch if ch.is_control() => encoded.push(' '),
            ch => encoded.push(ch),
        }
    }
    format!("\"{encoded}\"")
}

/// Remove `;ALTREP=...` parameters from every property line.
///
/// Real-world Exchange/Outlook invites embed raw unescaped `"` characters
/// inside `ALTREP="data:text/html,..."` (HTML attributes like `class="foo"`).
/// This violates RFC 5545 §3.1 (`QSAFE-CHAR` excludes `"`) and causes strict
/// parsers to reject the whole VEVENT. We don't use the ALTREP value, so it's
/// simplest to drop the parameter before parsing.
fn strip_altrep_params(ical: &str) -> String {
    let mut out = String::with_capacity(ical.len());
    for line in ical.split_inclusive('\n') {
        out.push_str(&strip_altrep_from_line(line));
    }
    out
}

fn strip_altrep_from_line(line: &str) -> Cow<'_, str> {
    // Only consider the parameter region (before the real property `:`
    // separator). This keeps a literal `;ALTREP=` that happens to appear
    // inside a property VALUE from being clobbered.
    let search_end = property_value_separator(line).unwrap_or(line.len());
    let bytes = line.as_bytes();
    let Some(start) = find_altrep_param(&bytes[..search_end]) else {
        return Cow::Borrowed(line);
    };
    let after_eq = start + b";ALTREP=".len();
    let end = altrep_value_end(bytes, after_eq);
    let mut result = String::with_capacity(line.len());
    result.push_str(&line[..start]);
    result.push_str(&line[end..]);
    Cow::Owned(result)
}

/// Case-insensitive search for `;ALTREP=` in a byte slice. Avoids the
/// per-line lowercasing allocation on the common (no-ALTREP) path.
fn find_altrep_param(haystack: &[u8]) -> Option<usize> {
    const NEEDLE: &[u8] = b";altrep=";
    if haystack.len() < NEEDLE.len() {
        return None;
    }
    haystack
        .windows(NEEDLE.len())
        .position(|w| w.iter().zip(NEEDLE).all(|(a, b)| a.eq_ignore_ascii_case(b)))
}

/// Find the offset of the real property/value `:` separator on a content line.
///
/// Parameter values may be quoted; colons inside quotes don't count. This scan
/// tolerates Exchange/Outlook's malformed quoted values that contain raw `"`
/// chars by only treating a `"` as the closing quote when the next byte is
/// `;`, `:`, `\r`, `\n`, or end-of-input.
fn property_value_separator(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_quotes = false;

    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                if in_quotes {
                    let next = bytes.get(i + 1).copied();
                    if matches!(next, Some(b';' | b':' | b'\r' | b'\n') | None) {
                        in_quotes = false;
                    }
                } else {
                    in_quotes = true;
                }
            }
            b':' if !in_quotes => return Some(i),
            b'\r' | b'\n' if !in_quotes => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Given a line and the offset just after `;ALTREP=`, return the offset where
/// the parameter value ends. Handles both quoted and unquoted forms and
/// tolerates raw `"` inside the quoted value by only treating `"` followed by
/// `;`, `:`, or end-of-line as the real closer.
fn altrep_value_end(bytes: &[u8], after_eq: usize) -> usize {
    if after_eq >= bytes.len() {
        return bytes.len();
    }
    if bytes[after_eq] == b'"' {
        let mut i = after_eq + 1;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => match bytes.get(i + 1) {
                    Some(b';') | Some(b':') | Some(b'\r') | Some(b'\n') | None => {
                        return i + 1;
                    }
                    _ => {}
                },
                b'\n' => return i,
                _ => {}
            }
            i += 1;
        }
        bytes.len()
    } else {
        let mut i = after_eq;
        while i < bytes.len() {
            match bytes[i] {
                b';' | b':' | b'\r' | b'\n' => return i,
                _ => i += 1,
            }
        }
        bytes.len()
    }
}

/// Find a property value by name on a parser component.
fn find_property_value(component: &icalendar::parser::Component<'_>, name: &str) -> Option<String> {
    component
        .find_prop(name)
        .map(|p| p.val.as_str().to_string())
}

/// Parse a DTSTART or DTEND property, extracting the ISO 8601 value,
/// whether it is an all-day date, and the TZID if present.
fn parse_dt_property(
    component: &icalendar::parser::Component<'_>,
    prop_name: &str,
) -> (String, bool, Option<String>) {
    let Some(prop) = component.find_prop(prop_name) else {
        return (String::new(), false, None);
    };

    let raw_val = prop.val.as_str();

    // Check for TZID parameter
    let tzid = prop.params.iter().find_map(|p| {
        if p.key.as_str() == "TZID" {
            p.val.as_ref().map(|v| v.as_str().to_string())
        } else {
            None
        }
    });

    // Check VALUE=DATE parameter (all-day event)
    let value_type = prop.params.iter().find_map(|p| {
        if p.key.as_str() == "VALUE" {
            p.val.as_ref().map(|v| v.as_str().to_string())
        } else {
            None
        }
    });

    let all_day = value_type.as_deref() == Some("DATE");

    // Convert iCal datetime format to ISO 8601
    let iso_datetime = ical_datetime_to_iso(raw_val, all_day, tzid.as_deref());

    (iso_datetime, all_day, tzid)
}

/// Convert an iCalendar date/datetime string to ISO 8601 format.
///
/// Handles formats like:
/// - `20250415` (DATE, all-day) -> `2025-04-15`
/// - `20250415T100000` (local datetime) -> `2025-04-15T10:00:00`
/// - `20250415T100000Z` (UTC datetime) -> `2025-04-15T10:00:00Z`
fn ical_datetime_to_iso(val: &str, all_day: bool, tzid: Option<&str>) -> String {
    let val = val.trim();

    // Invalid source metadata must remain viewable without slicing inside UTF-8.
    if !val.is_ascii() {
        return val.to_string();
    }

    if all_day && val.len() >= 8 {
        return format!("{}-{}-{}", &val[0..4], &val[4..6], &val[6..8]);
    }

    if val.len() >= 15 {
        let utc_suffix = if val.ends_with('Z') { "Z" } else { "" };
        let iso = format!(
            "{}-{}-{}T{}:{}:{}{}",
            &val[0..4],
            &val[4..6],
            &val[6..8],
            &val[9..11],
            &val[11..13],
            &val[13..15],
            utc_suffix,
        );

        // If there's a TZID and the time isn't already UTC, convert to UTC
        if !val.ends_with('Z') {
            let tz_str = tzid.unwrap_or("");
            return crate::calendar::timezone::to_utc(&iso, tz_str);
        }

        return iso;
    }

    val.to_string()
}

/// Convert an ISO 8601 datetime back to iCalendar format for use in REPLY.
fn to_ical_datetime(iso: &str) -> String {
    // Convert ISO 8601 to iCalendar format: "2025-04-15T10:00:00.000Z" -> "20250415T100000Z"
    // Parse with chrono to normalize, then format as iCal UTC.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso) {
        return dt
            .with_timezone(&chrono::Utc)
            .format("%Y%m%dT%H%M%SZ")
            .to_string();
    }
    // Try parsing without timezone (treat as UTC)
    if let Ok(dt) =
        chrono::NaiveDateTime::parse_from_str(iso.trim_end_matches('Z'), "%Y-%m-%dT%H:%M:%S%.f")
    {
        return dt.format("%Y%m%dT%H%M%SZ").to_string();
    }
    if let Ok(dt) =
        chrono::NaiveDateTime::parse_from_str(iso.trim_end_matches('Z'), "%Y-%m-%dT%H:%M:%S")
    {
        return dt.format("%Y%m%dT%H%M%SZ").to_string();
    }
    // Fallback: just strip dashes/colons
    iso.replace(['-', ':'], "").replace(".000", "")
}

/// Parse the ORGANIZER property from a VEVENT.
///
/// ORGANIZER is formatted like:
/// `ORGANIZER;CN=John Doe:mailto:john@example.com`
fn parse_organizer(
    component: &icalendar::parser::Component<'_>,
) -> (Option<String>, Option<String>) {
    let Some(prop) = component.find_prop("ORGANIZER") else {
        return (None, None);
    };

    let raw_val = prop.val.as_str();
    let email = extract_mailto(raw_val);

    let cn = prop.params.iter().find_map(|p| {
        if p.key.as_str() == "CN" {
            p.val.as_ref().map(|v| v.as_str().to_string())
        } else {
            None
        }
    });

    (email, cn)
}

/// Parse all ATTENDEE properties from a VEVENT.
fn parse_attendees(component: &icalendar::parser::Component<'_>) -> Vec<Attendee> {
    let mut attendees = Vec::new();

    for prop in &component.properties {
        if prop.name.as_str() != "ATTENDEE" {
            continue;
        }

        let raw_val = prop.val.as_str();
        let email = match extract_mailto(raw_val) {
            Some(e) => e,
            None => continue,
        };

        let name = prop.params.iter().find_map(|p| {
            if p.key.as_str() == "CN" {
                p.val.as_ref().map(|v| v.as_str().to_string())
            } else {
                None
            }
        });

        let status = prop
            .params
            .iter()
            .find_map(|p| {
                if p.key.as_str() == "PARTSTAT" {
                    p.val.as_ref().map(|v| v.as_str().to_lowercase())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "needs-action".to_string());

        attendees.push(Attendee {
            email,
            name,
            status,
            is_self: None,
        });
    }

    attendees
}

/// Extract the email address from a `mailto:user@example.com` URI.
fn extract_mailto(val: &str) -> Option<String> {
    let lower = val.to_lowercase();
    if let Some(pos) = lower.find("mailto:") {
        Some(val[pos + 7..].trim().to_string())
    } else {
        // Sometimes the value is just the email without mailto:
        if val.contains('@') {
            Some(val.trim().to_string())
        } else {
            None
        }
    }
}

/// Compute an end datetime from a start datetime and an iCalendar DURATION.
///
/// Handles simple durations like PT1H, PT30M, P1D, PT1H30M.
fn compute_end_from_duration(dtstart: &str, duration: &str) -> String {
    // Try to parse dtstart as a chrono DateTime
    if let Ok(start) = chrono::DateTime::parse_from_rfc3339(dtstart) {
        if let Some(dur) = parse_ical_duration(duration) {
            let end = start + dur;
            return end.to_rfc3339();
        }
    }

    // Try parsing without timezone (e.g., "2025-04-15T10:00:00")
    if let Ok(start) = chrono::NaiveDateTime::parse_from_str(dtstart, "%Y-%m-%dT%H:%M:%S") {
        if let Some(dur) = parse_ical_duration(duration) {
            let end = start + dur;
            return end.format("%Y-%m-%dT%H:%M:%S").to_string();
        }
    }

    // Fallback
    dtstart.to_string()
}

/// Parse an iCalendar DURATION value like "PT1H30M", "P1D", "PT45M" into chrono::Duration.
fn parse_ical_duration(duration: &str) -> Option<chrono::Duration> {
    let s = duration.trim();
    if !s.starts_with('P') {
        return None;
    }

    let s = &s[1..]; // strip 'P'
    let mut days = 0i64;
    let mut hours = 0i64;
    let mut minutes = 0i64;
    let mut seconds = 0i64;

    let mut in_time = false;
    let mut num_buf = String::new();

    for ch in s.chars() {
        match ch {
            'T' => {
                in_time = true;
            }
            '0'..='9' => {
                num_buf.push(ch);
            }
            'D' if !in_time => {
                days = num_buf.parse().unwrap_or(0);
                num_buf.clear();
            }
            'W' if !in_time => {
                let weeks: i64 = num_buf.parse().unwrap_or(0);
                days += weeks * 7;
                num_buf.clear();
            }
            'H' if in_time => {
                hours = num_buf.parse().unwrap_or(0);
                num_buf.clear();
            }
            'M' if in_time => {
                minutes = num_buf.parse().unwrap_or(0);
                num_buf.clear();
            }
            'S' if in_time => {
                seconds = num_buf.parse().unwrap_or(0);
                num_buf.clear();
            }
            _ => {}
        }
    }

    Some(
        chrono::Duration::days(days)
            + chrono::Duration::hours(hours)
            + chrono::Duration::minutes(minutes)
            + chrono::Duration::seconds(seconds),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_ical_data(ical_text: &str) -> RecurrenceKind {
        match parse_ical_data(ical_text).as_slice() {
            [invite] => invite.recurrence_kind,
            _ => RecurrenceKind::Unknown,
        }
    }

    fn recurrence_event(uid: &str, properties: &str) -> String {
        format!(
            "BEGIN:VEVENT\nUID:{uid}\nDTSTAMP:20260901T120000Z\n\
             DTSTART:20260913T100000Z\nSUMMARY:Visible event\n{properties}END:VEVENT\n"
        )
    }

    fn recurrence_calendar(components: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//Chithi//EN\n\
             {components}END:VCALENDAR\n"
        )
    }

    #[test]
    fn import_groups_series_by_uid_and_keeps_timezone_without_scheduling() {
        let raw = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Example//EN\r\n\
METHOD:REQUEST\r\n\
BEGIN:VTIMEZONE\r\nTZID:Europe/Stockholm\r\nEND:VTIMEZONE\r\n\
BEGIN:VEVENT\r\nUID:series@example.test\r\nDTSTART:20260914T080000Z\r\n\
DTEND:20260914T090000Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Series\r\n\
ORGANIZER:mailto:owner@example.test\r\n\
ATTENDEE:mailto:guest@example.test\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:single@example.test\r\nDTSTART:20260915T080000Z\r\n\
DTEND:20260915T090000Z\r\nSUMMARY:Single\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:series@example.test\r\n\
RECURRENCE-ID:20260921T080000Z\r\nDTSTART:20260921T100000Z\r\n\
DTEND:20260921T110000Z\r\nSUMMARY:Moved occurrence\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";

        let groups = parse_ical_event_groups(raw).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].representative.uid, "series@example.test");
        assert_eq!(
            groups[0].representative.recurrence_kind,
            RecurrenceKind::Series
        );
        assert_eq!(groups[0].component_count, 2);
        assert_eq!(parse_ical_data(&groups[0].ical_raw).len(), 2);
        assert!(groups[0].ical_raw.contains("BEGIN:VTIMEZONE"));
        assert!(!groups[0].ical_raw.contains("METHOD:"));
        assert!(!groups[0].ical_raw.contains("ORGANIZER:"));
        assert!(!groups[0].ical_raw.contains("ATTENDEE:"));
        assert!(!groups[0].ical_raw.contains("UID:single@example.test"));
        assert_eq!(groups[1].representative.uid, "single@example.test");
        assert_eq!(groups[1].component_count, 1);
    }

    #[test]
    fn import_rejects_multiple_calendar_envelopes() {
        let event = recurrence_event("one", "");
        let raw = format!(
            "{}{}",
            recurrence_calendar(&event),
            recurrence_calendar(&event)
        );
        assert!(parse_ical_event_groups(&raw).is_err());
    }

    #[test]
    fn recurrence_master_and_override_order_never_authorizes_resource_mutation() {
        let master = recurrence_event("shared", "RRULE:FREQ=WEEKLY\n");
        let occurrence = recurrence_event("shared", "RECURRENCE-ID:20260920T100000Z\n");
        for (components, expected) in [
            (
                format!("{master}{occurrence}"),
                [RecurrenceKind::Series, RecurrenceKind::Occurrence],
            ),
            (
                format!("{occurrence}{master}"),
                [RecurrenceKind::Occurrence, RecurrenceKind::Series],
            ),
        ] {
            let raw = recurrence_calendar(&components);
            let invites = parse_ical_data(&raw);
            assert_eq!(invites.len(), 2);
            for (invite, kind) in invites.iter().zip(expected) {
                assert_eq!(invite.recurrence_kind, kind);
                assert_eq!(invite.ical_raw, raw);
                assert_eq!(invite.summary.as_deref(), Some("Visible event"));
            }
            assert_eq!(classify_ical_data(&raw), RecurrenceKind::Unknown);
        }
    }

    #[test]
    fn recurrence_unrelated_or_skipped_components_are_not_standalone() {
        let first = recurrence_event("one", "");
        for sibling in [
            recurrence_event("two", ""),
            recurrence_event("one", ""),
            recurrence_event("one", "RECURRENCE-ID:20260920T100000Z\n"),
            "BEGIN:VEVENT\nSUMMARY:Missing UID\nEND:VEVENT\n".into(),
            "BEGIN:VTODO\nUID:task\nEND:VTODO\n".into(),
            format!(
                "BEGIN:VTIMEZONE\n{}END:VTIMEZONE\n",
                recurrence_event("nested", "")
            ),
        ] {
            let raw = recurrence_calendar(&format!("{first}{sibling}"));
            let parsed = parse_ical_data(&raw);
            assert!(!parsed.is_empty(), "{raw}");
            assert_eq!(parsed[0].recurrence_kind, RecurrenceKind::Unknown, "{raw}");
            assert_eq!(classify_ical_data(&raw), RecurrenceKind::Unknown, "{raw}");
        }
    }

    #[test]
    fn recurrence_sets_without_supported_rrule_still_classify_as_series() {
        for evidence in [
            "RRULE:FREQ=HOURLY;BYMINUTE=15\n",
            "RDATE:20260920T100000Z,20260927T100000Z\n",
            "RDATE;VALUE=PERIOD:20260920T100000Z/PT1H\n",
            "EXDATE:20260920T100000Z\n",
            "EXRULE:FREQ=WEEKLY\n",
            "rdate:20260920T100000Z\n",
        ] {
            let raw = recurrence_calendar(&recurrence_event("series", evidence));
            assert_eq!(
                classify_ical_data(&raw),
                RecurrenceKind::Series,
                "{evidence}"
            );
        }
    }

    #[test]
    fn recurrence_id_validates_date_types_values_and_parameters() {
        for property in [
            "RECURRENCE-ID:20260920T100000Z\n",
            "RECURRENCE-ID;VALUE=DATE-TIME:20260920T100000\n",
            "RECURRENCE-ID;TZID=Europe/Stockholm:20260920T100000\n",
            "RECURRENCE-ID;VALUE=DATE:20260920\n",
            "RECURRENCE-ID;RANGE=THISANDFUTURE:20260920T100000Z\n",
            "recurrence-id;value=date:20260920\n",
        ] {
            let raw = recurrence_calendar(&recurrence_event("instance", property));
            assert_eq!(
                classify_ical_data(&raw),
                RecurrenceKind::Occurrence,
                "{property}"
            );
        }
        for property in [
            "RECURRENCE-ID\n",
            "RECURRENCE-ID:\n",
            "RECURRENCE-ID:20260920\n",
            "RECURRENCE-ID:20260230T100000Z\n",
            "RECURRENCE-ID:20260920T250000Z\n",
            "RECURRENCE-ID:20260920T100000Zjunk\n",
            "RECURRENCE-ID;VALUE:20260920T100000Z\n",
            "RECURRENCE-ID;VALUE=:20260920T100000Z\n",
            "RECURRENCE-ID;VALUE=TEXT:20260920T100000Z\n",
            "RECURRENCE-ID;VALUE=DATE:20260920T100000Z\n",
            "RECURRENCE-ID;VALUE=DATE-TIME:20260920\n",
            "RECURRENCE-ID;VALUE=DATE;VALUE=DATE:20260920\n",
            "RECURRENCE-ID;TZID=:20260920T100000\n",
            "RECURRENCE-ID;TZID=UTC:20260920T100000Z\n",
            "RECURRENCE-ID;RANGE=INVALID:20260920T100000Z\n",
            "RECURRENCE-ID:20260920T100000Z\nRECURRENCE-ID:20260927T100000Z\n",
        ] {
            let raw = recurrence_calendar(&recurrence_event("instance", property));
            assert_eq!(
                classify_ical_data(&raw),
                RecurrenceKind::Unknown,
                "{property}"
            );
        }
    }

    #[test]
    fn recurrence_incomplete_or_malformed_ics_is_not_standalone() {
        let raw = recurrence_calendar(&recurrence_event("one", ""));
        for malformed in [
            String::new(),
            raw.replace("VERSION:2.0\n", ""),
            raw.replace("PRODID:-//Chithi//EN\n", ""),
            raw.replace("DTSTAMP:20260901T120000Z\n", ""),
            raw.replace("DTSTART:20260913T100000Z\n", ""),
            raw.replace("20260913T100000Z", "20260230T100000Z"),
            raw.replace("20260913T100000Z", "2026💌0913T100000Z"),
            raw.replace("END:VCALENDAR", "END:VEVENT"),
            raw.replace("END:VCALENDAR\n", ""),
            raw.replace("END:VEVENT\n", ""),
            format!("{raw}BEGIN:VEVENT\nUID:truncated"),
            format!("{raw}{raw}"),
        ] {
            assert_eq!(
                classify_ical_data(&malformed),
                RecurrenceKind::Unknown,
                "{malformed}"
            );
        }
        for property in [
            "RRULE:\n",
            "RDATE:\n",
            "EXDATE;VALUE=:\n",
            "UID:duplicate\n",
            "DTSTART:20260914T100000Z\n",
            "DTEND:invalid\n",
            "DURATION:invalid\n",
            "DURATION:PT\n",
            "DURATION:P1DT\n",
            "DTEND:20260913T110000Z\nDURATION:PT1H\n",
        ] {
            let malformed = recurrence_calendar(&recurrence_event("one", property));
            assert_eq!(
                classify_ical_data(&malformed),
                RecurrenceKind::Unknown,
                "{property}"
            );
        }
    }

    #[test]
    fn recurrence_complete_standalone_including_timezone_and_alarm_metadata() {
        for properties in [
            "",
            "DTEND:20260913T110000Z\n",
            "DURATION:PT1H\n",
            "BEGIN:VALARM\nACTION:DISPLAY\nDESCRIPTION:Reminder\nTRIGGER:-PT15M\nEND:VALARM\n",
        ] {
            let event = recurrence_event("one", properties);
            let raw = recurrence_calendar(&format!(
                "BEGIN:VTIMEZONE\nTZID:UTC\nBEGIN:STANDARD\n\
                 DTSTART:19700101T000000\nTZOFFSETFROM:+0000\nTZOFFSETTO:+0000\n\
                 RRULE:FREQ=YEARLY\nEND:STANDARD\nEND:VTIMEZONE\n{event}"
            ));
            assert_eq!(
                classify_ical_data(&raw),
                RecurrenceKind::Standalone,
                "{raw}"
            );
        }
        let raw = recurrence_calendar(&recurrence_event("one", ""))
            .replace("DTSTART:20260913T100000Z", "DTSTART;VALUE=DATE:20260913");
        assert_eq!(classify_ical_data(&raw), RecurrenceKind::Standalone);
    }

    #[test]
    fn test_extract_mailto() {
        assert_eq!(
            extract_mailto("mailto:alice@example.com"),
            Some("alice@example.com".to_string())
        );
        assert_eq!(
            extract_mailto("MAILTO:Bob@Example.com"),
            Some("Bob@Example.com".to_string())
        );
        assert_eq!(
            extract_mailto("alice@example.com"),
            Some("alice@example.com".to_string())
        );
        assert_eq!(extract_mailto("not-an-email"), None);
    }

    #[test]
    fn test_to_ical_datetime_from_rfc3339() {
        assert_eq!(
            to_ical_datetime("2026-04-07T17:00:00.000Z"),
            "20260407T170000Z"
        );
        assert_eq!(to_ical_datetime("2026-04-07T17:00:00Z"), "20260407T170000Z");
    }

    #[test]
    fn test_to_ical_datetime_from_naive() {
        assert_eq!(to_ical_datetime("2026-04-07T17:00:00"), "20260407T170000Z");
    }

    #[test]
    fn test_ical_datetime_to_iso_utc() {
        assert_eq!(
            ical_datetime_to_iso("20260407T170000Z", false, None),
            "2026-04-07T17:00:00Z"
        );
    }

    #[test]
    fn test_ical_datetime_to_iso_local() {
        assert_eq!(
            ical_datetime_to_iso("20260407T170000", false, None),
            "2026-04-07T17:00:00Z"
        );
    }

    #[test]
    fn test_ical_datetime_to_iso_allday() {
        assert_eq!(ical_datetime_to_iso("20260407", true, None), "2026-04-07");
    }

    #[test]
    fn test_ical_datetime_to_iso_with_tzid() {
        assert_eq!(
            ical_datetime_to_iso("20260414T140000", false, Some("Europe/Stockholm")),
            "2026-04-14T12:00:00Z"
        );
    }

    #[test]
    fn test_ical_datetime_to_iso_utc_ignores_tzid() {
        assert_eq!(
            ical_datetime_to_iso("20260414T120000Z", false, Some("Europe/Stockholm")),
            "2026-04-14T12:00:00Z"
        );
    }

    #[test]
    fn test_parse_ical_data_request() {
        let ical = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:test-uid-123\r\n\
SUMMARY:Team Standup\r\n\
DTSTART:20260407T170000Z\r\n\
DTEND:20260407T180000Z\r\n\
LOCATION:Room 42\r\n\
DESCRIPTION:Daily standup meeting\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:bob@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";

        let invites = parse_ical_data(ical);
        assert_eq!(invites.len(), 1);

        let inv = &invites[0];
        assert_eq!(inv.method, "REQUEST");
        assert_eq!(inv.uid, "test-uid-123");
        assert_eq!(inv.summary, Some("Team Standup".to_string()));
        assert_eq!(inv.location, Some("Room 42".to_string()));
        assert_eq!(inv.description, Some("Daily standup meeting".to_string()));
        assert_eq!(inv.organizer_email, Some("alice@example.com".to_string()));
        assert_eq!(inv.organizer_name, Some("Alice".to_string()));
        assert_eq!(inv.attendees.len(), 1);
        assert_eq!(inv.attendees[0].email, "bob@example.com");
        assert_eq!(inv.attendees[0].status, "needs-action");
        assert_eq!(inv.sequence, 0);
        assert!(!inv.all_day);
    }

    #[test]
    fn test_parse_ical_data_with_duration() {
        let ical = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:dur-test\r\n\
SUMMARY:Quick Chat\r\n\
DTSTART:20260407T170000Z\r\n\
DURATION:PT30M\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";

        let invites = parse_ical_data(ical);
        assert_eq!(invites.len(), 1);
        // dtend should be computed from dtstart + duration
        assert!(
            invites[0].dtend.contains("17:30:00"),
            "dtend should be 30min after dtstart, got: {}",
            invites[0].dtend
        );
    }

    #[test]
    fn test_parse_ical_data_allday() {
        let ical = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:allday-test\r\n\
SUMMARY:Holiday\r\n\
DTSTART;VALUE=DATE:20260407\r\n\
DTEND;VALUE=DATE:20260408\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";

        let invites = parse_ical_data(ical);
        assert_eq!(invites.len(), 1);
        assert!(invites[0].all_day);
        assert_eq!(invites[0].dtstart, "2026-04-07");
    }

    #[test]
    fn test_parse_ical_data_skips_missing_uid() {
        let ical = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
SUMMARY:No UID Event\r\n\
DTSTART:20260407T170000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR";

        let invites = parse_ical_data(ical);
        assert_eq!(invites.len(), 0, "Events without UID should be skipped");
    }

    #[test]
    fn test_parse_microsoft_exchange_invite() {
        // Real iCal from Microsoft Exchange Server with CRLF line endings
        // and RFC 5545 folded lines (continuation lines starting with space)
        let ical = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nPRODID:Microsoft Exchange Server 2010\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:UTC\r\nBEGIN:STANDARD\r\nDTSTART:16010101T000000\r\nTZOFFSETFROM:+0000\r\nTZOFFSETTO:+0000\r\nEND:STANDARD\r\nBEGIN:DAYLIGHT\r\nDTSTART:16010101T000000\r\nTZOFFSETFROM:+0000\r\nTZOFFSETTO:+0000\r\nEND:DAYLIGHT\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nORGANIZER;CN=kushal das:mailto:chithiapp@outlook.com\r\nATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE;CN=sdossec@gm\r\n ail.com:mailto:sdossec@gmail.com\r\nDESCRIPTION;LANGUAGE=en-US:This is yo food event.\r\nUID:040000008200E00074C5B7101A82E008000000009B9A10BF48CBDC01000000000000000\r\n 0100000009B9409D22D123D48BA2ABC0BABC17911\r\nSUMMARY;LANGUAGE=en-US:Yo food\r\nDTSTART;TZID=UTC:20260414T170000\r\nDTEND;TZID=UTC:20260414T180000\r\nCLASS:PUBLIC\r\nPRIORITY:5\r\nDTSTAMP:20260413T132342Z\r\nTRANSP:OPAQUE\r\nSTATUS:CONFIRMED\r\nSEQUENCE:0\r\nBEGIN:VALARM\r\nDESCRIPTION:REMINDER\r\nTRIGGER;RELATED=START:-PT15M\r\nACTION:DISPLAY\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

        let invites = parse_ical_data(ical);
        assert_eq!(
            invites.len(),
            1,
            "Should parse 1 invite from Microsoft Exchange iCal"
        );
        let inv = &invites[0];
        assert_eq!(inv.summary, Some("Yo food".to_string()));
        assert_eq!(inv.method, "REQUEST");
        assert!(inv.organizer_email.as_deref() == Some("chithiapp@outlook.com"));
    }

    #[test]
    fn test_parse_ical_with_lf_line_folding() {
        // Regression for issue #70. Radicale / Thunderbird-exported calendars
        // use LF-only line endings with LF-based folds (\n<space>). The old
        // unfolder only handled CRLF folds, so continuation lines survived
        // and the strict icalendar parser failed at the next BEGIN marker
        // with "Satisfy at: BEGIN:VEVENT".
        let ical = "BEGIN:VCALENDAR\n\
VERSION:2.0\n\
PRODID:-//Mozilla.org/NONSGML Mozilla Calendar V1.1//EN\n\
BEGIN:VEVENT\n\
UID:lf-fold-test\n\
DTSTART;TZID=Europe/Stockholm:20240815T100000\n\
DTEND;TZID=Europe/Stockholm:20240815T110000\n\
ATTENDEE;CN=Example User;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:exampleus\n er@example.com\n\
SUMMARY:Folded attendee\n\
STATUS:CONFIRMED\n\
X-MOZ-GENERATION:1\n\
END:VEVENT\n\
END:VCALENDAR\n";

        let invites = parse_ical_data(ical);
        assert_eq!(
            invites.len(),
            1,
            "LF-folded VEVENT should parse after unfold"
        );
        let inv = &invites[0];
        assert_eq!(inv.uid, "lf-fold-test");
        assert_eq!(inv.summary, Some("Folded attendee".to_string()));
        assert_eq!(inv.attendees.len(), 1);
        assert_eq!(inv.attendees[0].email, "exampleuser@example.com");
    }

    #[test]
    fn test_parse_ical_with_altrep_description() {
        // Regression for issue #46. Real Exchange/Outlook DESCRIPTION lines
        // embed an ALTREP="data:text/html,..." payload whose HTML contains raw
        // double-quote characters (e.g. <p class="foo">). The quoted param
        // value therefore contains unescaped " chars, which the strict
        // icalendar parser rejects with "Satisfy at: BEGIN:VEVENT". The event
        // also uses RFC 5545 line folding (CRLF + space).
        let ical = "BEGIN:VCALENDAR\r\n\
METHOD:REQUEST\r\n\
PRODID:Microsoft Exchange Server 2010\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:bob@exam\r\n ple.com\r\n\
DESCRIPTION;LANGUAGE=en-US;ALTREP=\"data:text/html,<html><body><p class=\"gre\r\n eting\">Hello, world: this is <b>bold</b></p></body></html>\":Plain text fall\r\n back description\r\n\
UID:altrep-test-uid\r\n\
SUMMARY:Meeting with ALTREP description\r\n\
DTSTART;TZID=UTC:20260414T170000\r\n\
DTEND;TZID=UTC:20260414T180000\r\n\
DTSTAMP:20260413T132342Z\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

        let invites = parse_ical_data(ical);
        assert_eq!(
            invites.len(),
            1,
            "Should parse 1 invite with ALTREP DESCRIPTION"
        );
        let inv = &invites[0];
        assert_eq!(inv.uid, "altrep-test-uid");
        assert_eq!(
            inv.summary,
            Some("Meeting with ALTREP description".to_string())
        );
        assert_eq!(
            inv.description,
            Some("Plain text fallback description".to_string())
        );
    }

    #[test]
    fn test_strip_altrep_quoted_with_inner_quotes() {
        let line =
            "DESCRIPTION;LANGUAGE=en-US;ALTREP=\"data:text/html,<p class=\"a\">x</p>\":plain\n";
        assert_eq!(
            strip_altrep_from_line(line).as_ref(),
            "DESCRIPTION;LANGUAGE=en-US:plain\n"
        );
    }

    #[test]
    fn test_strip_altrep_quoted_well_formed() {
        let line = "DESCRIPTION;ALTREP=\"cid:part1\":hello\n";
        assert_eq!(strip_altrep_from_line(line).as_ref(), "DESCRIPTION:hello\n");
    }

    #[test]
    fn test_strip_altrep_followed_by_other_param() {
        let line = "DESCRIPTION;ALTREP=\"cid:part1\";LANGUAGE=en:hello\n";
        assert_eq!(
            strip_altrep_from_line(line).as_ref(),
            "DESCRIPTION;LANGUAGE=en:hello\n"
        );
    }

    #[test]
    fn test_strip_altrep_case_insensitive() {
        let line = "DESCRIPTION;altrep=\"cid:part1\":hello\n";
        assert_eq!(strip_altrep_from_line(line).as_ref(), "DESCRIPTION:hello\n");
    }

    #[test]
    fn test_strip_altrep_unquoted() {
        let line = "DESCRIPTION;ALTREP=cid:part1:hello\n";
        // Unquoted terminates at first ':' per RFC 5545 §3.1, so we strip
        // only ";ALTREP=cid" and the remainder becomes the property value.
        assert_eq!(
            strip_altrep_from_line(line).as_ref(),
            "DESCRIPTION:part1:hello\n"
        );
    }

    #[test]
    fn test_strip_altrep_line_without_altrep_is_borrowed() {
        let line = "SUMMARY:Team Standup\n";
        let out = strip_altrep_from_line(line);
        assert_eq!(out.as_ref(), line);
        assert!(
            matches!(out, Cow::Borrowed(_)),
            "no-ALTREP lines must not allocate"
        );
    }

    #[test]
    fn test_strip_altrep_leaves_value_substring_alone() {
        // A literal `;ALTREP=` that appears inside the property VALUE
        // (after the real `:` separator) must not be touched.
        let line = "DESCRIPTION:look at ;ALTREP=example in the value\n";
        let out = strip_altrep_from_line(line);
        assert_eq!(out.as_ref(), line);
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn test_property_value_separator_plain() {
        assert_eq!(property_value_separator("SUMMARY:hello\n"), Some(7));
    }

    #[test]
    fn test_property_value_separator_with_quoted_param() {
        // The first `:` is inside the quoted ALTREP value; the real separator
        // is the one after the closing `"`.
        let line = "DESCRIPTION;ALTREP=\"data:text/html,x\":plain\n";
        let pos = property_value_separator(line).unwrap();
        assert_eq!(&line[pos..pos + 1], ":");
        assert_eq!(&line[pos + 1..].trim_end(), &"plain");
    }

    #[test]
    fn test_property_value_separator_with_inner_quotes() {
        // Malformed Exchange payload — raw `"` inside quoted ALTREP.
        let line = "DESCRIPTION;ALTREP=\"data:text/html,<p class=\"a\">x</p>\":plain\n";
        let pos = property_value_separator(line).unwrap();
        assert_eq!(&line[pos + 1..].trim_end(), &"plain");
    }

    #[test]
    fn test_generate_reply_accepted() {
        let invite = ParsedInvite {
            method: "REQUEST".to_string(),
            uid: "test-uid-123".to_string(),
            summary: Some("Team Standup".to_string()),
            description: None,
            location: None,
            dtstart: "2026-04-07T17:00:00Z".to_string(),
            dtend: "2026-04-07T18:00:00Z".to_string(),
            all_day: false,
            timezone: None,
            organizer_email: Some("alice@example.com".to_string()),
            organizer_name: Some("Alice".to_string()),
            attendees: vec![],
            recurrence_rule: None,
            recurrence_kind: RecurrenceKind::Standalone,
            sequence: 0,
            ical_raw: String::new(),
        };

        let reply = generate_reply(&invite, "bob@example.com", Some("Bob Builder"), "accepted");

        assert!(reply.contains("METHOD:REPLY"), "Should have METHOD:REPLY");
        assert!(
            reply.contains("PARTSTAT=ACCEPTED"),
            "Should have ACCEPTED partstat"
        );
        assert!(
            reply.contains("mailto:bob@example.com"),
            "Should contain attendee email"
        );
        assert!(reply.contains("CN=\"Bob Builder\""));
        assert!(reply.contains("UID:test-uid-123"), "Should preserve UID");
        assert!(
            reply.contains("ORGANIZER;CN=\"Alice\":mailto:alice@example.com"),
            "Should preserve organizer"
        );
        assert!(
            reply.contains("SUMMARY:Team Standup"),
            "Should preserve summary"
        );
    }

    #[test]
    fn test_generate_reply_declined() {
        let invite = ParsedInvite {
            method: "REQUEST".to_string(),
            uid: "uid-456".to_string(),
            summary: Some("Meeting".to_string()),
            description: None,
            location: None,
            dtstart: "2026-04-07T17:00:00Z".to_string(),
            dtend: "2026-04-07T18:00:00Z".to_string(),
            all_day: false,
            timezone: None,
            organizer_email: Some("org@example.com".to_string()),
            organizer_name: None,
            attendees: vec![],
            recurrence_rule: None,
            recurrence_kind: RecurrenceKind::Standalone,
            sequence: 1,
            ical_raw: String::new(),
        };

        let reply = generate_reply(&invite, "user@example.com", None, "declined");
        assert!(reply.contains("PARTSTAT=DECLINED"));
        assert!(reply.contains("SEQUENCE:1"));
    }

    #[test]
    fn test_generate_invite_with_attendees() {
        let attendees = vec![
            Attendee {
                email: "bob@example.com".to_string(),
                name: Some("Bob".to_string()),
                status: "needs-action".to_string(),
                is_self: None,
            },
            Attendee {
                email: "carol@example.com".to_string(),
                name: None,
                status: "needs-action".to_string(),
                is_self: None,
            },
        ];

        let ical = generate_invite(
            "new-uid-789",
            "Project Review",
            "2026-04-07T17:00:00Z",
            "2026-04-07T18:00:00Z",
            Some("Conference Room"),
            Some("Quarterly review"),
            "alice@example.com",
            Some("Alice"),
            &attendees,
            None,
            None,
        );

        assert!(ical.contains("METHOD:REQUEST"));
        assert!(ical.contains("UID:new-uid-789"));
        assert!(ical.contains("SUMMARY:Project Review"));
        assert!(ical.contains("LOCATION:Conference Room"));
        assert!(ical.contains("DESCRIPTION:Quarterly review"));
        assert!(ical.contains("ORGANIZER;CN=\"Alice\":mailto:alice@example.com"));
        assert!(ical.contains("mailto:bob@example.com"));
        assert!(ical.contains(";CN=\"Bob\""));
        assert!(ical.contains("mailto:carol@example.com"));
        assert!(ical.contains("STATUS:CONFIRMED"));
    }

    #[test]
    fn test_generate_invite_roundtrip() {
        // Generate an invite, then parse it back
        let attendees = vec![Attendee {
            email: "bob@example.com".to_string(),
            name: None,
            status: "needs-action".to_string(),
            is_self: None,
        }];

        let ical = generate_invite(
            "roundtrip-uid",
            "Roundtrip Test",
            "2026-04-07T17:00:00Z",
            "2026-04-07T18:00:00Z",
            None,
            None,
            "alice@example.com",
            None,
            &attendees,
            None,
            None,
        );

        let parsed = parse_ical_data(&ical);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].uid, "roundtrip-uid");
        assert_eq!(parsed[0].summary, Some("Roundtrip Test".to_string()));
        assert_eq!(parsed[0].method, "REQUEST");
        assert_eq!(
            parsed[0].organizer_email,
            Some("alice@example.com".to_string())
        );
        assert_eq!(parsed[0].attendees.len(), 1);
        assert_eq!(parsed[0].attendees[0].email, "bob@example.com");
    }

    #[test]
    fn test_generate_invite_organizer_no_cn() {
        // BUG: organizer CN was set to account display_name ("jMAP")
        // which confused recipients. When organizer_name is None,
        // ORGANIZER should use mailto: without CN parameter.
        let ical = generate_invite(
            "uid-no-cn",
            "Test Meeting",
            "2026-04-07T17:00:00Z",
            "2026-04-07T18:00:00Z",
            None,
            None,
            "kushal@example.org",
            None, // No organizer name
            &[],
            None,
            None,
        );
        assert!(
            ical.contains("ORGANIZER:mailto:kushal@example.org"),
            "Should have ORGANIZER without CN, got: {}",
            ical
        );
        assert!(!ical.contains("CN="), "Should NOT have CN parameter");
    }

    #[test]
    fn test_generate_invite_organizer_with_cn() {
        let ical = generate_invite(
            "uid-cn",
            "Test Meeting",
            "2026-04-07T17:00:00Z",
            "2026-04-07T18:00:00Z",
            None,
            None,
            "kushal@example.org",
            Some("Kushal Das"),
            &[],
            None,
            None,
        );
        assert!(ical.contains("ORGANIZER;CN=\"Kushal Das\":mailto:kushal@example.org"));

        let injected = generate_invite(
            "uid-2",
            "Meeting",
            "2026-08-28T12:00:00Z",
            "2026-08-28T13:00:00Z",
            None,
            None,
            "mallory@example.org",
            Some("Mallory\r\nATTENDEE:mailto:attacker@example.org;\"^^"),
            &[],
            None,
            None,
        );
        assert!(injected.contains("CN=\"Mallory^nATTENDEE:mailto:attacker@example.org;^'^^^^\""));
        assert!(!injected.contains("\r\nATTENDEE:mailto:attacker@example.org"));
    }

    #[test]
    fn test_parse_ical_duration() {
        assert_eq!(
            parse_ical_duration("PT1H"),
            Some(chrono::Duration::hours(1))
        );
        assert_eq!(
            parse_ical_duration("PT30M"),
            Some(chrono::Duration::minutes(30))
        );
        assert_eq!(
            parse_ical_duration("PT1H30M"),
            Some(chrono::Duration::minutes(90))
        );
        assert_eq!(parse_ical_duration("P1D"), Some(chrono::Duration::days(1)));
        assert_eq!(parse_ical_duration("P1W"), Some(chrono::Duration::weeks(1)));
        assert_eq!(parse_ical_duration("invalid"), None);
    }
}
