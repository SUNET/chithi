//! Storage for the event ↔ meet-provider meeting link (#148).
//!
//! When the user clicks "Add Zoom" / "Add Talk" / "Add Matrix" on
//! an event we create a remote meeting and record a row here so
//! later updates (reschedule) and deletes (cancel) can act on the
//! same remote object instead of leaving it orphaned. One binding
//! per event; if the user replaces the meet link, the previous
//! binding is overwritten.

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct MeetMeeting {
    pub event_id: String,
    pub account_id: String,
    pub protocol: String,
    pub meeting_id: String,
    pub join_url: String,
}

/// Replace any existing binding for `event_id` with the new one.
/// `INSERT OR REPLACE` so the user can swap out the Zoom link
/// without us leaking stale rows.
pub fn upsert(conn: &Connection, m: &MeetMeeting) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meet_meetings
            (event_id, account_id, protocol, meeting_id, join_url)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            m.event_id,
            m.account_id,
            m.protocol,
            m.meeting_id,
            m.join_url,
        ],
    )?;
    Ok(())
}

/// Read the binding for an event, if any.
pub fn get(conn: &Connection, event_id: &str) -> Result<Option<MeetMeeting>> {
    let row = conn
        .query_row(
            "SELECT event_id, account_id, protocol, meeting_id, join_url
             FROM meet_meetings
             WHERE event_id = ?1",
            params![event_id],
            |r| {
                Ok(MeetMeeting {
                    event_id: r.get(0)?,
                    account_id: r.get(1)?,
                    protocol: r.get(2)?,
                    meeting_id: r.get(3)?,
                    join_url: r.get(4)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Drop the binding row. Used after the provider DELETE succeeds —
/// the CASCADE on `calendar_events` deletion would catch it anyway,
/// but `delete_event` looks the row up *before* removing the event,
/// so it has to clear the side-table itself.
pub fn delete(conn: &Connection, event_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM meet_meetings WHERE event_id = ?1",
        params![event_id],
    )?;
    Ok(())
}
