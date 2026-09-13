//! CalDAV calendar backend (RFC 4791).

use async_trait::async_trait;

use crate::calendar::ical;
use crate::calendar::{attendee_status_for_email, CalendarEvent, RecurrenceKind};
use crate::db;
use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::mail::caldav::{CalDavClient, CalDavConfig, CalDavEvent};

use super::{get_unpushed_events, CalendarBackend, CalendarBackendCtx, PushedEvent};

pub struct CalDavCalendarBackend;

struct InitialUploadSnapshot {
    event: CalendarEvent,
    revision: i64,
}

/// Reload the candidate and its token together; the unpushed list may be stale.
fn capture_initial_upload(
    conn: &mut rusqlite::Connection,
    account_id: &str,
    event_id: &str,
) -> Result<InitialUploadSnapshot> {
    let transaction = conn.transaction()?;
    let event = db::calendar::get_event(&transaction, event_id)?;
    let revision = db::calendar_revision::get(&transaction, event_id)?;
    if event.account_id != account_id || event.remote_id.as_deref().is_some_and(|id| !id.is_empty())
    {
        return Err(Error::Sync(
            "Event is no longer an unpushed event for this account".into(),
        ));
    }
    transaction.commit()?;
    Ok(InitialUploadSnapshot { event, revision })
}

/// Attach transport metadata only if no write occurred during the accepted PUT.
fn persist_initial_upload(
    conn: &mut rusqlite::Connection,
    expected: &InitialUploadSnapshot,
    remote_id: &str,
    etag: &str,
    uid: &str,
) -> Result<()> {
    (|| -> Result<()> {
        let transaction = conn.transaction()?;
        let current = db::calendar::get_event(&transaction, &expected.event.id)?;
        let revision = db::calendar_revision::get(&transaction, &expected.event.id)?;
        if current != expected.event || revision != expected.revision {
            return Err(Error::Sync("Event changed during the initial upload".into()));
        }
        // ical_data is original source evidence, never a serialization cache.
        // Metadata attachment must neither revoke nor recreate invitation proof.
        let updated = transaction.execute(
            "UPDATE calendar_events SET remote_id = ?1, etag = ?2, uid = ?3 WHERE id = ?4",
            rusqlite::params![remote_id, etag, uid, expected.event.id],
        )?;
        if updated != 1 {
            return Err(Error::Sync("Event metadata was not saved".into()));
        }
        transaction.commit()?;
        Ok(())
    })()
    .map_err(|error| {
        Error::Sync(format!(
            "CalDAV accepted the initial PUT at '{remote_id}', but local metadata could not be attached: {error}. The remote upload has not been undone; sync and review the event before retrying"
        ))
    })
}

async fn push_initial_event(
    db: &db::pool::DbPool,
    client: &CalDavClient,
    account_id: &str,
    event_id: &str,
    remote_to_local: &std::collections::HashMap<String, (String, bool)>,
) -> Result<()> {
    let snapshot = {
        let mut conn = db.writer().await;
        capture_initial_upload(&mut conn, account_id, event_id)?
    };
    let event = &snapshot.event;
    let remote_cal_href = remote_to_local
        .iter()
        .find(|(_, (local_id, _))| *local_id == event.calendar_id)
        .map(|(remote_href, _)| remote_href)
        .filter(|href| !href.is_empty())
        .ok_or_else(|| Error::Sync("Destination calendar is unavailable".into()))?;
    let uid = event
        .uid
        .clone()
        .unwrap_or_else(|| format!("{}@chithi", uuid::Uuid::new_v4()));
    let ical_data = creation_ical_data(event, &uid)?;
    let etag = client.put_event(remote_cal_href, &uid, &ical_data).await?;
    let remote_id = format!("{}/{}.ics", remote_cal_href.trim_end_matches('/'), uid);
    let mut conn = db.writer().await;
    persist_initial_upload(&mut conn, &snapshot, &remote_id, &etag, &uid)?;
    log::info!(
        "sync_calendars: pushed event '{}' to CalDAV, remote_id={}",
        event.title,
        remote_id
    );
    Ok(())
}

