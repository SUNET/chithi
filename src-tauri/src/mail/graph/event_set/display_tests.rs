use super::*;
use tests::{master, server};

fn source(pattern: Value, start: &str) -> Value {
    let mut value = master();
    value["start"] = json!({"dateTime":format!("{start}T09:00:00"), "timeZone":"UTC"});
    value["end"] = json!({"dateTime":format!("{start}T10:00:00"), "timeZone":"UTC"});
    value["originalStartTimeZone"] = json!("UTC");
    value["originalEndTimeZone"] = json!("UTC");
    value["recurrence"] = json!({
        "pattern":pattern,
        "range":{"type":"numbered", "startDate":start, "numberOfOccurrences":3, "recurrenceTimeZone":"UTC"},
    });
    value
}

fn display(value: &Value) -> CalendarEvent {
    display_event(value, &crate::backend::testutil::event(), "selected").unwrap()
}

fn positions(event: &CalendarEvent, start: &str, end: &str) -> Vec<String> {
    crate::calendar::display_recurrence::expand(event, start, end, 100)
        .unwrap()
        .occurrences
        .into_iter()
        .map(|item| item.original_start)
        .collect()
}

#[test]
fn optional_subject_is_display_only_and_never_replaced_in_native_content() {
    for series in [true, false] {
        for subject in [None, Some(Value::Null), Some(json!(""))] {
            let mut value = source(json!({"type":"daily", "interval":1}), "2026-09-14");
            if !series {
                value["type"] = json!("singleInstance");
                value["recurrence"] = Value::Null;
            }
            match subject {
                Some(subject) => value["subject"] = subject,
                None => {
                    value.as_object_mut().unwrap().remove("subject");
                }
            }
            let original = value.clone();
            let event = display(&value);
            assert!(event.title.is_empty());
            assert_eq!(value, original);
            assert!(canonical(&value, &event, "selected").is_err());
        }
    }
    for subject in [json!(false), json!(42), json!([]), json!({})] {
        let mut value = source(json!({"type":"daily", "interval":1}), "2026-09-14");
        value["subject"] = subject;
        assert!(display_event(&value, &crate::backend::testutil::event(), "selected").is_err());
    }
}

#[test]
fn all_six_graph_patterns_display_exact_positions_and_preserve_the_native_range() {
    for (pattern, expected, strict_supported) in [
        (
            json!({"type":"daily", "interval":2}),
            vec!["2026-09-14", "2026-09-16", "2026-09-18"],
            true,
        ),
        (
            json!({"type":"weekly", "interval":2, "daysOfWeek":["tuesday","thursday"], "firstDayOfWeek":"sunday"}),
            vec!["2026-09-15", "2026-09-17", "2026-09-29"],
            false,
        ),
        (
            json!({"type":"absoluteMonthly", "interval":1, "dayOfMonth":15}),
            vec!["2026-09-15", "2026-10-15", "2026-11-15"],
            false,
        ),
        (
            json!({"type":"relativeMonthly", "interval":1, "daysOfWeek":["thursday"], "index":"second"}),
            vec!["2026-10-08", "2026-11-12", "2026-12-10"],
            false,
        ),
        (
            json!({"type":"absoluteYearly", "interval":1, "dayOfMonth":15, "month":4}),
            vec!["2027-04-15", "2028-04-15", "2029-04-15"],
            false,
        ),
        (
            json!({"type":"relativeYearly", "interval":1, "daysOfWeek":["wednesday"], "index":"last", "month":11}),
            vec!["2026-11-25", "2027-11-24", "2028-11-29"],
            false,
        ),
    ] {
        let value = source(pattern.clone(), "2026-09-14");
        let original = value.clone();
        let event = display(&value);
        let expected = expected
            .into_iter()
            .map(|date| format!("{date}T09:00:00Z"))
            .collect::<Vec<_>>();
        assert_eq!(
            positions(&event, "2026-09-01", "2030-01-01"),
            expected,
            "{pattern}"
        );
        assert_eq!(event.start_time, expected[0]);
        assert_eq!(value, original);
        assert_eq!(
            canonical(&value, &event, "selected").is_ok(),
            strict_supported,
            "{pattern}"
        );
    }
}

