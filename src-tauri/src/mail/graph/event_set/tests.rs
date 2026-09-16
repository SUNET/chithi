use super::*;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn server(
    build: impl FnOnce(&str) -> Vec<(u16, Value)>,
) -> (GraphClient, tokio::task::JoinHandle<Vec<String>>) {
    let (root, task) = serve_json(build).await;
    let client = GraphClient::with_client(
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        "secret-test-token",
        super::super::GraphEndpoints::new(&root, &root),
    );
    (client, task)
}

pub(crate) async fn serve_json(
    build: impl FnOnce(&str) -> Vec<(u16, Value)>,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/graph", listener.local_addr().unwrap());
    let responses = build(&root);
    let result_root = root.clone();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        let mut posted: Option<Value> = None;
        for (status, body) in responses {
            let (mut socket, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8(bytes).unwrap();
            if request.starts_with("POST ") {
                posted = Some(self::body(&request));
            }
            requests.push(request);
            if status == 0 {
                continue;
            }
            let mut body = body;
            if let Some(values) = body["value"].as_array_mut() {
                for value in values {
                    if value.get("__creation").is_some() {
                        value.as_object_mut().unwrap().remove("__creation");
                        value["singleValueExtendedProperties"] =
                            posted.as_ref().unwrap()["singleValueExtendedProperties"].clone();
                        value["transactionId"] = posted.as_ref().unwrap()["transactionId"].clone();
                    }
                }
            }
            let body = if status == 204 {
                String::new()
            } else {
                body.to_string().replace("ROOT", &root)
            };
            socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    (result_root, task)
}

fn fixture(id: &str) -> Value {
    json!({"id": id, "@odata.etag": "W/\"one\"", "type": "singleInstance", "seriesMasterId": null, "recurrence": null,
        "isCancelled": false, "isAllDay": false, "subject": "Planning", "iCalUId": "uid",
        "body": {"contentType": "html", "content": "<p>Rich body</p>"}, "location": {"displayName": "Room", "uniqueId": "native-room"},
        "organizer": {"emailAddress": {"address": "owner@example.test"}}, "attendees": [{"emailAddress": {"address": "room@example.test", "name": "Room"}, "type": "resource", "status": {"response": "accepted"}}], "responseStatus": {"response": "organizer"},
        "start": {"dateTime": "2026-09-14T09:00:00", "timeZone": "UTC"}, "end": {"dateTime": "2026-09-14T10:00:00", "timeZone": "UTC"},
        "originalStartTimeZone": "Europe/Stockholm", "originalEndTimeZone": "Europe/Stockholm"})
}

fn master() -> Value {
    let mut value = fixture("master");
    value["type"] = json!("seriesMaster");
    value["recurrence"] = json!({"pattern": {"type": "daily", "interval": 1}, "range": {"type": "numbered", "numberOfOccurrences": 4, "startDate": "2026-09-14", "recurrenceTimeZone": "Europe/Stockholm"}});
    value["exceptionOccurrences"] = json!([]);
    value["cancelledOccurrences"] = json!([]);
    value
}

fn exception(id: &str, position: &str) -> Value {
    let mut value = fixture(id);
    value["type"] = json!("exception");
    value["seriesMasterId"] = json!("master");
    value["originalStart"] = json!(position);
    value
}

fn snapshot(value: &Value) -> CalendarEventSet {
    let template = crate::backend::testutil::event();
    CalendarEventSet {
        event: canonical(value, &template, "selected").unwrap(),
        overrides: vec![],
        native: Some(native(value, "selected").unwrap()),
        content: None,
    }
}

fn body(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

fn headers(requests: &[String]) {
    for request in requests {
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("authorization: bearer secret-test-token\r\n"));
        assert!(lower.contains("idtype=\"immutableid\""));
    }
}

#[tokio::test]
async fn standalone_read_whole_edit_noop_and_conditional_delete() {
    let source = fixture("event");
    let mut changed = source.clone();
    changed["subject"] = json!("Changed");
    changed["@odata.etag"] = json!("W/\"two\"");
    let (client, captured) = server(|_| {
        vec![
            (200, source.clone()),
            (204, Value::Null),
            (200, changed.clone()),
            (204, Value::Null),
        ]
    })
    .await;
    let before = client
        .fetch_calendar_event_set("selected", "event", &crate::backend::testutil::event())
        .await
        .unwrap();
    assert_eq!(before.event.timezone.as_deref(), Some("Europe/Stockholm"));
    let mut desired = before.clone();
    desired.event.title = "Changed".into();
    let updated = client
        .update_calendar_event_set("acc1", &before, &desired)
        .await
        .unwrap();
    assert_eq!(updated.event.title, "Changed");
    assert_eq!(
        client
            .update_calendar_event_set("acc1", &updated, &updated)
            .await
            .unwrap(),
        updated
    );
    client
        .delete_calendar_event_set("acc1", &updated)
        .await
        .unwrap();
    let requests = captured.await.unwrap();
    headers(&requests);
    assert_eq!(body(&requests[1]), json!({"subject": "Changed"}));
    assert!(requests[1].contains("if-match: W/\"one\""));
    assert!(requests[3].starts_with("DELETE /graph/me/calendars/selected/events/event "));
    assert!(requests[3].contains("if-match: W/\"two\""));
}

#[tokio::test]
async fn resolves_instance_to_master_and_reads_all_exception_pages_outside_view() {
    let mut master = master();
    master["exceptionOccurrences"] = json!([{"id": "far-away"}]);
    master["exceptionOccurrences@odata.nextLink"] =
        json!("ROOT/me/calendars/selected/events/master/exceptionOccurrences?$skip=1");
    let far = exception("far-away", "2026-09-15T09:00:00.0000000Z");
    let mut farther = exception("farther", "2026-09-16T09:00:00Z");
    farther["start"]["dateTime"] = json!("2028-09-16T09:00:00");
    farther["end"]["dateTime"] = json!("2028-09-16T10:00:00");
    let (client, captured) = server(|_| {
        vec![
            (200, far.clone()),
            (200, master.clone()),
            (200, json!({"value": [{"id": "farther"}]})),
            (200, far),
            (200, farther),
            (200, master),
        ]
    })
    .await;
    let set = client
        .fetch_calendar_event_set("selected", "far-away", &crate::backend::testutil::event())
        .await
        .unwrap();
    assert_eq!(set.event.remote_id.as_deref(), Some("master"));
    assert_eq!(set.overrides.len(), 2);
    assert_eq!(set.overrides[0].original_start, "2026-09-15T09:00:00Z");
    assert_eq!(
        set.overrides[1].event.as_ref().unwrap().start_time,
        "2028-09-16T09:00:00Z"
    );
    headers(&captured.await.unwrap());
}

#[tokio::test]
async fn incomplete_cancelled_snapshot_is_an_error_not_an_empty_set() {
    let mut source = master();
    source["cancelledOccurrences"] = json!(["OID.master.2026-09-15"]);
    let (client, captured) = server(|_| vec![(200, source.clone()), (200, source)]).await;
    let error = client
        .fetch_calendar_event_set("selected", "master", &crate::backend::testutil::event())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("PidLidAppointmentRecur"));
    assert_eq!(captured.await.unwrap().len(), 2);
}

#[tokio::test]
async fn untrusted_continuation_is_rejected_before_credentials_leave_origin() {
    let (client, captured) = server(|_| {
        vec![(
            200,
            json!({"value": [], "@odata.nextLink": "https://attacker.invalid/events"}),
        )]
    })
    .await;
    let page = client
        .calendar_set_request(
            Method::GET,
            "/me/calendars/selected/events",
            &[],
            None,
            None,
        )
        .await
        .unwrap();
    assert!(client
        .set_pages(page, "/me/calendars/selected/events")
        .await
        .unwrap_err()
        .to_string()
        .contains("untrusted"));
    assert_eq!(captured.await.unwrap().len(), 1);
}

#[test]
fn strict_canonical_validation_rejects_incomplete_and_malformed_fields() {
    for field in [
        "isCancelled",
        "isAllDay",
        "body",
        "attendees",
        "organizer",
        "originalStartTimeZone",
        "@odata.etag",
        "recurrence",
        "seriesMasterId",
    ] {
        let mut value = fixture("event");
        value.as_object_mut().unwrap().remove(field);
        assert!(
            canonical(&value, &crate::backend::testutil::event(), "selected").is_err(),
            "{field}"
        );
    }
    for cancelled in [Value::Null, json!("false"), json!(true), json!(0)] {
        let mut value = fixture("event");
        value["isCancelled"] = cancelled;
        assert!(canonical(&value, &crate::backend::testutil::event(), "selected").is_err());
    }
    let mut value = fixture("event");
    value["start"]["timeZone"] = json!("Unknown/Zone");
    assert!(canonical(&value, &crate::backend::testutil::event(), "selected").is_err());
}

#[test]
fn all_day_utc_response_restores_original_local_date() {
    let mut value = fixture("event");
    value["isAllDay"] = json!(true);
    value["start"]["dateTime"] = json!("2026-09-13T22:00:00");
    value["end"]["dateTime"] = json!("2026-09-14T22:00:00");
    let event = canonical(&value, &crate::backend::testutil::event(), "selected").unwrap();
    assert_eq!(event.start_time, "2026-09-14");
    assert_eq!(event.end_time, "2026-09-15");
    let created = super::super::event_to_graph_json(&event).unwrap();
    assert_eq!(
        created["start"],
        json!({"dateTime": "2026-09-14T00:00:00", "timeZone": "Europe/Stockholm"})
    );
    value["end"]["dateTime"] = json!("2026-09-14T23:00:00");
    assert!(canonical(&value, &event, "selected").is_err());
}

#[tokio::test]
async fn selected_calendar_copy_retains_html_and_resource_attendee() {
    let desired = snapshot(&fixture("foreign-source"));
    let (client, captured) = server(|_| {
        vec![
            (200, json!({"value": []})),
            (201, json!({"id": "destination"})),
            (200, fixture("destination")),
        ]
    })
    .await;
    let created = client
        .create_calendar_event_set(
            "destination-account",
            "chosen calendar",
            &desired,
            "persisted-op",
        )
        .await
        .unwrap();
    assert_eq!(created.event.account_id, "destination-account");
    assert_eq!(created.event.remote_id.as_deref(), Some("destination"));
    let requests = captured.await.unwrap();
    headers(&requests);
    assert!(requests[1].starts_with("POST /graph/me/calendars/chosen%20calendar/events "));
    let payload = body(&requests[1]);
    assert_eq!(payload["body"], fixture("source")["body"]);
    assert_eq!(payload["attendees"][0]["type"], "resource");
    assert!(payload["attendees"][0].get("status").is_none());
    assert!(payload.get("id").is_none());
    assert_eq!(payload["transactionId"].as_str().unwrap().len(), 64);
}

#[tokio::test]
async fn foreign_identity_and_crossing_boundary_fail_before_io() {
    let (client, captured) = server(|_| vec![]).await;
    let before = snapshot(&master());
    assert!(client
        .delete_calendar_event_set("other-account", &before)
        .await
        .is_err());
    let mut desired = before.clone();
    desired.native.as_mut().unwrap().event_id = "wrong".into();
    assert!(client
        .update_calendar_event_set("acc1", &before, &desired)
        .await
        .is_err());
    desired = before.clone();
    let mut event = before.event.clone();
    event.recurrence_kind = RecurrenceKind::Occurrence;
    event.recurrence_rule = None;
    event.start_time = "2026-09-16T10:00:00Z".into();
    event.end_time = "2026-09-16T11:00:00Z".into();
    desired.overrides.push(CalendarOverride {
        original_start: "2026-09-15T09:00:00Z".into(),
        event: Some(event),
        native: None,
    });
    assert!(client
        .update_calendar_event_set("acc1", &before, &desired)
        .await
        .unwrap_err()
        .to_string()
        .contains("ErrorOccurrenceCrossingBoundary"));
    assert!(captured.await.unwrap().is_empty());
}

fn cancellation_blob(deleted: &[u32], modified: &[(u32, u32)]) -> Vec<u8> {
    let epoch = NaiveDate::from_ymd_opt(1601, 1, 1).unwrap();
    let start = NaiveDate::from_ymd_opt(2026, 9, 14)
        .unwrap()
        .signed_duration_since(epoch)
        .num_minutes() as u32;
    let mut bytes = Vec::new();
    for n in [0x3004_u16, 0x3004, 0x200a, 0, 0] {
        bytes.extend(n.to_le_bytes());
    }
    for n in [0_u32, 1440, 0, 0x2022, 4, 1, deleted.len() as u32] {
        bytes.extend(n.to_le_bytes());
    }
    for n in deleted {
        bytes.extend((start + n * 1440).to_le_bytes());
    }
    bytes.extend((modified.len() as u32).to_le_bytes());
    for (_, effective) in modified {
        bytes.extend((start + effective * 1440).to_le_bytes());
    }
    for n in [start, start + 3 * 1440, 0x3006, 0x3009, 660, 720] {
        bytes.extend(n.to_le_bytes());
    }
    bytes.extend((modified.len() as u16).to_le_bytes());
    for (original, effective) in modified {
        for n in [
            start + effective * 1440 + 660,
            start + effective * 1440 + 720,
            start + original * 1440 + 660,
        ] {
            bytes.extend(n.to_le_bytes());
        }
        bytes.extend(0_u16.to_le_bytes());
    }
    bytes.extend(0_u32.to_le_bytes());
    for _ in modified {
        for n in [4_u32, 0, 0] {
            bytes.extend(n.to_le_bytes());
        }
    }
    bytes.extend(0_u32.to_le_bytes());
    bytes
}

fn with_cancellations(mut master: Value, deleted: &[u32], modified: &[(u32, u32)]) -> Value {
    master["singleValueExtendedProperties"] = json!([{"id": RECUR_PROPERTY, "value": base64::engine::general_purpose::STANDARD.encode(cancellation_blob(deleted, modified))}]);
    master["cancelledOccurrences"] = json!(["completely-opaque-cancellation"]);
    master
}

#[test]
fn documented_blob_rejects_every_truncation_and_recovers_original_deleted_slots() {
    let event = snapshot(&master()).event;
    let bytes = cancellation_blob(&[1, 2], &[(1, 3)]);
    let modified = CalendarOverride {
        original_start: "2026-09-15T09:00:00Z".into(),
        event: Some(event.clone()),
        native: None,
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    assert_eq!(
        recurrence_blob::deleted_positions(&encoded, &event, std::slice::from_ref(&modified))
            .unwrap(),
        ["2026-09-16T09:00:00Z"]
    );
    for end in 0..bytes.len() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes[..end]);
        assert!(
            recurrence_blob::deleted_positions(&encoded, &event, std::slice::from_ref(&modified))
                .is_err(),
            "truncated at {end}"
        );
    }
    for offset in [0, 2, 4, 6, 8, 14, 18, 22, 26, 30, 34] {
        let mut corrupt = bytes.clone();
        corrupt[offset] = 255;
        let encoded = base64::engine::general_purpose::STANDARD.encode(corrupt);
        assert!(
            recurrence_blob::deleted_positions(&encoded, &event, std::slice::from_ref(&modified))
                .is_err(),
            "corrupt at {offset}"
        );
    }
}

