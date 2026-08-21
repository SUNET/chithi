//! Mail backends: one implementor per mail protocol.
//!
//! Covers the provider entry points commands dispatch on (account sync,
//! per-folder sync, body prefetch, server search, draft save) plus capability
//! flags for the command-layer concurrency machinery (IDLE
//! suspension, background folder sync). Queued user operations
//! (move/copy/delete/flag/send) go through the ops worker; the
//! protocol-specific push/idle starters (`start_imap_idle`,
//! `start_jmap_push`) stay per-provider like meet auth flows.

use async_trait::async_trait;

use crate::account::MailAccountConfig;
use crate::db::pool::DbPool;
use crate::error::Result;
use crate::event::SharedEventSink;
use crate::message::{BackendMessageRef, BodyLocation, SearchHit, SearchQuery};
use crate::ops::queue::MailOp;
use crate::provider::ProviderServices;

pub mod graph;
pub mod imap;
pub mod jmap;

/// Everything a mail operation needs besides the account: application event
/// delivery, the DB pool, the Maildir root, and shared provider services.
#[derive(Clone)]
pub struct MailSyncCtx {
    pub events: SharedEventSink,
    pub db: std::sync::Arc<DbPool>,
    pub data_dir: std::path::PathBuf,
    pub providers: std::sync::Arc<ProviderServices>,
}

/// Provider-neutral inputs for fetching one raw RFC 822 body into Maildir.
#[derive(Debug, Clone)]
pub struct BodyFetchRequest {
    pub message_id: String,
    pub message_ref: BackendMessageRef,
    pub folder_path: String,
    pub flags: Vec<String>,
    pub body_location: BodyLocation,
}

/// Representation a provider accepts when creating a server-side draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftStorageFormat {
    /// The provider stores the complete RFC 5322 message verbatim.
    RawMime,
    /// The provider accepts separate envelope and plaintext-body fields.
    StructuredText,
}

/// Provider-neutral inputs for creating a server-side draft.
///
/// IMAP and JMAP consume `raw_message`. Graph consumes the structured fields
/// until its raw-MIME draft endpoint is implemented.
#[derive(Debug, Clone)]
pub struct DraftSaveRequest {
    pub raw_message: Vec<u8>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: String,
}

impl BodyFetchRequest {
    pub fn from_db_row(
        account: &MailAccountConfig,
        message_id: &str,
        folder_path: &str,
        uid: u32,
        flags: Vec<String>,
        body_location: BodyLocation,
    ) -> Result<Self> {
        let protocol = account.protocol.as_str();
        let message_ref =
            BackendMessageRef::from_db_row(protocol, &account.id, message_id, folder_path, uid)
                .or_else(|| {
                    // Preserve the legacy JMAP body-fetch fallback for rows whose
                    // synthetic database id does not carry the expected prefix.
                    (protocol == "jmap").then(|| BackendMessageRef::jmap(folder_path, message_id))
                })
                .ok_or_else(|| {
                    crate::error::Error::Other(format!(
                        "Cannot recover {} message reference from database id {}",
                        protocol, message_id
                    ))
                })?;

        Ok(Self {
            message_id: message_id.to_string(),
            message_ref,
            folder_path: folder_path.to_string(),
            flags,
            body_location,
        })
    }
}

#[async_trait]
pub trait MailBackend: Send + Sync {
    /// Protocol discriminator stored on the mail service binding
    /// (`service_bindings.protocol`).
    fn protocol(&self) -> &'static str;

    /// O365 over IMAP allows one connection per account at a time, so
    /// IDLE must be suspended around any other server operation. The
    /// command owns the suspend/resume because it owns the idle-handle
    /// state.
    fn suspends_idle_for_ops(&self, _account: &MailAccountConfig) -> bool {
        false
    }

    /// Full account sync, invoked directly by the sync command. Emits its own
    /// `sync-started` / `sync-complete` progress events (inside the provider
    /// sync loops); errors are emitted as `sync-error` by the command.
    async fn sync_account(
        &self,
        ctx: &MailSyncCtx,
        account: &MailAccountConfig,
        current_folder: Option<String>,
    ) -> Result<()>;

