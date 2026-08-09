//! JMAP calendar backend (RFC 8984 JSCalendar via `CalendarEvent/*`).

use async_trait::async_trait;

use crate::db;
use crate::db::accounts::AccountFull;
use crate::db::calendar::CalendarEvent;
use crate::db::pool::DbPool;
use crate::error::Result;
use crate::mail::jmap::{JmapCalendarEvent, JmapConnection};

use super::{get_unpushed_events, CalendarBackend, InviteReplyDelivery, PushedEvent};

pub struct JmapCalendarBackend;

/// Build the wire event from a local row. `id` is empty for creates —
/// the server assigns one.
fn to_jmap_event(event: &CalendarEvent, remote_calendar_id: &str) -> JmapCalendarEvent {
    JmapCalendarEvent {
        id: String::new(),
        calendar_id: remote_calendar_id.to_string(),
        title: event.title.clone(),
        description: event.description.clone(),
        location: event.location.clone(),
        start: event.start_time.clone(),
        end: event.end_time.clone(),
        all_day: event.all_day,
        timezone: event.timezone.clone(),
        recurrence_rule: event.recurrence_rule.clone(),
        uid: event.uid.clone(),
        organizer_email: event.organizer_email.clone(),
        attendees_json: event.attendees_json.clone(),
    }
}

#[async_trait]
impl CalendarBackend for JmapCalendarBackend {
    fn protocol(&self) -> &'static str {
        "jmap"
    }

    fn invite_reply_delivery(&self) -> InviteReplyDelivery {
        InviteReplyDelivery::JmapSubmission
    }

    async fn sync(&self, db: &DbPool, account: &AccountFull) -> Result<()> {
        let account_id = account.id.as_str();
        let jmap_config = crate::auth::build_jmap_config(account).await?;

        let jmap_conn = JmapConnection::connect(&jmap_config).await?;

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

            let conn = db.writer().await;
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
                    organizer_email: ev.organizer_email.clone(),
                    attendees_json: ev.attendees_json.clone(),
                    my_status: None,
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

            let mut deleted = 0u32;
            for (local_id, remote_id) in &local_synced {
                if !server_ids.contains(remote_id) {
                    conn.execute(
                        "DELETE FROM calendar_events WHERE id = ?1",
                        rusqlite::params![local_id],
                    )
                    .ok();
                    deleted += 1;
                }
            }
            if deleted > 0 {
                log::info!(
                    "sync_calendars: removed {} server-deleted events from '{}'",
                    deleted,
                    jcal.name
                );
            }
        }

        // Step 3: Push local events (no remote_id) to the JMAP server
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
                        log::warn!(
                            "sync_calendars: no remote calendar for local event '{}'",
                            ev.title
                        );
                        continue;
                    }

                    let jmap_event = to_jmap_event(ev, &remote_cal_id);

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
                            conn.execute(
                                "UPDATE calendar_events SET remote_id = ?1 WHERE id = ?2",
                                rusqlite::params![remote_id, ev.id],
                            )
                            .ok();
                        }
                        Err(e) => {
                            log::error!(
                                "sync_calendars: failed to push event '{}': {}",
                                ev.title,
                                e
                            )
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn push_created_event(
        &self,
        account: &AccountFull,
        event: &CalendarEvent,
        remote_calendar_id: &str,
    ) -> Result<Option<PushedEvent>> {
        let jmap_config = crate::auth::build_jmap_config(account).await?;
        let conn_jmap = JmapConnection::connect(&jmap_config).await?;
        let jmap_event = to_jmap_event(event, remote_calendar_id);
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
        account: &AccountFull,
        remote_id: &str,
        _remote_calendar_id: &str,
    ) -> Result<()> {
        let jmap_config = crate::auth::build_jmap_config(account).await?;
        let conn_jmap = JmapConnection::connect(&jmap_config).await?;
        conn_jmap
            .delete_calendar_event(&jmap_config, remote_id)
            .await
    }

    async fn push_calendar_rename(
        &self,
        account: &AccountFull,
        remote_id: &str,
        name: &str,
    ) -> Result<()> {
        let jmap_config = crate::auth::build_jmap_config(account).await?;
        let conn_jmap = JmapConnection::connect(&jmap_config).await?;
        conn_jmap
            .rename_calendar(&jmap_config, remote_id, name)
            .await
    }

    async fn push_calendar_color(
        &self,
        account: &AccountFull,
        remote_id: &str,
        color: &str,
    ) -> Result<()> {
        let jmap_config = crate::auth::build_jmap_config(account).await?;
        let conn_jmap = JmapConnection::connect(&jmap_config).await?;
        conn_jmap
            .set_calendar_color(&jmap_config, remote_id, color)
            .await
    }
}

#[cfg(test)]
mod payload_tests {
    use super::to_jmap_event;
    use crate::backend::testutil::event;

    #[test]
    fn create_payload_leaves_id_to_server_and_targets_given_calendar() {
        let local = event();
        let wire = to_jmap_event(&local, "remote-cal-7");
        assert_eq!(wire.id, "");
        assert_eq!(wire.calendar_id, "remote-cal-7");
        assert_eq!(wire.title, local.title);
        assert_eq!(wire.start, local.start_time);
        assert_eq!(wire.end, local.end_time);
        assert!(!wire.all_day);
    }

    #[test]
    fn all_day_flag_carries_through() {
        let mut local = event();
        local.all_day = true;
        assert!(to_jmap_event(&local, "c").all_day);
    }
}
