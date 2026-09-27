//! The deliberately small recurrence language supported by the calendar editor.
//!
//! This generates original series positions, before exclusions or overrides. Dates
//! are Gregorian, in years 0001..=9999. Timed positions use the DTSTART wall clock
//! in its IANA zone (UTC when absent), and preserve elapsed duration. Ambiguous
//! positions use the earlier instant; nonexistent positions do not consume COUNT.
//! DTSTART itself always retains its supplied instant, including in an overlap.
//!
//! Window bounds are half-open. ISO dates mean UTC midnight, including for timed
//! events; all-day dates use that same comparison axis, without timezone shifts.
//! Date-valued UNTIL, in contrast, includes the entire event-local calendar date.
//! Work beyond the explicit candidate budget is an error, never completeness.

use chrono::{
    DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone,
    Timelike, Utc,
};
use chrono_tz::Tz;

use super::{recurrence_identity::OccurrenceFields, CalendarEvent};
use crate::error::{Error, Result};

const CANDIDATE_BUDGET: usize = 200_000;
const MAX_RESULTS: usize = 10_000;
const DAY_CODES: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedOccurrence {
    /// UTC RFC 3339 with `Z`, or an ISO date for an all-day position.
    pub original_start: String,
    pub fields: OccurrenceFields,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    pub occurrences: Vec<GeneratedOccurrence>,
    /// True only when another overlapping position was actually found.
    pub has_more: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frequency {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Debug, Clone, Copy)]
enum Until {
    Date(NaiveDate),
    Instant(DateTime<Utc>),
}

#[derive(Debug)]
struct Rule {
    frequency: Frequency,
    interval: u32,
    by_day: Option<Vec<u32>>,
    week_start: u32,
    count: Option<u32>,
    until: Option<Until>,
}

fn invalid(message: &str) -> Error {
    Error::Other(format!("simple recurrence: {message}"))
}

fn range_error() -> Error {
    invalid("position or boundary exceeds the supported date range (0001..9999)")
}

fn check_date(date: NaiveDate) -> Result<NaiveDate> {
    if (1..=9999).contains(&date.year()) {
        Ok(date)
    } else {
        Err(range_error())
    }
}

fn date(value: &str) -> Result<NaiveDate> {
    if value.len() != 10
        || !value.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        })
    {
        return Err(invalid("expected an ISO date (YYYY-MM-DD)"));
    }
    check_date(
        NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map_err(|_| invalid("invalid calendar date"))?,
    )
}

fn instant(value: &str) -> Result<DateTime<Utc>> {
    if value.len() > 64 {
        return Err(invalid("invalid RFC 3339 instant"));
    }
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|_| invalid("expected an RFC 3339 instant with an explicit offset"))?
        .with_timezone(&Utc);
    check_date(parsed.date_naive())?;
    if parsed.nanosecond() >= 1_000_000_000 {
        return Err(invalid("leap-second timestamps are unsupported"));
    }
    Ok(parsed)
}

fn midnight(value: NaiveDate) -> Result<DateTime<Utc>> {
    Ok(value
        .and_hms_opt(0, 0, 0)
        .ok_or_else(range_error)?
        .and_utc())
}

fn boundary(value: &str) -> Result<DateTime<Utc>> {
    if value.len() == 10 {
        midnight(date(value)?)
    } else {
        instant(value)
    }
}

fn utc_string(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn positive_integer(value: &str) -> Result<u32> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("COUNT and INTERVAL require positive integers"));
    }
    value
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid("COUNT or INTERVAL is zero or exceeds u32"))
}

fn weekday(value: &str) -> Result<u32> {
    DAY_CODES
        .iter()
        .position(|code| *code == value)
        .map(|index| index as u32)
        .ok_or_else(|| invalid("expected an unqualified weekday MO..SU"))
}

fn parse_until(value: &str) -> Result<Until> {
    if value.len() == 10 {
        return Ok(Until::Date(date(value)?));
    }
    if value.len() == 8 && value.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(Until::Date(check_date(
            NaiveDate::parse_from_str(value, "%Y%m%d")
                .map_err(|_| invalid("invalid UNTIL date"))?,
        )?));
    }
    if value.len() != 16
        || !value.bytes().enumerate().all(|(i, b)| match i {
            8 => b == b'T',
            15 => b == b'Z',
            _ => b.is_ascii_digit(),
        })
    {
        return Err(invalid("UNTIL must be a date or RFC 5545 UTC timestamp"));
    }
    let parsed = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ")
        .map_err(|_| invalid("invalid UNTIL timestamp"))?;
    check_date(parsed.date())?;
    if parsed.nanosecond() >= 1_000_000_000 {
        return Err(invalid("UNTIL leap seconds are unsupported"));
    }
    Ok(Until::Instant(parsed.and_utc()))
}

impl Rule {
    fn parse(input: &str) -> Result<Self> {
        if input.len() > 4096 || !input.is_ascii() || input.bytes().any(|b| b.is_ascii_control()) {
            return Err(invalid("RRULE must be at most 4096 printable ASCII bytes"));
        }
        let uppercase = input.trim().to_ascii_uppercase();
        let input = uppercase.strip_prefix("RRULE:").unwrap_or(&uppercase);
        let mut frequency = None;
        let mut interval = 1;
        let mut by_day = None;
        let mut week_start = 0;
        let mut count = None;
        let mut until = None;
        let mut keys = std::collections::HashSet::new();
        for part in input.split(';') {
            let (key, value) = part
                .split_once('=')
                .ok_or_else(|| invalid("malformed RRULE part"))?;
            let (key, value) = (key.trim(), value.trim());
            if value.is_empty() || !keys.insert(key) {
                return Err(invalid("empty or duplicate RRULE part"));
            }
            match key {
                "FREQ" => {
                    frequency = Some(match value {
                        "DAILY" => Frequency::Daily,
                        "WEEKLY" => Frequency::Weekly,
                        "MONTHLY" => Frequency::Monthly,
                        "YEARLY" => Frequency::Yearly,
                        _ => return Err(invalid("unsupported FREQ")),
                    });
                }
                "INTERVAL" => {
                    interval = positive_integer(value)?;
                    if interval > 99 {
                        return Err(invalid("INTERVAL must be in 1..=99"));
                    }
                }
                "COUNT" => count = Some(positive_integer(value)?),
                "UNTIL" => until = Some(parse_until(value)?),
                "WKST" => week_start = weekday(value)?,
                "BYDAY" => {
                    let mut days = value
                        .split(',')
                        .map(|day| weekday(day.trim()))
                        .collect::<Result<Vec<_>>>()?;
                    days.sort_unstable();
                    days.dedup();
                    by_day = Some(days);
                }
                _ => return Err(invalid(&format!("unsupported RRULE part {key}"))),
            }
        }
        let frequency = frequency.ok_or_else(|| invalid("RRULE requires FREQ"))?;
        if frequency != Frequency::Weekly && (by_day.is_some() || keys.contains("WKST")) {
            return Err(invalid("BYDAY and WKST are supported only for WEEKLY"));
        }
        if count.is_some() && until.is_some() {
            return Err(invalid("COUNT and UNTIL are mutually exclusive"));
        }
        Ok(Self {
            frequency,
            interval,
            by_day,
            week_start,
            count,
            until,
        })
    }

