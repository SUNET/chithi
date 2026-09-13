//! Conversion between iCal `RRULE` strings and JSCalendar
//! `RecurrenceRule` objects (RFC 8984 §4.3.2).
//!
//! The app's canonical local format for `calendar_events.recurrence_rule`
//! is the iCal RRULE value string (e.g. `FREQ=WEEKLY;INTERVAL=2;BYDAY=TU`):
//! it's what the invite parser extracts (calendar::ical), what CalDAV and
//! Google deliver, and what the frontend expander (src/lib/rrule.ts)
//! understands. JMAP speaks JSCalendar instead, so the wire layer must
//! convert in both directions — previously it did neither, which made
//! server recurrences invisible locally and silently dropped local
//! recurrences on push.
//!
//! The frontend only expands daily, weekly, monthly and yearly rules, with
//! unqualified `BYDAY` on weekly rules. Rules outside that subset are
//! rejected rather than silently changed into a different series.

use serde_json::{json, Value};

const DAY_CODES: [&str; 7] = ["su", "mo", "tu", "we", "th", "fr", "sa"];

fn is_valid_day(code: &str) -> bool {
    DAY_CODES.contains(&code)
}

/// Normalize the supported invitation RRULE subset without changing its bounds.
///
/// Accept one optional `RRULE:` prefix, normalize ASCII case and surrounding
/// spaces, and retain the original part order, numeric values and UNTIL form.
/// Unlike the viewer converter, this rejects malformed fields before conversion
/// can discard them. A supported rule alone does not prove local provenance.
pub fn normalize_invitation_rrule(rule: &str, timezone: Option<&str>) -> Option<String> {
    if !rule.is_ascii()
        || rule.bytes().any(|byte| byte.is_ascii_control())
        || timezone.is_some_and(|timezone| timezone.parse::<chrono_tz::Tz>().is_err())
    {
        return None;
    }
    let rule = rule.trim().to_ascii_uppercase();
    let rule = rule.strip_prefix("RRULE:").unwrap_or(&rule);
    let mut keys = std::collections::HashSet::new();
    let mut parts = Vec::new();
    for part in rule.split(';') {
        let (key, value) = part.split_once('=')?;
        let key = key.trim();
        let value = value.trim();
        if !keys.insert(key) || value.is_empty() {
            return None;
        }
        match key {
            "INTERVAL" | "COUNT" => {
                if !value.bytes().all(|byte| byte.is_ascii_digit())
                    || value.parse::<u32>().ok()? == 0
                {
                    return None;
                }
            }
            "UNTIL" if !valid_ical_until_grammar(value) => return None,
            _ => {}
        }
        let value = if key == "BYDAY" {
            value
                .split(',')
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(",")
        } else {
            value.to_string()
        };
        parts.push(format!("{key}={value}"));
    }
    if keys.contains("COUNT") && keys.contains("UNTIL") {
        return None;
    }
    let normalized = parts.join(";");
    // Check before the converter strips prefixes: a second RRULE prefix must
    // never become valid by passing through another normalization layer.
    if !is_locally_supported_rrule(&normalized) {
        return None;
    }
    let rules = rrule_to_jscalendar(&normalized, timezone)?;
    if keys.contains("UNTIL") && !rules[0]["until"].as_str().is_some_and(valid_local_datetime) {
        return None;
    }
    Some(normalized)
}

/// Convert only rules that can be represented faithfully during local creation.
pub(crate) fn faithful_local_recurrence_rules(rule: &str, timezone: Option<&str>) -> Option<Value> {
    let rule = normalize_invitation_rrule(rule, timezone)?;
    rrule_to_jscalendar(&rule, timezone)
}

fn valid_ical_until_grammar(value: &str) -> bool {
    match value.len() {
        8 => value.bytes().all(|byte| byte.is_ascii_digit()),
        15 | 16 => value.bytes().enumerate().all(|(index, byte)| match index {
            8 => byte == b'T',
            15 => byte == b'Z',
            _ => byte.is_ascii_digit(),
        }),
        _ => false,
    }
}

