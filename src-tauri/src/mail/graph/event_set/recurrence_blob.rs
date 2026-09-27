//! Read-only MS-OXOCAL 2.2.1.44.1/.2/.4/.5 codec for PidLidAppointmentRecur.
//! https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-oxocal/cf7153b4-f8b5-4cb6-bf14-e78d21f94814
//! DeletedInstanceDates includes modified originals; ModifiedInstanceDates holds
//! effective dates. Subtract ExceptionInfo originals, not modified dates.

use super::*;
use base64::Engine;
use std::collections::BTreeSet;

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.0.len() {
            return Err(invalid("truncated recurrence blob"));
        }
        let (value, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(value)
    }
    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn zero(&mut self) -> Result<()> {
        if self.u32()? != 0 {
            return Err(invalid("nonzero reserved recurrence field"));
        }
        Ok(())
    }
    fn dates(&mut self) -> Result<Vec<u32>> {
        let count = self.u32()? as usize;
        if count > 65_535 || count > self.0.len() / 4 {
            return Err(invalid("invalid recurrence date count"));
        }
        let mut result = Vec::with_capacity(count);
        for _ in 0..count {
            let date = self.u32()?;
            if date % 1440 != 0 || result.last().is_some_and(|last| *last >= date) {
                return Err(invalid(
                    "unordered, duplicate or non-midnight recurrence dates",
                ));
            }
            result.push(date);
        }
        Ok(result)
    }
    fn ansi(&mut self) -> Result<()> {
        let length = self.u16()?;
        let bytes = self.u16()?;
        if u32::from(length) != u32::from(bytes) + 1 {
            return Err(invalid("invalid recurrence string length"));
        }
        self.take(bytes as usize)?;
        Ok(())
    }
    fn wide(&mut self) -> Result<()> {
        let length = self.u16()? as usize;
        let bytes = self.take(length * 2)?;
        let units = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect::<Vec<_>>();
        String::from_utf16(&units).map_err(invalid)?;
        Ok(())
    }
}