    fn serialize(&self) -> String {
        let frequency = match self.frequency {
            Frequency::Daily => "DAILY",
            Frequency::Weekly => "WEEKLY",
            Frequency::Monthly => "MONTHLY",
            Frequency::Yearly => "YEARLY",
        };
        let mut parts = vec![format!("FREQ={frequency}")];
        if self.interval != 1 {
            parts.push(format!("INTERVAL={}", self.interval));
        }
        if let Some(days) = &self.by_day {
            parts.push(format!(
                "BYDAY={}",
                days.iter()
                    .map(|day| DAY_CODES[*day as usize])
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        if self.week_start != 0 {
            parts.push(format!("WKST={}", DAY_CODES[self.week_start as usize]));
        }
        if let Some(count) = self.count {
            parts.push(format!("COUNT={count}"));
        }
        if let Some(until) = self.until {
            parts.push(format!(
                "UNTIL={}",
                match until {
                    Until::Date(date) => date.format("%Y%m%d").to_string(),
                    Until::Instant(time) => time.format("%Y%m%dT%H%M%SZ").to_string(),
                }
            ));
        }
        parts.join(";")
    }
}

struct Schedule<'a> {
    event: &'a CalendarEvent,
    rule: Rule,
    start: DateTime<Utc>,
    local_start: NaiveDateTime,
    duration: Duration,
    zone: Tz,
    week_anchor: NaiveDate,
    week_days: Vec<u32>,
    first_week_days: Vec<u32>,
}

/// A slot can be an invalid calendar date (February 31). Its period floor still
/// lets a bounded query stop without searching for a later valid occurrence.
struct Candidate {
    floor: NaiveDate,
    date: Option<NaiveDate>,
}

impl<'a> Schedule<'a> {
    fn from_event(event: &'a CalendarEvent) -> Result<Self> {
        Self::new(
            event,
            event
                .recurrence_rule
                .as_deref()
                .ok_or_else(|| invalid("event has no recurrence rule"))?,
        )
    }

    fn new(event: &'a CalendarEvent, rule: &str) -> Result<Self> {
        let rule = Rule::parse(rule)?;
        let zone = match event.timezone.as_deref() {
            None => chrono_tz::UTC,
            Some(name) => name
                .parse::<Tz>()
                .map_err(|_| invalid("unknown IANA timezone"))?,
        };
        let template = OccurrenceFields {
            title: event.title.clone(),
            description: event.description.clone(),
            location: event.location.clone(),
            start_time: event.start_time.clone(),
            end_time: event.end_time.clone(),
            all_day: event.all_day,
            timezone: event.timezone.clone(),
        };
        template.validate()?;
        let (start, end, local_start) = if event.all_day {
            let start = midnight(date(&event.start_time)?)?;
            let end = midnight(date(&event.end_time)?)?;
            (start, end, start.naive_utc())
        } else {
            let start = instant(&event.start_time)?;
            let end = instant(&event.end_time)?;
            (start, end, start.with_timezone(&zone).naive_local())
        };
        check_date(local_start.date())?;
        let duration = end.signed_duration_since(start);
        if duration <= Duration::zero() {
            return Err(invalid("event end must be after its start"));
        }
        let start_weekday = local_start.weekday().num_days_from_monday();
        if rule
            .by_day
            .as_ref()
            .is_some_and(|days| !days.contains(&start_weekday))
        {
            return Err(invalid("DTSTART weekday must be included in BYDAY"));
        }
        match rule.until {
            Some(Until::Date(until)) if until < local_start.date() => {
                return Err(invalid("UNTIL is before DTSTART"));
            }
            Some(Until::Instant(_)) if event.all_day => {
                return Err(invalid("all-day rules require a date-valued UNTIL"));
            }
            Some(Until::Instant(until)) if until < start => {
                return Err(invalid("UNTIL is before DTSTART"));
            }
            _ => {}
        }
        let start_offset = (start_weekday + 7 - rule.week_start) % 7;
        let week_anchor = local_start
            .date()
            .checked_sub_signed(Duration::days(i64::from(start_offset)))
            .ok_or_else(range_error)?;
        let mut week_days: Vec<_> = rule
            .by_day
            .clone()
            .unwrap_or_else(|| vec![start_weekday])
            .into_iter()
            .map(|day| (day + 7 - rule.week_start) % 7)
            .collect();
        week_days.sort_unstable();
        let first_week_days = week_days
            .iter()
            .copied()
            .filter(|day| *day >= start_offset)
            .collect();
        Ok(Self {
            event,
            rule,
            start,
            local_start,
            duration,
            zone,
            week_anchor,
            week_days,
            first_week_days,
        })
    }

    /// Only these schedules have one real occurrence per arithmetic slot.
    fn arithmetic_count(&self) -> bool {
        (self.event.all_day || self.zone == chrono_tz::UTC)
            && matches!(self.rule.frequency, Frequency::Daily | Frequency::Weekly)
    }

    fn candidate(&self, slot: u64) -> Result<Candidate> {
        let slot = i64::try_from(slot).map_err(|_| range_error())?;
        let interval = i64::from(self.rule.interval);
        let start = self.local_start.date();
        let add_days = |anchor: NaiveDate, days: i64| -> Result<NaiveDate> {
            let delta = Duration::try_days(days).ok_or_else(range_error)?;
            anchor.checked_add_signed(delta).ok_or_else(range_error)
        };
        let date = match self.rule.frequency {
            Frequency::Daily => {
                add_days(start, slot.checked_mul(interval).ok_or_else(range_error)?)?
            }
            Frequency::Weekly => {
                let first = self.first_week_days.len() as i64;
                let (week, day) = if slot < first {
                    (0, self.first_week_days[slot as usize])
                } else {
                    let rest = slot - first;
                    (
                        rest / self.week_days.len() as i64 + 1,
                        self.week_days[rest as usize % self.week_days.len()],
                    )
                };
                let days = week
                    .checked_mul(interval * 7)
                    .and_then(|days| days.checked_add(i64::from(day)))
                    .ok_or_else(range_error)?;
                add_days(self.week_anchor, days)?
            }
            Frequency::Monthly | Frequency::Yearly => {
                let step = slot.checked_mul(interval).ok_or_else(range_error)?;
                let (year, month) = if self.rule.frequency == Frequency::Monthly {
                    let month = (i64::from(start.year()) * 12 + i64::from(start.month0()))
                        .checked_add(step)
                        .ok_or_else(range_error)?;
                    (month / 12, (month % 12 + 1) as u32)
                } else {
                    (
                        i64::from(start.year())
                            .checked_add(step)
                            .ok_or_else(range_error)?,
                        start.month(),
                    )
                };
                // A period just outside our public range is still useful to
                // establish that a bounded window or UNTIL has ended.
                let year = i32::try_from(year).map_err(|_| range_error())?;
                return Ok(Candidate {
                    floor: NaiveDate::from_ymd_opt(year, month, 1).ok_or_else(range_error)?,
                    date: NaiveDate::from_ymd_opt(year, month, start.day()),
                });
            }
        };
        Ok(Candidate {
            floor: date,
            date: Some(date),
        })
    }

    fn at_date(&self, date: NaiveDate) -> Result<Option<DateTime<Utc>>> {
        check_date(date)?;
        if date == self.local_start.date() {
            return Ok(Some(self.start));
        }
        if self.event.all_day {
            return Ok(Some(midnight(date)?));
        }
        let local = date.and_time(self.local_start.time());
        let time = match self.zone.from_local_datetime(&local) {
            LocalResult::Single(time) => time.with_timezone(&Utc),
            LocalResult::Ambiguous(a, b) => a.min(b).with_timezone(&Utc),
            LocalResult::None => return Ok(None),
        };
        check_date(time.date_naive())?;
        Ok(Some(time))
    }

    fn within_until(&self, time: DateTime<Utc>, date: NaiveDate) -> bool {
        match self.rule.until {
            None => true,
            Some(Until::Date(until)) => date <= until,
            Some(Until::Instant(until)) => time <= until,
        }
    }

    fn past_until_date(&self, floor: NaiveDate) -> bool {
        match self.rule.until {
            None => false,
            Some(Until::Date(until)) => floor > until,
            // Every legal timezone offset is less than a day. The extra day
            // avoids assumptions about historical date-line transitions.
            Some(Until::Instant(until)) => {
                floor.signed_duration_since(until.date_naive()).num_days() > 2
            }
        }
    }

    /// A slot at or before the first possible position on `date`.
    fn seek(&self, date: NaiveDate) -> u64 {
        let start = self.local_start.date();
        if date <= start {
            return 0;
        }
        let interval = i64::from(self.rule.interval);
        match self.rule.frequency {
            Frequency::Daily => (date.signed_duration_since(start).num_days() / interval) as u64,
            Frequency::Weekly => {
                let cycle =
                    date.signed_duration_since(self.week_anchor).num_days() / (7 * interval);
                if cycle == 0 {
                    0
                } else {
                    self.first_week_days.len() as u64
                        + (cycle as u64 - 1) * self.week_days.len() as u64
                }
            }
            Frequency::Monthly => {
                let months = i64::from(date.year() - start.year()) * 12 + i64::from(date.month())
                    - i64::from(start.month());
                (months / interval) as u64
            }
            Frequency::Yearly => (i64::from(date.year() - start.year()) / interval) as u64,
        }
    }

    fn target_slot(&self, original_start: &str) -> Result<(u64, DateTime<Utc>)> {
        let target = if self.event.all_day {
            midnight(date(original_start)?)?
        } else {
            instant(original_start)?
        };
        if target < self.start {
            return Err(invalid("original_start is not a series position"));
        }
        let local_date = if self.event.all_day {
            target.date_naive()
        } else {
            target.with_timezone(&self.zone).date_naive()
        };
        // WEEKLY seek returns the start of a cycle; at most seven slots can
        // belong to the target week. Other frequencies need exactly one.
        let attempts = if self.rule.frequency == Frequency::Weekly {
            self.week_days.len()
        } else {
            1
        };
        for slot in (self.seek(local_date)..).take(attempts) {
            let candidate = self.candidate(slot)?;
            if candidate.date == Some(local_date) {
                if self.at_date(local_date)? == Some(target)
                    && self.within_until(target, local_date)
                {
                    return Ok((slot, target));
                }
                break;
            }
            if candidate.floor > local_date {
                break;
            }
        }
        Err(invalid("original_start is not a series position"))
    }

    fn fields(&self, start: DateTime<Utc>) -> Result<OccurrenceFields> {
        let end = start
            .checked_add_signed(self.duration)
            .ok_or_else(range_error)?;
        check_date(end.date_naive())?;
        let format = |time: DateTime<Utc>| {
            if self.event.all_day {
                time.date_naive().to_string()
            } else {
                utc_string(time)
            }
        };
        Ok(OccurrenceFields {
            title: self.event.title.clone(),
            description: self.event.description.clone(),
            location: self.event.location.clone(),
            start_time: format(start),
            end_time: format(end),
            all_day: self.event.all_day,
            timezone: self.event.timezone.clone(),
        })
    }
}

#[derive(Default)]
struct Budget(usize);

impl Budget {
    fn spend(&mut self) -> Result<()> {
        if self.0 >= CANDIDATE_BUDGET {
            return Err(invalid("candidate work budget exceeded (200000 positions)"));
        }
        self.0 += 1;
        Ok(())
    }
}

#[derive(Default)]
struct Cursor {
    slot: u64,
    ordinal: u64,
    done: bool,
}

impl Cursor {
    fn at_slot(
        schedule: &Schedule<'_>,
        slot: u64,
        need_ordinal: bool,
        budget: &mut Budget,
    ) -> Result<Self> {
        if schedule.arithmetic_count() || (!need_ordinal && schedule.rule.count.is_none()) {
            return Ok(Self {
                slot,
                ordinal: if schedule.arithmetic_count() { slot } else { 0 },
                done: false,
            });
        }
        let mut cursor = Self::default();
        while cursor.slot < slot && !cursor.done {
            cursor.step(schedule, budget)?;
        }
        Ok(cursor)
    }

    /// Inspect precisely one calendar slot; invalid dates and gaps have no index.
    fn step(
        &mut self,
        schedule: &Schedule<'_>,
        budget: &mut Budget,
    ) -> Result<Option<(u64, DateTime<Utc>)>> {
        if self.done
            || schedule
                .rule
                .count
                .is_some_and(|count| self.ordinal >= u64::from(count))
        {
            self.done = true;
            return Ok(None);
        }
        budget.spend()?;
        let candidate = schedule.candidate(self.slot)?;
        self.slot += 1;
        if schedule.past_until_date(candidate.floor) {
            self.done = true;
            return Ok(None);
        }
        let Some(date) = candidate.date else {
            return Ok(None);
        };
        let Some(time) = schedule.at_date(date)? else {
            return Ok(None);
        };
        if !schedule.within_until(time, date) {
            self.done = true;
            return Ok(None);
        }
        let index = self.ordinal;
        self.ordinal += 1;
        Ok(Some((index, time)))
    }
}

/// Validate and serialize the editor's supported rule subset in stable order.
/// Accepts one optional RRULE prefix and case/space normalization. Explicit
/// INTERVAL=1 and WKST=MO are omitted; BYDAY is a sorted, deduplicated set.
/// Unknown parts, mismatching DTSTART, and bounds before DTSTART are errors.
pub fn normalize_rule(rule: &str, event: &CalendarEvent) -> Result<String> {
    Ok(Schedule::new(event, rule)?.rule.serialize())
}

/// Expand positions overlapping `[window_start, window_end)`, before exclusions.
/// Returns at most `limit` positions (0..=10000); `has_more` requires finding an
/// additional match in this window, not just a later position in the series.
/// A date-only boundary means UTC midnight. An empty/reversed window is invalid.
/// Uncounted rules seek directly; exact historical DST counting is budgeted.
pub fn expand(
    event: &CalendarEvent,
    window_start: &str,
    window_end: &str,
    limit: usize,
) -> Result<Expansion> {
    if limit > MAX_RESULTS {
        return Err(invalid("expansion limit exceeds 10000"));
    }
    let schedule = Schedule::from_event(event)?;
    let (lower, upper) = (boundary(window_start)?, boundary(window_end)?);
    if lower >= upper {
        return Err(invalid("window end must be after window start"));
    }
    let mut result = Expansion {
        occurrences: Vec::new(),
        has_more: false,
    };
    if upper <= schedule.start {
        return Ok(result);
    }
    // Seek conservatively in UTC dates: no timezone conversion or DST
    // assumption may discard a long event overlapping the lower boundary.
    let earliest = lower
        .checked_sub_signed(schedule.duration)
        .and_then(|time| time.checked_sub_signed(Duration::days(2)))
        .ok_or_else(range_error)?;
    let mut budget = Budget::default();
    let mut cursor = Cursor::at_slot(
        &schedule,
        schedule.seek(earliest.date_naive()),
        false,
        &mut budget,
    )?;
    while !cursor.done {
        if schedule
            .rule
            .count
            .is_some_and(|count| cursor.ordinal >= u64::from(count))
        {
            break;
        }
        let candidate = schedule.candidate(cursor.slot)?;
        if candidate
            .floor
            .signed_duration_since(upper.date_naive())
            .num_days()
            > 2
            || schedule.past_until_date(candidate.floor)
        {
            break;
        }
        let Some((_, start)) = cursor.step(&schedule, &mut budget)? else {
            continue;
        };
        if start >= upper {
            break;
        }
        let end = start
            .checked_add_signed(schedule.duration)
            .ok_or_else(range_error)?;
        if end > lower {
            if result.occurrences.len() == limit {
                result.has_more = true;
                break;
            }
            let fields = schedule.fields(start)?;
            result.occurrences.push(GeneratedOccurrence {
                original_start: fields.start_time.clone(),
                fields,
            });
        }
    }
    Ok(result)
}

/// Resolve an original series position, never an override's effective start.
/// Timed identities may use any equivalent RFC 3339 offset; all-day identities
/// must be ISO dates. Membership includes COUNT/UNTIL and the selected DST fold.
pub fn resolve(event: &CalendarEvent, original_start: &str) -> Result<OccurrenceFields> {
    let schedule = Schedule::from_event(event)?;
    let (slot, start) = schedule.target_slot(original_start)?;
    if schedule.rule.count.is_some() {
        let mut budget = Budget::default();
        let mut cursor = Cursor::at_slot(&schedule, slot, true, &mut budget)?;
        if cursor.step(&schedule, &mut budget)?.is_none() {
            return Err(invalid("original_start is outside the series count"));
        }
    }
    schedule.fields(start)
}

/// Zero-based index among actual generated positions, before exclusions.
/// Daily/weekly UTC and all-day schedules count arithmetically. Other schedules
/// inspect at most 200000 candidate positions, then return an explicit error.
pub fn occurrence_index(event: &CalendarEvent, original_start: &str) -> Result<u32> {
    let schedule = Schedule::from_event(event)?;
    let (slot, _) = schedule.target_slot(original_start)?;
    let mut budget = Budget::default();
    let mut cursor = Cursor::at_slot(&schedule, slot, true, &mut budget)?;
    let (index, _) = cursor
        .step(&schedule, &mut budget)?
        .ok_or_else(|| invalid("original_start is outside the series bounds"))?;
    u32::try_from(index).map_err(|_| invalid("occurrence index exceeds u32"))
}

/// Inverse of `occurrence_index`, returning a canonical original start.
/// Invalid monthly/yearly dates and nonexistent local times have no index.
/// Out-of-series, date-range, and work-budget failures are errors.
pub fn position_at(event: &CalendarEvent, index: u32) -> Result<String> {
    let schedule = Schedule::from_event(event)?;
    if schedule.rule.count.is_some_and(|count| index >= count) {
        return Err(invalid("occurrence index is outside the series count"));
    }
    let mut budget = Budget::default();
    let mut cursor = if schedule.arithmetic_count() {
        Cursor::at_slot(&schedule, u64::from(index), true, &mut budget)?
    } else {
        Cursor::default()
    };
    while !cursor.done {
        if let Some((ordinal, time)) = cursor.step(&schedule, &mut budget)? {
            if ordinal == u64::from(index) {
                return Ok(if event.all_day {
                    time.date_naive().to_string()
                } else {
                    utc_string(time)
                });
            }
        }
    }
    Err(invalid("occurrence index is outside the series bounds"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::RecurrenceKind;

    fn event(start: &str, end: &str, zone: Option<&str>, rule: &str) -> CalendarEvent {
        CalendarEvent {
            id: "simple-recurrence-test".into(),
            account_id: "account".into(),
            calendar_id: "calendar".into(),
            uid: None,
            title: "Planning".into(),
            description: Some("Agenda".into()),
            location: Some("Room".into()),
            start_time: start.into(),
            end_time: end.into(),
            all_day: start.len() == 10,
            timezone: zone.map(str::to_owned),
            recurrence_rule: Some(rule.into()),
            recurrence_kind: RecurrenceKind::Series,
            organizer_email: None,
            attendees_json: None,
            my_status: None,
            source_message_id: None,
            ical_data: None,
            remote_id: None,
            etag: None,
        }
    }

    fn daily(rule: &str) -> CalendarEvent {
        event("2026-09-14", "2026-09-15", None, rule)
    }

    fn starts(event: &CalendarEvent, from: &str, to: &str) -> Vec<String> {
        let result = expand(event, from, to, 100).unwrap();
        assert!(!result.has_more);
        result
            .occurrences
            .into_iter()
            .map(|occurrence| occurrence.original_start)
            .collect()
    }

    fn assert_positions(event: &CalendarEvent, expected: &[&str]) {
        for (index, expected) in expected.iter().enumerate() {
            assert_eq!(position_at(event, index as u32).unwrap(), *expected);
            assert_eq!(occurrence_index(event, expected).unwrap(), index as u32);
            assert_eq!(resolve(event, expected).unwrap().start_time, *expected);
        }
        assert!(position_at(event, expected.len() as u32).is_err());
    }

    #[test]
    fn normalization_is_stable_and_preserves_week_phase() {
        let event = daily("unused");
        let normalized = normalize_rule(
            " rrule:count=004;byday= we,mo,we;freq=weekly;wkst=su;interval=02 ",
            &event,
        )
        .unwrap();
        assert_eq!(
            normalized,
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;WKST=SU;COUNT=4"
        );
        assert_eq!(normalize_rule(&normalized, &event).unwrap(), normalized);
        assert_eq!(
            normalize_rule("FREQ=WEEKLY;INTERVAL=1;WKST=MO", &event).unwrap(),
            "FREQ=WEEKLY"
        );
        assert_eq!(
            normalize_rule("FREQ=DAILY;UNTIL=2026-09-14", &event).unwrap(),
            "FREQ=DAILY;UNTIL=20260914"
        );
    }

    #[test]
    fn rejects_unsupported_malformed_or_ambiguous_rules() {
        for rule in [
            "",
            "RRULE:",
            "RRULE:RRULE:FREQ=DAILY",
            "FREQ=DAILY;",
            "FREQ=DAILY;COUNT",
            "FREQ=DAILY;COUNT=",
            "FREQ=DAILY;COUNT=+2",
            "FREQ=DAILY;COUNT=-2",
            "FREQ=DAILY;COUNT=0",
            "FREQ=DAILY;COUNT=4294967296",
            "FREQ=DAILY;COUNT=2;count=3",
            "FREQ=DAILY;FREQ=DAILY",
            "FREQ=DAILY;INTERVAL=0",
            "FREQ=DAILY;INTERVAL=100",
            "FREQ=DAILY;INTERVAL=1.5",
            "FREQ=DAILY;COUNT=1;UNTIL=20261231",
            "FREQ=DAILY;UNTIL=20260230",
            "FREQ=DAILY;UNTIL=20260914T120000",
            "FREQ=DAILY;UNTIL=20260914T120000+0100",
            "FREQ=DAILY;UNTIL=20260914T250000Z",
            "FREQ=DAILY;UNTIL=20260914T120060Z",
            "FREQ=DAILY;UNTIL=2026-09-14T12:00:00Z",
            "FREQ=DAILY;UNTIL=20260913",
            "FREQ=DAILY;BYDAY=MO",
            "FREQ=MONTHLY;BYDAY=MO",
            "FREQ=YEARLY;BYMONTH=9",
            "FREQ=MONTHLY;BYMONTHDAY=14",
            "FREQ=WEEKLY;BYDAY=1MO",
            "FREQ=WEEKLY;BYDAY=MO,",
            "FREQ=WEEKLY;BYDAY=MO,XX",
            "FREQ=WEEKLY;BYDAY=TU",
            "FREQ=WEEKLY;WKST=XX",
            "FREQ=DAILY;WKST=MO",
            "FREQ=DAILY;X-EXTENSION=1",
            "FREQ=HOURLY",
            "FREQ=DAILY\r\nEXDATE:20260915",
            "FREQ=DAILY\t",
            "FREQ=DAILY\0",
            "FREQ=DÄILY",
        ] {
            assert!(normalize_rule(rule, &daily(rule)).is_err(), "{rule:?}");
        }
        assert!(normalize_rule(&"a".repeat(4097), &daily("FREQ=DAILY")).is_err());
    }

    #[test]
    fn invalid_event_data_does_not_gain_validity_from_a_rule() {
        let valid = daily("FREQ=DAILY");
        for zone in ["", "Not/A_Zone", " UTC", "UTC\n", "W. Europe Standard Time"] {
            let mut value = valid.clone();
            value.timezone = Some(zone.into());
            assert!(normalize_rule("FREQ=DAILY", &value).is_err(), "{zone:?}");
        }
        let mut value = valid.clone();
        value.end_time = value.start_time.clone();
        assert!(normalize_rule("FREQ=DAILY", &value).is_err());
        value = valid.clone();
        value.title = " ".into();
        assert!(normalize_rule("FREQ=DAILY", &value).is_err());
        value = valid.clone();
        value.recurrence_rule = None;
        assert!(expand(&value, "2026-09-14", "2026-09-15", 1).is_err());
        for bad in ["2026-9-14", "0000-09-14", "2026-02-30"] {
            value = valid.clone();
            value.start_time = bad.into();
            assert!(normalize_rule("FREQ=DAILY", &value).is_err());
        }
        for bad in ["2026-09-14T10:00:00", "2026-09-14T10:00:60Z"] {
            value = event(bad, "2026-09-14T12:00:00Z", None, "FREQ=DAILY");
            assert!(normalize_rule("FREQ=DAILY", &value).is_err());
        }
    }

    #[test]
    fn daily_interval_count_and_original_membership() {
        let event = daily("FREQ=DAILY;INTERVAL=2;COUNT=3");
        assert_positions(&event, &["2026-09-14", "2026-09-16", "2026-09-18"]);
        for nonmember in ["2026-09-12", "2026-09-15", "2026-09-20"] {
            assert!(resolve(&event, nonmember).is_err());
            assert!(occurrence_index(&event, nonmember).is_err());
        }
        assert!(resolve(&event, "2026-09-14T00:00:00Z").is_err());
    }

    #[test]
    fn weekly_multi_day_count_excludes_earlier_days_in_first_week() {
        let event = event(
            "2026-09-16",
            "2026-09-17",
            None,
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=FR,MO,WE;COUNT=5",
        );
        let expected = [
            "2026-09-16",
            "2026-09-18",
            "2026-09-28",
            "2026-09-30",
            "2026-10-02",
        ];
        assert_positions(&event, &expected);
        assert_eq!(starts(&event, "2026-09-01", "2026-11-01"), expected);
        assert!(resolve(&event, "2026-09-14").is_err());
        assert!(resolve(&event, "2026-09-21").is_err());
    }

    #[test]
    fn weekly_implicit_day_and_non_monday_week_start() {
        let event = event(
            "2026-09-20",
            "2026-09-21",
            None,
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=SU,MO;WKST=SU;COUNT=4",
        );
        assert_positions(
            &event,
            &["2026-09-20", "2026-09-21", "2026-10-04", "2026-10-05"],
        );
        let mut monday = event.clone();
        monday.recurrence_rule = Some("FREQ=WEEKLY;INTERVAL=2;BYDAY=SU,MO;COUNT=4".into());
        assert_positions(
            &monday,
            &["2026-09-20", "2026-09-28", "2026-10-04", "2026-10-12"],
        );
        let implicit = daily("FREQ=WEEKLY;INTERVAL=3;COUNT=3");
        assert_positions(&implicit, &["2026-09-14", "2026-10-05", "2026-10-26"]);
    }

    #[test]
    fn month_end_skips_invalid_dates_without_consuming_count() {
        let event = event("2026-01-31", "2026-02-02", None, "FREQ=MONTHLY;COUNT=4");
        assert_positions(
            &event,
            &["2026-01-31", "2026-03-31", "2026-05-31", "2026-07-31"],
        );
        assert_eq!(
            resolve(&event, "2026-03-31").unwrap().end_time,
            "2026-04-02"
        );
        assert!(resolve(&event, "2026-03-03").is_err());
        assert_eq!(
            starts(&event, "2026-02-01", "2026-04-01"),
            ["2026-01-31", "2026-03-31"]
        );
    }

    #[test]
    fn leap_day_uses_gregorian_century_rules() {
        let event = event("2096-02-29", "2096-03-01", None, "FREQ=YEARLY;COUNT=3");
        assert_positions(&event, &["2096-02-29", "2104-02-29", "2108-02-29"]);
        assert!(starts(&event, "2100-01-01", "2101-01-01").is_empty());
        assert!(resolve(&event, "2100-03-01").is_err());
        let every_other_month = event_for_month_interval();
        assert_positions(
            &every_other_month,
            &["2026-12-31", "2027-08-31", "2027-12-31"],
        );
    }

    fn event_for_month_interval() -> CalendarEvent {
        event(
            "2026-12-31",
            "2027-01-01",
            None,
            "FREQ=MONTHLY;INTERVAL=4;COUNT=3",
        )
    }

    #[test]
    fn spring_gap_does_not_consume_daily_or_weekly_count() {
        for rule in ["FREQ=DAILY;COUNT=3", "FREQ=WEEKLY;BYDAY=SA,SU,MO;COUNT=3"] {
            let event = event(
                "2026-03-07T07:30:00Z",
                "2026-03-07T08:30:00Z",
                Some("America/New_York"),
                rule,
            );
            let expected = if rule.starts_with("FREQ=DAILY") {
                [
                    "2026-03-07T07:30:00Z",
                    "2026-03-09T06:30:00Z",
                    "2026-03-10T06:30:00Z",
                ]
            } else {
                [
                    "2026-03-07T07:30:00Z",
                    "2026-03-09T06:30:00Z",
                    "2026-03-14T06:30:00Z",
                ]
            };
            assert_positions(&event, &expected);
            assert_eq!(starts(&event, "2026-03-01", "2026-04-01"), expected);
            assert!(resolve(&event, "2026-03-08T07:30:00Z").is_err());
            assert!(resolve(&event, "2026-03-08T06:30:00Z").is_err());
        }
    }

    #[test]
    fn fall_overlap_uses_first_instant_and_preserves_elapsed_duration() {
        let event = event(
            "2026-10-31T05:30:00Z",
            "2026-10-31T07:30:00Z",
            Some("America/New_York"),
            "FREQ=DAILY;COUNT=3",
        );
        assert_positions(
            &event,
            &[
                "2026-10-31T05:30:00Z",
                "2026-11-01T05:30:00Z",
                "2026-11-02T06:30:00Z",
            ],
        );
        let fields = resolve(&event, "2026-11-01T01:30:00-04:00").unwrap();
        assert_eq!(fields.end_time, "2026-11-01T07:30:00Z");
        assert_eq!(fields.timezone.as_deref(), Some("America/New_York"));
        assert_eq!(fields.title, event.title);
        assert_eq!(fields.description, event.description);
        assert_eq!(fields.location, event.location);
        assert!(resolve(&event, "2026-11-01T06:30:00Z").is_err());
    }

    #[test]
    fn dtstart_in_second_overlap_retains_explicit_instant() {
        let event = event(
            "2026-11-01T01:30:00-05:00",
            "2026-11-01T02:30:00-05:00",
            Some("America/New_York"),
            "FREQ=DAILY;COUNT=2",
        );
        assert_positions(&event, &["2026-11-01T06:30:00Z", "2026-11-02T06:30:00Z"]);
        assert!(resolve(&event, "2026-11-01T05:30:00Z").is_err());
    }

    #[test]
    fn half_hour_gap_and_skipped_calendar_day_are_not_hour_assumptions() {
        let lord_howe = event(
            "2026-10-02T15:45:00Z",
            "2026-10-02T16:45:00Z",
            Some("Australia/Lord_Howe"),
            "FREQ=DAILY;COUNT=2",
        );
        assert_positions(
            &lord_howe,
            &["2026-10-02T15:45:00Z", "2026-10-04T15:15:00Z"],
        );
        let apia = event(
            "2011-12-29T19:00:00Z",
            "2011-12-29T20:00:00Z",
            Some("Pacific/Apia"),
            "FREQ=DAILY;COUNT=3",
        );
        assert_positions(
            &apia,
            &[
                "2011-12-29T19:00:00Z",
                "2011-12-30T19:00:00Z",
                "2011-12-31T19:00:00Z",
            ],
        );
    }

    #[test]
    fn all_day_durations_and_dates_are_unaffected_by_dst() {
        let event = event(
            "2026-03-07",
            "2026-03-10",
            Some("America/New_York"),
            "FREQ=DAILY;COUNT=3",
        );
        assert_positions(&event, &["2026-03-07", "2026-03-08", "2026-03-09"]);
        assert_eq!(
            resolve(&event, "2026-03-08").unwrap().end_time,
            "2026-03-11"
        );
        assert_eq!(
            starts(&event, "2026-03-10", "2026-03-11"),
            ["2026-03-08", "2026-03-09"]
        );
    }

    #[test]
    fn until_date_is_inclusive_in_event_local_date() {
        let event = event(
            "2026-09-15T06:30:00Z",
            "2026-09-15T07:30:00Z",
            Some("America/Los_Angeles"),
            "FREQ=DAILY;UNTIL=20260915",
        );
        assert_positions(&event, &["2026-09-15T06:30:00Z", "2026-09-16T06:30:00Z"]);
        assert_positions(&daily("FREQ=DAILY;UNTIL=20260914"), &["2026-09-14"]);
        assert!(normalize_rule("FREQ=DAILY;UNTIL=20260913", &event).is_err());
    }

    #[test]
    fn until_utc_is_inclusive_and_compares_instants() {
        let mut event = event(
            "2026-03-07T14:00:00Z",
            "2026-03-07T15:00:00Z",
            Some("America/New_York"),
            "FREQ=DAILY;UNTIL=20260308T130000Z",
        );
        assert_positions(&event, &["2026-03-07T14:00:00Z", "2026-03-08T13:00:00Z"]);
        event.recurrence_rule = Some("FREQ=DAILY;UNTIL=20260308T125959Z".into());
        assert_positions(&event, &["2026-03-07T14:00:00Z"]);
        assert!(normalize_rule("FREQ=DAILY;UNTIL=20260307T135959Z", &event).is_err());
        assert!(normalize_rule("FREQ=DAILY;UNTIL=20260914T000000Z", &daily("")).is_err());
    }

    #[test]
    fn until_ending_on_a_nonexistent_position_terminates() {
        let event = event(
            "2026-03-07T07:30:00Z",
            "2026-03-07T08:30:00Z",
            Some("America/New_York"),
            "FREQ=DAILY;UNTIL=20260308",
        );
        assert_positions(&event, &["2026-03-07T07:30:00Z"]);
        assert_eq!(
            starts(&event, "2026-03-08", "2026-03-09"),
            Vec::<String>::new()
        );
        let mut monthly = event_for_month_interval();
        monthly.recurrence_rule = Some("FREQ=MONTHLY;UNTIL=20270228".into());
        assert_positions(&monthly, &["2026-12-31", "2027-01-31"]);
    }

    #[test]
    fn half_open_overlap_and_long_duration_seek() {
        let event = event(
            "2026-09-14T10:00:00Z",
            "2026-09-16T10:00:00Z",
            None,
            "FREQ=DAILY;COUNT=3",
        );
        assert_eq!(
            starts(&event, "2026-09-16T10:00:00Z", "2026-09-17T10:00:00Z"),
            ["2026-09-15T10:00:00Z", "2026-09-16T10:00:00Z"]
        );
        assert!(starts(&event, "2026-09-01", "2026-09-14T10:00:00Z").is_empty());
        let long = event_for_long_duration();
        assert_eq!(starts(&long, "2026-12-01", "2026-12-02"), ["2026-01-01"]);
    }

    fn event_for_long_duration() -> CalendarEvent {
        event("2026-01-01", "2027-01-01", None, "FREQ=YEARLY")
    }

    #[test]
    fn window_formats_share_explicit_utc_axis() {
        let event = event(
            "2026-09-14T23:30:00-07:00",
            "2026-09-15T00:30:00-07:00",
            Some("America/Los_Angeles"),
            "FREQ=DAILY;COUNT=2",
        );
        assert_eq!(
            expand(&event, "2026-09-15", "2026-09-16", 10).unwrap(),
            expand(
                &event,
                "2026-09-14T17:00:00-07:00",
                "2026-09-16T00:00:00Z",
                10,
            )
            .unwrap()
        );
        let all_day = daily("FREQ=DAILY;COUNT=2");
        assert_eq!(
            starts(&all_day, "2026-09-14T12:00:00Z", "2026-09-15T00:00:00Z"),
            ["2026-09-14"]
        );
    }

    #[test]
    fn has_more_means_an_additional_overlap_in_the_window() {
        let event = daily("FREQ=DAILY");
        let result = expand(&event, "2026-09-14", "2026-09-17", 2).unwrap();
        assert_eq!(result.occurrences.len(), 2);
        assert!(result.has_more);
        let result = expand(&event, "2026-09-14", "2026-09-16", 2).unwrap();
        assert_eq!(result.occurrences.len(), 2);
        assert!(!result.has_more);
        let result = expand(&event, "2026-09-14", "2026-09-15", 0).unwrap();
        assert!(result.occurrences.is_empty());
        assert!(result.has_more);
        let result = expand(&event, "2026-09-01", "2026-09-14", 0).unwrap();
        assert!(!result.has_more);
        let finite = daily("FREQ=DAILY;COUNT=2");
        assert!(
            !expand(&finite, "2026-09-14", "2027-01-01", 2)
                .unwrap()
                .has_more
        );
    }

    #[test]
    fn invalid_windows_and_limits_are_errors() {
        let event = daily("FREQ=DAILY");
        for (start, end) in [
            ("2026-09-14", "2026-09-14"),
            ("2026-09-15", "2026-09-14"),
            ("2026-02-30", "2026-09-14"),
            ("2026-09-14", "2026-09-15T00:00:00"),
        ] {
            assert!(expand(&event, start, end, 10).is_err());
        }
        assert!(expand(&event, "2026-09-14", "2026-09-15", MAX_RESULTS + 1).is_err());
    }

    #[test]
    fn fractional_instants_and_offsets_round_trip_without_precision_loss() {
        let event = event(
            "2026-09-14T10:00:00.123456789+02:00",
            "2026-09-14T11:00:00.987654321+02:00",
            Some("Europe/Stockholm"),
            "FREQ=DAILY;COUNT=2",
        );
        let fields = resolve(&event, "2026-09-15T10:00:00.123456789+02:00").unwrap();
        assert_eq!(fields.start_time, "2026-09-15T08:00:00.123456789Z");
        assert_eq!(fields.end_time, "2026-09-15T09:00:00.987654321Z");
        assert!(resolve(&event, "2026-09-15T08:00:00.123456788Z").is_err());
        assert!(resolve(&event, "2026-09-15").is_err());
    }

    #[test]
    fn ancient_daily_and_weekly_counted_series_seek_and_index_arithmetically() {
        for rule in [
            "FREQ=DAILY;COUNT=4294967295",
            "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR,SA,SU;COUNT=4294967295",
        ] {
            let event = event("0001-01-01", "0001-01-02", None, rule);
            assert_eq!(
                starts(&event, "9998-12-01", "9998-12-03"),
                ["9998-12-01", "9998-12-02"]
            );
            let index = occurrence_index(&event, "9998-12-01").unwrap();
            assert!(index > CANDIDATE_BUDGET as u32);
            assert_eq!(position_at(&event, index).unwrap(), "9998-12-01");
            assert!(position_at(&event, u32::MAX).is_err());
        }
    }

    #[test]
    fn ancient_uncounted_zoned_rules_seek_without_history_enumeration() {
        for rule in ["FREQ=DAILY", "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR,SA,SU"] {
            let event = event(
                "0001-01-02T09:00:00Z",
                "0001-01-02T10:00:00Z",
                Some("Europe/Stockholm"),
                rule,
            );
            let result = expand(&event, "9998-12-01", "9998-12-03", 10).unwrap();
            assert_eq!(result.occurrences.len(), 2);
            assert!(!result.has_more);
            for generated in result.occurrences {
                assert_eq!(
                    resolve(&event, &generated.original_start).unwrap(),
                    generated.fields
                );
            }
        }
    }

    #[test]
    fn budget_exhaustion_is_never_reported_as_completion_or_nonmembership() {
        let mut event = event(
            "0001-01-02T09:00:00Z",
            "0001-01-02T10:00:00Z",
            Some("Europe/Stockholm"),
            "FREQ=DAILY;COUNT=4294967295",
        );
        let error = expand(&event, "9998-12-01", "9998-12-02", 1).unwrap_err();
        assert!(error.to_string().contains("work budget"));
        assert!(position_at(&event, 300_000)
            .unwrap_err()
            .to_string()
            .contains("work budget"));
        event.recurrence_rule = Some("FREQ=DAILY".into());
        let generated = expand(&event, "9998-12-01", "9998-12-02", 1)
            .unwrap()
            .occurrences
            .remove(0);
        assert!(occurrence_index(&event, &generated.original_start)
            .unwrap_err()
            .to_string()
            .contains("work budget"));
        event.recurrence_rule = Some("FREQ=DAILY;COUNT=4294967295".into());
        assert!(resolve(&event, &generated.original_start)
            .unwrap_err()
            .to_string()
            .contains("work budget"));
    }

    #[test]
    fn small_count_ends_before_expensive_historical_seek() {
        let event = event(
            "0001-01-02T09:00:00Z",
            "0001-01-02T10:00:00Z",
            Some("Europe/Stockholm"),
            "FREQ=DAILY;COUNT=2",
        );
        assert!(starts(&event, "9998-12-01", "9998-12-02").is_empty());
    }

    #[test]
    fn date_range_edges_terminate_or_error_explicitly() {
        let event = event("9999-01-01", "9999-01-02", None, "FREQ=YEARLY;INTERVAL=99");
        assert_eq!(starts(&event, "9999-01-01", "9999-12-31"), ["9999-01-01"]);
        assert!(position_at(&event, 1).is_err());
        let early = event_for_early_week();
        assert_positions(&early, &["0001-01-01", "0001-01-07", "0001-01-08"]);
    }

    fn event_for_early_week() -> CalendarEvent {
        event(
            "0001-01-01",
            "0001-01-02",
            None,
            "FREQ=WEEKLY;BYDAY=MO,SU;WKST=SU;COUNT=3",
        )
    }

    #[test]
    fn sought_expansion_matches_phase_positions_across_supported_patterns() {
        for (start, end, rule, zone) in [
            ("2026-01-31", "2026-02-02", "FREQ=MONTHLY;COUNT=40", None),
            ("2024-02-29", "2024-03-01", "FREQ=YEARLY;COUNT=8", None),
            (
                "2026-09-16",
                "2026-09-18",
                "FREQ=WEEKLY;INTERVAL=3;BYDAY=MO,WE,SU;WKST=FR;COUNT=40",
                None,
            ),
            (
                "2026-03-01T07:30:00Z",
                "2026-03-01T09:30:00Z",
                "FREQ=DAILY;COUNT=40",
                Some("America/New_York"),
            ),
        ] {
            let event = event(start, end, zone, rule);
            let full = expand(&event, "2024-01-01", "2060-01-01", 100).unwrap();
            assert!(!full.has_more);
            for (index, generated) in full.occurrences.iter().enumerate() {
                assert_eq!(
                    position_at(&event, index as u32).unwrap(),
                    generated.original_start
                );
                assert_eq!(
                    occurrence_index(&event, &generated.original_start).unwrap(),
                    index as u32
                );
                assert_eq!(
                    resolve(&event, &generated.original_start).unwrap(),
                    generated.fields
                );
                let sought = expand(
                    &event,
                    &generated.fields.start_time,
                    &generated.fields.end_time,
                    100,
                )
                .unwrap();
                assert!(sought.occurrences.contains(generated));
            }
        }
    }
}
