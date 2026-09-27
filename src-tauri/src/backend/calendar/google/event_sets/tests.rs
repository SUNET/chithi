use super::*;
use crate::backend::calendar::google::sync_testutil::{
    serve_occurrence_responses, services, setup_db,
};
use crate::backend::calendar::google::GoogleCalendarBackend;
use crate::backend::calendar::CalendarBackend;
use crate::backend::testutil::{account, event};

type Step = (&'static str, u16, Value);

fn master() -> Value {
    json!({"id": "series1", "etag": "\"master-v1\"", "iCalUID": "series@example.test",
        "summary": "Weekly planning", "description": "<p>Full native body</p>", "location": "Room 2",
        "start": {"dateTime": "2026-09-14T10:00:00+02:00", "timeZone": "Europe/Stockholm"},
        "end": {"dateTime": "2026-09-14T11:00:00+02:00", "timeZone": "Europe/Stockholm"},
        "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=10"], "status": "confirmed", "eventType": "default",
        "organizer": {"email": "source@example.test", "self": true},
        "attendees": [{"email": "guest@example.test", "displayName": "Guest", "responseStatus": "accepted", "optional": true, "comment": "native comment"}],
        "reminders": {"useDefault": false, "overrides": [{"method": "popup", "minutes": 17}]},
        "visibility": "private", "transparency": "transparent"})
}

fn exception(position: &str, id: &str) -> Value {
    let mut value = master();
    value.as_object_mut().unwrap().remove("recurrence");
    value["id"] = json!(id);
    value["etag"] = json!(format!("\"{id}-v1\""));
    value["recurringEventId"] = json!("series1");
    value["originalStartTime"] = json!({"dateTime": position, "timeZone": "Europe/Stockholm"});
    value["start"] = json!({"dateTime": position, "timeZone": "Europe/Stockholm"});
    let end = DateTime::parse_from_rfc3339(position).unwrap() + chrono::Duration::hours(1);
    value["end"] = json!({"dateTime": end.to_rfc3339(), "timeZone": "Europe/Stockholm"});
    value
}

fn cancelled(position: &str, id: &str) -> Value {
    json!({"id": id, "etag": format!("\"{id}-v1\""), "status": "cancelled",
        "recurringEventId": "series1", "originalStartTime": {"dateTime": position}})
}

fn snapshot(master: &Value, exceptions: &[Value]) -> CalendarEventSet {
    let account = account("calendar", "google");
    let event = parse_event(master, &account, &event()).unwrap();
    CalendarEventSet {
        overrides: exceptions
            .iter()
            .map(|v| CalendarOverride {
                original_start: original(v, &event).unwrap(),
                event: if v["status"] == "cancelled" {
                    None
                } else {
                    Some(parse_event(v, &account, &event).unwrap())
                },
                native: Some(native(v, "source@calendar.test").unwrap()),
            })
            .collect(),
        event,
        native: Some(native(master, "source@calendar.test").unwrap()),
        content: None,
    }
}

fn read_steps(master: &Value, exceptions: &[Value]) -> Vec<Step> {
    vec![
        ("GET", 200, master.clone()),
        ("GET", 200, json!({"items": exceptions})),
        ("GET", 200, master.clone()),
    ]
}

fn body(request: &str) -> Value {
    serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap()
}

fn request_url(request: &str) -> url::Url {
    url::Url::parse(&format!(
        "http://localhost{}",
        request.split_whitespace().nth(1).unwrap()
    ))
    .unwrap()
}