/// RFC 8984 LocalDateTime permits fractional seconds, but never a UTC suffix.
pub(crate) fn valid_local_datetime(value: &str) -> bool {
    if !value.is_ascii() || value.len() < 19 {
        return false;
    }
    let (seconds, fraction) = value.split_at(19);
    seconds.bytes().enumerate().all(|(i, byte)| match i {
        4 | 7 => byte == b'-',
        10 => byte == b'T',
        13 | 16 => byte == b':',
        _ => byte.is_ascii_digit(),
    }) && (fraction.is_empty()
        || fraction.strip_prefix('.').is_some_and(|digits| {
            !digits.is_empty()
                && !digits.ends_with('0')
                && digits.bytes().all(|byte| byte.is_ascii_digit())
        }))
        && chrono::NaiveDateTime::parse_from_str(seconds, "%Y-%m-%dT%H:%M:%S").is_ok()
}

/// Convert an iCal RRULE value string into a JSCalendar `recurrenceRules`
/// array (a one-element array — RRULE describes a single rule).
/// Returns `None` when the input isn't an RRULE string (e.g. legacy rows
/// that hold raw JSON) or lacks a usable FREQ.
pub fn rrule_to_jscalendar(rrule: &str, timezone: Option<&str>) -> Option<Value> {
    let rrule = rrule.trim().trim_start_matches("RRULE:");
    if !is_locally_supported_rrule(rrule) {
        return None;
    }

    let mut freq = None;
    let mut interval = None;
    let mut count = None;
    let mut until = None;
    let mut by_day = None;

    for part in rrule.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_uppercase().as_str() {
            "FREQ" => {
                let f = value.to_ascii_lowercase();
                if matches!(f.as_str(), "yearly" | "monthly" | "weekly" | "daily") {
                    freq = Some(f);
                }
            }
            "INTERVAL" => interval = value.parse::<u32>().ok().filter(|n| *n > 1),
            "COUNT" => count = value.parse::<u32>().ok(),
            "UNTIL" => until = ical_until_to_local(value, timezone),
            "BYDAY" => {
                let days: Vec<Value> = value.split(',').filter_map(parse_nday).collect();
                if !days.is_empty() {
                    by_day = Some(days);
                }
            }
            _ => {}
        }
    }

    let freq = freq?;
    let mut rule = serde_json::Map::new();
    rule.insert("@type".into(), json!("RecurrenceRule"));
    rule.insert("frequency".into(), json!(freq));
    if let Some(n) = interval {
        rule.insert("interval".into(), json!(n));
    }
    if let Some(n) = count {
        rule.insert("count".into(), json!(n));
    }
    if let Some(u) = until {
        rule.insert("until".into(), json!(u));
    }
    if let Some(d) = by_day {
        rule.insert("byDay".into(), Value::Array(d));
    }
    Some(Value::Array(vec![Value::Object(rule)]))
}

