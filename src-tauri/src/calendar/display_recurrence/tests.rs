use super::*;
use crate::calendar::{actions, event_set::CalendarEventSet, simple_recurrence, RecurrenceKind};

fn event(rule: &str) -> CalendarEvent {
    CalendarEvent {
        uid: Some("display".into()),
        all_day: true,
        timezone: None,
        start_time: "2020-01-01".into(),
        end_time: "2020-01-02".into(),
        recurrence_rule: Some(rule.into()),
        recurrence_kind: RecurrenceKind::Series,
        ..crate::backend::testutil::event()
    }
}

fn dates(event: &CalendarEvent, start: &str, end: &str) -> Vec<String> {
    expand(event, start, end, 100)
        .unwrap()
        .occurrences
        .into_iter()
        .map(|item| item.original_start)
        .collect()
}

#[test]
fn yearly_by_month_day_and_ordinal_weekdays_are_display_not_edit_grammar() {
    for (rule, expected) in [
        ("FREQ=YEARLY;BYMONTH=10;BYMONTHDAY=8", vec!["2026-10-08"]),
        ("FREQ=MONTHLY;BYDAY=2TH", vec!["2026-10-08"]),
        ("FREQ=MONTHLY;BYDAY=-1FR", vec!["2026-10-30"]),
        (
            "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1",
            vec!["2026-10-30"],
        ),
    ] {
        let event = event(rule);
        assert_eq!(dates(&event, "2026-10-01", "2026-11-01"), expected);
        assert!(simple_recurrence::normalize_rule(rule, &event).is_err());
    }
}

#[test]
fn all_day_utc_until_and_date_until_have_inclusive_cutoffs() {
    for until in ["20261008T235959Z", "20261008"] {
        let event = event(&format!("FREQ=DAILY;UNTIL={until}"));
        assert_eq!(
            dates(&event, "2026-10-07", "2026-10-10"),
            ["2026-10-07", "2026-10-08"]
        );
    }
    let event = event("FREQ=DAILY;UNTIL=20261008T000000Z");
    assert_eq!(
        dates(&event, "2026-10-07", "2026-10-10"),
        ["2026-10-07", "2026-10-08"]
    );
}

#[test]
fn expired_bounds_do_not_invent_future_positions_or_hide_explicit_overrides() {
    let mut event = event("FREQ=DAILY;UNTIL=20190101T000000Z");
    assert!(dates(&event, "2026-10-01", "2026-11-01").is_empty());
    let mut extra = event.clone();
    extra.start_time = "2026-10-08".into();
    extra.end_time = "2026-10-09".into();
    extra.recurrence_kind = RecurrenceKind::Occurrence;
    extra.recurrence_rule = None;
    let set = CalendarEventSet {
        event: event.clone(),
        overrides: vec![crate::calendar::event_set::CalendarOverride {
            original_start: "2026-10-08".into(),
            event: Some(extra),
            native: None,
        }],
        native: None,
        content: None,
    };
    let page = actions::project_display(&set, &event, "", "2026-10-01", "2026-11-01", 100).unwrap();
    assert_eq!(page.occurrences.len(), 1);
    event.recurrence_rule = Some("FREQ=DAILY;UNTIL=20190101T000000Z;BYMONTH=99".into());
    assert!(expand(&event, "2026-10-01", "2026-11-01", 100).is_err());
}

#[test]
fn original_count_long_overlap_half_open_window_and_exact_limits() {
    let mut event = event("FREQ=MONTHLY;BYDAY=2TH;COUNT=3");
    event.start_time = "2026-08-13".into();
    event.end_time = "2026-08-16".into();
    assert_eq!(dates(&event, "2026-10-09", "2026-10-12"), ["2026-10-08"]);
    assert!(dates(&event, "2026-11-01", "2026-12-01").is_empty());
    event.recurrence_rule = Some("FREQ=DAILY;COUNT=2".into());
    event.start_time = "2026-10-08".into();
    event.end_time = "2026-10-09".into();
    assert!(
        !expand(&event, "2026-10-08", "2026-10-10", 2)
            .unwrap()
            .has_more
    );
    assert!(
        expand(&event, "2026-10-08", "2026-10-10", 1)
            .unwrap()
            .has_more
    );
    assert_eq!(dates(&event, "2026-10-09", "2026-10-10"), ["2026-10-09"]);
}

