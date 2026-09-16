//! Scoped calendar edits and renderer projections. Native resources never cross
//! this module's DTO boundary; a selection always retains its original position.

use chrono::{
    DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc,
};
use serde::{Deserialize, Serialize};

use super::event_set::{
    apply_event_fields, canonical_position, event_fields, CalendarEventSet, CalendarOverride,
};
use super::recurrence_identity::{OccurrenceFields, RecurrenceMutationScope};
use super::{simple_recurrence as recurrence, CalendarEvent, RecurrenceKind};
use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalendarSelection {
    pub event_id: String,
    pub token: String,
    pub original_start: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalendarOccurrence {
    pub selection: CalendarSelection,
    pub event_id: String,
    pub account_id: String,
    pub calendar_id: String,
    pub fields: OccurrenceFields,
    pub recurrence_kind: RecurrenceKind,
    pub recurrence_rule: Option<String>,
    pub is_exception: bool,
}

#[derive(Debug, Serialize)]
pub struct CalendarOccurrencePage {
    pub occurrences: Vec<CalendarOccurrence>,
    pub has_more: bool,
    /// Cached provider masters whose complete finite set needs an authoritative
    /// read before generating positions. These IDs are ordinary event anchors.
    pub needs_hydration: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CalendarEventSetView {
    pub master: CalendarOccurrence,
    pub page: CalendarOccurrencePage,
    pub exception_count: usize,
}

pub(crate) fn event_set_view(
    set: &CalendarEventSet,
    anchor: &CalendarEvent,
    token: &str,
    start: &str,
    end: &str,
    limit: usize,
) -> Result<CalendarEventSetView> {
    Ok(CalendarEventSetView {
        page: project(set, anchor, token, start, end, limit)?,
        master: CalendarOccurrence {
            selection: CalendarSelection {
                event_id: anchor.id.clone(),
                token: token.to_string(),
                original_start: None,
            },
            event_id: anchor.id.clone(),
            account_id: anchor.account_id.clone(),
            calendar_id: anchor.calendar_id.clone(),
            fields: normalized_fields(event_fields(&set.event))?,
            recurrence_kind: set.event.recurrence_kind,
            recurrence_rule: set.event.recurrence_rule.clone(),
            is_exception: false,
        },
        exception_count: set.overrides.len(),
    })
}

/// Omitted properties remain unchanged. Empty optional strings clear a field;
/// an empty recurrence rule converts an entire series into a standalone event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalendarEdit {
    pub title: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub all_day: Option<bool>,
    pub timezone: Option<String>,
    pub recurrence_rule: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalendarActionInput {
    pub selection: CalendarSelection,
    pub scope: RecurrenceMutationScope,
    pub edit: CalendarEdit,
    pub destination_calendar_id: Option<String>,
    #[serde(default)]
    pub reset_exceptions: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalendarConfirmations {
    #[serde(default)]
    pub replacement_meeting_identity: bool,
    #[serde(default)]
    pub reset_exceptions: bool,
}

#[derive(Debug, Serialize)]
pub struct CalendarActionPlan {
    pub operation_id: String,
    pub requires: CalendarConfirmations,
    pub preview: OccurrenceFields,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CalendarActionStage {
    Planned,
    Applying,
    DestinationVerified,
    SourceRemovalPending,
    Reconciling,
    Completed,
}

#[derive(Debug, Serialize)]
pub struct CalendarActionResult {
    pub operation_id: String,
    pub stage: CalendarActionStage,
    pub event_id: String,
    pub requires: CalendarConfirmations,
}

pub(crate) fn invalid(message: &str) -> Error {
    Error::Other(format!("Calendar action: {message}"))
}

pub(crate) fn boundary(value: &str) -> Result<DateTime<Utc>> {
    if value.len() == 10 {
        return NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .ok()
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .map(|date| date.and_utc())
            .ok_or_else(|| invalid("invalid date"));
    }
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|_| invalid("time must include an RFC3339 offset"))
}

pub(crate) fn window(start: &str, end: &str, limit: usize) -> Result<()> {
    let duration = boundary(end)? - boundary(start)?;
    if duration <= Duration::zero()
        || duration > Duration::days(366)
        || !(1..=2000).contains(&limit)
    {
        return Err(invalid(
            "window must be positive and at most 366 days; limit must be 1..2000",
        ));
    }
    Ok(())
}

fn overlaps(fields: &OccurrenceFields, start: &str, end: &str) -> Result<bool> {
    Ok(boundary(&fields.start_time)? < boundary(end)?
        && boundary(&fields.end_time)? > boundary(start)?)
}

pub(crate) fn selected_event(set: &CalendarEventSet, key: Option<&str>) -> Result<CalendarEvent> {
    set.validate()?;
    match (set.event.recurrence_kind, key) {
        (RecurrenceKind::Standalone, None) => Ok(set.event.clone()),
        (RecurrenceKind::Series, Some(key)) => {
            let canonical = canonical_position(&set.event, key)?;
            if canonical != key {
                return Err(invalid("original position is not canonical"));
            }
            if let Some(exception) = set.overrides.iter().find(|item| item.original_start == key) {
                return exception
                    .event
                    .clone()
                    .ok_or_else(|| invalid("selected occurrence is excluded"));
            }
            let mut event = set.event.clone();
            apply_event_fields(&mut event, &recurrence::resolve(&set.event, key)?);
            event.recurrence_rule = None;
            event.recurrence_kind = RecurrenceKind::Occurrence;
            Ok(event)
        }
        (RecurrenceKind::Series, None) => Ok(set.event.clone()),
        _ => Err(invalid("selection does not match event classification")),
    }
}

pub(crate) fn project(
    set: &CalendarEventSet,
    anchor: &CalendarEvent,
    token: &str,
    start: &str,
    end: &str,
    limit: usize,
) -> Result<CalendarOccurrencePage> {
    window(start, end, limit)?;
    set.validate()?;
    let mut values = Vec::new();
    let mut has_more = false;
    if set.event.recurrence_kind == RecurrenceKind::Series {
        // Request enough original positions to compensate for all finite exclusions.
        let budget = limit
            .checked_add(set.overrides.len())
            .ok_or_else(|| invalid("too many exceptions"))?;
        if budget > 10_000 {
            return Err(invalid("projection exceeds exception budget"));
        }
        let expanded = recurrence::expand(&set.event, start, end, budget)?;
        has_more = expanded.has_more;
        for occurrence in expanded.occurrences {
            if !set
                .overrides
                .iter()
                .any(|item| item.original_start == occurrence.original_start)
            {
                values.push((Some(occurrence.original_start), occurrence.fields, false));
            }
        }
        // Moved-in overrides are found even when their original slot is outside the window.
        for exception in &set.overrides {
            if let Some(event) = &exception.event {
                let fields = event_fields(event);
                if overlaps(&fields, start, end)? {
                    values.push((Some(exception.original_start.clone()), fields, true));
                }
            }
        }
    } else if overlaps(&event_fields(&set.event), start, end)? {
        values.push((None, event_fields(&set.event), false));
    }
    for (_, fields, _) in &mut values {
        *fields = normalized_fields(fields.clone())?;
    }
    values.sort_by(|a, b| a.1.start_time.cmp(&b.1.start_time).then(a.0.cmp(&b.0)));
    has_more |= values.len() > limit;
    values.truncate(limit);
    Ok(CalendarOccurrencePage {
        needs_hydration: Vec::new(),
        occurrences: values
            .into_iter()
            .map(
                |(original_start, fields, is_exception)| CalendarOccurrence {
                    selection: CalendarSelection {
                        event_id: anchor.id.clone(),
                        token: token.into(),
                        original_start,
                    },
                    event_id: anchor.id.clone(),
                    account_id: anchor.account_id.clone(),
                    calendar_id: anchor.calendar_id.clone(),
                    fields,
                    recurrence_kind: set.event.recurrence_kind,
                    recurrence_rule: set.event.recurrence_rule.clone(),
                    is_exception,
                },
            )
            .collect(),
        has_more,
    })
}

fn zone(event: &CalendarEvent) -> Result<chrono_tz::Tz> {
    event
        .timezone
        .as_deref()
        .unwrap_or("UTC")
        .parse()
        .map_err(|_| invalid("unknown IANA timezone"))
}

fn local(value: &str, event: &CalendarEvent) -> Result<NaiveDateTime> {
    let instant = boundary(value)?;
    Ok(if event.all_day {
        instant.naive_utc()
    } else {
        instant.with_timezone(&zone(event)?).naive_local()
    })
}

fn from_local(value: NaiveDateTime, event: &CalendarEvent) -> Result<String> {
    if event.all_day {
        return Ok(value.date().to_string());
    }
    let instant = zone(event)?
        .from_local_datetime(&value)
        .earliest()
        .ok_or_else(|| invalid("reschedule lands in a nonexistent local time"))?;
    Ok(instant
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::AutoSi, true))
}

fn end_after(start: &str, duration: Duration, event: &CalendarEvent) -> Result<String> {
    let end = boundary(start)?
        .checked_add_signed(duration)
        .ok_or_else(|| invalid("date overflow"))?;
    Ok(if event.all_day {
        end.date_naive().to_string()
    } else {
        end.to_rfc3339_opts(SecondsFormat::AutoSi, true)
    })
}

fn apply_patch(event: &mut CalendarEvent, edit: &CalendarEdit) -> Result<()> {
    if let Some(value) = &edit.title {
        event.title.clone_from(value);
    }
    for (target, value) in [
        (&mut event.description, &edit.description),
        (&mut event.location, &edit.location),
        (&mut event.timezone, &edit.timezone),
    ] {
        if let Some(value) = value {
            *target = (!value.is_empty()).then(|| value.clone());
        }
    }
    if let Some(value) = edit.all_day {
        event.all_day = value;
    }
    let duration = boundary(&event.end_time)? - boundary(&event.start_time)?;
    if let Some(value) = &edit.start_time {
        event.start_time.clone_from(value);
        if edit.end_time.is_none() {
            event.end_time = end_after(value, duration, event)?;
        }
    }
    if let Some(value) = &edit.end_time {
        event.end_time.clone_from(value);
    }
    event_fields(event).validate()?;
    zone(event)?;
    Ok(())
}

fn shift_rule(before: &CalendarEvent, desired: &CalendarEvent, rule: &str) -> Result<String> {
    let shift = local(&desired.start_time, desired)? - local(&before.start_time, before)?;
    let day_shift = (local(&desired.start_time, desired)?.date()
        - local(&before.start_time, before)?.date())
    .num_days();
    let weekdays = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];
    let normalized = recurrence::normalize_rule(rule, before)?;
    normalized
        .split(';')
        .map(|part| {
            if let Some(days) = part.strip_prefix("BYDAY=") {
                let shifted = days
                    .split(',')
                    .map(|day| {
                        let index = weekdays
                            .iter()
                            .position(|value| *value == day)
                            .ok_or_else(|| invalid("invalid weekday"))?;
                        Ok(weekdays[(index as i64 + day_shift).rem_euclid(7) as usize])
                    })
                    .collect::<Result<Vec<_>>>()?;
                return Ok(format!("BYDAY={}", shifted.join(",")));
            }
            if let Some(until) = part.strip_prefix("UNTIL=") {
                if let Ok(date) = NaiveDate::parse_from_str(until, "%Y%m%d") {
                    let shifted = date
                        .checked_add_signed(Duration::days(day_shift))
                        .ok_or_else(|| invalid("UNTIL overflow"))?;
                    return Ok(format!("UNTIL={}", shifted.format("%Y%m%d")));
                }
                let utc = NaiveDateTime::parse_from_str(until, "%Y%m%dT%H%M%SZ")
                    .map_err(|_| invalid("invalid UNTIL"))?
                    .and_utc();
                let new_local = local(&utc.to_rfc3339(), before)?
                    .checked_add_signed(shift)
                    .ok_or_else(|| invalid("UNTIL overflow"))?;
                return Ok(format!(
                    "UNTIL={}",
                    boundary(&from_local(new_local, desired)?)?.format("%Y%m%dT%H%M%SZ")
                ));
            }
            Ok(part.to_string())
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join(";"))
}

/// Pure edit algebra. Series times refer to the selected occurrence, so dragging
/// a later occurrence never substitutes that occurrence for the master's DTSTART.
pub(crate) fn desired_set(
    before: &CalendarEventSet,
    input: &CalendarActionInput,
) -> Result<CalendarEventSet> {
    before.validate()?;
    let selected = selected_event(before, input.selection.original_start.as_deref())?;
    let mut desired = before.clone();
    desired.capture_content()?;
    if input.scope == RecurrenceMutationScope::ThisOccurrence
        && before.event.recurrence_kind == RecurrenceKind::Series
    {
        if input.edit.recurrence_rule.is_some() || input.reset_exceptions {
            return Err(invalid("recurrence changes require entire-series scope"));
        }
        let key = input
            .selection
            .original_start
            .clone()
            .ok_or_else(|| invalid("an occurrence position is required"))?;
        let mut event = selected;
        apply_patch(&mut event, &input.edit)?;
        event.recurrence_rule = None;
        event.recurrence_kind = RecurrenceKind::Occurrence;
        if let Some(exception) = desired
            .overrides
            .iter_mut()
            .find(|item| item.original_start == key)
        {
            exception.event = Some(event);
        } else {
            desired.overrides.push(CalendarOverride {
                original_start: key.clone(),
                event: Some(event),
                native: None,
            });
        }
        if input.edit.description.is_some() {
            desired.mark_description_plain(Some(&key))?;
        }
    } else {
        let mut edited = selected.clone();
        apply_patch(&mut edited, &input.edit)?;
        let mut master_edit = input.edit.clone();
        if before.event.recurrence_kind == RecurrenceKind::Series
            && input.selection.original_start.is_some()
        {
            if before.event.all_day != edited.all_day {
                return Err(invalid(
                    "change all-day mode using the series master selection",
                ));
            }
            let shift =
                local(&edited.start_time, &edited)? - local(&selected.start_time, &selected)?;
            let new_start = local(&before.event.start_time, &before.event)?
                .checked_add_signed(shift)
                .ok_or_else(|| invalid("date overflow"))?;
            let mut template = before.event.clone();
            template.timezone = edited.timezone.clone();
            if input.edit.start_time.is_some() || input.edit.timezone.is_some() {
                master_edit.start_time = Some(from_local(new_start, &template)?);
            }
            if input.edit.start_time.is_some()
                || input.edit.end_time.is_some()
                || input.edit.timezone.is_some()
            {
                let start = master_edit
                    .start_time
                    .as_deref()
                    .unwrap_or(&before.event.start_time);
                master_edit.end_time = Some(end_after(
                    start,
                    if input.edit.end_time.is_some() {
                        boundary(&edited.end_time)? - boundary(&edited.start_time)?
                    } else {
                        boundary(&before.event.end_time)? - boundary(&before.event.start_time)?
                    },
                    &template,
                )?);
            }
        }
        apply_patch(&mut desired.event, &master_edit)?;
        let mut inheriting_descriptions = std::collections::HashSet::new();
        if input.edit.description.is_some() {
            for item in &before.overrides {
                if item
                    .event
                    .as_ref()
                    .is_some_and(|event| event.description == before.event.description)
                    && before.description_content_type(Some(&item.original_start))?
                        == before.description_content_type(None)?
                {
                    inheriting_descriptions.insert(item.original_start.clone());
                }
            }
            desired.mark_description_plain(None)?;
            for key in &inheriting_descriptions {
                desired.mark_description_plain(Some(key))?;
            }
        }
        if input.edit.recurrence_rule.is_none()
            && (before.event.start_time != desired.event.start_time
                || before.event.timezone != desired.event.timezone)
        {
            if let Some(rule) = before.event.recurrence_rule.as_deref() {
                desired.event.recurrence_rule =
                    Some(shift_rule(&before.event, &desired.event, rule)?);
            }
        }
        if let Some(rule) = &input.edit.recurrence_rule {
            desired.event.recurrence_rule = if rule.is_empty() {
                None
            } else {
                Some(recurrence::normalize_rule(rule, &desired.event)?)
            };
            desired.event.recurrence_kind =
                RecurrenceKind::from_rule(desired.event.recurrence_rule.as_deref());
        }
        if let Some(rule) = desired.event.recurrence_rule.as_deref() {
            desired.event.recurrence_rule = Some(recurrence::normalize_rule(rule, &desired.event)?);
        }
        if input.reset_exceptions {
            desired.overrides.clear();
            desired.remap_content(&[]);
        } else if !before.overrides.is_empty() {
            if desired.event.recurrence_kind != RecurrenceKind::Series {
                return Err(invalid(
                    "removing recurrence requires explicit exception reset",
                ));
            }
            // Map finite exceptions by recurrence phase, retaining cancellations and
            // each exception's displacement from its original wall-clock position.
            let mut positions = Vec::new();
            for exception in &mut desired.overrides {
                let index = recurrence::occurrence_index(&before.event, &exception.original_start)?;
                let new_key = recurrence::position_at(&desired.event, index).map_err(|_| {
                    invalid("new pattern cannot retain all exceptions; explicitly reset exceptions")
                })?;
                if let Some(event) = &mut exception.event {
                    let displacement = local(&event.start_time, event)?
                        - local(&exception.original_start, &before.event)?;
                    let duration = boundary(&event.end_time)? - boundary(&event.start_time)?;
                    let new_local = local(&new_key, &desired.event)?
                        .checked_add_signed(displacement)
                        .ok_or_else(|| invalid("exception date overflow"))?;
                    event.all_day = desired.event.all_day;
                    event.timezone = desired.event.timezone.clone();
                    event.start_time = from_local(new_local, event)?;
                    event.end_time = end_after(&event.start_time, duration, event)?;
                    // Only inheriting values follow a sparse whole-series edit.
                    if event.title == before.event.title && input.edit.title.is_some() {
                        event.title = desired.event.title.clone();
                    }
                    if inheriting_descriptions.contains(&exception.original_start) {
                        event.description = desired.event.description.clone();
                    }
                    if event.location == before.event.location && input.edit.location.is_some() {
                        event.location = desired.event.location.clone();
                    }
                }
                positions.push((exception.original_start.clone(), new_key.clone()));
                exception.original_start = new_key;
            }
            desired.remap_content(&positions);
        }
        // Verify that the selected phase still lands exactly on the requested
        // start. Monthly skips or BYDAY changes must never silently shift it.
        if before.event.recurrence_kind == RecurrenceKind::Series
            && desired.event.recurrence_kind == RecurrenceKind::Series
            && input.edit.start_time.is_some()
        {
            if let Some(key) = input.selection.original_start.as_deref() {
                let index = recurrence::occurrence_index(&before.event, key)?;
                let new_key = recurrence::position_at(&desired.event, index)?;
                let old_fields = recurrence::resolve(&before.event, key)?;
                let displacement = local(&selected.start_time, &selected)?
                    - local(&old_fields.start_time, &before.event)?;
                if local(&new_key, &desired.event)? + displacement
                    != local(&edited.start_time, &edited)?
                {
                    return Err(invalid(
                        "reschedule cannot preserve the selected recurrence phase",
                    ));
                }
            }
        }
    }
    desired.validate()?;
    Ok(desired)
}

pub(crate) fn semantic_eq(left: &CalendarEventSet, right: &CalendarEventSet) -> bool {
    equivalent_sets(left, right).unwrap_or(false)
}

/// Equality is a proof over the supported schedule and complete effective
/// content. Identity and account-local RSVP projections are checked separately
/// by the coordinator; an unsupported representation is never positive evidence.
fn equivalent_sets(left: &CalendarEventSet, right: &CalendarEventSet) -> Result<bool> {
    fn normalized(set: &CalendarEventSet) -> Result<CalendarEventSet> {
        let mut set = set.clone();
        set.event.timezone = canonical_zone(&set.event)?;
        set.event.recurrence_rule = semantic_rule(&set.event)?;
        let mut positions = Vec::new();
        for item in &mut set.overrides {
            let key = canonical_position(&set.event, &item.original_start)?;
            positions.push((item.original_start.clone(), key.clone()));
            item.original_start = key;
            if let Some(event) = &mut item.event {
                event.timezone = canonical_zone(event)?;
            }
        }
        set.remap_content(&positions);
        set.validate()?;
        Ok(set)
    }
    let left = normalized(left)?;
    let right = normalized(right)?;
    if left.event.recurrence_kind != right.event.recurrence_kind
        || !rules_equivalent(&left.event, &right.event)?
        || !content_equivalent(&left, None, &right, None)?
    {
        return Ok(false);
    }
    let keys: std::collections::BTreeSet<_> = left
        .overrides
        .iter()
        .chain(&right.overrides)
        .map(|item| item.original_start.as_str())
        .collect();
    for key in keys {
        // Membership is necessary even for matching exclusions: an arbitrary
        // detached event must not disappear as an alleged generated default.
        recurrence::resolve(&left.event, key)?;
        recurrence::resolve(&right.event, key)?;
        let a = left
            .overrides
            .iter()
            .find(|item| item.original_start == key);
        let b = right
            .overrides
            .iter()
            .find(|item| item.original_start == key);
        match (a, b) {
            (Some(a), Some(b)) if a.event.is_none() || b.event.is_none() => {
                if a.event.is_some() != b.event.is_some() {
                    return Ok(false);
                }
            }
            (Some(a), None) if a.event.is_none() => return Ok(false),
            (None, Some(b)) if b.event.is_none() => return Ok(false),
            _ if !content_equivalent(&left, Some(key), &right, Some(key))? => return Ok(false),
            _ => {}
        }
    }
    Ok(true)
}

fn content_equivalent(
    left: &CalendarEventSet,
    left_key: Option<&str>,
    right: &CalendarEventSet,
    right_key: Option<&str>,
) -> Result<bool> {
    let a = selected_event(left, left_key)?;
    let b = selected_event(right, right_key)?;
    let mut af = normalized_fields(event_fields(&a))?;
    let mut bf = normalized_fields(event_fields(&b))?;
    af.timezone = canonical_zone(&a)?;
    bf.timezone = canonical_zone(&b)?;
    // Description format and participants require private native provenance.
    af.description = None;
    bf.description = None;
    Ok(af == bf
        && left
            .semantic_content(left_key)?
            .equivalent(&right.semantic_content(right_key)?)?)
}

fn canonical_zone(event: &CalendarEvent) -> Result<Option<String>> {
    if event.all_day {
        // DATE boundaries are exclusive calendar dates, independent of a zone.
        return Ok(None);
    }
    let name = event.timezone.as_deref().unwrap_or("UTC");
    let name = if name.parse::<chrono_tz::Tz>().is_ok() {
        name
    } else {
        super::timezone::windows_to_iana(name).unwrap_or(name)
    };
    // IANA backward links, not cities selected by their current UTC offset.
    // Unlisted aliases conservatively compare unequal until explicitly supported.
    let name = match name {
        "Etc/UTC" | "Etc/UCT" | "UCT" | "Universal" | "Etc/Universal" | "Zulu" | "Etc/Zulu"
        | "GMT" | "Etc/GMT" | "GMT0" | "Etc/GMT0" | "Greenwich" | "Etc/Greenwich" | "GMT+0"
        | "GMT-0" | "Etc/GMT+0" | "Etc/GMT-0" => "UTC",
        "US/Eastern" => "America/New_York",
        "US/Central" => "America/Chicago",
        "US/Mountain" => "America/Denver",
        "US/Pacific" => "America/Los_Angeles",
        "US/Arizona" => "America/Phoenix",
        "US/Alaska" => "America/Anchorage",
        "US/Aleutian" => "America/Adak",
        "US/Hawaii" => "Pacific/Honolulu",
        "Asia/Calcutta" => "Asia/Kolkata",
        "Asia/Katmandu" => "Asia/Kathmandu",
        "Asia/Saigon" => "Asia/Ho_Chi_Minh",
        "Europe/Kiev" => "Europe/Kyiv",
        "GB" | "GB-Eire" => "Europe/London",
        "Japan" => "Asia/Tokyo",
        "NZ" => "Pacific/Auckland",
        "Australia/ACT" | "Australia/NSW" => "Australia/Sydney",
        "Australia/Victoria" => "Australia/Melbourne",
        "Canada/Eastern" => "America/Toronto",
        "Canada/Pacific" => "America/Vancouver",
        other => other,
    };
    name.parse::<chrono_tz::Tz>()
        .map_err(|_| invalid("unknown recurrence timezone"))?;
    Ok(Some(name.to_owned()))
}

fn semantic_rule(event: &CalendarEvent) -> Result<Option<String>> {
    let Some(rule) = event.recurrence_rule.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let uppercase = rule.trim().to_ascii_uppercase();
    let rule = uppercase.strip_prefix("RRULE:").unwrap_or(&uppercase);
    let parts = rule
        .split(';')
        .map(|part| {
            let Some((key, value)) = part.split_once('=') else {
                return Ok(part.to_owned());
            };
            if key.trim() != "UNTIL" {
                return Ok(part.to_owned());
            }
            let value = value.trim();
            if value.len() == 8 || value.len() == 10 || value.len() == 16 && value.ends_with('Z') {
                return Ok(part.to_owned());
            }
            let instant = if let Ok(instant) = DateTime::parse_from_rfc3339(value) {
                instant.with_timezone(&Utc)
            } else {
                let local = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
                    .map_err(|_| invalid("unsupported UNTIL representation"))?;
                zone(event)?
                    .from_local_datetime(&local)
                    .single()
                    .ok_or_else(|| invalid("ambiguous UNTIL wall clock"))?
                    .with_timezone(&Utc)
            };
            // The supported RRULE language has whole-second cutoff precision.
            if instant.timestamp_subsec_nanos() != 0 {
                return Err(invalid("fractional UNTIL"));
            }
            Ok(format!("UNTIL={}", instant.format("%Y%m%dT%H%M%SZ")))
        })
        .collect::<Result<Vec<_>>>()?
        .join(";");
    let rule = recurrence::normalize_rule(&parts, event)?;
    let mut parts: std::collections::BTreeMap<_, _> = rule
        .split(';')
        .filter_map(|part| part.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    if parts.get("FREQ").is_some_and(|v| v == "WEEKLY") {
        parts.entry("BYDAY".into()).or_insert_with(|| {
            let date = if event.all_day {
                boundary(&event.start_time).map(|d| d.date_naive())
            } else {
                local(&event.start_time, event).map(|d| d.date())
            };
            // normalize_rule has already validated DTSTART and the zone.
            ["MO", "TU", "WE", "TH", "FR", "SA", "SU"][date
                .expect("validated DTSTART")
                .weekday()
                .num_days_from_monday()
                as usize]
                .into()
        });
        if !parts.contains_key("INTERVAL") {
            parts.remove("WKST");
        }
    }
    Ok(Some(
        parts
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(";"),
    ))
}

fn rules_equivalent(a: &CalendarEvent, b: &CalendarEvent) -> Result<bool> {
    if a.recurrence_rule == b.recurrence_rule {
        return Ok(true);
    }
    let (Some(ar), Some(br)) = (&a.recurrence_rule, &b.recurrence_rule) else {
        return Ok(false);
    };
    fn split(rule: &str) -> (String, Option<&str>) {
        (
            rule.split(';')
                .filter(|p| !p.starts_with("UNTIL="))
                .collect::<Vec<_>>()
                .join(";"),
            rule.split(';').find_map(|p| p.strip_prefix("UNTIL=")),
        )
    }
    let (ar, au) = split(ar);
    let (br, bu) = split(br);
    if ar != br {
        return Ok(false);
    }
    let (Some(au), Some(bu)) = (au, bu) else {
        return Ok(false);
    };
    fn exclusive(value: &str, event: &CalendarEvent) -> Result<DateTime<Utc>> {
        if let Ok(date) = NaiveDate::parse_from_str(value, "%Y%m%d") {
            let next = date
                .succ_opt()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .ok_or_else(|| invalid("UNTIL overflow"))?;
            return if event.all_day {
                Ok(next.and_utc())
            } else {
                zone(event)?
                    .from_local_datetime(&next)
                    .single()
                    .map(|d| d.with_timezone(&Utc))
                    .ok_or_else(|| invalid("ambiguous UNTIL date boundary"))
            };
        }
        NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ")
            .ok()
            .and_then(|d| d.and_utc().checked_add_signed(Duration::nanoseconds(1)))
            .ok_or_else(|| invalid("invalid UNTIL"))
    }
    let (a_end, b_end) = (exclusive(au, a)?, exclusive(bu, b)?);
    if a_end == b_end {
        return Ok(true);
    }
    // Same infinite pattern: only the interval between the two exclusive cutoffs
    // can differ. Seek there with a bounded generator, never sample a prefix.
    let mut unbounded = a.clone();
    unbounded.recurrence_rule = Some(ar);
    let lo = a_end.min(b_end);
    let hi = a_end.max(b_end);
    let expanded = recurrence::expand(&unbounded, &lo.to_rfc3339(), &hi.to_rfc3339(), 10_000)?;
    Ok(!expanded.has_more
        && expanded
            .occurrences
            .iter()
            .all(|o| boundary(&o.original_start).is_ok_and(|start| start < lo)))
}

fn normalized_fields(mut fields: OccurrenceFields) -> Result<OccurrenceFields> {
    fields.validate()?;
    fields.description = fields.description.filter(|value| !value.is_empty());
    fields.location = fields.location.filter(|value| !value.is_empty());
    if !fields.all_day {
        fields.start_time =
            boundary(&fields.start_time)?.to_rfc3339_opts(SecondsFormat::AutoSi, true);
        fields.end_time = boundary(&fields.end_time)?.to_rfc3339_opts(SecondsFormat::AutoSi, true);
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native(set: &mut CalendarEventSet, protocol: &str, value: serde_json::Value) {
        set.content = None;
        set.native = Some(super::super::event_set::NativeCalendarResource {
            protocol: protocol.into(),
            calendar_id: "remote-calendar".into(),
            event_id: "remote-event".into(),
            revision: Some("revision".into()),
            data: value.to_string(),
        });
    }

    fn graph_body(set: &mut CalendarEventSet, kind: &str, body: &str) {
        set.event.description = Some(body.into());
        native(
            set,
            "graph",
            serde_json::json!({"body": {"contentType": kind, "content": body}, "attendees": []}),
        );
    }

    fn attendee(set: &mut CalendarEventSet, json: serde_json::Value) {
        set.event.attendees_json = Some(json.to_string());
    }

    #[test]
    fn semantic_rule_order_case_defaults_and_weekly_dtstart_day() {
        let mut a = series();
        a.event.recurrence_rule = Some("FREQ=WEEKLY;COUNT=8".into());
        let mut b = a.clone();
        b.event.recurrence_rule =
            Some("rrule:count=008;wkst=mo;byday=sa,sa;interval=01;freq=weekly".into());
        assert!(semantic_eq(&a, &b));
        b.event.recurrence_rule = Some("FREQ=WEEKLY;BYDAY=SA,SU;COUNT=8".into());
        assert!(!semantic_eq(&a, &b));
        b.event.recurrence_rule = Some("FREQ=WEEKLY;COUNT=9".into());
        assert!(!semantic_eq(&a, &b));
        b.event.recurrence_rule = Some("FREQ=WEEKLY;BYMONTH=3;COUNT=8".into());
        assert!(!semantic_eq(&b, &b));
    }

    #[test]
    fn semantic_week_start_remains_meaningful_for_multiweek_intervals() {
        let mut a = series();
        a.event.recurrence_rule = Some("FREQ=WEEKLY;INTERVAL=2;BYDAY=SA,SU;COUNT=8".into());
        let mut b = a.clone();
        b.event.recurrence_rule = Some("FREQ=WEEKLY;INTERVAL=2;WKST=SU;BYDAY=SU,SA;COUNT=8".into());
        assert!(!semantic_eq(&a, &b));
    }

    #[test]
    fn semantic_until_compares_complete_cutoff_difference_in_master_zone() {
        let mut a = series();
        a.event.recurrence_rule = Some("FREQ=DAILY;UNTIL=20260310".into());
        for until in [
            "2026-03-10",
            "20260310T235959",
            "20260311T035959Z",
            "2026-03-10T23:59:59-04:00",
            "20260310T130000Z",
        ] {
            let mut b = a.clone();
            b.event.recurrence_rule = Some(format!("UNTIL={until};FREQ=DAILY"));
            assert!(semantic_eq(&a, &b), "{until}");
            assert!(semantic_eq(&b, &a), "{until}");
        }
        for until in [
            "20260310T125959Z",
            "20260311T130000Z",
            "2026-03-10T13:00:00.1Z",
        ] {
            let mut b = a.clone();
            b.event.recurrence_rule = Some(format!("FREQ=DAILY;UNTIL={until}"));
            assert!(!semantic_eq(&a, &b), "{until}");
        }
    }

    #[test]
    fn semantic_until_does_not_sample_only_a_near_term_prefix() {
        let mut a = series();
        a.event.recurrence_rule = Some("FREQ=YEARLY;UNTIL=20990307".into());
        let mut b = a.clone();
        b.event.recurrence_rule = Some("FREQ=YEARLY;UNTIL=21000307".into());
        assert!(!semantic_eq(&a, &b));
    }

    #[test]
    fn semantic_instants_aliases_and_utc_defaults_preserve_recurrence_zone() {
        let a = series();
        let mut b = a.clone();
        b.event.start_time = "2026-03-07T09:00:00.000-05:00".into();
        b.event.end_time = "2026-03-07T10:00:00-05:00".into();
        b.event.timezone = Some("US/Eastern".into());
        assert!(semantic_eq(&a, &b));
        b.event.timezone = Some("America/Lima".into());
        assert!(!semantic_eq(&a, &b));
        b.event.timezone = Some("America/Toronto".into());
        assert!(!semantic_eq(&a, &b));
        let mut a = a;
        a.event.timezone = None;
        b.event.timezone = Some("Etc/UTC".into());
        assert!(semantic_eq(&a, &b));
        b.event.end_time = "2026-03-07T15:00:00.001Z".into();
        assert!(!semantic_eq(&a, &b));
    }

    #[test]
    fn semantic_all_day_uses_exclusive_dates_not_midnight_instants() {
        let mut a = series();
        a.event.all_day = true;
        a.event.start_time = "2026-03-07".into();
        a.event.end_time = "2026-03-09".into();
        a.event.recurrence_rule = Some("FREQ=DAILY;UNTIL=20260310".into());
        let mut b = a.clone();
        b.event.timezone = None;
        b.event.recurrence_rule = Some("FREQ=DAILY;INTERVAL=1;UNTIL=2026-03-10".into());
        assert!(semantic_eq(&a, &b));
        b.event.end_time = "2026-03-08".into();
        assert!(!semantic_eq(&a, &b));
        b.event.end_time = a.event.end_time.clone();
        b.event.all_day = false;
        b.event.start_time = "2026-03-07T00:00:00Z".into();
        b.event.end_time = "2026-03-09T00:00:00Z".into();
        assert!(!semantic_eq(&a, &b));
    }

    #[test]
    fn semantic_identity_is_ignored_but_core_content_is_not() {
        let a = series();
        let mut b = a.clone();
        b.event.id = "new-event".into();
        b.event.uid = Some("new-uid".into());
        b.event.account_id = "new-account".into();
        b.event.calendar_id = "new-calendar".into();
        b.event.remote_id = Some("remote".into());
        b.event.etag = Some("new-revision".into());
        b.event.source_message_id = Some("source".into());
        b.event.my_status = Some("accepted".into());
        assert!(semantic_eq(&a, &b));
        b.event.title = "Changed".into();
        assert!(!semantic_eq(&a, &b));
        b.event.title = a.event.title.clone();
        b.event.location = Some("New room".into());
        assert!(!semantic_eq(&a, &b));
        b.event.location = Some(String::new());
        assert!(semantic_eq(&a, &b));
        b.event.description = None;
        assert!(!semantic_eq(&a, &b));
    }

    #[test]
    fn semantic_restored_override_requires_all_effective_content() {
        let a = series();
        let key = "2026-03-08T13:00:00Z";
        let generated = selected_event(&a, Some(key)).unwrap();
        let mut b = a.clone();
        b.overrides.push(CalendarOverride {
            original_start: "2026-03-08T09:00:00-04:00".into(),
            event: Some(generated.clone()),
            native: None,
        });
        assert!(semantic_eq(&a, &b));
        assert!(semantic_eq(&b, &a));
        for change in [
            "description",
            "location",
            "time",
            "attendee",
            "organizer",
            "title",
        ] {
            let event = b.overrides[0].event.as_mut().unwrap();
            *event = generated.clone();
            match change {
                "description" => event.description = Some("Unique agenda".into()),
                "location" => event.location = Some("Special room".into()),
                "time" => event.end_time = "2026-03-08T15:00:00Z".into(),
                "attendee" => {
                    event.attendees_json = Some(r#"[{"email":"extra@example.test"}]"#.into())
                }
                "organizer" => event.organizer_email = Some("owner@example.test".into()),
                _ => event.title = "Unique title".into(),
            }
            assert!(!semantic_eq(&a, &b), "{change}");
        }
        b.overrides[0].event = None;
        assert!(!semantic_eq(&a, &b));
    }

    #[test]
    fn semantic_exception_order_canonical_keys_and_duplicate_rejection() {
        let mut a = series();
        let mut changed = selected_event(&a, Some("2026-03-08T13:00:00Z")).unwrap();
        changed.title = "Changed".into();
        a.overrides = vec![
            CalendarOverride {
                original_start: "2026-03-08T13:00:00Z".into(),
                event: Some(changed),
                native: None,
            },
            CalendarOverride {
                original_start: "2026-03-09T13:00:00Z".into(),
                event: None,
                native: None,
            },
        ];
        let mut b = a.clone();
        b.overrides.reverse();
        b.overrides[1].original_start = "2026-03-08T09:00:00.000-04:00".into();
        assert!(semantic_eq(&a, &b));
        b.overrides.push(b.overrides[1].clone());
        assert!(!semantic_eq(&a, &b));
        b = a.clone();
        b.overrides[0].original_start = "2026-03-10T13:00:00Z".into();
        assert!(!semantic_eq(&a, &b));
        a.overrides[1].original_start = "2026-03-20T13:00:00Z".into();
        assert!(!semantic_eq(&a, &a));
    }

    #[test]
    fn semantic_attendees_normalize_identity_order_status_and_organizer_entry() {
        let mut a = series();
        a.event.organizer_email = Some("owner@example.test".into());
        attendee(
            &mut a,
            serde_json::json!([
            {"email":"One@example.test", "name":"One"},
            {"email":"two@example.test", "status":"TENTATIVE", "role":"optional"}]),
        );
        let mut b = a.clone();
        b.event.organizer_email = Some("MAILTO:OWNER@EXAMPLE.TEST".into());
        attendee(
            &mut b,
            serde_json::json!([
            {"email":"two@example.test", "status":"tentativelyAccepted", "role":"OPT-PARTICIPANT"},
            {"email":"owner@example.test", "status":"accepted"},
            {"email":"mailto:one@EXAMPLE.TEST", "status":"needsAction", "name":"Directory label", "is_self":false}]),
        );
        assert!(semantic_eq(&a, &b));
        for bad in [
            serde_json::json!([]),
            serde_json::json!([{"email":"one@example.test","status":"accepted"},{"email":"two@example.test","status":"tentative","role":"optional"}]),
            serde_json::json!([{"email":"other@example.test"},{"email":"two@example.test","status":"tentative","role":"optional"}]),
            serde_json::json!([{"email":"one@example.test"},{"email":"two@example.test","status":"tentative"}]),
            serde_json::json!([{"email":"one@example.test"},{"email":"one@example.test"}]),
            serde_json::json!([{"email":"one@example.test","status":"invalid"}]),
        ] {
            attendee(&mut b, bad);
            assert!(!semantic_eq(&a, &b));
        }
        b = a.clone();
        b.event.organizer_email = Some("different@example.test".into());
        assert!(!semantic_eq(&a, &b));
        b = a.clone();
        b.event.attendees_json = Some("{}".into());
        assert!(!semantic_eq(&b, &b));
    }

    #[test]
    fn semantic_new_meeting_resets_responses_in_master_and_overrides_explicitly() {
        let mut a = series();
        a.event.organizer_email = Some("owner@example.test".into());
        attendee(
            &mut a,
            serde_json::json!([{"email":"guest@example.test", "name":"Guest", "status":"accepted"}]),
        );
        let mut changed = selected_event(&a, Some("2026-03-08T13:00:00Z")).unwrap();
        changed.attendees_json = Some(
            serde_json::json!([{"email":"other@example.test", "status":"declined"}]).to_string(),
        );
        a.overrides.push(CalendarOverride {
            original_start: "2026-03-08T13:00:00Z".into(),
            event: Some(changed),
            native: None,
        });
        let mut desired = a.clone();
        desired.prepare_new_meeting("owner@example.test").unwrap();
        assert!(!semantic_eq(&a, &desired));
        let attendees: serde_json::Value =
            serde_json::from_str(desired.event.attendees_json.as_ref().unwrap()).unwrap();
        assert_eq!(attendees[0]["status"], "needs-action");
        assert_eq!(attendees[0]["name"], "Guest");
        let exception: serde_json::Value = serde_json::from_str(
            desired.overrides[0]
                .event
                .as_ref()
                .unwrap()
                .attendees_json
                .as_ref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(exception[0]["status"], "needs-action");
        let mut canonical = desired.clone();
        attendee(
            &mut canonical,
            serde_json::json!([{"email":"guest@example.test", "status":"needs-action"},
            {"email":"owner@example.test","status":"accepted"}]),
        );
        assert!(semantic_eq(&desired, &canonical));
        attendee(
            &mut canonical,
            serde_json::json!([{"email":"guest@example.test", "status":"accepted"}]),
        );
        assert!(!semantic_eq(&desired, &canonical));
    }

    #[test]
    fn semantic_native_roles_survive_lossy_provider_attendee_dtos() {
        let mut a = series();
        attendee(
            &mut a,
            serde_json::json!([{"email":"guest@example.test", "status":"needs-action"}]),
        );
        native(
            &mut a,
            "google",
            serde_json::json!({"description":"Agenda", "attendees":[
            {"email":"guest@example.test", "optional":true, "responseStatus":"needsAction"}]}),
        );
        let mut b = a.clone();
        native(
            &mut b,
            "graph",
            serde_json::json!({"body":{"contentType":"text","content":"Agenda"}, "attendees":[
            {"emailAddress":{"address":"guest@example.test"}, "type":"optional", "status":{"response":"notResponded"}}]}),
        );
        assert!(semantic_eq(&a, &b));
        for kind in ["required", "resource"] {
            native(
                &mut b,
                "graph",
                serde_json::json!({"body":{"contentType":"text","content":"Agenda"}, "attendees":[
                {"emailAddress":{"address":"guest@example.test"}, "type":kind, "status":{"response":"notResponded"}}]}),
            );
            assert!(!semantic_eq(&a, &b));
        }
    }

    #[test]
    fn semantic_description_plain_materialization_is_narrow_and_faithful() {
        let mut a = series();
        a.event.description = Some("A & B\n<literal>".into());
        for html in [
            "<div>A &amp; B<br>&lt;literal&gt;</div>",
            "<html><body><p>A &#38; B<br />&#x3c;literal&#62;</p></body></html>",
        ] {
            let mut b = a.clone();
            graph_body(&mut b, "html", html);
            assert!(semantic_eq(&a, &b), "{html}");
        }
        let mut b = a.clone();
        graph_body(&mut b, "text", "A & B\r\n<literal>");
        assert!(semantic_eq(&a, &b));
        for html in [
            "<b>A &amp; B</b><br>&lt;literal&gt;",
            "<div style=\"color:red\">A &amp; B<br>&lt;literal&gt;</div>",
            "<div>A &amp; B &lt;literal&gt;</div>",
            "A &amp; B\n&lt;literal&gt;",
            "<div>A&nbsp;&amp; B<br>&lt;literal&gt;</div>",
        ] {
            graph_body(&mut b, "html", html);
            assert!(!semantic_eq(&a, &b), "{html}");
        }
    }

    #[test]
    fn semantic_rich_body_cannot_be_verified_by_stripping_or_literal_markup() {
        let mut a = series();
        graph_body(
            &mut a,
            "html",
            "<p><b>Agenda</b> <a href=\"https://example.test\">link</a></p>",
        );
        let mut b = a.clone();
        assert!(semantic_eq(&a, &b));
        let rich = a.event.description.clone().unwrap();
        graph_body(&mut b, "text", &rich);
        assert!(!semantic_eq(&a, &b));
        graph_body(&mut b, "text", "Agenda link");
        assert!(!semantic_eq(&a, &b));
        b.event.description = Some(rich.clone());
        native(&mut b, "google", serde_json::json!({"description":rich}));
        assert!(semantic_eq(&a, &b));
        native(
            &mut b,
            "jmap",
            serde_json::json!({"event":{"description":rich,"descriptionContentType":"text/html"}}),
        );
        assert!(semantic_eq(&a, &b));
        native(
            &mut b,
            "jmap",
            serde_json::json!({"event":{"description":rich}}),
        );
        assert!(!semantic_eq(&a, &b));
    }

    #[test]
    fn semantic_jmap_effective_override_uses_its_own_body_type_and_participants() {
        let mut a = series();
        let key = "2026-03-08T13:00:00Z";
        let mut event = selected_event(&a, Some(key)).unwrap();
        event.description = Some("<b>Special</b>".into());
        a.overrides.push(CalendarOverride {
            original_start: key.into(),
            event: Some(event),
            native: None,
        });
        native(
            &mut a,
            "jmap",
            serde_json::json!({"event":{"description":"Agenda", "timeZone":"America/New_York",
            "recurrenceOverrides":{"2026-03-08T09:00:00":{"description":"<b>Special</b>","descriptionContentType":"text/html"}}}}),
        );
        let mut b = a.clone();
        native(
            &mut b,
            "jmap",
            serde_json::json!({"event":{"description":"Agenda", "timeZone":"America/New_York",
            "recurrenceOverrides":{"2026-03-08T09:00:00":{"description":"<b>Special</b>"}}}}),
        );
        assert!(!semantic_eq(&a, &b));
        assert!(semantic_eq(&a, &a));
    }

    #[test]
    fn semantic_caldav_description_is_text_and_alternate_body_is_not_discarded() {
        let mut a = series();
        a.event.description = Some("<b>Literal</b>".into());
        let mut b = a.clone();
        b.native = Some(super::super::event_set::NativeCalendarResource { protocol:"caldav".into(),
            calendar_id:"calendar".into(), event_id:"event".into(), revision:Some("v1".into()),
            data:"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:source-uid\r\nDESCRIPTION:<b>Literal</b>\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".into() });
        assert!(semantic_eq(&a, &b));
        graph_body(&mut a, "html", "<b>Literal</b>");
        assert!(!semantic_eq(&a, &b));
        let data = &mut b.native.as_mut().unwrap().data;
        *data = data.replace(
            "END:VEVENT",
            "X-ALT-DESC;FMTTYPE=text/html:<b>Rich</b>\r\nEND:VEVENT",
        );
        assert!(!semantic_eq(&b, &b));
    }

    #[test]
    fn semantic_private_provenance_survives_selection_local_transfer_and_journal() {
        let mut source = series();
        graph_body(&mut source, "html", "<b>Rich agenda</b>");
        attendee(
            &mut source,
            serde_json::json!([{"email":"guest@example.test", "status":"needs-action"}]),
        );
        native(
            &mut source,
            "graph",
            serde_json::json!({"body":{"contentType":"html","content":"<b>Rich agenda</b>"},
            "attendees":[{"emailAddress":{"address":"guest@example.test"},"type":"optional","status":{"response":"notResponded"}}]}),
        );
        let mut selected = source.standalone_at("2026-03-08T13:00:00Z").unwrap();
        assert_eq!(
            selected.description_content_type(None).unwrap(),
            "text/html"
        );
        let before = selected.clone();
        selected.native = None;
        selected.event.ical_data = None;
        assert!(semantic_eq(&before, &selected));
        let journal = serde_json::to_string(&selected).unwrap();
        let restored: CalendarEventSet = serde_json::from_str(&journal).unwrap();
        assert!(semantic_eq(&before, &restored));
        let mut lost = restored.clone();
        lost.content = None;
        assert!(!semantic_eq(&before, &lost));
        let page = event_set_view(
            &restored,
            &restored.event,
            "token",
            "2026-03-01",
            "2026-04-01",
            5,
        )
        .unwrap();
        let dto = serde_json::to_string(&page).unwrap();
        assert!(!dto.contains("\"content\""));
        assert!(!dto.contains("\"roles\""));
        assert!(!dto.contains("guest@example.test"));
    }

    #[test]
    fn semantic_description_edit_has_explicit_plain_intent_but_copy_keeps_rich() {
        let mut source = series();
        graph_body(&mut source, "html", "<b>Old agenda</b>");
        let key = "2026-03-08T13:00:00Z";
        let copied = desired_set(
            &source,
            &input(
                key,
                RecurrenceMutationScope::ThisOccurrence,
                CalendarEdit {
                    title: Some("New title".into()),
                    ..Default::default()
                },
            ),
        )
        .unwrap();
        assert_eq!(
            copied.description_content_type(Some(key)).unwrap(),
            "text/html"
        );
        let edited = desired_set(
            &source,
            &input(
                key,
                RecurrenceMutationScope::ThisOccurrence,
                CalendarEdit {
                    description: Some("<b>Literal UI text</b>".into()),
                    ..Default::default()
                },
            ),
        )
        .unwrap();
        assert_eq!(edited.description_content_type(None).unwrap(), "text/html");
        assert_eq!(
            edited.description_content_type(Some(key)).unwrap(),
            "text/plain"
        );
        let mut canonical = edited.clone();
        canonical.content = None;
        canonical.overrides[0].native = Some(super::super::event_set::NativeCalendarResource {
            protocol:"graph".into(), calendar_id:"calendar".into(), event_id:"override".into(), revision:Some("v2".into()),
            data:serde_json::json!({"body":{"contentType":"text","content":"<b>Literal UI text</b>"},"attendees":[]}).to_string() });
        assert!(semantic_eq(&edited, &canonical));
        canonical.overrides[0].native.as_mut().unwrap().data = serde_json::json!({
            "body":{"contentType":"html","content":"<b>Literal UI text</b>"},"attendees":[]})
        .to_string();
        assert!(!semantic_eq(&edited, &canonical));
    }

    #[test]
    fn semantic_remapped_embedded_rich_override_retains_provenance() {
        let mut source = series();
        let mut changed = selected_event(&source, Some("2026-03-08T13:00:00Z")).unwrap();
        changed.description = Some("<b>Unique</b>".into());
        source.overrides.push(CalendarOverride {
            original_start: "2026-03-08T13:00:00Z".into(),
            event: Some(changed),
            native: None,
        });
        native(
            &mut source,
            "jmap",
            serde_json::json!({"event":{"description":"Agenda","timeZone":"America/New_York",
            "recurrenceOverrides":{"2026-03-08T09:00:00":{"description":"<b>Unique</b>","descriptionContentType":"text/html"}}}}),
        );
        let desired = desired_set(
            &source,
            &input(
                "2026-03-07T14:00:00Z",
                RecurrenceMutationScope::EntireSeries,
                CalendarEdit {
                    recurrence_rule: Some("FREQ=WEEKLY;COUNT=4".into()),
                    ..Default::default()
                },
            ),
        )
        .unwrap();
        assert_eq!(desired.overrides[0].original_start, "2026-03-14T13:00:00Z");
        assert_eq!(
            desired
                .description_content_type(Some("2026-03-14T13:00:00Z"))
                .unwrap(),
            "text/html"
        );
        let mut canonical = desired.clone();
        canonical.content = None;
        native(
            &mut canonical,
            "jmap",
            serde_json::json!({"event":{"description":"Agenda","timeZone":"America/New_York",
            "recurrenceOverrides":{"2026-03-14T09:00:00":{"description":"<b>Unique</b>","descriptionContentType":"text/html"}}}}),
        );
        assert!(semantic_eq(&desired, &canonical));
        native(
            &mut canonical,
            "jmap",
            serde_json::json!({"event":{"description":"Agenda","timeZone":"America/New_York",
            "recurrenceOverrides":{"2026-03-14T09:00:00":{"description":"<b>Unique</b>"}}}}),
        );
        assert!(!semantic_eq(&desired, &canonical));
    }

    #[test]
    fn semantic_identical_markup_with_different_format_is_a_unique_override() {
        let mut source = series();
        graph_body(&mut source, "html", "<b>Agenda</b>");
        let mut modified = source.clone();
        modified.overrides.push(CalendarOverride {
            original_start: "2026-03-08T13:00:00Z".into(),
            event: Some(selected_event(&source, Some("2026-03-08T13:00:00Z")).unwrap()),
            native: Some(super::super::event_set::NativeCalendarResource {
                protocol: "graph".into(),
                calendar_id: "calendar".into(),
                event_id: "override".into(),
                revision: Some("v1".into()),
                data: serde_json::json!({
                    "body":{"contentType":"text","content":"<b>Agenda</b>"},"attendees":[]})
                .to_string(),
            }),
        });
        assert!(!semantic_eq(&source, &modified));
        let desired = desired_set(
            &modified,
            &input(
                "2026-03-07T14:00:00Z",
                RecurrenceMutationScope::EntireSeries,
                CalendarEdit {
                    description: Some("Updated master".into()),
                    ..Default::default()
                },
            ),
        )
        .unwrap();
        assert_eq!(
            desired.overrides[0]
                .event
                .as_ref()
                .unwrap()
                .description
                .as_deref(),
            Some("<b>Agenda</b>")
        );
    }

    #[test]
    fn semantic_new_organizer_does_not_remove_previous_owner_from_people() {
        let mut source = series();
        source.event.organizer_email = Some("previous@example.test".into());
        attendee(
            &mut source,
            serde_json::json!([
            {"email":"previous@example.test","status":"accepted"},
            {"email":"guest@example.test","status":"tentative"}]),
        );
        source.prepare_new_meeting("new@example.test").unwrap();
        let attendees: serde_json::Value =
            serde_json::from_str(source.event.attendees_json.as_ref().unwrap()).unwrap();
        assert_eq!(
            source.event.organizer_email.as_deref(),
            Some("new@example.test")
        );
        let values = attendees.as_array().unwrap();
        assert_eq!(values.len(), 2);
        assert!(values
            .iter()
            .any(|value| value["email"] == "previous@example.test"));
        assert!(values.iter().all(|value| value["status"] == "needs-action"));
    }

    #[test]
    fn semantic_rejects_native_attendee_projection_loss_and_html_whitespace_loss() {
        let mut a = series();
        native(
            &mut a,
            "google",
            serde_json::json!({"description":"Agenda",
            "attendees":[{"email":"lost@example.test","responseStatus":"accepted"}]}),
        );
        assert!(!semantic_eq(&a, &a));
        a = series();
        a.event.description = Some("A\nB".into());
        let mut b = a.clone();
        graph_body(&mut b, "html", "A&#10;B");
        assert!(!semantic_eq(&a, &b));
        a.event.description = Some("A  B".into());
        graph_body(&mut b, "html", "A  B");
        assert!(!semantic_eq(&a, &b));
    }

    #[test]
    fn semantic_native_full_body_organizer_and_responses_bind_fresh_projections() {
        let a = series();
        let mut b = a.clone();
        native(
            &mut b,
            "graph",
            serde_json::json!({
            "body":{"contentType":"html","content":"<b>Agenda</b>"},"attendees":[]}),
        );
        assert!(!semantic_eq(&a, &b));
        assert!(b.capture_content().is_err());
        native(
            &mut b,
            "google",
            serde_json::json!({"description":"Agenda",
            "organizer":{"email":"hidden@example.test"}}),
        );
        assert!(!semantic_eq(&a, &b));
        let mut a = a;
        attendee(&mut a, serde_json::json!([{"email":"guest@example.test"}]));
        b = a.clone();
        native(
            &mut b,
            "google",
            serde_json::json!({"description":"Agenda",
            "attendees":[{"email":"guest@example.test","responseStatus":"accepted"}]}),
        );
        assert!(!semantic_eq(&a, &b));
    }

    pub(crate) fn series() -> CalendarEventSet {
        let event = CalendarEvent {
            id: "event".into(),
            account_id: "account".into(),
            calendar_id: "calendar".into(),
            uid: Some("source-uid".into()),
            title: "Planning".into(),
            description: Some("Agenda".into()),
            location: None,
            start_time: "2026-03-07T14:00:00Z".into(),
            end_time: "2026-03-07T15:00:00Z".into(),
            all_day: false,
            timezone: Some("America/New_York".into()),
            recurrence_kind: RecurrenceKind::Series,
            recurrence_rule: Some("FREQ=DAILY;COUNT=4".into()),
            organizer_email: None,
            attendees_json: None,
            my_status: None,
            source_message_id: None,
            ical_data: None,
            remote_id: None,
            etag: None,
        };
        CalendarEventSet {
            event,
            overrides: vec![],
            native: None,
            content: None,
        }
    }

    fn input(key: &str, scope: RecurrenceMutationScope, edit: CalendarEdit) -> CalendarActionInput {
        CalendarActionInput {
            selection: CalendarSelection {
                event_id: "event".into(),
                token: "opaque".into(),
                original_start: Some(key.into()),
            },
            scope,
            edit,
            destination_calendar_id: None,
            reset_exceptions: false,
        }
    }

    #[test]
    fn one_occurrence_keeps_original_slot_and_other_positions() {
        let before = series();
        let desired = desired_set(
            &before,
            &input(
                "2026-03-08T13:00:00Z",
                RecurrenceMutationScope::ThisOccurrence,
                CalendarEdit {
                    start_time: Some("2026-03-12T13:00:00Z".into()),
                    ..Default::default()
                },
            ),
        )
        .unwrap();
        assert_eq!(desired.event, before.event);
        assert_eq!(desired.overrides[0].original_start, "2026-03-08T13:00:00Z");
        assert_eq!(
            desired.overrides[0].event.as_ref().unwrap().end_time,
            "2026-03-12T14:00:00Z"
        );
        let page = project(
            &desired,
            &before.event,
            "opaque",
            "2026-03-12",
            "2026-03-13",
            10,
        )
        .unwrap();
        assert_eq!(page.occurrences.len(), 1);
        assert_eq!(
            page.occurrences[0].selection.original_start.as_deref(),
            Some("2026-03-08T13:00:00Z")
        );
        assert!(page.occurrences[0].is_exception);
    }

    #[test]
    fn drag_later_phase_preserves_count_duration_and_dst_wall_clock() {
        let before = series();
        let desired = desired_set(
            &before,
            &input(
                "2026-03-09T13:00:00Z",
                RecurrenceMutationScope::EntireSeries,
                CalendarEdit {
                    start_time: Some("2026-03-10T15:00:00Z".into()),
                    ..Default::default()
                },
            ),
        )
        .unwrap();
        assert_eq!(desired.event.start_time, "2026-03-08T15:00:00Z");
        assert_eq!(desired.event.end_time, "2026-03-08T16:00:00Z");
        assert_eq!(desired.event.recurrence_rule, before.event.recurrence_rule);
        assert_eq!(
            recurrence::position_at(&desired.event, 2).unwrap(),
            "2026-03-10T15:00:00Z"
        );
        assert!(recurrence::position_at(&desired.event, 4).is_err());
    }

    #[test]
    fn all_day_drag_preserves_days_without_timezone_conversion() {
        let mut before = series();
        before.event.all_day = true;
        before.event.start_time = "2026-03-07".into();
        before.event.end_time = "2026-03-10".into();
        let desired = desired_set(
            &before,
            &input(
                "2026-03-09",
                RecurrenceMutationScope::EntireSeries,
                CalendarEdit {
                    start_time: Some("2026-03-12".into()),
                    ..Default::default()
                },
            ),
        )
        .unwrap();
        assert_eq!(desired.event.start_time, "2026-03-10");
        assert_eq!(desired.event.end_time, "2026-03-13");
        assert_eq!(
            recurrence::position_at(&desired.event, 2).unwrap(),
            "2026-03-12"
        );
    }

    #[test]
    fn changed_pattern_maps_cancelled_and_modified_exceptions_by_phase() {
        let mut before = series();
        let mut exception = selected_event(&before, Some("2026-03-08T13:00:00Z")).unwrap();
        exception.start_time = "2026-03-08T16:00:00Z".into();
        exception.end_time = "2026-03-08T18:00:00Z".into();
        exception.title = "Special".into();
        before.overrides = vec![
            CalendarOverride {
                original_start: "2026-03-08T13:00:00Z".into(),
                event: Some(exception),
                native: None,
            },
            CalendarOverride {
                original_start: "2026-03-09T13:00:00Z".into(),
                event: None,
                native: None,
            },
        ];
        let desired = desired_set(
            &before,
            &input(
                "2026-03-07T14:00:00Z",
                RecurrenceMutationScope::EntireSeries,
                CalendarEdit {
                    recurrence_rule: Some("FREQ=WEEKLY;COUNT=4".into()),
                    title: Some("Updated".into()),
                    ..Default::default()
                },
            ),
        )
        .unwrap();
        assert_eq!(desired.overrides[0].original_start, "2026-03-14T13:00:00Z");
        let exception = desired.overrides[0].event.as_ref().unwrap();
        assert_eq!(exception.start_time, "2026-03-14T16:00:00Z");
        assert_eq!(exception.end_time, "2026-03-14T18:00:00Z");
        assert_eq!(exception.title, "Special");
        assert_eq!(desired.overrides[1].original_start, "2026-03-21T13:00:00Z");
        assert!(desired.overrides[1].event.is_none());
    }

    #[test]
    fn shortened_pattern_requires_explicit_reset_and_scope_isolation() {
        let mut before = series();
        before.overrides.push(CalendarOverride {
            original_start: "2026-03-10T13:00:00Z".into(),
            event: None,
            native: None,
        });
        let mut edit = input(
            "2026-03-07T14:00:00Z",
            RecurrenceMutationScope::EntireSeries,
            CalendarEdit {
                recurrence_rule: Some("FREQ=DAILY;COUNT=2".into()),
                ..Default::default()
            },
        );
        assert!(desired_set(&before, &edit).is_err());
        edit.reset_exceptions = true;
        assert!(desired_set(&before, &edit).unwrap().overrides.is_empty());
        edit.scope = RecurrenceMutationScope::ThisOccurrence;
        assert!(desired_set(&before, &edit).is_err());
    }

    #[test]
    fn projection_is_bounded_and_does_not_serialize_provider_payloads() {
        let mut set = series();
        set.event.ical_data = Some("SECRET-ICAL".into());
        set.native = Some(crate::calendar::event_set::NativeCalendarResource {
            protocol: "caldav".into(),
            calendar_id: "SECRET-CALENDAR".into(),
            event_id: "SECRET-TARGET".into(),
            revision: Some("SECRET-ETAG".into()),
            data: "SECRET-NATIVE".into(),
        });
        let page = project(&set, &set.event, "opaque", "2026-03-01", "2026-04-01", 2).unwrap();
        assert_eq!(page.occurrences.len(), 2);
        assert!(page.has_more);
        let view =
            event_set_view(&set, &set.event, "opaque", "2026-03-01", "2026-04-01", 2).unwrap();
        assert!(view.master.selection.original_start.is_none());
        let serialized = serde_json::to_string(&view).unwrap();
        assert!(!serialized.contains("SECRET"));
        assert!(!serialized.contains("native"));
        assert!(project(&set, &set.event, "opaque", "2026-01-01", "2028-01-01", 2).is_err());
        assert!(selected_event(&set, Some("2026-03-08T14:00:00Z")).is_err());
    }
}