#[tokio::test]
async fn complete_cancellation_read_uses_legacy_slots_not_opaque_id_suffixes() {
    let source = with_cancellations(master(), &[2], &[]);
    let (client, captured) =
        server(|_| vec![(200, source.clone()), (200, source.clone()), (200, source)]).await;
    let set = client
        .fetch_calendar_event_set("selected", "master", &crate::backend::testutil::event())
        .await
        .unwrap();
    assert_eq!(set.overrides.len(), 1);
    assert_eq!(set.overrides[0].original_start, "2026-09-16T09:00:00Z");
    assert!(set.overrides[0].event.is_none());
    let requests = captured.await.unwrap();
    headers(&requests);
    assert!(requests[1].contains("singleValueExtendedProperties"));
}

#[tokio::test]
async fn single_generated_edit_looks_up_original_start_and_writes_only_title() {
    let before = snapshot(&master());
    let mut desired = before.clone();
    let mut occurrence = exception("occurrence", "2026-09-15T09:00:00Z");
    occurrence["type"] = json!("occurrence");
    occurrence["start"]["dateTime"] = json!("2026-09-15T09:00:00");
    occurrence["end"]["dateTime"] = json!("2026-09-15T10:00:00");
    let mut edited = occurrence.clone();
    edited["type"] = json!("exception");
    edited["subject"] = json!("One only");
    edited["@odata.etag"] = json!("W/\"two\"");
    desired.overrides.push(CalendarOverride {
        original_start: "2026-09-15T09:00:00Z".into(),
        event: Some(canonical(&edited, &before.event, "selected").unwrap()),
        native: None,
    });
    let mut after = master();
    after["exceptionOccurrences"] = json!([{"id": "occurrence"}]);
    after["@odata.etag"] = json!("W/\"two\"");
    let (client, captured) = server(|_| {
        vec![
            (200, json!({"value": [occurrence]})),
            (204, Value::Null),
            (200, after.clone()),
            (200, after.clone()),
            (200, edited),
            (200, after),
        ]
    })
    .await;
    let updated = client
        .update_calendar_event_set("acc1", &before, &desired)
        .await
        .unwrap();
    assert_eq!(
        updated.overrides[0].event.as_ref().unwrap().title,
        "One only"
    );
    let requests = captured.await.unwrap();
    headers(&requests);
    assert!(requests[0].starts_with("GET /graph/me/calendars/selected/events/master/instances?"));
    assert!(requests[0].contains("2026-09-13"));
    assert_eq!(body(&requests[1]), json!({"subject": "One only"}));
    assert!(requests[1].contains("if-match: W/\"one\""));
}

