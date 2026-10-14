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

pub(crate) fn cancelled_exception_fixture() -> (Value, BTreeMap<String, Value>) {
    let mut source = master();
    let mut children = BTreeMap::new();
    let mut cancelled = source.clone();
    cancelled["id"] = json!("cancelled");
    cancelled["type"] = json!("exception");
    cancelled["seriesMasterId"] = json!("master");
    cancelled["isCancelled"] = json!(true);
    cancelled["originalStart"] = json!("2026-09-15T09:00:00Z");
    cancelled["recurrence"] = Value::Null;
    children.insert("cancelled".into(), cancelled);
    for (id, date, hour) in [("live-a", "2026-09-16", 12), ("live-b", "2026-09-17", 14)] {
        let mut child = source.clone();
        child["id"] = json!(id);
        child["type"] = json!("exception");
        child["seriesMasterId"] = json!("master");
        child["originalStart"] = json!(format!("{date}T09:00:00Z"));
        child["start"]["dateTime"] = json!(format!("{date}T{hour}:00:00"));
        child["end"]["dateTime"] = json!(format!("{date}T{}:00:00", hour + 1));
        child["recurrence"] = Value::Null;
        children.insert(id.into(), child);
    }
    source["exceptionOccurrences"] = json!(children.values().collect::<Vec<_>>());
    (source, children)
}

/// Route by resource ID so concurrent exception reads are not order-dependent.
pub(crate) async fn serve_display_series(
    source: Value,
    children: BTreeMap<String, Value>,
    expected_requests: usize,
    after: Option<Value>,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/graph", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        let mut master_reads = 0;
        for _ in 0..expected_requests {
            let (mut socket, request) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                tests::accept_request(&listener),
            )
            .await
            .unwrap();
            let path = request
                .split_ascii_whitespace()
                .nth(1)
                .unwrap()
                .split('?')
                .next()
                .unwrap();
            let id = path.rsplit('/').next().unwrap();
            let value = match id {
                "calendars" => {
                    json!({"value":[{"id":"primary", "name":"Calendar", "isDefaultCalendar":true, "color":"auto"}]})
                }
                "events" => json!({"value":[source.clone()]}),
                "master" => {
                    master_reads += 1;
                    if master_reads == 3 {
                        after.as_ref().unwrap_or(&source).clone()
                    } else {
                        source.clone()
                    }
                }
                id => children.get(id).unwrap().clone(),
            };
            requests.push(request);
            tests::respond(&mut socket, 200, &value.to_string()).await;
        }
        requests
    });
    (root, task)
}

fn display_client(root: &str) -> GraphClient {
    GraphClient::with_client(
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        "secret-test-token",
        super::super::GraphEndpoints::new(root, root),
    )
}

#[tokio::test]
async fn cancelled_exception_is_an_exclusion_while_live_exceptions_and_native_data_survive() {
    let (source, children) = cancelled_exception_fixture();
    let (root, requests) = serve_display_series(source.clone(), children.clone(), 6, None).await;
    let set = display_client(&root)
        .fetch_calendar_display_set("selected", "master", &crate::backend::testutil::event())
        .await
        .unwrap();
    assert_eq!(set.overrides.len(), 3);
    let cancelled = &set.overrides[0];
    assert_eq!(cancelled.original_start, "2026-09-15T09:00:00Z");
    assert!(cancelled.event.is_none());
    assert_eq!(
        serde_json::from_str::<Value>(&cancelled.native.as_ref().unwrap().data).unwrap(),
        children["cancelled"]
    );
    assert_eq!(
        serde_json::from_str::<Value>(&set.native.as_ref().unwrap().data).unwrap(),
        source
    );
    let page = crate::calendar::actions::project_display(
        &set,
        &set.event,
        "",
        "2026-09-14",
        "2026-09-18",
        100,
    )
    .unwrap();
    assert_eq!(
        page.occurrences
            .iter()
            .map(|item| item.fields.start_time.as_str())
            .collect::<Vec<_>>(),
        [
            "2026-09-14T09:00:00Z",
            "2026-09-16T12:00:00Z",
            "2026-09-17T14:00:00Z"
        ]
    );
    assert!(!serde_json::to_string(&page)
        .unwrap()
        .contains("isCancelled"));
    assert!(requests
        .await
        .unwrap()
        .iter()
        .all(|request| request.starts_with("GET ")));
}

