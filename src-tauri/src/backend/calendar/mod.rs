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

/// Explicit result for optional provider capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarCapability<T> {
    Supported(T),
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RoomSuggestion {
    pub name: String,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RoomAvailability {
    pub state: String,
    pub busy_start: Option<String>,
    pub busy_end: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ParticipantSchedule {
    pub email: String,
    pub available: bool,
    pub busy: Vec<BusyPeriod>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BusyPeriod {
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomAvailabilityRequest {
    pub room_address: String,
    pub start_time: String,
    pub end_time: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantScheduleRequest {
    pub emails: Vec<String>,
    pub start_time: String,
    pub end_time: String,
}

/// Valid responses accepted from the calendar RSVP IPC boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteResponse {
    Accepted,
    Tentative,
    Declined,
}

impl InviteResponse {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Tentative => "tentative",
            Self::Declined => "declined",
        }
    }
}

impl TryFrom<&str> for InviteResponse {
    type Error = crate::error::Error;

    fn try_from(value: &str) -> std::result::Result<Self, Self::Error> {
        match value.trim().to_ascii_lowercase().as_str() {
            "accepted" => Ok(Self::Accepted),
            "tentative" => Ok(Self::Tentative),
            "declined" => Ok(Self::Declined),
            _ => Err(crate::error::Error::Other(format!(
                "Unsupported invite response: {}",
                value
            ))),
        }
    }
}

/// Provider-neutral event data needed by remote RSVP implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRsvpRequest {
    pub uid: String,
    pub response: InviteResponse,
    pub summary: Option<String>,
    pub start_time: String,
    pub end_time: String,
    pub all_day: bool,
    pub description: Option<String>,
    pub location: Option<String>,
    pub organizer_email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRsvpOutcome {
    pub remote_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttendeeResponseUpdate {
    pub remote_id: String,
    pub attendee_email: String,
    pub response: String,
}

/// How command-owned iTIP replies are delivered for this provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteReplyDelivery {
    Smtp,
    JmapSubmission,
    Provider,
}

/// Where a provider's remote RSVP belongs in command-owned orchestration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteRsvpPolicy {
    Unsupported,
    RequiredBeforeLocal,
    BestEffortAfterLocal,
}

#[async_trait]
pub trait CalendarBackend: Send + Sync {
    /// Protocol discriminator stored on the calendar service binding
    /// (`service_bindings.protocol`).
    fn protocol(&self) -> &'static str;

    /// How the command should deliver the generated iTIP reply.
    fn invite_reply_delivery(&self) -> InviteReplyDelivery {
        InviteReplyDelivery::Smtp
    }

    /// When the command should invoke this provider's remote RSVP call.
    fn remote_rsvp_policy(&self) -> RemoteRsvpPolicy {
        RemoteRsvpPolicy::Unsupported
    }

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

    async fn list_room_suggestions(
        &self,
        _account: &AccountFull,
    ) -> Result<CalendarCapability<Vec<RoomSuggestion>>> {
        Ok(CalendarCapability::Unsupported)
    }

    async fn check_room_availability(
        &self,
        _account: &AccountFull,
        _request: &RoomAvailabilityRequest,
    ) -> Result<CalendarCapability<RoomAvailability>> {
        Ok(CalendarCapability::Unsupported)
    }

    async fn get_participant_schedules(
        &self,
        _account: &AccountFull,
        _request: &ParticipantScheduleRequest,
    ) -> Result<CalendarCapability<Vec<ParticipantSchedule>>> {
        Ok(CalendarCapability::Unsupported)
    }

    async fn apply_remote_rsvp(
        &self,
        _account: &AccountFull,
        _request: &RemoteRsvpRequest,
    ) -> Result<CalendarCapability<RemoteRsvpOutcome>> {
        Ok(CalendarCapability::Unsupported)
    }

    async fn push_attendee_responses(
        &self,
        _account: &AccountFull,
        _updates: &[AttendeeResponseUpdate],
    ) -> Result<CalendarCapability<()>> {
        Ok(CalendarCapability::Unsupported)
    }
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

    fn account(calendar_protocol: &str, caldav_url: &str) -> AccountFull {
        let mut account = crate::backend::testutil::account("calendar", calendar_protocol);
        account.caldav_url = caldav_url.into();
        account
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

    #[test]
    fn invite_reply_delivery_matches_provider_semantics() {
        let cases = [
            ("caldav", InviteReplyDelivery::Smtp),
            ("google", InviteReplyDelivery::Smtp),
            ("jmap", InviteReplyDelivery::JmapSubmission),
            ("graph", InviteReplyDelivery::Provider),
        ];

        for (protocol, expected) in cases {
            let backend = for_account(&account(protocol, "")).unwrap();
            assert_eq!(backend.invite_reply_delivery(), expected);
        }
    }

    #[test]
    fn remote_rsvp_policy_matches_callable_provider_methods() {
        let cases = [
            ("caldav", RemoteRsvpPolicy::Unsupported),
            ("jmap", RemoteRsvpPolicy::Unsupported),
            ("google", RemoteRsvpPolicy::BestEffortAfterLocal),
            ("graph", RemoteRsvpPolicy::RequiredBeforeLocal),
        ];
        for (protocol, expected) in cases {
            let backend = for_account(&account(protocol, "")).unwrap();
            assert_eq!(backend.remote_rsvp_policy(), expected);
        }
    }

    #[test]
    fn invite_response_parsing_is_case_insensitive_and_rejects_unknown_values() {
        assert_eq!(
            InviteResponse::try_from(" ACCEPTED ").unwrap(),
            InviteResponse::Accepted
        );
        assert_eq!(
            InviteResponse::try_from("Tentative").unwrap(),
            InviteResponse::Tentative
        );
        assert_eq!(
            InviteResponse::try_from("declined").unwrap(),
            InviteResponse::Declined
        );
        assert!(InviteResponse::try_from("maybe").is_err());
    }
}

/// Per-provider semantics ADR 0050 calls load-bearing. The fixture
/// account has no credentials or server URLs, so any I/O attempt fails
/// before the network — which is exactly what these tests lean on:
/// deferred/no-op paths must succeed without I/O, swallowing backends
/// must turn the failure into `Ok`, propagating backends into `Err`.
#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::backend::testutil::{account, event, temp_pool};

    /// CalDAV never pushes at create time — events go out with the
    /// next sync's unpushed-rows pass.
    #[tokio::test]
    async fn caldav_defers_event_creation_to_sync() {
        let pushed = caldav::CalDavCalendarBackend
            .push_created_event(&account("calendar", "caldav"), &event(), "cal-href")
            .await
            .unwrap();
        assert!(pushed.is_none());
    }

    /// JMAP and CalDAV do not push event updates (trait default
    /// no-op). An override that starts pushing would hit the missing
    /// server config and fail this test.
    #[tokio::test]
    async fn jmap_and_caldav_do_not_push_event_updates() {
        jmap::JmapCalendarBackend
            .push_updated_event(&account("calendar", "jmap"), "r1", &event())
            .await
            .unwrap();
        caldav::CalDavCalendarBackend
            .push_updated_event(&account("calendar", "caldav"), "r1", &event())
            .await
            .unwrap();
    }

    /// Google color pushes swallow a missing OAuth token — the local
    /// pick sticks.
    #[tokio::test]
    async fn google_color_push_swallows_missing_token() {
        google::GoogleCalendarBackend
            .push_calendar_color(&account("calendar", "google"), "r1", "#a1b2c3")
            .await
            .unwrap();
    }

    /// Graph color pushes swallow only Graph API errors; a missing
    /// token propagates (pre-trait behaviour, kept verbatim).
    #[tokio::test]
    async fn graph_color_push_propagates_missing_token() {
        let result = graph::GraphCalendarBackend
            .push_calendar_color(&account("calendar", "graph"), "r1", "#a1b2c3")
            .await;
        assert!(result.is_err());
    }

    /// Google sync falls back to CalDAV only when `caldav_url` is
    /// set; without one the failure propagates.
    #[tokio::test]
    async fn google_sync_without_caldav_fallback_propagates() {
        let (_dir, db) = temp_pool();
        let result = google::GoogleCalendarBackend
            .sync(&db, &account("calendar", "google"))
            .await;
        assert!(result.is_err());
    }

    /// Graph calendar sync propagates credential failures.
    #[tokio::test]
    async fn graph_sync_propagates_missing_token() {
        let (_dir, db) = temp_pool();
        let result = graph::GraphCalendarBackend
            .sync(&db, &account("calendar", "graph"))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn unsupported_scheduling_capabilities_do_not_attempt_io() {
        let caldav = account("calendar", "caldav");
        let jmap = account("calendar", "jmap");
        let room_request = RoomAvailabilityRequest {
            room_address: "room@example.com".into(),
            start_time: "2026-08-10T09:00:00Z".into(),
            end_time: "2026-08-10T10:00:00Z".into(),
        };
        let schedule_request = ParticipantScheduleRequest {
            emails: vec!["person@example.com".into()],
            start_time: room_request.start_time.clone(),
            end_time: room_request.end_time.clone(),
        };

        assert_eq!(
            caldav::CalDavCalendarBackend
                .list_room_suggestions(&caldav)
                .await
                .unwrap(),
            CalendarCapability::Unsupported
        );
        assert_eq!(
            jmap::JmapCalendarBackend
                .check_room_availability(&jmap, &room_request)
                .await
                .unwrap(),
            CalendarCapability::Unsupported
        );
        assert_eq!(
            jmap::JmapCalendarBackend
                .get_participant_schedules(&jmap, &schedule_request)
                .await
                .unwrap(),
            CalendarCapability::Unsupported
        );
    }

    #[tokio::test]
    async fn supported_scheduling_capabilities_attempt_provider_auth() {
        let graph = account("calendar", "graph");
        let google = account("calendar", "google");
        let request = ParticipantScheduleRequest {
            emails: vec!["person@example.com".into()],
            start_time: "2026-08-10T09:00:00Z".into(),
            end_time: "2026-08-10T10:00:00Z".into(),
        };

        assert!(graph::GraphCalendarBackend
            .get_participant_schedules(&graph, &request)
            .await
            .is_err());
        assert!(google::GoogleCalendarBackend
            .get_participant_schedules(&google, &request)
            .await
            .is_err());
        assert!(graph::GraphCalendarBackend
            .list_room_suggestions(&graph)
            .await
            .is_err());
        let room_request = RoomAvailabilityRequest {
            room_address: "room@example.com".into(),
            start_time: request.start_time.clone(),
            end_time: request.end_time.clone(),
        };
        assert!(graph::GraphCalendarBackend
            .check_room_availability(&graph, &room_request)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn remote_rsvp_capabilities_match_their_policies() {
        let graph = account("calendar", "graph");
        let google = account("calendar", "google");
        let caldav = account("calendar", "caldav");
        let request = RemoteRsvpRequest {
            uid: "event@example.com".into(),
            response: InviteResponse::Accepted,
            summary: Some("Planning".into()),
            start_time: "2026-08-10T09:00:00Z".into(),
            end_time: "2026-08-10T10:00:00Z".into(),
            all_day: false,
            description: None,
            location: None,
            organizer_email: Some("organizer@example.com".into()),
        };

        assert!(graph::GraphCalendarBackend
            .apply_remote_rsvp(&graph, &request)
            .await
            .is_err());
        assert!(google::GoogleCalendarBackend
            .apply_remote_rsvp(&google, &request)
            .await
            .is_err());
        assert_eq!(
            caldav::CalDavCalendarBackend
                .apply_remote_rsvp(&caldav, &request)
                .await
                .unwrap(),
            CalendarCapability::Unsupported
        );
    }

    #[tokio::test]
    async fn only_jmap_handles_remote_attendee_responses() {
        let update = AttendeeResponseUpdate {
            remote_id: "event-1".into(),
            attendee_email: "person@example.com".into(),
            response: "accepted".into(),
        };
        let jmap = account("calendar", "jmap");
        let graph = account("calendar", "graph");

        assert_eq!(
            jmap::JmapCalendarBackend
                .push_attendee_responses(&jmap, std::slice::from_ref(&update))
                .await
                .unwrap(),
            CalendarCapability::Supported(())
        );
        assert_eq!(
            graph::GraphCalendarBackend
                .push_attendee_responses(&graph, &[update])
                .await
                .unwrap(),
            CalendarCapability::Unsupported
        );
    }
}
