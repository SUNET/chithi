//! Bounded RFC 5545 display projection. Reading a rule never grants permission
//! to edit it; structural mutations continue to use the editor's small grammar.

use std::collections::HashSet;
use std::ops::ControlFlow;

use chrono::{
    DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Timelike, Utc,
};
use rrule::{Frequency, RRule, RRuleSet, Tz, Unvalidated};

use super::event_set::event_fields;
use super::recurrence_projection::{Expansion, GeneratedOccurrence};
use super::CalendarEvent;
use crate::error::{Error, Result};

mod equivalence;
pub(crate) use equivalence::equivalent_rule_cutoffs;

// Bound historical enumeration as well as results in the requested window. The
// crate's `after` filter alone does not bound work spent reaching that window.
const GENERATED_BUDGET: u16 = 50_000;
const PERIOD_BUDGET: i64 = 90_000;

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::Other(format!("recurrence display: {message}"))
}

fn boundary(value: &str) -> Result<DateTime<Utc>> {
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return date
            .and_hms_opt(0, 0, 0)
            .map(|value| value.and_utc())
            .ok_or_else(|| invalid("invalid date"));
    }
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(invalid)
}

fn engine_date(value: DateTime<Utc>) -> Result<()> {
    if !(1..=9998).contains(&value.year()) {
        return Err(invalid("date exceeds recurrence engine range"));
    }
    Ok(())
}

fn zone(event: &CalendarEvent) -> Result<Tz> {
    if event.all_day {
        return Ok(Tz::UTC);
    }
    let zone: chrono_tz::Tz = event
        .timezone
        .as_deref()
        .unwrap_or("UTC")
        .parse()
        .map_err(invalid)?;
    Ok(zone.into())
}

/// Date cutoffs include the whole event-local date. Explicit UTC cutoffs remain
/// instants even on all-day upstream events; never truncate them to a date.
fn rule_text(value: &str, zone: Tz) -> Result<String> {
    if value.len() > 4096 || !value.is_ascii() || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(invalid("RRULE must be at most 4096 printable ASCII bytes"));
    }
    let uppercase = value.trim().to_ascii_uppercase();
    let value = uppercase.strip_prefix("RRULE:").unwrap_or(&uppercase);
    let mut keys = HashSet::new();
    let mut parts = Vec::new();
    for part in value.split(';') {
        let (key, value) = part
            .split_once('=')
            .ok_or_else(|| invalid("malformed RRULE part"))?;
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() || !keys.insert(key) {
            return Err(invalid("empty or duplicate RRULE part"));
        }
        if key == "UNTIL" && !value.ends_with('Z') {
            let local = if value.len() == 8 {
                NaiveDate::parse_from_str(value, "%Y%m%d")
                    .map_err(invalid)?
                    .and_hms_opt(23, 59, 59)
                    .ok_or_else(|| invalid("invalid cutoff"))?
            } else {
                NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").map_err(invalid)?
            };
            let instant = zone
                .from_local_datetime(&local)
                .single()
                .ok_or_else(|| invalid("ambiguous or nonexistent cutoff"))?;
            parts.push(format!(
                "UNTIL={}",
                instant.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ")
            ));
        } else {
            parts.push(format!("{key}={value}"));
        }
    }
    if keys.contains("COUNT") && keys.contains("UNTIL") {
        return Err(invalid("COUNT and UNTIL cannot be combined"));
    }
    Ok(parts.join(";"))
}

fn check_work(rule: &RRule, start: DateTime<Utc>, upper: DateTime<Utc>) -> Result<()> {
    let unit = match rule.get_freq() {
        Frequency::Yearly => 365 * 86400,
        Frequency::Monthly => 28 * 86400,
        Frequency::Weekly => 7 * 86400,
        Frequency::Daily => 86400,
        Frequency::Hourly => 3600,
        Frequency::Minutely => 60,
        Frequency::Secondly => 1,
    };
    let interval = i64::from(rule.get_interval());
    if interval == 0
        || upper.signed_duration_since(start).num_seconds() / unit / interval > PERIOD_BUDGET
    {
        return Err(invalid(
            "historical expansion exceeds the display work budget",
        ));
    }
    // A yearly BYHOUR/BYMINUTE/BYSECOND cross product can allocate millions of
    // candidates before yielding even one result. Bound that per-period work.
    let candidates = rule.get_by_hour().len().max(1)
        * rule.get_by_minute().len().max(1)
        * rule.get_by_second().len().max(1);
    let days = match rule.get_freq() {
        Frequency::Yearly => 366,
        Frequency::Monthly => 31,
        Frequency::Weekly => 7,
        _ => 1,
    };
    if candidates * days > usize::from(GENERATED_BUDGET) {
        return Err(invalid(
            "per-period expansion exceeds the display work budget",
        ));
    }
    Ok(())
}