async fn captured(handle: tokio::task::JoinHandle<Vec<String>>) -> Vec<String> {
    tokio::time::timeout(std::time::Duration::from_secs(3), handle)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn reads_exact_requested_instance_then_complete_out_of_view_pages() {
    let master = master();
    let moved = exception("2026-10-26T09:00:00Z", "opaque-modified-id");
    let cancelled = cancelled("2026-11-02T09:00:00Z", "opaque-cancelled-id");
    let requested = exception("2026-09-21T08:00:00Z", "requested-opaque-id");
    let responses = vec![
        ("GET", 200, requested),
        ("GET", 200, master.clone()),
        (
            "GET",
            200,
            json!({"items": [moved], "nextPageToken": "page-two"}),
        ),
        ("GET", 200, json!({"items": [cancelled]})),
        ("GET", 200, master),
    ];
    let (root, requests) = serve_occurrence_responses(responses).await;
    let client = services(&root).google_client("acc1").await.unwrap();
    let mut local = event();
    local.remote_id = Some("requested-opaque-id".into());
    let set = fetch(
        &client,
        &account("calendar", "google"),
        &local,
        "source@calendar.test",
    )
    .await
    .unwrap();
    assert_eq!(set.event.remote_id.as_deref(), Some("series1"));
    assert_eq!(
        set.event.description.as_deref(),
        Some("<p>Full native body</p>")
    );
    assert_eq!(set.overrides.len(), 2);
    assert_eq!(set.overrides[1].original_start, "2026-11-02T09:00:00Z");
    assert!(set.overrides[1].event.is_none());
    let requests = captured(requests).await;
    assert!(requests[0].contains("/events/requested-opaque-id "));
    for request in &requests[2..4] {
        let url = request_url(request);
        let query = url.query_pairs().collect::<Vec<_>>();
        assert_eq!(query.iter().filter(|(k, _)| k == "singleEvents").count(), 1);
        assert!(query
            .iter()
            .any(|(k, v)| k == "singleEvents" && v == "false"));
        assert!(query.iter().any(|(k, v)| k == "showDeleted" && v == "true"));
        assert!(!query.iter().any(|(k, _)| k == "timeMin" || k == "timeMax"));
    }
    assert!(requests[3].contains("pageToken=page-two"));
}

#[tokio::test]
async fn selected_nondefault_ordinary_creation_supports_dst_recurrence() {
    let (_dir, db) = setup_db().await;
    let (root, requests) = serve_occurrence_responses(vec![(
        "POST",
        200,
        json!({"id": "created", "iCalUID": "new@google"}),
    )])
    .await;
    let services = services(&root);
    let desired = snapshot(&master(), &[]);
    GoogleCalendarBackend
        .push_created_event(
            &CalendarBackendCtx {
                db: &db,
                services: &services,
            },
            &account("calendar", "google"),
            &desired.event,
            "selected@calendar.test",
        )
        .await
        .unwrap();
    let requests = captured(requests).await;
    assert!(requests[0].contains("/calendars/selected%40calendar.test/events?"));
    let value = body(&requests[0]);
    assert_eq!(
        value["recurrence"],
        json!(["RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=10"])
    );
    assert_eq!(value["start"]["timeZone"], "Europe/Stockholm");
    assert_eq!(value["start"]["dateTime"], "2026-09-14T10:00:00+02:00");
    assert!(value.get("iCalUID").is_none());
}

#[tokio::test]
async fn ordinary_update_uses_stored_source_calendar_not_desired_calendar() {
    let (_dir, db) = setup_db().await;
    let mut local = snapshot(&master(), &[]).event;
    {
        let conn = db.writer().await;
        conn.execute(
            "UPDATE calendars SET remote_id = 'selected@calendar.test' WHERE id = 'cal1'",
            [],
        )
        .unwrap();
        crate::db::calendar::insert_event(&conn, &local).unwrap();
    }
    local.calendar_id = "foreign-desired-calendar".into();
    let (root, requests) = serve_occurrence_responses(vec![("PATCH", 200, json!({}))]).await;
    let services = services(&root);
    GoogleCalendarBackend
        .push_updated_event(
            &CalendarBackendCtx {
                db: &db,
                services: &services,
            },
            &account("calendar", "google"),
            "series1",
            &local,
        )
        .await
        .unwrap();
    let requests = captured(requests).await;
    assert!(requests[0].contains("/calendars/selected%40calendar.test/events/series1?"));
}

#[tokio::test]
async fn master_title_patch_preserves_native_and_existing_effective_exception() {
    let master = master();
    let ex = exception("2026-09-21T08:00:00Z", "opaque-existing");
    let before = snapshot(&master, std::slice::from_ref(&ex));
    let mut desired = before.clone();
    desired.event.title = "Renamed master".into();
    let mut updated = master.clone();
    updated["summary"] = json!(desired.event.title);
    updated["etag"] = json!("\"master-v2\"");
    let mut responses = read_steps(&master, std::slice::from_ref(&ex));
    responses.push(("PATCH", 200, updated.clone()));
    responses.push(("GET", 200, json!({"items": [ex.clone()]})));
    responses.extend(read_steps(&updated, &[ex]));
    let (root, requests) = serve_occurrence_responses(responses).await;
    let client = services(&root).google_client("acc1").await.unwrap();
    let canonical = update(&client, &account("calendar", "google"), &before, &desired)
        .await
        .unwrap();
    assert_eq!(canonical.event.title, "Renamed master");
    let requests = captured(requests).await;
    assert_eq!(body(&requests[3]), json!({"summary": "Renamed master"}));
    assert!(requests[3].contains("if-match: \"master-v1\""));
    assert!(requests[4].contains("originalStart=2026-09-21T08%3A00%3A00Z"));
}

#[tokio::test]
async fn generated_and_existing_overrides_are_selected_by_original_start() {
    for existing in [false, true] {
        let master = master();
        let instance = exception("2026-09-21T08:00:00Z", "opaque-target");
        let exceptions = if existing {
            vec![instance.clone()]
        } else {
            vec![]
        };
        let before = snapshot(&master, &exceptions);
        let mut desired = before.clone();
        let mut ex = snapshot(&master, std::slice::from_ref(&instance))
            .overrides
            .remove(0);
        if !existing {
            ex.native = None;
        }
        ex.event.as_mut().unwrap().title = "Selected instance".into();
        ex.event.as_mut().unwrap().description = Some("<b>Edited full body</b>".into());
        desired.overrides = vec![ex];
        let mut updated = instance.clone();
        updated["summary"] = json!("Selected instance");
        updated["description"] = json!("<b>Edited full body</b>");
        updated["etag"] = json!("\"instance-v2\"");
        let mut responses = read_steps(&master, &exceptions);
        responses.push(("GET", 200, json!({"items": [instance]})));
        responses.push(("PATCH", 200, updated.clone()));
        responses.extend(read_steps(&master, &[updated]));
        let (root, requests) = serve_occurrence_responses(responses).await;
        let client = services(&root).google_client("acc1").await.unwrap();
        update(&client, &account("calendar", "google"), &before, &desired)
            .await
            .unwrap();
        let requests = captured(requests).await;
        assert!(requests[3].contains("/events/series1/instances?"));
        assert!(requests[4].contains("/events/opaque-target?"));
        assert!(requests[4].contains("if-match: \"opaque-target-v1\""));
        assert_eq!(
            body(&requests[4]),
            json!({"summary": "Selected instance", "description": "<b>Edited full body</b>"})
        );
    }
}

#[tokio::test]
async fn rule_change_replays_finite_cancellation_and_retained_exception() {
    let master = master();
    let ex = exception("2026-09-21T08:00:00Z", "old-exception");
    let cancellation = cancelled("2026-09-28T08:00:00Z", "old-cancellation");
    let before = snapshot(&master, &[ex.clone(), cancellation]);
    let mut desired = before.clone();
    desired.event.recurrence_rule = Some("FREQ=WEEKLY;BYDAY=MO;COUNT=20".into());
    let mut updated = master.clone();
    updated["recurrence"] = json!(["RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=20"]);
    updated["etag"] = json!("\"master-v2\"");
    let generated = exception("2026-09-28T08:00:00Z", "new-slot-id");
    let mut responses = read_steps(
        &master,
        &[
            ex.clone(),
            cancelled("2026-09-28T08:00:00Z", "old-cancellation"),
        ],
    );
    responses.push(("PATCH", 200, updated.clone()));
    responses.push(("GET", 200, json!({"items": [ex.clone()]})));
    responses.push(("GET", 200, json!({"items": [generated]})));
    responses.push(("DELETE", 200, json!({})));
    responses.extend(read_steps(
        &updated,
        &[ex, cancelled("2026-09-28T08:00:00Z", "new-slot-id")],
    ));
    let (root, requests) = serve_occurrence_responses(responses).await;
    let client = services(&root).google_client("acc1").await.unwrap();
    update(&client, &account("calendar", "google"), &before, &desired)
        .await
        .unwrap();
    let requests = captured(requests).await;
    assert_eq!(
        body(&requests[3]),
        json!({"recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=20"]})
    );
    assert!(requests[6].contains("/events/new-slot-id?"));
    assert!(requests[6].contains("if-match: \"new-slot-id-v1\""));
}