fn minute_time(minutes: u32) -> Result<NaiveDateTime> {
    NaiveDate::from_ymd_opt(1601, 1, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .and_then(|d| d.checked_add_signed(Duration::minutes(minutes.into())))
        .ok_or_else(|| invalid("recurrence date overflow"))
}

pub(super) fn deleted_positions(
    encoded: &str,
    master: &CalendarEvent,
    overrides: &[CalendarOverride],
) -> Result<Vec<String>> {
    if encoded.len() > 16 * 1024 * 1024 {
        return Err(invalid("recurrence blob exceeds size budget"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(invalid)?;
    let mut r = Reader(&bytes);
    if r.u16()? != 0x3004 || r.u16()? != 0x3004 {
        return Err(invalid("unknown recurrence blob version"));
    }
    let frequency = r.u16()?;
    let pattern = r.u16()?;
    if !matches!(r.u16()?, 0 | 1 | 2 | 9 | 10 | 11 | 12) {
        return Err(invalid("non-Gregorian recurrence blob"));
    }
    let first = r.u32()?;
    let period = r.u32()?;
    let recurrence = master
        .recurrence_rule
        .as_deref()
        .ok_or_else(|| invalid("blob without recurrence"))?;
    let normalized = simple_recurrence::normalize_rule(recurrence, master)?;
    let fields: BTreeMap<_, _> = normalized
        .split(';')
        .filter_map(|p| p.split_once('='))
        .collect();
    let interval = fields
        .get("INTERVAL")
        .copied()
        .unwrap_or("1")
        .parse::<u32>()
        .map_err(invalid)?;
    let (expected_frequency, expected_pattern, expected_period) = match fields.get("FREQ").copied()
    {
        Some("DAILY") => (0x200a, 0, interval * 1440),
        Some("WEEKLY") => (0x200b, 1, interval),
        Some("MONTHLY") => (0x200c, 2, interval),
        Some("YEARLY") => (0x200d, 2, interval * 12),
        _ => return Err(invalid("unrepresentable blob recurrence")),
    };
    if (frequency, pattern, period) != (expected_frequency, expected_pattern, expected_period)
        || first % 1440 != 0
    {
        return Err(invalid("blob recurrence differs from Graph pattern"));
    }
    r.zero()?;
    let (_, local_start) = super::super::recurring_graph_time(
        &master.start_time,
        master.all_day,
        master.timezone.as_deref(),
    )?;
    if pattern == 1 {
        let mask = r.u32()?;
        let days = fields
            .get("BYDAY")
            .map(|s| s.split(',').collect::<Vec<_>>())
            .unwrap_or_else(|| vec![DAYS[local_start.weekday().num_days_from_monday() as usize].0]);
        let expected = days.iter().try_fold(0_u32, |mask, day| {
            let index = DAYS
                .iter()
                .position(|(code, _)| code == day)
                .ok_or_else(|| invalid("invalid weekday"))?;
            Ok::<_, Error>(mask | (1 << ((index + 1) % 7)))
        })?;
        if mask != expected {
            return Err(invalid("blob weekdays differ from Graph"));
        }
    } else if pattern == 2 && r.u32()? != local_start.day() {
        return Err(invalid("blob day of month differs from Graph"));
    }
    let end_type = r.u32()?;
    let count = r.u32()?;
    let first_dow = r.u32()?;
    if first_dow > 6 {
        return Err(invalid("invalid blob first day of week"));
    }
    if frequency == 0x200b {
        let expected = fields.get("WKST").copied().unwrap_or("MO");
        if DAYS[((first_dow + 6) % 7) as usize].0 != expected {
            return Err(invalid("blob week phase differs from Graph"));
        }
    }
    let deleted = r.dates()?;
    let modified = r.dates()?;
    if modified.len() > deleted.len() {
        return Err(invalid("modified count exceeds deleted count"));
    }
    let start_date = r.u32()?;
    let end_date = r.u32()?;
    if start_date % 1440 != 0 || minute_time(start_date)?.date() != local_start.date() {
        return Err(invalid("blob DTSTART differs from Graph"));
    }
    match fields.get("COUNT") {
        Some(expected) if end_type == 0x2022 && count.to_string() == *expected => {}
        Some(_) => return Err(invalid("blob count differs from Graph")),
        None if fields.contains_key("UNTIL") => {
            if end_type != 0x2021 || end_date % 1440 != 0 {
                return Err(invalid("blob end type differs from Graph"));
            }
        }
        None if matches!(end_type, 0x2023 | 0xffffffff) && end_date == 0x5ae980df => {}
        None => return Err(invalid("blob range differs from Graph")),
    }
    if matches!(end_type, 0x2021 | 0x2022) {
        let last = simple_recurrence::position_at(
            master,
            count
                .checked_sub(1)
                .ok_or_else(|| invalid("empty bounded blob recurrence"))?,
        )?;
        let last_date = if master.all_day {
            NaiveDate::parse_from_str(&last, "%Y-%m-%d").map_err(invalid)?
        } else {
            instant(&last)?
                .with_timezone(&zone(master.timezone.as_deref().unwrap_or("UTC"))?)
                .date_naive()
        };
        if end_date % 1440 != 0 || minute_time(end_date)?.date() != last_date {
            return Err(invalid("blob last occurrence differs from Graph range"));
        }
    }
    if r.u32()? != 0x3006 {
        return Err(invalid("unknown appointment recurrence reader version"));
    }
    let writer = r.u32()?;
    if !matches!(writer, 0x3008 | 0x3009) {
        return Err(invalid("unknown appointment recurrence writer version"));
    }
    let start_offset = r.u32()?;
    let end_offset = r.u32()?;
    if start_offset >= 1440
        || start_offset != local_start.hour() * 60 + local_start.minute()
        || end_offset <= start_offset
    {
        return Err(invalid("blob time offsets differ from Graph"));
    }
    let exception_count = r.u16()? as usize;
    if exception_count != modified.len() || exception_count != overrides.len() {
        return Err(invalid(
            "blob exception count differs from complete Graph exceptions",
        ));
    }
    let mut exceptions = Vec::with_capacity(exception_count);
    let mut original_dates = BTreeSet::new();
    let mut effective_dates = BTreeSet::new();
    for _ in 0..exception_count {
        let start = r.u32()?;
        let end = r.u32()?;
        let original = r.u32()?;
        let flags = r.u16()?;
        if end <= start || flags & !0x03ff != 0 || !original_dates.insert(original / 1440 * 1440) {
            return Err(invalid("invalid recurrence exception"));
        }
        effective_dates.insert(start / 1440 * 1440);
        for bit in 0..9 {
            if flags & (1 << bit) != 0 {
                if bit == 0 || bit == 4 {
                    r.ansi()?;
                } else {
                    r.u32()?;
                }
            }
        }
        exceptions.push((start, end, original, flags));
    }
    r.zero()?;
    for &(start, end, original, flags) in &exceptions {
        if writer >= 0x3009 {
            let length = r.u32()? as usize;
            if length < 4 {
                return Err(invalid("invalid ChangeHighlight length"));
            }
            r.take(length)?;
        }
        r.zero()?;
        if flags & 0x11 != 0 {
            if (r.u32()?, r.u32()?, r.u32()?) != (start, end, original) {
                return Err(invalid("extended exception dates disagree"));
            }
            if flags & 1 != 0 {
                r.wide()?;
            }
            if flags & 0x10 != 0 {
                r.wide()?;
            }
            let reserved = r.u32()? as usize;
            r.take(reserved)?;
        }
    }
    r.zero()?;
    if !r.0.is_empty() {
        return Err(invalid("trailing recurrence blob data"));
    }
    if effective_dates != modified.into_iter().collect()
        || !original_dates.iter().all(|d| deleted.contains(d))
    {
        return Err(invalid("blob exception date arrays disagree"));
    }
    let tz = zone(master.timezone.as_deref().unwrap_or("UTC"))?;
    let graph_originals: BTreeSet<_> = overrides
        .iter()
        .map(|o| {
            if master.all_day {
                NaiveDate::parse_from_str(&o.original_start, "%Y-%m-%d").map_err(invalid)
            } else {
                Ok(instant(&o.original_start)?.with_timezone(&tz).date_naive())
            }
        })
        .collect::<Result<_>>()?;
    let blob_originals = original_dates
        .iter()
        .map(|d| Ok(minute_time(*d)?.date()))
        .collect::<Result<BTreeSet<_>>>()?;
    if graph_originals != blob_originals {
        return Err(invalid(
            "blob modified originals differ from Graph originalStart",
        ));
    }
    let mut positions = Vec::new();
    for date in deleted
        .into_iter()
        .filter(|date| !original_dates.contains(date))
    {
        let date = minute_time(date)?.date();
        let position = if master.all_day {
            date.to_string()
        } else {
            let local = date.and_time(local_start.time());
            let time = tz
                .from_local_datetime(&local)
                .earliest()
                .ok_or_else(|| invalid("deleted position falls in a timezone gap"))?;
            utc(time.with_timezone(&Utc))
        };
        simple_recurrence::resolve(master, &position)?;
        positions.push(position);
    }
    Ok(positions)
}
