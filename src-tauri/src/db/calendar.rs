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
}

/// Attendee for JSON serialization inside `attendees_json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attendee {
    pub email: String,
    pub name: Option<String>,
    pub status: String, // "accepted", "tentative", "declined", "needs-action"
}

// ---------------------------------------------------------------------------
// Calendar CRUD
// ---------------------------------------------------------------------------

pub fn list_calendars(conn: &Connection, account_id: &str) -> Result<Vec<Calendar>> {
    let mut stmt = conn.prepare(
        "SELECT id, account_id, name, color, is_default, remote_id
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
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn get_calendar(conn: &Connection, id: &str) -> Result<Calendar> {
    conn.query_row(
        "SELECT id, account_id, name, color, is_default, remote_id
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

/// Upsert a calendar by remote_id. If a calendar with the same (account_id, remote_id)
/// already exists, update its name and color. Otherwise insert a new row.
/// Returns the local calendar ID.
pub fn upsert_calendar_by_remote_id(
    conn: &Connection,
    account_id: &str,
    remote_id: &str,
    name: &str,
    color: &str,
    is_default: bool,
) -> Result<String> {
    // Check if we already have this calendar
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM calendars WHERE account_id = ?1 AND remote_id = ?2",
            params![account_id, remote_id],
            |row| row.get(0),
        )
        .ok();

    if let Some(id) = existing {
        conn.execute(
            "UPDATE calendars SET name = ?1, color = ?2, is_default = ?3 WHERE id = ?4",
            params![name, color, is_default, id],
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
                    ical_data = ?15, remote_id = ?16, etag = ?17,
                    updated_at = CURRENT_TIMESTAMP
                 WHERE id = ?18",
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
    let (query, do_calendar_filter) = if calendar_id.is_some() {
        (
            "SELECT id, account_id, calendar_id, uid, title, description, location,
                    start_time, end_time, all_day, timezone, recurrence_rule,
                    organizer_email, attendees_json, my_status, source_message_id,
                    ical_data, remote_id, etag
             FROM calendar_events
             WHERE account_id = ?1 AND calendar_id = ?2
               AND start_time < ?4 AND end_time > ?3
             ORDER BY start_time ASC",
            true,
        )
    } else {
        (
            "SELECT id, account_id, calendar_id, uid, title, description, location,
                    start_time, end_time, all_day, timezone, recurrence_rule,
                    organizer_email, attendees_json, my_status, source_message_id,
                    ical_data, remote_id, etag
             FROM calendar_events
             WHERE account_id = ?1
               AND start_time < ?3 AND end_time > ?2
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

pub fn get_event(conn: &Connection, id: &str) -> Result<CalendarEvent> {
    conn.query_row(
        "SELECT id, account_id, calendar_id, uid, title, description, location,
                start_time, end_time, all_day, timezone, recurrence_rule,
                organizer_email, attendees_json, my_status, source_message_id,
                ical_data, remote_id, etag
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
          ical_data, remote_id, etag)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
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
            ical_data = ?15, remote_id = ?16, etag = ?17,
            updated_at = CURRENT_TIMESTAMP
         WHERE id = ?18",
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
    conn.execute(
        "DELETE FROM calendar_events WHERE id = ?1",
        params![id],
    )?;
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
                ical_data, remote_id, etag
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
    })
}
