//! Calendar backends: one implementor per calendar protocol.
//!
//! ## Adding a provider
//!
//! 1. Put the transport client in `mail/` (e.g. `mail/google.rs`).
//! 2. Implement [`CalendarBackend`] for a unit struct in a new module
//!    here, moving the provider's push/error semantics into the impl
//!    (they differ deliberately — see each method's contract).
//! 3. Add the struct to [`registry`].
//!
//! Push methods are best-effort at the command layer: the local DB
//! write always lands, a failed push is logged. `sync` errors
//! propagate to the `calendar-sync-error` event.

use async_trait::async_trait;

use crate::db::accounts::AccountFull;
use crate::db::calendar::CalendarEvent;
use crate::db::pool::DbPool;
use crate::error::Result;

pub mod caldav;
pub mod google;
pub mod graph;
pub mod jmap;

/// Server identifiers returned by a successful event push.
pub struct PushedEvent {
    /// Provider-side event id; persisted as the local row's remote_id.
    pub remote_id: String,
    /// Set when the server rewrites the event UID (Google iCalUID,
    /// Exchange iCalUid). Persisted as the local UID so incoming RSVP
    /// replies match back to the event.
    pub canonical_uid: Option<String>,
}

#[async_trait]
pub trait CalendarBackend: Send + Sync {
    /// Protocol discriminator stored on the calendar service binding
    /// (`service_bindings.protocol`).
    fn protocol(&self) -> &'static str;

    /// Full account calendar sync: fetch remote calendars/events and
    /// reconcile the local DB. Interleaves provider I/O with DB writes
    /// (calendar/event upserts, deletion reconciliation, pushing
    /// locally created events), so it takes the pool.
    async fn sync(&self, db: &DbPool, account: &AccountFull) -> Result<()>;

    /// Push a newly created local event. `Ok(None)` means the provider
    /// defers the push (CalDAV events go out with the next sync's
    /// unpushed-rows pass). `remote_calendar_id` is the local
    /// calendar's remote handle; providers that only write to the
    /// default calendar ignore it.
    async fn push_created_event(
        &self,
        account: &AccountFull,
        event: &CalendarEvent,
        remote_calendar_id: &str,
    ) -> Result<Option<PushedEvent>>;

    /// Push field updates for an event. Default no-op: JMAP and CalDAV
    /// do not push event updates today (ADR 0050) — preserving that is
    /// deliberate; their updates reach the server via other paths.
    async fn push_updated_event(
        &self,
        _account: &AccountFull,
        _remote_id: &str,
        _event: &CalendarEvent,
    ) -> Result<()> {
        Ok(())
    }

    /// Delete an event on the server.
    async fn push_deleted_event(
        &self,
        account: &AccountFull,
        remote_id: &str,
        remote_calendar_id: &str,
    ) -> Result<()>;

    /// Push a calendar rename. Errors propagate — the caller leaves
    /// the local DB unchanged on remote failure.
    async fn push_calendar_rename(
        &self,
        account: &AccountFull,
        remote_id: &str,
        name: &str,
    ) -> Result<()>;

    /// Push a calendar color change. CalDAV and JMAP propagate
    /// failures; Graph and Google swallow them internally (system /
    /// shared calendars reject color writes with generic errors, and
    /// the local pick should stick regardless).
    async fn push_calendar_color(
        &self,
        account: &AccountFull,
        remote_id: &str,
        color: &str,
    ) -> Result<()>;
}

/// Static set of calendar backends compiled into this build. Adding a
/// provider = a new line here.
pub fn registry() -> &'static [&'static dyn CalendarBackend] {
    &[
        &jmap::JmapCalendarBackend,
        &google::GoogleCalendarBackend,
        &graph::GraphCalendarBackend,
        &caldav::CalDavCalendarBackend,
    ]
}

/// Find the backend for the account's enabled calendar binding.
///
/// Falls back to CalDAV for accounts with a configured `caldav_url`
/// but no matching protocol — pre-binding accounts and generic IMAP
/// accounts with DAV extras have always synced through that path.
pub fn for_account(account: &AccountFull) -> Option<&'static dyn CalendarBackend> {
    let proto = account.calendar_protocol_str();
    if let Some(backend) = registry().iter().copied().find(|b| b.protocol() == proto) {
        return Some(backend);
    }
    if !account.caldav_url.is_empty() {
        return Some(&caldav::CalDavCalendarBackend);
    }
    None
}

/// Local events that have never been pushed (no remote_id). Shared by
/// the JMAP and CalDAV syncs' push pass.
pub(crate) fn get_unpushed_events(
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

#[cfg(test)]
mod registry_tests {
    use super::*;
    use crate::db::service_bindings::ServiceBinding;

    fn account(calendar_protocol: &str, caldav_url: &str) -> AccountFull {
        let bindings = if calendar_protocol.is_empty() {
            Vec::new()
        } else {
            vec![ServiceBinding {
                id: "b1".into(),
                account_id: "acc1".into(),
                service: "calendar".into(),
                protocol: calendar_protocol.into(),
                enabled: true,
                sync_interval_seconds: None,
                config_json: "{}".into(),
            }]
        };
        AccountFull {
            id: "acc1".into(),
            display_name: "Test".into(),
            email: "u@example.com".into(),
            provider: "generic".into(),
            mail_protocol: String::new(),
            imap_host: String::new(),
            imap_port: 0,
            smtp_host: String::new(),
            smtp_port: 0,
            jmap_url: String::new(),
            caldav_url: caldav_url.into(),
            meet_url: String::new(),
            meet_protocol: String::new(),
            username: "u@example.com".into(),
            password: String::new(),
            use_tls: true,
            enabled: true,
            signature: String::new(),
            jmap_auth_method: String::new(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
            calendar_sync_enabled: true,
            auth_method: String::new(),
            bindings,
            mail_sync_enabled: true,
            contacts_sync_enabled: false,
            mail_sync_interval_seconds: None,
            calendar_sync_interval_seconds: None,
            contacts_sync_interval_seconds: None,
            pgp_attach_pubkey_on_sign: false,
            pgp_autocrypt_header: false,
            pgp_encrypt_subject: false,
            pgp_encrypt_drafts: false,
        }
    }

    #[test]
    fn protocols_resolve_to_matching_backends() {
        for proto in ["jmap", "google", "graph", "caldav"] {
            let b = for_account(&account(proto, "")).expect(proto);
            assert_eq!(b.protocol(), proto);
        }
    }

    #[test]
    fn caldav_url_fallback_without_binding() {
        let b = for_account(&account("", "https://dav.example.org")).unwrap();
        assert_eq!(b.protocol(), "caldav");
    }

    #[test]
    fn unknown_protocol_falls_back_to_caldav_only_with_url() {
        let b = for_account(&account("gopher", "https://dav.example.org")).unwrap();
        assert_eq!(b.protocol(), "caldav");
        assert!(for_account(&account("gopher", "")).is_none());
    }

    #[test]
    fn no_binding_no_caldav_url_is_none() {
        assert!(for_account(&account("", "")).is_none());
    }
}