/// Preserve source recurrence verbatim; generated ICS must never erase it.
fn creation_ical_data(event: &CalendarEvent, uid: &str) -> Result<String> {
    let unsupported = || {
        Error::Other(format!(
        "Cannot create CalDAV event '{}': complete recurrence data is unavailable. Keep this event local and manage its recurrence in the source calendar",
        event.title
    ))
    };
    if event.recurrence_kind == RecurrenceKind::Unknown {
        return Err(unsupported());
    }
    if let Some(raw) = &event.ical_data {
        if ical::parse_ical_data(raw)
            .iter()
            .any(|invite| invite.recurrence_kind == event.recurrence_kind)
        {
            return Ok(raw.clone());
        }
        return Err(unsupported());
    }
    let rule = event
        .recurrence_rule
        .as_deref()
        .filter(|rule| !rule.is_empty());
    let rule = match (event.recurrence_kind, rule) {
        (RecurrenceKind::Standalone, None) => None,
        (RecurrenceKind::Series, Some(rule))
            if event.source_message_id.is_none()
                && rule.is_ascii()
                && !rule.contains(['\r', '\n'])
                && crate::calendar::recurrence::rrule_to_jscalendar(
                    rule,
                    event.timezone.as_deref(),
                )
                .is_some() =>
        {
            Some(rule.trim().strip_prefix("RRULE:").unwrap_or(rule.trim()))
        }
        _ => return Err(unsupported()),
    };
    let mut raw = crate::mail::caldav::generate_ical_event(
        uid,
        &event.title,
        event.description.as_deref(),
        event.location.as_deref(),
        &event.start_time,
        &event.end_time,
        event.all_day,
        event.timezone.as_deref(),
    );
    if let Some(rule) = rule {
        raw = raw.replace("\r\nEND:VEVENT", &format!("\r\nRRULE:{rule}\r\nEND:VEVENT"));
    }
    Ok(raw)
}

/// Map the selected component, retaining the classification of its full resource.
fn from_caldav_event(
    event: &CalDavEvent,
    account: &AccountFull,
    calendar_id: &str,
) -> Option<CalendarEvent> {
    let parsed = ical::parse_ical_data(&event.ical_data);
    let invite = parsed.first()?;
    let attendees_json = if invite.attendees.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&invite.attendees).unwrap_or_else(|_| "[]".to_string()))
    };

    Some(CalendarEvent {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: account.id.clone(),
        calendar_id: calendar_id.to_string(),
        uid: Some(event.uid.clone()),
        title: invite
            .summary
            .clone()
            .unwrap_or_else(|| "(No title)".to_string()),
        description: invite.description.clone(),
        location: invite.location.clone(),
        start_time: invite.dtstart.clone(),
        end_time: invite.dtend.clone(),
        all_day: invite.all_day,
        timezone: invite.timezone.clone(),
        recurrence_rule: invite.recurrence_rule.clone(),
        recurrence_kind: invite.recurrence_kind,
        organizer_email: invite.organizer_email.clone(),
        attendees_json,
        my_status: attendee_status_for_email(&invite.attendees, &account.email),
        source_message_id: None,
        ical_data: Some(event.ical_data.clone()),
        remote_id: Some(event.href.clone()),
        etag: Some(event.etag.clone()),
    })
}

/// Connect with the account's DAV coordinates.
pub(super) async fn connect(
    ctx: &CalendarBackendCtx<'_>,
    account: &AccountFull,
) -> Result<CalDavClient> {
    let caldav_config = CalDavConfig {
        caldav_url: account.caldav_url.clone(),
        username: account.username.clone(),
        password: account.password.clone(),
        email: account.email.clone(),
    };
    ctx.services.caldav_client(&caldav_config).await
}