/// Convert a JSCalendar `recurrenceRules` array into an iCal RRULE value
/// string. Only the first rule is used — RRULE can express exactly one,
/// and multiple RRULEs are deprecated since RFC 5545.
/// Returns `None` when there is no rule with a usable frequency.
pub fn jscalendar_to_rrule(rules: &[Value], timezone: Option<&str>) -> Option<String> {
    let rule = rules.first()?;
    if rules.len() > 1 {
        log::warn!(
            "jscalendar_to_rrule: {} recurrence rules on event are not supported locally",
            rules.len()
        );
        return None;
    }

    let freq = rule["frequency"].as_str()?;
    let freq = freq.to_ascii_uppercase();
    if !matches!(freq.as_str(), "YEARLY" | "MONTHLY" | "WEEKLY" | "DAILY") {
        return None;
    }
    if !is_locally_supported_jscalendar_rule(rule, &freq, timezone) {
        return None;
    }

    // FREQ must come first: the frontend parser requires the string to
    // start with "FREQ=" (src/lib/rrule.ts).
    let mut parts = vec![format!("FREQ={}", freq)];

    if let Some(interval) = rule["interval"].as_u64() {
        if interval > 1 {
            parts.push(format!("INTERVAL={}", interval));
        }
    }
    if let Some(count) = rule["count"].as_u64() {
        parts.push(format!("COUNT={}", count));
    }
    if let Some(until) = rule["until"].as_str() {
        if let Some(u) = local_until_to_ical(until, timezone) {
            parts.push(format!("UNTIL={}", u));
        }
    }
    if let Some(days) = rule["byDay"].as_array() {
        let tokens: Vec<String> = days.iter().filter_map(format_nday).collect();
        if !tokens.is_empty() {
            parts.push(format!("BYDAY={}", tokens.join(",")));
        }
    }
    Some(parts.join(";"))
}

fn is_locally_supported_rrule(rrule: &str) -> bool {
    let mut freq = None;
    let mut by_day = None;
    for part in rrule.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            return false;
        };
        match key.trim().to_ascii_uppercase().as_str() {
            "FREQ" => freq = Some(value.trim().to_ascii_uppercase()),
            "INTERVAL" | "COUNT" | "UNTIL" => {}
            "BYDAY" => by_day = Some(value),
            _ => return false,
        }
    }
    let Some(freq) = freq else { return false };
    if !matches!(freq.as_str(), "DAILY" | "WEEKLY" | "MONTHLY" | "YEARLY") {
        return false;
    }
    by_day.is_none_or(|days| {
        freq == "WEEKLY"
            && days.split(',').all(|day| {
                let day = day.trim().to_ascii_lowercase();
                is_valid_day(&day)
            })
    })
}

fn is_locally_supported_jscalendar_rule(rule: &Value, freq: &str, timezone: Option<&str>) -> bool {
    let Some(object) = rule.as_object() else {
        return false;
    };
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "@type" | "frequency" | "interval" | "count" | "until" | "byDay"
        )
    }) {
        return false;
    }
    if object
        .get("interval")
        .is_some_and(|value| value.as_u64().is_none_or(|value| value == 0))
        || object
            .get("count")
            .is_some_and(|value| value.as_u64().is_none_or(|value| value == 0))
        || object.get("until").is_some_and(|value| {
            value
                .as_str()
                .and_then(|value| local_until_to_ical(value, timezone))
                .is_none()
        })
    {
        return false;
    }
    match object.get("byDay") {
        None => true,
        Some(Value::Array(days)) if freq == "WEEKLY" => days.iter().all(|day| {
            day["day"]
                .as_str()
                .is_some_and(|value| is_valid_day(&value.to_ascii_lowercase()))
                && day["nthOfPeriod"].as_i64().is_none_or(|nth| nth == 0)
        }),
        _ => false,
    }
}

/// Parse an iCal BYDAY token ("MO", "1TH", "-1FR") into a JSCalendar NDay.
fn parse_nday(token: &str) -> Option<Value> {
    let token = token.trim();
    if token.len() < 2 {
        return None;
    }
    let (ordinal, day) = token.split_at(token.len() - 2);
    let day = day.to_ascii_lowercase();
    if !is_valid_day(&day) {
        return None;
    }
    if ordinal.is_empty() {
        return Some(json!({ "@type": "NDay", "day": day }));
    }
    let nth: i64 = ordinal.parse().ok()?;
    Some(json!({ "@type": "NDay", "day": day, "nthOfPeriod": nth }))
}

/// Format a JSCalendar NDay object back into an iCal BYDAY token.
fn format_nday(nday: &Value) -> Option<String> {
    let day = nday["day"].as_str()?.to_ascii_lowercase();
    if !is_valid_day(&day) {
        return None;
    }
    let day = day.to_ascii_uppercase();
    match nday["nthOfPeriod"].as_i64() {
        Some(nth) if nth != 0 => Some(format!("{}{}", nth, day)),
        _ => Some(day),
    }
}