#[tokio::test]
async fn cancel_generated_occurrence_and_delete_source_are_conditional() {
    let before = snapshot(&master());
    let mut desired = before.clone();
    desired.overrides.push(CalendarOverride {
        original_start: "2026-09-16T09:00:00Z".into(),
        event: None,
        native: None,
    });
    let mut occurrence = exception("occurrence", "2026-09-16T09:00:00Z");
    occurrence["type"] = json!("occurrence");
    let mut after = with_cancellations(master(), &[2], &[]);
    after["@odata.etag"] = json!("W/\"two\"");
    let (client, captured) = server(|_| {
        vec![
            (200, json!({"value": [occurrence]})),
            (204, Value::Null),
            (200, after.clone()),
            (200, after.clone()),
            (200, after),
            (412, json!({"error": {"code": "ErrorIrresolvableConflict"}})),
        ]
    })
    .await;
    let updated = client
        .update_calendar_event_set("acc1", &before, &desired)
        .await
        .unwrap();
    assert!(updated.overrides[0].event.is_none());
    assert!(client
        .delete_calendar_event_set("acc1", &updated)
        .await
        .unwrap_err()
        .to_string()
        .contains("412"));
    let requests = captured.await.unwrap();
    headers(&requests);
    assert!(requests[1].starts_with("DELETE /graph/me/calendars/selected/events/occurrence "));
    assert!(requests[1].contains("if-match: W/\"one\""));
    assert!(requests[5].contains("if-match: W/\"two\""));
}