fn validate_rule(
    text: &str,
    start: DateTime<Utc>,
    zone: Tz,
    all_day: bool,
) -> Result<Option<RRule>> {
    engine_date(start)?;
    let raw: RRule<Unvalidated> = text.parse().map_err(invalid)?;
    let expired = raw
        .get_until()
        .is_some_and(|until| until.with_timezone(&Utc) < start);
    let dtstart = start
        .with_nanosecond(0)
        .ok_or_else(|| invalid("invalid start"))?
        .with_timezone(&zone);
    // Validate every other part even when an expired upstream cutoff supplies
    // no positions. The adjusted cutoff is never persisted or rendered.
    let raw = if expired {
        raw.until(dtstart.with_timezone(&Tz::UTC))
    } else {
        raw
    };
    let raw = if all_day {
        if matches!(
            raw.get_freq(),
            Frequency::Hourly | Frequency::Minutely | Frequency::Secondly
        ) {
            return Err(invalid("sub-day recurrence requires a timed DTSTART"));
        }
        // RFC 5545 ignores time-of-day BY parts for DATE-valued DTSTART.
        // Project midnight dates, without duplicate day identities.
        raw.by_hour(vec![0]).by_minute(vec![0]).by_second(vec![0])
    } else {
        raw
    };
    let rule = raw.validate(dtstart).map_err(invalid)?;
    Ok((!expired).then_some(rule))
}

pub(crate) fn normalize_rule(value: &str, event: &CalendarEvent) -> Result<String> {
    event_fields(event).validate_display()?;
    let zone = zone(event)?;
    let text = rule_text(value, zone)?;
    validate_rule(&text, boundary(&event.start_time)?, zone, event.all_day)?;
    Ok(text)
}