#[test]
fn relative_weekday_sets_select_one_combined_instance_not_one_per_weekday() {
    for (index, days, expected) in [
        (
            "first",
            json!(["thursday", "friday"]),
            vec!["2026-10-01", "2026-11-05", "2026-12-03"],
        ),
        (
            "second",
            json!(["monday", "friday"]),
            vec!["2026-10-05", "2026-11-06", "2026-12-07"],
        ),
        (
            "third",
            json!(["thursday"]),
            vec!["2026-09-17", "2026-10-15", "2026-11-19"],
        ),
        (
            "fourth",
            json!(["thursday"]),
            vec!["2026-09-24", "2026-10-22", "2026-11-26"],
        ),
        (
            "last",
            json!(["monday", "friday"]),
            vec!["2026-09-28", "2026-10-30", "2026-11-30"],
        ),
    ] {
        let value = source(
            json!({"type":"relativeMonthly", "interval":1, "daysOfWeek":days, "index":index}),
            "2026-09-14",
        );
        let event = display(&value);
        assert_eq!(
            positions(&event, "2026-09-01", "2027-01-01"),
            expected
                .into_iter()
                .map(|date| format!("{date}T09:00:00Z"))
                .collect::<Vec<_>>(),
            "{index}"
        );
    }
}

#[test]
fn graph_documented_nonmatching_start_anchors_interval_at_first_occurrence() {
    let mut value = source(
        json!({"type":"relativeMonthly", "interval":2, "daysOfWeek":["thursday"], "index":"first"}),
        "2017-08-29",
    );
    value["recurrence"]["range"]["type"] = json!("noEnd");
    let event = display(&value);
    assert_eq!(
        positions(&event, "2017-08-01", "2018-01-01"),
        ["2017-09-07T09:00:00Z", "2017-11-02T09:00:00Z"]
    );
    assert_eq!(event.start_time, "2017-09-07T09:00:00Z");
    assert_eq!(event.end_time, "2017-09-07T10:00:00Z");
    assert_eq!(value["recurrence"]["range"]["startDate"], "2017-08-29");
}

#[test]
fn documented_defaults_apply_only_to_display_and_invalid_explicit_values_do_not_default() {
    for (pattern, expected) in [
        (
            json!({"type":"weekly", "interval":2, "daysOfWeek":["monday", "sunday"]}),
            vec![
                "2026-09-14T09:00:00Z",
                "2026-09-27T09:00:00Z",
                "2026-09-28T09:00:00Z",
            ],
        ),
        (
            json!({"type":"relativeMonthly", "interval":1, "daysOfWeek":["thursday"]}),
            vec![
                "2026-10-01T09:00:00Z",
                "2026-11-05T09:00:00Z",
                "2026-12-03T09:00:00Z",
            ],
        ),
    ] {
        let value = source(pattern, "2026-09-14");
        let event = display(&value);
        assert_eq!(positions(&event, "2026-09-01", "2027-01-01"), expected);
        assert!(canonical(&value, &event, "selected").is_err());
    }
    let value = source(
        json!({"type":"weekly", "interval":2, "daysOfWeek":["monday", "sunday"], "firstDayOfWeek":"monday"}),
        "2026-09-14",
    );
    assert_eq!(
        positions(&display(&value), "2026-09-01", "2027-01-01"),
        [
            "2026-09-14T09:00:00Z",
            "2026-09-20T09:00:00Z",
            "2026-09-28T09:00:00Z"
        ]
    );
}

