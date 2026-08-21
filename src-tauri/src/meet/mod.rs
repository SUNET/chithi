//! Video-conferencing integrations (#148).
//!
//! Three providers in this slice:
//! - `talk` — Nextcloud Talk via the OCS Spreed v4 API.
//! - `matrix` — Matrix / Element Call.
//! - `zoom` — Zoom Marketplace OAuth + Meeting API.
//!
//! All three bind to a new `meet` service on the existing
//! service-binding plumbing (so accounts surface alongside
//! CalDAV / CardDAV-only accounts), and all three use a
//! browser-assisted login flow rather than ask the user for raw
//! passwords:
//! - Talk: Nextcloud "Login Flow v2" (poll-based, returns a
//!   long-lived app password tied to the user).
//! - Matrix: SSO redirect to the homeserver, captures
//!   `loginToken` on a local listener, exchanges via
//!   `m.login.token` for an `access_token`.
//! - Zoom: standard OAuth 2.0 Authorization Code + PKCE against
//!   a Marketplace-registered app on a pinned loopback port.
//!   Tokens auto-refresh on use (60-min access-token lifetime).
//!
//! ## Adding a new provider
//!
//! 1. Create a module under `meet/` (e.g. `meet/bbb.rs`) with the
//!    provider's auth flow and create-room logic.
//! 2. Implement [`MeetProvider`] for a unit struct in that module.
//!    Auth flows aren't part of the trait because they vary widely
//!    (poll, SSO redirect, static shared secret); they live as
//!    free functions on the module and have their own Tauri
//!    commands wired in `commands/meet.rs`.
//! 3. Add a constructor entry in [`registry`] so the dispatcher
//!    knows the new protocol string.
//!
//! That's it — the rest of the app reads the `meet` binding through
//! `provider_for(&account)` and never name-matches on the protocol
//! string.

use async_trait::async_trait;
use serde::Serialize;

use crate::db::accounts::AccountFull;
use crate::error::Result;
use crate::provider::ProviderServices;

pub mod matrix;
pub mod talk;
pub mod zoom;

pub struct MeetProviderCtx<'a> {
    pub services: &'a ProviderServices,
}

/// What `create_url` hands back. The join URL is what goes on the
/// calendar event; `meeting_id` is the provider-specific handle the
/// app remembers so it can later reschedule or delete the same
/// remote meeting (Zoom numeric id, Talk room token, Matrix room id).
#[derive(Debug, Clone, Serialize)]
pub struct MeetCreateResult {
    pub join_url: String,
    pub meeting_id: String,
}

/// Common surface every meet provider exposes once an account has
/// been authenticated. Auth flow stays per-provider because each
/// is shaped differently; this trait covers the post-auth actions
/// the rest of the app needs.
#[async_trait]
pub trait MeetProvider: Send + Sync {
    /// Protocol discriminator stored on the binding row
    /// (`service_bindings.protocol`).
    fn protocol(&self) -> &'static str;

    /// Create a fresh meeting room / call and return the join URL
    /// the user should put on their calendar event. The caller
    /// supplies a hint name (often the event title) plus optional
    /// `start_time` (ISO 8601 UTC, e.g. `2026-05-12T14:00:00Z`) and
    /// `duration_minutes`; each provider decides whether to pass
    /// them through. Persistent-room providers (Talk, Matrix) ignore
    /// the time inputs; time-bound providers (Zoom) include them so
    /// the meeting lands on the correct slot in the host's schedule.
    async fn create_url(
        &self,
        ctx: &MeetProviderCtx<'_>,
        account: &AccountFull,
        name: &str,
        start_time: Option<&str>,
        duration_minutes: Option<u32>,
    ) -> Result<MeetCreateResult>;

    /// Delete the remote meeting identified by `meeting_id`. Called
    /// when the calendar event is cancelled (or replaced). Errors
    /// surface to the caller; the caller logs and proceeds with the
    /// local cleanup either way so a transient provider failure
    /// doesn't strand the event in an undeletable state.
    async fn delete_meeting(
        &self,
        ctx: &MeetProviderCtx<'_>,
        account: &AccountFull,
        meeting_id: &str,
    ) -> Result<()>;

    /// Rename the remote meeting's title / topic. Called after
    /// create_event / update_event because the user typically
    /// clicks "Add video link" *before* typing the event title, so
    /// the title we sent to create_url was empty (and the provider
    /// defaulted to "Meeting"). Re-applying on save keeps the
    /// remote room's name in sync with the calendar event.
    async fn update_topic(
        &self,
        ctx: &MeetProviderCtx<'_>,
        account: &AccountFull,
        meeting_id: &str,
        topic: &str,
    ) -> Result<()>;

    /// Move an existing meeting to a new start time + duration.
    /// Default impl is a no-op so persistent-room providers
    /// (Talk, Matrix) inherit the right behaviour: their rooms
    /// aren't time-bound, so a date change on the calendar event
    /// doesn't require touching the provider.
    async fn reschedule_meeting(
        &self,
        _ctx: &MeetProviderCtx<'_>,
        _account: &AccountFull,
        _meeting_id: &str,
        _start_time: &str,
        _duration_minutes: u32,
    ) -> Result<()> {
        Ok(())
    }
}

/// Static set of providers compiled into this build. Adding a new
/// provider = a new line here. Lookup is by protocol string from
/// the meet binding.
pub fn registry() -> &'static [&'static dyn MeetProvider] {
    &[
        &talk::TalkProvider,
        &matrix::MatrixProvider,
        &zoom::ZoomProvider,
    ]
}

/// Dispatch helper: find the provider matching the account's enabled
/// meet binding. Returns `None` when the account has no meet binding
/// or the protocol is one we don't know about (which would surface
/// as a config error in the UI).
pub fn provider_for(account: &AccountFull) -> Option<&'static dyn MeetProvider> {
    let proto = account.meet_protocol_str();
    if proto.is_empty() {
        return None;
    }
    provider_for_protocol(proto)
}

/// Resolve a compiled provider by its stored protocol. Cleanup callers must
/// separately validate that the owning account has an exact meet binding.
pub fn provider_for_protocol(protocol: &str) -> Option<&'static dyn MeetProvider> {
    registry()
        .iter()
        .copied()
        .find(|provider| provider.protocol() == protocol)
}
