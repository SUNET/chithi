//! Mail backends: one implementor per mail protocol.
//!
//! Covers the sync entry points the sync commands dispatch on
//! (account sync, per-folder sync, body prefetch) plus capability
//! flags for the command-layer concurrency machinery (IDLE
//! suspension, background folder sync). Queued user operations
//! (move/delete/flag/send) go through the ops worker; the
//! protocol-specific push/idle starters (`start_imap_idle`,
//! `start_jmap_push`) stay per-provider like meet auth flows.

use async_trait::async_trait;

use crate::db::accounts::AccountFull;
use crate::db::pool::DbPool;
use crate::error::Result;
use crate::ops::queue::MailOp;

pub mod graph;
pub mod imap;
pub mod jmap;

/// Everything a mail sync needs besides the account: the app handle
/// (sync progress events are emitted deep inside the sync loops), the
/// DB pool, and the Maildir root.
pub struct MailSyncCtx {
    pub app: tauri::AppHandle,
    pub db: std::sync::Arc<DbPool>,
    pub data_dir: std::path::PathBuf,
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
    fn suspends_idle_for_ops(&self, _account: &AccountFull) -> bool {
        false
    }

    /// This provider has no cheap per-folder fetch — the command
    /// spawns a whole-account [`Self::sync_account`] in the background
    /// (holding the sync guard) and returns immediately so the UI's
    /// per-folder spinner doesn't sit there for minutes.
    fn folder_sync_backgrounds(&self) -> bool {
        false
    }

    /// Full account sync. Emits its own `sync-started` /
    /// `sync-complete` progress events (inside the provider sync
    /// loops); errors are emitted as `sync-error` by the command.
    async fn sync_account(
        &self,
        ctx: &MailSyncCtx,
        account: &AccountFull,
        current_folder: Option<String>,
    ) -> Result<()>;

    /// Sync one folder's envelopes; returns the new-message count.
    /// Not called when [`Self::folder_sync_backgrounds`] is true.
    async fn sync_folder(
        &self,
        ctx: &MailSyncCtx,
        account: &AccountFull,
        folder_path: &str,
    ) -> Result<u32>;

    /// Prefetch message bodies into the Maildir. Default no-op for
    /// providers that fetch bodies on demand (JMAP) or during sync.
    async fn prefetch_bodies(&self, _ctx: &MailSyncCtx, _account: &AccountFull) -> Result<u32> {
        Ok(0)
    }

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
    /// Execute one queued operation. Sync ops never reach this — the
    /// worker routes them through [`MailBackend::sync_account`].
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
pub fn for_account(account: &AccountFull) -> Option<&'static dyn MailBackend> {
    let proto = account.mail_protocol_str();
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
    use crate::db::service_bindings::ServiceBinding;

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
            let b = for_account(&account(proto, "password")).expect(proto);
            assert_eq!(b.protocol(), proto);
        }
    }

    #[test]
    fn unknown_protocol_falls_back_to_imap() {
        let b = for_account(&account("pop3", "password")).unwrap();
        assert_eq!(b.protocol(), "imap");
    }

    #[test]
    fn empty_protocol_is_none() {
        assert!(for_account(&account("", "password")).is_none());
    }

    #[test]
    fn only_o365_imap_suspends_idle() {
        let imap_o365 = account("imap", "oauth-microsoft");
        let imap_plain = account("imap", "password");
        let graph = account("graph", "oauth-microsoft");
        assert!(for_account(&imap_o365)
            .unwrap()
            .suspends_idle_for_ops(&imap_o365));
        assert!(!for_account(&imap_plain)
            .unwrap()
            .suspends_idle_for_ops(&imap_plain));
        assert!(!for_account(&graph).unwrap().suspends_idle_for_ops(&graph));
    }

    #[test]
    fn only_graph_backgrounds_folder_sync() {
        assert!(for_account(&account("graph", "oauth-microsoft"))
            .unwrap()
            .folder_sync_backgrounds());
        assert!(!for_account(&account("imap", "password"))
            .unwrap()
            .folder_sync_backgrounds());
        assert!(!for_account(&account("jmap", "password"))
            .unwrap()
            .folder_sync_backgrounds());
    }
}