#[tokio::test]
async fn copied_series_reconciles_conflict_preserves_exceptions_and_uses_destination_ids() {
    let account = account("calendar", "google");
    let source = master();
    let mut ex = exception("2026-09-21T08:00:00Z", "source-exception-id");
    ex["summary"] = json!("Overridden title");
    let desired = snapshot(
        &source,
        &[
            ex.clone(),
            cancelled("2026-09-28T08:00:00Z", "source-cancel-id"),
        ],
    );
    let id = operation_event_id(&account.id, "destination@calendar.test", "persisted-op");
    assert!(id
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'v').contains(&b)));
    let mut destination = source.clone();
    destination["id"] = json!(id);
    destination["iCalUID"] = json!("new-google-uid");
    destination["extendedProperties"] = json!({"private": {"chithiOperation": "persisted-op"}});
    let mut generated = exception("2026-09-21T08:00:00Z", "destination-opaque-slot");
    generated["recurringEventId"] = json!(id);
    generated["iCalUID"] = json!("new-google-uid");
    let mut copied = generated.clone();
    copied["summary"] = json!("Overridden title");
    copied["etag"] = json!("\"copied-v2\"");
    let mut excluded = cancelled("2026-09-28T08:00:00Z", "destination-exclusion");
    excluded["recurringEventId"] = json!(id);
    // The master insert and cancellation already succeeded in a previous attempt.
    let mut responses = vec![
        ("POST", 409, json!({"error": "duplicate"})),
        ("GET", 200, destination.clone()),
        ("GET", 200, json!({"items": [generated]})),
        ("PATCH", 200, copied.clone()),
        ("GET", 200, json!({"items": [excluded.clone()]})),
    ];
    responses.extend(read_steps(&destination, &[copied, excluded]));
    let (root, requests) = serve_occurrence_responses(responses).await;
    let client = services(&root).google_client("acc1").await.unwrap();
    let result = create(
        &client,
        &account,
        "destination@calendar.test",
        &desired,
        "persisted-op",
    )
    .await
    .unwrap();
    assert_eq!(result.event.uid.as_deref(), Some("new-google-uid"));
    assert_eq!(
        result.native.as_ref().unwrap().calendar_id,
        "destination@calendar.test"
    );
    let requests = captured(requests).await;
    let inserted = body(&requests[0]);
    assert_eq!(inserted["id"], id);
    assert_eq!(inserted["reminders"], source["reminders"]);
    assert_eq!(inserted["visibility"], "private");
    assert_eq!(inserted["attendees"][0]["optional"], true);
    assert!(inserted.get("iCalUID").is_none());
    assert!(inserted.get("organizer").is_none());
    assert!(requests[1].contains(&format!("/events/{id} ")));
    assert!(requests[3].contains("/events/destination-opaque-slot?"));
    assert!(requests.iter().all(|r| !r.contains("/calendars/source")));
}

