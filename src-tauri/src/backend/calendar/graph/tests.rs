use super::*;
use crate::backend::calendar::google::sync_testutil::{
    cache_event, serve_create_response, serve_patch_response, services, setup_db,
};
use crate::backend::testutil::{account, event};
use crate::calendar::recurrence_identity::{
    OccurrenceFields, RecurrenceIdentity, UpdateOccurrenceInput,
};
use crate::mail::graph::event_set::tests::{master, serve_json};
use serde_json::json;

fn occurrence_request() -> RemoteOccurrenceUpdate {
    let fields = OccurrenceFields {
        title: "Occurrence".into(),
        description: Some("Description".into()),
        location: Some("Room".into()),
        start_time: "2026-09-15T10:00:00Z".into(),
        end_time: "2026-09-15T11:00:00Z".into(),
        all_day: false,
        timezone: Some("UTC".into()),
    };
    let mut current = event();
    current.remote_id = Some("immutable-occurrence".into());
    current.recurrence_kind = RecurrenceKind::Occurrence;
    RemoteOccurrenceUpdate {
        target_id: "immutable-occurrence".into(), expected_provider_revision: Some("etag-1".into()),
        trusted_identity: RecurrenceIdentity { object_id: "object".into(), account_id: "acc1".into(), event_id: current.id.clone(),
            local_series_event_id: None, provider_calendar_id: Some("provider-calendar".into()), provider_series_id: Some("immutable-master".into()),
            provider_occurrence_id: Some("immutable-occurrence".into()), recurrence_id: Some("2026-09-15T09:00:00.0000000Z".into()),
            recurrence_timezone: Some("UTC".into()), recurrence_value_type: Some(RecurrenceValueType::DateTime), occurrence: fields.clone(),
            provider_native_data: Some(json!({"id":"immutable-occurrence", "type":"occurrence", "seriesMasterId":"immutable-master",
                "originalStart":"2026-09-15T09:00:00.0000000Z", "@odata.etag":"etag-1"}).to_string()),
            provider_revision: Some("etag-1".into()), kind: RecurrenceObjectKind::Occurrence },
        current_event: current, patch: UpdateOccurrenceInput::default(), desired: fields,
    }
}

#[test]
fn occurrence_write_requires_an_odata_etag() {
    let mut request = occurrence_request();
    request.expected_provider_revision = None;
    request.trusted_identity.provider_revision = None;
    assert!(matches!(
        validate_occurrence_update(&account("calendar", "graph"), &request),
        Err(Error::UnsupportedCapability { .. })
    ));
}

#[test]
fn occurrence_write_uses_the_provider_calendar_not_the_local_uuid() {
    let mut request = occurrence_request();
    request.current_event.calendar_id = "local-calendar-uuid".into();
    assert_eq!(
        validate_occurrence_update(&account("calendar", "graph"), &request)
            .unwrap()
            .0,
        "provider-calendar"
    );
}

