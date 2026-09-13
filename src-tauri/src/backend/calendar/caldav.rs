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
                    // Find the remote calendar href for this event's local calendar
                    let remote_cal_href = remote_to_local
                        .iter()
                        .find(|(_, (local_id, _))| *local_id == ev.calendar_id)
                        .map(|(remote_href, _)| remote_href.clone())
                        .unwrap_or_default();

                    if remote_cal_href.is_empty() {
                        push_failures.push(format!(
                            "{} ({}): destination calendar is unavailable",
                            ev.title, ev.id
                        ));
                        continue;
                    }

                    let uid = ev
                        .uid
                        .clone()
                        .unwrap_or_else(|| format!("{}@chithi", uuid::Uuid::new_v4()));

                    let ical_data = match creation_ical_data(ev, &uid) {
                        Ok(raw) => raw,
                        Err(error) => {
                            push_failures.push(format!("{}: {error}", ev.id));
                            continue;
                        }
                    };

                    match client.put_event(&remote_cal_href, &uid, &ical_data).await {
                        Ok(etag) => {
                            let remote_id =
                                format!("{}/{}.ics", remote_cal_href.trim_end_matches('/'), uid);
                            log::info!(
                                "sync_calendars: pushed event '{}' to CalDAV, remote_id={}",
                                ev.title,
                                remote_id
                            );
                            let conn = db.writer().await;
                            if let Err(error) = conn.execute(
                                "UPDATE calendar_events SET remote_id = ?1, etag = ?2, uid = ?3, ical_data = ?4 WHERE id = ?5",
                                rusqlite::params![remote_id, etag, uid, ical_data, ev.id],
                            ) {
                                push_failures.push(format!("{} ({}): failed to save remote id: {error}", ev.title, ev.id));
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
                "CalDAV calendar sync could not create {} local event(s): {}",
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
    use super::{creation_ical_data, from_caldav_event};
    use crate::backend::testutil::account;
    use crate::calendar::RecurrenceKind;
    use crate::db;
    use crate::mail::caldav::CalDavEvent;
    use rusqlite::Connection;

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