#[tokio::test]
async fn sparse_cancelled_exception_needs_identity_and_original_not_live_content() {
    let (mut source, children) = cancelled_exception_fixture();
    let complete = &children["cancelled"];
    let sparse = [
        "id",
        "type",
        "seriesMasterId",
        "isCancelled",
        "originalStart",
    ]
    .into_iter()
    .map(|key| (key.to_owned(), complete[key].clone()))
    .collect::<serde_json::Map<_, _>>();
    source["exceptionOccurrences"] = json!([{"id":"cancelled"}]);
    let (root, requests) = serve_display_series(
        source,
        BTreeMap::from([("cancelled".into(), Value::Object(sparse))]),
        4,
        None,
    )
    .await;
    let set = display_client(&root)
        .fetch_calendar_display_set("selected", "master", &crate::backend::testutil::event())
        .await
        .unwrap();
    assert!(set.overrides[0].event.is_none());
    assert_eq!(set.overrides[0].original_start, "2026-09-15T09:00:00Z");
    assert_eq!(requests.await.unwrap().len(), 4);
}

#[tokio::test]
async fn cancelled_exception_does_not_grant_strict_mutation_reads_or_accept_bad_originals() {
    let (mut source, mut children) = cancelled_exception_fixture();
    let child = children.remove("cancelled").unwrap();
    source["exceptionOccurrences"] = json!([{"id":"cancelled"}]);
    let children = BTreeMap::from([("cancelled".into(), child.clone())]);
    let (root, requests) = serve_display_series(source.clone(), children, 3, None).await;
    let error = display_client(&root)
        .fetch_calendar_event_set("selected", "master", &crate::backend::testutil::event())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("isCancelled must be false"));
    assert_eq!(requests.await.unwrap().len(), 3);
    for (key, value) in [
        ("id", json!("foreign")),
        ("type", json!("occurrence")),
        ("seriesMasterId", json!("foreign")),
        ("originalStart", Value::Null),
        ("originalStart", json!("invalid")),
        ("originalStart", json!("2026-09-15T08:00:00Z")),
        ("originalStart", json!("2026-09-20T09:00:00Z")),
        ("isCancelled", Value::Null),
        ("isCancelled", json!("true")),
    ] {
        let mut bad = child.clone();
        bad[key] = value;
        let (root, requests) = serve_display_series(
            source.clone(),
            BTreeMap::from([("cancelled".into(), bad)]),
            3,
            None,
        )
        .await;
        assert!(
            display_client(&root)
                .fetch_calendar_display_set(
                    "selected",
                    "master",
                    &crate::backend::testutil::event()
                )
                .await
                .is_err(),
            "{key}"
        );
        assert_eq!(requests.await.unwrap().len(), 3);
    }
}

#[tokio::test]
async fn cancelled_exception_keeps_master_revision_and_cancellation_boundaries_fail_closed() {
    let (mut source, mut children) = cancelled_exception_fixture();
    let child = children.remove("cancelled").unwrap();
    source["exceptionOccurrences"] = json!([{"id":"cancelled"}]);
    let mut changed = source.clone();
    changed["@odata.etag"] = json!("W/\"new\"");
    let (root, requests) = serve_display_series(
        source.clone(),
        BTreeMap::from([("cancelled".into(), child.clone())]),
        4,
        Some(changed),
    )
    .await;
    let failure = display_client(&root)
        .fetch_calendar_display_set("selected", "master", &crate::backend::testutil::event())
        .await
        .unwrap_err();
    assert!(matches!(failure.error, Error::CalendarReadIncomplete(_)));
    assert_eq!(requests.await.unwrap().len(), 4);
    source["isCancelled"] = json!(true);
    let (root, requests) = serve_display_series(
        source,
        BTreeMap::from([("cancelled".into(), child)]),
        2,
        None,
    )
    .await;
    let failure = display_client(&root)
        .fetch_calendar_display_set("selected", "master", &crate::backend::testutil::event())
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("isCancelled must be false"));
    assert_eq!(requests.await.unwrap().len(), 2);
}

#[tokio::test]
async fn cancelled_exception_original_proofs_preserve_dates_and_fractional_instants() {
    for all_day in [false, true] {
        let (mut source, mut children) = cancelled_exception_fixture();
        let mut child = children.remove("cancelled").unwrap();
        source["exceptionOccurrences"] = json!([{"id":"cancelled"}]);
        let expected = if all_day {
            source["isAllDay"] = json!(true);
            source["originalStartTimeZone"] = json!("UTC");
            source["originalEndTimeZone"] = json!("UTC");
            source["start"]["dateTime"] = json!("2026-09-14T00:00:00");
            source["end"]["dateTime"] = json!("2026-09-15T00:00:00");
            child["originalStart"] = json!("2026-09-15T00:00:00Z");
            "2026-09-15"
        } else {
            source["start"]["dateTime"] = json!("2026-09-14T09:00:00.125");
            source["end"]["dateTime"] = json!("2026-09-14T10:00:00.125");
            child["originalStart"] = json!("2026-09-15T09:00:00.125Z");
            "2026-09-15T09:00:00.125Z"
        };
        let (root, requests) = serve_display_series(
            source,
            BTreeMap::from([("cancelled".into(), child)]),
            4,
            None,
        )
        .await;
        let set = display_client(&root)
            .fetch_calendar_display_set("selected", "master", &crate::backend::testutil::event())
            .await
            .unwrap();
        assert_eq!(set.overrides[0].original_start, expected);
        assert!(set.overrides[0].event.is_none());
        assert_eq!(requests.await.unwrap().len(), 4);
    }
}

