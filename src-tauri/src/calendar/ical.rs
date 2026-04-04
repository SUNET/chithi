use mail_parser::{MessageParser, MimeHeaders};
use serde::{Deserialize, Serialize};

use crate::db::calendar::Attendee;

/// A parsed calendar invite extracted from an email or raw iCalendar text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedInvite {
    pub method: String,              // REQUEST, REPLY, CANCEL
    pub uid: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub dtstart: String,             // ISO 8601
    pub dtend: String,               // ISO 8601
    pub all_day: bool,
    pub timezone: Option<String>,
    pub organizer_email: Option<String>,
    pub organizer_name: Option<String>,
    pub attendees: Vec<Attendee>,
    pub recurrence_rule: Option<String>,
    pub sequence: u32,
    pub ical_raw: String,            // Original iCalendar text
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
                ct.ctype() == "text"
                    && ct.subtype().map(|s| s == "calendar").unwrap_or(false)
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

    // Use the icalendar parser to get structured components
    let components = match icalendar::parser::read_calendar_simple(ical_text) {
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

        let method = find_property_value(vcal, "METHOD")
            .unwrap_or_else(|| "REQUEST".to_string());

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
            let sequence: u32 = sequence_str
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

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
                sequence,
                ical_raw: ical_text.to_string(),
            });
        }
    }

    invites
}