#[test]
fn timed_recurrence_preserves_wall_clock_through_dst_and_fractional_precision() {
    let mut event = event("FREQ=DAILY;COUNT=3");
    event.all_day = false;
    event.timezone = Some("Europe/Stockholm".into());
    event.start_time = "2026-10-24T10:00:00.125Z".into();
    event.end_time = "2026-10-24T11:00:00.125Z".into();
    assert_eq!(
        dates(&event, "2026-10-24", "2026-10-27"),
        [
            "2026-10-24T10:00:00.125Z",
            "2026-10-25T11:00:00.125Z",
            "2026-10-26T11:00:00.125Z"
        ]
    );
    event.recurrence_rule = Some("FREQ=DAILY;UNTIL=20261025".into());
    assert_eq!(dates(&event, "2026-10-24", "2026-10-27").len(), 2);
}

#[test]
fn hostile_rules_and_exhausted_work_never_claim_an_empty_complete_calendar() {
    for rule in [
        "FREQ=DAILY;BYMONTH=99",
        "FREQ=DAILY;COUNT=2;UNTIL=20261008",
        "FREQ=DAILY;COUNT=1;COUNT=2",
        "FREQ=SECONDLY",
        "FREQ=HOURLY;INTERVAL=0",
    ] {
        assert!(
            expand(&event(rule), "2026-10-01", "2026-11-01", 100).is_err(),
            "{rule}"
        );
    }
    let hours = (0..24).map(|n| n.to_string()).collect::<Vec<_>>().join(",");
    let minutes = (0..60).map(|n| n.to_string()).collect::<Vec<_>>().join(",");
    let rule = format!("FREQ=YEARLY;BYHOUR={hours};BYMINUTE={minutes};BYSECOND={minutes}");
    let mut timed = event(&rule);
    timed.all_day = false;
    timed.start_time = "2020-01-01T10:00:00Z".into();
    timed.end_time = "2020-01-01T11:00:00Z".into();
    assert!(expand(&timed, "2026-10-01", "2026-11-01", 100)
        .unwrap_err()
        .to_string()
        .contains("budget"));
}

#[test]
fn date_recurrences_ignore_time_parts_without_duplicate_day_identities() {
    let event = event("FREQ=DAILY;BYHOUR=10,12;BYMINUTE=30;BYSECOND=10;UNTIL=20261008T235959Z");
    assert_eq!(
        dates(&event, "2026-10-07", "2026-10-10"),
        ["2026-10-07", "2026-10-08"]
    );
}

#[test]
fn dst_gaps_are_skipped_without_consuming_count_and_folds_use_first_instant() {
    let mut event = event("FREQ=DAILY;COUNT=3");
    event.all_day = false;
    event.timezone = Some("Europe/Stockholm".into());
    event.start_time = "2026-03-28T01:30:00Z".into();
    event.end_time = "2026-03-28T02:30:00Z".into();
    assert_eq!(
        dates(&event, "2026-03-28", "2026-04-02"),
        [
            "2026-03-28T01:30:00Z",
            "2026-03-30T00:30:00Z",
            "2026-03-31T00:30:00Z",
        ]
    );
    event.start_time = "2026-10-24T00:30:00Z".into();
    event.end_time = "2026-10-24T01:30:00Z".into();
    assert_eq!(
        dates(&event, "2026-10-24", "2026-10-27"),
        [
            "2026-10-24T00:30:00Z",
            "2026-10-25T00:30:00Z",
            "2026-10-26T01:30:00Z",
        ]
    );
}