#[tokio::test]
async fn lost_create_response_reconciles_verified_tags_without_second_post() {
    let desired = snapshot(&fixture("foreign-source"));
    let (client, captured) = server(|_| {
        vec![
            (200, json!({"value": []})),
            (0, Value::Null),
            (
                200,
                json!({"value": [{"id": "created", "__creation": true}]}),
            ),
            (200, fixture("created")),
        ]
    })
    .await;
    let created = client
        .create_calendar_event_set("acc1", "selected", &desired, "lost-create-operation")
        .await
        .unwrap();
    assert_eq!(created.event.remote_id.as_deref(), Some("created"));
    let requests = captured.await.unwrap();
    headers(&requests);
    assert_eq!(
        requests.iter().filter(|r| r.starts_with("POST ")).count(),
        1
    );
    assert!(requests[2].contains("ChithiCalendarOperation"));
}

#[tokio::test]
async fn malformed_creation_preflight_never_posts() {
    for response in [
        json!({}),
        json!({"value": {}}),
        json!({"value": [{"id": "one"}]}),
        json!({"value": [], "@odata.nextLink": "https://attacker.invalid"}),
    ] {
        let (client, captured) = server(|_| vec![(200, response)]).await;
        assert!(client
            .create_calendar_event_set("acc1", "selected", &snapshot(&fixture("source")), "op")
            .await
            .is_err());
        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET "));
    }
}

