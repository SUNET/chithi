//! JMAP calendar backend (RFC 8984 JSCalendar via `CalendarEvent/*`).

use async_trait::async_trait;

use crate::calendar::{attendee_status_from_json, CalendarEvent};
use crate::db;
use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::mail::jmap::{JmapCalendarEvent, JmapConfig, JmapConnection};

use super::{
    get_unpushed_events, AttendeeResponseUpdate, CalendarBackend, CalendarBackendCtx,
    CalendarCapability, InviteReplyDelivery, PushedEvent,
};

pub struct JmapCalendarBackend;

async fn connect(
    ctx: &CalendarBackendCtx<'_>,
    account: &AccountFull,
) -> Result<(JmapConfig, JmapConnection)> {
    let (config, connection) = ctx.services.jmap_client(account).await?;
    Ok((config, connection))
}

/// Build the wire event from a local row. `id` is empty for creates —
/// the server assigns one.
fn to_jmap_event(event: &CalendarEvent, remote_calendar_id: &str) -> Result<JmapCalendarEvent> {
    JmapCalendarEvent::for_local_creation(event, remote_calendar_id)
}

#[async_trait]
impl CalendarBackend for JmapCalendarBackend {
    fn protocol(&self) -> &'static str {
        "jmap"
    }

    fn invite_reply_delivery(&self) -> InviteReplyDelivery {
        InviteReplyDelivery::JmapSubmission
    }

    async fn sync(&self, ctx: &CalendarBackendCtx<'_>, account: &AccountFull) -> Result<()> {
        let db = ctx.db;
        let account_id = account.id.as_str();
        let (jmap_config, jmap_conn) = connect(ctx, account).await?;

        // Step 1: Fetch and upsert calendars
        let jmap_calendars = jmap_conn.list_jmap_calendars(&jmap_config).await?;
        log::info!(
            "sync_calendars: fetched {} calendars from JMAP for account {}",
            jmap_calendars.len(),
            account_id
        );

        // Build a mapping from remote calendar ID to local calendar ID
        let mut remote_to_local: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        {
            let conn = db.writer().await;
            for jcal in &jmap_calendars {
                let color = jcal.color.as_deref().unwrap_or("#4285f4");
                let local_id = db::calendar::upsert_calendar_by_remote_id(
                    &conn,
                    account_id,
                    &jcal.id,
                    &jcal.name,
                    color,
                    jcal.is_default,
                )?;
                remote_to_local.insert(jcal.id.clone(), local_id);
            }
        }

        // Step 2: For each calendar, fetch events and upsert into local DB
        for jcal in &jmap_calendars {
            let events = match jmap_conn
                .fetch_calendar_events(&jmap_config, Some(&jcal.id))
                .await
            {
                Ok(evts) => evts,
                Err(e) => {
                    log::error!(
                        "sync_calendars: failed to fetch events for calendar '{}': {}",
                        jcal.name,
                        e
                    );
                    continue;
                }
            };

            log::info!(
                "sync_calendars: fetched {} events for calendar '{}'",
                events.len(),
                jcal.name
            );

            let local_cal_id = remote_to_local.get(&jcal.id).cloned().unwrap_or_default();

            let mut conn = db.writer().await;
            for ev in &events {
                let event_id = uuid::Uuid::new_v4().to_string();
                let cal_event = CalendarEvent {
                    id: event_id,
                    account_id: account_id.to_string(),
                    calendar_id: local_cal_id.clone(),
                    uid: ev.uid.clone(),
                    title: ev.title.clone(),
                    description: ev.description.clone(),
                    location: ev.location.clone(),
                    start_time: ev.start.clone(),
                    end_time: ev.end.clone(),
                    all_day: ev.all_day,
                    timezone: ev.timezone.clone(),
                    recurrence_rule: ev.recurrence_rule.clone(),
                    recurrence_kind: ev.recurrence_kind,
                    organizer_email: ev.organizer_email.clone(),
                    attendees_json: ev.attendees_json.clone(),
                    my_status: attendee_status_from_json(
                        ev.attendees_json.as_deref(),
                        &account.email,
                    ),
                    source_message_id: None,
                    ical_data: None,
                    remote_id: Some(ev.id.clone()),
                    etag: None,
                };

                if let Err(e) = db::calendar::upsert_event_by_remote_id(&conn, &cal_event) {
                    log::error!(
                        "sync_calendars: failed to upsert event '{}': {}",
                        ev.title,
                        e
                    );
                }
            }

            // Remove local events with remote_id that no longer exist on server
            let server_ids: std::collections::HashSet<String> =
                events.iter().map(|e| e.id.clone()).collect();
            let local_synced: Vec<(String, String)> = conn
                .prepare(
                    "SELECT id, remote_id FROM calendar_events WHERE account_id = ?1 AND calendar_id = ?2 AND remote_id IS NOT NULL AND remote_id != ''",
                )
                .and_then(|mut stmt| {
                    stmt.query_map(rusqlite::params![account_id, local_cal_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                })
                .unwrap_or_default();

            let deleted_ids: Vec<String> = local_synced
                .iter()
                .filter(|(_, remote_id)| !server_ids.contains(remote_id))
                .map(|(local_id, _)| local_id.clone())
                .collect();
            let deleted = if deleted_ids.is_empty() {
                0
            } else {
                match conn.transaction() {
                    Ok(transaction) => {
                        match db::calendar_event_deletion::delete_events(&transaction, &deleted_ids)
                        {
                            Ok(result) if transaction.commit().is_ok() => result.deleted,
                            _ => 0,
                        }
                    }
                    Err(_) => 0,
                }
            };
            if deleted > 0 {
                log::info!(
                    "sync_calendars: removed {} server-deleted events from '{}'",
                    deleted,
                    jcal.name
                );
            }
        }

        // Step 3: Push local events (no remote_id) to the JMAP server
        let mut push_failures = Vec::new();
        {
            let conn = db.writer().await;
            let local_events: Vec<CalendarEvent> = get_unpushed_events(&conn, account_id)?;

            if !local_events.is_empty() {
                log::info!(
                    "sync_calendars: pushing {} local events to JMAP",
                    local_events.len()
                );
                drop(conn); // Release lock for async calls

                for ev in &local_events {
                    // Find the remote calendar ID for this event's local calendar
                    let remote_cal_id = remote_to_local
                        .iter()
                        .find(|(_, local_id)| **local_id == ev.calendar_id)
                        .map(|(remote_id, _)| remote_id.clone())
                        .unwrap_or_default();

                    if remote_cal_id.is_empty() {
                        push_failures.push(format!(
                            "{} ({}): destination calendar is unavailable",
                            ev.title, ev.id
                        ));
                        continue;
                    }

                    let jmap_event = match to_jmap_event(ev, &remote_cal_id) {
                        Ok(event) => event,
                        Err(error) => {
                            push_failures.push(format!("{}: {error}", ev.id));
                            continue;
                        }
                    };

                    match jmap_conn
                        .create_calendar_event(&jmap_config, &jmap_event)
                        .await
                    {
                        Ok(remote_id) => {
                            log::info!(
                                "sync_calendars: pushed event '{}' to JMAP, remote_id={}",
                                ev.title,
                                remote_id
                            );
                            let conn = db.writer().await;
                            if let Err(error) = conn.execute(
                                "UPDATE calendar_events SET remote_id = ?1 WHERE id = ?2",
                                rusqlite::params![remote_id, ev.id],
                            ) {
                                push_failures.push(format!(
                                    "{} ({}): failed to save remote id: {error}",
                                    ev.title, ev.id
                                ));
                            }
                        }
                        Err(e) => {
                            push_failures.push(format!("{} ({}): {e}", ev.title, ev.id));
                        }
                    }
                }
            }
        }

        if push_failures.is_empty() {
            Ok(())
        } else {
            Err(Error::Sync(format!(
                "JMAP calendar sync could not create {} local event(s): {}",
                push_failures.len(),
                push_failures.join("; ")
            )))
        }
    }

    async fn push_created_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        event: &CalendarEvent,
        remote_calendar_id: &str,
    ) -> Result<Option<PushedEvent>> {
        let jmap_event = to_jmap_event(event, remote_calendar_id)?;
        let (jmap_config, conn_jmap) = connect(ctx, account).await?;
        let remote_id = conn_jmap
            .create_calendar_event(&jmap_config, &jmap_event)
            .await?;
        Ok(Some(PushedEvent {
            remote_id,
            canonical_uid: None,
        }))
    }

    async fn push_deleted_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        _remote_calendar_id: &str,
    ) -> Result<()> {
        let (jmap_config, conn_jmap) = connect(ctx, account).await?;
        conn_jmap
            .delete_calendar_event(&jmap_config, remote_id)
            .await
    }

    async fn push_calendar_rename(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        name: &str,
    ) -> Result<()> {
        let (jmap_config, conn_jmap) = connect(ctx, account).await?;
        conn_jmap
            .rename_calendar(&jmap_config, remote_id, name)
            .await
    }

    async fn push_calendar_color(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        color: &str,
    ) -> Result<()> {
        let (jmap_config, conn_jmap) = connect(ctx, account).await?;
        conn_jmap
            .set_calendar_color(&jmap_config, remote_id, color)
            .await
    }

    async fn push_attendee_responses(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        updates: &[AttendeeResponseUpdate],
    ) -> Result<CalendarCapability<()>> {
        let Ok((jmap_config, connection)) = connect(ctx, account).await else {
            return Ok(CalendarCapability::Supported(()));
        };
        let Ok(events) = connection.fetch_calendar_events(&jmap_config, None).await else {
            return Ok(CalendarCapability::Supported(()));
        };

        for update in updates {
            let Some(event) = events.iter().find(|event| event.id == update.remote_id) else {
                continue;
            };
            let Some(attendees_json) = event.attendees_json.as_deref() else {
                continue;
            };
            let Ok(attendees) = serde_json::from_str::<Vec<serde_json::Value>>(attendees_json)
            else {
                continue;
            };
            for (index, attendee) in attendees.iter().enumerate() {
                if attendee["email"].as_str() == Some(&update.attendee_email) {
                    let participant_key = format!("att{}", index);
                    connection
                        .update_participant_status(
                            &jmap_config,
                            &update.remote_id,
                            &participant_key,
                            &update.response,
                        )
                        .await
                        .ok();
                    break;
                }
            }
        }

        Ok(CalendarCapability::Supported(()))
    }
}

