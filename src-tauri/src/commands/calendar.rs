use serde::Deserialize;
use tauri::State;

use crate::calendar::ical::{self, ParsedInvite};
use crate::db;
use crate::db::calendar::{Attendee, Calendar, CalendarEvent, NewCalendar};
use crate::error::Result;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct NewEventInput {
    pub account_id: String,
    pub calendar_id: String,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_time: String,
    pub end_time: String,
    pub all_day: bool,
    pub timezone: Option<String>,
    pub recurrence_rule: Option<String>,
    pub attendees: Vec<Attendee>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateEventInput {
    pub calendar_id: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub all_day: Option<bool>,
    pub timezone: Option<String>,
    pub recurrence_rule: Option<String>,
    pub attendees: Option<Vec<Attendee>>,
}

// ---------------------------------------------------------------------------
// Calendar management commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_calendars(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Vec<Calendar>> {
    log::debug!("list_calendars: account={}", account_id);
    let conn = state.db.reader();
    let calendars = db::calendar::list_calendars(&conn, &account_id)?;
    log::debug!("list_calendars: found {} calendars", calendars.len());
    Ok(calendars)
}

#[tauri::command]
pub async fn create_calendar(state: State<'_, AppState>, calendar: NewCalendar) -> Result<String> {
    log::info!(
        "create_calendar: account={} name='{}'",
        calendar.account_id,
        calendar.name
    );
    let id = uuid::Uuid::new_v4().to_string();
    let conn = state.db.writer().await;
    db::calendar::insert_calendar(&conn, &id, &calendar)?;
    log::info!("create_calendar: created calendar id={}", id);
    Ok(id)
}

#[tauri::command]
pub async fn update_calendar(
    state: State<'_, AppState>,
    calendar_id: String,
    name: String,
    color: String,
) -> Result<()> {
    log::info!(
        "update_calendar: id={} name='{}' color='{}'",
        calendar_id,
        name,
        color
    );

    // Load current calendar + account so we know whether the name changed
    // and, if so, which protocol to push the rename through. Drop the
    // reader before any await so the backend stays non-blocking.
    let (existing, account) = {
        let conn = state.db.reader();
        let cal = db::calendar::get_calendar(&conn, &calendar_id)?;
        let acct = db::accounts::get_account_full(&conn, &cal.account_id)?;
        (cal, acct)
    };

    let name_changed = existing.name != name;
    let remote_id = existing.remote_id.clone().filter(|r| !r.is_empty());
    if name_changed {
        if let Some(ref rid) = remote_id {
            push_calendar_rename(&account, rid, &name).await?;
        } else {
            log::info!(
                "update_calendar: skipping remote rename (no remote_id, local-only calendar)"
            );
        }
    }

    let conn = state.db.writer().await;
    db::calendar::update_calendar(&conn, &calendar_id, &name, &color)?;
    Ok(())
}

/// Push a calendar rename to the account's remote server. Mirrors the
/// per-protocol dispatch in [`sync_calendars`]. Errors here must propagate
/// so the command leaves the local DB unchanged on remote failure.
async fn push_calendar_rename(
    account: &db::accounts::AccountFull,
    remote_id: &str,
    new_name: &str,
) -> Result<()> {
    let cal_proto = account.calendar_protocol_str();
    if cal_proto == "jmap" {
        let jmap_config = crate::commands::sync_cmd::build_jmap_config(account).await?;
        let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
        conn_jmap
            .rename_calendar(&jmap_config, remote_id, new_name)
            .await?;
        return Ok(());
    }

    if cal_proto == "graph" {
        let token = crate::mail::graph::get_graph_token(&account.id).await?;
        let client = crate::mail::graph::GraphClient::new(&token);
        client.rename_calendar(remote_id, new_name).await?;
        return Ok(());
    }

    if cal_proto == "google" {
        // Prefer the Google Calendar REST endpoint; fall back to CalDAV
        // PROPPATCH if REST fails (OAuth not configured, or remote_id is
        // actually a CalDAV href).
        if let Ok(token) = get_google_token(&account.id).await {
            let url = format!(
                "https://www.googleapis.com/calendar/v3/calendars/{}",
                urlencoding::encode(remote_id)
            );
            let http = reqwest::Client::new();
            let resp = http
                .patch(&url)
                .bearer_auth(&token)
                .json(&serde_json::json!({ "summary": new_name }))
                .send()
                .await
                .map_err(|e| {
                    crate::error::Error::Other(format!("Google Calendar PATCH failed: {}", e))
                })?;
            if resp.status().is_success() {
                return Ok(());
            }
            let body = resp.text().await.unwrap_or_default();
            log::warn!(
                "update_calendar: Google REST rename failed ({}), falling back to CalDAV",
                body.chars().take(200).collect::<String>()
            );
        }
        // Fall through to CalDAV below.
    }

    if !account.caldav_url.is_empty() {
        use crate::mail::caldav::{CalDavClient, CalDavConfig};
        let caldav_config = CalDavConfig {
            caldav_url: account.caldav_url.clone(),
            username: account.username.clone(),
            password: account.password.clone(),
            email: account.email.clone(),
        };
        let client = CalDavClient::connect(&caldav_config).await?;
        client.rename_calendar(remote_id, new_name).await?;
        return Ok(());
    }

    Err(crate::error::Error::Other(format!(
        "No remote rename path configured for account {} (calendar_protocol={})",
        account.id,
        account.calendar_protocol_str()
    )))
}