#[test]
fn utc_cutoffs_and_half_open_bounds_are_compared_after_zone_resolution() {
    let mut event = event("FREQ=DAILY;UNTIL=20261025T003000Z");
    event.all_day = false;
    event.timezone = Some("Europe/Stockholm".into());
    event.start_time = "2026-10-24T00:30:00Z".into();
    event.end_time = "2026-10-24T01:30:00Z".into();
    assert_eq!(dates(&event, "2026-10-24", "2026-10-27").len(), 2);
    assert_eq!(
        dates(&event, "2026-10-25T00:30:00Z", "2026-10-25T00:30:00.001Z"),
        ["2026-10-25T00:30:00Z"]
    );
    event.recurrence_rule = Some("FREQ=DAILY;UNTIL=20261025T002959Z".into());
    assert_eq!(dates(&event, "2026-10-24", "2026-10-27").len(), 1);
    event.recurrence_rule = Some("FREQ=DAILY;UNTIL=20261025T003000Z".into());
    event.start_time = "2026-10-24T00:30:00.125Z".into();
    event.end_time = "2026-10-24T01:30:00.125Z".into();
    assert_eq!(dates(&event, "2026-10-24", "2026-10-27").len(), 1);
}

#[test]
fn exclusions_moved_overrides_and_finite_additions_remain_canonical() {
    use crate::calendar::event_set::CalendarOverride;
    let event = event("FREQ=MONTHLY;BYDAY=2TH");
    let mut moved = event.clone();
    moved.start_time = "2026-10-09".into();
    moved.end_time = "2026-10-10".into();
    moved.recurrence_kind = RecurrenceKind::Occurrence;
    moved.recurrence_rule = None;
    let mut added = moved.clone();
    added.start_time = "2026-10-12".into();
    added.end_time = "2026-10-13".into();
    let set = CalendarEventSet {
        event: event.clone(),
        overrides: vec![
            CalendarOverride {
                original_start: "2026-09-10".into(),
                event: None,
                native: None,
            },
            CalendarOverride {
                original_start: "2026-10-08".into(),
                event: Some(moved),
                native: None,
            },
            CalendarOverride {
                original_start: "2026-10-12".into(),
                event: Some(added),
                native: None,
            },
        ],
        native: None,
        content: None,
    };
    let page = actions::project_display(&set, &event, "", "2026-09-01", "2026-11-01", 100).unwrap();
    assert!(!page.has_more);
    assert_eq!(
        page.occurrences
            .iter()
            .map(|item| item.fields.start_time.as_str())
            .collect::<Vec<_>>(),
        ["2026-10-09", "2026-10-12"]
    );
}

#[test]
fn generated_budget_exhaustion_is_an_error_not_a_complete_empty_window() {
    let mut event = event("FREQ=DAILY");
    event.start_time = "1800-01-01".into();
    event.end_time = "1800-01-02".into();
    assert!(expand(&event, "2026-10-01", "2026-11-01", 100)
        .unwrap_err()
        .to_string()
        .contains("budget"));
    event.recurrence_rule = Some("FREQ=DAILY;COUNT=2".into());
    assert!(dates(&event, "2026-10-01", "2026-11-01").is_empty());
}

#[test]
fn explicit_start_in_second_fold_is_preserved_without_creating_two_positions() {
    let mut event = event("FREQ=DAILY;COUNT=2");
    event.all_day = false;
    event.timezone = Some("Europe/Stockholm".into());
    event.start_time = "2026-10-25T01:30:00Z".into();
    event.end_time = "2026-10-25T02:30:00Z".into();
    assert_eq!(
        dates(&event, "2026-10-25", "2026-10-28"),
        ["2026-10-25T01:30:00Z", "2026-10-26T01:30:00Z"]
    );
}