#[async_trait]
impl CalendarBackend for CalDavCalendarBackend {
    fn protocol(&self) -> &'static str {
        "caldav"
    }

    async fn sync(&self, ctx: &CalendarBackendCtx<'_>, account: &AccountFull) -> Result<()> {
        let db = ctx.db;
        let account_id = account.id.as_str();
        let client = connect(ctx, account).await?;

        // Step 1: List calendars from server
        let caldav_calendars = client.list_calendars().await?;
        log::info!(
            "sync_calendars: fetched {} calendars from CalDAV for account {}",
            caldav_calendars.len(),
            account_id
        );

        // Build a mapping from remote calendar href to (local id, is_subscribed)
        // — same shape as the Graph sync (#47). The is_subscribed flag
        // is preserved across re-syncs by upsert_calendar_by_remote_id, so
        // we read it back from the DB after upserting and use it below to
        // skip event sync for calendars the user has unsubscribed from.
        // Without this skip, unsubscribing a calendar drops its events
        // (via unsubscribe_calendar) but the very next sync re-pulls them
        // and they show up as "ghost" events.
        let mut remote_to_local: std::collections::HashMap<String, (String, bool)> =
            std::collections::HashMap::new();

        {
            let conn = db.writer().await;
            for (idx, cal) in caldav_calendars.iter().enumerate() {
                let color = cal.color.as_deref().unwrap_or("#4285f4");
                let is_default = idx == 0; // First calendar is default
                let local_id = db::calendar::upsert_calendar_by_remote_id(
                    &conn, account_id, &cal.href, &cal.name, color, is_default,
                )?;
                // Propagate the read error rather than swallowing it as
                // `subscribed = true`: the row was just upserted above so a
                // failure here means the DB itself is misbehaving and the
                // sync should abort rather than blindly re-pull events the
                // user has unsubscribed from.
                let subscribed: bool = conn.query_row(
                    "SELECT is_subscribed FROM calendars WHERE id = ?1",
                    rusqlite::params![local_id],
                    |row| row.get(0),
                )?;
                remote_to_local.insert(cal.href.clone(), (local_id, subscribed));
            }
        }

        // Step 2: For each subscribed calendar, fetch events and upsert into local DB
        for cal in &caldav_calendars {
            let Some((local_cal_id, subscribed)) = remote_to_local.get(&cal.href) else {
                continue;
            };
            if !subscribed {
                log::debug!(
                    "sync_calendars_caldav: skipping unsubscribed calendar '{}'",
                    cal.name
                );
                continue;
            }
            let caldav_events = match client.fetch_events(&cal.href).await {
                Ok(evts) => evts,
                Err(e) => {
                    log::error!(
                        "sync_calendars: failed to fetch CalDAV events for calendar '{}': {}",
                        cal.name,
                        e
                    );
                    continue;
                }
            };

            log::info!(
                "sync_calendars: fetched {} events from CalDAV calendar '{}'",
                caldav_events.len(),
                cal.name
            );

            let mut conn = db.writer().await;
            for ev in &caldav_events {
                // Reparse even with an unchanged etag: legacy rows need source
                // classification, and the shared upsert persists it on refresh.
                let Some(cal_event) = from_caldav_event(ev, account, local_cal_id) else {
                    log::debug!(
                        "sync_calendars: could not parse iCal data for event href={}",
                        ev.href
                    );
                    continue;
                };

                if let Err(e) = db::calendar::upsert_event_by_remote_id(&conn, &cal_event) {
                    log::error!(
                        "sync_calendars: failed to upsert CalDAV event '{}': {}",
                        cal_event.title,
                        e
                    );
                }
            }

            // Remove local events with remote_id that no longer exist on server
            let server_hrefs: std::collections::HashSet<String> =
                caldav_events.iter().map(|e| e.href.clone()).collect();
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
                .filter(|(_, remote_id)| !server_hrefs.contains(remote_id))
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
                    "sync_calendars: removed {} server-deleted events from CalDAV calendar '{}'",
                    deleted,
                    cal.name
                );
            }
        }

        // Step 3: Push local events with no remote_id to CalDAV
        let mut push_failures = Vec::new();
        {
            let conn = db.writer().await;
            let local_events: Vec<CalendarEvent> = get_unpushed_events(&conn, account_id)?;

            if !local_events.is_empty() {
                log::info!(
                    "sync_calendars: pushing {} local events to CalDAV",
                    local_events.len()
                );
                drop(conn); // Release lock for async calls

                for ev in &local_events {
                    if let Err(error) =
                        push_initial_event(db, &client, account_id, &ev.id, &remote_to_local).await
                    {
                        push_failures.push(format!("{} ({}): {error}", ev.title, ev.id));
                    }
                }
            }
        }

        if push_failures.is_empty() {
            Ok(())
        } else {
            Err(Error::Sync(format!(
                "CalDAV calendar sync could not complete initial upload for {} local event(s): {}",
                push_failures.len(),
                push_failures.join("; ")
            )))
        }
    }

    fn validate_event_creation(&self, event: &CalendarEvent, _: &str) -> Result<()> {
        creation_ical_data(event, event.uid.as_deref().unwrap_or(&event.id)).map(|_| ())
    }

    /// CalDAV events are not pushed at create time — the next sync's
    /// unpushed-rows pass PUTs them (see `sync` step 3).
    async fn push_created_event(
        &self,
        _ctx: &CalendarBackendCtx<'_>,
        _account: &AccountFull,
        _event: &CalendarEvent,
        _remote_calendar_id: &str,
    ) -> Result<Option<PushedEvent>> {
        Ok(None)
    }

    async fn push_deleted_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        _remote_calendar_id: &str,
    ) -> Result<()> {
        let client = connect(ctx, account).await?;
        client.delete_event(remote_id).await
    }

    async fn push_calendar_rename(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        name: &str,
    ) -> Result<()> {
        let client = connect(ctx, account).await?;
        client.rename_calendar(remote_id, name).await
    }

    async fn push_calendar_color(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        color: &str,
    ) -> Result<()> {
        let client = connect(ctx, account).await?;
        client.set_calendar_color(remote_id, color).await
    }
}

#[cfg(test)]
mod recurrence_tests {
    use super::*;
    use crate::backend::testutil::{account, event, temp_pool};
    use crate::calendar::RecurrenceKind;
    use crate::db;
    use crate::db::pool::DbPool;
    use crate::mail::caldav::CalDavEvent;
    use crate::provider::ProviderServices;
    use rusqlite::Connection;
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn component(properties: &str) -> String {
        format!(
            "BEGIN:VEVENT\nUID:shared\nDTSTAMP:20260901T120000Z\n\
             DTSTART:20260913T100000Z\nSUMMARY:Visible event\n{properties}END:VEVENT\n"
        )
    }

