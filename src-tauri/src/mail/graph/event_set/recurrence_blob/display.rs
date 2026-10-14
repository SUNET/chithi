//! Display-only Graph/blob proofs. MonthNth encodes a weekday mask and one
//! combined index (1..=4, or 5 for last), not one ordinal per weekday.

use super::*;

pub(super) fn validate_fields(fields: &BTreeMap<&str, &str>) -> Result<()> {
    let permitted: &[&str] = match fields.get("FREQ").copied() {
        Some("DAILY") => &[],
        Some("WEEKLY") => &["BYDAY", "WKST"],
        Some("MONTHLY") => &["BYMONTHDAY", "BYDAY", "BYSETPOS"],
        Some("YEARLY") => &["BYMONTH", "BYMONTHDAY", "BYDAY", "BYSETPOS"],
        _ => return Err(invalid("unrepresentable display blob frequency")),
    };
    for key in fields.keys() {
        if !["FREQ", "INTERVAL", "COUNT", "UNTIL"].contains(key) && !permitted.contains(key) {
            return Err(invalid("unrepresentable display blob selector"));
        }
    }
    let number = |key, maximum| -> Result<()> {
        if let Some(value) = fields.get(key) {
            value
                .parse::<u32>()
                .ok()
                .filter(|value| (1..=maximum).contains(value))
                .ok_or_else(|| invalid("invalid display blob selector"))?;
        }
        Ok(())
    };
    number("BYMONTH", 12)?;
    number("BYMONTHDAY", 31)?;
    if matches!(fields.get("FREQ"), Some(&"MONTHLY" | &"YEARLY")) {
        if fields.contains_key("BYDAY") != fields.contains_key("BYSETPOS")
            || (fields.contains_key("BYSETPOS") && fields.contains_key("BYMONTHDAY"))
        {
            return Err(invalid("unrepresentable display blob month selectors"));
        }
        if fields
            .get("BYSETPOS")
            .is_some_and(|index| !["1", "2", "3", "4", "-1"].contains(index))
        {
            return Err(invalid("unrepresentable display blob weekday index"));
        }
    }
    Ok(())
}

pub(super) struct BlobRange {
    pub first: u32,
    pub period: u32,
    pub first_dow: u32,
    pub start_date: u32,
    pub end_date: u32,
    pub end_type: u32,
    pub count: u32,
}

fn local_date(master: &CalendarEvent, position: &str) -> Result<NaiveDate> {
    if master.all_day {
        NaiveDate::parse_from_str(position, "%Y-%m-%d").map_err(invalid)
    } else {
        Ok(instant(position)?
            .with_timezone(&zone(master.timezone.as_deref().unwrap_or("UTC"))?)
            .date_naive())
    }
}