#[tokio::test]
async fn creates_complete_series_with_live_override_and_exclusion() {
    let source = master();
    let mut desired = snapshot(&source);
    let mut occurrence = exception("instance-live", "2026-09-15T09:00:00Z");
    occurrence["type"] = json!("occurrence");
    occurrence["start"]["dateTime"] = json!("2026-09-15T09:00:00");
    occurrence["end"]["dateTime"] = json!("2026-09-15T10:00:00");
    let mut edited = occurrence.clone();
    edited["type"] = json!("exception");
    edited["subject"] = json!("Copied exception");
    edited["@odata.etag"] = json!("W/\"two\"");
    desired.overrides.push(CalendarOverride {
        original_start: "2026-09-15T09:00:00Z".into(),
        event: Some(canonical(&edited, &desired.event, "selected").unwrap()),
        native: Some(native(&edited, "foreign-calendar").unwrap()),
    });
    desired.overrides.push(CalendarOverride {
        original_start: "2026-09-16T09:00:00Z".into(),
        event: None,
        native: None,
    });
    let mut after_live = master();
    after_live["@odata.etag"] = json!("W/\"two\"");
    after_live["exceptionOccurrences"] = json!([{"id": "instance-live"}]);
    let mut after_delete = with_cancellations(after_live.clone(), &[1, 2], &[(1, 1)]);
    after_delete["@odata.etag"] = json!("W/\"three\"");
    let mut cancelled_target = exception("instance-delete", "2026-09-16T09:00:00Z");
    cancelled_target["type"] = json!("occurrence");
    let (client, captured) = server(|_| {
        vec![
            (200, json!({"value": []})),
            (201, json!({"id": "master"})),
            (200, source.clone()),
            (200, source.clone()),
            (200, source),
            (200, json!({"value": [occurrence]})),
            (204, Value::Null),
            (200, after_live.clone()),
            (200, after_live.clone()),
            (200, edited.clone()),
            (200, after_live),
            (200, json!({"value": [cancelled_target]})),
            (204, Value::Null),
            (200, after_delete.clone()),
            (200, after_delete.clone()),
            (200, edited),
            (200, after_delete),
        ]
    })
    .await;
    let result = client
        .create_calendar_event_set("acc1", "destination", &desired, "copy-series")
        .await
        .unwrap();
    assert_eq!(result.overrides.len(), 2);
    assert_eq!(
        result.overrides[0].event.as_ref().unwrap().title,
        "Copied exception"
    );
    assert!(result.overrides[1].event.is_none());
    let requests = captured.await.unwrap();
    headers(&requests);
    assert_eq!(
        requests.iter().filter(|r| r.starts_with("POST ")).count(),
        1
    );
    assert_eq!(body(&requests[6]), json!({"subject": "Copied exception"}));
    assert!(
        requests[12].starts_with("DELETE /graph/me/calendars/destination/events/instance-delete ")
    );
    assert!(requests.iter().all(|r| !r.contains("/foreign-calendar/")));
}

