use super::*;
use base64::Engine;
use tests::{master, server};

struct Fixture {
    value: Value,
    bytes: Vec<u8>,
    offsets: BTreeMap<&'static str, usize>,
}

fn day(value: &str) -> NaiveDate {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
}

fn minutes(value: &str) -> u32 {
    day(value)
        .signed_duration_since(day("1601-01-01"))
        .num_minutes() as u32
}

fn put32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend(value.to_le_bytes());
}

fn fixture(
    kind: &str,
    days: &[&str],
    mask: u32,
    index: &str,
    interval: u32,
    start: &str,
    originals: &[&str],
    deleted: &[&str],
    modified: &[(&str, &str)],
) -> Fixture {
    let mut value = master();
    value["start"] = json!({"dateTime":format!("{start}T09:00:00"), "timeZone":"UTC"});
    value["end"] = json!({"dateTime":format!("{start}T10:00:00"), "timeZone":"UTC"});
    value["originalStartTimeZone"] = json!("UTC");
    value["originalEndTimeZone"] = json!("UTC");
    value["recurrence"] = json!({
        "pattern":{"type":kind, "interval":interval, "daysOfWeek":days, "index":index, "month":9},
        "range":{"type":"numbered", "startDate":start, "numberOfOccurrences":originals.len(), "recurrenceTimeZone":"UTC"},
    });
    let mut bytes = Vec::new();
    let frequency = if kind == "relativeYearly" {
        0x200d_u16
    } else {
        0x200c_u16
    };
    for number in [0x3004, 0x3004, frequency, 3, 0] {
        bytes.extend(number.to_le_bytes());
    }
    let mut offsets = BTreeMap::new();
    let period = if kind == "relativeYearly" {
        interval * 12
    } else {
        interval
    };
    let first = day(originals[0]);
    let months = (first.year() - 1601) as u32 * 12 + first.month0();
    let phase = day("1601-01-01")
        .checked_add_months(chrono::Months::new(months % period))
        .unwrap();
    let epoch = day("1601-01-01");
    let nth = match index {
        "first" => 1,
        "second" => 2,
        "third" => 3,
        "fourth" => 4,
        "last" => 5,
        _ => panic!(),
    };
    for (key, number) in [
        (
            "phase",
            phase.signed_duration_since(epoch).num_minutes() as u32,
        ),
        ("period", period),
        ("sliding", 0),
        ("mask", mask),
        ("nth", nth),
        ("end_type", 0x2022),
        ("count", originals.len() as u32),
        ("first_dow", 0),
    ] {
        offsets.insert(key, bytes.len());
        put32(&mut bytes, number);
    }
    put32(&mut bytes, deleted.len() as u32);
    for date in deleted {
        offsets.entry("deleted").or_insert(bytes.len());
        put32(&mut bytes, minutes(date));
    }
    put32(&mut bytes, modified.len() as u32);
    for (_, effective) in modified {
        put32(&mut bytes, minutes(effective));
    }
    for (key, number) in [
        ("start_date", minutes(originals[0])),
        ("end_date", minutes(originals[originals.len() - 1])),
        ("reader", 0x3006),
        ("writer", 0x3009),
        ("start_offset", 540),
        ("end_offset", 600),
    ] {
        offsets.insert(key, bytes.len());
        put32(&mut bytes, number);
    }
    bytes.extend((modified.len() as u16).to_le_bytes());
    for (original, effective) in modified {
        put32(&mut bytes, minutes(effective) + 540);
        put32(&mut bytes, minutes(effective) + 600);
        put32(&mut bytes, minutes(original) + 540);
        bytes.extend(0_u16.to_le_bytes());
    }
    put32(&mut bytes, 0);
    for _ in modified {
        for number in [4, 0, 0] {
            put32(&mut bytes, number);
        }
    }
    put32(&mut bytes, 0);
    value["cancelledOccurrences"] = json!(["opaque-cancelled-instance"]);
    Fixture {
        value,
        bytes,
        offsets,
    }
}