pub(crate) fn expand(
    event: &CalendarEvent,
    lower: &str,
    upper: &str,
    limit: usize,
) -> Result<Expansion> {
    event_fields(event).validate_display()?;
    if limit > 10_000 {
        return Err(invalid("result limit exceeds 10000"));
    }
    let (lower, upper) = (boundary(lower)?, boundary(upper)?);
    engine_date(lower)?;
    engine_date(upper)?;
    if lower >= upper {
        return Err(invalid("window end must be after start"));
    }
    let (start, end) = (boundary(&event.start_time)?, boundary(&event.end_time)?);
    let duration = end.signed_duration_since(start);
    let mut expansion = Expansion {
        occurrences: Vec::new(),
        has_more: false,
    };
    visit_original_starts(event, lower, upper, |generated| {
        let end = generated
            .checked_add_signed(duration)
            .ok_or_else(|| invalid("end overflow"))?;
        if end <= lower && !(duration == Duration::zero() && generated >= lower) {
            return Ok(ControlFlow::Continue(()));
        }
        if expansion.occurrences.len() == limit {
            expansion.has_more = true;
            return Ok(ControlFlow::Break(()));
        }
        let mut fields = event_fields(event);
        fields.start_time = format_position(event, generated);
        fields.end_time = format_position(event, end);
        expansion.occurrences.push(GeneratedOccurrence {
            original_start: fields.start_time.clone(),
            fields,
        });
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(expansion)
}

fn format_position(event: &CalendarEvent, time: DateTime<Utc>) -> String {
    if event.all_day {
        time.date_naive().to_string()
    } else {
        time.to_rfc3339_opts(SecondsFormat::AutoSi, true)
    }
}

/// Original positions before exclusions/overrides, for read-only provider proofs.
/// Enumeration shares the display engine's COUNT, DST and work-budget semantics.
#[derive(Debug)]
pub(crate) struct OriginalPositionSummary {
    pub count: u32,
    pub first: Option<String>,
    pub last: Option<String>,
}

pub(crate) fn verify_original_positions(
    event: &CalendarEvent,
    upper: &str,
    required: &[String],
) -> Result<OriginalPositionSummary> {
    let mut remaining = required.iter().map(String::as_str).collect::<HashSet<_>>();
    let mut summary = OriginalPositionSummary {
        count: 0,
        first: None,
        last: None,
    };
    visit_original_starts(
        event,
        boundary(&event.start_time)?,
        boundary(upper)?,
        |generated| {
            let position = format_position(event, generated);
            remaining.remove(position.as_str());
            summary.count += 1;
            summary.first.get_or_insert_with(|| position.clone());
            summary.last = Some(position);
            Ok(ControlFlow::Continue(()))
        },
    )?;
    if !remaining.is_empty() {
        return Err(invalid(
            "requested original position is outside the recurrence",
        ));
    }
    Ok(summary)
}

fn visit_original_starts(
    event: &CalendarEvent,
    lower: DateTime<Utc>,
    upper: DateTime<Utc>,
    mut visit: impl FnMut(DateTime<Utc>) -> Result<ControlFlow<()>>,
) -> Result<()> {
    event_fields(event).validate_display()?;
    engine_date(lower)?;
    engine_date(upper)?;
    let (start, end) = (boundary(&event.start_time)?, boundary(&event.end_time)?);
    let duration = end.signed_duration_since(start);
    let zone = zone(event)?;
    let text = rule_text(
        event
            .recurrence_rule
            .as_deref()
            .ok_or_else(|| invalid("missing RRULE"))?,
        zone,
    )?;
    let Some(rule) = validate_rule(&text, start, zone, event.all_day)? else {
        return Ok(());
    };
    if upper <= start {
        return Ok(());
    }
    let cutoff = rule
        .get_until()
        .map(|until| until.with_timezone(&Utc))
        .unwrap_or(upper)
        .min(upper);
    if cutoff
        .checked_add_signed(duration)
        .is_some_and(|end| end < lower)
    {
        return Ok(());
    }
    check_work(&rule, start, cutoff)?;
    // The library shifts nonexistent local times forward. Generate the RFC
    // wall-clock pattern in UTC instead, then resolve it explicitly: gaps are
    // skipped (and not counted); folds use the first occurrence of that time.
    let dtstart = start
        .with_timezone(&zone)
        .naive_local()
        .with_nanosecond(0)
        .ok_or_else(|| invalid("invalid start"))?
        .and_utc()
        .with_timezone(&Tz::UTC);
    let unbounded = text
        .split(';')
        .filter(|part| !part.starts_with("COUNT=") && !part.starts_with("UNTIL="))
        .collect::<Vec<_>>()
        .join(";");
    let civil_rule = validate_rule(
        &unbounded,
        dtstart.with_timezone(&Utc),
        Tz::UTC,
        event.all_day,
    )?
    .ok_or_else(|| invalid("unexpected expired wall-clock rule"))?;
    // UTC offsets are less than one day. This envelope also covers offset
    // changes inside the window, without relying on either endpoint's offset.
    let civil_upper = cutoff
        .checked_add_signed(Duration::days(1))
        .ok_or_else(|| invalid("window overflow"))?
        .with_timezone(&Tz::UTC);
    let dates = RRuleSet::new(dtstart).rrule(civil_rule).limit();
    let mut dates = dates.into_iter();
    let mut count = 0;
    let mut generated_count = 0;
    loop {
        if rule.get_count().is_some_and(|maximum| count >= maximum) {
            return Ok(());
        }
        // Stream instead of collecting the whole history before checking COUNT
        // or the requested result limit. Keep both candidate and engine limits.
        if generated_count == GENERATED_BUDGET {
            return Err(invalid("expansion exceeded the display work budget"));
        }
        let generated = dates.next().ok_or_else(|| {
            invalid("recurrence engine stopped before the window boundary; display work budget exhausted")
        })?;
        if generated > civil_upper {
            return Ok(());
        }
        generated_count += 1;
        let local = generated
            .naive_utc()
            .with_nanosecond(start.nanosecond())
            .ok_or_else(|| invalid("invalid fractional start"))?;
        let generated = if local == start.with_timezone(&zone).naive_local() {
            // Explicit DTSTART is already an instant, including a provider's
            // choice of the second instant in an ambiguous clock hour.
            start
        } else {
            let Some(generated) = zone.from_local_datetime(&local).earliest() else {
                continue;
            };
            generated.with_timezone(&Utc)
        };
        if generated < start {
            continue;
        }
        if generated > cutoff {
            continue;
        }
        count += 1;
        if generated >= upper {
            continue;
        }
        if visit(generated)?.is_break() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests;