#[cfg(test)]
mod payload_tests {
    use super::to_jmap_event;
    use crate::backend::testutil::event;

    #[test]
    fn create_payload_leaves_id_to_server_and_targets_given_calendar() {
        let local = event();
        let wire = to_jmap_event(&local, "remote-cal-7").unwrap();
        assert_eq!(wire.id, "");
        assert_eq!(wire.calendar_id, "remote-cal-7");
        assert_eq!(wire.title, local.title);
        assert_eq!(wire.start, local.start_time);
        assert_eq!(wire.end, local.end_time);
        assert!(!wire.all_day);
        assert_eq!(wire.recurrence_kind, local.recurrence_kind);
    }

    #[test]
    fn all_day_flag_carries_through() {
        let mut local = event();
        local.all_day = true;
        assert!(to_jmap_event(&local, "c").unwrap().all_day);
    }
}

#[cfg(test)]
mod deferred_creation_tests {
    use super::*;
    use crate::backend::testutil::{account, event, temp_pool};
    use crate::calendar::RecurrenceKind;
    use crate::provider::ProviderServices;
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct CalendarServer {
        root: String,
        writes: Arc<Mutex<Vec<Value>>>,
        events: Arc<Mutex<Vec<Value>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for CalendarServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl CalendarServer {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let root = format!("http://{}", listener.local_addr().unwrap());
            let writes = Arc::new(Mutex::new(Vec::new()));
            let events = Arc::new(Mutex::new(Vec::<Value>::new()));
            let captured = writes.clone();
            let stored = events.clone();
            let base = root.clone();
            let task = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    let mut chunk = [0; 4096];
                    let header_end = loop {
                        let count = stream.read(&mut chunk).await.unwrap();
                        assert!(count > 0);
                        bytes.extend_from_slice(&chunk[..count]);
                        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                            break end + 4;
                        }
                    };
                    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    while bytes.len() < header_end + length {
                        let count = stream.read(&mut chunk).await.unwrap();
                        assert!(count > 0);
                        bytes.extend_from_slice(&chunk[..count]);
                    }
                    let response = if headers.starts_with("GET ") {
                        json!({
                            "apiUrl": format!("{base}/jmap/api"),
                            "downloadUrl": format!("{base}/download/{{blobId}}"),
                            "uploadUrl": format!("{base}/upload/{{accountId}}"),
                            "primaryAccounts": {"urn:ietf:params:jmap:mail": "remote-account"},
                            "accounts": {"remote-account": {"accountCapabilities": {
                                "urn:ietf:params:jmap:mail": {},
                                "urn:ietf:params:jmap:calendars": {}
                            }}}
                        })
                    } else {
                        let request: Value =
                            serde_json::from_slice(&bytes[header_end..header_end + length])
                                .unwrap();
                        let mut responses = Vec::new();
                        for call in request["methodCalls"].as_array().unwrap() {
                            let method = call[0].as_str().unwrap();
                            let body = match method {
                                "Calendar/get" => json!({"list": [{
                                    "id": "remote-cal", "name": "Calendar", "isDefault": true
                                }]}),
                                "CalendarEvent/query" => json!({
                                    "ids": stored.lock().unwrap().iter().map(|event| event["id"].clone()).collect::<Vec<_>>()
                                }),
                                "CalendarEvent/get" => {
                                    json!({"list": stored.lock().unwrap().clone()})
                                }
                                "CalendarEvent/set" => {
                                    let mut event = call[1]["create"]["new1"].clone();
                                    assert!(event.is_object());
                                    captured.lock().unwrap().push(event.clone());
                                    let mut events = stored.lock().unwrap();
                                    let id = format!("remote-{}", events.len());
                                    event["id"] = json!(id);
                                    events.push(event);
                                    json!({"created": {"new1": {"id": id}}})
                                }
                                _ => panic!("unexpected JMAP method {method}"),
                            };
                            responses.push(json!([method, body, call[2]]));
                        }
                        json!({"methodResponses": responses, "sessionState": "state"})
                    };
                    let body = response.to_string();
                    stream.write_all(format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    ).as_bytes()).await.unwrap();
                }
            });
            Self {
                root,
                writes,
                events,
                task,
            }
        }

        fn services(&self) -> ProviderServices {
            // Basic-auth calendar sync does not consult OAuth or the keyring.
            let mut services = ProviderServices::production().unwrap();
            let http = reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(3))
                .build()
                .unwrap();
            services.transports.jmap_discovery_http = http.clone();
            services.transports.jmap_api_http = http;
            services
        }

        fn account(&self) -> AccountFull {
            let mut account = account("calendar", "jmap");
            account.jmap_url = self.root.clone();
            account.jmap_auth_method = "basic".into();
            account
        }
    }

    #[tokio::test]
    async fn deferred_creation_blocks_laundering_but_processes_valid_rows_on_each_sync() {
        let server = CalendarServer::start().await;
        let services = server.services();
        let account = server.account();
        let (_directory, db) = temp_pool();
        let context = CalendarBackendCtx {
            db: &db,
            services: &services,
        };
        let mut blocked = Vec::new();
        let mut allowed = Vec::new();
        {
            let conn = db.writer().await;
            db::schema::initialize(&conn).unwrap();
            conn.execute(
                "INSERT INTO accounts (id, display_name, email, username)
                 VALUES ('acc1', 'Test', 'u@example.com', 'u@example.com')",
                [],
            )
            .unwrap();
            let calendar_id = db::calendar::upsert_calendar_by_remote_id(
                &conn,
                &account.id,
                "remote-cal",
                "Calendar",
                "#4285f4",
                true,
            )
            .unwrap();
            let raw = "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//Test//EN\n\
                BEGIN:VEVENT\nUID:source\nDTSTAMP:20260901T120000Z\n\
                DTSTART:20260913T100000Z\nRRULE:FREQ=WEEKLY\n\
                EXDATE:20260920T100000Z\nEND:VEVENT\nEND:VCALENDAR\n";
            for (id, kind, rule, ical) in [
                ("unknown", RecurrenceKind::Unknown, None, None),
                (
                    "occurrence",
                    RecurrenceKind::Occurrence,
                    None,
                    Some(raw.replace(
                        "RRULE:FREQ=WEEKLY\nEXDATE:20260920T100000Z",
                        "RECURRENCE-ID:20260920T100000Z",
                    )),
                ),
                ("series-no-rule", RecurrenceKind::Series, None, None),
                (
                    "series-invalid",
                    RecurrenceKind::Series,
                    Some("FREQ=WEEKLY;COUNT=bad"),
                    None,
                ),
                (
                    "series-exclusions",
                    RecurrenceKind::Series,
                    Some("FREQ=WEEKLY"),
                    Some(raw.into()),
                ),
                ("valid-standalone", RecurrenceKind::Standalone, None, None),
                (
                    "valid-series",
                    RecurrenceKind::Series,
                    Some("FREQ=WEEKLY;COUNT=4"),
                    None,
                ),
            ] {
                let event = CalendarEvent {
                    id: id.into(),
                    calendar_id: calendar_id.clone(),
                    title: id.into(),
                    uid: Some(format!("{id}@test")),
                    recurrence_kind: kind,
                    recurrence_rule: rule.map(str::to_string),
                    ical_data: ical,
                    // Both legacy representations of an unpushed remote id occur.
                    remote_id: (kind == RecurrenceKind::Occurrence).then(String::new),
                    ..event()
                };
                db::calendar::insert_event(&conn, &event).unwrap();
                if id.starts_with("valid-") {
                    allowed.push(event);
                } else {
                    blocked.push(event);
                }
            }
        }
        for _ in 0..2 {
            let error = JmapCalendarBackend
                .sync(&context, &account)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("could not create 5 local event(s)"),
                "{error}"
            );
            assert!(error.contains("source calendar"), "{error}");
            let conn = db.reader();
            for before in &blocked {
                assert!(error.contains(&before.id), "{error}");
                let after = db::calendar::get_event(&conn, &before.id).unwrap();
                assert_eq!(
                    serde_json::to_value(after).unwrap(),
                    serde_json::to_value(before).unwrap()
                );
            }
            for before in &allowed {
                let after = db::calendar::get_event(&conn, &before.id).unwrap();
                assert_eq!(after.recurrence_kind, before.recurrence_kind);
                assert!(after.remote_id.is_some());
            }
            let writes = server.writes.lock().unwrap();
            assert_eq!(
                writes.len(),
                2,
                "blocked rows must never reach CalendarEvent/set"
            );
            assert!(writes
                .iter()
                .all(|event| event["uid"].as_str().unwrap().starts_with("valid-")));
            let series = writes
                .iter()
                .find(|event| event["title"] == "valid-series")
                .unwrap();
            assert_eq!(series["recurrenceRules"][0]["frequency"], "weekly");
            assert_eq!(series["recurrenceRules"][0]["count"], 4);
        }
    }

    #[tokio::test]
    async fn direct_transport_creation_rejects_unknown_occurrences_and_lossy_provider_dtos() {
        let server = CalendarServer::start().await;
        server.events.lock().unwrap().push(json!({
            "id": "source", "@type": "Event", "uid": "source", "calendarIds": {"remote-cal": true},
            "start": "2026-09-13T10:00:00", "recurrenceRules": [{"frequency": "weekly"}],
            "recurrenceOverrides": {"2026-09-20T10:00:00": {"excluded": true}}
        }));
        let services = server.services();
        let (config, connection) = services.jmap_client(&server.account()).await.unwrap();
        let valid = to_jmap_event(&event(), "remote-cal").unwrap();
        for kind in [
            RecurrenceKind::Unknown,
            RecurrenceKind::Occurrence,
            RecurrenceKind::Series,
        ] {
            let mut blocked = valid.clone();
            blocked.recurrence_kind = kind;
            blocked.recurrence_rule =
                (kind == RecurrenceKind::Series).then(|| "FREQ=WEEKLY".into());
            let error = connection
                .create_calendar_event(&config, &blocked)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("Cannot create JMAP event"), "{error}");
        }
        let fetched = connection
            .fetch_calendar_events(&config, None)
            .await
            .unwrap();
        assert_eq!(fetched[0].recurrence_kind, RecurrenceKind::Series);
        assert!(fetched[0].recurrence_rule.is_some());
        assert!(connection
            .create_calendar_event(&config, &fetched[0])
            .await
            .is_err());
        assert!(server.writes.lock().unwrap().is_empty());
    }
}