#[tokio::test]
async fn whole_rule_change_replays_complete_desired_override_after_master_reset() {
    let mut before = snapshot(&master());
    let old = exception("old-instance", "2026-09-15T09:00:00Z");
    before.overrides.push(CalendarOverride {
        original_start: "2026-09-15T09:00:00Z".into(),
        event: Some(canonical(&old, &before.event, "selected").unwrap()),
        native: Some(native(&old, "selected").unwrap()),
    });
    let mut desired = before.clone();
    desired.event.recurrence_rule = Some("FREQ=WEEKLY;COUNT=4".into());
    desired.overrides[0].original_start = "2026-09-21T09:00:00Z".into();
    let event = desired.overrides[0].event.as_mut().unwrap();
    event.title = "Retained exception".into();
    event.start_time = "2026-09-21T12:00:00Z".into();
    event.end_time = "2026-09-21T13:00:00Z".into();
    let mut reset = master();
    reset["recurrence"]["pattern"] = json!({"type": "weekly", "interval": 1, "daysOfWeek": ["monday"], "firstDayOfWeek": "monday"});
    reset["@odata.etag"] = json!("W/\"reset\"");
    let mut generated = exception("new-instance", "2026-09-21T09:00:00Z");
    generated["type"] = json!("occurrence");
    generated["start"]["dateTime"] = json!("2026-09-21T09:00:00");
    generated["end"]["dateTime"] = json!("2026-09-21T10:00:00");
    let mut replayed = generated.clone();
    replayed["type"] = json!("exception");
    replayed["subject"] = json!("Retained exception");
    replayed["start"]["dateTime"] = json!("2026-09-21T12:00:00");
    replayed["end"]["dateTime"] = json!("2026-09-21T13:00:00");
    let mut after = reset.clone();
    after["exceptionOccurrences"] = json!([{"id": "new-instance"}]);
    after["@odata.etag"] = json!("W/\"replayed\"");
    let (client, captured) = server(|_| {
        vec![
            (204, Value::Null),
            (200, reset.clone()),
            (200, reset.clone()),
            (200, reset),
            (200, json!({"value": [generated]})),
            (204, Value::Null),
            (200, after.clone()),
            (200, after.clone()),
            (200, replayed),
            (200, after),
        ]
    })
    .await;
    let updated = client
        .update_calendar_event_set("acc1", &before, &desired)
        .await
        .unwrap();
    assert_eq!(
        updated.overrides[0].event.as_ref().unwrap().title,
        "Retained exception"
    );
    let requests = captured.await.unwrap();
    headers(&requests);
    assert_eq!(body(&requests[0]).as_object().unwrap().len(), 1);
    assert_eq!(
        body(&requests[0])["recurrence"]["pattern"]["type"],
        "weekly"
    );
    let replay = body(&requests[5]);
    assert_eq!(replay["body"]["contentType"], "html");
    assert_eq!(replay["attendees"][0]["type"], "resource");
    assert!(replay.get("recurrence").is_none());
    assert!(requests[5].starts_with("PATCH /graph/me/calendars/selected/events/new-instance "));
    assert!(requests.iter().all(|r| !r.contains("/old-instance ")));
}

