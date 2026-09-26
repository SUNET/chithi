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
//! The command layer owns iTIP composition, local persistence, event emission,
//! and cross-provider ordering. Provider network operations and their explicit
//! unsupported outcomes live behind [`CalendarBackend`].

use async_trait::async_trait;

use crate::calendar::event_set::CalendarEventSet;
use crate::calendar::recurrence_identity::{
    OccurrenceFields, RecurrenceIdentity, RecurrenceIdentitySeed, UpdateOccurrenceInput,
};
use crate::calendar::CalendarEvent;
use crate::db::accounts::AccountFull;
use crate::db::pool::DbPool;
use crate::error::Result;
use crate::provider::ProviderServices;

pub mod caldav;
pub mod google;
pub mod graph;
pub mod jmap;

pub struct CalendarBackendCtx<'a> {
    pub db: &'a DbPool,
    pub services: &'a ProviderServices,
}

/// Server identifiers returned by a successful event push.
pub struct PushedEvent {
    /// Provider-side event id; persisted as the local row's remote_id.
    pub remote_id: String,
    /// Set when the server rewrites the event UID (Google iCalUID,
    /// Exchange iCalUid). Persisted as the local UID so incoming RSVP
    /// replies match back to the event.
    pub canonical_uid: Option<String>,
    /// Provider revision returned with creation, when available.
    pub etag: Option<String>,
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
    pub attendees: Vec<crate::calendar::Attendee>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRsvpOutcome {
    pub remote_id: Option<String>,
}

/// Trusted provider-neutral input for one occurrence update. This is an
/// internal contract and intentionally does not implement `Serialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteOccurrenceUpdate {
    pub target_id: String,
    pub expected_provider_revision: Option<String>,
    /// Immutable identity, including the provider-calendar scope.
    pub trusted_identity: RecurrenceIdentity,
    pub current_event: CalendarEvent,
    /// Original sparse user patch. Providers must emit only these properties.
    pub patch: UpdateOccurrenceInput,
    pub desired: OccurrenceFields,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteOccurrenceUpdateOutcome {
    pub replacement_identity: RecurrenceIdentitySeed,
    pub occurrence: OccurrenceFields,
    pub canonical_event: Option<CalendarEvent>,
    /// Complete nonempty master/override/exclusion set parsed from an embedded
    /// resource. Detached resources must return `None`.
    pub canonical_recurrence_objects: Option<Vec<RecurrenceIdentitySeed>>,
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

/// How precisely a provider can honor a user-selected creation destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventCreationTarget {
    /// Creation uses the selected calendar's remote identifier.
    SelectedCalendar,
    /// The provider API currently creates only on the account default.
    AccountDefault,
}

/// Recurrence fidelity available when importing source-backed iCalendar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecurringImportFidelity {
    Unsupported,
    PatternedRecurrence,
    RawIcalendar,
}

#[async_trait]
pub trait CalendarBackend: Send + Sync {
    /// Protocol discriminator stored on the calendar service binding
    /// (`service_bindings.protocol`).
    fn protocol(&self) -> &'static str;

    /// Read the authoritative standalone event or complete series containing
    /// `event`, including modified/cancelled occurrences outside any view window.
    /// Native data is private and must not be projected directly into IPC.
    async fn fetch_event_set(
        &self,
        _ctx: &CalendarBackendCtx<'_>,
        _account: &AccountFull,
        _event: &CalendarEvent,
        _remote_calendar_id: &str,
    ) -> Result<CalendarEventSet> {
        Err(crate::error::Error::UnsupportedCapability {
            protocol: self.protocol(),
            capability: "calendar event-set read",
        })
    }

    /// Apply only the semantic differences between two complete snapshots,
    /// using the original native revisions, then return canonical provider data.
    /// Retained native fields must not be replaced by a reconstructed DTO.
    async fn update_event_set(
        &self,
        _ctx: &CalendarBackendCtx<'_>,
        _account: &AccountFull,
        _before: &CalendarEventSet,
        _desired: &CalendarEventSet,
    ) -> Result<CalendarEventSet> {
        Err(crate::error::Error::UnsupportedCapability {
            protocol: self.protocol(),
            capability: "calendar event-set update",
        })
    }

