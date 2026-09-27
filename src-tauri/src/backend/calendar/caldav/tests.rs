use super::*;
use crate::backend::testutil::{account, event, temp_pool};
use crate::calendar::event_set::{CalendarOverride, NativeCalendarResource};
use crate::provider::ProviderServices;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn raw() -> String {
    "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Fixture//EN\r\nX-CALENDAR:keep\r\n\
     BEGIN:VTIMEZONE\r\nTZID:Europe/Stockholm\r\nX-TIMEZONE:keep\r\nEND:VTIMEZONE\r\n\
     BEGIN:VEVENT\r\nUID:series\r\nDTSTAMP:20260901T100000Z\r\n\
     DTSTART;TZID=Europe/Stockholm:20260914T100000\r\nDTEND;TZID=Europe/Stockholm:20260914T110000\r\n\
     SUMMARY:Master\r\nRRULE:FREQ=WEEKLY;COUNT=6\r\n\
     EXDATE;TZID=Europe/Stockholm:20260928T100000\r\n\
     RDATE;TZID=Europe/Stockholm:20261001T100000\r\n\
     ORGANIZER;SCHEDULE-AGENT=SERVER:mailto:owner@example.org\r\n\
     ATTENDEE;CN=Guest;PARTSTAT=ACCEPTED;SCHEDULE-AGENT=SERVER:mailto:guest@example.org\r\n\
     X-NATIVE;X-PARAM=keep:untouched\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\n\
     DESCRIPTION:alarm\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\n\
     BEGIN:VEVENT\r\nUID:series\r\nDTSTAMP:20260901T100000Z\r\n\
     RECURRENCE-ID;TZID=Europe/Stockholm:20261005T100000\r\n\
     DTSTART:20261005T110000Z\r\nDTEND:20261005T120000Z\r\nSUMMARY:Exception\r\n\
     X-SIBLING:keep\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".into()
}

fn snapshot(raw: String) -> CalendarEventSet {
    resource::snapshot(
        NativeCalendarResource {
            protocol: "caldav".into(),
            calendar_id: "/calendar/".into(),
            event_id: "/calendar/series.ics".into(),
            revision: Some("\"v1\"".into()),
            data: raw,
        },
        &event(),
    )
    .unwrap()
}

fn modified(before: &CalendarEventSet) -> CalendarEventSet {
    let mut desired = before.clone();
    desired.event.title = "New title, semicolon; newline\nwith å".into();
    desired.event.description = Some("Long description ø".repeat(15));
    desired.event.location = Some("Room 42".into());
    desired.event.recurrence_rule = Some("FREQ=DAILY;INTERVAL=2;COUNT=8".into());
    desired
}

#[test]
fn fields_and_rules_preserve_native_siblings_and_roundtrip_text() {
    let before = snapshot(raw());
    assert_eq!(before.overrides.len(), 3);
    assert!(before
        .overrides
        .iter()
        .any(|o| o.original_start == "2026-09-28T08:00:00Z" && o.event.is_none()));
    let desired = modified(&before);
    let data = resource::rewrite(&before, &desired).unwrap();
    for retained in [
        "X-CALENDAR:keep",
        "X-TIMEZONE:keep",
        "X-NATIVE;X-PARAM=keep:untouched",
        "X-SIBLING:keep",
        "DESCRIPTION:alarm",
        "SCHEDULE-AGENT=SERVER",
        "RECURRENCE-ID;TZID=Europe/Stockholm:20261005T100000",
    ] {
        assert!(data.contains(retained), "{retained}");
    }
    assert!(data.contains("SUMMARY:New title\\, semicolon\\; newline\\nwith å"));
    assert!(data.contains("RRULE:FREQ=DAILY;INTERVAL=2;COUNT=8"));
    assert!(data.split("\r\n").all(|line| line.len() <= 75));
    let after = snapshot(data);
    assert_eq!(after.event.title, desired.event.title);
    assert_eq!(after.event.description, desired.event.description);
    assert!(after
        .overrides
        .iter()
        .any(|o| o.event.as_ref().is_some_and(|e| e.title == "Exception")));
}