#[test]
fn editor_recurrence_patterns_preserve_interval_days_and_until_local_date() {
    let mut event = snapshot(&master()).event;
    for (rule, expected_type) in [
        ("FREQ=DAILY;INTERVAL=2;COUNT=3", "daily"),
        (
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;WKST=SU;COUNT=5",
            "weekly",
        ),
        ("FREQ=MONTHLY;COUNT=3", "absoluteMonthly"),
        ("FREQ=YEARLY;INTERVAL=2;COUNT=3", "absoluteYearly"),
    ] {
        event.recurrence_rule = Some(rule.into());
        let value = super::super::event_to_graph_json(&event).unwrap();
        assert_eq!(value["recurrence"]["pattern"]["type"], expected_type);
        if expected_type == "weekly" {
            assert_eq!(value["recurrence"]["pattern"]["firstDayOfWeek"], "sunday");
            assert_eq!(
                value["recurrence"]["pattern"]["daysOfWeek"],
                json!(["monday", "wednesday"])
            );
        }
    }
    event.recurrence_rule = Some("FREQ=DAILY;UNTIL=20260916T085959Z".into());
    assert_eq!(
        super::super::event_to_graph_json(&event).unwrap()["recurrence"]["range"]["endDate"],
        "2026-09-15"
    );
    event.recurrence_rule = Some("FREQ=DAILY;UNTIL=20260916".into());
    assert_eq!(
        super::super::event_to_graph_json(&event).unwrap()["recurrence"]["range"]["endDate"],
        "2026-09-16"
    );
}