#[tauri::command]
pub async fn delete_calendar(state: State<'_, AppState>, calendar_id: String) -> Result<()> {
    log::info!("delete_calendar: id={}", calendar_id);
    let conn = state.db.writer().await;
    db::calendar::delete_calendar(&conn, &calendar_id)?;
    log::info!("delete_calendar: deleted calendar {}", calendar_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Event management commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn unsubscribe_calendar(state: State<'_, AppState>, calendar_id: String) -> Result<()> {
    log::info!("unsubscribe_calendar: id={}", calendar_id);
    let conn = state.db.writer().await;
    db::calendar::set_calendar_subscribed(&conn, &calendar_id, false)?;
    let deleted = db::calendar::delete_calendar_events(&conn, &calendar_id)?;
    log::info!(
        "unsubscribe_calendar: deleted {} events for calendar {}",
        deleted,
        calendar_id
    );
    Ok(())
}

// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn get_events(
    state: State<'_, AppState>,
    account_id: String,
    start: String,
    end: String,
    calendar_id: Option<String>,
) -> Result<Vec<CalendarEvent>> {
    log::debug!(
        "get_events: account={} range={}..{} calendar={:?}",
        account_id,
        start,
        end,
        calendar_id
    );
    let conn = state.db.reader();
    let events =
        db::calendar::list_events(&conn, &account_id, calendar_id.as_deref(), &start, &end)?;
    log::debug!("get_events: found {} events", events.len());
    Ok(events)
}

#[tauri::command]
pub async fn create_event(state: State<'_, AppState>, event: NewEventInput) -> Result<String> {
    log::info!(
        "create_event: account={} calendar={} title='{}' attendees={}",
        event.account_id,
        event.calendar_id,
        event.title,
        event.attendees.len()
    );
    let id = uuid::Uuid::new_v4().to_string();

    let attendees_json = if event.attendees.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&event.attendees).unwrap_or_else(|_| "[]".to_string()))
    };

    // Get organizer email from account
    let organizer_email = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &event.account_id)
            .ok()
            .map(|a| a.email)
    };

    let cal_event = CalendarEvent {
        id: id.clone(),
        account_id: event.account_id,
        calendar_id: event.calendar_id,
        uid: Some(format!("{}@chithi", uuid::Uuid::new_v4())),
        title: event.title,
        description: event.description,
        location: event.location,
        start_time: event.start_time,
        end_time: event.end_time,
        all_day: event.all_day,
        timezone: event.timezone,
        recurrence_rule: event.recurrence_rule,
        organizer_email,
        attendees_json,
        my_status: None,
        source_message_id: None,
        ical_data: None,
        remote_id: None,
        etag: None,
    };

    // Insert locally first, then push to server
    {
        let conn = state.db.writer().await;
        db::calendar::insert_event(&conn, &cal_event)?;
        let account = db::accounts::get_account_full(&conn, &cal_event.account_id)?;

        if account.calendar_protocol_str() == "google" {
            // Create on Google Calendar via REST API
            drop(conn);
            if let Ok(token) = get_google_token(&cal_event.account_id).await {
                let http = reqwest::Client::new();
                let mut google_event = serde_json::json!({
                    "summary": cal_event.title,
                    "start": if cal_event.all_day {
                        serde_json::json!({"date": cal_event.start_time.split('T').next().unwrap_or_default()})
                    } else {
                        serde_json::json!({"dateTime": cal_event.start_time})
                    },
                    "end": if cal_event.all_day {
                        serde_json::json!({"date": cal_event.end_time.split('T').next().unwrap_or_default()})
                    } else {
                        serde_json::json!({"dateTime": cal_event.end_time})
                    },
                    "iCalUID": cal_event.uid,
                });
                if let Some(ref desc) = cal_event.description {
                    google_event["description"] = serde_json::json!(desc);
                }
                if let Some(ref loc) = cal_event.location {
                    google_event["location"] = serde_json::json!(loc);
                }
                if let Some(ref att_json) = cal_event.attendees_json {
                    if let Ok(atts) = serde_json::from_str::<Vec<serde_json::Value>>(att_json) {
                        let google_attendees: Vec<serde_json::Value> = atts
                            .iter()
                            .filter_map(|a| {
                                a["email"].as_str().map(|e| serde_json::json!({"email": e}))
                            })
                            .collect();
                        if !google_attendees.is_empty() {
                            google_event["attendees"] = serde_json::json!(google_attendees);
                        }
                    }
                }
                let send_updates = if cal_event.attendees_json.is_some() {
                    "all"
                } else {
                    "none"
                };
                let url = format!(
                    "https://www.googleapis.com/calendar/v3/calendars/primary/events?sendUpdates={}",
                    send_updates
                );
                match http
                    .post(&url)
                    .bearer_auth(&token)
                    .json(&google_event)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        if let Ok(data) = resp.json::<serde_json::Value>().await {
                            let remote_id = data["id"].as_str().unwrap_or_default().to_string();
                            log::info!("create_event: pushed to Google Calendar, id={}", remote_id);
                            let conn = state.db.writer().await;
                            conn.execute(
                                "UPDATE calendar_events SET remote_id = ?1 WHERE id = ?2",
                                rusqlite::params![remote_id, id],
                            )
                            .ok();
                            // Update local UID to match Google's iCalUID so RSVP
                            // replies can be matched back to the event.
                            if let Some(ical_uid) = data["iCalUID"].as_str() {
                                conn.execute(
                                    "UPDATE calendar_events SET uid = ?1 WHERE id = ?2",
                                    rusqlite::params![ical_uid, id],
                                )
                                .ok();
                                log::info!(
                                    "create_event: updated local UID to Google iCalUID={}",
                                    ical_uid
                                );
                            }
                        }
                    }
                    Ok(resp) => {
                        let body = resp.text().await.unwrap_or_default();
                        log::error!("create_event: Google Calendar insert failed: {}", body);
                    }
                    Err(e) => log::error!("create_event: Google Calendar request failed: {}", e),
                }
            }
        } else if account.calendar_protocol_str() == "graph" {
            drop(conn);
            if let Ok(token) = crate::mail::graph::get_graph_token(&cal_event.account_id).await {
                let client = crate::mail::graph::GraphClient::new(&token);
                let mut graph_event = serde_json::json!({
                    "subject": cal_event.title,
                    "start": if cal_event.all_day {
                        serde_json::json!({"dateTime": format!("{}T00:00:00", cal_event.start_time.split('T').next().unwrap_or_default()), "timeZone": "UTC"})
                    } else {
                        serde_json::json!({"dateTime": cal_event.start_time, "timeZone": "UTC"})
                    },
                    "end": if cal_event.all_day {
                        serde_json::json!({"dateTime": format!("{}T00:00:00", cal_event.end_time.split('T').next().unwrap_or_default()), "timeZone": "UTC"})
                    } else {
                        serde_json::json!({"dateTime": cal_event.end_time, "timeZone": "UTC"})
                    },
                    "isAllDay": cal_event.all_day,
                });
                if let Some(ref desc) = cal_event.description {
                    graph_event["body"] =
                        serde_json::json!({"contentType": "text", "content": desc});
                }
                if let Some(ref loc) = cal_event.location {
                    graph_event["location"] = serde_json::json!({"displayName": loc});
                }
                if let Some(ref att_json) = cal_event.attendees_json {
                    if let Ok(atts) = serde_json::from_str::<Vec<serde_json::Value>>(att_json) {
                        let mut graph_atts: Vec<serde_json::Value> = atts.iter()
                            .filter_map(|a| a["email"].as_str().map(|e| serde_json::json!({
                                "emailAddress": {"address": e, "name": a["name"].as_str().unwrap_or("")},
                                "type": "required",
                            })))
                            .collect();
                        // Add the organizer as an attendee with isOrganizer=true
                        if let Some(ref org_email) = cal_event.organizer_email {
                            graph_atts.push(serde_json::json!({
                                "emailAddress": {"address": org_email, "name": ""},
                                "type": "required",
                                "status": {"response": "organizer"},
                            }));
                        }
                        if !graph_atts.is_empty() {
                            graph_event["attendees"] = serde_json::json!(graph_atts);
                        }
                        log::info!(
                            "create_event: O365 event with {} attendees",
                            graph_atts.len()
                        );
                    }
                }
                // Graph sends invite emails automatically when attendees are present
                log::debug!(
                    "create_event: O365 graph_event JSON: {}",
                    serde_json::to_string_pretty(&graph_event).unwrap_or_default()
                );
                match client.create_event(&graph_event).await {
                    Ok((remote_id, ical_uid)) => {
                        log::info!(
                            "create_event: pushed to Graph Calendar, id={}, iCalUid={:?}",
                            remote_id,
                            ical_uid
                        );
                        let conn = state.db.writer().await;
                        conn.execute(
                            "UPDATE calendar_events SET remote_id = ?1 WHERE id = ?2",
                            rusqlite::params![remote_id, id],
                        )
                        .ok();
                        // Update the local UID to match Exchange's iCalUid so that
                        // incoming RSVP reply emails (which reference this UID) can
                        // be matched back to the event by process_invite_reply.
                        if let Some(ref ical_uid) = ical_uid {
                            conn.execute(
                                "UPDATE calendar_events SET uid = ?1 WHERE id = ?2",
                                rusqlite::params![ical_uid, id],
                            )
                            .ok();
                            log::info!(
                                "create_event: updated local UID to Exchange iCalUid={}",
                                ical_uid
                            );
                        }
                    }
                    Err(e) => log::error!("create_event: Graph Calendar push failed: {}", e),
                }
            }
        } else if account.calendar_protocol_str() == "jmap" {
            // Look up the remote calendar ID from local calendar
            let calendar = db::calendar::get_calendar(&conn, &cal_event.calendar_id)?;
            let remote_cal_id = calendar.remote_id.clone().unwrap_or_default();
            if remote_cal_id.is_empty() {
                log::warn!(
                    "create_event: no remote calendar ID for local calendar '{}'",
                    cal_event.calendar_id
                );
            }
            drop(conn); // Release lock before async call
            let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;
            let jmap_event = crate::mail::jmap::JmapCalendarEvent {
                id: String::new(),
                calendar_id: remote_cal_id,
                title: cal_event.title.clone(),
                description: cal_event.description.clone(),
                location: cal_event.location.clone(),
                start: cal_event.start_time.clone(),
                end: cal_event.end_time.clone(),
                all_day: cal_event.all_day,
                timezone: cal_event.timezone.clone(),
                recurrence_rule: cal_event.recurrence_rule.clone(),
                uid: cal_event.uid.clone(),
                organizer_email: cal_event.organizer_email.clone(),
                attendees_json: cal_event.attendees_json.clone(),
            };
            match crate::mail::jmap::JmapConnection::connect(&jmap_config).await {
                Ok(conn_jmap) => {
                    match conn_jmap
                        .create_calendar_event(&jmap_config, &jmap_event)
                        .await
                    {
                        Ok(remote_id) => {
                            log::info!("create_event: pushed to JMAP, remote_id={}", remote_id);
                            let conn = state.db.writer().await;
                            conn.execute(
                                "UPDATE calendar_events SET remote_id = ?1 WHERE id = ?2",
                                rusqlite::params![remote_id, id],
                            )
                            .ok();
                        }
                        Err(e) => log::error!("create_event: JMAP push failed: {}", e),
                    }
                }
                Err(e) => log::error!("create_event: JMAP connect failed: {}", e),
            }
        }
    }

    log::info!("create_event: created event id={}", id);
    Ok(id)
}

#[tauri::command]
pub async fn update_event(
    state: State<'_, AppState>,
    event_id: String,
    event: UpdateEventInput,
) -> Result<()> {
    log::info!("update_event: id={}", event_id);
    let conn = state.db.writer().await;

    // Load existing event, apply updates
    let mut existing = db::calendar::get_event(&conn, &event_id)?;

    if let Some(calendar_id) = event.calendar_id {
        existing.calendar_id = calendar_id;
    }
    if let Some(title) = event.title {
        existing.title = title;
    }
    if let Some(description) = event.description {
        existing.description = Some(description);
    }
    if let Some(location) = event.location {
        existing.location = Some(location);
    }
    if let Some(start_time) = event.start_time {
        existing.start_time = start_time;
    }
    if let Some(end_time) = event.end_time {
        existing.end_time = end_time;
    }
    if let Some(all_day) = event.all_day {
        existing.all_day = all_day;
    }
    if let Some(timezone) = event.timezone {
        existing.timezone = Some(timezone);
    }
    if let Some(recurrence_rule) = event.recurrence_rule {
        existing.recurrence_rule = Some(recurrence_rule);
    }
    if let Some(attendees) = event.attendees {
        existing.attendees_json = if attendees.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&attendees).unwrap_or_else(|_| "[]".to_string()))
        };
    }

    db::calendar::update_event(&conn, &existing)?;
    log::info!("update_event: updated event {}", event_id);

    // Push update to server
    let account = db::accounts::get_account_full(&conn, &existing.account_id)?;
    if let Some(ref remote_id) = existing.remote_id.filter(|r| !r.is_empty()) {
        if account.calendar_protocol_str() == "google" {
            drop(conn);
            if let Ok(token) = get_google_token(&existing.account_id).await {
                let http = reqwest::Client::new();
                let mut patch = serde_json::json!({
                    "summary": existing.title,
                    "start": if existing.all_day {
                        serde_json::json!({"date": existing.start_time.split('T').next().unwrap_or_default()})
                    } else {
                        serde_json::json!({"dateTime": existing.start_time})
                    },
                    "end": if existing.all_day {
                        serde_json::json!({"date": existing.end_time.split('T').next().unwrap_or_default()})
                    } else {
                        serde_json::json!({"dateTime": existing.end_time})
                    },
                });
                if let Some(ref desc) = existing.description {
                    patch["description"] = serde_json::json!(desc);
                }
                if let Some(ref loc) = existing.location {
                    patch["location"] = serde_json::json!(loc);
                }
                let send_updates = if existing.attendees_json.is_some() {
                    "all"
                } else {
                    "none"
                };
                let url = format!(
                        "https://www.googleapis.com/calendar/v3/calendars/primary/events/{}?sendUpdates={}",
                        urlencoding::encode(remote_id), send_updates
                    );
                match http
                    .patch(&url)
                    .bearer_auth(&token)
                    .json(&patch)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        log::info!("update_event: updated on Google Calendar");
                    }
                    Ok(resp) => {
                        let body = resp.text().await.unwrap_or_default();
                        log::error!("update_event: Google Calendar PATCH failed: {}", body);
                    }
                    Err(e) => log::error!("update_event: Google Calendar request failed: {}", e),
                }
            }
        } else if account.calendar_protocol_str() == "graph" {
            drop(conn);
            if let Ok(token) = crate::mail::graph::get_graph_token(&existing.account_id).await {
                let client = crate::mail::graph::GraphClient::new(&token);
                let mut patch = serde_json::json!({
                    "subject": existing.title,
                    "start": {"dateTime": existing.start_time, "timeZone": "UTC"},
                    "end": {"dateTime": existing.end_time, "timeZone": "UTC"},
                    "isAllDay": existing.all_day,
                });
                if let Some(ref desc) = existing.description {
                    patch["body"] = serde_json::json!({"contentType": "text", "content": desc});
                }
                if let Some(ref loc) = existing.location {
                    patch["location"] = serde_json::json!({"displayName": loc});
                }
                match client.update_event(remote_id, &patch).await {
                    Ok(()) => log::info!("update_event: updated on Graph Calendar"),
                    Err(e) => log::error!("update_event: Graph Calendar PATCH failed: {}", e),
                }
            }
        }
        // JMAP update is handled by existing code path via update_calendar_event
    }

    Ok(())
}

