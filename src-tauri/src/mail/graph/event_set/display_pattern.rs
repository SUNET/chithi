//! Graph's six read-only recurrence patterns, independent of mutation support.
//! https://learn.microsoft.com/en-us/graph/api/resources/recurrencepattern

use chrono::Datelike;

use super::{append_rule_range, invalid, CalendarEvent, CalendarOverride, Result, Value, DAYS};

fn integer(pattern: &Value, key: &str, maximum: u64) -> Result<u64> {
    pattern[key]
        .as_u64()
        .filter(|value| (1..=maximum).contains(value))
        .ok_or_else(|| invalid(format!("invalid display recurrence {key}")))
}

fn weekday(value: &Value) -> Result<&'static str> {
    DAYS.iter()
        .find(|(_, name)| Some(*name) == value.as_str())
        .map(|(code, _)| *code)
        .ok_or_else(|| invalid("invalid display recurrence weekday"))
}

fn days(pattern: &Value) -> Result<String> {
    let days = pattern["daysOfWeek"]
        .as_array()
        .filter(|days| !days.is_empty() && days.len() <= DAYS.len())
        .ok_or_else(|| invalid("display recurrence weekdays are missing or excessive"))?;
    let mut codes = Vec::new();
    for day in days {
        let code = weekday(day)?;
        if codes.contains(&code) {
            return Err(invalid("duplicate display recurrence weekday"));
        }
        codes.push(code);
    }
    Ok(codes.join(","))
}

pub(super) fn project_recurrence(recurrence: &Value, event: &mut CalendarEvent) -> Result<String> {
    let pattern = &recurrence["pattern"];
    let kind = pattern["type"]
        .as_str()
        .ok_or_else(|| invalid("missing display recurrence type"))?;
    let frequency = match kind {
        "daily" => "DAILY",
        "weekly" => "WEEKLY",
        "absoluteMonthly" | "relativeMonthly" => "MONTHLY",
        "absoluteYearly" | "relativeYearly" => "YEARLY",
        _ => return Err(invalid("unknown display recurrence type")),
    };
    // The bounded display engine represents intervals as u16, not the
    // editor's 1..=99 subset. Larger provider intervals remain explicit errors.
    let interval = integer(pattern, "interval", u16::MAX.into())?;
    let mut rule = format!("FREQ={frequency}");
    match kind {
        "weekly" => {
            rule.push_str(&format!(";BYDAY={}", days(pattern)?));
            let first = pattern
                .get("firstDayOfWeek")
                .map(weekday)
                .transpose()?
                .unwrap_or("SU");
            rule.push_str(&format!(";WKST={first}"));
        }
        "absoluteMonthly" | "absoluteYearly" => {
            rule.push_str(&format!(
                ";BYMONTHDAY={}",
                integer(pattern, "dayOfMonth", 31)?
            ));
        }
        "relativeMonthly" | "relativeYearly" => {
            let index = match pattern.get("index") {
                None => 1,
                Some(value) => match value.as_str() {
                    Some("first") => 1,
                    Some("second") => 2,
                    Some("third") => 3,
                    Some("fourth") => 4,
                    Some("last") => -1,
                    _ => return Err(invalid("invalid display recurrence index")),
                },
            };
            // Select one instance among the allowed days, not one instance
            // of each weekday. This also handles "last weekday of the month".
            rule.push_str(&format!(";BYDAY={};BYSETPOS={index}", days(pattern)?));
        }
        _ => {}
    }
    if matches!(kind, "absoluteYearly" | "relativeYearly") {
        rule.push_str(&format!(";BYMONTH={}", integer(pattern, "month", 12)?));
    }
    let (_, local) = super::super::recurring_graph_time(
        &event.start_time,
        event.all_day,
        event.timezone.as_deref(),
    )?;
    // Graph's startDate is a range boundary, not necessarily an occurrence.
    // Its documented relativeMonthly example starts August 29, interval=2,
    // first Thursday, but first occurs in September (not October). Anchor RFC
    // interval counting at the first matching occurrence, without publishing
    // the range boundary as an invented event or altering the native payload.
    let mut bounded = rule.clone();
    bounded.push_str(&format!(";INTERVAL={interval}"));
    append_rule_range(&recurrence["range"], local.date(), &mut bounded)?;
    // Preserve the existing implicit-day grammar when the original anchor
    // already supplies these exact selectors. Besides being equivalent RFC,
    // this retains the strict legacy cancellation codec for supported series.
    let same_day = pattern["dayOfMonth"].as_u64() == Some(local.day().into());
    if (kind == "absoluteMonthly" && same_day)
        || (kind == "absoluteYearly"
            && same_day
            && pattern["month"].as_u64() == Some(local.month().into()))
    {
        bounded = bounded
            .split(';')
            .filter(|part| !part.starts_with("BYMONTHDAY=") && !part.starts_with("BYMONTH="))
            .collect::<Vec<_>>()
            .join(";");
    }
    anchor_first_occurrence(&rule, kind, event)?;
    crate::calendar::display_recurrence::normalize_rule(&bounded, event)
}

