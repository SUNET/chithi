use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::Result;

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Calendar {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub color: String,
    pub is_default: bool,
    pub remote_id: Option<String>,
    pub is_subscribed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewCalendar {
    pub account_id: String,
    pub name: String,
    pub color: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    pub account_id: String,
    pub calendar_id: String,
    pub uid: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_time: String,
    pub end_time: String,
    pub all_day: bool,
    pub timezone: Option<String>,
    pub recurrence_rule: Option<String>,
    pub organizer_email: Option<String>,
    pub attendees_json: Option<String>,
    pub my_status: Option<String>,
    pub source_message_id: Option<String>,
    pub ical_data: Option<String>,
    pub remote_id: Option<String>,
    pub etag: Option<String>,
    /// Online-meeting (Teams) join URL attached to this event. Populated
    /// for O365 events created/synced with `isOnlineMeeting` (ADR 0050);
    /// `None` for everything else.
    #[serde(default)]
    pub online_meeting_url: Option<String>,
}

/// Attendee for JSON serialization inside `attendees_json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attendee {
    pub email: String,
    pub name: Option<String>,
    pub status: String, // "accepted", "tentative", "declined", "needs-action"
}

/// A calendar invite — a `CalendarEvent` plus its row `created_at`
/// timestamp. Returned by `list_invites` for the Invites view, where the
/// arrival time backs the "recently received" sort. `created_at` is kept
/// off `CalendarEvent` itself so the many event-construction sites in the
/// sync code are unaffected; `#[serde(flatten)]` keeps the wire JSON flat.
#[derive(Debug, Clone, Serialize)]
pub struct Invite {
    #[serde(flatten)]
    pub event: CalendarEvent,
    pub created_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Calendar CRUD
// ---------------------------------------------------------------------------

pub fn list_calendars(conn: &Connection, account_id: &str) -> Result<Vec<Calendar>> {
    let mut stmt = conn.prepare(
        "SELECT id, account_id, name, color, is_default, remote_id, is_subscribed
         FROM calendars
         WHERE account_id = ?1
         ORDER BY is_default DESC, name ASC",
    )?;
    let rows = stmt
        .query_map(params![account_id], |row| {
            Ok(Calendar {
                id: row.get(0)?,
                account_id: row.get(1)?,
                name: row.get(2)?,
                color: row.get(3)?,
                is_default: row.get(4)?,
                remote_id: row.get(5)?,
                is_subscribed: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn get_calendar(conn: &Connection, id: &str) -> Result<Calendar> {
    conn.query_row(
        "SELECT id, account_id, name, color, is_default, remote_id, is_subscribed
         FROM calendars WHERE id = ?1",
        params![id],
        |row| {
            Ok(Calendar {
                id: row.get(0)?,
                account_id: row.get(1)?,
                name: row.get(2)?,
                color: row.get(3)?,
                is_default: row.get(4)?,
                remote_id: row.get(5)?,
                is_subscribed: row.get(6)?,
            })
        },
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => {
            crate::error::Error::Other(format!("Calendar not found: {}", id))
        }
        other => crate::error::Error::Database(other),
    })
}

pub fn set_calendar_subscribed(conn: &Connection, id: &str, subscribed: bool) -> Result<()> {
    let rows = conn.execute(
        "UPDATE calendars SET is_subscribed = ?1 WHERE id = ?2",
        params![subscribed, id],
    )?;
    if rows == 0 {
        return Err(crate::error::Error::Other(format!(
            "Calendar not found: {}",
            id
        )));
    }
    Ok(())
}

pub fn delete_calendar_events(conn: &Connection, calendar_id: &str) -> Result<usize> {
    let count = conn.execute(
        "DELETE FROM calendar_events WHERE calendar_id = ?1",
        params![calendar_id],
    )?;
    Ok(count)
}

pub fn insert_calendar(conn: &Connection, id: &str, calendar: &NewCalendar) -> Result<()> {
    conn.execute(
        "INSERT INTO calendars (id, account_id, name, color, is_default)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            id,
            calendar.account_id,
            calendar.name,
            calendar.color,
            calendar.is_default,
        ],
    )?;
    Ok(())
}

pub fn update_calendar(conn: &Connection, id: &str, name: &str, color: &str) -> Result<()> {
    let rows = conn.execute(
        "UPDATE calendars SET name = ?1, color = ?2 WHERE id = ?3",
        params![name, color, id],
    )?;
    if rows == 0 {
        return Err(crate::error::Error::Other(format!(
            "Calendar not found: {}",
            id
        )));
    }
    Ok(())
}

pub fn delete_calendar(conn: &Connection, id: &str) -> Result<()> {
    // Delete associated events first
    conn.execute(
        "DELETE FROM calendar_events WHERE calendar_id = ?1",
        params![id],
    )?;
    conn.execute("DELETE FROM calendars WHERE id = ?1", params![id])?;
    Ok(())
}

/// Upsert a calendar by remote_id. If a calendar with the same
/// (account_id, remote_id) already exists, update its name and
/// is_default flag — but **preserve the user-customized color**.
/// CalDAV / JMAP / Google rarely expose a stable "calendar color"
/// property, so on every resync the caller would otherwise hand us
/// a fallback colour like `#4285f4` and stomp the user's pick from
/// the sidebar's color picker (#132). On INSERT the supplied color
/// is what gets stored.
///
/// Returns the local calendar ID.
pub fn upsert_calendar_by_remote_id(
    conn: &Connection,
    account_id: &str,
    remote_id: &str,
    name: &str,
    color: &str,
    is_default: bool,
) -> Result<String> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM calendars WHERE account_id = ?1 AND remote_id = ?2",
            params![account_id, remote_id],
            |row| row.get(0),
        )
        .ok();

    if let Some(id) = existing {
        conn.execute(
            "UPDATE calendars SET name = ?1, is_default = ?2 WHERE id = ?3",
            params![name, is_default, id],
        )?;
        Ok(id)
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO calendars (id, account_id, name, color, is_default, remote_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, account_id, name, color, is_default, remote_id],
        )?;
        Ok(id)
    }
}

/// Upsert an event by remote_id. If an event with the same remote_id already exists,
/// update it. Otherwise insert a new row.
pub fn upsert_event_by_remote_id(conn: &Connection, event: &CalendarEvent) -> Result<()> {
    if let Some(ref remote_id) = event.remote_id {
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM calendar_events WHERE account_id = ?1 AND remote_id = ?2",
                params![event.account_id, remote_id],
                |row| row.get(0),
            )
            .ok();

        if let Some(existing_id) = existing {
            // Update the existing event, keeping its local ID
            conn.execute(
                "UPDATE calendar_events SET
                    calendar_id = ?1, uid = ?2, title = ?3, description = ?4,
                    location = ?5, start_time = ?6, end_time = ?7, all_day = ?8,
                    timezone = ?9, recurrence_rule = ?10, organizer_email = ?11,
                    attendees_json = ?12, my_status = ?13, source_message_id = ?14,
                    ical_data = ?15, remote_id = ?16, etag = ?17, online_meeting_url = ?18,
                    updated_at = CURRENT_TIMESTAMP
                 WHERE id = ?19",
                params![
                    event.calendar_id,
                    event.uid,
                    event.title,
                    event.description,
                    event.location,
                    event.start_time,
                    event.end_time,
                    event.all_day,
                    event.timezone,
                    event.recurrence_rule,
                    event.organizer_email,
                    event.attendees_json,
                    event.my_status,
                    event.source_message_id,
                    event.ical_data,
                    event.remote_id,
                    event.etag,
                    event.online_meeting_url,
                    existing_id,
                ],
            )?;
            return Ok(());
        }
    }