#[tauri::command]
pub async fn delete_event(state: State<'_, AppState>, event_id: String) -> Result<()> {
    log::info!("delete_event: id={}", event_id);

    // Look up the event, account, and calendar remote_id
    let (event, account, cal_remote_id) = {
        let conn = state.db.reader();
        let evt = db::calendar::get_event(&conn, &event_id)?;
        let acc = db::accounts::get_account_full(&conn, &evt.account_id)?;
        let cal = db::calendar::get_calendar(&conn, &evt.calendar_id).ok();
        let cal_rid = cal
            .and_then(|c| c.remote_id)
            .unwrap_or_else(|| "primary".to_string());
        (evt, acc, cal_rid)
    };

    // Delete from server if event has a remote_id
    if let Some(ref remote_id) = event.remote_id {
        if !remote_id.is_empty() {
            if account.calendar_protocol_str() == "google" {
                // Delete via Google Calendar API
                match get_google_token(&event.account_id).await {
                    Ok(token) => {
                        let http = reqwest::Client::new();
                        let url = format!(
                            "https://www.googleapis.com/calendar/v3/calendars/{}/events/{}?sendUpdates=all",
                            urlencoding::encode(&cal_remote_id),
                            urlencoding::encode(remote_id)
                        );
                        match http.delete(&url).bearer_auth(&token).send().await {
                            Ok(resp)
                                if resp.status().is_success()
                                    || resp.status().as_u16() == 204
                                    || resp.status().as_u16() == 410 =>
                            {
                                log::info!("delete_event: deleted from Google Calendar");
                            }
                            Ok(resp) => {
                                let body = resp.text().await.unwrap_or_default();
                                log::error!(
                                    "delete_event: Google Calendar delete failed: {}",
                                    body
                                );
                            }
                            Err(e) => {
                                log::error!("delete_event: Google Calendar request failed: {}", e)
                            }
                        }
                    }
                    Err(e) => log::warn!(
                        "delete_event: no Google OAuth token, skipping server delete: {}",
                        e
                    ),
                }
            } else if account.calendar_protocol_str() == "graph" {
                match crate::mail::graph::get_graph_token(&event.account_id).await {
                    Ok(token) => {
                        let client = crate::mail::graph::GraphClient::new(&token);
                        if let Err(e) = client.delete_event(remote_id).await {
                            log::error!("delete_event: Graph Calendar delete failed: {}", e);
                        } else {
                            log::info!("delete_event: deleted from Graph Calendar");
                        }
                    }
                    Err(e) => log::warn!(
                        "delete_event: no Graph token, skipping server delete: {}",
                        e
                    ),
                }
            } else if account.calendar_protocol_str() == "jmap" {
                let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;
                match crate::mail::jmap::JmapConnection::connect(&jmap_config).await {
                    Ok(conn_jmap) => {
                        if let Err(e) = conn_jmap
                            .delete_calendar_event(&jmap_config, remote_id)
                            .await
                        {
                            log::error!("delete_event: JMAP server delete failed: {}", e);
                        }
                    }
                    Err(e) => log::error!("delete_event: JMAP connect failed: {}", e),
                }
            } else if !account.caldav_url.is_empty() {
                let caldav_config = crate::mail::caldav::CalDavConfig {
                    caldav_url: account.caldav_url.clone(),
                    username: account.username.clone(),
                    password: account.password.clone(),
                    email: account.email.clone(),
                };
                match crate::mail::caldav::CalDavClient::connect(&caldav_config).await {
                    Ok(client) => {
                        if let Err(e) = client.delete_event(remote_id).await {
                            log::error!("delete_event: CalDAV server delete failed: {}", e);
                        }
                    }
                    Err(e) => log::error!("delete_event: CalDAV connect failed: {}", e),
                }
            }
        }
    }

    let conn = state.db.writer().await;
    db::calendar::delete_event(&conn, &event_id)?;
    log::info!("delete_event: deleted event {}", event_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Calendar sync command
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn sync_calendars(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    #[allow(unused_variables)] force_full_sync: Option<bool>,
) -> Result<()> {
    log::info!("sync_calendars: account={}", account_id);

    let account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    // Gate on the per-account toggle before any side effects. Running the
    // force_full_sync token-clearing below for a disabled account would
    // make the *next* sync after re-enabling do an unnecessary full sync.
    if !account.calendar_sync_enabled {
        log::info!(
            "sync_calendars: skipping account {} (calendar sync disabled)",
            account_id
        );
        return Ok(());
    }

    // When force_full_sync is true (manual Sync button), clear Google/O365
    // sync tokens to force a full sync that reconciles server-side deletions.
    if force_full_sync.unwrap_or(false) {
        let conn = state.db.writer().await;
        // Escape SQL LIKE metacharacters in account_id to prevent
        // unintended pattern matching if the id contains % or _.
        let escaped_id = account_id.replace('%', "\\%").replace('_', "\\_");
        conn.execute(
            "DELETE FROM app_metadata WHERE key LIKE ?1 ESCAPE '\\'",
            rusqlite::params![format!("google_sync_token_{escaped_id}_%")],
        )
        .ok();
        log::info!(
            "sync_calendars: cleared sync tokens for full sync (account={})",
            account_id
        );
    }

    if account.calendar_protocol_str() == "jmap" {
        sync_calendars_jmap(&state, &account_id, &account).await?;
    } else if account.calendar_protocol_str() == "google" {
        // Gmail: use Google CalDAV with OAuth2 bearer token
        match sync_calendars_google(&state, &account_id, &account).await {
            Ok(()) => {}
            Err(e) => {
                log::warn!(
                    "sync_calendars: Gmail CalDAV sync failed (OAuth may not be configured): {}",
                    e
                );
                // Fall back to configured CalDAV URL if available
                if !account.caldav_url.is_empty() {
                    sync_calendars_caldav(&state, &account_id, &account).await?;
                }
            }
        }
    } else if account.calendar_protocol_str() == "graph" {
        sync_calendars_graph(&state, &account_id).await?;
    } else if !account.caldav_url.is_empty() {
        sync_calendars_caldav(&state, &account_id, &account).await?;
    } else {
        log::debug!(
            "sync_calendars: skipping account {} (no JMAP or CalDAV configured)",
            account_id
        );
    }

    // Notify frontend that calendar data has changed
    use tauri::Emitter;
    app.emit("calendar-changed", account_id.as_str()).ok();

    log::info!("sync_calendars: completed for account {}", account_id);
    Ok(())
}

/// Sync calendars and events via JMAP.
async fn sync_calendars_jmap(
    state: &State<'_, AppState>,
    account_id: &str,
    account: &db::accounts::AccountFull,
) -> Result<()> {
    let jmap_config = crate::commands::sync_cmd::build_jmap_config(account).await?;

    let jmap_conn = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;

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
        let conn = state.db.writer().await;
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

        let conn = state.db.writer().await;
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
        let conn = state.db.writer().await;
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

                let jmap_event = crate::mail::jmap::JmapCalendarEvent {
                    id: String::new(),
                    calendar_id: remote_cal_id,
                    title: ev.title.clone(),
                    description: ev.description.clone(),
                    location: ev.location.clone(),
                    start: ev.start_time.clone(),
                    end: ev.end_time.clone(),
                    all_day: ev.all_day,
                    timezone: ev.timezone.clone(),
                    recurrence_rule: ev.recurrence_rule.clone(),
                    uid: ev.uid.clone(),
                    organizer_email: ev.organizer_email.clone(),
                    attendees_json: ev.attendees_json.clone(),
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
                        let conn = state.db.writer().await;
                        conn.execute(
                            "UPDATE calendar_events SET remote_id = ?1 WHERE id = ?2",
                            rusqlite::params![remote_id, ev.id],
                        )
                        .ok();
                    }
                    Err(e) => {
                        log::error!("sync_calendars: failed to push event '{}': {}", ev.title, e)
                    }
                }
            }
        }
    }

    Ok(())
}