    fn resource(components: &str) -> CalDavEvent {
        CalDavEvent {
            href: "/calendar/event.ics".into(),
            etag: "unchanged-etag".into(),
            uid: "shared".into(),
            ical_data: format!(
                "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//Chithi//EN\n\
                 {components}END:VCALENDAR\n"
            ),
        }
    }

    async fn upload_db() -> (tempfile::TempDir, DbPool) {
        let (dir, db) = temp_pool();
        {
            let conn = db.writer().await;
            db::schema::initialize(&conn).unwrap();
            conn.execute_batch(
                "INSERT INTO accounts (id, display_name, email, username)
                 VALUES ('acc1', 'Test', 'u@example.com', 'u@example.com');
                 INSERT INTO calendars (id, account_id, name, remote_id)
                 VALUES ('cal1', 'acc1', 'Calendar', '/calendar/');",
            )
            .unwrap();
        }
        (dir, db)
    }

    fn local_series() -> CalendarEvent {
        CalendarEvent {
            recurrence_kind: RecurrenceKind::Series,
            recurrence_rule: Some("FREQ=WEEKLY;COUNT=4".into()),
            ..event()
        }
    }

    fn insert_proven_series(conn: &mut Connection, event: &CalendarEvent) {
        let transaction = conn.transaction().unwrap();
        db::calendar::insert_event(&transaction, event).unwrap();
        db::calendar_invitation::record_local_series(&transaction, event).unwrap();
        transaction.commit().unwrap();
        assert!(db::calendar_invitation::validated_series_rule(conn, event).is_ok());
    }

    fn proof_count(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM calendar_invitation_recurrence",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn destinations() -> HashMap<String, (String, bool)> {
        HashMap::from([("/calendar/".into(), ("cal1".into(), true))])
    }