    /// Sync one folder's envelopes; returns the new-message count.
    /// This must touch ONLY the requested folder — a user right-clicking
    /// "sync" on a folder expects exactly that folder to sync and then
    /// the operation to stop (Graph used to background a whole-account
    /// sync here; delta sync made a true per-folder fetch possible).
    async fn sync_folder(
        &self,
        ctx: &MailSyncCtx,
        account: &MailAccountConfig,
        folder_path: &str,
    ) -> Result<u32>;

    /// Prefetch message bodies into the Maildir. Default no-op for
    /// providers that fetch bodies on demand (JMAP) or during sync.
    async fn prefetch_bodies(
        &self,
        _ctx: &MailSyncCtx,
        _account: &MailAccountConfig,
    ) -> Result<u32> {
        Ok(0)
    }

    /// Fetch one raw RFC 822 body and persist it under the Maildir root.
    /// Returns the relative path; the command records it in the database.
    async fn fetch_body_to_disk(
        &self,
        ctx: &MailSyncCtx,
        account: &MailAccountConfig,
        request: &BodyFetchRequest,
    ) -> Result<String>;

    /// Search messages across the account on the provider server.
    async fn search_messages(
        &self,
        ctx: &MailSyncCtx,
        account: &MailAccountConfig,
        query: &SearchQuery,
    ) -> Result<Vec<SearchHit>>;

    /// Describe the draft representation this provider can persist.
    fn draft_storage_format(&self) -> DraftStorageFormat;

    /// Create a server-side draft.
    async fn save_draft(
        &self,
        ctx: &MailSyncCtx,
        account: &MailAccountConfig,
        request: &DraftSaveRequest,
    ) -> Result<()>;

    /// Create the per-account executor the ops worker drives queued
    /// [`MailOp`]s through. Called once per worker lifetime.
    fn op_executor(&self) -> Box<dyn MailOpExecutor>;
}

/// Per-account executor for queued [`MailOp`]s (move/delete/flag/copy/
/// send), created via [`MailBackend::op_executor`]. Unlike the
/// stateless backends, an executor may hold connection state across
/// calls — the IMAP executor keeps a persistent connection with
/// staleness tracking and reconnect backoff.
#[async_trait]
pub trait MailOpExecutor: Send {
    /// Execute one queued operation. Sync and replay operations never reach
    /// this; the worker handles them directly.
    async fn execute(&mut self, ctx: &MailSyncCtx, account_id: &str, op: MailOp) -> Result<()>;

    /// Called once when the worker shuts down; close connections.
    async fn shutdown(&mut self) {}
}

/// Static set of mail backends compiled into this build.
pub fn registry() -> &'static [&'static dyn MailBackend] {
    &[
        &imap::ImapMailBackend,
        &jmap::JmapMailBackend,
        &graph::GraphMailBackend,
    ]
}