#[test]
fn finite_old_series_and_small_result_windows_stop_without_losing_counts_or_limits() {
    let mut event = event("FREQ=DAILY;COUNT=3");
    event.start_time = "1990-01-01".into();
    event.end_time = "1990-01-02".into();
    assert!(dates(&event, "2026-10-01", "2026-11-01").is_empty());
    let page = expand(&event, "1990-01-01", "2026-11-01", 2).unwrap();
    assert_eq!(page.occurrences.len(), 2);
    assert!(page.has_more);
    let page = expand(&event, "1990-01-01", "2026-11-01", 3).unwrap();
    assert_eq!(page.occurrences.len(), 3);
    assert!(!page.has_more);
    event.recurrence_rule = Some("FREQ=DAILY".into());
    let page = expand(&event, "1990-01-01", "2026-11-01", 1).unwrap();
    assert_eq!(page.occurrences.len(), 1);
    assert!(page.has_more);
}

#[test]
fn engine_budget_exhaustion_cannot_be_mistaken_for_a_complete_empty_window() {
    let event = event("FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=30");
    assert!(expand(&event, "2026-10-01", "2026-11-01", 100)
        .unwrap_err()
        .to_string()
        .contains("budget"));
}

#[test]
fn exact_candidate_budget_requires_count_completion_without_an_extra_candidate() {
    let mut event = event("FREQ=DAILY;COUNT=50000");
    event.start_time = "1800-01-01".into();
    event.end_time = "1800-01-02".into();
    assert!(dates(&event, "2000-01-01", "2000-02-01").is_empty());
    event.recurrence_rule = Some("FREQ=DAILY;COUNT=50001".into());
    assert!(expand(&event, "2000-01-01", "2000-02-01", 100)
        .unwrap_err()
        .to_string()
        .contains("budget"));
}

#[test]
fn original_position_proof_uses_inclusive_cutoffs_and_exact_keys_not_event_overlap() {
    let mut event = event("FREQ=MONTHLY;BYDAY=TH;BYSETPOS=2;UNTIL=20261210T000000Z");
    event.start_time = "2026-10-08".into();
    event.end_time = "2026-10-12".into();
    let summary = verify_original_positions(
        &event,
        "2026-12-10T00:00:00.000000001Z",
        &["2026-11-12".into()],
    )
    .unwrap();
    assert_eq!(summary.count, 3);
    assert_eq!(summary.first.as_deref(), Some("2026-10-08"));
    assert_eq!(summary.last.as_deref(), Some("2026-12-10"));
    assert!(verify_original_positions(&event, "2027-01-01", &["2026-11-13".into()]).is_err());
    assert!(verify_original_positions(&event, "2026-12-10", &["2026-12-10".into()]).is_err());
}

#[test]
fn original_position_proof_shares_dst_counts_and_explicit_budget_errors() {
    let mut event = event("FREQ=DAILY;COUNT=3");
    event.all_day = false;
    event.timezone = Some("Europe/Stockholm".into());
    event.start_time = "2026-03-28T01:30:00Z".into();
    event.end_time = "2026-03-28T02:30:00Z".into();
    let summary =
        verify_original_positions(&event, "2026-04-02", &["2026-03-30T00:30:00Z".into()]).unwrap();
    assert_eq!(summary.count, 3);
    assert_eq!(summary.last.as_deref(), Some("2026-03-31T00:30:00Z"));
    assert!(
        verify_original_positions(&event, "2026-04-02", &["2026-03-29T01:30:00Z".into()]).is_err()
    );
    event.start_time = "1800-01-01T09:00:00Z".into();
    event.end_time = "1800-01-01T10:00:00Z".into();
    event.recurrence_rule = Some("FREQ=DAILY".into());
    assert!(verify_original_positions(&event, "2026-11-01", &[])
        .unwrap_err()
        .to_string()
        .contains("budget"));
}

