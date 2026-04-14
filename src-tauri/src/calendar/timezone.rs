/// Timezone conversion utilities for calendar events.
///
/// All calendar events are stored with UTC times. This module handles
/// converting provider-specific datetime+timezone pairs to UTC.

/// Convert a datetime string to UTC given an IANA timezone identifier.
///
/// Handles these input formats:
/// - Already UTC: `"2026-04-14T12:00:00Z"` → returned as-is
/// - With offset: `"2026-04-14T14:00:00+02:00"` → converted to UTC
/// - Naive + tzid: `"2026-04-14T14:00:00"` + `"Europe/Stockholm"` → converted to UTC
/// - Naive, no tzid: `"2026-04-14T14:00:00"` + `""` → treated as UTC
/// - All-day date: `"2026-04-14"` → returned as-is (no conversion)
pub fn to_utc(datetime: &str, tzid: &str) -> String {
    let dt = datetime.trim();

    // All-day dates (YYYY-MM-DD, no T) pass through unchanged
    if !dt.contains('T') {
        return dt.to_string();
    }

    // Already has Z suffix → parse, normalize, return UTC
    if dt.ends_with('Z') {
        if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(dt) {
            return parsed
                .with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
        }
        // Try without fractional seconds: strip trailing Z, parse naive, re-add Z
        let bare = dt.trim_end_matches('Z');
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(bare, "%Y-%m-%dT%H:%M:%S") {
            return ndt.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        }
        return dt.to_string();
    }

    // Has explicit offset (contains + or - after the T) → parse as fixed-offset
    if dt.contains('+') || dt.rfind('-').map(|i| i > 10).unwrap_or(false) {
        if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(dt) {
            return parsed
                .with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
        }
    }

    // Naive datetime + IANA timezone → convert via chrono-tz
    if !tzid.is_empty() {
        if let Ok(tz) = tzid.parse::<chrono_tz::Tz>() {
            if let Ok(naive) =
                chrono::NaiveDateTime::parse_from_str(dt, "%Y-%m-%dT%H:%M:%S")
            {
                use chrono::TimeZone;
                if let chrono::LocalResult::Single(local) = tz.from_local_datetime(&naive) {
                    return local
                        .with_timezone(&chrono::Utc)
                        .format("%Y-%m-%dT%H:%M:%SZ")
                        .to_string();
                }
                // Ambiguous (DST fall-back) — pick the earlier one
                if let chrono::LocalResult::Ambiguous(early, _late) =
                    tz.from_local_datetime(&naive)
                {
                    return early
                        .with_timezone(&chrono::Utc)
                        .format("%Y-%m-%dT%H:%M:%SZ")
                        .to_string();
                }
                // None (DST gap) — the time doesn't exist; push forward
                log::warn!(
                    "to_utc: datetime {} in {} falls in a DST gap, treating as UTC",
                    dt,
                    tzid
                );
            }
        } else {
            log::warn!("to_utc: unrecognized timezone '{}', treating as UTC", tzid);
        }
    }

    // Fallback: treat as UTC
    format!("{}Z", dt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_already_utc() {
        assert_eq!(to_utc("2026-04-14T12:00:00Z", ""), "2026-04-14T12:00:00Z");
    }

    #[test]
    fn test_already_utc_ignores_tzid() {
        assert_eq!(to_utc("2026-04-14T12:00:00Z", "America/New_York"), "2026-04-14T12:00:00Z");
    }

    #[test]
    fn test_with_positive_offset() {
        assert_eq!(to_utc("2026-04-14T14:00:00+02:00", ""), "2026-04-14T12:00:00Z");
    }

    #[test]
    fn test_with_negative_offset() {
        assert_eq!(to_utc("2026-04-14T08:00:00-04:00", ""), "2026-04-14T12:00:00Z");
    }

    #[test]
    fn test_naive_with_tzid_stockholm() {
        // Stockholm is UTC+2 in April (CEST)
        assert_eq!(to_utc("2026-04-14T14:00:00", "Europe/Stockholm"), "2026-04-14T12:00:00Z");
    }

    #[test]
    fn test_offset_datetime_tzid_ignored() {
        // Offset already present → parsed directly, tzid ignored
        assert_eq!(to_utc("2026-04-14T08:00:00-04:00", "America/New_York"), "2026-04-14T12:00:00Z");
    }

    #[test]
    fn test_naive_no_tzid_treated_as_utc() {
        assert_eq!(to_utc("2026-04-14T14:00:00", ""), "2026-04-14T14:00:00Z");
    }

    #[test]
    fn test_invalid_tzid_treated_as_utc() {
        assert_eq!(to_utc("2026-04-14T14:00:00", "Not/A/Timezone"), "2026-04-14T14:00:00Z");
    }

    #[test]
    fn test_allday_date_unchanged() {
        assert_eq!(to_utc("2026-04-14", "Europe/Stockholm"), "2026-04-14");
    }

    #[test]
    fn test_with_fractional_seconds() {
        assert_eq!(to_utc("2026-04-14T12:00:00.000Z", ""), "2026-04-14T12:00:00Z");
    }

    #[test]
    fn test_winter_time_stockholm() {
        // Stockholm is UTC+1 in January (CET)
        assert_eq!(to_utc("2026-01-14T13:00:00", "Europe/Stockholm"), "2026-01-14T12:00:00Z");
    }
}