#[tokio::test]
async fn cancelled_exception_and_blob_deleted_slot_have_independent_verified_counts() {
    use base64::Engine;
    let (mut source, mut children) = cancelled_exception_fixture();
    let child = children.remove("cancelled").unwrap();
    source["exceptionOccurrences"] = json!([{"id":"cancelled"}]);
    source["cancelledOccurrences"] = json!(["opaque-deleted-slot"]);
    source["singleValueExtendedProperties"] = json!([{"id":RECUR_PROPERTY, "value":base64::engine::general_purpose::STANDARD.encode(tests::cancellation_blob(&[1,2], &[(1,1)]))}]);
    let (root, requests) = serve_display_series(
        source,
        BTreeMap::from([("cancelled".into(), child)]),
        4,
        None,
    )
    .await;
    let set = display_client(&root)
        .fetch_calendar_display_set("selected", "master", &crate::backend::testutil::event())
        .await
        .unwrap();
    assert_eq!(set.overrides.len(), 2);
    assert!(set.overrides.iter().all(|item| item.event.is_none()));
    let page = crate::calendar::actions::project_display(
        &set,
        &set.event,
        "",
        "2026-09-14",
        "2026-09-18",
        100,
    )
    .unwrap();
    assert_eq!(
        page.occurrences
            .iter()
            .map(|item| item.fields.start_time.as_str())
            .collect::<Vec<_>>(),
        ["2026-09-14T09:00:00Z", "2026-09-17T09:00:00Z"]
    );
    assert_eq!(requests.await.unwrap().len(), 4);
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

#[test]
fn utc_standalone_display_does_not_require_resolving_legacy_creation_metadata() {
    let mut value = source(json!({"type":"daily", "interval":1}), "2026-05-27");
    value["type"] = json!("singleInstance");
    value["recurrence"] = Value::Null;
    value["start"]["dateTime"] = json!("2026-05-27T08:00:00.0000000");
    value["end"]["dateTime"] = json!("2026-05-27T09:00:00.0000000");
    value["originalStartTimeZone"] = json!("tzone://Microsoft/Custom");
    value["originalEndTimeZone"] = json!("tzone://Microsoft/Custom");
    let original = value.clone();
    let event = display(&value);
    assert_eq!(event.start_time, "2026-05-27T08:00:00Z");
    assert_eq!(event.end_time, "2026-05-27T09:00:00Z");
    assert_eq!(event.timezone.as_deref(), Some("UTC"));
    assert_eq!(value, original);
    assert!(canonical(&value, &event, "selected").is_err());
    value["start"]["timeZone"] = json!("Unknown/BoundaryZone");
    assert!(display_event(&value, &event, "selected").is_err());
}

#[test]
fn all_day_display_restores_boundary_dates_independently_of_recurrence_zone() {
    let mut value = source(
        json!({"type":"absoluteYearly", "interval":1, "month":11, "dayOfMonth":3}),
        "1978-11-03",
    );
    value["isAllDay"] = json!(true);
    value["start"]["dateTime"] = json!("1978-11-03T00:00:00.0000000");
    value["end"]["dateTime"] = json!("1978-11-04T00:00:00.0000000");
    value["recurrence"]["range"] = json!({"type":"noEnd", "startDate":"1978-11-03", "endDate":"0001-01-01", "numberOfOccurrences":0, "recurrenceTimeZone":"W. Europe Standard Time"});
    let original = value.clone();
    let event = display(&value);
    assert_eq!(event.start_time, "1978-11-03");
    assert_eq!(event.end_time, "1978-11-04");
    assert_eq!(event.timezone.as_deref(), Some("UTC"));
    assert_eq!(
        positions(&event, "2026-11-01", "2026-12-01"),
        ["2026-11-03"]
    );
    assert_eq!(value, original);
    assert!(canonical(&value, &event, "selected").is_err());
    value["end"]["dateTime"] = json!("1978-11-04T01:00:00");
    assert!(display_event(&value, &event, "selected").is_err());
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