fn anchor_first_occurrence(rule: &str, kind: &str, event: &mut CalendarEvent) -> Result<()> {
    if kind == "daily" {
        return Ok(());
    }
    let mut candidate = event.clone();
    candidate.recurrence_rule = Some(rule.to_owned());
    // Eight years cover the longest Gregorian gap between valid February 29
    // dates (including a non-leap century). Impossible dates fail explicitly.
    let days = match kind {
        "weekly" => 14,
        "absoluteMonthly" | "relativeMonthly" => 62,
        _ => 366 * 8,
    };
    let (lower, upper) = if event.all_day {
        let start =
            chrono::NaiveDate::parse_from_str(&event.start_time, "%Y-%m-%d").map_err(invalid)?;
        let upper = start
            .checked_add_signed(chrono::Duration::days(days))
            .ok_or_else(|| invalid("display recurrence anchor overflow"))?;
        (start.to_string(), upper.to_string())
    } else {
        let start = super::instant(&event.start_time)?;
        let upper = start
            .checked_add_signed(chrono::Duration::days(days))
            .ok_or_else(|| invalid("display recurrence anchor overflow"))?;
        (super::utc(start), super::utc(upper))
    };
    let first = crate::calendar::display_recurrence::expand(&candidate, &lower, &upper, 1)?
        .occurrences
        .into_iter()
        .next()
        .ok_or_else(|| invalid("display recurrence has no matching Gregorian date"))?;
    event.start_time = first.fields.start_time;
    event.end_time = first.fields.end_time;
    Ok(())
}

/// An explicitly cancelled exception is a verified original exclusion, even
/// when Graph's separate cancelledOccurrences list is empty. Prove all such
/// originals in one bounded pass; never infer positions from occurrence IDs.
pub(super) fn verify_cancelled_exceptions(
    event: &CalendarEvent,
    overrides: &[CalendarOverride],
) -> Result<()> {
    let positions = overrides
        .iter()
        .filter(|item| item.event.is_none())
        .map(|item| item.original_start.clone())
        .collect::<Vec<_>>();
    if positions.is_empty() {
        return Ok(());
    }
    let mut last = None;
    for position in &positions {
        let time = if event.all_day {
            chrono::NaiveDate::parse_from_str(position, "%Y-%m-%d")
                .map_err(invalid)?
                .and_hms_opt(0, 0, 0)
                .ok_or_else(|| invalid("invalid cancelled exception date"))?
                .and_utc()
        } else {
            super::instant(position)?
        };
        last = Some(last.map_or(time, |last: chrono::DateTime<chrono::Utc>| last.max(time)));
    }
    let upper = last
        .expect("nonempty cancelled original positions")
        .checked_add_signed(if event.all_day {
            chrono::Duration::days(1)
        } else {
            chrono::Duration::nanoseconds(1)
        })
        .ok_or_else(|| invalid("cancelled exception proof overflow"))?;
    crate::calendar::display_recurrence::verify_original_positions(
        event,
        &super::utc(upper),
        &positions,
    )
    .map_err(invalid)?;
    Ok(())
}
