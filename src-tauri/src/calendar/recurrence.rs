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
//! Only the fields the rest of the app understands are mapped
//! (FREQ/INTERVAL/COUNT/UNTIL/BYDAY/BYMONTHDAY/BYMONTH/BYSETPOS/WKST).
//! Unknown fields are ignored rather than failing the whole rule.
//!
//! UNTIL note: RRULE's UNTIL is (usually) a UTC instant while
//! JSCalendar's `until` is a floating local date-time. We convert by
//! dropping/appending the `Z` — exact enough for boundary handling in
//! every consumer we have; a date-only UNTIL becomes end-of-day so the
//! final occurrence stays included.

use serde_json::{json, Value};

const DAY_CODES: [&str; 7] = ["su", "mo", "tu", "we", "th", "fr", "sa"];

fn is_valid_day(code: &str) -> bool {
    DAY_CODES.contains(&code)
}

/// Convert an iCal RRULE value string into a JSCalendar `recurrenceRules`
/// array (a one-element array — RRULE describes a single rule).
/// Returns `None` when the input isn't an RRULE string (e.g. legacy rows
/// that hold raw JSON) or lacks a usable FREQ.
pub fn rrule_to_jscalendar(rrule: &str) -> Option<Value> {
    let rrule = rrule.trim().trim_start_matches("RRULE:");
    if rrule.is_empty() {
        return None;
    }

    let mut freq = None;
    let mut interval = None;
    let mut count = None;
    let mut until = None;
    let mut by_day = None;
    let mut by_month_day = None;
    let mut by_month = None;
    let mut by_set_position = None;
    let mut wkst = None;

    for part in rrule.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_uppercase().as_str() {
            "FREQ" => {
                let f = value.to_ascii_lowercase();
                if matches!(
                    f.as_str(),
                    "yearly" | "monthly" | "weekly" | "daily" | "hourly" | "minutely" | "secondly"
                ) {
                    freq = Some(f);
                }
            }
            "INTERVAL" => interval = value.parse::<u32>().ok().filter(|n| *n > 1),
            "COUNT" => count = value.parse::<u32>().ok(),
            "UNTIL" => until = ical_until_to_local(value),
            "BYDAY" => {
                let days: Vec<Value> = value.split(',').filter_map(parse_nday).collect();
                if !days.is_empty() {
                    by_day = Some(days);
                }
            }
            "BYMONTHDAY" => {
                let days: Vec<i64> = value.split(',').filter_map(|d| d.parse().ok()).collect();
                if !days.is_empty() {
                    by_month_day = Some(days);
                }
            }
            "BYMONTH" => {
                // JSCalendar months are strings ("10"), iCal's are ints.
                let months: Vec<String> = value
                    .split(',')
                    .filter(|m| m.parse::<u8>().is_ok())
                    .map(|m| m.to_string())
                    .collect();
                if !months.is_empty() {
                    by_month = Some(months);
                }
            }
            "BYSETPOS" => {
                let positions: Vec<i64> = value.split(',').filter_map(|p| p.parse().ok()).collect();
                if !positions.is_empty() {
                    by_set_position = Some(positions);
                }
            }
            "WKST" => {
                let day = value.to_ascii_lowercase();
                if is_valid_day(&day) {
                    wkst = Some(day);
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
    if let Some(d) = by_month_day {
        rule.insert("byMonthDay".into(), json!(d));
    }
    if let Some(m) = by_month {
        rule.insert("byMonth".into(), json!(m));
    }
    if let Some(p) = by_set_position {
        rule.insert("bySetPosition".into(), json!(p));
    }
    if let Some(w) = wkst {
        rule.insert("firstDayOfWeek".into(), json!(w));
    }

    Some(Value::Array(vec![Value::Object(rule)]))
}

/// Convert a JSCalendar `recurrenceRules` array into an iCal RRULE value
/// string. Only the first rule is used — RRULE can express exactly one,
/// and multiple RRULEs are deprecated since RFC 5545.
/// Returns `None` when there is no rule with a usable frequency.
pub fn jscalendar_to_rrule(rules: &[Value]) -> Option<String> {
    let rule = rules.first()?;
    if rules.len() > 1 {
        log::warn!(
            "jscalendar_to_rrule: {} recurrence rules on event, using the first",
            rules.len()
        );
    }

    let freq = rule["frequency"].as_str()?;
    let freq = freq.to_ascii_uppercase();
    if !matches!(
        freq.as_str(),
        "YEARLY" | "MONTHLY" | "WEEKLY" | "DAILY" | "HOURLY" | "MINUTELY" | "SECONDLY"
    ) {
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
        if let Some(u) = local_until_to_ical(until) {
            parts.push(format!("UNTIL={}", u));
        }
    }
    if let Some(days) = rule["byDay"].as_array() {
        let tokens: Vec<String> = days.iter().filter_map(format_nday).collect();
        if !tokens.is_empty() {
            parts.push(format!("BYDAY={}", tokens.join(",")));
        }
    }
    if let Some(days) = rule["byMonthDay"].as_array() {
        let tokens: Vec<String> = days
            .iter()
            .filter_map(|d| d.as_i64())
            .map(|d| d.to_string())
            .collect();
        if !tokens.is_empty() {
            parts.push(format!("BYMONTHDAY={}", tokens.join(",")));
        }
    }
    if let Some(months) = rule["byMonth"].as_array() {
        // JSCalendar months are strings, possibly "5L" for leap months in
        // non-Gregorian rscales — skip those, we only handle Gregorian.
        let tokens: Vec<String> = months
            .iter()
            .filter_map(|m| match m {
                Value::String(s) if s.parse::<u8>().is_ok() => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .collect();
        if !tokens.is_empty() {
            parts.push(format!("BYMONTH={}", tokens.join(",")));
        }
    }
    if let Some(positions) = rule["bySetPosition"].as_array() {
        let tokens: Vec<String> = positions
            .iter()
            .filter_map(|p| p.as_i64())
            .map(|p| p.to_string())
            .collect();
        if !tokens.is_empty() {
            parts.push(format!("BYSETPOS={}", tokens.join(",")));
        }
    }
    if let Some(wkst) = rule["firstDayOfWeek"].as_str() {
        if is_valid_day(&wkst.to_ascii_lowercase()) {
            parts.push(format!("WKST={}", wkst.to_ascii_uppercase()));
        }
    }

    Some(parts.join(";"))
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
fn ical_until_to_local(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches('Z');
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
        return Some(format!(
            "{}-{}-{}T{}:{}:{}",
            &value[0..4],
            &value[4..6],
            &value[6..8],
            &value[9..11],
            &value[11..13],
            &value[13..15]
        ));
    }
    None
}

/// JSCalendar LocalDateTime ("2026-12-31T17:00:00", fractional seconds
/// tolerated) → iCal UNTIL ("20261231T170000Z"); date-only → "20261231".
fn local_until_to_ical(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches('Z');
    let compact: String = value
        .chars()
        .take(19) // "YYYY-MM-DDTHH:MM:SS", drops fractional seconds
        .filter(|c| c.is_ascii_digit() || *c == 'T')
        .collect();

    match compact.len() {
        8 => Some(compact),                  // date only
        15 => Some(format!("{}Z", compact)), // date-time
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
    fn biweekly_byday_to_jscalendar() {
        // The exact rule from the bug report ("OCM checkpoint meeting").
        let rules = rrule_to_jscalendar("FREQ=WEEKLY;INTERVAL=2;BYDAY=TU").unwrap();
        let r = rule(&rules);
        assert_eq!(r["@type"], "RecurrenceRule");
        assert_eq!(r["frequency"], "weekly");
        assert_eq!(r["interval"], 2);
        assert_eq!(r["byDay"], json!([{ "@type": "NDay", "day": "tu" }]));
        assert!(!r.contains_key("count"));
        assert!(!r.contains_key("until"));
    }

    #[test]
    fn monthly_nth_weekday_with_until() {
        let rules = rrule_to_jscalendar("FREQ=MONTHLY;BYDAY=1TH;UNTIL=20130531T215959Z").unwrap();
        let r = rule(&rules);
        assert_eq!(r["frequency"], "monthly");
        assert_eq!(r["until"], "2013-05-31T21:59:59");
        assert_eq!(
            r["byDay"],
            json!([{ "@type": "NDay", "day": "th", "nthOfPeriod": 1 }])
        );
    }

    #[test]
    fn yearly_bymonth_bymonthday() {
        let rules = rrule_to_jscalendar("FREQ=YEARLY;BYMONTH=10;BYMONTHDAY=20").unwrap();
        let r = rule(&rules);
        assert_eq!(r["frequency"], "yearly");
        assert_eq!(r["byMonth"], json!(["10"]));
        assert_eq!(r["byMonthDay"], json!([20]));
    }

    #[test]
    fn date_only_until_becomes_end_of_day() {
        let rules = rrule_to_jscalendar("FREQ=WEEKLY;BYDAY=WE;UNTIL=20160127").unwrap();
        assert_eq!(rule(&rules)["until"], "2016-01-27T23:59:59");
    }

    #[test]
    fn until_without_z_is_accepted() {
        // Seen in real CalDAV data: "UNTIL=20251231T170000" (no Z).
        let rules = rrule_to_jscalendar("FREQ=WEEKLY;BYDAY=MO,WE;UNTIL=20251231T170000").unwrap();
        assert_eq!(rule(&rules)["until"], "2025-12-31T17:00:00");
    }

    #[test]
    fn wkst_and_count() {
        let rules = rrule_to_jscalendar("FREQ=WEEKLY;COUNT=2;INTERVAL=2;BYDAY=FR;WKST=SU").unwrap();
        let r = rule(&rules);
        assert_eq!(r["count"], 2);
        assert_eq!(r["firstDayOfWeek"], "su");
    }

    #[test]
    fn rrule_prefix_is_tolerated() {
        let rules = rrule_to_jscalendar("RRULE:FREQ=DAILY;COUNT=5").unwrap();
        assert_eq!(rule(&rules)["frequency"], "daily");
    }

    #[test]
    fn json_and_garbage_input_yield_none() {
        assert!(rrule_to_jscalendar("[{\"@type\":\"RecurrenceRule\"}]").is_none());
        assert!(rrule_to_jscalendar("").is_none());
        assert!(rrule_to_jscalendar("BYDAY=MO").is_none());
        assert!(rrule_to_jscalendar("FREQ=FORTNIGHTLY").is_none());
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
            jscalendar_to_rrule(&rules).unwrap(),
            "FREQ=WEEKLY;UNTIL=20261231T170000Z;BYDAY=MO,WE"
        );
    }

    #[test]
    fn jscalendar_to_rrule_starts_with_freq_for_frontend_parser() {
        let rules = vec![json!({ "frequency": "daily", "count": 3 })];
        let rrule = jscalendar_to_rrule(&rules).unwrap();
        assert!(rrule.starts_with("FREQ="), "frontend requires FREQ first");
        assert_eq!(rrule, "FREQ=DAILY;COUNT=3");
    }

    #[test]
    fn jscalendar_numeric_bymonth_and_setpos() {
        let rules = vec![json!({
            "frequency": "monthly",
            "byMonth": [10],
            "bySetPosition": [-1]
        })];
        assert_eq!(
            jscalendar_to_rrule(&rules).unwrap(),
            "FREQ=MONTHLY;BYMONTH=10;BYSETPOS=-1"
        );
    }

    #[test]
    fn jscalendar_multiple_rules_uses_first() {
        let rules = vec![
            json!({ "frequency": "weekly" }),
            json!({ "frequency": "daily" }),
        ];
        assert_eq!(jscalendar_to_rrule(&rules).unwrap(), "FREQ=WEEKLY");
    }

    #[test]
    fn jscalendar_invalid_yields_none() {
        assert!(jscalendar_to_rrule(&[]).is_none());
        assert!(jscalendar_to_rrule(&[json!({ "count": 3 })]).is_none());
        assert!(jscalendar_to_rrule(&[json!({ "frequency": "fortnightly" })]).is_none());
    }

    #[test]
    fn round_trip_preserves_semantics() {
        for rrule in [
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU",
            "FREQ=DAILY;COUNT=5",
            "FREQ=MONTHLY;UNTIL=20130531T215959Z;BYDAY=1TH",
            "FREQ=WEEKLY;INTERVAL=2;COUNT=2;BYDAY=FR;WKST=SU",
            "FREQ=YEARLY;BYMONTHDAY=20;BYMONTH=10",
        ] {
            let js = rrule_to_jscalendar(rrule).unwrap();
            let back = jscalendar_to_rrule(js.as_array().unwrap()).unwrap();
            assert_eq!(back, rrule, "round trip changed the rule");
        }
    }

    #[test]
    fn until_fractional_seconds_truncated() {
        assert_eq!(
            local_until_to_ical("2026-12-31T17:00:00.000"),
            Some("20261231T170000Z".into())
        );
        assert_eq!(local_until_to_ical("2026-01-27"), Some("20260127".into()));
        assert_eq!(local_until_to_ical("not a date"), None);
    }
}