#[test]
fn generated_occurrence_clones_master_and_existing_exception_keeps_original_id() {
    let before = snapshot(raw());
    let mut desired = before.clone();
    let mut generated = before.event.clone();
    generated.recurrence_kind = RecurrenceKind::Occurrence;
    generated.recurrence_rule = None;
    generated.title = "Generated edit".into();
    generated.start_time = "2026-09-21T10:00:00Z".into();
    generated.end_time = "2026-09-21T11:00:00Z".into();
    desired.overrides.push(CalendarOverride {
        original_start: "2026-09-21T08:00:00Z".into(),
        event: Some(generated),
        native: None,
    });
    desired
        .overrides
        .iter_mut()
        .find(|o| o.original_start == "2026-10-05T08:00:00Z")
        .unwrap()
        .event
        .as_mut()
        .unwrap()
        .title = "Existing edit".into();
    let data = resource::rewrite(&before, &desired).unwrap();
    assert!(data.contains("RECURRENCE-ID;TZID=Europe/Stockholm:20260921T100000"));
    assert!(data.contains("RECURRENCE-ID;TZID=Europe/Stockholm:20261005T100000"));
    assert_eq!(data.matches("BEGIN:VALARM").count(), 2);
    let after = snapshot(data);
    assert_eq!(after.event.title, before.event.title);
    assert_eq!(after.overrides.len(), 4);
    assert!(after.overrides.iter().any(|o| o
        .event
        .as_ref()
        .is_some_and(|e| e.title == "Generated edit")));
}

#[test]
fn opaque_structure_can_change_title_but_cannot_be_flattened() {
    let before = snapshot(raw().replace("FREQ=WEEKLY;COUNT=6", "FREQ=MONTHLY;BYDAY=1MO"));
    let mut desired = before.clone();
    desired.event.title = "Safe title".into();
    assert!(resource::rewrite(&before, &desired)
        .unwrap()
        .contains("RRULE:FREQ=MONTHLY;BYDAY=1MO"));
    desired.event.recurrence_rule = Some("FREQ=DAILY".into());
    assert!(resource::rewrite(&before, &desired).is_err());
    assert!(resource::snapshot(
        NativeCalendarResource {
            data: raw().replace(
                "RECURRENCE-ID;TZID",
                "RECURRENCE-ID;RANGE=THISANDFUTURE;TZID"
            ),
            ..before.native.clone().unwrap()
        },
        &before.event
    )
    .is_err());
}

#[test]
fn all_day_date_exclusive_end_and_copy_overrides_exclusions() {
    let mut desired = CalendarEventSet {
        event: event(),
        overrides: Vec::new(),
        native: None,
        content: None,
    };
    desired.event.all_day = true;
    desired.event.start_time = "2026-09-14".into();
    desired.event.end_time = "2026-09-16".into();
    desired.event.timezone = None;
    desired.event.recurrence_kind = RecurrenceKind::Series;
    desired.event.recurrence_rule = Some("FREQ=YEARLY;UNTIL=20300914".into());
    desired.overrides.push(CalendarOverride {
        original_start: "2027-09-14".into(),
        event: None,
        native: None,
    });
    let data = resource::create(&desired, "new-uid", "marker").unwrap();
    assert!(data.contains("DTSTART;VALUE=DATE:20260914\r\n"));
    assert!(data.contains("DTEND;VALUE=DATE:20260916\r\n"));
    assert!(data.contains("EXDATE;VALUE=DATE:20270914\r\n"));
    resource::verify_operation(&data, "new-uid", "marker").unwrap();
    let copy = resource::create(&snapshot(raw()), "new-uid", "marker").unwrap();
    assert_eq!(copy.matches("UID:new-uid\r\n").count(), 2);
    assert!(copy.contains("EXDATE;TZID=Europe/Stockholm"));
    assert!(copy.contains("RDATE;TZID=Europe/Stockholm"));
    assert!(copy.contains("X-SIBLING:keep"));
}