/// iCal UNTIL ("20261231T170000Z", "20261231T170000", "20261231") →
/// JSCalendar LocalDateTime ("2026-12-31T17:00:00").
fn ical_until_to_local(value: &str, timezone: Option<&str>) -> Option<String> {
    let value = value.trim();
    let utc = value.ends_with('Z');
    let value = value.trim_end_matches('Z');
    let digits_ok = |s: &str| s.chars().all(|c| c.is_ascii_digit());

    if value.len() == 8 && digits_ok(value) {
        // Date-only UNTIL is inclusive of that day — use end of day.
        return Some(format!(
            "{}-{}-{}T23:59:59",
            &value[0..4],
            &value[4..6],
            &value[6..8]
        ));
    }
    if value.len() == 15
        && &value[8..9] == "T"
        && digits_ok(&value[0..8])
        && digits_ok(&value[9..15])
    {
        let local = format!(
            "{}-{}-{}T{}:{}:{}",
            &value[0..4],
            &value[4..6],
            &value[6..8],
            &value[9..11],
            &value[11..13],
            &value[13..15]
        );
        if !utc {
            return Some(local);
        }
        let naive = chrono::NaiveDateTime::parse_from_str(&local, "%Y-%m-%dT%H:%M:%S").ok()?;
        let utc = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(naive, chrono::Utc);
        if let Some(tz) = timezone.and_then(|tz| tz.parse::<chrono_tz::Tz>().ok()) {
            return Some(
                utc.with_timezone(&tz)
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string(),
            );
        }
        return Some(local);
    }
    None
}