/// Sync calendars and events via Google Calendar API with OAuth2.
async fn sync_calendars_google(
    state: &State<'_, AppState>,
    account_id: &str,
    _account: &db::accounts::AccountFull,
) -> Result<()> {
    let access_token = get_google_token(account_id).await?;
    let http = reqwest::Client::new();

    // Step 1: List calendars via Google Calendar API
    let resp = http
        .get("https://www.googleapis.com/calendar/v3/users/me/calendarList")
        .bearer_auth(&access_token)
        .send()
        .await
        .map_err(|e| crate::error::Error::Other(format!("Google Calendar API failed: {}", e)))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(crate::error::Error::Other(format!(
            "Google Calendar API error: {}",
            body
        )));
    }

    let data: serde_json::Value = resp.json().await.map_err(|e| {
        crate::error::Error::Other(format!("Google Calendar API parse error: {}", e))
    })?;

    let items = data["items"].as_array();
    log::info!(
        "sync_calendars_google: fetched {} calendars",
        items.map(|i| i.len()).unwrap_or(0)
    );

    let mut remote_to_local: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    {
        let conn = state.db.writer().await;
        if let Some(calendars) = items {
            for cal in calendars {
                let cal_id = cal["id"].as_str().unwrap_or_default();
                let name = cal["summary"].as_str().unwrap_or("Calendar");
                let color = cal["backgroundColor"].as_str().unwrap_or("#4285f4");
                let is_primary = cal["primary"].as_bool().unwrap_or(false);

                let local_id = db::calendar::upsert_calendar_by_remote_id(
                    &conn, account_id, cal_id, name, color, is_primary,
                )?;
                remote_to_local.insert(cal_id.to_string(), local_id);
            }
        }
    }

    // Step 2: Fetch events for each calendar (with syncToken for incremental sync)
    for (remote_cal_id, local_cal_id) in &remote_to_local {
        let sync_key = format!("google_sync_token_{}_{}", account_id, remote_cal_id);

        // Check for existing syncToken
        let existing_token: Option<String> = {
            let conn = state.db.reader();
            conn.query_row(
                "SELECT value FROM app_metadata WHERE key = ?1",
                rusqlite::params![sync_key],
                |row| row.get(0),
            )
            .ok()
        };

        let resp = if let Some(ref token) = existing_token {
            // Incremental sync
            log::debug!(
                "sync_calendars_google: incremental sync for calendar {}",
                remote_cal_id
            );
            http.get(format!(
                "https://www.googleapis.com/calendar/v3/calendars/{}/events",
                urlencoding::encode(remote_cal_id)
            ))
            .bearer_auth(&access_token)
            .query(&[("syncToken", token.as_str())])
            .send()
            .await
        } else {
            // Full sync
            let now = chrono::Utc::now();
            let time_min = (now - chrono::Duration::days(30)).to_rfc3339();
            let time_max = (now + chrono::Duration::days(180)).to_rfc3339();
            http.get(format!(
                "https://www.googleapis.com/calendar/v3/calendars/{}/events",
                urlencoding::encode(remote_cal_id)
            ))
            .bearer_auth(&access_token)
            .query(&[
                ("timeMin", time_min.as_str()),
                ("timeMax", time_max.as_str()),
                ("singleEvents", "true"),
                ("maxResults", "500"),
            ])
            .send()
            .await
        };

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                log::error!(
                    "sync_calendars_google: events fetch failed for {}: {}",
                    remote_cal_id,
                    e
                );
                continue;
            }
        };

        if resp.status().as_u16() == 410 {
            // syncToken expired — clear it and retry with full sync on next cycle
            log::info!(
                "sync_calendars_google: syncToken expired for {}, will full sync next time",
                remote_cal_id
            );
            let conn = state.db.writer().await;
            conn.execute(
                "DELETE FROM app_metadata WHERE key = ?1",
                rusqlite::params![sync_key],
            )
            .ok();
            continue;
        }
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            log::error!(
                "sync_calendars_google: events error for {}: {}",
                remote_cal_id,
                body
            );
            continue;
        }

        let events_data: serde_json::Value = match resp.json().await {
            Ok(d) => d,
            Err(e) => {
                log::error!("sync_calendars_google: events parse error: {}", e);
                continue;
            }
        };

        let events = events_data["items"].as_array();
        let count = events.map(|e| e.len()).unwrap_or(0);
        log::info!(
            "sync_calendars_google: fetched {} events for calendar {}",
            count,
            remote_cal_id
        );

        let conn = state.db.writer().await;
        let mut server_event_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut server_uids: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Some(events) = events {
            for ev in events {
                let event_id_remote = ev["id"].as_str().unwrap_or_default();
                server_event_ids.insert(event_id_remote.to_string());
                if let Some(uid) = ev["iCalUID"].as_str() {
                    server_uids.insert(uid.to_string());
                }

                // Incremental sync: cancelled events should be deleted locally
                if ev["status"].as_str() == Some("cancelled") {
                    let deleted = conn
                        .execute(
                            "DELETE FROM calendar_events WHERE account_id = ?1 AND remote_id = ?2",
                            rusqlite::params![account_id, event_id_remote],
                        )
                        .unwrap_or(0);
                    // Also delete by iCalUID for events created locally via respond_to_invite
                    if let Some(ical_uid) = ev["iCalUID"].as_str() {
                        conn.execute(
                            "DELETE FROM calendar_events WHERE account_id = ?1 AND uid = ?2 AND remote_id IS NULL",
                            rusqlite::params![account_id, ical_uid],
                        ).ok();
                    }
                    if deleted > 0 {
                        log::info!(
                            "sync_calendars_google: deleted cancelled event '{}'",
                            event_id_remote
                        );
                    }
                    continue;
                }

                let title = ev["summary"].as_str().unwrap_or("(No title)");
                let description = ev["description"].as_str().map(|s| s.to_string());
                let location = ev["location"].as_str().map(|s| s.to_string());

                // Parse start/end — can be date (all-day) or dateTime
                let start_tz = ev["start"]["timeZone"].as_str().map(|s| s.to_string());
                let (start_time, all_day) = if let Some(dt) = ev["start"]["dateTime"].as_str() {
                    (
                        crate::calendar::timezone::to_utc(dt, start_tz.as_deref().unwrap_or("")),
                        false,
                    )
                } else if let Some(d) = ev["start"]["date"].as_str() {
                    (d.to_string(), true)
                } else {
                    continue;
                };

                let end_time = if let Some(dt) = ev["end"]["dateTime"].as_str() {
                    let end_tz = ev["end"]["timeZone"].as_str().unwrap_or("");
                    crate::calendar::timezone::to_utc(dt, end_tz)
                } else if let Some(d) = ev["end"]["date"].as_str() {
                    d.to_string()
                } else {
                    start_time.clone()
                };

                let organizer_email = ev["organizer"]["email"].as_str().map(|s| s.to_string());
                let uid = ev["iCalUID"].as_str().map(|s| s.to_string());

                let cal_event = CalendarEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    account_id: account_id.to_string(),
                    calendar_id: local_cal_id.clone(),
                    uid,
                    title: title.to_string(),
                    description,
                    location,
                    start_time,
                    end_time,
                    all_day,
                    timezone: start_tz,
                    recurrence_rule: None,
                    organizer_email,
                    attendees_json: None,
                    my_status: None,
                    source_message_id: None,
                    ical_data: None,
                    remote_id: Some(event_id_remote.to_string()),
                    etag: ev["etag"].as_str().map(|s| s.to_string()),
                };

                if let Err(e) = db::calendar::upsert_event_by_remote_id(&conn, &cal_event) {
                    log::error!("sync_calendars_google: upsert event failed: {}", e);
                }
            }
        }

        // Drop the conn lock before acquiring again for syncToken
        drop(conn);

        // Save nextSyncToken for incremental sync next time
        if let Some(next_token) = events_data["nextSyncToken"].as_str() {
            let conn = state.db.writer().await;
            conn.execute(
                "INSERT OR REPLACE INTO app_metadata (key, value) VALUES (?1, ?2)",
                rusqlite::params![sync_key, next_token],
            )
            .ok();
            log::debug!(
                "sync_calendars_google: saved syncToken for calendar {}",
                remote_cal_id
            );
        }

        // During full sync (no syncToken), reconcile: delete local events
        // whose remote_id no longer appears on the server. Incremental sync
        // handles deletions via "status: cancelled" (see above).
        if existing_token.is_none() && !server_event_ids.is_empty() {
            let conn = state.db.writer().await;
            let local_events: Vec<(String, String)> = conn
                .prepare(
                    "SELECT ce.id, ce.remote_id FROM calendar_events ce
                     JOIN calendars c ON ce.calendar_id = c.id
                     WHERE ce.account_id = ?1 AND ce.remote_id IS NOT NULL AND ce.remote_id != ''
                     AND c.remote_id = ?2",
                )
                .map(|mut stmt| {
                    stmt.query_map(rusqlite::params![account_id, remote_cal_id], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default()
                })
                .unwrap_or_default();

            let mut deleted = 0;
            for (local_id, remote_id) in &local_events {
                if !server_event_ids.contains(remote_id) {
                    db::calendar::delete_event(&conn, local_id).ok();
                    deleted += 1;
                }
            }
            // Also remove orphan events (no remote_id) by matching UID
            if !server_uids.is_empty() {
                let orphans: Vec<(String, String)> = conn
                    .prepare(
                        "SELECT ce.id, ce.uid FROM calendar_events ce
                         JOIN calendars c ON ce.calendar_id = c.id
                         WHERE ce.account_id = ?1 AND (ce.remote_id IS NULL OR ce.remote_id = '')
                         AND ce.uid IS NOT NULL AND c.remote_id = ?2",
                    )
                    .map(|mut stmt| {
                        stmt.query_map(rusqlite::params![account_id, remote_cal_id], |row| {
                            Ok((row.get(0)?, row.get(1)?))
                        })
                        .map(|rows| rows.filter_map(|r| r.ok()).collect())
                        .unwrap_or_default()
                    })
                    .unwrap_or_default();
                for (local_id, uid) in &orphans {
                    if !server_uids.contains(uid) {
                        db::calendar::delete_event(&conn, local_id).ok();
                        deleted += 1;
                    }
                }
            }
            if deleted > 0 {
                log::info!(
                    "sync_calendars_google: removed {} server-deleted events from '{}'",
                    deleted,
                    remote_cal_id
                );
            }
        }
    }

    log::info!(
        "sync_calendars_google: completed for account {}",
        account_id
    );
    Ok(())
}

/// Get a valid Google OAuth2 access token, refreshing if expired.
async fn get_google_token(account_id: &str) -> Result<String> {
    let tokens = crate::oauth::load_tokens(account_id)?.ok_or_else(|| {
        crate::error::Error::Other(
            "No Google OAuth tokens. Please sign in with Google in Settings.".into(),
        )
    })?;

    if !tokens.is_expired() {
        return Ok(tokens.access_token);
    }

    let refresh_token = tokens
        .refresh_token
        .ok_or_else(|| crate::error::Error::Other("No refresh token".into()))?;
    match crate::oauth::refresh_access_token(&crate::oauth::GOOGLE, &refresh_token).await {
        Ok(new_tokens) => {
            crate::oauth::store_tokens(account_id, &new_tokens)?;
            Ok(new_tokens.access_token)
        }
        Err(e) => Err(e),
    }
}

