//! Self-contained VTIMEZONE definitions from the same bundled IANA database used
//! by the recurrence engine. Explicit transitions cover its complete 0001..9999
//! date domain; this avoids guessing a perpetual RRULE from recent DST patterns.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, NaiveDate, Offset, TimeZone, Utc};
use chrono_tz::{OffsetComponents, Tz};

use crate::error::{Error, Result};

fn invalid() -> Error {
    Error::Other("CalDAV could not construct the IANA timezone definition".into())
}

fn offset_at(zone: Tz, timestamp: i64) -> Result<(i32, bool)> {
    let instant = DateTime::from_timestamp(timestamp, 0).ok_or_else(invalid)?;
    let offset = zone.offset_from_utc_datetime(&instant.naive_utc());
    Ok((
        offset.fix().local_minus_utc(),
        offset.dst_offset().num_seconds() != 0,
    ))
}

fn offset_text(seconds: i32) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let seconds = seconds.unsigned_abs();
    let mut value = format!("{sign}{:02}{:02}", seconds / 3600, seconds % 3600 / 60);
    if !seconds.is_multiple_of(60) {
        value.push_str(&format!("{:02}", seconds % 60));
    }
    value
}

fn observance(output: &mut String, local: &str, from: i32, to: i32, daylight: bool) {
    let kind = if daylight { "DAYLIGHT" } else { "STANDARD" };
    output.push_str(&format!(
        "BEGIN:{kind}\r\nDTSTART:{local}\r\nTZOFFSETFROM:{}\r\nTZOFFSETTO:{}\r\nEND:{kind}\r\n",
        offset_text(from),
        offset_text(to),
    ));
}

pub(super) fn definition(tzid: &str) -> Result<String> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().map_err(|_| invalid())?;
    if let Some(value) = cache.get(tzid).cloned() {
        return Ok(value);
    }
    let zone: Tz = tzid.parse().map_err(|_| invalid())?;
    let midnight = |year| -> Result<i64> {
        Ok(NaiveDate::from_ymd_opt(year, 1, 1)
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .ok_or_else(invalid)?
            .and_utc()
            .timestamp())
    };
    let mut cursor = midnight(1)?;
    let end = midnight(10000)?;
    let (mut previous, _) = offset_at(zone, cursor)?;
    let mut output = format!("BEGIN:VTIMEZONE\r\nTZID:{tzid}\r\n");
    observance(&mut output, "00010101T000000", previous, previous, false);
    // IANA offset transitions are separated by more than a day. Daily probes
    // identify each change; binary search retains its exact second, including
    // historical local-mean-time offsets. No host timezone files are consulted.
    while cursor < end {
        let next = (cursor + 86400).min(end);
        let (offset, daylight) = offset_at(zone, next)?;
        if offset != previous {
            let (mut low, mut high) = (cursor, next);
            while high - low > 1 {
                let middle = low + (high - low) / 2;
                if offset_at(zone, middle)?.0 == previous {
                    low = middle;
                } else {
                    high = middle;
                }
            }
            let local = DateTime::<Utc>::from_timestamp(high + i64::from(previous), 0)
                .ok_or_else(invalid)?
                .format("%Y%m%dT%H%M%S")
                .to_string();
            observance(&mut output, &local, previous, offset, daylight);
            previous = offset;
        }
        cursor = next;
    }
    output.push_str("END:VTIMEZONE\r\n");
    cache.insert(tzid.to_owned(), output.clone());
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stockholm_contains_exact_spring_and_autumn_transitions() {
        let data = definition("Europe/Stockholm").unwrap();
        assert!(data.contains("DTSTART:20260329T020000\r\nTZOFFSETFROM:+0100\r\nTZOFFSETTO:+0200"));
        assert!(data.contains("DTSTART:20261025T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100"));
        assert!(!data.contains("RRULE"));
        assert_eq!(definition("Europe/Stockholm").unwrap(), data);
    }
}