#[tokio::test]
async fn conditional_source_deletion_checks_complete_versions_and_exact_calendar() {
    let master = master();
    let before = snapshot(&master, &[]);
    let mut responses = read_steps(&master, &[]);
    responses.push(("DELETE", 200, json!({})));
    let (root, requests) = serve_occurrence_responses(responses).await;
    let (_dir, db) = setup_db().await;
    let services = services(&root);
    GoogleCalendarBackend
        .delete_event_set(
            &CalendarBackendCtx {
                db: &db,
                services: &services,
            },
            &account("calendar", "google"),
            &before,
        )
        .await
        .unwrap();
    let requests = captured(requests).await;
    assert!(requests[3].contains("/calendars/source%40calendar.test/events/series1?"));
    assert!(requests[3].contains("if-match: \"master-v1\""));
}

#[tokio::test]
async fn native_move_checks_permissions_and_reads_canonical_destination() {
    let master = master();
    let before = snapshot(&master, &[]);
    let mut responses = read_steps(&master, &[]);
    responses.extend([
        ("GET", 200, json!({"accessRole": "owner"})),
        ("GET", 200, json!({"accessRole": "writer"})),
        ("POST", 200, master.clone()),
    ]);
    responses.extend(read_steps(&master, &[]));
    let (root, requests) = serve_occurrence_responses(responses).await;
    let client = services(&root).google_client("acc1").await.unwrap();
    let result = move_native(
        &client,
        &account("calendar", "google"),
        &before,
        "destination@calendar.test",
    )
    .await
    .unwrap();
    let CalendarCapability::Supported(result) = result else {
        panic!("expected native move");
    };
    assert_eq!(
        result.native.unwrap().calendar_id,
        "destination@calendar.test"
    );
    let requests = captured(requests).await;
    assert!(requests[5].contains("/calendars/source%40calendar.test/events/series1/move?"));
    assert!(requests[5].contains("destination=destination%40calendar.test"));
    assert!(requests[5].contains("if-match: \"master-v1\""));
    assert!(requests[6].contains("/calendars/destination%40calendar.test/events/series1 "));
}