impl Fixture {
    fn encode(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(&self.bytes)
    }
    fn source(&self) -> Value {
        let mut source = self.value.clone();
        source["singleValueExtendedProperties"] =
            json!([{"id":RECUR_PROPERTY, "value":self.encode()}]);
        source
    }
    fn change(&mut self, key: &str, value: u32) {
        let offset = self.offsets[key];
        self.bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn event(&self) -> CalendarEvent {
        display_event(&self.value, &crate::backend::testutil::event(), "selected").unwrap()
    }
}

fn monthly() -> Fixture {
    fixture(
        "relativeMonthly",
        &["thursday"],
        0x10,
        "second",
        1,
        "2026-09-14",
        &["2026-10-08", "2026-11-12", "2026-12-10"],
        &["2026-11-12"],
        &[],
    )
}

fn observed_range_case(relative_monthly: bool) -> Fixture {
    let (originals, deleted, end, start_time, end_time, start_offset, end_offset) =
        if relative_monthly {
            (
                vec!["2022-02-23", "2022-03-30", "2022-04-27"],
                "2022-03-30",
                "2022-05-24",
                "12:00:00",
                "13:00:00",
                780,
                840,
            )
        } else {
            (
                vec![
                    "2023-06-26",
                    "2023-07-03",
                    "2023-07-10",
                    "2023-07-17",
                    "2023-07-24",
                    "2023-07-31",
                    "2023-08-07",
                    "2023-08-14",
                    "2023-08-21",
                ],
                "2023-07-10",
                "2023-08-27",
                "09:15:00",
                "09:45:00",
                675,
                705,
            )
        };
    let start = originals[0];
    let mut case = fixture(
        "relativeMonthly",
        if relative_monthly {
            &["wednesday"]
        } else {
            &["monday"]
        },
        if relative_monthly { 0x08 } else { 0x02 },
        if relative_monthly { "last" } else { "first" },
        1,
        start,
        &originals,
        &[deleted],
        &[],
    );
    if !relative_monthly {
        // Weekly PatternTypeSpecific contains the mask but no MonthNth index.
        let nth = case.offsets.remove("nth").unwrap();
        case.bytes.drain(nth..nth + 4);
        for offset in case.offsets.values_mut() {
            if *offset > nth {
                *offset -= 4;
            }
        }
        case.bytes[4..6].copy_from_slice(&0x200b_u16.to_le_bytes());
        case.bytes[6..8].copy_from_slice(&1_u16.to_le_bytes());
        case.change("phase", minutes(start) % 10080);
        case.change("first_dow", 1);
        case.value["recurrence"]["pattern"] = json!({"type":"weekly", "interval":1, "daysOfWeek":["monday"], "firstDayOfWeek":"monday"});
    }
    case.value["start"] =
        json!({"dateTime":format!("{start}T{start_time}.0000000"), "timeZone":"UTC"});
    case.value["end"] = json!({"dateTime":format!("{start}T{end_time}.0000000"), "timeZone":"UTC"});
    case.value["originalStartTimeZone"] = json!("W. Europe Standard Time");
    case.value["originalEndTimeZone"] = json!("W. Europe Standard Time");
    case.value["recurrence"]["range"] = json!({"type":"endDate", "startDate":start, "endDate":end, "numberOfOccurrences":0, "recurrenceTimeZone":"W. Europe Standard Time"});
    case.change("end_type", 0x2021);
    case.change("end_date", minutes(end));
    case.change("start_offset", start_offset);
    case.change("end_offset", end_offset);
    case
}

pub(crate) fn observed_range_boundary_master(relative_monthly: bool) -> Value {
    observed_range_case(relative_monthly).source()
}

pub(crate) fn relative_master() -> Value {
    monthly().source()
}

#[test]
fn display_monthnth_supports_all_indexes_weekday_sets_and_yearly_month_phase() {
    for (kind, days, mask, index, interval, start, originals, deleted) in [
        (
            "relativeMonthly",
            vec!["thursday", "friday"],
            0x30,
            "first",
            1,
            "2026-09-14",
            vec!["2026-10-01", "2026-11-05", "2026-12-03"],
            "2026-11-05",
        ),
        (
            "relativeMonthly",
            vec!["thursday"],
            0x10,
            "second",
            1,
            "2026-09-14",
            vec!["2026-10-08", "2026-11-12", "2026-12-10"],
            "2026-11-12",
        ),
        (
            "relativeMonthly",
            vec!["monday", "friday"],
            0x22,
            "third",
            1,
            "2026-09-14",
            vec!["2026-10-09", "2026-11-09", "2026-12-11"],
            "2026-11-09",
        ),
        (
            "relativeMonthly",
            vec!["thursday"],
            0x10,
            "fourth",
            2,
            "2026-09-14",
            vec!["2026-09-24", "2026-11-26", "2027-01-28"],
            "2026-11-26",
        ),
        (
            "relativeMonthly",
            vec!["monday", "friday"],
            0x22,
            "last",
            1,
            "2026-09-14",
            vec!["2026-09-28", "2026-10-30", "2026-11-30"],
            "2026-10-30",
        ),
        (
            "relativeYearly",
            vec!["friday"],
            0x20,
            "last",
            1,
            "2026-09-14",
            vec!["2026-09-25", "2027-09-24", "2028-09-29"],
            "2027-09-24",
        ),
    ] {
        let case = fixture(
            kind,
            &days,
            mask,
            index,
            interval,
            start,
            &originals,
            &[deleted],
            &[],
        );
        let event = case.event();
        assert_eq!(
            recurrence_blob::deleted_display_positions(&case.encode(), &event, &[], None).unwrap(),
            [format!("{deleted}T09:00:00Z")],
            "{kind}/{index}"
        );
        assert!(canonical(&case.source(), &event, "selected").is_err());
        assert!(recurrence_blob::deleted_positions(&case.encode(), &event, &[]).is_err());
    }
}

#[test]
fn display_blob_cutoff_is_inclusive_and_must_agree_with_full_occurrence_count() {
    let mut case = monthly();
    case.value["recurrence"]["range"] = json!({"type":"endDate", "startDate":"2026-09-14", "endDate":"2026-12-31", "recurrenceTimeZone":"UTC"});
    case.change("end_type", 0x2021);
    assert_eq!(
        recurrence_blob::deleted_display_positions(&case.encode(), &case.event(), &[], None)
            .unwrap(),
        ["2026-11-12T09:00:00Z"]
    );
    case.value["recurrence"]["range"]["endDate"] = json!("2027-01-14");
    assert!(
        recurrence_blob::deleted_display_positions(&case.encode(), &case.event(), &[], None)
            .unwrap_err()
            .to_string()
            .contains("count")
    );
    case.value["recurrence"]["range"]["endDate"] = json!("2026-12-10");
    assert!(
        recurrence_blob::deleted_display_positions(&case.encode(), &case.event(), &[], None)
            .is_ok()
    );
    case.value["recurrence"]["range"]["endDate"] = json!("2026-12-09");
    assert!(
        recurrence_blob::deleted_display_positions(&case.encode(), &case.event(), &[], None)
            .is_err()
    );
}

#[test]
fn observed_date_bounded_blobs_prove_the_exact_graph_boundary_without_extending_the_series() {
    for relative_monthly in [false, true] {
        let case = observed_range_case(relative_monthly);
        let event = case.event();
        let end = day(case.value["recurrence"]["range"]["endDate"]
            .as_str()
            .unwrap());
        let expected = if relative_monthly {
            "2022-03-30T11:00:00Z"
        } else {
            "2023-07-10T09:15:00Z"
        };
        assert_eq!(
            recurrence_blob::deleted_display_positions(&case.encode(), &event, &[], Some(end))
                .unwrap(),
            [expected]
        );
        // Neither mutation decoding nor an unrelated caller without the exact
        // Graph range evidence acquires the imported representation's support.
        assert!(recurrence_blob::deleted_positions(&case.encode(), &event, &[]).is_err());
        assert!(
            recurrence_blob::deleted_display_positions(&case.encode(), &event, &[], None).is_err()
        );
        assert!(recurrence_blob::deleted_display_positions(
            &case.encode(),
            &event,
            &[],
            end.succ_opt()
        )
        .is_err());
        for (key, value) in [
            (
                "end_date",
                minutes(if relative_monthly {
                    "2022-04-28"
                } else {
                    "2023-08-22"
                }),
            ),
            ("count", if relative_monthly { 4 } else { 10 }),
            (
                "deleted",
                minutes(
                    case.value["recurrence"]["range"]["endDate"]
                        .as_str()
                        .unwrap(),
                ),
            ),
            ("phase", 1440),
        ] {
            let mut bad = observed_range_case(relative_monthly);
            bad.change(key, value);
            assert!(
                recurrence_blob::deleted_display_positions(&bad.encode(), &event, &[], Some(end))
                    .is_err(),
                "{relative_monthly}/{key}"
            );
        }
    }
    let case = monthly();
    assert!(recurrence_blob::deleted_display_positions(
        &case.encode(),
        &case.event(),
        &[],
        Some(day("2026-12-31"))
    )
    .is_err());
}

#[tokio::test]
async fn failed_display_read_retains_private_master_and_reports_claimed_and_computed_dates() {
    let mut case = monthly();
    case.value["recurrence"]["range"] = json!({"type":"endDate", "startDate":"2026-09-14", "endDate":"2026-12-31", "recurrenceTimeZone":"UTC"});
    case.change("end_type", 0x2021);
    case.change("end_date", minutes("2026-12-30"));
    let mut source = case.source();
    source["subject"] = json!("private diagnostic title");
    source["body"]["content"] = json!("private diagnostic body");
    let (client, captured) = server(|_| vec![(200, source.clone()), (200, source.clone())]).await;
    let failure = client
        .fetch_calendar_display_set("selected", "master", &crate::backend::testutil::event())
        .await
        .unwrap_err();
    let error = failure.to_string();
    assert!(
        error.contains("blob=2026-12-30")
            && error.contains("computed=2026-12-10")
            && error.contains("cutoff=20261231T235959Z"),
        "{error}"
    );
    for formatted in [format!("{failure:?}"), error] {
        assert!(!formatted.contains("private diagnostic"));
        assert!(!formatted.contains(&case.encode()));
    }
    let native = failure.native.unwrap();
    assert_eq!(serde_json::from_str::<Value>(&native.data).unwrap(), source);
    assert!(matches!(failure.error, Error::Sync(_)));
    assert!(captured
        .await
        .unwrap()
        .iter()
        .all(|request| request.starts_with("GET ")));
}

#[test]
fn noend_display_blob_checks_finite_originals_without_interpreting_opaque_ids() {
    let mut case = monthly();
    case.value["recurrence"]["range"] =
        json!({"type":"noEnd", "startDate":"2026-09-14", "recurrenceTimeZone":"UTC"});
    case.change("end_type", 0x2023);
    case.change("end_date", 0x5ae980df);
    case.change("count", 10);
    assert!(
        recurrence_blob::deleted_display_positions(&case.encode(), &case.event(), &[], None)
            .is_ok()
    );
    case.change("deleted", minutes("2026-11-13"));
    assert!(
        recurrence_blob::deleted_display_positions(&case.encode(), &case.event(), &[], None)
            .unwrap_err()
            .to_string()
            .contains("outside the recurrence")
    );
}

#[test]
fn modified_originals_are_subtracted_but_still_verified_against_the_pattern() {
    let case = fixture(
        "relativeMonthly",
        &["thursday"],
        0x10,
        "second",
        1,
        "2026-09-14",
        &["2026-10-08", "2026-11-12", "2026-12-10"],
        &["2026-10-08", "2026-11-12"],
        &[("2026-10-08", "2026-10-09")],
    );
    let event = case.event();
    let override_item = CalendarOverride {
        original_start: "2026-10-08T09:00:00Z".into(),
        event: Some(event.clone()),
        native: None,
    };
    assert_eq!(
        recurrence_blob::deleted_display_positions(
            &case.encode(),
            &event,
            std::slice::from_ref(&override_item),
            None
        )
        .unwrap(),
        ["2026-11-12T09:00:00Z"]
    );
    let wrong_time = CalendarOverride {
        original_start: "2026-10-08T10:00:00Z".into(),
        ..override_item
    };
    assert!(recurrence_blob::deleted_display_positions(
        &case.encode(),
        &event,
        &[wrong_time],
        None
    )
    .is_err());
}

#[test]
fn display_blob_rejects_truncation_corrupt_patterns_dates_phase_and_ranges() {
    let case = monthly();
    let event = case.event();
    for end in 0..case.bytes.len() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&case.bytes[..end]);
        assert!(
            matches!(
                recurrence_blob::deleted_display_positions(&encoded, &event, &[], None),
                Err(Error::Sync(_))
            ),
            "{end}"
        );
    }
    for (key, value) in [
        ("phase", 1440),
        ("period", 2),
        ("mask", 0x20),
        ("nth", 1),
        ("nth", 6),
        ("count", 2),
        ("count", 4),
        ("end_type", 0x2021),
        ("first_dow", 7),
        ("deleted", minutes("2026-11-13")),
        ("deleted", minutes("2027-01-14")),
        ("start_date", minutes("2026-09-14")),
        ("end_date", minutes("2026-12-11")),
        ("start_offset", 541),
        ("reader", 0),
    ] {
        let mut bad = monthly();
        bad.change(key, value);
        assert!(
            matches!(
                recurrence_blob::deleted_display_positions(&bad.encode(), &event, &[], None),
                Err(Error::Sync(_))
            ),
            "{key}={value}"
        );
    }
}