async fn server(
    responses: Vec<(u16, Option<&'static str>, String)>,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/dav/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, etag, body) in responses {
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
                if let Some(end) = bytes.windows(4).position(|p| p == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let size = headers
                        .lines()
                        .find_map(|line| {
                            let (k, v) = line.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + size {
                        break;
                    }
                }
            }
            requests.push(String::from_utf8(bytes).unwrap());
            if status == 0 {
                continue;
            }
            let header = etag.map(|v| format!("ETag: {v}\r\n")).unwrap_or_default();
            socket.write_all(format!("HTTP/1.1 {status} Test\r\n{header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    (url, task)
}

fn services() -> ProviderServices {
    let mut services = super::super::google::sync_testutil::services("");
    services.transports.dav_http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    services
}

#[tokio::test]
async fn actual_conditional_put_and_canonical_get() {
    let before = snapshot(raw());
    let desired = modified(&before);
    let canonical = resource::rewrite(&before, &desired).unwrap();
    let (url, task) = server(vec![
        (204, Some("\"put\""), String::new()),
        (200, Some("W/\"get\""), canonical),
    ])
    .await;
    let mut account = account("calendar", "caldav");
    account.caldav_url = url;
    let (_dir, db) = temp_pool();
    let services = services();
    let ctx = CalendarBackendCtx {
        db: &db,
        services: &services,
    };
    let after = CalDavCalendarBackend
        .update_event_set(&ctx, &account, &before, &desired)
        .await
        .unwrap();
    assert_eq!(after.event.etag.as_deref(), Some("W/\"get\""));
    let requests = task.await.unwrap();
    assert!(requests[0].starts_with("PUT /calendar/series.ics "));
    assert!(requests[0].contains("if-match: \"v1\"\r\n"));
    assert!(requests[1].starts_with("GET /calendar/series.ics "));
}

#[tokio::test]
async fn weak_revision_requires_fresh_identical_strong_snapshot() {
    let mut before = snapshot(raw());
    before.native.as_mut().unwrap().revision = Some("W/\"v1\"".into());
    let (url, task) = server(vec![(200, Some("W/\"v1\""), raw())]).await;
    let mut account = account("calendar", "caldav");
    account.caldav_url = url;
    let (_dir, db) = temp_pool();
    let services = services();
    let error = CalDavCalendarBackend
        .delete_event_set(
            &CalendarBackendCtx {
                db: &db,
                services: &services,
            },
            &account,
            &before,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("strong ETag"));
    assert_eq!(task.await.unwrap().len(), 1);
}

#[tokio::test]
async fn exact_native_move_and_conditional_delete() {
    let before = snapshot(raw());
    let (url, task) = server(vec![
        (201, None, String::new()),
        (200, Some("\"moved\""), raw()),
        (204, None, String::new()),
    ])
    .await;
    let mut account = account("calendar", "caldav");
    account.caldav_url = url;
    let (_dir, db) = temp_pool();
    let services = services();
    let ctx = CalendarBackendCtx {
        db: &db,
        services: &services,
    };
    let super::super::CalendarCapability::Supported(moved) = CalDavCalendarBackend
        .move_event_set_native(&ctx, &account, &before, "/selected/")
        .await
        .unwrap()
    else {
        panic!("MOVE unsupported")
    };
    CalDavCalendarBackend
        .delete_event_set(&ctx, &account, &moved)
        .await
        .unwrap();
    let requests = task.await.unwrap();
    assert!(requests[0].starts_with("MOVE /calendar/series.ics "));
    assert!(requests[0].contains("/selected/series.ics\r\n"));
    assert!(requests[0].contains("overwrite: F\r\n"));
    assert!(requests[0].contains("if-match: \"v1\"\r\n"));
    assert!(requests[2].starts_with("DELETE /selected/series.ics "));
    assert!(requests[2].contains("if-match: \"moved\"\r\n"));
}

#[tokio::test]
async fn create_exact_calendar_retry_collision_and_lost_response() {
    use sha2::{Digest, Sha256};
    let desired = snapshot(raw());
    let marker = format!("{:x}", Sha256::digest(b"operation-42"));
    let uid = format!("chithi-{marker}");
    let data = resource::create(&desired, &uid, &marker).unwrap();
    for (status, canonical, success) in [
        (201, data.clone(), true),
        (412, data.clone(), true),
        (0, data.clone(), true),
        (412, raw(), false),
    ] {
        let (url, task) = server(vec![
            (status, None, String::new()),
            (200, Some("\"created\""), canonical),
        ])
        .await;
        let mut account = account("calendar", "caldav");
        account.caldav_url = url;
        let (_dir, db) = temp_pool();
        let services = services();
        let result = CalDavCalendarBackend
            .create_event_set(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services,
                },
                &account,
                "/selected/",
                &desired,
                "operation-42",
            )
            .await;
        assert_eq!(result.is_ok(), success, "{result:?}");
        let requests = task.await.unwrap();
        assert!(requests[0].starts_with(&format!("PUT /selected/{uid}.ics ")));
        assert!(requests[0].contains("if-none-match: *\r\n"));
        assert!(!requests[0].contains("if-match:"));
        assert!(requests[0].contains("X-SIBLING:keep"));
    }
}

#[tokio::test]
async fn ordinary_update_sends_a_real_put_using_database_calendar_routing() {
    let data = raw().replace("RRULE:FREQ=WEEKLY;COUNT=6\r\n", "");
    // Use a genuinely standalone object, rather than a master stripped of its rule.
    let start = data.find("BEGIN:VEVENT").unwrap();
    let end = data[start..].find("END:VEVENT").unwrap() + start + "END:VEVENT".len();
    let body = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Fixture//EN\r\n{}\r\nEND:VCALENDAR\r\n",
        &data[start..end]
    )
    .replace("EXDATE;TZID=Europe/Stockholm:20260928T100000\r\n", "")
    .replace("RDATE;TZID=Europe/Stockholm:20261001T100000\r\n", "");
    let before = snapshot(body.clone());
    let mut desired = before.event.clone();
    desired.title = "Ordinary update".into();
    let (url, task) = server(vec![
        (200, Some("\"v1\""), body.clone()),
        (204, None, String::new()),
        (
            200,
            Some("\"v2\""),
            body.replace("SUMMARY:Master", "SUMMARY:Ordinary update"),
        ),
    ])
    .await;
    let mut account = account("calendar", "caldav");
    account.caldav_url = url;
    let (_dir, db) = temp_pool();
    let services = services();
    {
        let conn = db.writer().await;
        crate::db::schema::initialize(&conn).unwrap();
        conn.execute("INSERT INTO accounts (id, display_name, email, username) VALUES (?1, 'Test', 'test@example.org', 'test')", [&account.id]).unwrap();
        conn.execute("INSERT INTO calendars (id, account_id, name, remote_id) VALUES (?1, ?2, 'Calendar', '/calendar/')", rusqlite::params![desired.calendar_id, account.id]).unwrap();
    }
    CalDavCalendarBackend
        .push_updated_event(
            &CalendarBackendCtx {
                db: &db,
                services: &services,
            },
            &account,
            "/calendar/series.ics",
            &desired,
        )
        .await
        .unwrap();
    let requests = task.await.unwrap();
    assert!(requests[1].starts_with("PUT /calendar/series.ics "));
    assert!(requests[1].contains("SUMMARY:Ordinary update\r\n"));
    assert!(requests[1].contains("if-match: \"v1\"\r\n"));
}

#[tokio::test]
async fn move_only_explicit_method_rejection_is_unsupported() {
    for status in [405, 501, 403, 409, 412, 500, 207, 0] {
        let (url, task) = server(vec![(status, None, String::new())]).await;
        let mut account = account("calendar", "caldav");
        account.caldav_url = url;
        let (_dir, db) = temp_pool();
        let services = services();
        let result = CalDavCalendarBackend
            .move_event_set_native(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services,
                },
                &account,
                &snapshot(raw()),
                "/destination/",
            )
            .await;
        if [405, 501].contains(&status) {
            assert_eq!(
                result.unwrap(),
                super::super::CalendarCapability::Unsupported
            );
        } else {
            assert!(result.is_err(), "{status}");
        }
        assert_eq!(task.await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn conditional_delete_conflict_or_lost_response_is_an_error() {
    for status in [412, 0] {
        let (url, task) = server(vec![(status, None, String::new())]).await;
        let mut account = account("calendar", "caldav");
        account.caldav_url = url;
        let (_dir, db) = temp_pool();
        let services = services();
        assert!(CalDavCalendarBackend
            .delete_event_set(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services
                },
                &account,
                &snapshot(raw())
            )
            .await
            .is_err());
        let requests = task.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("if-match: \"v1\"\r\n"));
    }
}

#[tokio::test]
async fn detached_exception_is_read_outside_view_and_edited_at_its_own_href() {
    let full = raw();
    let rid = full.find("RECURRENCE-ID").unwrap();
    let begin = full[..rid].rfind("BEGIN:VEVENT").unwrap();
    let end = full[rid..].find("END:VEVENT").unwrap() + rid + "END:VEVENT\r\n".len();
    let detached = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Detached//EN\r\n{}END:VCALENDAR\r\n",
        &full[begin..end]
    );
    let master = format!("{}{}", &full[..begin], &full[end..]);
    let report = format!("<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:response><d:href>/calendar/detached.ics</d:href><d:propstat><d:prop><d:getetag>\"detached\"</d:getetag><c:calendar-data>{}</c:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>", detached.replace('&', "&amp;").replace('<', "&lt;"));
    let (url, task) = server(vec![
        (200, Some("\"v1\""), master.clone()),
        (207, None, report),
        (200, Some("\"detached\""), detached.clone()),
        (204, None, String::new()),
        (200, Some("\"v2\""), master.clone()),
        (
            200,
            Some("\"d2\""),
            detached.replace("SUMMARY:Exception", "SUMMARY:Edited detached"),
        ),
    ])
    .await;
    let mut account = account("calendar", "caldav");
    account.caldav_url = url;
    let (_dir, db) = temp_pool();
    let services = services();
    let ctx = CalendarBackendCtx {
        db: &db,
        services: &services,
    };
    let template = snapshot(master).event;
    let before = CalDavCalendarBackend
        .fetch_event_set(&ctx, &account, &template, "/calendar/")
        .await
        .unwrap();
    let mut desired = before.clone();
    let exception = desired
        .overrides
        .iter_mut()
        .find(|o| o.original_start == "2026-10-05T08:00:00Z")
        .unwrap();
    assert_eq!(
        exception.native.as_ref().unwrap().event_id,
        "/calendar/detached.ics"
    );
    exception.event.as_mut().unwrap().title = "Edited detached".into();
    let after = CalDavCalendarBackend
        .update_event_set(&ctx, &account, &before, &desired)
        .await
        .unwrap();
    assert!(after.overrides.iter().any(|o| o
        .event
        .as_ref()
        .is_some_and(|e| e.title == "Edited detached")));
    let requests = task.await.unwrap();
    assert!(requests[1].starts_with("REPORT /calendar/ "));
    assert!(!requests[1].contains("time-range"));
    assert!(requests[3].starts_with("PUT /calendar/detached.ics "));
    assert!(requests[3].contains("if-match: \"detached\"\r\n"));
    assert!(requests[3].contains("RECURRENCE-ID;TZID=Europe/Stockholm:20261005T100000\r\n"));
}

#[tokio::test]
async fn generated_occurrence_is_materialized_by_http_put() {
    let before = snapshot(raw());
    let mut desired = before.clone();
    let mut occurrence = before.event.clone();
    occurrence.title = "Materialized over HTTP".into();
    occurrence.recurrence_rule = None;
    occurrence.recurrence_kind = RecurrenceKind::Occurrence;
    occurrence.start_time = "2026-09-21T11:00:00Z".into();
    occurrence.end_time = "2026-09-21T12:00:00Z".into();
    desired.overrides.push(CalendarOverride {
        original_start: "2026-09-21T08:00:00Z".into(),
        event: Some(occurrence),
        native: None,
    });
    let data = resource::rewrite(&before, &desired).unwrap();
    let (url, task) = server(vec![
        (204, None, String::new()),
        (200, Some("\"v2\""), data),
    ])
    .await;
    let mut account = account("calendar", "caldav");
    account.caldav_url = url;
    let (_dir, db) = temp_pool();
    let services = services();
    let after = CalDavCalendarBackend
        .update_event_set(
            &CalendarBackendCtx {
                db: &db,
                services: &services,
            },
            &account,
            &before,
            &desired,
        )
        .await
        .unwrap();
    assert_eq!(after.overrides.len(), 4);
    let requests = task.await.unwrap();
    assert!(requests[0].contains("RECURRENCE-ID;TZID=Europe/Stockholm:20260921T100000\r\n"));
    assert!(requests[0].contains("SUMMARY:Materialized over HTTP\r\n"));
    assert!(requests[0].contains("X-SIBLING:keep\r\n"));
    assert_eq!(requests[0].matches("BEGIN:VALARM").count(), 2);
}

#[tokio::test]
async fn weak_validator_can_upgrade_but_never_rebase_changed_content() {
    for changed in [false, true] {
        let mut before = snapshot(raw());
        before.native.as_mut().unwrap().revision = Some("W/\"v1\"".into());
        let mut responses = vec![(
            200,
            Some("\"fresh\""),
            if changed {
                raw().replace("SUMMARY:Master", "SUMMARY:Concurrent")
            } else {
                raw()
            },
        )];
        if !changed {
            responses.push((204, None, String::new()));
        }
        let (url, task) = server(responses).await;
        let mut account = account("calendar", "caldav");
        account.caldav_url = url;
        let (_dir, db) = temp_pool();
        let services = services();
        let result = CalDavCalendarBackend
            .delete_event_set(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services,
                },
                &account,
                &before,
            )
            .await;
        assert_eq!(result.is_err(), changed);
        let requests = task.await.unwrap();
        assert!(requests[0].starts_with("GET /calendar/series.ics "));
        if !changed {
            assert!(requests[1].contains("if-match: \"fresh\"\r\n"));
        }
    }
}