#[tokio::test]
async fn stale_snapshot_and_failed_patch_never_continue_to_exception_writes() {
    for stale in [false, true] {
        let master = master();
        let before = snapshot(&master, &[]);
        let mut desired = before.clone();
        desired.event.title = "Changed".into();
        let mut current = master.clone();
        if stale {
            current["etag"] = json!("\"stale\"");
        }
        let mut responses = read_steps(&current, &[]);
        if !stale {
            responses.push(("PATCH", 412, json!({"error": "precondition failed"})));
        }
        let (root, requests) = serve_occurrence_responses(responses).await;
        let client = services(&root).google_client("acc1").await.unwrap();
        assert!(
            update(&client, &account("calendar", "google"), &before, &desired)
                .await
                .is_err()
        );
        assert_eq!(captured(requests).await.len(), if stale { 3 } else { 4 });
    }
}

#[tokio::test]
async fn move_failure_is_error_not_unsupported_and_nondefault_preflight_has_no_io() {
    let master = master();
    let before = snapshot(&master, &[]);
    let mut responses = read_steps(&master, &[]);
    responses.extend([
        ("GET", 200, json!({"accessRole": "owner"})),
        ("GET", 200, json!({"accessRole": "writer"})),
        ("POST", 403, json!({"error": "forbidden"})),
    ]);
    let (root, requests) = serve_occurrence_responses(responses).await;
    let client = services(&root).google_client("acc1").await.unwrap();
    assert!(move_native(
        &client,
        &account("calendar", "google"),
        &before,
        "destination"
    )
    .await
    .is_err());
    assert_eq!(captured(requests).await.len(), 6);
    let mut special = master;
    special["eventType"] = json!("outOfOffice");
    let before = snapshot(&special, &[]);
    assert!(matches!(
        move_native(
            &client,
            &account("calendar", "google"),
            &before,
            "destination"
        )
        .await
        .unwrap(),
        CalendarCapability::Unsupported
    ));
}

#[test]
fn all_day_exclusive_dates_and_timed_until_are_protocol_correct() {
    let mut event = snapshot(&master(), &[]).event;
    event.recurrence_rule = Some("FREQ=DAILY;UNTIL=20260915".into());
    assert_eq!(
        event_set_content(&event).unwrap()["recurrence"],
        json!(["RRULE:FREQ=DAILY;UNTIL=20260915T215959Z"])
    );
    event.all_day = true;
    event.start_time = "2026-09-14".into();
    event.end_time = "2026-09-17".into();
    let body = event_set_content(&event).unwrap();
    assert_eq!(body["start"], json!({"date": "2026-09-14"}));
    assert_eq!(body["end"], json!({"date": "2026-09-17"}));
    assert_eq!(
        body["recurrence"],
        json!(["RRULE:FREQ=DAILY;UNTIL=20260915"])
    );
    event.timezone = Some("not-a-zone".into());
    assert!(event_set_content(&event).is_err());
}

#[tokio::test]
async fn lost_insert_response_is_reconciled_by_exact_operation_identity() {
    let account = account("calendar", "google");
    let mut desired = snapshot(&master(), &[]);
    desired.native = None;
    desired.event.recurrence_kind = RecurrenceKind::Standalone;
    desired.event.recurrence_rule = None;
    let id = operation_event_id(&account.id, "destination", "lost-response");
    let mut created = master();
    created.as_object_mut().unwrap().remove("recurrence");
    created["id"] = json!(id);
    created["extendedProperties"] = json!({"private": {"chithiOperation": "lost-response"}});
    let responses = vec![
        ("POST", 0, json!({})),
        ("GET", 200, created.clone()),
        ("GET", 200, created),
    ];
    let (root, requests) = serve_occurrence_responses(responses).await;
    let client = services(&root).google_client("acc1").await.unwrap();
    let canonical = create(&client, &account, "destination", &desired, "lost-response")
        .await
        .unwrap();
    assert_eq!(canonical.event.remote_id.as_deref(), Some(id.as_str()));
    let requests = captured(requests).await;
    assert_eq!(
        requests.iter().filter(|r| r.starts_with("POST ")).count(),
        1
    );
    assert!(requests[1].contains(&format!("/events/{id} ")));
    assert!(!requests.iter().any(|r| r.contains("iCalUID=")));
}