    /// Create in the exact selected calendar. `operation_id` is a persisted
    /// idempotency identity; retry must reconcile, not blindly create a duplicate.
    /// Source provider identifiers in `desired` are never destination targets.
    async fn create_event_set(
        &self,
        _ctx: &CalendarBackendCtx<'_>,
        _account: &AccountFull,
        _remote_calendar_id: &str,
        _desired: &CalendarEventSet,
        _operation_id: &str,
    ) -> Result<CalendarEventSet> {
        Err(crate::error::Error::UnsupportedCapability {
            protocol: self.protocol(),
            capability: "calendar event-set creation",
        })
    }

    /// Conditionally remove the source of a verified transfer. This is not a
    /// renderer-facing recurring-delete capability.
    async fn delete_event_set(
        &self,
        _ctx: &CalendarBackendCtx<'_>,
        _account: &AccountFull,
        _before: &CalendarEventSet,
    ) -> Result<()> {
        Err(crate::error::Error::UnsupportedCapability {
            protocol: self.protocol(),
            capability: "calendar transfer source removal",
        })
    }

    /// A native calendar move, when available under this account's credentials.
    /// Unsupported must have no remote side effects. Ambiguous transport failures
    /// must be errors, never an invitation to fall back to copy/delete.
    async fn move_event_set_native(
        &self,
        _ctx: &CalendarBackendCtx<'_>,
        _account: &AccountFull,
        _before: &CalendarEventSet,
        _remote_calendar_id: &str,
    ) -> Result<CalendarCapability<CalendarEventSet>> {
        Ok(CalendarCapability::Unsupported)
    }

    /// How the command should deliver the generated iTIP reply.
    fn invite_reply_delivery(&self) -> InviteReplyDelivery {
        InviteReplyDelivery::Smtp
    }

    /// When the command should invoke this provider's remote RSVP call.
    fn remote_rsvp_policy(&self) -> RemoteRsvpPolicy {
        RemoteRsvpPolicy::Unsupported
    }

    fn event_creation_target(&self) -> EventCreationTarget {
        EventCreationTarget::SelectedCalendar
    }

    fn recurring_import_fidelity(&self) -> RecurringImportFidelity {
        RecurringImportFidelity::Unsupported
    }

    /// Full account calendar sync: fetch remote calendars/events and
    /// reconcile the local DB. Interleaves provider I/O with DB writes
    /// (calendar/event upserts, deletion reconciliation, pushing
    /// locally created events), so it takes the pool.
    async fn sync(&self, ctx: &CalendarBackendCtx<'_>, account: &AccountFull) -> Result<()>;

    /// Pure payload preflight, before local insertion or meeting ownership transfer.
    fn validate_event_creation(
        &self,
        event: &CalendarEvent,
        remote_calendar_id: &str,
    ) -> Result<()>;

    /// Push a newly created local event. `Ok(None)` means the provider defers
    /// the push. `remote_calendar_id` is the local calendar's remote handle;
    /// providers that only write to the default calendar ignore it.
    async fn push_created_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        event: &CalendarEvent,
        remote_calendar_id: &str,
    ) -> Result<Option<PushedEvent>>;

    /// Push field updates for an event. Providers without immediate ordinary
    /// update support inherit the no-op and reconcile on their next sync.
    async fn push_updated_event(
        &self,
        _ctx: &CalendarBackendCtx<'_>,
        _account: &AccountFull,
        _remote_id: &str,
        _event: &CalendarEvent,
    ) -> Result<()> {
        Ok(())
    }

    /// Push a refreshed personal invitation copy without scheduling guests.
    /// Returns the replacement provider revision when one is available.
    async fn push_updated_invitation_copy(
        &self,
        _ctx: &CalendarBackendCtx<'_>,
        _account: &AccountFull,
        _remote_id: &str,
        _event: &CalendarEvent,
    ) -> Result<Option<String>> {
        Err(crate::error::Error::UnsupportedCapability {
            protocol: self.protocol(),
            capability: "personal invitation copy update",
        })
    }

    /// Delete an event on the server.
    async fn push_deleted_event(
        &self,
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        remote_calendar_id: &str,
    ) -> Result<()>;

    /// Push a calendar rename. Errors propagate — the caller leaves
    /// the local DB unchanged on remote failure.
    async fn push_calendar_rename(
        &self,
        ctx: &CalendarBackendCtx<'_>,
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
        ctx: &CalendarBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
        color: &str,
    ) -> Result<()>;

