//! Scheduling provenance for events created from invitation email.

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvitationSource {
    pub source_account_id: String,
    pub source_message_id: String,
    pub invitation_uid: String,
}

pub fn record(conn: &Connection, event_id: &str, source: &InvitationSource) -> Result<()> {
    conn.execute(
        "INSERT INTO calendar_invitation_sources
         (event_id, source_account_id, source_message_id, invitation_uid)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(event_id) DO UPDATE SET
             source_account_id = excluded.source_account_id,
             source_message_id = excluded.source_message_id,
             invitation_uid = excluded.invitation_uid",
        params![
            event_id,
            source.source_account_id,
            source.source_message_id,
            source.invitation_uid
        ],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, event_id: &str) -> Result<Option<InvitationSource>> {
    conn.query_row(
        "SELECT source_account_id, source_message_id, invitation_uid
         FROM calendar_invitation_sources WHERE event_id = ?1",
        [event_id],
        |row| {
            Ok(InvitationSource {
                source_account_id: row.get(0)?,
                source_message_id: row.get(1)?,
                invitation_uid: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

#[cfg(test)]
pub fn response_status(
    conn: &Connection,
    source_account_id: &str,
    invitation_uid: &str,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT event.my_status
         FROM calendar_invitation_sources source
         JOIN calendar_events event ON event.id = source.event_id
         WHERE source.source_account_id = ?1 AND source.invitation_uid = ?2
         ORDER BY event.updated_at DESC LIMIT 1",
        params![source_account_id, invitation_uid],
        |row| row.get::<_, Option<String>>(0),
    )
    .optional()
    .map(Option::flatten)
    .map_err(Into::into)
}

pub fn event_id(
    conn: &Connection,
    source_account_id: &str,
    invitation_uid: &str,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT event_id FROM calendar_invitation_sources
         WHERE source_account_id = ?1 AND invitation_uid = ?2
         LIMIT 1",
        params![source_account_id, invitation_uid],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}