#[tokio::test]
async fn id_conflict_with_foreign_operation_is_not_replayed() {
    let account = account("calendar", "google");
    let desired = snapshot(&master(), &[]);
    let mut collision = master();
    collision["id"] = json!(operation_event_id(&account.id, "destination", "op1"));
    collision["extendedProperties"] = json!({"private": {"chithiOperation": "someone-else"}});
    let (root, requests) =
        serve_occurrence_responses(vec![("POST", 409, json!({})), ("GET", 200, collision)]).await;
    let client = services(&root).google_client("acc1").await.unwrap();
    assert!(create(&client, &account, "destination", &desired, "op1")
        .await
        .is_err());
    assert_eq!(captured(requests).await.len(), 2);
}

#[tokio::test]
async fn all_day_series_creation_copies_multiday_duration_and_exclusion() {
    let account = account("calendar", "google");
    let mut master = master();
    master["start"] = json!({"date": "2026-09-14"});
    master["end"] = json!({"date": "2026-09-17"});
    let cancellation = json!({"id": "old-cancel", "status": "cancelled", "recurringEventId": "series1", "originalStartTime": {"date": "2026-09-21"}});
    let desired = snapshot(&master, &[cancellation]);
    let id = operation_event_id(&account.id, "selected", "all-day-op");
    master["id"] = json!(id);
    master["extendedProperties"] = json!({"private": {"chithiOperation": "all-day-op"}});
    let mut generated = master.clone();
    generated.as_object_mut().unwrap().remove("recurrence");
    generated["id"] = json!("new-all-day-instance");
    generated["recurringEventId"] = json!(id);
    generated["originalStartTime"] = json!({"date": "2026-09-21"});
    generated["start"] = json!({"date": "2026-09-21"});
    generated["end"] = json!({"date": "2026-09-24"});
    let cancelled = json!({"id": "new-all-day-instance", "status": "cancelled", "recurringEventId": id, "originalStartTime": {"date": "2026-09-21"}});
    let mut responses = vec![
        ("POST", 200, master.clone()),
        ("GET", 200, json!({"items": [generated]})),
        ("DELETE", 200, json!({})),
    ];
    responses.extend(read_steps(&master, &[cancelled]));
    let (root, requests) = serve_occurrence_responses(responses).await;
    let client = services(&root).google_client("acc1").await.unwrap();
    let result = create(&client, &account, "selected", &desired, "all-day-op")
        .await
        .unwrap();
    assert_eq!(result.event.end_time, "2026-09-17");
    assert_eq!(result.overrides[0].original_start, "2026-09-21");
    let requests = captured(requests).await;
    assert_eq!(body(&requests[0])["end"], json!({"date": "2026-09-17"}));
    assert!(requests[1].contains("originalStart=2026-09-21&"));
    assert!(requests[2].contains("/events/new-all-day-instance?"));
}

#[tokio::test]
async fn failed_conditional_delete_is_reported_and_read_only_move_has_no_write() {
    let master = master();
    let before = snapshot(&master, &[]);
    let mut responses = read_steps(&master, &[]);
    responses.push(("DELETE", 412, json!({"error": "changed"})));
    let (root, requests) = serve_occurrence_responses(responses).await;
    let (_dir, db) = setup_db().await;
    let provider = services(&root);
    assert!(GoogleCalendarBackend
        .delete_event_set(
            &CalendarBackendCtx {
                db: &db,
                services: &provider
            },
            &account("calendar", "google"),
            &before
        )
        .await
        .is_err());
    assert_eq!(captured(requests).await.len(), 4);
    let mut responses = read_steps(&master, &[]);
    responses.push(("GET", 200, json!({"accessRole": "reader"})));
    let (root, requests) = serve_occurrence_responses(responses).await;
    let client = services(&root).google_client("acc1").await.unwrap();
    assert!(matches!(
        move_native(
            &client,
            &account("calendar", "google"),
            &before,
            "destination"
        )
        .await
        .unwrap(),
        CalendarCapability::Unsupported
    ));
    assert!(captured(requests)
        .await
        .iter()
        .all(|r| r.starts_with("GET ")));
}