    // No remote_id match — insert (INSERT OR REPLACE keyed on primary id)
    insert_event(conn, event)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Event CRUD
// ---------------------------------------------------------------------------

pub fn list_events(
    conn: &Connection,
    account_id: &str,
    calendar_id: Option<&str>,
    start: &str,
    end: &str,
) -> Result<Vec<CalendarEvent>> {
    // Single-shot events are filtered to those that overlap the visible
    // window. Recurring events (`recurrence_rule` non-empty) ALWAYS pass
    // through regardless of their master row's date range — their
    // occurrences are computed by the frontend's expandRRule, and the
    // master row's start_time can be years before the visible window.
    // Without this carve-out, an event whose RRULE master sits in 2021
    // never reaches expandRRule when the user views 2026.
    let (query, do_calendar_filter) = if calendar_id.is_some() {
        (
            "SELECT id, account_id, calendar_id, uid, title, description, location,
                    start_time, end_time, all_day, timezone, recurrence_rule,
                    organizer_email, attendees_json, my_status, source_message_id,
                    ical_data, remote_id, etag, online_meeting_url
             FROM calendar_events
             WHERE account_id = ?1 AND calendar_id = ?2
               AND ((start_time < ?4 AND end_time > ?3)
                    OR (recurrence_rule IS NOT NULL AND recurrence_rule != ''))
             ORDER BY start_time ASC",
            true,
        )
    } else {
        (
            "SELECT id, account_id, calendar_id, uid, title, description, location,
                    start_time, end_time, all_day, timezone, recurrence_rule,
                    organizer_email, attendees_json, my_status, source_message_id,
                    ical_data, remote_id, etag, online_meeting_url
             FROM calendar_events
             WHERE account_id = ?1
               AND ((start_time < ?3 AND end_time > ?2)
                    OR (recurrence_rule IS NOT NULL AND recurrence_rule != ''))
             ORDER BY start_time ASC",
            false,
        )
    };

    let mut stmt = conn.prepare(query)?;

    let rows = if do_calendar_filter {
        let cal_id = calendar_id.unwrap_or("");
        stmt.query_map(params![account_id, cal_id, start, end], map_event_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        stmt.query_map(params![account_id, start, end], map_event_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };

    Ok(rows)
}

/// List all invites for an account — events where the account is an
/// attendee but NOT the organizer. Used by the dedicated Invites view.
///
/// `since` (ISO 8601) bounds the recent past: single-shot events whose
/// `end_time` is older than `since` are dropped. Recurring events
/// (non-empty `recurrence_rule`) always pass the date filter — the
/// frontend computes their next occurrence.
///
/// SQL narrows by date and organizer; the attendee-membership check is
/// done in Rust because `attendees_json` is an opaque JSON blob that
/// SQLite cannot reliably search.
pub fn list_invites(
    conn: &Connection,
    account_id: &str,
    account_email: &str,
    since: &str,
) -> Result<Vec<Invite>> {
    let mut stmt = conn.prepare(
        "SELECT id, account_id, calendar_id, uid, title, description, location,
                start_time, end_time, all_day, timezone, recurrence_rule,
                organizer_email, attendees_json, my_status, source_message_id,
                ical_data, remote_id, etag, online_meeting_url, created_at
         FROM calendar_events
         WHERE account_id = ?1
           AND organizer_email IS NOT NULL
           AND organizer_email != ''
           AND organizer_email <> ?2 COLLATE NOCASE
           AND (end_time > ?3
                OR (recurrence_rule IS NOT NULL AND recurrence_rule != ''))
         ORDER BY start_time ASC",
    )?;
    let rows = stmt
        .query_map(params![account_id, account_email, since], |row| {
            Ok(Invite {
                event: map_event_row(row)?,
                created_at: row.get(20)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    // Keep only events where the account's own address is among the
    // attendees (case-insensitive) — i.e. you were genuinely invited,
    // not merely the owner of a calendar that holds the event.
    let me = account_email.to_lowercase();
    let filtered = rows
        .into_iter()
        .filter(|inv| match inv.event.attendees_json.as_deref() {
            Some(json) => serde_json::from_str::<Vec<Attendee>>(json)
                .map(|atts| atts.iter().any(|a| a.email.to_lowercase() == me))
                .unwrap_or(false),
            None => false,
        })
        .collect();
    Ok(filtered)
}

pub fn get_event(conn: &Connection, id: &str) -> Result<CalendarEvent> {
    conn.query_row(
        "SELECT id, account_id, calendar_id, uid, title, description, location,
                start_time, end_time, all_day, timezone, recurrence_rule,
                organizer_email, attendees_json, my_status, source_message_id,
                ical_data, remote_id, etag, online_meeting_url
         FROM calendar_events WHERE id = ?1",
        params![id],
        map_event_row,
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => {
            crate::error::Error::Other(format!("Calendar event not found: {}", id))
        }
        other => crate::error::Error::Database(other),
    })
}

pub fn insert_event(conn: &Connection, event: &CalendarEvent) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO calendar_events
         (id, account_id, calendar_id, uid, title, description, location,
          start_time, end_time, all_day, timezone, recurrence_rule,
          organizer_email, attendees_json, my_status, source_message_id,
          ical_data, remote_id, etag, online_meeting_url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
        params![
            event.id,
            event.account_id,
            event.calendar_id,
            event.uid,
            event.title,
            event.description,
            event.location,
            event.start_time,
            event.end_time,
            event.all_day,
            event.timezone,
            event.recurrence_rule,
            event.organizer_email,
            event.attendees_json,
            event.my_status,
            event.source_message_id,
            event.ical_data,
            event.remote_id,
            event.etag,
            event.online_meeting_url,
        ],
    )?;
    Ok(())
}

pub fn update_event(conn: &Connection, event: &CalendarEvent) -> Result<()> {
    let rows = conn.execute(
        "UPDATE calendar_events SET
            calendar_id = ?1, uid = ?2, title = ?3, description = ?4,
            location = ?5, start_time = ?6, end_time = ?7, all_day = ?8,
            timezone = ?9, recurrence_rule = ?10, organizer_email = ?11,
            attendees_json = ?12, my_status = ?13, source_message_id = ?14,
            ical_data = ?15, remote_id = ?16, etag = ?17, online_meeting_url = ?18,
            updated_at = CURRENT_TIMESTAMP
         WHERE id = ?19",
        params![
            event.calendar_id,
            event.uid,
            event.title,
            event.description,
            event.location,
            event.start_time,
            event.end_time,
            event.all_day,
            event.timezone,
            event.recurrence_rule,
            event.organizer_email,
            event.attendees_json,
            event.my_status,
            event.source_message_id,
            event.ical_data,
            event.remote_id,
            event.etag,
            event.online_meeting_url,
            event.id,
        ],
    )?;
    if rows == 0 {
        return Err(crate::error::Error::Other(format!(
            "Calendar event not found: {}",
            event.id
        )));
    }
    Ok(())
}

pub fn delete_event(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM calendar_events WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn get_event_by_uid(
    conn: &Connection,
    account_id: &str,
    uid: &str,
) -> Result<Option<CalendarEvent>> {
    let result = conn.query_row(
        "SELECT id, account_id, calendar_id, uid, title, description, location,
                start_time, end_time, all_day, timezone, recurrence_rule,
                organizer_email, attendees_json, my_status, source_message_id,
                ical_data, remote_id, etag, online_meeting_url
         FROM calendar_events
         WHERE account_id = ?1 AND uid = ?2
         LIMIT 1",
        params![account_id, uid],
        map_event_row,
    );

    match result {
        Ok(event) => Ok(Some(event)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(crate::error::Error::Database(e)),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn map_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CalendarEvent> {
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
        online_meeting_url: row.get(19)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE accounts (
                id TEXT PRIMARY KEY,
                display_name TEXT NOT NULL,
                email TEXT NOT NULL,
                provider TEXT NOT NULL,
                mail_protocol TEXT NOT NULL DEFAULT 'imap',
                imap_host TEXT NOT NULL DEFAULT '',
                imap_port INTEGER NOT NULL DEFAULT 993,
                smtp_host TEXT NOT NULL DEFAULT '',
                smtp_port INTEGER NOT NULL DEFAULT 587,
                jmap_url TEXT NOT NULL DEFAULT '',
                username TEXT NOT NULL,
                password TEXT NOT NULL,
                use_tls INTEGER NOT NULL DEFAULT 1,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE calendars (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                color TEXT DEFAULT '#4285f4',
                is_default INTEGER DEFAULT 0,
                remote_id TEXT,
                is_subscribed INTEGER NOT NULL DEFAULT 1,
                UNIQUE(account_id, remote_id)
            );
            CREATE TABLE calendar_events (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                calendar_id TEXT NOT NULL,
                uid TEXT,
                title TEXT NOT NULL,
                description TEXT,
                location TEXT,
                start_time TEXT NOT NULL,
                end_time TEXT NOT NULL,
                all_day INTEGER DEFAULT 0,
                timezone TEXT,
                recurrence_rule TEXT,
                organizer_email TEXT,
                attendees_json TEXT,
                my_status TEXT,
                source_message_id TEXT,
                ical_data TEXT,
                remote_id TEXT,
                etag TEXT,
                online_meeting_url TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO accounts (id, display_name, email, provider, username, password)
            VALUES ('acc1', 'Test', 'test@example.com', 'generic', 'user', 'pass');
            ",
        )
        .unwrap();
        conn
    }

    fn make_event(id: &str, title: &str, remote_id: Option<&str>) -> CalendarEvent {
        CalendarEvent {
            id: id.to_string(),
            account_id: "acc1".to_string(),
            calendar_id: "cal1".to_string(),
            uid: Some(format!("{}@test", id)),
            title: title.to_string(),
            description: None,
            location: None,
            start_time: "2026-04-07T17:00:00Z".to_string(),
            end_time: "2026-04-07T18:00:00Z".to_string(),
            all_day: false,
            timezone: None,
            recurrence_rule: None,
            organizer_email: None,
            attendees_json: None,
            my_status: None,
            source_message_id: None,
            ical_data: None,
            remote_id: remote_id.map(|s| s.to_string()),
            etag: None,
            online_meeting_url: None,
        }
    }

    #[test]
    fn test_insert_and_get_event() {
        let conn = setup_db();
        let event = make_event("e1", "Meeting", None);
        insert_event(&conn, &event).unwrap();

        let fetched = get_event(&conn, "e1").unwrap();
        assert_eq!(fetched.title, "Meeting");
        assert_eq!(fetched.uid, Some("e1@test".to_string()));
        assert!(fetched.remote_id.is_none());
    }

    #[test]
    fn test_online_meeting_url_roundtrips() {
        let conn = setup_db();
        // Insert with a Teams join URL, then read it back (ADR 0050).
        let mut event = make_event("e1", "Teams call", None);
        event.online_meeting_url =
            Some("https://teams.microsoft.com/l/meetup-join/abc".to_string());
        insert_event(&conn, &event).unwrap();
        let fetched = get_event(&conn, "e1").unwrap();
        assert_eq!(
            fetched.online_meeting_url.as_deref(),
            Some("https://teams.microsoft.com/l/meetup-join/abc")
        );

        // Clearing it (best-effort Teams removal) round-trips through update.
        let mut cleared = fetched;
        cleared.online_meeting_url = None;
        update_event(&conn, &cleared).unwrap();
        assert_eq!(get_event(&conn, "e1").unwrap().online_meeting_url, None);

        // Events created without a meeting have a NULL URL.
        let plain = make_event("e2", "No call", None);
        insert_event(&conn, &plain).unwrap();
        assert_eq!(get_event(&conn, "e2").unwrap().online_meeting_url, None);
    }

    #[test]
    fn test_insert_then_update_remote_id() {
        let conn = setup_db();
        let event = make_event("e1", "Meeting", None);
        insert_event(&conn, &event).unwrap();

        // Simulate what create_event does: INSERT first, then UPDATE remote_id
        conn.execute(
            "UPDATE calendar_events SET remote_id = ?1 WHERE id = ?2",
            params!["remote-abc", "e1"],
        )
        .unwrap();

        let fetched = get_event(&conn, "e1").unwrap();
        assert_eq!(fetched.remote_id, Some("remote-abc".to_string()));
    }

    #[test]
    fn test_update_before_insert_has_no_effect() {
        let conn = setup_db();

        // UPDATE on non-existent row does nothing (the old bug)
        let rows = conn
            .execute(
                "UPDATE calendar_events SET remote_id = ?1 WHERE id = ?2",
                params!["remote-abc", "e1"],
            )
            .unwrap();
        assert_eq!(rows, 0, "UPDATE on non-existent row should affect 0 rows");

        // INSERT with remote_id = None
        let event = make_event("e1", "Meeting", None);
        insert_event(&conn, &event).unwrap();

        let fetched = get_event(&conn, "e1").unwrap();
        assert!(
            fetched.remote_id.is_none(),
            "remote_id should be None because UPDATE happened before INSERT"
        );
    }

    #[test]
    fn test_delete_event() {
        let conn = setup_db();
        let event = make_event("e1", "Meeting", None);
        insert_event(&conn, &event).unwrap();

        delete_event(&conn, "e1").unwrap();

        let result = get_event(&conn, "e1");
        assert!(result.is_err(), "Event should not exist after deletion");
    }

    #[test]
    fn test_get_event_by_uid() {
        let conn = setup_db();
        let event = make_event("e1", "Meeting", None);
        insert_event(&conn, &event).unwrap();

        let found = get_event_by_uid(&conn, "acc1", "e1@test").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().title, "Meeting");

        let not_found = get_event_by_uid(&conn, "acc1", "nonexistent@test").unwrap();
        assert!(not_found.is_none());
    }

    #[test]
    fn test_get_event_by_uid_returns_my_status() {
        let conn = setup_db();
        let mut event = make_event("e1", "Invite Meeting", None);
        event.my_status = Some("accepted".to_string());
        insert_event(&conn, &event).unwrap();

        let found = get_event_by_uid(&conn, "acc1", "e1@test").unwrap().unwrap();
        assert_eq!(found.my_status, Some("accepted".to_string()));
    }

    fn attendees_json(emails: &[&str]) -> String {
        let atts: Vec<Attendee> = emails
            .iter()
            .map(|e| Attendee {
                email: e.to_string(),
                name: None,
                status: "needs-action".to_string(),
            })
            .collect();
        serde_json::to_string(&atts).unwrap()
    }

    #[test]
    fn test_list_invites_basic() {
        let conn = setup_db();
        let since = "2026-01-01T00:00:00Z";

        // 1. A genuine invite: someone else organizes, we attend.
        let mut invite = make_event("inv1", "Team Sync", None);
        invite.organizer_email = Some("boss@example.com".to_string());
        invite.attendees_json = Some(attendees_json(&["test@example.com", "boss@example.com"]));
        insert_event(&conn, &invite).unwrap();

        // 2. Self-organized event — excluded (organizer == account email).
        let mut mine = make_event("mine1", "My Event", None);
        mine.organizer_email = Some("test@example.com".to_string());
        mine.attendees_json = Some(attendees_json(&["test@example.com"]));
        insert_event(&conn, &mine).unwrap();

        // 3. Event with no organizer — excluded.
        let plain = make_event("plain1", "Personal", None);
        insert_event(&conn, &plain).unwrap();

        // 4. Organized by someone else but we're not an attendee — excluded.
        let mut other = make_event("other1", "Not Mine", None);
        other.organizer_email = Some("boss@example.com".to_string());
        other.attendees_json = Some(attendees_json(&["someone@example.com"]));
        insert_event(&conn, &other).unwrap();

        let invites = list_invites(&conn, "acc1", "test@example.com", since).unwrap();
        assert_eq!(invites.len(), 1);
        assert_eq!(invites[0].event.id, "inv1");
    }

    #[test]
    fn test_list_invites_organizer_match_is_case_insensitive() {
        let conn = setup_db();
        let mut mine = make_event("mine1", "My Event", None);
        mine.organizer_email = Some("TEST@Example.com".to_string());
        mine.attendees_json = Some(attendees_json(&["test@example.com"]));
        insert_event(&conn, &mine).unwrap();

        let invites =
            list_invites(&conn, "acc1", "test@example.com", "2026-01-01T00:00:00Z").unwrap();
        assert!(
            invites.is_empty(),
            "self-organized event must be excluded regardless of email case"
        );
    }

    #[test]
    fn test_list_invites_date_window() {
        let conn = setup_db();
        let since = "2026-04-01T00:00:00Z";

        // Past non-recurring invite (ended before `since`) — excluded.
        let mut past = make_event("past1", "Old Meeting", None);
        past.start_time = "2026-03-01T10:00:00Z".to_string();
        past.end_time = "2026-03-01T11:00:00Z".to_string();
        past.organizer_email = Some("boss@example.com".to_string());
        past.attendees_json = Some(attendees_json(&["test@example.com"]));
        insert_event(&conn, &past).unwrap();

        // Past recurring invite — always included (occurrences computed later).
        let mut recurring = make_event("rec1", "Weekly Standup", None);
        recurring.start_time = "2026-01-01T10:00:00Z".to_string();
        recurring.end_time = "2026-01-01T10:30:00Z".to_string();
        recurring.recurrence_rule = Some("FREQ=WEEKLY".to_string());
        recurring.organizer_email = Some("boss@example.com".to_string());
        recurring.attendees_json = Some(attendees_json(&["test@example.com"]));
        insert_event(&conn, &recurring).unwrap();

        let invites = list_invites(&conn, "acc1", "test@example.com", since).unwrap();
        assert_eq!(invites.len(), 1);
        assert_eq!(invites[0].event.id, "rec1");
    }

    #[test]
    fn test_upsert_event_by_remote_id_insert() {
        let conn = setup_db();
        let event = make_event("e1", "Remote Event", Some("remote-1"));
        upsert_event_by_remote_id(&conn, &event).unwrap();

        let fetched = get_event(&conn, "e1").unwrap();
        assert_eq!(fetched.title, "Remote Event");
        assert_eq!(fetched.remote_id, Some("remote-1".to_string()));
    }

    #[test]
    fn test_upsert_event_by_remote_id_update() {
        let conn = setup_db();
        let event = make_event("e1", "Original", Some("remote-1"));
        upsert_event_by_remote_id(&conn, &event).unwrap();

        // Upsert again with same remote_id but different local ID and title
        let updated = CalendarEvent {
            id: "e2".to_string(),
            title: "Updated".to_string(),
            remote_id: Some("remote-1".to_string()),
            ..make_event("e2", "Updated", Some("remote-1"))
        };
        upsert_event_by_remote_id(&conn, &updated).unwrap();

        // The original row (id=e1) should be updated, not a new row created
        let fetched = get_event(&conn, "e1").unwrap();
        assert_eq!(fetched.title, "Updated");

        // e2 should not exist as a separate row
        assert!(get_event(&conn, "e2").is_err());
    }

    #[test]
    fn test_upsert_event_no_remote_id_inserts_new() {
        let conn = setup_db();
        let event = make_event("e1", "Local Event", None);
        upsert_event_by_remote_id(&conn, &event).unwrap();

        let fetched = get_event(&conn, "e1").unwrap();
        assert_eq!(fetched.title, "Local Event");
    }

    #[test]
    fn test_list_events_by_time_range() {
        let conn = setup_db();
        let e1 = CalendarEvent {
            start_time: "2026-04-07T10:00:00Z".to_string(),
            end_time: "2026-04-07T11:00:00Z".to_string(),
            ..make_event("e1", "Morning", None)
        };
        let e2 = CalendarEvent {
            start_time: "2026-04-07T20:00:00Z".to_string(),
            end_time: "2026-04-07T21:00:00Z".to_string(),
            ..make_event("e2", "Evening", None)
        };
        let e3 = CalendarEvent {
            start_time: "2026-04-08T10:00:00Z".to_string(),
            end_time: "2026-04-08T11:00:00Z".to_string(),
            ..make_event("e3", "Tomorrow", None)
        };
        insert_event(&conn, &e1).unwrap();
        insert_event(&conn, &e2).unwrap();
        insert_event(&conn, &e3).unwrap();

        let events = list_events(
            &conn,
            "acc1",
            None,
            "2026-04-07T00:00:00Z",
            "2026-04-07T23:59:59Z",
        )
        .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].title, "Morning");
        assert_eq!(events[1].title, "Evening");
    }

    #[test]
    fn test_calendar_crud() {
        let conn = setup_db();
        let cal = NewCalendar {
            account_id: "acc1".to_string(),
            name: "Work".to_string(),
            color: "#ff0000".to_string(),
            is_default: true,
        };
        insert_calendar(&conn, "cal1", &cal).unwrap();

        let fetched = get_calendar(&conn, "cal1").unwrap();
        assert_eq!(fetched.name, "Work");
        assert_eq!(fetched.color, "#ff0000");

        update_calendar(&conn, "cal1", "Personal", "#00ff00").unwrap();
        let updated = get_calendar(&conn, "cal1").unwrap();
        assert_eq!(updated.name, "Personal");
        assert_eq!(updated.color, "#00ff00");
    }

    #[test]
    fn test_upsert_calendar_by_remote_id() {
        let conn = setup_db();

        // First upsert creates a new calendar with the supplied color.
        let id1 =
            upsert_calendar_by_remote_id(&conn, "acc1", "remote-cal", "Work", "#4285f4", true)
                .unwrap();

        let cal = get_calendar(&conn, &id1).unwrap();
        assert_eq!(cal.name, "Work");
        assert_eq!(cal.color, "#4285f4");
        assert_eq!(cal.remote_id, Some("remote-cal".to_string()));

        // Second upsert with same remote_id updates name + is_default
        // but **must preserve the existing color** so a resync doesn't
        // stomp the user's manual pick (#132).
        let id2 = upsert_calendar_by_remote_id(
            &conn,
            "acc1",
            "remote-cal",
            "Work Updated",
            "#ff0000",
            false,
        )
        .unwrap();

        assert_eq!(id1, id2, "Should return same local ID");
        let updated = get_calendar(&conn, &id2).unwrap();
        assert_eq!(updated.name, "Work Updated");
        assert!(!updated.is_default);
        assert_eq!(
            updated.color, "#4285f4",
            "color must be preserved across resync"
        );
    }

    #[test]
    fn test_delete_calendar_cascades_events() {
        let conn = setup_db();
        let cal = NewCalendar {
            account_id: "acc1".to_string(),
            name: "Work".to_string(),
            color: "#4285f4".to_string(),
            is_default: true,
        };
        insert_calendar(&conn, "cal1", &cal).unwrap();

        let event = make_event("e1", "Meeting", None);
        insert_event(&conn, &event).unwrap();

        delete_calendar(&conn, "cal1").unwrap();

        assert!(
            get_event(&conn, "e1").is_err(),
            "Events should be deleted with calendar"
        );
    }

    #[test]
    fn test_deletion_reconciliation_pattern() {
        // Simulates the sync deletion reconciliation:
        // server has events A, B. Local has A, B, C (C was deleted on server).
        let conn = setup_db();
        insert_event(&conn, &make_event("e1", "A", Some("remote-a"))).unwrap();
        insert_event(&conn, &make_event("e2", "B", Some("remote-b"))).unwrap();
        insert_event(&conn, &make_event("e3", "C", Some("remote-c"))).unwrap();

        // Server only has A and B
        let server_ids: std::collections::HashSet<String> = ["remote-a", "remote-b"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // Find local events with remote_id
        let local_synced: Vec<(String, String)> = conn
            .prepare(
                "SELECT id, remote_id FROM calendar_events WHERE account_id = ?1 AND remote_id IS NOT NULL",
            )
            .unwrap()
            .query_map(params!["acc1"], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        let mut deleted = 0u32;
        for (local_id, remote_id) in &local_synced {
            if !server_ids.contains(remote_id) {
                conn.execute(
                    "DELETE FROM calendar_events WHERE id = ?1",
                    params![local_id],
                )
                .ok();
                deleted += 1;
            }
        }

        assert_eq!(deleted, 1);
        assert!(get_event(&conn, "e1").is_ok());
        assert!(get_event(&conn, "e2").is_ok());
        assert!(get_event(&conn, "e3").is_err(), "C should be deleted");
    }

    fn make_cal(account_id: &str, name: &str, is_default: bool) -> NewCalendar {
        NewCalendar {
            account_id: account_id.to_string(),
            name: name.to_string(),
            color: "#4285f4".to_string(),
            is_default,
        }
    }

    #[test]
    fn test_set_calendar_subscribed() {
        let conn = setup_db();
        insert_calendar(&conn, "cal1", &make_cal("acc1", "Work", true)).unwrap();

        let cal = get_calendar(&conn, "cal1").unwrap();
        assert!(cal.is_subscribed);

        set_calendar_subscribed(&conn, "cal1", false).unwrap();
        let cal = get_calendar(&conn, "cal1").unwrap();
        assert!(!cal.is_subscribed);

        set_calendar_subscribed(&conn, "cal1", true).unwrap();
        let cal = get_calendar(&conn, "cal1").unwrap();
        assert!(cal.is_subscribed);
    }

    #[test]
    fn test_set_calendar_subscribed_not_found() {
        let conn = setup_db();
        let result = set_calendar_subscribed(&conn, "nonexistent", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_delete_calendar_events() {
        let conn = setup_db();
        insert_calendar(&conn, "cal1", &make_cal("acc1", "Work", true)).unwrap();
        insert_calendar(&conn, "cal2", &make_cal("acc1", "Personal", false)).unwrap();
        let mut e1 = make_event("e1", "Event 1", None);
        e1.calendar_id = "cal1".to_string();
        insert_event(&conn, &e1).unwrap();
        let mut e2 = make_event("e2", "Event 2", None);
        e2.calendar_id = "cal1".to_string();
        insert_event(&conn, &e2).unwrap();
        let mut e3 = make_event("e3", "Event 3", None);
        e3.calendar_id = "cal2".to_string();
        insert_event(&conn, &e3).unwrap();

        let deleted = delete_calendar_events(&conn, "cal1").unwrap();
        assert_eq!(deleted, 2);

        assert!(get_event(&conn, "e3").is_ok());
        assert!(get_event(&conn, "e1").is_err());
        assert!(get_event(&conn, "e2").is_err());
    }

    #[test]
    fn test_delete_calendar_events_empty() {
        let conn = setup_db();
        insert_calendar(&conn, "cal1", &make_cal("acc1", "Work", true)).unwrap();
        let deleted = delete_calendar_events(&conn, "cal1").unwrap();
        assert_eq!(deleted, 0);
    }
}