#[test]
fn occurrence_write_rejects_any_native_identity_mismatch() {
    for field in [
        "id",
        "type",
        "seriesMasterId",
        "originalStart",
        "@odata.etag",
    ] {
        let mut request = occurrence_request();
        let mut native: serde_json::Value = serde_json::from_str(
            request
                .trusted_identity
                .provider_native_data
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        native[field] = json!("different");
        request.trusted_identity.provider_native_data = Some(native.to_string());
        assert!(validate_occurrence_update(&account("calendar", "graph"), &request).is_err());
    }
}

#[tokio::test]
async fn publishes_confirmed_standalone_creation() {
    for rule in [None, Some("")] {
        let (_dir, db) = setup_db().await;
        let (root, captured) = serve_create_response(
            json!({"id":"created-event", "iCalUId":"canonical@example.test"}),
        )
        .await;
        let providers = services(&root);
        let mut source = event();
        source.start_time = "2026-09-14T09:00:00Z".into();
        source.end_time = "2026-09-14T10:00:00Z".into();
        source.recurrence_rule = rule.map(str::to_owned);
        let created = GraphCalendarBackend
            .push_created_event(
                &CalendarBackendCtx {
                    db: &db,
                    services: &providers,
                },
                &account("calendar", "graph"),
                &source,
                "primary",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(created.remote_id, "created-event");
        assert_eq!(
            created.canonical_uid.as_deref(),
            Some("canonical@example.test")
        );
        let requests = captured.await.unwrap();
        assert!(
            requests[0].starts_with("POST /calendar-api/me/calendars/primary/events HTTP/1.1\r\n")
        );
        let payload: serde_json::Value =
            serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert!(payload.get("recurrence").is_none());
        assert_eq!(payload["subject"], source.title);
    }
}

fn recurring_source() -> CalendarEvent {
    let mut source = event();
    source.start_time = "2026-09-14T09:00:00Z".into();
    source.end_time = "2026-09-14T10:00:00Z".into();
    source.timezone = Some("Europe/Stockholm".into());
    source.recurrence_kind = RecurrenceKind::Series;
    source.recurrence_rule = Some("FREQ=WEEKLY;BYDAY=MO;COUNT=3".into());
    source.ical_data = Some("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:series@example.test\r\nRRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=3\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".into());
    source
}

#[tokio::test]
async fn publishes_supported_recurrence_in_the_graph_request() {
    let (_dir, db) = setup_db().await;
    let (root, captured) =
        serve_create_response(json!({"id":"created-series", "iCalUId":"series@example.test"}))
            .await;
    let providers = services(&root);
    GraphCalendarBackend
        .push_created_event(
            &CalendarBackendCtx {
                db: &db,
                services: &providers,
            },
            &account("calendar", "graph"),
            &recurring_source(),
            "selected",
        )
        .await
        .unwrap();
    let requests = captured.await.unwrap();
    let payload: serde_json::Value =
        serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(payload["recurrence"]["pattern"]["type"], "weekly");
    assert_eq!(payload["recurrence"]["range"]["numberOfOccurrences"], 3);
}

#[tokio::test]
async fn personal_copy_update_preserves_recurrence_without_scheduling_guests() {
    let (_dir, db) = setup_db().await;
    let (root, captured) = serve_patch_response(json!({})).await;
    let providers = services(&root);
    let mut source = recurring_source();
    source.title = "Changed".into();
    source.description = None;
    source.location = None;
    source.organizer_email = Some("organizer@example.test".into());
    source.attendees_json =
        Some(json!([{"email":"guest@example.test", "status":"accepted"}]).to_string());
    GraphCalendarBackend
        .push_updated_invitation_copy(
            &CalendarBackendCtx {
                db: &db,
                services: &providers,
            },
            &account("calendar", "graph"),
            "remote-series",
            &source,
        )
        .await
        .unwrap();
    let requests = captured.await.unwrap();
    let payload: serde_json::Value =
        serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(payload["subject"], "Changed");
    assert_eq!(payload["body"]["content"], "");
    assert_eq!(payload["location"]["displayName"], "");
    assert_eq!(payload["start"]["timeZone"], "Europe/Stockholm");
    assert_eq!(payload["recurrence"]["pattern"]["type"], "weekly");
    assert!(payload.get("organizer").is_none());
    assert!(payload.get("attendees").is_none());
}

#[tokio::test]
async fn personal_copy_update_explicitly_removes_recurrence() {
    let (_dir, db) = setup_db().await;
    let (root, captured) = serve_patch_response(json!({})).await;
    let providers = services(&root);
    let mut source = event();
    source.recurrence_kind = RecurrenceKind::Standalone;
    source.recurrence_rule = None;
    source.attendees_json = Some(json!([{"email":"guest@example.test"}]).to_string());
    GraphCalendarBackend
        .push_updated_invitation_copy(
            &CalendarBackendCtx {
                db: &db,
                services: &providers,
            },
            &account("calendar", "graph"),
            "remote-event",
            &source,
        )
        .await
        .unwrap();
    let requests = captured.await.unwrap();
    let payload: serde_json::Value =
        serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert!(payload["recurrence"].is_null());
    assert!(payload.get("attendees").is_none());
}

fn standalone() -> serde_json::Value {
    let mut value = master();
    value["type"] = json!("singleInstance");
    value["recurrence"] = serde_json::Value::Null;
    value
}

#[tokio::test]
async fn evidenced_boundary_zones_and_legacy_creation_metadata_publish_without_errors() {
    let (_dir, db) = setup_db().await;
    let mut series = master();
    series["isAllDay"] = json!(true);
    series["start"] = json!({"dateTime":"1978-11-03T00:00:00.0000000", "timeZone":"UTC"});
    series["end"] = json!({"dateTime":"1978-11-04T00:00:00.0000000", "timeZone":"UTC"});
    series["originalStartTimeZone"] = json!("UTC");
    series["originalEndTimeZone"] = json!("UTC");
    series["recurrence"] = json!({"pattern":{"type":"absoluteYearly", "interval":1, "month":11, "dayOfMonth":3}, "range":{"type":"noEnd", "startDate":"1978-11-03", "recurrenceTimeZone":"W. Europe Standard Time"}});
    let mut ordinary = standalone();
    ordinary["id"] = json!("custom-created");
    ordinary["start"] = json!({"dateTime":"2026-05-27T08:00:00.0000000", "timeZone":"UTC"});
    ordinary["end"] = json!({"dateTime":"2026-05-27T09:00:00.0000000", "timeZone":"UTC"});
    ordinary["originalStartTimeZone"] = json!("tzone://Microsoft/Custom");
    ordinary["originalEndTimeZone"] = json!("tzone://Microsoft/Custom");
    let (root, captured) = serve_json(|_| vec![
        (200, json!({"value":[{"id":"primary", "name":"Calendar", "isDefaultCalendar":true, "color":"auto"}]})),
        (200, json!({"value":[series.clone(), ordinary]})),
        (200, series.clone()), (200, series.clone()), (200, series),
    ]).await;
    let providers = services(&root);
    GraphCalendarBackend
        .sync(
            &CalendarBackendCtx {
                db: &db,
                services: &providers,
            },
            &account("calendar", "graph"),
        )
        .await
        .unwrap();
    assert_eq!(captured.await.unwrap().len(), 5);
    let conn = db.reader();
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM calendar_remote_resources WHERE set_data IS NOT NULL AND error IS NULL", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
    let (start, end): (String, String) = conn
        .query_row(
            "SELECT start_time, end_time FROM calendar_events WHERE remote_id = 'master'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((start.as_str(), end.as_str()), ("1978-11-03", "1978-11-04"));
    let start: String = conn
        .query_row(
            "SELECT start_time FROM calendar_events WHERE remote_id = 'custom-created'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(start, "2026-05-27T08:00:00Z");
}

#[tokio::test]
async fn display_only_graph_resources_replace_stale_rows_and_keep_genuine_errors_isolated() {
    let (_dir, db) = setup_db().await;
    {
        let conn = db.writer().await;
        cache_event(&conn, "old", Some("obsolete"));
    }
    let mut series = master();
    series["subject"] = serde_json::Value::Null;
    series["recurrence"]["pattern"] = json!({"type":"relativeMonthly", "interval":1, "daysOfWeek":["thursday"], "index":"second"});
    let mut ordinary = standalone();
    ordinary["id"] = json!("untitled");
    ordinary.as_object_mut().unwrap().remove("subject");
    let mut invalid = ordinary.clone();
    invalid["id"] = json!("malformed-title");
    invalid["subject"] = json!(42);
    let (root, captured) = serve_json(|_| vec![
        (200, json!({"value":[{"id":"primary", "name":"Calendar", "isDefaultCalendar":true, "color":"auto"}]})),
        (200, json!({"value":[series.clone(), ordinary, invalid]})),
        (200, series.clone()),
        (200, series.clone()),
        (200, series),
    ]).await;
    let providers = services(&root);
    let result = GraphCalendarBackend
        .sync(
            &CalendarBackendCtx {
                db: &db,
                services: &providers,
            },
            &account("calendar", "graph"),
        )
        .await;
    assert!(
        matches!(result, Err(Error::Sync(ref error)) if error.contains("malformed-title") && !error.contains("lossless subset")),
        "{result:?}"
    );
    let requests = captured.await.unwrap();
    assert_eq!(requests.len(), 5);
    assert!(requests.iter().all(|request| request.starts_with("GET ")));
    let conn = db.reader();
    assert!(db::calendar::get_event(&conn, "old").is_err());
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM calendar_remote_resources WHERE set_data IS NOT NULL AND error IS NULL", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM calendar_remote_resources WHERE set_data IS NULL AND error IS NOT NULL", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM calendar_events WHERE title = ''",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    let (start, rule): (String, String) = conn
        .query_row(
            "SELECT start_time, recurrence_rule FROM calendar_events WHERE remote_id = 'master'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(start, "2026-10-08T09:00:00Z");
    assert!(rule.contains("BYDAY=TH;BYSETPOS=2"));
}

#[tokio::test]
async fn verified_odata_continuation_publishes_complete_mirror_but_wrong_series_rolls_back() {
    for valid in [true, false] {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_event(&conn, "previous", Some("old-event"));
        }
        let mut series = master();
        let next_series = if valid { "master" } else { "another-series" };
        series["exceptionOccurrences@odata.nextLink"] = json!(format!(
            "ROOT/users('signed-in-user')/events('{next_series}')/exceptionOccurrences?$skiptoken=opaque"
        ));
        let mut ordinary = standalone();
        ordinary["id"] = json!("ordinary");
        let mut child = standalone();
        child["id"] = json!("exception");
        child["type"] = json!("exception");
        child["seriesMasterId"] = json!("master");
        child["originalStart"] = json!("2026-09-15T09:00:00Z");
        child["start"]["dateTime"] = json!("2026-09-15T10:00:00");
        child["end"]["dateTime"] = json!("2026-09-15T11:00:00");
        let (root, captured) = serve_json(|_| {
            let mut responses = vec![
                (200, json!({"value":[{"id":"primary", "name":"Calendar", "isDefaultCalendar":true, "color":"auto"}]})),
                (200, json!({"value":[series.clone(), ordinary]})),
                (200, series.clone()),
                (200, series.clone()),
            ];
            if valid {
                responses.extend([
                    (200, json!({"id":"signed-in-user", "userPrincipalName":"owner@example.test"})),
                    (200, json!({"value":[{"id":"exception"}]})),
                    (200, child),
                    (200, series),
                ]);
            }
            responses
        }).await;
        let providers = services(&root);
        let result = GraphCalendarBackend
            .sync(
                &CalendarBackendCtx {
                    db: &db,
                    services: &providers,
                },
                &account("calendar", "graph"),
            )
            .await;
        let requests = captured.await.unwrap();
        let conn = db.reader();
        let resources: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM calendar_remote_resources",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if valid {
            result.unwrap();
            assert_eq!(resources, 2);
            assert!(db::calendar::get_event(&conn, "previous").is_err());
            assert_eq!(requests.len(), 8);
            assert!(requests[5].starts_with("GET /graph/users('signed-in-user')/events('master')/exceptionOccurrences?$skiptoken=opaque "));
        } else {
            assert!(
                matches!(result, Err(Error::CalendarReadIncomplete(_))),
                "{result:?}"
            );
            assert_eq!(resources, 0);
            assert!(db::calendar::get_event(&conn, "previous").is_ok());
            assert_eq!(requests.len(), 4);
        }
        assert!(requests.iter().all(|request| request.starts_with("GET ")));
    }
}

#[tokio::test]
async fn incomplete_series_read_cannot_remove_any_previous_projection() {
    let (_dir, db) = setup_db().await;
    {
        let conn = db.writer().await;
        cache_event(&conn, "keep", Some("previous"));
    }
    let mut series = master();
    series
        .as_object_mut()
        .unwrap()
        .remove("exceptionOccurrences");
    let (root, captured) = serve_json(|_| vec![
        (200, json!({"value":[{"id":"primary", "name":"Calendar", "isDefaultCalendar":true, "color":"auto"}]})),
            (200, json!({"value":[series.clone()]})),
            (200, series.clone()),
            (200, series),
    ]).await;
    let providers = services(&root);
    let result = GraphCalendarBackend
        .sync(
            &CalendarBackendCtx {
                db: &db,
                services: &providers,
            },
            &account("calendar", "graph"),
        )
        .await;
    assert!(
        matches!(result, Err(Error::CalendarReadIncomplete(_))),
        "{result:?}"
    );
    assert_eq!(captured.await.unwrap().len(), 4);
    assert!(db::calendar::get_event(&db.reader(), "keep").is_ok());
    assert_eq!(
        db.reader()
            .query_row(
                "SELECT COUNT(*) FROM calendar_remote_resources",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn mirror_replaces_legacy_rows_and_ignores_claims_without_remote_writes() {
    let (_dir, db) = setup_db().await;
    {
        let conn = db.writer().await;
        cache_event(&conn, "stale", Some("obsolete"));
        cache_event(&conn, "unpublished", None);
        conn.execute("INSERT INTO calendar_action_operations(operation_id, account_id, event_id, data) VALUES ('pending', 'acc1', 'stale', '{}')", []).unwrap();
        conn.execute("INSERT INTO calendar_action_claims(event_id, operation_id) VALUES ('stale', 'pending')", []).unwrap();
    }
    let value = standalone();
    let (root, captured) = serve_json(|_| vec![(200, json!({"value":[{"id":"primary", "name":"Calendar", "isDefaultCalendar":true, "color":"auto"}]})), (200, json!({"value":[value]}))]).await;
    let providers = services(&root);
    GraphCalendarBackend
        .sync(
            &CalendarBackendCtx {
                db: &db,
                services: &providers,
            },
            &account("calendar", "graph"),
        )
        .await
        .unwrap();
    let requests = captured.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| request.starts_with("GET ")));
    let conn = db.reader();
    assert!(db::calendar::get_event(&conn, "stale").is_err());
    assert!(db::calendar::get_event(&conn, "unpublished").is_err());
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM calendar_events", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM calendar_action_operations WHERE completed = 0",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn unsupported_resource_does_not_preserve_stale_content_or_block_other_events() {
    let (_dir, db) = setup_db().await;
    {
        let conn = db.writer().await;
        cache_event(&conn, "bad", Some("unsupported"));
    }
    let mut invalid = standalone();
    invalid["id"] = json!("unsupported");
    invalid["start"] = json!({});
    let valid = standalone();
    let (root, captured) = serve_json(|_| vec![(200, json!({"value":[{"id":"primary", "name":"Calendar", "isDefaultCalendar":true, "color":"auto"}]})), (200, json!({"value":[invalid, valid]}))]).await;
    let providers = services(&root);
    GraphCalendarBackend
        .sync(
            &CalendarBackendCtx {
                db: &db,
                services: &providers,
            },
            &account("calendar", "graph"),
        )
        .await
        .unwrap_err();
    assert_eq!(captured.await.unwrap().len(), 2);
    let conn = db.reader();
    assert!(db::calendar::get_event(&conn, "bad").is_err());
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM calendar_remote_resources",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM calendar_events", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn relative_series_cancellations_publish_and_corrupt_blobs_stay_resource_scoped() {
    use base64::Engine;
    for valid in [true, false] {
        let (_dir, db) = setup_db().await;
        let mut source = crate::mail::graph::event_set::recurrence_blob_tests::relative_master();
        if !valid {
            let data = source["singleValueExtendedProperties"][0]["value"]
                .as_str()
                .unwrap();
            let mut bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap();
            // MonthNth index: Graph says second, corrupted blob claims first.
            bytes[26..30].copy_from_slice(&1_u32.to_le_bytes());
            source["singleValueExtendedProperties"][0]["value"] =
                json!(base64::engine::general_purpose::STANDARD.encode(bytes));
        }
        let mut ordinary = standalone();
        ordinary["id"] = json!("ordinary");
        let expected_source = source.clone();
        let (root, captured) = serve_json(|_| {
            let mut responses = vec![
                (200, json!({"value":[{"id":"primary", "name":"Calendar", "isDefaultCalendar":true, "color":"auto"}]})),
                (200, json!({"value":[source.clone(), ordinary]})),
                (200, source.clone()), (200, source.clone()),
            ];
            if valid { responses.push((200, source)); }
            responses
        }).await;
        let providers = services(&root);
        let result = GraphCalendarBackend
            .sync(
                &CalendarBackendCtx {
                    db: &db,
                    services: &providers,
                },
                &account("calendar", "graph"),
            )
            .await;
        if valid {
            result.unwrap();
        } else {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("Graph resource master") && error.contains("weekday index"),
                "{error}"
            );
        }
        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), if valid { 5 } else { 4 });
        assert!(requests.iter().all(|request| request.starts_with("GET ")));
        let conn = db.reader();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM calendar_remote_resources",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM calendar_remote_resources WHERE error IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            if valid { 0 } else { 1 }
        );
        let native: String = conn
            .query_row(
                "SELECT native_data FROM calendar_remote_resources WHERE remote_id = 'master'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let native: crate::calendar::event_set::NativeCalendarResource =
            serde_json::from_str(&native).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&native.data).unwrap(),
            expected_source
        );
        if valid {
            let id: String = conn
                .query_row(
                    "SELECT event_id FROM calendar_remote_resources WHERE remote_id = 'master'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let set = db::calendar_sync::displayed_set(&conn, &id)
                .unwrap()
                .unwrap();
            let page = crate::calendar::actions::project_display(
                &set,
                &set.event,
                "",
                "2026-10-01",
                "2027-01-01",
                100,
            )
            .unwrap();
            assert_eq!(
                page.occurrences
                    .iter()
                    .map(|item| item.fields.start_time.as_str())
                    .collect::<Vec<_>>(),
                ["2026-10-08T09:00:00Z", "2026-12-10T09:00:00Z"]
            );
            assert!(crate::calendar::simple_recurrence::normalize_rule(
                set.event.recurrence_rule.as_deref().unwrap(),
                &set.event
            )
            .is_err());
        }
    }
}

#[tokio::test]
async fn observed_graph_range_boundary_blobs_publish_only_the_verified_occurrence_set() {
    for relative_monthly in [false, true] {
        let (_dir, db) = setup_db().await;
        let source =
            crate::mail::graph::event_set::recurrence_blob_tests::observed_range_boundary_master(
                relative_monthly,
            );
        let (root, captured) = serve_json(|_| vec![
            (200, json!({"value":[{"id":"primary", "name":"Calendar", "isDefaultCalendar":true, "color":"auto"}]})),
            (200, json!({"value":[source.clone()]})),
            (200, source.clone()), (200, source.clone()), (200, source.clone()),
        ]).await;
        let providers = services(&root);
        GraphCalendarBackend
            .sync(
                &CalendarBackendCtx {
                    db: &db,
                    services: &providers,
                },
                &account("calendar", "graph"),
            )
            .await
            .unwrap();
        assert_eq!(captured.await.unwrap().len(), 5);
        let conn = db.reader();
        let id: String = conn.query_row("SELECT event_id FROM calendar_remote_resources WHERE remote_id = 'master' AND error IS NULL", [], |row| row.get(0)).unwrap();
        let set = db::calendar_sync::displayed_set(&conn, &id)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&set.native.as_ref().unwrap().data).unwrap(),
            source
        );
        let (start, end, count, last) = if relative_monthly {
            ("2022-02-01", "2022-06-01", 2, "2022-04-27T11:00:00Z")
        } else {
            ("2023-06-01", "2023-09-01", 8, "2023-08-21T09:15:00Z")
        };
        let page = crate::calendar::actions::project_display(&set, &set.event, "", start, end, 100)
            .unwrap();
        assert_eq!(page.occurrences.len(), count);
        assert_eq!(page.occurrences.last().unwrap().fields.start_time, last);
        assert!(!page.has_more);
        assert_eq!(set.overrides.len(), 1);
    }
}