#[tokio::test]
async fn default_utc_creation_verifies_plain_text_without_losing_native_html() {
    let mut remote = fixture("created");
    remote["originalStartTimeZone"] = json!("UTC");
    remote["originalEndTimeZone"] = json!("UTC");
    remote["attendees"] = json!([]);
    remote["location"] = json!({"displayName": "", "locationType": "default"});
    remote["body"] =
        json!({"contentType": "html", "content": "<html><body>Plain text</body></html>"});
    let mut desired = snapshot(&remote);
    desired.native = None;
    desired.event.remote_id = None;
    desired.event.etag = None;
    desired.event.timezone = None;
    desired.event.description = Some("Plain text".into());
    desired.event.attendees_json = None;
    desired.event.location = None;
    let (client, captured) = server(|_| vec![(200, json!({"value": []})), (201, json!({"id": "created"})), (200, remote.clone()), (200, json!({"id": "created", "@odata.etag": "W/\"one\"", "body": {"contentType": "text", "content": "Plain text"}}))]).await;
    let result = client
        .create_calendar_event_set("acc1", "selected", &desired, "plain-create")
        .await
        .unwrap();
    assert_eq!(result.event.timezone.as_deref(), Some("UTC"));
    assert_eq!(
        result.event.description.as_deref(),
        remote["body"]["content"].as_str()
    );
    let requests = captured.await.unwrap();
    headers(&requests);
    assert_eq!(requests.len(), 4);
    assert!(requests[3].contains("outlook.body-content-type=\"text\""));
}

#[test]
fn ambiguous_wall_time_is_rejected_instead_of_losing_the_explicit_fold() {
    let mut event = snapshot(&fixture("source")).event;
    event.timezone = Some("America/New_York".into());
    event.start_time = "2026-11-01T06:30:00Z".into();
    event.end_time = "2026-11-01T07:30:00Z".into();
    assert!(super::super::event_to_graph_json(&event)
        .unwrap_err()
        .to_string()
        .contains("ambiguous"));
    event.timezone = Some("UTC".into());
    assert_eq!(
        super::super::event_to_graph_json(&event).unwrap()["start"]["dateTime"],
        "2026-11-01T06:30:00"
    );
}