#[test]
fn display_interval_is_not_limited_by_the_editor_and_historical_count_still_applies() {
    let value = source(json!({"type":"daily", "interval":100}), "2026-09-14");
    let event = display(&value);
    assert_eq!(
        positions(&event, "2026-09-01", "2027-05-01"),
        [
            "2026-09-14T09:00:00Z",
            "2026-12-23T09:00:00Z",
            "2027-04-02T09:00:00Z"
        ]
    );
    assert!(canonical(&value, &event, "selected").is_err());
    let mut value = source(
        json!({"type":"relativeMonthly", "interval":1, "daysOfWeek":["thursday"], "index":"second"}),
        "2026-09-14",
    );
    value["recurrence"]["range"]["numberOfOccurrences"] = json!(2);
    let event = display(&value);
    assert_eq!(
        positions(&event, "2026-11-01", "2027-01-01"),
        ["2026-11-12T09:00:00Z"]
    );
    assert!(positions(&event, "2026-12-01", "2027-01-01").is_empty());
}

#[test]
fn end_date_is_inclusive_in_the_known_recurrence_zone_across_dst() {
    let mut value = master();
    value["recurrence"]["pattern"] =
        json!({"type":"relativeMonthly", "interval":1, "daysOfWeek":["friday"], "index":"last"});
    value["recurrence"]["range"] = json!({"type":"endDate", "startDate":"2026-09-14", "endDate":"2026-10-30", "recurrenceTimeZone":"Europe/Stockholm"});
    let event = display(&value);
    assert_eq!(
        positions(&event, "2026-09-01", "2026-12-01"),
        ["2026-09-25T09:00:00Z", "2026-10-30T10:00:00Z"]
    );
}

#[test]
fn all_day_relative_yearly_dates_and_leap_day_anchors_remain_dates() {
    for (pattern, expected) in [
        (
            json!({"type":"relativeYearly", "interval":1, "daysOfWeek":["friday"], "index":"last", "month":9}),
            vec!["2026-09-25", "2027-09-24", "2028-09-29"],
        ),
        (
            json!({"type":"absoluteYearly", "interval":1, "month":2, "dayOfMonth":29}),
            vec!["2028-02-29", "2032-02-29", "2036-02-29"],
        ),
    ] {
        let mut value = source(pattern, "2026-09-14");
        value["isAllDay"] = json!(true);
        value["start"]["dateTime"] = json!("2026-09-14T00:00:00");
        value["end"]["dateTime"] = json!("2026-09-15T00:00:00");
        let event = display(&value);
        assert_eq!(positions(&event, "2026-09-01", "2040-01-01"), expected);
        assert_eq!(event.start_time.len(), 10);
        assert_eq!(event.end_time.len(), 10);
        assert!(canonical(&value, &event, "selected").is_err());
    }
}

#[test]
fn malformed_patterns_and_ranges_remain_explicit_errors() {
    for pattern in [
        json!({"type":"hourly", "interval":1}),
        json!({"type":"daily", "interval":0}),
        json!({"type":"daily", "interval":65536}),
        json!({"type":"daily", "interval":1.5}),
        json!({"type":"daily", "interval":"1"}),
        json!({"type":"absoluteMonthly", "interval":1, "dayOfMonth":32}),
        json!({"type":"absoluteYearly", "interval":1, "dayOfMonth":15, "month":13}),
        json!({"type":"absoluteYearly", "interval":1, "dayOfMonth":30, "month":2}),
        json!({"type":"relativeMonthly", "interval":1, "daysOfWeek":[]}),
        json!({"type":"relativeMonthly", "interval":1, "daysOfWeek":["holiday"]}),
        json!({"type":"relativeMonthly", "interval":1, "daysOfWeek":["monday", "monday"]}),
        json!({"type":"relativeMonthly", "interval":1, "daysOfWeek":["monday"], "index":"fifth"}),
        json!({"type":"relativeMonthly", "interval":1, "daysOfWeek":["monday"], "index":null}),
        json!({"type":"weekly", "interval":1, "daysOfWeek":["monday"], "firstDayOfWeek":null}),
    ] {
        let value = source(pattern.clone(), "2026-09-14");
        assert!(
            display_event(&value, &crate::backend::testutil::event(), "selected").is_err(),
            "{pattern}"
        );
    }
    for range in [
        json!({"type":"numbered", "startDate":"2026-09-14", "numberOfOccurrences":0}),
        json!({"type":"numbered", "startDate":"2026-09-14", "numberOfOccurrences":-1}),
        json!({"type":"numbered", "startDate":"2026-09-13", "numberOfOccurrences":3}),
        json!({"type":"endDate", "startDate":"2026-09-14", "endDate":"2026-02-30"}),
        json!({"type":"unknown", "startDate":"2026-09-14"}),
    ] {
        let mut value = source(
            json!({"type":"relativeMonthly", "interval":1, "daysOfWeek":["thursday"]}),
            "2026-09-14",
        );
        value["recurrence"]["range"] = range.clone();
        assert!(
            display_event(&value, &crate::backend::testutil::event(), "selected").is_err(),
            "{range}"
        );
    }
}