#[tokio::test]
async fn ambiguous_original_start_is_an_error_before_any_occurrence_write() {
    let master = master();
    let before = snapshot(&master, &[]);
    let first = exception("2026-09-21T08:00:00Z", "first");
    let second = exception("2026-09-21T08:00:00Z", "second");
    let mut desired = before.clone();
    desired.overrides = vec![CalendarOverride {
        original_start: "2026-09-21T08:00:00Z".into(),
        event: None,
        native: None,
    }];
    let mut responses = read_steps(&master, &[]);
    responses.push(("GET", 200, json!({"items": [first, second]})));
    let (root, requests) = serve_occurrence_responses(responses).await;
    let client = services(&root).google_client("acc1").await.unwrap();
    assert!(
        update(&client, &account("calendar", "google"), &before, &desired)
            .await
            .is_err()
    );
    assert!(captured(requests)
        .await
        .iter()
        .all(|r| r.starts_with("GET ")));
}

#[tokio::test]
async fn exception_changed_after_preflight_does_not_gain_a_fresh_write_validator() {
    let master = master();
    let ex = exception("2026-09-21T08:00:00Z", "existing");
    let before = snapshot(&master, std::slice::from_ref(&ex));
    let mut desired = before.clone();
    desired.overrides[0].event.as_mut().unwrap().title = "Our edit".into();
    let mut responses = read_steps(&master, std::slice::from_ref(&ex));
    let mut concurrent = ex;
    concurrent["etag"] = json!("\"concurrent-v2\"");
    concurrent["summary"] = json!("Someone else's edit");
    responses.push(("GET", 200, json!({"items": [concurrent]})));
    let (root, requests) = serve_occurrence_responses(responses).await;
    let client = services(&root).google_client("acc1").await.unwrap();
    let error = update(&client, &account("calendar", "google"), &before, &desired)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("exception changed"));
    assert!(captured(requests)
        .await
        .iter()
        .all(|r| r.starts_with("GET ")));
}

#[tokio::test]
async fn shifted_master_replays_rebased_original_position_without_using_old_id() {
    let master = master();
    let mut ex = exception("2026-09-21T08:00:00Z", "old-id");
    ex["summary"] = json!("Retained exception");
    let before = snapshot(&master, std::slice::from_ref(&ex));
    let mut desired = before.clone();
    desired.event.start_time = "2026-09-14T09:00:00Z".into();
    desired.event.end_time = "2026-09-14T10:00:00Z".into();
    desired.overrides[0].original_start = "2026-09-21T09:00:00Z".into();
    let event = desired.overrides[0].event.as_mut().unwrap();
    event.start_time = "2026-09-21T09:00:00Z".into();
    event.end_time = "2026-09-21T10:00:00Z".into();
    let mut updated = master.clone();
    updated["start"]["dateTime"] = json!("2026-09-14T11:00:00+02:00");
    updated["end"]["dateTime"] = json!("2026-09-14T12:00:00+02:00");
    updated["etag"] = json!("\"shifted-v2\"");
    let generated = exception("2026-09-21T09:00:00Z", "new-opaque-id");
    let mut canonical_ex = generated.clone();
    canonical_ex["summary"] = json!("Retained exception");
    let mut responses = read_steps(&master, &[ex]);
    responses.push(("PATCH", 200, updated.clone()));
    responses.push(("GET", 200, json!({"items": [generated]})));
    responses.push(("PATCH", 200, canonical_ex.clone()));
    responses.extend(read_steps(&updated, &[canonical_ex]));
    let (root, requests) = serve_occurrence_responses(responses).await;
    let client = services(&root).google_client("acc1").await.unwrap();
    update(&client, &account("calendar", "google"), &before, &desired)
        .await
        .unwrap();
    let requests = captured(requests).await;
    assert!(requests[4].contains("originalStart=2026-09-21T09%3A00%3A00Z"));
    assert!(requests[5].contains("/events/new-opaque-id?"));
    assert!(!requests.iter().any(|r| r.contains("/events/old-id")));
}
