//! CalDAV calendar backend (RFC 4791).

use async_trait::async_trait;

use crate::calendar::ical;
use crate::calendar::CalendarEvent;
use crate::db;
use crate::db::accounts::AccountFull;
use crate::error::Result;
use crate::mail::caldav::{CalDavClient, CalDavConfig};

use super::{get_unpushed_events, CalendarBackend, CalendarBackendCtx, PushedEvent};

pub struct CalDavCalendarBackend;

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
                // Parse the iCalendar data to extract event details
                let parsed = ical::parse_ical_data(&ev.ical_data);
                if parsed.is_empty() {
                    log::debug!(
                        "sync_calendars: could not parse iCal data for event href={}",
                        ev.href
                    );
                    continue;
                }
                let invite = &parsed[0];

                let attendees_json = if invite.attendees.is_empty() {
                    None
                } else {
                    Some(
                        serde_json::to_string(&invite.attendees)
                            .unwrap_or_else(|_| "[]".to_string()),
                    )
                };

                let event_id = uuid::Uuid::new_v4().to_string();
                let cal_event = CalendarEvent {
                    id: event_id,
                    account_id: account_id.to_string(),
                    calendar_id: local_cal_id.clone(),
                    uid: Some(ev.uid.clone()),
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
                    organizer_email: invite.organizer_email.clone(),
                    attendees_json,
                    my_status: None,
                    source_message_id: None,
                    ical_data: Some(ev.ical_data.clone()),
                    remote_id: Some(ev.href.clone()),
                    etag: Some(ev.etag.clone()),
                };

                if let Err(e) = db::calendar::upsert_event_by_remote_id(&conn, &cal_event) {
                    log::error!(
                        "sync_calendars: failed to upsert CalDAV event '{}': {}",
                        invite.summary.as_deref().unwrap_or("?"),
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
                        log::warn!(
                            "sync_calendars: no remote CalDAV calendar for local event '{}'",
                            ev.title
                        );
                        continue;
                    }

                    let uid = ev
                        .uid
                        .clone()
                        .unwrap_or_else(|| format!("{}@chithi", uuid::Uuid::new_v4()));

                    // Use existing ical_data if available, or generate new
                    let ical_data = ev.ical_data.clone().unwrap_or_else(|| {
                        crate::mail::caldav::generate_ical_event(
                            &uid,
                            &ev.title,
                            ev.description.as_deref(),
                            ev.location.as_deref(),
                            &ev.start_time,
                            &ev.end_time,
                            ev.all_day,
                            ev.timezone.as_deref(),
                        )
                    });

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
                            conn.execute(
                                "UPDATE calendar_events SET remote_id = ?1, etag = ?2, uid = ?3, ical_data = ?4 WHERE id = ?5",
                                rusqlite::params![remote_id, etag, uid, ical_data, ev.id],
                            )
                            .ok();
                        }
                        Err(e) => {
                            log::error!(
                                "sync_calendars: failed to push event '{}' to CalDAV: {}",
                                ev.title,
                                e
                            );
                        }
                    }
                }
            }
        }

        Ok(())
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