/// Generate an iTIP REPLY iCalendar for responding to an invite.
///
/// `response` should be one of: "ACCEPTED", "TENTATIVE", "DECLINED".
pub fn generate_reply(invite: &ParsedInvite, user_email: &str, response: &str) -> String {
    let partstat = response.to_uppercase();
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");

    // Build the REPLY iCalendar manually for full control over iTIP format
    let mut lines = Vec::new();
    lines.push("BEGIN:VCALENDAR".to_string());
    lines.push("VERSION:2.0".to_string());
    lines.push("PRODID:-//Emails Desktop Client//EN".to_string());
    lines.push("METHOD:REPLY".to_string());
    lines.push("BEGIN:VEVENT".to_string());

    // Preserve organizer from original invite
    if let Some(ref org_email) = invite.organizer_email {
        if let Some(ref org_name) = invite.organizer_name {
            lines.push(format!("ORGANIZER;CN={}:mailto:{}", org_name, org_email));
        } else {
            lines.push(format!("ORGANIZER:mailto:{}", org_email));
        }
    }

    // Add the replying attendee with their response
    lines.push(format!(
        "ATTENDEE;PARTSTAT={};RSVP=FALSE:mailto:{}",
        partstat, user_email
    ));

    lines.push(format!("UID:{}", invite.uid));
    if let Some(ref summary) = invite.summary {
        lines.push(format!("SUMMARY:{}", summary));
    }
    lines.push(format!("DTSTART:{}", to_ical_datetime(&invite.dtstart)));
    lines.push(format!("DTEND:{}", to_ical_datetime(&invite.dtend)));
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
) -> String {
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");

    let mut lines = Vec::new();
    lines.push("BEGIN:VCALENDAR".to_string());
    lines.push("VERSION:2.0".to_string());
    lines.push("PRODID:-//Emails Desktop Client//EN".to_string());
    lines.push("METHOD:REQUEST".to_string());
    lines.push("BEGIN:VEVENT".to_string());

    // Organizer
    if let Some(name) = organizer_name {
        lines.push(format!("ORGANIZER;CN={}:mailto:{}", name, organizer_email));
    } else {
        lines.push(format!("ORGANIZER:mailto:{}", organizer_email));
    }

    // Attendees
    for attendee in attendees {
        let cn = attendee
            .name
            .as_ref()
            .map(|n| format!(";CN={}", n))
            .unwrap_or_default();
        lines.push(format!(
            "ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE{}:mailto:{}",
            cn, attendee.email
        ));
    }

    lines.push(format!("UID:{}", uid));
    lines.push(format!("SUMMARY:{}", summary));
    lines.push(format!("DTSTART:{}", to_ical_datetime(dtstart)));
    lines.push(format!("DTEND:{}", to_ical_datetime(dtend)));

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

/// Find a property value by name on a parser component.
fn find_property_value(
    component: &icalendar::parser::Component<'_>,
    name: &str,
) -> Option<String> {
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
fn ical_datetime_to_iso(val: &str, all_day: bool, _tzid: Option<&str>) -> String {
    let val = val.trim();

    if all_day && val.len() >= 8 {
        // DATE format: YYYYMMDD
        return format!(
            "{}-{}-{}",
            &val[0..4],
            &val[4..6],
            &val[6..8]
        );
    }

    // DATETIME format: YYYYMMDDTHHmmss or YYYYMMDDTHHmmssZ
    if val.len() >= 15 {
        let utc_suffix = if val.ends_with('Z') { "Z" } else { "" };
        return format!(
            "{}-{}-{}T{}:{}:{}{}",
            &val[0..4],
            &val[4..6],
            &val[6..8],
            &val[9..11],
            &val[11..13],
            &val[13..15],
            utc_suffix,
        );
    }

    // Fallback: return as-is
    val.to_string()
}

/// Convert an ISO 8601 datetime back to iCalendar format for use in REPLY.
fn to_ical_datetime(iso: &str) -> String {
    // Convert ISO 8601 to iCalendar format: "2025-04-15T10:00:00.000Z" -> "20250415T100000Z"
    // Parse with chrono to normalize, then format as iCal UTC.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso) {
        return dt.with_timezone(&chrono::Utc).format("%Y%m%dT%H%M%SZ").to_string();
    }
    // Try parsing without timezone (treat as UTC)
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(
        iso.trim_end_matches('Z'),
        "%Y-%m-%dT%H:%M:%S%.f",
    ) {
        return dt.format("%Y%m%dT%H%M%SZ").to_string();
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(
        iso.trim_end_matches('Z'),
        "%Y-%m-%dT%H:%M:%S",
    ) {
        return dt.format("%Y%m%dT%H%M%SZ").to_string();
    }
    // Fallback: just strip dashes/colons
    iso.replace('-', "").replace(':', "").replace(".000", "")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_mailto() {
        assert_eq!(extract_mailto("mailto:alice@example.com"), Some("alice@example.com".to_string()));
        assert_eq!(extract_mailto("MAILTO:Bob@Example.com"), Some("Bob@Example.com".to_string()));
        assert_eq!(extract_mailto("alice@example.com"), Some("alice@example.com".to_string()));
        assert_eq!(extract_mailto("not-an-email"), None);
    }

    #[test]
    fn test_to_ical_datetime_from_rfc3339() {
        assert_eq!(to_ical_datetime("2026-04-07T17:00:00.000Z"), "20260407T170000Z");
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
            "2026-04-07T17:00:00"
        );
    }

    #[test]
    fn test_ical_datetime_to_iso_allday() {
        assert_eq!(
            ical_datetime_to_iso("20260407", true, None),
            "2026-04-07"
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
        assert!(invites[0].dtend.contains("17:30:00"), "dtend should be 30min after dtstart, got: {}", invites[0].dtend);
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
            sequence: 0,
            ical_raw: String::new(),
        };

        let reply = generate_reply(&invite, "bob@example.com", "accepted");

        assert!(reply.contains("METHOD:REPLY"), "Should have METHOD:REPLY");
        assert!(reply.contains("PARTSTAT=ACCEPTED"), "Should have ACCEPTED partstat");
        assert!(reply.contains("mailto:bob@example.com"), "Should contain attendee email");
        assert!(reply.contains("UID:test-uid-123"), "Should preserve UID");
        assert!(reply.contains("ORGANIZER;CN=Alice:mailto:alice@example.com"), "Should preserve organizer");
        assert!(reply.contains("SUMMARY:Team Standup"), "Should preserve summary");
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
            sequence: 1,
            ical_raw: String::new(),
        };

        let reply = generate_reply(&invite, "user@example.com", "declined");
        assert!(reply.contains("PARTSTAT=DECLINED"));
        assert!(reply.contains("SEQUENCE:1"));
    }

    #[test]
    fn test_generate_invite_with_attendees() {
        let attendees = vec![
            Attendee { email: "bob@example.com".to_string(), name: Some("Bob".to_string()), status: "needs-action".to_string() },
            Attendee { email: "carol@example.com".to_string(), name: None, status: "needs-action".to_string() },
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
        );

        assert!(ical.contains("METHOD:REQUEST"));
        assert!(ical.contains("UID:new-uid-789"));
        assert!(ical.contains("SUMMARY:Project Review"));
        assert!(ical.contains("LOCATION:Conference Room"));
        assert!(ical.contains("DESCRIPTION:Quarterly review"));
        assert!(ical.contains("ORGANIZER;CN=Alice:mailto:alice@example.com"));
        assert!(ical.contains("mailto:bob@example.com"));
        assert!(ical.contains(";CN=Bob"));
        assert!(ical.contains("mailto:carol@example.com"));
        assert!(ical.contains("STATUS:CONFIRMED"));
    }

    #[test]
    fn test_generate_invite_roundtrip() {
        // Generate an invite, then parse it back
        let attendees = vec![
            Attendee { email: "bob@example.com".to_string(), name: None, status: "needs-action".to_string() },
        ];

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
        );

        let parsed = parse_ical_data(&ical);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].uid, "roundtrip-uid");
        assert_eq!(parsed[0].summary, Some("Roundtrip Test".to_string()));
        assert_eq!(parsed[0].method, "REQUEST");
        assert_eq!(parsed[0].organizer_email, Some("alice@example.com".to_string()));
        assert_eq!(parsed[0].attendees.len(), 1);
        assert_eq!(parsed[0].attendees[0].email, "bob@example.com");
    }

    #[test]
    fn test_parse_ical_duration() {
        assert_eq!(parse_ical_duration("PT1H"), Some(chrono::Duration::hours(1)));
        assert_eq!(parse_ical_duration("PT30M"), Some(chrono::Duration::minutes(30)));
        assert_eq!(parse_ical_duration("PT1H30M"), Some(chrono::Duration::minutes(90)));
        assert_eq!(parse_ical_duration("P1D"), Some(chrono::Duration::days(1)));
        assert_eq!(parse_ical_duration("P1W"), Some(chrono::Duration::weeks(1)));
        assert_eq!(parse_ical_duration("invalid"), None);
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