#[test]
fn ui_rules_and_transfer_attendees_are_exported_as_complete_content() {
    for rule in [
        "FREQ=DAILY;INTERVAL=2;COUNT=8",
        "FREQ=WEEKLY;BYDAY=MO,WE;UNTIL=20261231",
        "FREQ=MONTHLY;INTERVAL=3;COUNT=4",
        "FREQ=YEARLY;COUNT=2",
    ] {
        let mut desired = snapshot(raw());
        desired.native = None;
        for item in &mut desired.overrides {
            item.native = None;
        }
        desired.event.recurrence_rule = Some(rule.into());
        let data = resource::create(&desired, "export", "marker").unwrap();
        assert!(data.contains("ORGANIZER:mailto:owner@example.org\r\n"));
        assert!(
            data.contains("ATTENDEE;CN=\"Guest\";PARTSTAT=ACCEPTED:mailto:guest@example.org\r\n")
        );
        assert!(data.contains("DTSTART;TZID=Europe/Stockholm:20260914T100000\r\n"));
        let after = snapshot(data);
        assert_eq!(after.overrides.len(), desired.overrides.len());
        if rule.contains("UNTIL") {
            assert!(after
                .event
                .recurrence_rule
                .as_ref()
                .unwrap()
                .contains("UNTIL=20261231T225959Z"));
        }
    }
}

#[tokio::test]
async fn source_owner_uid_and_collection_mismatches_fail_before_http() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut account = account("calendar", "caldav");
    account.caldav_url = format!("http://{}/dav/", listener.local_addr().unwrap());
    let (_dir, db) = temp_pool();
    let services = services();
    let ctx = CalendarBackendCtx {
        db: &db,
        services: &services,
    };
    for mismatch in ["owner", "uid", "collection"] {
        let mut before = snapshot(raw());
        match mismatch {
            "owner" => before.event.account_id = "another-account".into(),
            "uid" => before.event.uid = Some("another-uid".into()),
            _ => before.native.as_mut().unwrap().calendar_id = "/another-calendar/".into(),
        }
        assert!(CalDavCalendarBackend
            .delete_event_set(&ctx, &account, &before)
            .await
            .is_err());
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}