pub(super) fn verify_positions(
    master: &CalendarEvent,
    overrides: &[CalendarOverride],
    deleted: &[String],
    fields: &BTreeMap<&str, &str>,
    range: BlobRange,
    graph_range_end: Option<NaiveDate>,
) -> Result<()> {
    let start = minute_time(range.start_date)?.date();
    let epoch = NaiveDate::from_ymd_opt(1601, 1, 1).expect("valid recurrence epoch");
    let expected_first = match fields.get("FREQ").copied() {
        Some("DAILY") => i64::from(range.start_date) % i64::from(range.period),
        Some("WEEKLY") => {
            let offset = (start.weekday().num_days_from_sunday() + 7 - range.first_dow) % 7;
            (i64::from(range.start_date) - i64::from(offset) * 1440)
                .rem_euclid(i64::from(range.period) * 10080)
        }
        _ => {
            let months = (start.year() - 1601) * 12 + start.month0() as i32;
            let offset = u32::try_from(months).map_err(invalid)? % range.period;
            epoch
                .checked_add_months(chrono::Months::new(offset))
                .ok_or_else(|| invalid("blob month phase overflow"))?
                .signed_duration_since(epoch)
                .num_minutes()
        }
    };
    if i64::from(range.first) != expected_first {
        return Err(invalid("blob interval phase differs from Graph"));
    }
    if fields
        .get("BYMONTH")
        .is_some_and(|month| month.parse::<u32>().ok() != Some(start.month()))
    {
        return Err(invalid("blob yearly month differs from Graph"));
    }
    let mut required = deleted.to_vec();
    required.extend(overrides.iter().map(|item| item.original_start.clone()));
    required.push(master.start_time.clone());
    // Bound a complete proof by Graph's cutoff, or by the claimed final blob
    // date for numbered series. An unbounded series only needs membership of
    // the finite originals; no provider occurrence IDs are interpreted.
    let upper = if let Some(until) = fields.get("UNTIL") {
        NaiveDateTime::parse_from_str(until, "%Y%m%dT%H%M%SZ")
            .map_err(invalid)?
            .and_utc()
            .checked_add_signed(Duration::nanoseconds(1))
            .ok_or_else(|| invalid("blob cutoff overflow"))?
    } else {
        let mut last = if fields.contains_key("COUNT") {
            if !range.end_date.is_multiple_of(1440) {
                return Err(invalid("non-midnight blob end date"));
            }
            minute_time(range.end_date)?.date()
        } else {
            start
        };
        for position in &required {
            last = last.max(local_date(master, position)?);
        }
        // All known UTC offsets are less than a day. An envelope avoids
        // inventing a resolution for a nonexistent midnight boundary.
        last.and_hms_opt(0, 0, 0)
            .ok_or_else(|| invalid("invalid blob end date"))?
            .and_utc()
            .checked_add_signed(Duration::days(2))
            .ok_or_else(|| invalid("blob end date overflow"))?
    };
    let summary = crate::calendar::display_recurrence::verify_original_positions(
        master,
        &utc(upper),
        &required,
    )
    .map_err(invalid)?;
    if summary.first.as_deref() != Some(master.start_time.as_str()) {
        return Err(invalid("blob first occurrence differs from Graph"));
    }
    if matches!(range.end_type, 0x2021 | 0x2022) {
        if summary.count != range.count || summary.count == 0 {
            return Err(invalid(format!(
                "blob occurrence count differs from Graph range (blob={}, computed={})",
                range.count, summary.count
            )));
        }
        let last = summary
            .last
            .as_deref()
            .ok_or_else(|| invalid("empty bounded blob recurrence"))?;
        let claimed = minute_time(range.end_date)?.date();
        let computed = local_date(master, last)?;
        let boundary_matches = if let Some(end) = graph_range_end {
            let cutoff = fields
                .get("UNTIL")
                .filter(|_| range.end_type == 0x2021)
                .ok_or_else(|| invalid("Graph range boundary requires a date-bounded blob"))?;
            let cutoff = NaiveDateTime::parse_from_str(cutoff, "%Y%m%dT%H%M%SZ")
                .map_err(invalid)?
                .and_utc();
            let local = if master.all_day {
                cutoff.naive_utc()
            } else {
                cutoff
                    .with_timezone(&zone(master.timezone.as_deref().unwrap_or("UTC"))?)
                    .naive_local()
            };
            if local.date() != end
                || local.time()
                    != chrono::NaiveTime::from_hms_opt(23, 59, 59).expect("valid day end")
            {
                return Err(invalid(
                    "Graph endDate differs from the projected inclusive cutoff",
                ));
            }
            // Imported Exchange blobs can store the inclusive range boundary
            // even when no occurrence starts that day. The complete count and
            // every required original have already been verified above; accept
            // only that exact provider boundary, never an arbitrary later date.
            claimed == end && computed <= end
        } else {
            false
        };
        if !range.end_date.is_multiple_of(1440) || (claimed != computed && !boundary_matches) {
            return Err(invalid(format!("blob last occurrence differs from Graph range (blob={claimed}, computed={computed}, cutoff={})", fields.get("UNTIL").copied().unwrap_or("COUNT"))));
        }
    }
    Ok(())
}