    fn injected_services() -> ProviderServices {
        let mut services = crate::backend::calendar::google::sync_testutil::services("");
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-injected-client",
            reqwest::header::HeaderValue::from_static("caldav-upload-test"),
        );
        services.transports.dav_http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .default_headers(headers)
            .build()
            .unwrap();
        services
    }

    fn multistatus(body: &str) -> String {
        format!(
            "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">\
             {body}</d:multistatus>"
        )
    }

    fn sync_responses(events: &str) -> Vec<(&'static str, u16, String)> {
        vec![
            (
                "PROPFIND",
                207,
                multistatus(
                    "<d:response><d:propstat><d:prop><d:current-user-principal>\
                 <d:href>/principal/</d:href></d:current-user-principal>\
                 </d:prop></d:propstat></d:response>",
                ),
            ),
            (
                "PROPFIND",
                207,
                multistatus(
                    "<d:response><d:propstat><d:prop><c:calendar-home-set>\
                 <d:href>/home/</d:href></c:calendar-home-set>\
                 </d:prop></d:propstat></d:response>",
                ),
            ),
            (
                "PROPFIND",
                207,
                multistatus(
                    "<d:response><d:href>/calendar/</d:href><d:propstat><d:prop>\
                 <d:displayname>Calendar</d:displayname>\
                 <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>\
                 </d:prop></d:propstat></d:response>",
                ),
            ),
            ("REPORT", 207, multistatus(events)),
        ]
    }

    async fn serve_dav(
        responses: Vec<(&'static str, u16, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}/dav/", listener.local_addr().unwrap());
        let captured = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (method, status, body) in responses {
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(5), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0, "request ended before its headers and body");
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
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
                assert_eq!(request.split_whitespace().next(), Some(method), "{request}");
                assert!(request.contains("x-injected-client: caldav-upload-test\r\n"));
                requests.push(request);
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/xml\r\n\
                     ETag: \"uploaded-etag\"\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (root, captured)
    }

    async fn sync_at(db: &DbPool, root: &str) -> Result<()> {
        let mut account = account("calendar", "caldav");
        account.caldav_url = root.into();
        CalDavCalendarBackend
            .sync(
                &CalendarBackendCtx {
                    db,
                    services: &injected_services(),
                },
                &account,
            )
            .await
    }

    async fn client_at(root: &str) -> CalDavClient {
        injected_services()
            .caldav_client(&CalDavConfig {
                caldav_url: root.into(),
                username: "user".into(),
                password: "pass".into(),
                email: "u@example.com".into(),
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn initial_series_put_preserves_proof_until_real_provider_read() {
        let (_dir, db) = upload_db().await;
        let local = local_series();
        {
            let mut conn = db.writer().await;
            insert_proven_series(&mut conn, &local);
        }
        let mut responses = sync_responses("");
        responses.push(("PUT", 201, String::new()));
        let (root, captured) = serve_dav(responses).await;
        sync_at(&db, &root).await.unwrap();
        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), 5);
        let uploaded = requests[4].split_once("\r\n\r\n").unwrap().1;
        let parsed = ical::parse_ical_data(uploaded);
        assert_eq!(parsed[0].recurrence_kind, RecurrenceKind::Series);
        assert_eq!(parsed[0].recurrence_rule, local.recurrence_rule);
        let attached = db::calendar::get_event(&db.reader(), &local.id).unwrap();
        let uid = attached.uid.as_ref().unwrap();
        assert!(requests[4].starts_with(&format!("PUT /calendar/{uid}.ics HTTP/1.1\r\n")));
        assert!(uploaded.contains(&format!("UID:{uid}\r\n")));
        let expected = CalendarEvent {
            uid: Some(uid.clone()),
            remote_id: Some(format!("/calendar/{uid}.ics")),
            etag: Some("\"uploaded-etag\"".into()),
            ..local
        };
        assert_eq!(attached, expected);
        assert!(attached.ical_data.is_none());
        assert_eq!(
            db::calendar_invitation::validated_series_rule(&db.reader(), &attached).unwrap(),
            "FREQ=WEEKLY;COUNT=4"
        );
        assert_eq!(proof_count(&db.reader()), 1);

        // A real REPORT supplies authoritative source even with the same RRULE/etag.
        let provider_source = uploaded.replace("\r\n", "\n").trim().to_string();
        let report = format!(
            "<d:response><d:href>{}</d:href><d:propstat><d:prop>\
             <d:getetag>\"uploaded-etag\"</d:getetag>\
             <c:calendar-data><![CDATA[{provider_source}]]></c:calendar-data>\
             </d:prop></d:propstat></d:response>",
            attached.remote_id.as_ref().unwrap()
        );
        let (root, captured) = serve_dav(sync_responses(&report)).await;
        sync_at(&db, &root).await.unwrap();
        assert_eq!(captured.await.unwrap().len(), 4);
        let conn = db.reader();
        let refreshed = db::calendar::get_event(&conn, &attached.id).unwrap();
        assert_eq!(refreshed.recurrence_rule, attached.recurrence_rule);
        assert_eq!(
            refreshed.ical_data.as_deref(),
            Some(provider_source.as_str())
        );
        assert_eq!(refreshed.remote_id, attached.remote_id);
        assert_eq!(refreshed.etag.as_deref(), Some("uploaded-etag"));
        assert!(db::calendar_invitation::validated_series_rule(&conn, &refreshed).is_err());
        assert_eq!(proof_count(&conn), 0);
        assert!(get_unpushed_events(&conn, "acc1").unwrap().is_empty());
    }

    #[tokio::test]
    async fn initial_put_preserves_raw_sources_and_never_infers_proof() {
        let (_dir, db) = upload_db().await;
        let mut originals = vec![
            event(),
            CalendarEvent {
                id: "unproven".into(),
                ..local_series()
            },
        ];
        for (id, kind, properties) in [
            ("raw-standalone", RecurrenceKind::Standalone, ""),
            (
                "raw-series",
                RecurrenceKind::Series,
                "RDATE:20260920T100000Z\nEXDATE:20260927T100000Z\n",
            ),
            (
                "raw-occurrence",
                RecurrenceKind::Occurrence,
                "RECURRENCE-ID:20260920T100000Z\n",
            ),
        ] {
            originals.push(CalendarEvent {
                id: id.into(),
                uid: Some(id.into()),
                recurrence_kind: kind,
                ical_data: Some(
                    resource(&component(properties))
                        .ical_data
                        .replace("UID:shared", &format!("UID:{id}")),
                ),
                source_message_id: Some("original-message".into()),
                remote_id: Some(String::new()),
                ..event()
            });
        }
        {
            let conn = db.writer().await;
            for event in &originals {
                db::calendar::insert_event(&conn, event).unwrap();
            }
            assert_eq!(get_unpushed_events(&conn, "acc1").unwrap().len(), 5);
        }
        let mut responses = sync_responses("");
        responses.extend((0..originals.len()).map(|_| ("PUT", 201, String::new())));
        let (root, captured) = serve_dav(responses).await;
        sync_at(&db, &root).await.unwrap();
        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), 9);
        let conn = db.reader();
        for original in originals {
            let attached = db::calendar::get_event(&conn, &original.id).unwrap();
            let uid = attached.uid.as_ref().unwrap();
            let request = requests
                .iter()
                .find(|request| {
                    request.starts_with(&format!("PUT /calendar/{uid}.ics HTTP/1.1\r\n"))
                })
                .unwrap();
            if let Some(raw) = &original.ical_data {
                assert_eq!(request.split_once("\r\n\r\n").unwrap().1, raw);
            }
            assert_eq!(
                attached,
                CalendarEvent {
                    uid: Some(uid.clone()),
                    remote_id: Some(format!("/calendar/{uid}.ics")),
                    etag: Some("\"uploaded-etag\"".into()),
                    ..original
                }
            );
            assert!(db::calendar_invitation::validated_series_rule(&conn, &attached).is_err());
        }
        assert_eq!(proof_count(&conn), 0);
        assert!(get_unpushed_events(&conn, "acc1").unwrap().is_empty());
    }

    #[tokio::test]
    async fn unsafe_unpushed_rows_fail_before_any_upload_http() {
        let (_dir, db) = upload_db().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client_at(&format!("http://{}/dav/", listener.local_addr().unwrap())).await;
        let mut invalid = vec![
            CalendarEvent {
                recurrence_kind: RecurrenceKind::Unknown,
                ..event()
            },
            CalendarEvent {
                recurrence_kind: RecurrenceKind::Occurrence,
                ..event()
            },
            CalendarEvent {
                recurrence_kind: RecurrenceKind::Series,
                ..event()
            },
            CalendarEvent {
                source_message_id: Some("message".into()),
                ..local_series()
            },
            CalendarEvent {
                ical_data: Some(String::new()),
                ..local_series()
            },
            CalendarEvent {
                ical_data: Some(resource(&component("")).ical_data),
                ..local_series()
            },
            CalendarEvent {
                recurrence_rule: Some("FREQ=WEEKLY".into()),
                ..event()
            },
        ];
        for rule in [
            "",
            "FREQ=INVALID",
            "FREQ=WEEKLY;BYDAY=MÖ",
            "FREQ=WEEKLY\r\nEXDATE:20260920T100000Z",
        ] {
            invalid.push(CalendarEvent {
                recurrence_rule: Some(rule.into()),
                ..local_series()
            });
        }
        {
            let conn = db.writer().await;
            for (index, event) in invalid.iter_mut().enumerate() {
                event.id = format!("invalid-{index}");
                db::calendar::insert_event(&conn, event).unwrap();
            }
        }
        let loaded = get_unpushed_events(&db.reader(), "acc1").unwrap();
        assert_eq!(loaded.len(), 11);
        for event in loaded {
            let revision = db::calendar_revision::get(&db.reader(), &event.id).unwrap();
            let error = push_initial_event(&db, &client, "acc1", &event.id, &destinations())
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("complete recurrence data is unavailable"),
                "{error}"
            );
            assert_eq!(
                db::calendar::get_event(&db.reader(), &event.id).unwrap(),
                event
            );
            assert_eq!(
                db::calendar_revision::get(&db.reader(), &event.id).unwrap(),
                revision
            );
        }
        assert_eq!(proof_count(&db.reader()), 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn stale_unpushed_candidates_are_reloaded_and_rejected_before_http() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client_at(&format!("http://{}/dav/", listener.local_addr().unwrap())).await;
        for change in [
            "UPDATE calendar_events SET recurrence_kind = 'unknown', ical_data = 'new source'",
            "UPDATE calendar_events SET remote_id = 'already-pushed'",
            "UPDATE calendar_events SET calendar_id = 'unavailable'",
            "DELETE FROM calendar_event_revisions",
            "DELETE FROM calendar_events",
        ] {
            let (_dir, db) = upload_db().await;
            let candidate = {
                let mut conn = db.writer().await;
                insert_proven_series(&mut conn, &local_series());
                let loaded = get_unpushed_events(&conn, "acc1").unwrap().remove(0);
                conn.execute(change, []).unwrap();
                loaded
            };
            let before = db::calendar::get_event(&db.reader(), &candidate.id).ok();
            let proofs = proof_count(&db.reader());
            assert!(
                push_initial_event(&db, &client, "acc1", &candidate.id, &destinations())
                    .await
                    .is_err(),
                "{change}"
            );
            assert_eq!(
                db::calendar::get_event(&db.reader(), &candidate.id).ok(),
                before
            );
            assert_eq!(proof_count(&db.reader()), proofs);
        }
        let (_dir, db) = upload_db().await;
        db::calendar::insert_event(&*db.writer().await, &event()).unwrap();
        assert!(
            push_initial_event(&db, &client, "another-account", "e1", &destinations())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn upload_uses_fresh_payload_uid_and_destination_after_candidate_loading() {
        let (_dir, db) = upload_db().await;
        let candidate = {
            let conn = db.writer().await;
            db::calendar::insert_event(&conn, &event()).unwrap();
            let candidate = get_unpushed_events(&conn, "acc1").unwrap().remove(0);
            conn.execute(
                "UPDATE calendar_events SET title = 'Fresh title', uid = 'fresh-uid', calendar_id = 'cal2'",
                [],
            ).unwrap();
            candidate
        };
        let (root, captured) = serve_dav(vec![("PUT", 201, String::new())]).await;
        let client = client_at(&root).await;
        let mapping = HashMap::from([
            ("/old/".into(), ("cal1".into(), true)),
            ("/fresh/".into(), ("cal2".into(), true)),
        ]);
        push_initial_event(&db, &client, "acc1", &candidate.id, &mapping)
            .await
            .unwrap();
        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("PUT /fresh/fresh-uid.ics HTTP/1.1\r\n"));
        assert!(requests[0].contains("SUMMARY:Fresh title\r\n"));
        assert!(requests[0].contains("UID:fresh-uid\r\n"));
        assert!(!requests[0].contains("SUMMARY:Standup"));
        let attached = db::calendar::get_event(&db.reader(), &candidate.id).unwrap();
        assert_eq!(attached.remote_id.as_deref(), Some("/fresh/fresh-uid.ics"));
        assert!(attached.ical_data.is_none());
    }

    #[tokio::test]
    async fn post_put_snapshot_check_preserves_concurrent_refresh_and_revoked_proof() {
        for change in [
            "provider-source",
            "proof-clear",
            "noop",
            "aba",
            "hidden",
            "replace",
            "delete",
            "missing-token",
        ] {
            let (_dir, db) = upload_db().await;
            let mut conn = db.writer().await;
            let mut local = local_series();
            local.uid = Some("shared".into());
            insert_proven_series(&mut conn, &local);
            let snapshot = capture_initial_upload(&mut conn, "acc1", &local.id).unwrap();
            assert_eq!(snapshot.event, local);
            assert_eq!(
                snapshot.revision,
                db::calendar_revision::get(&conn, &local.id).unwrap()
            );
            match change {
                "provider-source" => {
                    let mut remote = local.clone();
                    remote.remote_id = Some("/calendar/provider.ics".into());
                    remote.etag = Some("provider-etag".into());
                    remote.ical_data = Some(
                        resource(&component(
                            "RRULE:FREQ=WEEKLY;COUNT=4\nEXDATE:20260920T100000Z\n",
                        ))
                        .ical_data,
                    );
                    db::calendar::upsert_event_by_remote_id(&conn, &remote).unwrap();
                    assert_eq!(proof_count(&conn), 0);
                }
                "proof-clear" => {
                    let transaction = conn.transaction().unwrap();
                    transaction
                        .execute("UPDATE calendar_events SET title = title", [])
                        .unwrap();
                    db::calendar_invitation::invalidate(&transaction, &local.id).unwrap();
                    transaction.commit().unwrap();
                    assert_eq!(db::calendar::get_event(&conn, &local.id).unwrap(), local);
                    assert_eq!(proof_count(&conn), 0);
                }
                "noop" => {
                    conn.execute("UPDATE calendar_events SET title = title", [])
                        .unwrap();
                }
                "aba" => {
                    conn.execute("UPDATE calendar_events SET title = 'Changed'", [])
                        .unwrap();
                    conn.execute("UPDATE calendar_events SET title = ?1", [&local.title])
                        .unwrap();
                }
                "hidden" => {
                    conn.execute(
                        "UPDATE calendar_events SET pending_rsvp_status = 'accepted'",
                        [],
                    )
                    .unwrap();
                }
                "replace" => {
                    conn.execute(
                        "INSERT OR REPLACE INTO calendar_events SELECT * FROM calendar_events",
                        [],
                    )
                    .unwrap();
                }
                "delete" => {
                    conn.execute("DELETE FROM calendar_events", []).unwrap();
                }
                "missing-token" => {
                    conn.execute("DELETE FROM calendar_event_revisions", [])
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let before = db::calendar::get_event(&conn, &local.id).ok();
            let revision = db::calendar_revision::get(&conn, &local.id).ok();
            let proofs = proof_count(&conn);
            assert_ne!(revision, Some(snapshot.revision), "{change}");
            let error = persist_initial_upload(
                &mut conn,
                &snapshot,
                "/calendar/uploaded.ics",
                "uploaded-etag",
                "uploaded-uid",
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("CalDAV accepted the initial PUT"), "{error}");
            assert!(
                error.contains("remote upload has not been undone"),
                "{error}"
            );
            assert_eq!(
                db::calendar::get_event(&conn, &local.id).ok(),
                before,
                "{change}"
            );
            assert_eq!(
                db::calendar_revision::get(&conn, &local.id).ok(),
                revision,
                "{change}"
            );
            assert_eq!(proof_count(&conn), proofs, "{change}");
        }
    }

    #[tokio::test]
    async fn failed_put_or_metadata_save_preserves_local_event_and_proof() {
        for (status, reject_save) in [(500, false), (201, true)] {
            let (_dir, db) = upload_db().await;
            let local = local_series();
            let revision = {
                let mut conn = db.writer().await;
                insert_proven_series(&mut conn, &local);
                if reject_save {
                    conn.execute_batch(
                        "CREATE TRIGGER reject_upload_metadata BEFORE UPDATE OF remote_id ON calendar_events
                         BEGIN SELECT RAISE(ABORT, 'injected save failure'); END;",
                    ).unwrap();
                }
                db::calendar_revision::get(&conn, &local.id).unwrap()
            };
            let mut responses = sync_responses("");
            responses.push(("PUT", status, String::new()));
            let (root, captured) = serve_dav(responses).await;
            let error = sync_at(&db, &root).await.unwrap_err().to_string();
            assert!(
                error.contains("could not complete initial upload for 1 local event(s)"),
                "{error}"
            );
            assert_eq!(
                error.contains("CalDAV accepted the initial PUT"),
                reject_save,
                "{error}"
            );
            assert_eq!(captured.await.unwrap().len(), 5);
            let conn = db.reader();
            assert_eq!(db::calendar::get_event(&conn, &local.id).unwrap(), local);
            assert_eq!(
                db::calendar_revision::get(&conn, &local.id).unwrap(),
                revision
            );
            assert!(db::calendar_invitation::validated_series_rule(&conn, &local).is_ok());
            assert_eq!(proof_count(&conn), 1);
        }
    }

    #[test]
    fn deferred_creation_preserves_source_recurrence_and_never_invents_standalone() {
        let mut event = crate::backend::testutil::event();
        for kind in [
            RecurrenceKind::Unknown,
            RecurrenceKind::Occurrence,
            RecurrenceKind::Series,
        ] {
            event.recurrence_kind = kind;
            assert!(creation_ical_data(&event, "uid").is_err());
        }
        event.recurrence_kind = RecurrenceKind::Standalone;
        let raw = creation_ical_data(&event, "uid").unwrap();
        assert_eq!(
            crate::calendar::ical::parse_ical_data(&raw)[0].recurrence_kind,
            RecurrenceKind::Standalone
        );
        event.recurrence_kind = RecurrenceKind::Series;
        event.recurrence_rule = Some("FREQ=WEEKLY;COUNT=4".into());
        let raw = creation_ical_data(&event, "uid").unwrap();
        let parsed = crate::calendar::ical::parse_ical_data(&raw);
        assert_eq!(parsed[0].recurrence_kind, RecurrenceKind::Series);
        assert_eq!(parsed[0].recurrence_rule, event.recurrence_rule);

        for (kind, properties) in [
            (
                RecurrenceKind::Occurrence,
                "RECURRENCE-ID:20260920T100000Z\n",
            ),
            (
                RecurrenceKind::Series,
                "RDATE:20260920T100000Z\nEXDATE:20260927T100000Z\n",
            ),
        ] {
            let raw = resource(&component(properties)).ical_data;
            event.recurrence_kind = kind;
            event.recurrence_rule = None;
            event.ical_data = Some(raw.clone());
            assert_eq!(creation_ical_data(&event, "uid").unwrap(), raw);
            event.recurrence_kind = RecurrenceKind::Unknown;
            assert!(creation_ical_data(&event, "uid").is_err());
        }
    }

    #[test]
    fn selected_row_retains_resource_recurrence_classification() {
        let account = account("calendar", "caldav");
        let master = component("RRULE:FREQ=WEEKLY\n");
        let occurrence = component("RECURRENCE-ID:20260920T100000Z\n");
        let standalone = component("");
        for (components, kind) in [
            (standalone.clone(), RecurrenceKind::Standalone),
            (format!("{master}{occurrence}"), RecurrenceKind::Series),
            (format!("{occurrence}{master}"), RecurrenceKind::Occurrence),
            (format!("{standalone}{occurrence}"), RecurrenceKind::Unknown),
            (format!("{standalone}{standalone}"), RecurrenceKind::Unknown),
        ] {
            let resource = resource(&components);
            let event = from_caldav_event(&resource, &account, "calendar-1").unwrap();
            assert_eq!(event.recurrence_kind, kind);
            assert_eq!(
                event.ical_data.as_deref(),
                Some(resource.ical_data.as_str())
            );
            assert_eq!(event.remote_id.as_deref(), Some(resource.href.as_str()));
            assert_eq!(event.title, "Visible event");
        }
    }

    #[test]
    fn unchanged_etag_refresh_recovers_unknown_classification_on_existing_row() {
        let conn = Connection::open_in_memory().unwrap();
        db::schema::initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('acc1', 'Test', 'u@example.com', 'u@example.com')",
            [],
        )
        .unwrap();
        let account = account("calendar", "caldav");
        let calendar_id = db::calendar::upsert_calendar_by_remote_id(
            &conn,
            &account.id,
            "/calendar/",
            "Calendar",
            "#4285f4",
            true,
        )
        .unwrap();
        let resource = resource(&component(""));
        let mut legacy = from_caldav_event(&resource, &account, &calendar_id).unwrap();
        legacy.recurrence_kind = RecurrenceKind::Unknown;
        db::calendar::upsert_event_by_remote_id(&conn, &legacy).unwrap();

        let refreshed = from_caldav_event(&resource, &account, &calendar_id).unwrap();
        assert_eq!(legacy.etag, refreshed.etag);
        db::calendar::upsert_event_by_remote_id(&conn, &refreshed).unwrap();
        let stored: (String, String, String) = conn
            .query_row(
                "SELECT id, recurrence_kind, etag FROM calendar_events WHERE remote_id = ?1",
                [&resource.href],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(stored, (legacy.id, "standalone".into(), resource.etag));
    }
}