/// Sync calendars and events via CalDAV.
async fn sync_calendars_caldav(
    state: &State<'_, AppState>,
    account_id: &str,
    account: &db::accounts::AccountFull,
) -> Result<()> {
    use crate::mail::caldav::{CalDavClient, CalDavConfig};

    let caldav_config = CalDavConfig {
        caldav_url: account.caldav_url.clone(),
        username: account.username.clone(),
        password: account.password.clone(),
        email: account.email.clone(),
    };

    let client = CalDavClient::connect(&caldav_config).await?;

    // Step 1: List calendars from server
    let caldav_calendars = client.list_calendars().await?;
    log::info!(
        "sync_calendars: fetched {} calendars from CalDAV for account {}",
        caldav_calendars.len(),
        account_id
    );

    // Build a mapping from remote calendar href to local calendar ID
    let mut remote_to_local: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    {
        let conn = state.db.writer().await;
        for (idx, cal) in caldav_calendars.iter().enumerate() {
            let color = cal.color.as_deref().unwrap_or("#4285f4");
            let is_default = idx == 0; // First calendar is default
            let local_id = db::calendar::upsert_calendar_by_remote_id(
                &conn, account_id, &cal.href, &cal.name, color, is_default,
            )?;
            remote_to_local.insert(cal.href.clone(), local_id);
        }
    }

    // Step 2: For each calendar, fetch events and upsert into local DB
    for cal in &caldav_calendars {
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

        let local_cal_id = remote_to_local.get(&cal.href).cloned().unwrap_or_default();

        let conn = state.db.writer().await;
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
                Some(serde_json::to_string(&invite.attendees).unwrap_or_else(|_| "[]".to_string()))
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

        let mut deleted = 0u32;
        for (local_id, remote_id) in &local_synced {
            if !server_hrefs.contains(remote_id) {
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
                "sync_calendars: removed {} server-deleted events from CalDAV calendar '{}'",
                deleted,
                cal.name
            );
        }
    }

    // Step 3: Push local events with no remote_id to CalDAV
    {
        let conn = state.db.writer().await;
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
                    .find(|(_, local_id)| **local_id == ev.calendar_id)
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
                        let conn = state.db.writer().await;
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

// ---------------------------------------------------------------------------
// Invite handling commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn get_email_invites(
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
) -> Result<Vec<ParsedInvite>> {
    log::info!(
        "get_email_invites: account={} message={}",
        account_id,
        message_id
    );
    let conn = state.db.reader();

    // Look up the message to get its maildir path
    let (maildir_path, _from_email, _to, _cc, _flags, _encrypted, _signed) =
        db::messages::get_message_metadata(&conn, &account_id, &message_id)?;

    if maildir_path.is_empty() {
        log::debug!("get_email_invites: message body not fetched yet");
        return Ok(vec![]);
    }

    // Read the raw message from disk (maildir_path is relative to data_dir)
    let full_path = crate::path_validation::resolve_under(&state.data_dir, &maildir_path)?;
    log::debug!("get_email_invites: reading from {}", full_path.display());
    let raw = std::fs::read(&full_path).map_err(|e| {
        crate::error::Error::Other(format!(
            "Failed to read message file '{}': {}",
            full_path.display(),
            e
        ))
    })?;

    let invites = ical::parse_ical_from_email(&raw);
    log::info!(
        "get_email_invites: found {} invites in message {}",
        invites.len(),
        message_id
    );
    Ok(invites)
}

#[tauri::command]
pub async fn get_invite_status(
    state: State<'_, AppState>,
    account_id: String,
    invite_uid: String,
) -> Result<Option<String>> {
    let conn = state.db.reader();
    let event = db::calendar::get_event_by_uid(&conn, &account_id, &invite_uid)?;
    Ok(event.and_then(|e| e.my_status))
}

#[tauri::command]
pub async fn respond_to_invite(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
    invite_uid: String,
    response: String,
) -> Result<()> {
    log::info!(
        "respond_to_invite: account={} message={} uid={} response={}",
        account_id,
        message_id,
        invite_uid,
        response
    );

    // Step 1: Parse the invite from the email
    let (raw, account) = {
        let conn = state.db.writer().await;
        let (maildir_path, _from_email, _to, _cc, _flags, _encrypted, _signed) =
            db::messages::get_message_metadata(&conn, &account_id, &message_id)?;

        if maildir_path.is_empty() {
            return Err(crate::error::Error::Other(
                "Message body not fetched yet".to_string(),
            ));
        }

        let full_path = crate::path_validation::resolve_under(&state.data_dir, &maildir_path)?;
        let raw = std::fs::read(&full_path).map_err(|e| {
            crate::error::Error::Other(format!(
                "Failed to read message file '{}': {}",
                full_path.display(),
                e
            ))
        })?;

        let account = db::accounts::get_account_full(&conn, &account_id)?;
        (raw, account)
    };

    let invites = ical::parse_ical_from_email(&raw);
    let invite = invites
        .iter()
        .find(|inv| inv.uid == invite_uid)
        .ok_or_else(|| {
            crate::error::Error::Other(format!(
                "Invite with UID '{}' not found in message",
                invite_uid
            ))
        })?;

    // Step 2: Generate the iTIP REPLY
    let reply_ical = ical::generate_reply(invite, &account.email, &response);

    // Step 3: Send the reply to the organizer
    if let Some(ref organizer_email) = invite.organizer_email {
        let subject = format!(
            "Re: {}",
            invite.summary.as_deref().unwrap_or("Calendar Invite")
        );

        // Build an email with the iCal reply as a text/calendar attachment
        let body_text = format!(
            "This is a {} response to the calendar invitation \"{}\".",
            response.to_lowercase(),
            invite.summary.as_deref().unwrap_or("Calendar Invite")
        );

        if account.calendar_protocol_str() == "jmap" {
            log::info!("respond_to_invite: sending reply via JMAP");
            let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;

            let raw_message = build_calendar_reply_message(
                &account.email,
                organizer_email,
                &subject,
                &body_text,
                &reply_ical,
            )?;

            let jmap_conn = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
            jmap_conn.send_email(&jmap_config, &raw_message).await?;
        } else {
            log::info!("respond_to_invite: sending reply via SMTP");
            let raw_message = build_calendar_reply_message(
                &account.email,
                organizer_email,
                &subject,
                &body_text,
                &reply_ical,
            )?;

            // For O365: refresh SMTP-scoped OAuth token
            let (smtp_password, use_xoauth2) = get_smtp_credentials(&account).await?;

            send_raw_smtp(
                &account.smtp_host,
                account.smtp_port,
                &account.username,
                &smtp_password,
                account.use_tls,
                use_xoauth2,
                &account.email,
                organizer_email,
                &raw_message,
            )
            .await?;
        }
    } else {
        log::info!("respond_to_invite: no organizer email, skipping send");
    }

    // Step 4: Create/update event in local calendar
    let my_status = response.to_lowercase();
    let conn = state.db.writer().await;

    // Find the best calendar for this account: prefer default, then any with
    // a remote_id (synced from server), then any existing, finally create one.
    let calendars = db::calendar::list_calendars(&conn, &account_id)?;
    let calendar_id = if let Some(cal) = calendars
        .iter()
        .find(|c| c.is_default && c.remote_id.is_some())
    {
        cal.id.clone()
    } else if let Some(cal) = calendars.iter().find(|c| c.is_default) {
        cal.id.clone()
    } else if let Some(cal) = calendars.iter().find(|c| c.remote_id.is_some()) {
        cal.id.clone()
    } else if let Some(cal) = calendars.first() {
        cal.id.clone()
    } else {
        // No calendars at all — create a default one
        let cal_id = uuid::Uuid::new_v4().to_string();
        let new_cal = NewCalendar {
            account_id: account_id.clone(),
            name: "Calendar".to_string(),
            color: random_calendar_color(),
            is_default: true,
        };
        db::calendar::insert_calendar(&conn, &cal_id, &new_cal)?;
        log::info!("respond_to_invite: created default calendar id={}", cal_id);
        cal_id
    };

    let attendees_json = if invite.attendees.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&invite.attendees).unwrap_or_else(|_| "[]".to_string()))
    };

    // Check if we already have this event
    if let Some(mut existing) = db::calendar::get_event_by_uid(&conn, &account_id, &invite_uid)? {
        existing.my_status = Some(my_status);
        existing.attendees_json = attendees_json;
        db::calendar::update_event(&conn, &existing)?;
        log::info!(
            "respond_to_invite: updated existing event {} status={}",
            existing.id,
            response
        );
    } else {
        let event_id = uuid::Uuid::new_v4().to_string();
        let cal_event = CalendarEvent {
            id: event_id.clone(),
            account_id: account_id.clone(),
            calendar_id,
            uid: Some(invite.uid.clone()),
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
            my_status: Some(my_status),
            source_message_id: Some(message_id.clone()),
            ical_data: Some(invite.ical_raw.clone()),
            remote_id: None,
            etag: None,
        };
        db::calendar::insert_event(&conn, &cal_event)?;
        log::info!(
            "respond_to_invite: created event {} status={}",
            event_id,
            response
        );
    }

    // Step 5: Update Google Calendar if this is a Gmail account with OAuth
    if account.calendar_protocol_str() == "google" {
        drop(conn); // Release DB lock before async
        if let Ok(token) = get_google_token(&account_id).await {
            let google_status = match response.to_lowercase().as_str() {
                "accepted" => "accepted",
                "tentative" => "tentative",
                "declined" => "declined",
                _ => "needsAction",
            };
            // Find event on Google Calendar by iCalUID, import if not found
            let http = reqwest::Client::new();
            let search_url = format!(
                "https://www.googleapis.com/calendar/v3/calendars/primary/events?iCalUID={}",
                urlencoding::encode(&invite_uid)
            );
            let mut google_event_id_found = None;
            if let Ok(resp) = http.get(&search_url).bearer_auth(&token).send().await {
                if let Ok(data) = resp.json::<serde_json::Value>().await {
                    if let Some(items) = data["items"].as_array() {
                        google_event_id_found = items
                            .first()
                            .and_then(|e| e["id"].as_str())
                            .map(|s| s.to_string());
                    }
                }
            }
            // If not on Google Calendar yet, import it
            if google_event_id_found.is_none() {
                let import_event = serde_json::json!({
                    "iCalUID": invite_uid,
                    "summary": invite.summary,
                    "start": if invite.all_day {
                        serde_json::json!({"date": invite.dtstart.split('T').next().unwrap_or_default()})
                    } else {
                        serde_json::json!({"dateTime": invite.dtstart})
                    },
                    "end": if invite.all_day {
                        serde_json::json!({"date": invite.dtend.split('T').next().unwrap_or_default()})
                    } else {
                        serde_json::json!({"dateTime": invite.dtend})
                    },
                    "description": invite.description,
                    "location": invite.location,
                    "organizer": {"email": invite.organizer_email},
                    "attendees": [{
                        "email": account.email,
                        "responseStatus": google_status,
                        "self": true,
                    }],
                });
                match http
                    .post("https://www.googleapis.com/calendar/v3/calendars/primary/events/import")
                    .bearer_auth(&token)
                    .json(&import_event)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        if let Ok(data) = resp.json::<serde_json::Value>().await {
                            google_event_id_found = data["id"].as_str().map(|s| s.to_string());
                            log::info!("respond_to_invite: imported event to Google Calendar");
                        }
                    }
                    Ok(resp) => {
                        let body = resp.text().await.unwrap_or_default();
                        log::warn!("respond_to_invite: Google Calendar import failed: {}", body);
                    }
                    Err(e) => log::warn!(
                        "respond_to_invite: Google Calendar import request failed: {}",
                        e
                    ),
                }
            }
            // PATCH the attendee status on Google
            if let Some(ref geid) = google_event_id_found {
                if !geid.is_empty() {
                    let patch_url = format!(
                        "https://www.googleapis.com/calendar/v3/calendars/primary/events/{}?sendUpdates=none",
                        urlencoding::encode(geid)
                    );
                    let attendees_patch = serde_json::json!({
                        "attendees": [{
                            "email": account.email,
                            "responseStatus": google_status,
                            "self": true,
                        }]
                    });
                    match http
                        .patch(&patch_url)
                        .bearer_auth(&token)
                        .json(&attendees_patch)
                        .send()
                        .await
                    {
                        Ok(r) if r.status().is_success() => {
                            log::info!(
                                "respond_to_invite: updated Google Calendar response to {}",
                                google_status
                            );
                        }
                        Ok(r) => {
                            let body = r.text().await.unwrap_or_default();
                            log::warn!("respond_to_invite: Google Calendar PATCH failed: {}", body);
                        }
                        Err(e) => {
                            log::warn!("respond_to_invite: Google Calendar request failed: {}", e)
                        }
                    }
                }
            }
            // Store the Google Calendar event ID as remote_id on the local event
            // so that Google Calendar sync doesn't create a duplicate.
            if let Some(ref geid) = google_event_id_found {
                if !geid.is_empty() {
                    let conn = state.db.writer().await;
                    conn.execute(
                        "UPDATE calendar_events SET remote_id = ?1 WHERE uid = ?2 AND account_id = ?3 AND (remote_id IS NULL OR remote_id = '')",
                        rusqlite::params![geid, invite_uid, account_id],
                    ).ok();
                    log::info!(
                        "respond_to_invite: stored Google Calendar remote_id={}",
                        geid
                    );
                }
            }
        }
    }

    // Step 6: Update O365 calendar via Graph API RSVP
    if account.calendar_protocol_str() == "graph" {
        match crate::mail::graph::get_graph_token(&account_id).await {
            Ok(token) => {
                let client = crate::mail::graph::GraphClient::new(&token);
                match client.find_event_by_ical_uid(&invite_uid).await {
                    Ok(Some(graph_event_id)) => {
                        match client.rsvp_event(&graph_event_id, &response, "").await {
                            Ok(()) => {
                                log::info!(
                                    "respond_to_invite: updated O365 Calendar response to {}",
                                    response
                                );
                                // Store remote_id so process_invite_reply can find the event later
                                let conn = state.db.writer().await;
                                conn.execute(
                                    "UPDATE calendar_events SET remote_id = ?1 WHERE uid = ?2 AND account_id = ?3",
                                    rusqlite::params![graph_event_id, invite_uid, account_id],
                                ).ok();
                            }
                            Err(e) => {
                                log::warn!("respond_to_invite: O365 Graph RSVP failed: {}", e)
                            }
                        }
                    }
                    Ok(None) => log::debug!(
                        "respond_to_invite: event not found on O365 Calendar by iCalUId"
                    ),
                    Err(e) => {
                        log::warn!("respond_to_invite: O365 Graph event lookup failed: {}", e)
                    }
                }
            }
            Err(e) => log::warn!("respond_to_invite: failed to get O365 token: {}", e),
        }
    }

    // Notify frontend that calendar data changed so the UI refreshes
    use tauri::Emitter as _;
    app.emit("calendar-changed", account_id.as_str()).ok();

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Pick a random color from a curated palette for new calendars.
pub fn random_calendar_color() -> String {
    let colors = [
        "#4285f4", // Google Blue
        "#0b8043", // Green
        "#8e24aa", // Purple
        "#d50000", // Red
        "#f4511e", // Orange
        "#039be5", // Cyan
        "#616161", // Grey
        "#e67c73", // Salmon
        "#f6bf26", // Yellow
        "#33b679", // Teal
    ];
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as usize;
    colors[seed % colors.len()].to_string()
}