/// JSCalendar LocalDateTime ("2026-12-31T17:00:00", fractional seconds
/// tolerated) → iCal UNTIL ("20261231T170000Z"); date-only → "20261231".
fn local_until_to_ical(value: &str, timezone: Option<&str>) -> Option<String> {
    let value = value.trim().trim_end_matches('Z');
    let compact: String = value
        .chars()
        .take(19) // "YYYY-MM-DDTHH:MM:SS", drops fractional seconds
        .filter(|c| c.is_ascii_digit() || *c == 'T')
        .collect();

    match compact.len() {
        8 => Some(compact), // date only
        15 => {
            let local = format!(
                "{}-{}-{}T{}:{}:{}",
                &compact[0..4],
                &compact[4..6],
                &compact[6..8],
                &compact[9..11],
                &compact[11..13],
                &compact[13..15]
            );
            let utc = crate::calendar::timezone::to_utc(&local, timezone.unwrap_or(""));
            Some(utc.replace(['-', ':'], "").replace("-", ""))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(rules: &Value) -> &serde_json::Map<String, Value> {
        rules.as_array().unwrap()[0].as_object().unwrap()
    }

    #[test]
    fn invitation_normalization_preserves_supported_grammar_and_bounds() {
        for (input, normalized) in [
            ("FREQ=DAILY;COUNT=5", "FREQ=DAILY;COUNT=5"),
            ("RRULE:FREQ=YEARLY;INTERVAL=1", "FREQ=YEARLY;INTERVAL=1"),
            (
                " rrule:freq = weekly ;interval=02;byday= mo, we ;count=004 ",
                "FREQ=WEEKLY;INTERVAL=02;BYDAY=MO,WE;COUNT=004",
            ),
            (
                "COUNT=4;FREQ=WEEKLY;BYDAY=FR",
                "COUNT=4;FREQ=WEEKLY;BYDAY=FR",
            ),
            ("FREQ=DAILY;UNTIL=20280229", "FREQ=DAILY;UNTIL=20280229"),
            (
                "FREQ=MONTHLY;UNTIL=20261231T170000",
                "FREQ=MONTHLY;UNTIL=20261231T170000",
            ),
            (
                "FREQ=WEEKLY;UNTIL=20261101T140000Z",
                "FREQ=WEEKLY;UNTIL=20261101T140000Z",
            ),
        ] {
            assert_eq!(
                normalize_invitation_rrule(input, Some("America/New_York")).as_deref(),
                Some(normalized),
                "{input}"
            );
            assert_eq!(
                normalize_invitation_rrule(normalized, Some("America/New_York")).as_deref(),
                Some(normalized)
            );
        }
    }

    #[test]
    fn faithful_conversion_preserves_existing_jmap_values() {
        let rules = faithful_local_recurrence_rules(
            "RRULE:FREQ=WEEKLY;INTERVAL=2;COUNT=4;BYDAY=MO,WE",
            None,
        )
        .unwrap();
        assert_eq!(
            rules,
            json!([{
                "@type": "RecurrenceRule", "frequency": "weekly", "interval": 2,
                "count": 4, "byDay": [
                    {"@type": "NDay", "day": "mo"},
                    {"@type": "NDay", "day": "we"}
                ]
            }])
        );
        for (input, timezone, until) in [
            ("20280229", None, "2028-02-29T23:59:59"),
            ("20261231T170000", None, "2026-12-31T17:00:00"),
            ("20261231T170000Z", None, "2026-12-31T17:00:00"),
            (
                "20261101T140000Z",
                Some("America/New_York"),
                "2026-11-01T09:00:00",
            ),
        ] {
            let rules =
                faithful_local_recurrence_rules(&format!("FREQ=MONTHLY;UNTIL={input}"), timezone)
                    .unwrap();
            assert_eq!(rule(&rules)["frequency"], "monthly");
            assert_eq!(rule(&rules)["until"], until);
        }
        let rules =
            faithful_local_recurrence_rules("FREQ=YEARLY;INTERVAL=1;COUNT=2", None).unwrap();
        assert_eq!(
            rules,
            json!([{"@type": "RecurrenceRule", "frequency": "yearly", "count": 2}])
        );
    }

    #[test]
    fn invitation_rules_reject_malformed_or_lossy_grammar() {
        for input in [
            "",
            "RRULE:",
            "RRULE:RRULE:FREQ=DAILY",
            "rrule:RRULE:FREQ=DAILY",
            "RRULE :FREQ=DAILY",
            "FREQ=DAILY\r\nEXDATE:20260914T100000Z",
            "FREQ=DAILY\n",
            "\rFREQ=DAILY",
            "FREQ=DAILY\t",
            "FREQ=DAILY\u{000b}",
            "FREQ=DAILY\0",
            "FREQ=DAILY\u{007f}",
            "FREQ=DAILY\u{00a0}",
            "FREQ=DÄILY",
            "FREQ=DAILY;FREQ=WEEKLY",
            "FREQ=DAILY;freq=DAILY",
            "FREQ=DAILY;",
            "FREQ=DAILY;COUNT",
            "FREQ=DAILY;COUNT=",
            "FREQ=DAILY;COUNT==2",
            "FREQ=DAILY;COUNT=bad",
            "FREQ=DAILY;COUNT=0",
            "FREQ=DAILY;COUNT=+2",
            "FREQ=DAILY;COUNT=-2",
            "FREQ=DAILY;COUNT=4294967296",
            "FREQ=DAILY;INTERVAL=0",
            "FREQ=DAILY;INTERVAL=+2",
            "FREQ=DAILY;INTERVAL=1.5",
            "FREQ=DAILY;INTERVAL=4294967296",
            "FREQ=DAILY;COUNT=2;UNTIL=20261231",
            "FREQ=DAILY;UNTIL=20260230",
            "FREQ=DAILY;UNTIL=20260230T100000",
            "FREQ=DAILY;UNTIL=20261331T100000Z",
            "FREQ=DAILY;UNTIL=20261231T250000",
            "FREQ=DAILY;UNTIL=20261231T170000ZZ",
            "FREQ=DAILY;UNTIL=20261231Z",
            "FREQ=DAILY;UNTIL=20261231T170000+0100",
            "FREQ=DAILY;UNTIL=20261231T170000.1Z",
            "FREQ=DAILY;UNTIL=2026-12-31",
            "FREQ=DAILY;BYDAY=MO",
            "FREQ=MONTHLY;BYDAY=1MO",
            "FREQ=WEEKLY;BYDAY=1MO",
            "FREQ=WEEKLY;BYDAY=MO,",
            "FREQ=WEEKLY;BYDAY=MO,NO",
            "FREQ=WEEKLY;BYDAY=MO;BYDAY=WE",
            "FREQ=WEEKLY;BYSETPOS=1",
            "FREQ=WEEKLY;WKST=SU",
            "FREQ=YEARLY;BYMONTH=10",
            "FREQ=HOURLY",
            "FREQ=DAILY;EXDATE=20261231",
            "RDATE:20261231",
            "BYDAY=MO",
            "[{\"frequency\":\"daily\"}]",
        ] {
            assert!(
                normalize_invitation_rrule(input, None).is_none(),
                "{input:?}"
            );
            assert!(
                faithful_local_recurrence_rules(input, None).is_none(),
                "{input:?}"
            );
        }
        for timezone in ["", "Mars/Olympus_Mons", "UTC\n"] {
            assert!(normalize_invitation_rrule("FREQ=DAILY", Some(timezone)).is_none());
        }
    }

    #[test]
    fn biweekly_byday_to_jscalendar() {
        // The exact rule from the bug report ("OCM checkpoint meeting").
        let rules = rrule_to_jscalendar("FREQ=WEEKLY;INTERVAL=2;BYDAY=TU", None).unwrap();
        let r = rule(&rules);
        assert_eq!(r["@type"], "RecurrenceRule");
        assert_eq!(r["frequency"], "weekly");
        assert_eq!(r["interval"], 2);
        assert_eq!(r["byDay"], json!([{ "@type": "NDay", "day": "tu" }]));
        assert!(!r.contains_key("count"));
        assert!(!r.contains_key("until"));
    }

    #[test]
    fn date_only_until_becomes_end_of_day() {
        let rules = rrule_to_jscalendar("FREQ=WEEKLY;BYDAY=WE;UNTIL=20160127", None).unwrap();
        assert_eq!(rule(&rules)["until"], "2016-01-27T23:59:59");
    }

    #[test]
    fn until_without_z_is_accepted() {
        // Seen in real CalDAV data: "UNTIL=20251231T170000" (no Z).
        let rules =
            rrule_to_jscalendar("FREQ=WEEKLY;BYDAY=MO,WE;UNTIL=20251231T170000", None).unwrap();
        assert_eq!(rule(&rules)["until"], "2025-12-31T17:00:00");
    }

    #[test]
    fn count_is_supported() {
        let rules = rrule_to_jscalendar("FREQ=WEEKLY;COUNT=2;INTERVAL=2;BYDAY=FR", None).unwrap();
        let r = rule(&rules);
        assert_eq!(r["count"], 2);
    }

    #[test]
    fn rrule_prefix_is_tolerated() {
        let rules = rrule_to_jscalendar("RRULE:FREQ=DAILY;COUNT=5", None).unwrap();
        assert_eq!(rule(&rules)["frequency"], "daily");
    }

    #[test]
    fn json_and_garbage_input_yield_none() {
        assert!(rrule_to_jscalendar("[{\"@type\":\"RecurrenceRule\"}]", None).is_none());
        assert!(rrule_to_jscalendar("", None).is_none());
        assert!(rrule_to_jscalendar("BYDAY=MO", None).is_none());
        assert!(rrule_to_jscalendar("FREQ=FORTNIGHTLY", None).is_none());
        assert!(rrule_to_jscalendar("FREQ=MONTHLY;BYDAY=1TH", None).is_none());
        assert!(rrule_to_jscalendar("FREQ=YEARLY;BYMONTH=10", None).is_none());
    }

    #[test]
    fn jscalendar_to_rrule_stalwart_style() {
        let rules = vec![json!({
            "@type": "RecurrenceRule",
            "frequency": "weekly",
            "byDay": [
                { "@type": "NDay", "day": "mo" },
                { "@type": "NDay", "day": "we" }
            ],
            "until": "2026-12-31T17:00:00"
        })];
        assert_eq!(
            jscalendar_to_rrule(&rules, None).unwrap(),
            "FREQ=WEEKLY;UNTIL=20261231T170000Z;BYDAY=MO,WE"
        );
    }

    #[test]
    fn jscalendar_to_rrule_starts_with_freq_for_frontend_parser() {
        let rules = vec![json!({ "frequency": "daily", "count": 3 })];
        let rrule = jscalendar_to_rrule(&rules, None).unwrap();
        assert!(rrule.starts_with("FREQ="), "frontend requires FREQ first");
        assert_eq!(rrule, "FREQ=DAILY;COUNT=3");
    }

    #[test]
    fn jscalendar_unsupported_constraints_yield_none() {
        let rules = vec![json!({
            "frequency": "monthly",
            "byMonth": [10],
            "bySetPosition": [-1]
        })];
        assert!(jscalendar_to_rrule(&rules, None).is_none());
    }

    #[test]
    fn jscalendar_multiple_rules_yield_none() {
        let rules = vec![
            json!({ "frequency": "weekly" }),
            json!({ "frequency": "daily" }),
        ];
        assert!(jscalendar_to_rrule(&rules, None).is_none());
    }

    #[test]
    fn jscalendar_invalid_yields_none() {
        assert!(jscalendar_to_rrule(&[], None).is_none());
        assert!(jscalendar_to_rrule(&[json!({ "count": 3 })], None).is_none());
        assert!(jscalendar_to_rrule(&[json!({ "frequency": "fortnightly" })], None).is_none());
        assert!(jscalendar_to_rrule(
            &[json!({ "frequency": "monthly", "byDay": [{ "day": "th", "nthOfPeriod": 1 }] })],
            None
        )
        .is_none());
    }

    #[test]
    fn round_trip_preserves_semantics() {
        for rrule in [
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU",
            "FREQ=DAILY;COUNT=5",
            "FREQ=MONTHLY;UNTIL=20130531T215959Z",
            "FREQ=WEEKLY;INTERVAL=2;COUNT=2;BYDAY=FR",
        ] {
            let js = rrule_to_jscalendar(rrule, None).unwrap();
            let back = jscalendar_to_rrule(js.as_array().unwrap(), None).unwrap();
            assert_eq!(back, rrule, "round trip changed the rule");
        }
    }

    #[test]
    fn until_fractional_seconds_truncated() {
        assert_eq!(
            local_until_to_ical("2026-12-31T17:00:00.000", None),
            Some("20261231T170000Z".into())
        );
        assert_eq!(
            local_until_to_ical("2026-01-27", None),
            Some("20260127".into())
        );
        assert_eq!(local_until_to_ical("not a date", None), None);
    }

    #[test]
    fn until_converts_between_utc_and_event_timezone() {
        let rules = rrule_to_jscalendar(
            "FREQ=WEEKLY;UNTIL=20261101T140000Z",
            Some("America/New_York"),
        )
        .unwrap();
        assert_eq!(rule(&rules)["until"], "2026-11-01T09:00:00");
        assert_eq!(
            jscalendar_to_rrule(rules.as_array().unwrap(), Some("America/New_York")),
            Some("FREQ=WEEKLY;UNTIL=20261101T140000Z".into()),
        );
    }
}