/// Find the backend for the account's enabled mail binding.
///
/// A non-empty protocol that matches nothing falls back to IMAP — the
/// pre-trait dispatch chains all ended in an IMAP else-branch, and
/// pre-binding accounts rely on it. Empty (no enabled mail binding)
/// returns `None`; callers skip those accounts.
pub fn for_account(account: &MailAccountConfig) -> Option<&'static dyn MailBackend> {
    let proto = account.protocol.as_str();
    if proto.is_empty() {
        return None;
    }
    Some(
        registry()
            .iter()
            .copied()
            .find(|b| b.protocol() == proto)
            .unwrap_or(&imap::ImapMailBackend),
    )
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use crate::db::accounts::AccountFull;
    use crate::db::service_bindings::ServiceBinding;
    use crate::message::BodyLocation;

    fn account(mail_protocol: &str, auth_method: &str) -> AccountFull {
        let bindings = if mail_protocol.is_empty() {
            Vec::new()
        } else {
            vec![ServiceBinding {
                id: "b1".into(),
                account_id: "acc1".into(),
                service: "mail".into(),
                protocol: mail_protocol.into(),
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
            mail_protocol: mail_protocol.into(),
            imap_host: String::new(),
            imap_port: 0,
            smtp_host: String::new(),
            smtp_port: 0,
            jmap_url: String::new(),
            caldav_url: String::new(),
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
            calendar_sync_enabled: false,
            auth_method: auth_method.into(),
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
        for proto in ["imap", "jmap", "graph"] {
            let config = account(proto, "password").mail_config();
            let b = for_account(&config).expect(proto);
            assert_eq!(b.protocol(), proto);
        }
    }

    #[test]
    fn backends_report_their_draft_storage_format() {
        let cases = [
            ("imap", DraftStorageFormat::RawMime),
            ("jmap", DraftStorageFormat::RawMime),
            ("graph", DraftStorageFormat::StructuredText),
        ];

        for (protocol, expected) in cases {
            let config = account(protocol, "password").mail_config();
            let backend = for_account(&config).unwrap();
            assert_eq!(backend.draft_storage_format(), expected);
        }
    }

    #[test]
    fn unknown_protocol_falls_back_to_imap() {
        let config = account("pop3", "password").mail_config();
        let b = for_account(&config).unwrap();
        assert_eq!(b.protocol(), "imap");
    }

    #[test]
    fn empty_protocol_is_none() {
        assert!(for_account(&account("", "password").mail_config()).is_none());
    }

    #[test]
    fn focused_config_uses_enabled_binding_and_mail_fields() {
        let mut account = account("imap", "oauth-microsoft");
        account.imap_host = "imap.example.com".into();
        account.imap_port = 993;
        account.smtp_host = "smtp.example.com".into();
        account.smtp_port = 587;
        account.password = "keyring-secret".into();

        let config = account.mail_config();

        assert_eq!(config.protocol, "imap");
        assert_eq!(config.id, "acc1");
        assert_eq!(config.imap_host, "imap.example.com");
        assert_eq!(config.smtp_host, "smtp.example.com");
        assert_eq!(config.password, "keyring-secret");
        assert_eq!(config.auth_method, "oauth-microsoft");
    }

    #[test]
    fn focused_config_does_not_route_disabled_legacy_mail_protocol() {
        let mut account = account("imap", "password");
        account.bindings[0].enabled = false;

        let config = account.mail_config();

        assert!(config.protocol.is_empty());
        assert!(for_account(&config).is_none());
    }

    #[test]
    fn only_o365_imap_suspends_idle() {
        let imap_o365 = account("imap", "oauth-microsoft");
        let imap_plain = account("imap", "password");
        let graph = account("graph", "oauth-microsoft");
        let imap_o365 = imap_o365.mail_config();
        let imap_plain = imap_plain.mail_config();
        let graph = graph.mail_config();
        assert!(for_account(&imap_o365)
            .unwrap()
            .suspends_idle_for_ops(&imap_o365));
        assert!(!for_account(&imap_plain)
            .unwrap()
            .suspends_idle_for_ops(&imap_plain));
        assert!(!for_account(&graph).unwrap().suspends_idle_for_ops(&graph));
    }

    #[test]
    fn body_fetch_request_recovers_provider_references() {
        let cases = [
            ("imap", "acc1_INBOX_42", "INBOX", 42),
            ("jmap", "acc1_mailbox_email_with_underscores", "mailbox", 0),
            ("graph", "acc1_AAMk_opaque", "folder", 0),
        ];

        for (protocol, message_id, folder, uid) in cases {
            let account = account(protocol, "password").mail_config();
            let request = BodyFetchRequest::from_db_row(
                &account,
                message_id,
                folder,
                uid,
                Vec::new(),
                BodyLocation::NotFetched,
            )
            .unwrap();
            assert_eq!(request.message_ref.to_db_id("acc1"), message_id);
        }
    }

    #[test]
    fn body_fetch_request_preserves_legacy_jmap_raw_id_fallback() {
        let account = account("jmap", "password").mail_config();
        let request = BodyFetchRequest::from_db_row(
            &account,
            "raw_email_id",
            "mailbox",
            0,
            Vec::new(),
            BodyLocation::NotFetched,
        )
        .unwrap();
        assert_eq!(
            request.message_ref.into_jmap_email_id().as_deref(),
            Some("raw_email_id")
        );
    }
}