#[tokio::test]
async fn cancelled_exception_publishes_as_an_exclusion_and_revision_races_preserve_the_mirror() {
    use crate::mail::graph::event_set::display_tests::{
        cancelled_exception_fixture, serve_display_series,
    };
    for changed in [false, true] {
        let (_dir, db) = setup_db().await;
        {
            let conn = db.writer().await;
            cache_event(&conn, "previous", Some("obsolete"));
        }
        let (source, children) = cancelled_exception_fixture();
        let after = changed.then(|| {
            let mut after = source.clone();
            after["@odata.etag"] = json!("W/\"changed\"");
            after
        });
        let (root, requests) = serve_display_series(source.clone(), children, 8, after).await;
        let providers = services(&root);
        let result = GraphCalendarBackend
            .sync(
                &CalendarBackendCtx {
                    db: &db,
                    services: &providers,
                },
                &account("calendar", "graph"),
            )
            .await;
        if changed {
            assert!(matches!(result, Err(Error::CalendarReadIncomplete(_))));
        } else {
            result.unwrap();
        }
        assert!(requests
            .await
            .unwrap()
            .iter()
            .all(|request| request.starts_with("GET ")));
        let conn = db.reader();
        if changed {
            assert!(db::calendar::get_event(&conn, "previous").is_ok());
            assert_eq!(
                conn.query_row(
                    "SELECT COUNT(*) FROM calendar_remote_resources",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                0
            );
            continue;
        }
        assert!(db::calendar::get_event(&conn, "previous").is_err());
        let id: String = conn.query_row("SELECT event_id FROM calendar_remote_resources WHERE remote_id = 'master' AND error IS NULL", [], |row| row.get(0)).unwrap();
        let set = db::calendar_sync::displayed_set(&conn, &id)
            .unwrap()
            .unwrap();
        assert_eq!(set.overrides.len(), 3);
        assert!(set.overrides[0].event.is_none());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&set.native.as_ref().unwrap().data).unwrap(),
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
        let again = db::calendar_sync::displayed_set(&conn, &id)
            .unwrap()
            .unwrap();
        assert!(again.overrides[0].event.is_none());
    }
}