    async fn list_room_suggestions(
        &self,
        _ctx: &CalendarBackendCtx<'_>,
        _account: &AccountFull,
    ) -> Result<CalendarCapability<Vec<RoomSuggestion>>> {
        Ok(CalendarCapability::Unsupported)
    }

    async fn check_room_availability(
        &self,
        _ctx: &CalendarBackendCtx<'_>,
        _account: &AccountFull,
        _request: &RoomAvailabilityRequest,
    ) -> Result<CalendarCapability<RoomAvailability>> {
        Ok(CalendarCapability::Unsupported)
    }

    async fn get_participant_schedules(
        &self,
        _ctx: &CalendarBackendCtx<'_>,
        _account: &AccountFull,
        _request: &ParticipantScheduleRequest,
    ) -> Result<CalendarCapability<Vec<ParticipantSchedule>>> {
        Ok(CalendarCapability::Unsupported)
    }

    async fn apply_remote_rsvp(
        &self,
        _ctx: &CalendarBackendCtx<'_>,
        _account: &AccountFull,
        _request: &RemoteRsvpRequest,
    ) -> Result<CalendarCapability<RemoteRsvpOutcome>> {
        Ok(CalendarCapability::Unsupported)
    }

    async fn update_recurrence_occurrence(
        &self,
        _ctx: &CalendarBackendCtx<'_>,
        _account: &AccountFull,
        _request: &RemoteOccurrenceUpdate,
    ) -> Result<RemoteOccurrenceUpdateOutcome> {
        Err(crate::error::Error::UnsupportedCapability {
            protocol: self.protocol(),
            capability: "THIS-OCCURRENCE update",
        })
    }

    async fn push_attendee_responses(
        &self,
        _ctx: &CalendarBackendCtx<'_>,
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
    if let Some(backend) = for_protocol(proto) {
        return Some(backend);
    }
    if !account.caldav_url.is_empty() {
        return Some(&caldav::CalDavCalendarBackend);
    }
    None
}

pub fn for_protocol(protocol: &str) -> Option<&'static dyn CalendarBackend> {
    registry()
        .iter()
        .copied()
        .find(|backend| backend.protocol() == protocol)
}