/// Build a raw RFC5322 message with a text/calendar MIME part for an iTIP REPLY.
fn build_calendar_reply_message(
    from: &str,
    to: &str,
    subject: &str,
    body_text: &str,
    ical_reply: &str,
) -> Result<Vec<u8>> {
    use lettre::message::{header::ContentType, Mailbox, MultiPart, SinglePart};
    use lettre::Message;

    let from_mailbox: Mailbox = from.parse().map_err(|e| {
        crate::error::Error::Other(format!("Invalid 'from' address '{}': {}", from, e))
    })?;
    let to_mailbox: Mailbox = to
        .parse()
        .map_err(|e| crate::error::Error::Other(format!("Invalid 'to' address '{}': {}", to, e)))?;

    let message = Message::builder()
        .from(from_mailbox)
        .to(to_mailbox)
        .subject(subject)
        .multipart(
            MultiPart::mixed()
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_PLAIN)
                        .body(body_text.to_string()),
                )
                .singlepart(
                    SinglePart::builder()
                        .header(
                            ContentType::parse("text/calendar; method=REPLY; charset=UTF-8")
                                .unwrap_or(ContentType::TEXT_PLAIN),
                        )
                        .body(ical_reply.to_string()),
                ),
        )
        .map_err(|e| {
            crate::error::Error::Other(format!("Failed to build calendar reply message: {}", e))
        })?;

    Ok(message.formatted())
}

/// Get SMTP credentials for an account, refreshing OAuth tokens for O365.
async fn get_smtp_credentials(account: &db::accounts::AccountFull) -> Result<(String, bool)> {
    if account.calendar_protocol_str() == "graph" {
        let tokens = crate::oauth::load_tokens(&account.id)?
            .ok_or_else(|| crate::error::Error::Other("No O365 tokens for SMTP".into()))?;
        let refresh_token = tokens
            .refresh_token
            .ok_or_else(|| crate::error::Error::Other("No O365 refresh token for SMTP".into()))?;
        let smtp_tokens = crate::oauth::refresh_with_scopes(
            &crate::oauth::MICROSOFT,
            &refresh_token,
            crate::oauth::MICROSOFT_IMAP_SCOPES, // SMTP.Send is in the same scope set
        )
        .await?;
        crate::oauth::store_tokens(
            &account.id,
            &crate::oauth::OAuthTokens {
                access_token: smtp_tokens.access_token.clone(),
                refresh_token: smtp_tokens.refresh_token,
                expires_at: smtp_tokens.expires_at,
            },
        )?;
        Ok((smtp_tokens.access_token, true))
    } else {
        Ok((account.password.clone(), false))
    }
}

/// Send a pre-built raw message via SMTP, with XOAUTH2 support for O365.
async fn send_raw_smtp(
    smtp_host: &str,
    smtp_port: u16,
    username: &str,
    password: &str,
    use_tls: bool,
    use_xoauth2: bool,
    from: &str,
    to: &str,
    raw_message: &[u8],
) -> Result<()> {
    use lettre::transport::smtp::authentication::{Credentials, Mechanism};
    use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

    log::info!(
        "send_raw_smtp: from={} to={} via {}:{} (xoauth2={})",
        from,
        to,
        smtp_host,
        smtp_port,
        use_xoauth2,
    );

    let creds = Credentials::new(username.to_string(), password.to_string());
    let auth_mechanisms = if use_xoauth2 {
        vec![Mechanism::Xoauth2]
    } else {
        vec![Mechanism::Plain, Mechanism::Login]
    };

    let transport = if smtp_port == 587 {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(smtp_host)
            .map_err(|e| crate::error::Error::Other(format!("SMTP setup failed: {}", e)))?
            .port(smtp_port)
            .credentials(creds)
            .authentication(auth_mechanisms)
            .build()
    } else if use_tls || smtp_port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(smtp_host)
            .map_err(|e| crate::error::Error::Other(format!("SMTP setup failed: {}", e)))?
            .port(smtp_port)
            .credentials(creds)
            .authentication(auth_mechanisms)
            .build()
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(smtp_host)
            .map_err(|e| crate::error::Error::Other(format!("SMTP setup failed: {}", e)))?
            .port(smtp_port)
            .credentials(creds)
            .authentication(auth_mechanisms)
            .build()
    };

    // Build an envelope from the from/to addresses
    let from_addr: lettre::Address = from
        .parse()
        .map_err(|e| crate::error::Error::Other(format!("Invalid from address: {}", e)))?;
    let to_addr: lettre::Address = to
        .parse()
        .map_err(|e| crate::error::Error::Other(format!("Invalid to address: {}", e)))?;

    let envelope = lettre::address::Envelope::new(Some(from_addr), vec![to_addr])
        .map_err(|e| crate::error::Error::Other(format!("Failed to create envelope: {}", e)))?;

    transport
        .send_raw(&envelope, raw_message)
        .await
        .map_err(|e| {
            log::error!("SMTP send failed: {}", e);
            crate::error::Error::Other(format!("SMTP send failed: {}", e))
        })?;

    log::info!("send_raw_smtp: message sent successfully");
    Ok(())
}