#[test]
fn equivalent_cutoffs_compare_complete_difference_and_not_overlapping_event_ends() {
    let mut event = event("FREQ=WEEKLY;BYDAY=MO,WE");
    event.all_day = false;
    event.timezone = Some("Europe/Stockholm".into());
    event.start_time = "2024-03-18T18:30:00Z".into();
    event.end_time = "2024-03-25T20:00:00Z".into();
    let a = "FREQ=WEEKLY;BYDAY=MO,WE;UNTIL=20241231T183000Z";
    let b = "FREQ=WEEKLY;BYDAY=MO,WE;UNTIL=20241231T153000Z";
    for rules in [[a, b], [b, a]] {
        assert_eq!(equivalent_rule_cutoffs(&rules, &event).unwrap(), b);
    }
    for other in [
        "FREQ=WEEKLY;BYDAY=MO,WE;UNTIL=20241230T153000Z",
        "FREQ=WEEKLY;BYDAY=MO,WE;UNTIL=20250101T183000Z",
        "FREQ=WEEKLY;BYDAY=MO,WE",
        "FREQ=WEEKLY;BYDAY=MO,WE;INTERVAL=2;UNTIL=20241231T153000Z",
        "FREQ=WEEKLY;BYDAY=MO,FR;UNTIL=20241231T153000Z",
    ] {
        assert!(
            equivalent_rule_cutoffs(&[a, other], &event).is_err(),
            "{other}"
        );
        assert!(
            equivalent_rule_cutoffs(&[other, a], &event).is_err(),
            "{other}"
        );
    }
    assert!(equivalent_rule_cutoffs(
        &[a, "FREQ=WEEKLY;BYDAY=MO,WE;UNTIL=20241230T183000Z"],
        &event
    )
    .is_ok());
}

#[test]
fn equivalent_cutoff_proofs_resolve_dst_and_all_day_until_and_fail_on_budget_exhaustion() {
    let mut event = event("FREQ=DAILY");
    event.all_day = false;
    event.timezone = Some("Europe/Stockholm".into());
    event.start_time = "2026-10-24T00:30:00Z".into();
    event.end_time = "2026-10-24T01:30:00Z".into();
    assert!(equivalent_rule_cutoffs(
        &[
            "FREQ=DAILY;UNTIL=20261025T010000Z",
            "FREQ=DAILY;UNTIL=20261025T014000Z"
        ],
        &event
    )
    .is_ok());
    assert!(equivalent_rule_cutoffs(
        &[
            "FREQ=DAILY;UNTIL=20261025T000000Z",
            "FREQ=DAILY;UNTIL=20261025T003000Z"
        ],
        &event
    )
    .is_err());
    event.all_day = true;
    event.timezone = None;
    event.start_time = "2026-10-08".into();
    event.end_time = "2026-10-09".into();
    assert!(equivalent_rule_cutoffs(
        &[
            "FREQ=DAILY;UNTIL=20261008",
            "FREQ=DAILY;UNTIL=20261008T183000Z"
        ],
        &event
    )
    .is_ok());
    assert!(equivalent_rule_cutoffs(
        &[
            "FREQ=DAILY;UNTIL=20261008",
            "FREQ=DAILY;UNTIL=20261009T000000Z"
        ],
        &event
    )
    .is_err());
    event.start_time = "1800-01-01".into();
    event.end_time = "1800-01-02".into();
    assert!(equivalent_rule_cutoffs(
        &[
            "FREQ=DAILY;UNTIL=20261008T183000Z",
            "FREQ=DAILY;UNTIL=20261008T193000Z"
        ],
        &event
    )
    .unwrap_err()
    .to_string()
    .contains("budget"));
}

#[test]
#[ignore = "manual performance comparison, no wall-clock assertion"]
fn profile_old_counted_series_projection() {
    let mut event = event("FREQ=DAILY;COUNT=3");
    event.start_time = "1990-01-01".into();
    event.end_time = "1990-01-02".into();
    let start = std::time::Instant::now();
    for _ in 0..100 {
        assert!(expand(&event, "2026-10-01", "2026-11-01", 100)
            .unwrap()
            .occurrences
            .is_empty());
    }
    println!("100 old counted display series: {:?}", start.elapsed());
}