#[test]
fn display_cancellation_membership_preserves_all_day_dates_dst_and_work_budgets() {
    let mut case = monthly();
    case.value["isAllDay"] = json!(true);
    case.value["start"]["dateTime"] = json!("2026-09-14T00:00:00");
    case.value["end"]["dateTime"] = json!("2026-09-15T00:00:00");
    case.change("start_offset", 0);
    case.change("end_offset", 1440);
    assert_eq!(
        recurrence_blob::deleted_display_positions(&case.encode(), &case.event(), &[], None)
            .unwrap(),
        ["2026-11-12"]
    );
    let mut case = monthly();
    case.value["originalStartTimeZone"] = json!("Europe/Stockholm");
    case.value["originalEndTimeZone"] = json!("Europe/Stockholm");
    case.value["recurrence"]["range"]["recurrenceTimeZone"] = json!("Europe/Stockholm");
    case.value["start"]["timeZone"] = json!("Europe/Stockholm");
    case.value["end"]["timeZone"] = json!("Europe/Stockholm");
    assert_eq!(
        recurrence_blob::deleted_display_positions(&case.encode(), &case.event(), &[], None)
            .unwrap(),
        ["2026-11-12T08:00:00Z"]
    );
    let mut event = case.event();
    event.recurrence_rule = Some("FREQ=SECONDLY".into());
    assert!(matches!(
        recurrence_blob::deleted_display_positions(&case.encode(), &event, &[], None),
        Err(Error::Sync(_))
    ));
}

#[tokio::test]
async fn complete_relative_series_display_reads_cancellations_without_mutation_authority() {
    let source = relative_master();
    let (client, captured) = server(|_| {
        vec![
            (200, source.clone()),
            (200, source.clone()),
            (200, source.clone()),
        ]
    })
    .await;
    let set = client
        .fetch_calendar_display_set("selected", "master", &crate::backend::testutil::event())
        .await
        .unwrap();
    assert_eq!(set.event.start_time, "2026-10-08T09:00:00Z");
    assert_eq!(set.overrides.len(), 1);
    assert_eq!(set.overrides[0].original_start, "2026-11-12T09:00:00Z");
    assert!(set.overrides[0].event.is_none());
    assert!(canonical(&source, &set.event, "selected").is_err());
    assert!(simple_recurrence::normalize_rule(
        set.event.recurrence_rule.as_deref().unwrap(),
        &set.event
    )
    .is_err());
    assert_eq!(
        serde_json::from_str::<Value>(&set.native.as_ref().unwrap().data).unwrap(),
        source
    );
    assert!(captured
        .await
        .unwrap()
        .iter()
        .all(|request| request.starts_with("GET ")));
}