/// Local events that have never been pushed (no remote_id). Members of a
/// canonical provider set have no independent address and must not be uploaded.
/// Shared by the JMAP and CalDAV syncs' push pass.
pub(crate) fn get_unpushed_events(
    conn: &rusqlite::Connection,
    account_id: &str,
) -> Result<Vec<CalendarEvent>> {
    let mut stmt = conn.prepare(
        "SELECT event.id FROM calendar_events event
         WHERE event.account_id = ?1 AND (event.remote_id IS NULL OR event.remote_id = '')
           AND NOT EXISTS (SELECT 1 FROM calendar_action_members member
                           WHERE member.event_id = event.id
                             AND member.owner_event_id != event.id)",
    )?;
    let ids = stmt
        .query_map(rusqlite::params![account_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ids.iter()
        .map(|id| crate::db::calendar::get_event(conn, id))
        .collect()
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn deferred_create_loader_preserves_recurrence_classification() {
        use crate::calendar::RecurrenceKind;

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::schema::initialize(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('account', 'Test', 'test@example.com', 'test@example.com'),
                    ('other', 'Other', 'other@example.com', 'other@example.com');",
        )
        .unwrap();

        for kind in [
            RecurrenceKind::Unknown,
            RecurrenceKind::Standalone,
            RecurrenceKind::Series,
            RecurrenceKind::Occurrence,
        ] {
            let event = CalendarEvent {
                id: kind.as_str().into(),
                account_id: "account".into(),
                recurrence_kind: kind,
                recurrence_rule: (kind == RecurrenceKind::Series).then(|| "FREQ=WEEKLY".into()),
                remote_id: (kind == RecurrenceKind::Occurrence).then(String::new),
                ..crate::backend::testutil::event()
            };
            crate::db::calendar::insert_event(&conn, &event).unwrap();
        }
        for (id, account_id, remote_id) in [
            ("pushed", "account", Some("remote-id".into())),
            ("another-account", "other", None),
        ] {
            let event = CalendarEvent {
                id: id.into(),
                account_id: account_id.into(),
                remote_id,
                ..crate::backend::testutil::event()
            };
            crate::db::calendar::insert_event(&conn, &event).unwrap();
        }

        let events = get_unpushed_events(&conn, "account").unwrap();
        assert_eq!(events.len(), 4);
        for event in events {
            assert_eq!(event.id, event.recurrence_kind.as_str());
            assert_eq!(
                event.recurrence_rule.as_deref(),
                (event.recurrence_kind == RecurrenceKind::Series).then_some("FREQ=WEEKLY")
            );
        }
    }

    #[test]
    fn canonical_members_are_not_unpushed_events() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::schema::initialize(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('account', 'Test', 'test@example.com', 'test@example.com');
             INSERT INTO calendars (id, account_id, name)
             VALUES ('calendar', 'account', 'Calendar');",
        )
        .unwrap();
        for id in ["local", "detached", "master"] {
            let event = CalendarEvent {
                id: id.into(),
                account_id: "account".into(),
                calendar_id: "calendar".into(),
                remote_id: (id == "master").then(|| "master.ics".into()),
                ..crate::backend::testutil::event()
            };
            crate::db::calendar::insert_event(&conn, &event).unwrap();
        }
        conn.execute(
            "INSERT INTO calendar_action_members(event_id, owner_event_id)
             VALUES ('detached', 'master')",
            [],
        )
        .unwrap();
        let unpushed = get_unpushed_events(&conn, "account").unwrap();
        assert_eq!(unpushed.len(), 1);
        assert_eq!(unpushed[0].id, "local");
    }

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

/// Per-provider semantics ADR 0050 calls load-bearing. Localhost peers verify
/// immediate writes and remote failures; unconfigured accounts exercise each
/// provider's credential-error and unsupported-capability contracts.
#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::backend::testutil::{account, event, temp_pool};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn services() -> &'static ProviderServices {
        static SERVICES: std::sync::OnceLock<ProviderServices> = std::sync::OnceLock::new();
        SERVICES.get_or_init(|| ProviderServices::production().unwrap())
    }

    fn ctx(db: &DbPool) -> CalendarBackendCtx<'_> {
        CalendarBackendCtx {
            db,
            services: services(),
        }
    }

    #[tokio::test]
    async fn caldav_creation_is_synchronous_and_returns_remote_identity() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}/dav/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&chunk[..count]);
                let Some(headers_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..headers_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= headers_end + 4 + content_length {
                    break;
                }
            }
            socket
                .write_all(
                    b"HTTP/1.1 201 Created\r\nETag: \"created-etag\"\r\n\
                      Content-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        });

        let (_dir, db) = temp_pool();
        let mut services = google::sync_testutil::services("");
        services.transports.dav_http = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut caldav_account = account("calendar", "caldav");
        caldav_account.caldav_url = root;
        let created = CalendarEvent {
            uid: Some("contract-uid".into()),
            ..event()
        };
        let pushed = caldav::CalDavCalendarBackend
            .push_created_event(
                &CalendarBackendCtx {
                    db: &db,
                    services: &services,
                },
                &caldav_account,
                &created,
                "/calendar/",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pushed.remote_id, "/calendar/contract-uid.ics");
        assert_eq!(pushed.canonical_uid.as_deref(), Some("contract-uid"));
        assert_eq!(pushed.etag.as_deref(), Some("\"created-etag\""));
        let request = server.await.unwrap();
        assert!(request.starts_with("PUT /calendar/contract-uid.ics HTTP/1.1\r\n"));
    }

    async fn update_request(stream: &mut tokio::net::TcpStream) -> (String, String) {
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                return (
                    headers,
                    String::from_utf8(bytes[end + 4..end + 4 + length].to_vec()).unwrap(),
                );
            }
        }
    }

    /// Ordinary updates must reach the selected remote resource, use its current
    /// revision, and propagate both rejected writes and failed canonical reads.
    #[tokio::test]
    async fn jmap_and_caldav_push_event_updates_and_propagate_remote_errors() {
        use serde_json::{json, Value};

        for protocol in ["jmap", "caldav"] {
            for outcome in ["success", "conflict", "canonical-error"] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let root = format!("http://{}", listener.local_addr().unwrap());
                let base = root.clone();
                let server = tokio::spawn(async move {
                    let mut native = json!({
                        "@type": "Event", "id": "r1", "uid": "contract-uid",
                        "calendarIds": {"remote-calendar": true}, "title": "Standup",
                        "start": "2026-07-16T10:00:00", "duration": "PT30M",
                        "timeZone": "UTC", "showWithoutTime": false,
                        "x-native": "preserved"
                    });
                    let mut ical = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
                        PRODID:-//Contract//EN\r\nBEGIN:VEVENT\r\nUID:contract-uid\r\n\
                        DTSTAMP:20260701T100000Z\r\nDTSTART:20260716T100000Z\r\n\
                        DTEND:20260716T103000Z\r\nSUMMARY:Standup\r\n\
                        X-NATIVE:preserved\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
                        .to_string();
                    let mut steps = Vec::new();
                    if protocol == "jmap" {
                        steps.extend(["discovery", "discovery"]);
                    }
                    steps.extend(["get", "update"]);
                    if outcome != "conflict" {
                        steps.push("canonical");
                    }
                    for step in steps {
                        let (mut stream, _) = listener.accept().await.unwrap();
                        let (headers, body) = update_request(&mut stream).await;
                        let mut status = "200 OK";
                        let mut etag = "";
                        let response = if protocol == "jmap" {
                            if step == "discovery" {
                                assert!(headers.starts_with("GET /.well-known/jmap "));
                                json!({
                                    "apiUrl": format!("{base}/api"),
                                    "downloadUrl": format!("{base}/download"),
                                    "uploadUrl": format!("{base}/upload"),
                                    "primaryAccounts": {
                                        "urn:ietf:params:jmap:mail": "mail-account",
                                        "urn:ietf:params:jmap:calendars": "calendar-account"
                                    },
                                    "accounts": {
                                        "mail-account": {"accountCapabilities": {
                                            "urn:ietf:params:jmap:mail": {}
                                        }},
                                        "calendar-account": {"accountCapabilities": {
                                            "urn:ietf:params:jmap:calendars": {}
                                        }}
                                    }
                                })
                                .to_string()
                            } else {
                                assert!(headers.starts_with("POST /api "));
                                let request: Value = serde_json::from_str(&body).unwrap();
                                let calls = request["methodCalls"].as_array().unwrap();
                                assert_eq!(calls.len(), 1);
                                let call = &calls[0];
                                let args = &call[1];
                                assert_eq!(args["accountId"], "calendar-account");
                                let (method, result) = if step == "update" {
                                    assert_eq!(call[0], "CalendarEvent/set");
                                    assert_eq!(args["ifInState"], "data-0");
                                    assert_eq!(args["sendSchedulingMessages"], false);
                                    assert_eq!(args["create"], json!({}));
                                    assert_eq!(args["destroy"], json!([]));
                                    assert_eq!(args["update"], json!({"r1": {"title": "Updated"}}));
                                    if outcome == "conflict" {
                                        ("error", json!({"type": "stateMismatch"}))
                                    } else {
                                        native["title"] = args["update"]["r1"]["title"].clone();
                                        (
                                            "CalendarEvent/set",
                                            json!({
                                                "accountId": "calendar-account", "oldState": "data-0",
                                                "newState": "data-1", "updated": {"r1": null}
                                            }),
                                        )
                                    }
                                } else {
                                    assert_eq!(call[0], "CalendarEvent/get");
                                    assert_eq!(args["ids"], json!(["r1"]));
                                    (
                                        "CalendarEvent/get",
                                        json!({
                                            "accountId": "calendar-account",
                                            "state": if step == "get" { "data-0" } else { "data-1" },
                                            "list": [native], "notFound": []
                                        }),
                                    )
                                };
                                json!({"methodResponses": [[method, result, call[2]]],
                                    "sessionState": "session-not-data"})
                                .to_string()
                            }
                        } else if step == "update" {
                            assert!(headers.starts_with("PUT /calendar/r1.ics "));
                            assert!(headers.to_ascii_lowercase().contains("if-match: \"v1\""));
                            assert!(body.contains("SUMMARY:Updated\r\n"));
                            assert!(body.contains("UID:contract-uid\r\n"));
                            assert!(body.contains("X-NATIVE:preserved\r\n"));
                            if outcome == "conflict" {
                                status = "412 Precondition Failed";
                            } else {
                                status = "204 No Content";
                                ical = body;
                            }
                            String::new()
                        } else {
                            assert!(headers.starts_with("GET /calendar/r1.ics "));
                            etag = if step == "get" {
                                "ETag: \"v1\"\r\n"
                            } else {
                                "ETag: \"v2\"\r\n"
                            };
                            ical.clone()
                        };
                        if step == "canonical" && outcome == "canonical-error" {
                            status = "500 Internal Server Error";
                        }
                        stream.write_all(format!(
                            "HTTP/1.1 {status}\r\n{etag}Content-Length: {}\r\nConnection: close\r\n\r\n{response}",
                            response.len(),
                        ).as_bytes()).await.unwrap();
                    }
                });

                let (_dir, db) = temp_pool();
                let mut account = account("calendar", protocol);
                account.jmap_url = root.clone();
                account.jmap_auth_method = "basic".into();
                account.caldav_url = root;
                let (remote_calendar, remote_id) = if protocol == "jmap" {
                    ("remote-calendar", "r1")
                } else {
                    ("/calendar/", "/calendar/r1.ics")
                };
                let desired = CalendarEvent {
                    title: "Updated".into(),
                    uid: Some("contract-uid".into()),
                    remote_id: Some(remote_id.into()),
                    start_time: "2026-07-16T10:00:00Z".into(),
                    end_time: "2026-07-16T10:30:00Z".into(),
                    timezone: Some("UTC".into()),
                    ..event()
                };
                {
                    let conn = db.writer().await;
                    crate::db::schema::initialize(&conn).unwrap();
                    conn.execute(
                        "INSERT INTO accounts (id, display_name, email, username)
                         VALUES (?1, 'Test', 'u@example.com', 'u@example.com')",
                        [&account.id],
                    )
                    .unwrap();
                    conn.execute(
                        "INSERT INTO calendars (id, account_id, name, remote_id)
                         VALUES (?1, ?2, 'Selected calendar', ?3)",
                        rusqlite::params![desired.calendar_id, account.id, remote_calendar],
                    )
                    .unwrap();
                }
                let http = reqwest::Client::builder()
                    .no_proxy()
                    .timeout(std::time::Duration::from_secs(3))
                    .build()
                    .unwrap();
                let mut services = ProviderServices::production().unwrap();
                services.transports.jmap_discovery_http = http.clone();
                services.transports.jmap_api_http = http.clone();
                services.transports.dav_http = http;
                let result = for_protocol(protocol)
                    .unwrap()
                    .push_updated_event(
                        &CalendarBackendCtx {
                            db: &db,
                            services: &services,
                        },
                        &account,
                        remote_id,
                        &desired,
                    )
                    .await;
                tokio::time::timeout(std::time::Duration::from_secs(5), server)
                    .await
                    .expect("ordinary update must complete the expected remote request sequence")
                    .unwrap();
                if outcome == "success" {
                    result.unwrap();
                } else {
                    let error = result.unwrap_err().to_string();
                    let expected = match (protocol, outcome) {
                        (_, "canonical-error") => "500",
                        ("jmap", _) => "stateMismatch",
                        _ => "412",
                    };
                    assert!(error.contains(expected), "{protocol}, {outcome}: {error}");
                }
            }
        }
    }

    /// Google color pushes swallow a missing OAuth token — the local
    /// pick sticks.
    #[tokio::test]
    async fn google_color_push_swallows_missing_token() {
        let (_dir, db) = temp_pool();
        google::GoogleCalendarBackend
            .push_calendar_color(&ctx(&db), &account("calendar", "google"), "r1", "#a1b2c3")
            .await
            .unwrap();
    }

    /// Graph color pushes swallow only Graph API errors; a missing
    /// token propagates (pre-trait behaviour, kept verbatim).
    #[tokio::test]
    async fn graph_color_push_propagates_missing_token() {
        let (_dir, db) = temp_pool();
        let result = graph::GraphCalendarBackend
            .push_calendar_color(&ctx(&db), &account("calendar", "graph"), "r1", "#a1b2c3")
            .await;
        assert!(result.is_err());
    }

    /// Google sync falls back to CalDAV only when `caldav_url` is
    /// set; without one the failure propagates.
    #[tokio::test]
    async fn google_sync_without_caldav_fallback_propagates() {
        let (_dir, db) = temp_pool();
        let result = google::GoogleCalendarBackend
            .sync(&ctx(&db), &account("calendar", "google"))
            .await;
        assert!(result.is_err());
    }

    /// Graph calendar sync propagates credential failures.
    #[tokio::test]
    async fn graph_sync_propagates_missing_token() {
        let (_dir, db) = temp_pool();
        let result = graph::GraphCalendarBackend
            .sync(&ctx(&db), &account("calendar", "graph"))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn unsupported_scheduling_capabilities_do_not_attempt_io() {
        let (_dir, db) = temp_pool();
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
                .list_room_suggestions(&ctx(&db), &caldav)
                .await
                .unwrap(),
            CalendarCapability::Unsupported
        );
        assert_eq!(
            jmap::JmapCalendarBackend
                .check_room_availability(&ctx(&db), &jmap, &room_request)
                .await
                .unwrap(),
            CalendarCapability::Unsupported
        );
        assert_eq!(
            jmap::JmapCalendarBackend
                .get_participant_schedules(&ctx(&db), &jmap, &schedule_request)
                .await
                .unwrap(),
            CalendarCapability::Unsupported
        );
    }

    #[tokio::test]
    async fn scheduling_capabilities_apply_provider_auth_policy() {
        let (_dir, db) = temp_pool();
        let graph = account("calendar", "graph");
        let google = account("calendar", "google");
        let request = ParticipantScheduleRequest {
            emails: vec!["person@example.com".into()],
            start_time: "2026-08-10T09:00:00Z".into(),
            end_time: "2026-08-10T10:00:00Z".into(),
        };

        assert!(graph::GraphCalendarBackend
            .get_participant_schedules(&ctx(&db), &graph, &request)
            .await
            .is_err());
        assert!(google::GoogleCalendarBackend
            .get_participant_schedules(&ctx(&db), &google, &request)
            .await
            .is_err());
        assert_eq!(
            graph::GraphCalendarBackend
                .list_room_suggestions(&ctx(&db), &graph)
                .await
                .unwrap(),
            CalendarCapability::Supported(Vec::new())
        );
        let room_request = RoomAvailabilityRequest {
            room_address: "room@example.com".into(),
            start_time: request.start_time.clone(),
            end_time: request.end_time.clone(),
        };
        assert_eq!(
            graph::GraphCalendarBackend
                .check_room_availability(&ctx(&db), &graph, &room_request)
                .await
                .unwrap(),
            CalendarCapability::Supported(RoomAvailability {
                state: "unknown".into(),
                busy_start: None,
                busy_end: None,
            })
        );
    }

    #[tokio::test]
    async fn remote_rsvp_capabilities_match_their_policies() {
        let (_dir, db) = temp_pool();
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
            attendees: Vec::new(),
        };

        assert!(graph::GraphCalendarBackend
            .apply_remote_rsvp(&ctx(&db), &graph, &request)
            .await
            .is_err());
        assert!(google::GoogleCalendarBackend
            .apply_remote_rsvp(&ctx(&db), &google, &request)
            .await
            .is_err());
        assert_eq!(
            caldav::CalDavCalendarBackend
                .apply_remote_rsvp(&ctx(&db), &caldav, &request)
                .await
                .unwrap(),
            CalendarCapability::Unsupported
        );
    }

    #[tokio::test]
    async fn only_jmap_handles_remote_attendee_responses() {
        let (_dir, db) = temp_pool();
        let update = AttendeeResponseUpdate {
            remote_id: "event-1".into(),
            attendee_email: "person@example.com".into(),
            response: "accepted".into(),
        };
        let jmap = account("calendar", "jmap");
        let graph = account("calendar", "graph");

        assert_eq!(
            jmap::JmapCalendarBackend
                .push_attendee_responses(&ctx(&db), &jmap, std::slice::from_ref(&update))
                .await
                .unwrap(),
            CalendarCapability::Supported(())
        );
        assert_eq!(
            graph::GraphCalendarBackend
                .push_attendee_responses(&ctx(&db), &graph, &[update])
                .await
                .unwrap(),
            CalendarCapability::Unsupported
        );
    }
}