#[test]
fn unrelated_custom_zone_all_day_and_cancellation_errors_are_not_relaxed() {
    let mut value = source(
        json!({"type":"relativeMonthly", "interval":1, "daysOfWeek":["thursday"]}),
        "2026-09-14",
    );
    value["originalStartTimeZone"] = json!("tzone://Microsoft/Custom");
    value["recurrence"]["range"]["recurrenceTimeZone"] = json!("tzone://Microsoft/Custom");
    assert!(
        display_event(&value, &crate::backend::testutil::event(), "selected")
            .unwrap_err()
            .to_string()
            .contains("unknown timezone")
    );
    let mut value = source(json!({"type":"daily", "interval":1}), "2026-09-14");
    value["isAllDay"] = json!(true);
    assert!(
        display_event(&value, &crate::backend::testutil::event(), "selected")
            .unwrap_err()
            .to_string()
            .contains("all-day boundaries")
    );
    value["isAllDay"] = json!(false);
    value["isCancelled"] = json!(true);
    assert!(
        display_event(&value, &crate::backend::testutil::event(), "selected")
            .unwrap_err()
            .to_string()
            .contains("isCancelled must be false")
    );
}

#[tokio::test]
async fn complete_display_read_supports_null_subject_relative_pattern_and_exception_without_rewriting_upstream(
) {
    let mut value = source(
        json!({"type":"relativeMonthly", "interval":1, "daysOfWeek":["thursday"], "index":"second"}),
        "2026-09-14",
    );
    value["subject"] = Value::Null;
    value["exceptionOccurrences"] = json!([{"id":"exception"}]);
    let mut child = source(json!({"type":"daily", "interval":1}), "2026-10-09");
    child["id"] = json!("exception");
    child["type"] = json!("exception");
    child["seriesMasterId"] = json!("master");
    child["recurrence"] = Value::Null;
    child["subject"] = Value::Null;
    child["originalStart"] = json!("2026-10-08T09:00:00Z");
    let (client, captured) = server(|_| {
        vec![
            (200, value.clone()),
            (200, value.clone()),
            (200, child),
            (200, value.clone()),
        ]
    })
    .await;
    let set = client
        .fetch_calendar_display_set("selected", "master", &crate::backend::testutil::event())
        .await
        .unwrap();
    assert!(set.event.title.is_empty());
    assert_eq!(set.event.start_time, "2026-10-08T09:00:00Z");
    assert_eq!(set.overrides.len(), 1);
    assert!(set.overrides[0].event.as_ref().unwrap().title.is_empty());
    assert_eq!(set.overrides[0].original_start, "2026-10-08T09:00:00Z");
    assert_eq!(
        serde_json::from_str::<Value>(&set.native.unwrap().data).unwrap(),
        value
    );
    let requests = captured.await.unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests.iter().all(|request| request.starts_with("GET ")));
}