fn get_unpushed_events(
    conn: &rusqlite::Connection,
    account_id: &str,
) -> Result<Vec<CalendarEvent>> {
    let mut stmt = conn.prepare(
        "SELECT id, account_id, calendar_id, uid, title, description, location,
                start_time, end_time, all_day, timezone, recurrence_rule,
                organizer_email, attendees_json, my_status, source_message_id,
                ical_data, remote_id, etag
         FROM calendar_events
         WHERE account_id = ?1 AND (remote_id IS NULL OR remote_id = '')",
    )?;
    let events = stmt
        .query_map(rusqlite::params![account_id], |row| {
            Ok(CalendarEvent {
                id: row.get(0)?,
                account_id: row.get(1)?,
                calendar_id: row.get(2)?,
                uid: row.get(3)?,
                title: row.get(4)?,
                description: row.get(5)?,
                location: row.get(6)?,
                start_time: row.get(7)?,
                end_time: row.get(8)?,
                all_day: row.get(9)?,
                timezone: row.get(10)?,
                recurrence_rule: row.get(11)?,
                organizer_email: row.get(12)?,
                attendees_json: row.get(13)?,
                my_status: row.get(14)?,
                source_message_id: row.get(15)?,
                ical_data: row.get(16)?,
                remote_id: row.get(17)?,
                etag: row.get(18)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(events)
}

/// Send meeting invite emails to attendees for a calendar event.
#[tauri::command]
pub async fn send_invites(
    state: State<'_, AppState>,
    account_id: String,
    event_id: String,
    attendee_emails: Vec<String>,
) -> Result<()> {
    log::info!(
        "send_invites: account={} event={} attendees={:?}",
        account_id,
        event_id,
        attendee_emails
    );

    let (account, event) = {
        let conn = state.db.writer().await;
        let acc = db::accounts::get_account_full(&conn, &account_id)?;
        let evt = db::calendar::get_event(&conn, &event_id)?;
        (acc, evt)
    };

    // Gmail and O365 handle sending invite emails server-side when
    // events are pushed via Google Calendar API (sendUpdates=all) or
    // Graph API. Sending our own SMTP invite would create duplicates.
    if account.calendar_protocol_str() == "google" || account.calendar_protocol_str() == "graph" {
        log::info!(
            "send_invites: skipping manual send for {} account (server handles invites)",
            account.calendar_protocol_str()
        );
        // Still update attendees in the local DB
        let conn = state.db.writer().await;
        let attendees_json = serde_json::to_string(
            &attendee_emails
                .iter()
                .map(|e| Attendee {
                    email: e.clone(),
                    name: None,
                    status: "needs-action".to_string(),
                })
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default();
        conn.execute(
            "UPDATE calendar_events SET attendees_json = ?1 WHERE id = ?2",
            rusqlite::params![attendees_json, event_id],
        )
        .ok();
        return Ok(());
    }

    let attendees: Vec<Attendee> = attendee_emails
        .iter()
        .map(|email| Attendee {
            email: email.clone(),
            name: None,
            status: "needs-action".to_string(),
        })
        .collect();

    let uid = event.uid.as_deref().unwrap_or(&event_id);
    let ical = ical::generate_invite(
        uid,
        &event.title,
        &event.start_time,
        &event.end_time,
        event.location.as_deref(),
        event.description.as_deref(),
        &account.email,
        None, // Use email as organizer name — display_name is the account label, not a person's name
        &attendees,
        event.recurrence_rule.as_deref(),
        if event.all_day {
            None
        } else {
            event.timezone.as_deref()
        },
    );

    let subject = format!("Invitation: {}", event.title);
    let body_text = format!(
        "You have been invited to: {}\nWhen: {} - {}\n{}",
        event.title,
        event.start_time,
        event.end_time,
        event
            .location
            .as_deref()
            .map(|l| format!("Where: {}\n", l))
            .unwrap_or_default()
    );

    for attendee_email in &attendee_emails {
        let raw = build_invite_message(&account.email, attendee_email, &subject, &body_text, &ical);

        if raw.is_empty() {
            log::error!(
                "send_invites: failed to build invite message for {}",
                attendee_email
            );
            continue;
        }

        if account.calendar_protocol_str() == "jmap" {
            let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;
            let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
            conn_jmap.send_email(&jmap_config, &raw).await?;
        } else {
            let (smtp_password, use_xoauth2) = get_smtp_credentials(&account).await?;
            send_raw_smtp(
                &account.smtp_host,
                account.smtp_port,
                &account.username,
                &smtp_password,
                account.use_tls,
                use_xoauth2,
                &account.email,
                attendee_email,
                &raw,
            )
            .await?;
        }
        log::info!("send_invites: sent to {}", attendee_email);
    }

    // Update event's attendees in local DB
    {
        let conn = state.db.writer().await;
        let attendees_json = serde_json::to_string(&attendees).unwrap_or_default();
        conn.execute(
            "UPDATE calendar_events SET attendees_json = ?1 WHERE id = ?2",
            rusqlite::params![attendees_json, event_id],
        )
        .ok();
    }

    log::info!("send_invites: all invites sent for event {}", event_id);
    Ok(())
}

/// Process an incoming iTIP REPLY to update attendee status on the organizer's event.
/// Called when the organizer receives a METHOD:REPLY email.
#[tauri::command]
pub async fn process_invite_reply(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
) -> Result<()> {
    log::info!(
        "process_invite_reply: account={} message={}",
        account_id,
        message_id
    );

    let raw = {
        let conn = state.db.writer().await;
        let (maildir_path, _, _, _, _, _, _) =
            db::messages::get_message_metadata(&conn, &account_id, &message_id)?;
        let full_path = crate::path_validation::resolve_under(&state.data_dir, &maildir_path)?;
        std::fs::read(&full_path)
            .map_err(|e| crate::error::Error::Other(format!("Failed to read message: {}", e)))?
    };

    let replies = ical::parse_ical_from_email(&raw);
    let reply_invites: Vec<_> = replies
        .iter()
        .filter(|inv| inv.method.to_uppercase() == "REPLY")
        .collect();

    if reply_invites.is_empty() {
        log::debug!("process_invite_reply: no METHOD:REPLY found in message");
        return Ok(());
    }

    let conn = state.db.writer().await;
    let account = db::accounts::get_account_full(&conn, &account_id)?;

    for reply in &reply_invites {
        // Find the local event by UID
        let event = db::calendar::get_event_by_uid(&conn, &account_id, &reply.uid)?;
        let Some(event) = event else {
            log::debug!("process_invite_reply: no local event for UID {}", reply.uid);
            continue;
        };

        // Extract the respondent's email and status from the REPLY
        for attendee in &reply.attendees {
            let status = &attendee.status;
            log::info!(
                "process_invite_reply: {} responded '{}' to event '{}'",
                attendee.email,
                status,
                event.title
            );

            // Update local attendees_json
            if let Some(ref att_json) = event.attendees_json {
                if let Ok(mut attendees) = serde_json::from_str::<Vec<serde_json::Value>>(att_json)
                {
                    for att in attendees.iter_mut() {
                        if att["email"].as_str() == Some(&attendee.email) {
                            att["status"] = serde_json::json!(status);
                        }
                    }
                    let updated_json = serde_json::to_string(&attendees).unwrap_or_default();
                    conn.execute(
                        "UPDATE calendar_events SET attendees_json = ?1 WHERE id = ?2",
                        rusqlite::params![updated_json, event.id],
                    )
                    .ok();
                }
            }

            // Update on JMAP server if applicable
            if account.calendar_protocol_str() == "jmap" {
                if let Some(ref remote_id) = event.remote_id {
                    let jmap_config =
                        crate::commands::sync_cmd::build_jmap_config(&account).await?;
                    // Find participant key by matching email
                    // We need to fetch the event to find the right participant key
                    drop(conn);
                    if let Ok(jmap_conn) =
                        crate::mail::jmap::JmapConnection::connect(&jmap_config).await
                    {
                        // Fetch current event to find participant key
                        if let Ok(events) =
                            jmap_conn.fetch_calendar_events(&jmap_config, None).await
                        {
                            for ev in &events {
                                if ev.id == *remote_id {
                                    // Parse attendees to find the key
                                    if let Some(ref aj) = ev.attendees_json {
                                        if let Ok(atts) =
                                            serde_json::from_str::<Vec<serde_json::Value>>(aj)
                                        {
                                            for (i, a) in atts.iter().enumerate() {
                                                if a["email"].as_str() == Some(&attendee.email) {
                                                    let key = format!("att{}", i);
                                                    jmap_conn
                                                        .update_participant_status(
                                                            &jmap_config,
                                                            remote_id,
                                                            &key,
                                                            status,
                                                        )
                                                        .await
                                                        .ok();
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    return Ok(());
                }
            }
        }
    }

    // Notify frontend to refresh calendar UI
    use tauri::Emitter as _;
    app.emit("calendar-changed", account_id.as_str()).ok();

    log::info!("process_invite_reply: completed for account {}", account_id);
    Ok(())
}

/// Process a METHOD:CANCEL email — delete the matching local event.
#[tauri::command]
pub async fn process_cancelled_invite(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
) -> Result<()> {
    log::info!(
        "process_cancelled_invite: account={} message={}",
        account_id,
        message_id
    );

    let raw = {
        let conn = state.db.reader();
        let (maildir_path, _, _, _, _, _, _) =
            db::messages::get_message_metadata(&conn, &account_id, &message_id)?;
        let full_path = crate::path_validation::resolve_under(&state.data_dir, &maildir_path)?;
        std::fs::read(&full_path)
            .map_err(|e| crate::error::Error::Other(format!("Failed to read message: {}", e)))?
    };

    let invites = ical::parse_ical_from_email(&raw);
    let cancels: Vec<_> = invites
        .iter()
        .filter(|inv| inv.method.to_uppercase() == "CANCEL")
        .collect();

    if cancels.is_empty() {
        log::debug!("process_cancelled_invite: no METHOD:CANCEL found");
        return Ok(());
    }

    let conn = state.db.writer().await;
    let mut deleted = 0;
    for cancel in &cancels {
        if let Some(event) = db::calendar::get_event_by_uid(&conn, &account_id, &cancel.uid)? {
            // Verify the CANCEL's organizer matches the event's organizer to
            // prevent spoofed CANCEL emails from deleting events.
            if let Some(ref cancel_org) = cancel.organizer_email {
                if let Some(ref event_org) = event.organizer_email {
                    if cancel_org.to_lowercase() != event_org.to_lowercase() {
                        log::warn!(
                            "process_cancelled_invite: organizer mismatch for UID={} (cancel={}, event={}), skipping",
                            cancel.uid, cancel_org, event_org
                        );
                        continue;
                    }
                }
            }
            db::calendar::delete_event(&conn, &event.id)?;
            deleted += 1;
            log::info!(
                "process_cancelled_invite: deleted event '{}' (UID={})",
                event.title,
                cancel.uid
            );
        }
    }

    if deleted > 0 {
        use tauri::Emitter as _;
        app.emit("calendar-changed", account_id.as_str()).ok();
    }

    log::info!(
        "process_cancelled_invite: completed for account {}",
        account_id
    );
    Ok(())
}

/// Auto-process calendar emails (METHOD:REPLY and METHOD:CANCEL) found
/// during mail sync. Called after new messages are synced for an account.
/// This enables Thunderbird-style automatic invite processing without
/// requiring the user to open each reply/cancel email.
pub fn auto_process_calendar_emails(
    app: &tauri::AppHandle,
    db: &std::sync::Arc<crate::db::pool::DbPool>,
    account_id: &str,
    data_dir: &std::path::Path,
    new_message_ids: &[String],
) {
    if new_message_ids.is_empty() {
        return;
    }

    // Phase 1: read-only — gather all invites from new messages.
    // Uses only the reader connection (no writer lock needed yet).
    let mut all_invites: Vec<ParsedInvite> = Vec::new();

    // Canonicalise the base once for the whole loop; individual paths are
    // still validated per-iteration against this canonical base.
    let canonical_data_dir = match std::fs::canonicalize(data_dir) {
        Ok(p) => p,
        Err(e) => {
            log::warn!(
                "auto_process_calendar_emails: cannot canonicalise data dir {}: {}",
                data_dir.display(),
                e
            );
            return;
        }
    };

    for msg_id in new_message_ids {
        let maildir_path = {
            let conn = db.reader();
            match db::messages::get_message_metadata(&conn, account_id, msg_id) {
                Ok((path, _, _, _, _, _, _)) => path,
                Err(_) => continue,
            }
        };

        if maildir_path.is_empty() {
            continue; // Body not fetched yet
        }

        let full_path = match crate::path_validation::resolve_under_canonical(
            &canonical_data_dir,
            &maildir_path,
        ) {
            Ok(p) => p,
            Err(e) => {
                log::warn!(
                    "auto_process_calendar_emails: rejecting maildir path for msg {}: {}",
                    msg_id,
                    e
                );
                continue;
            }
        };

        let raw = match std::fs::read(&full_path) {
            Ok(data) => data,
            Err(_) => continue,
        };

        let invites = ical::parse_ical_from_email(&raw);
        all_invites.extend(invites);
    }

    if all_invites.is_empty() {
        return;
    }

    // Phase 2: acquire the writer ONCE and batch all calendar updates.
    let conn_w = tokio::runtime::Handle::current().block_on(db.writer());
    let mut calendar_changed = false;

    for invite in &all_invites {
        match invite.method.to_uppercase().as_str() {
            "REPLY" => {
                // Update attendee status on organizer's local event
                if let Some(event) =
                    db::calendar::get_event_by_uid(&conn_w, account_id, &invite.uid)
                        .ok()
                        .flatten()
                {
                    for attendee in &invite.attendees {
                        if let Some(ref att_json) = event.attendees_json {
                            if let Ok(mut attendees) =
                                serde_json::from_str::<Vec<serde_json::Value>>(att_json)
                            {
                                let mut updated = false;
                                for att in attendees.iter_mut() {
                                    if att["email"].as_str() == Some(&attendee.email) {
                                        att["status"] = serde_json::json!(&attendee.status);
                                        updated = true;
                                    }
                                }
                                if updated {
                                    let updated_json =
                                        serde_json::to_string(&attendees).unwrap_or_default();
                                    conn_w.execute(
                                        "UPDATE calendar_events SET attendees_json = ?1 WHERE id = ?2",
                                        rusqlite::params![updated_json, event.id],
                                    ).ok();
                                    log::info!(
                                        "auto_process: {} responded '{}' to event '{}'",
                                        attendee.email,
                                        attendee.status,
                                        event.title
                                    );
                                    calendar_changed = true;
                                }
                            }
                        }
                    }
                }
            }
            "CANCEL" => {
                // Delete cancelled event
                if let Some(event) =
                    db::calendar::get_event_by_uid(&conn_w, account_id, &invite.uid)
                        .ok()
                        .flatten()
                {
                    if db::calendar::delete_event(&conn_w, &event.id).is_ok() {
                        log::info!(
                            "auto_process: deleted cancelled event '{}' (UID={})",
                            event.title,
                            invite.uid
                        );
                        calendar_changed = true;
                    }
                }
            }
            _ => {} // REQUEST etc. handled by user interaction
        }
    }

    // Release writer before emitting events
    drop(conn_w);

    if calendar_changed {
        use tauri::Emitter;
        app.emit("calendar-changed", account_id).ok();
    }
}

fn build_invite_message(
    from: &str,
    to: &str,
    subject: &str,
    body_text: &str,
    ical_data: &str,
) -> Vec<u8> {
    use lettre::message::{header::ContentType, Mailbox, MultiPart, SinglePart};
    use lettre::Message;

    let from_mailbox: Mailbox = match from.parse() {
        Ok(m) => m,
        Err(e) => {
            log::error!("build_invite_message: invalid from '{}': {}", from, e);
            return Vec::new();
        }
    };
    let to_mailbox: Mailbox = match to.parse() {
        Ok(m) => m,
        Err(e) => {
            log::error!("build_invite_message: invalid to '{}': {}", to, e);
            return Vec::new();
        }
    };

    match Message::builder()
        .from(from_mailbox)
        .to(to_mailbox)
        .subject(subject)
        .multipart(
            MultiPart::mixed()
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_PLAIN)
                        .body(body_text.to_string()),
                )
                .singlepart(
                    SinglePart::builder()
                        .header(
                            ContentType::parse("text/calendar; method=REQUEST; charset=UTF-8")
                                .unwrap_or(ContentType::TEXT_PLAIN),
                        )
                        .body(ical_data.to_string()),
                ),
        ) {
        Ok(msg) => msg.formatted(),
        Err(e) => {
            log::error!("build_invite_message: failed to build message: {}", e);
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Microsoft Graph calendar sync
// ---------------------------------------------------------------------------

async fn sync_calendars_graph(state: &State<'_, AppState>, account_id: &str) -> Result<()> {
    log::info!("sync_calendars_graph: starting for account {}", account_id);

    let token = match crate::mail::graph::get_graph_token(account_id).await {
        Ok(t) => t,
        Err(e) => {
            log::error!("sync_calendars_graph: failed to get token: {}", e);
            return Err(e);
        }
    };
    let client = crate::mail::graph::GraphClient::new(&token);

    // 1. List Graph calendars and upsert each into the local table.
    // Multi-calendar support (#47): we keep a remote_id -> (local_id,
    // is_subscribed) map so the per-calendar event sync below can map
    // events to the right local calendar AND skip calendars the user
    // has unsubscribed from.
    let graph_calendars = match client.list_calendars().await {
        Ok(c) => c,
        Err(e) => {
            log::error!("sync_calendars_graph: list_calendars failed: {}", e);
            return Err(e);
        }
    };
    log::info!(
        "sync_calendars_graph: fetched {} calendars",
        graph_calendars.len()
    );

    let mut remote_to_local: std::collections::HashMap<String, (String, bool)> =
        std::collections::HashMap::new();

    {
        let conn = state.db.writer().await;
        for gc in &graph_calendars {
            // Look up existing row to preserve the user's is_subscribed
            // setting; if absent, we insert and default-subscribe.
            let existing: Option<(String, bool)> = conn
                .query_row(
                    "SELECT id, is_subscribed FROM calendars WHERE account_id = ?1 AND remote_id = ?2",
                    rusqlite::params![account_id, gc.id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .ok();

            let (local_id, subscribed) = match existing {
                Some((local_id, subscribed)) => {
                    conn.execute(
                        "UPDATE calendars SET name = ?1, color = ?2 WHERE id = ?3",
                        rusqlite::params![gc.name, gc.color, local_id],
                    )
                    .ok();
                    (local_id, subscribed)
                }
                None => {
                    let cal_id = uuid::Uuid::new_v4().to_string();
                    let cal = NewCalendar {
                        account_id: account_id.to_string(),
                        name: gc.name.clone(),
                        color: gc.color.clone(),
                        is_default: gc.is_default,
                    };
                    db::calendar::insert_calendar(&conn, &cal_id, &cal)?;
                    conn.execute(
                        "UPDATE calendars SET remote_id = ?1 WHERE id = ?2",
                        rusqlite::params![gc.id, cal_id],
                    )
                    .ok();
                    log::info!(
                        "sync_calendars_graph: created calendar '{}' ({})",
                        gc.name,
                        gc.id
                    );
                    (cal_id, true)
                }
            };
            remote_to_local.insert(gc.id.clone(), (local_id, subscribed));
        }
    }

    // 2. Fetch events for each subscribed calendar individually
    // (`/me/calendars/{id}/calendarView`) — the previous all-account
    // `/me/calendarView` collapsed every calendar's events onto the
    // default calendar.
    let now = chrono::Utc::now();
    let start =
        (now - chrono::Duration::days(90)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let end = (now + chrono::Duration::days(90)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    for gc in &graph_calendars {
        let Some((local_cal_id, subscribed)) = remote_to_local.get(&gc.id) else {
            continue;
        };
        if !subscribed {
            log::debug!(
                "sync_calendars_graph: skipping unsubscribed calendar '{}'",
                gc.name
            );
            continue;
        }

        let calendar_events = match client.list_events_for_calendar(&gc.id, &start, &end).await {
            Ok(e) => e,
            Err(e) => {
                log::error!(
                    "sync_calendars_graph: list_events_for_calendar('{}') failed: {}",
                    gc.name,
                    e
                );
                continue;
            }
        };
        log::info!(
            "sync_calendars_graph: fetched {} events for calendar '{}'",
            calendar_events.len(),
            gc.name
        );

        let conn = state.db.writer().await;
        let server_ids: std::collections::HashSet<String> =
            calendar_events.iter().map(|e| e.id.clone()).collect();

        for ge in &calendar_events {
            let existing = conn.query_row(
                "SELECT id FROM calendar_events WHERE account_id = ?1 AND remote_id = ?2",
                rusqlite::params![account_id, ge.id],
                |row| row.get::<_, String>(0),
            );

            match existing {
                Ok(local_id) => {
                    // Update in place. Also re-pin calendar_id in case
                    // the event moved between calendars on the server.
                    conn.execute(
                        "UPDATE calendar_events SET title = ?1, start_time = ?2, end_time = ?3,
                         all_day = ?4, location = ?5, organizer_email = ?6, attendees_json = ?7,
                         description = ?8, timezone = ?9, calendar_id = ?10 WHERE id = ?11",
                        rusqlite::params![
                            ge.subject,
                            ge.start,
                            ge.end,
                            ge.all_day,
                            ge.location,
                            ge.organizer_email,
                            ge.attendees_json,
                            ge.body_preview,
                            ge.timezone,
                            local_cal_id,
                            local_id,
                        ],
                    )
                    .ok();
                }
                Err(_) => {
                    let event = CalendarEvent {
                        id: uuid::Uuid::new_v4().to_string(),
                        account_id: account_id.to_string(),
                        calendar_id: local_cal_id.clone(),
                        uid: ge.ical_uid.clone(),
                        title: ge.subject.clone(),
                        description: ge.body_preview.clone(),
                        location: ge.location.clone(),
                        start_time: ge.start.clone(),
                        end_time: ge.end.clone(),
                        all_day: ge.all_day,
                        timezone: ge.timezone.clone(),
                        recurrence_rule: None,
                        organizer_email: ge.organizer_email.clone(),
                        attendees_json: ge.attendees_json.clone(),
                        my_status: None,
                        source_message_id: None,
                        ical_data: None,
                        remote_id: Some(ge.id.clone()),
                        etag: None,
                    };
                    db::calendar::insert_event(&conn, &event)?;
                }
            }
        }

        // Per-calendar reconciliation: drop events that this calendar
        // used to carry but that the server no longer returns. Scoped
        // to calendar_id so a deletion in one calendar doesn't wipe
        // events still present in another.
        let local_events: Vec<(String, String)> = conn
            .prepare(
                "SELECT id, remote_id FROM calendar_events
                 WHERE account_id = ?1 AND calendar_id = ?2
                   AND remote_id IS NOT NULL AND remote_id != ''",
            )?
            .query_map(rusqlite::params![account_id, local_cal_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let mut deleted = 0;
        for (local_id, remote_id) in &local_events {
            if !server_ids.contains(remote_id) {
                db::calendar::delete_event(&conn, local_id)?;
                deleted += 1;
            }
        }
        if deleted > 0 {
            log::info!(
                "sync_calendars_graph: removed {} server-deleted events from '{}'",
                deleted,
                gc.name
            );
        }
    }

    log::info!("sync_calendars_graph: completed for account {}", account_id);
    Ok(())
}

/// Return all IANA timezone names from the chrono-tz database.
#[tauri::command]
pub fn list_timezones() -> Vec<String> {
    let mut tzs: Vec<String> = chrono_tz::TZ_VARIANTS
        .iter()
        .map(|tz| tz.name().to_string())
        .collect();
    tzs.sort();
    tzs
}

/// Return the OS timezone, falling back to "UTC".
#[tauri::command]
pub fn get_default_timezone() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string())
}
