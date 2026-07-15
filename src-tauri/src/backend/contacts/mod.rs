//! Contact backends: one implementor per contacts protocol.
//!
//! ## Adding a provider
//!
//! 1. Put the transport client in `mail/`.
//! 2. Implement [`ContactBackend`] for a unit struct in a new module
//!    here, keeping the provider's push/error semantics.
//! 3. Add the struct to [`registry`] (and, if the provider writes a
//!    legacy `contact_books.sync_type` value, teach [`for_sync_type`]
//!    the alias).
//!
//! Push methods are best-effort at the command layer: the local DB
//! write always lands, a failed push is logged.

use async_trait::async_trait;

use crate::db::accounts::AccountFull;
use crate::db::contacts::Contact;
use crate::db::pool::DbPool;
use crate::error::Result;

pub mod carddav;
pub mod google;
pub mod graph;
pub mod jmap;

/// The book coordinates a push needs: the local book id plus the
/// book's remote handle (CardDAV collection href / JMAP address-book
/// id). Providers with a single implicit book ignore it.
pub struct BookRef<'a> {
    pub book_id: &'a str,
    pub remote_id: Option<&'a str>,
}

/// What to persist on the local contact row after a successful push.
/// `None` fields are left untouched (etag/vcard are CardDAV-only;
/// updates usually return no new remote_id).
pub struct PushedContact {
    pub remote_id: Option<String>,
    pub etag: Option<String>,
    pub vcard: Option<String>,
}

#[async_trait]
pub trait ContactBackend: Send + Sync {
    /// Protocol discriminator stored on the contacts service binding
    /// (`service_bindings.protocol`).
    fn protocol(&self) -> &'static str;

    /// Full account contact sync: fetch remote books/contacts and
    /// reconcile the local DB (including pushing local unpushed rows
    /// where the provider does that during sync).
    async fn sync(&self, db: &DbPool, account: &AccountFull) -> Result<()>;

    /// Push a new local contact. `Ok(None)` means the provider defers
    /// the push (JMAP creates go out with the next sync's
    /// unpushed-rows pass) or has nowhere to put it (CardDAV book
    /// without a collection href).
    async fn push_created_contact(
        &self,
        account: &AccountFull,
        book: &BookRef<'_>,
        contact: &Contact,
    ) -> Result<Option<PushedContact>>;

    /// Push field updates for a contact that already has a remote_id.
    async fn push_updated_contact(
        &self,
        account: &AccountFull,
        book: &BookRef<'_>,
        contact: &Contact,
        remote_id: &str,
    ) -> Result<Option<PushedContact>>;

    /// Delete a contact on the server.
    async fn push_deleted_contact(&self, account: &AccountFull, remote_id: &str) -> Result<()>;
}

/// Static set of contact backends compiled into this build.
pub fn registry() -> &'static [&'static dyn ContactBackend] {
    &[
        &jmap::JmapContactBackend,
        &google::GoogleContactBackend,
        &graph::GraphContactBackend,
        &carddav::CardDavContactBackend,
    ]
}

/// Find the backend for the account's enabled contacts binding.
pub fn for_account(account: &AccountFull) -> Option<&'static dyn ContactBackend> {
    for_sync_type(account.contacts_protocol_str())
}

/// Lookup keyed on the legacy `contact_books.sync_type` column, which
/// stores `o365` where the service binding says `graph`. Normalized
/// here; there is deliberately no data migration (ADR 0050) — Graph
/// sync still writes `'o365'` rows.
pub fn for_sync_type(sync_type: &str) -> Option<&'static dyn ContactBackend> {
    let proto = match sync_type {
        "o365" => "graph",
        other => other,
    };
    if proto.is_empty() {
        return None;
    }
    registry().iter().copied().find(|b| b.protocol() == proto)
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use crate::db::service_bindings::ServiceBinding;

    fn account(contacts_protocol: &str) -> AccountFull {
        let bindings = if contacts_protocol.is_empty() {
            Vec::new()
        } else {
            vec![ServiceBinding {
                id: "b1".into(),
                account_id: "acc1".into(),
                service: "contacts".into(),
                protocol: contacts_protocol.into(),
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
            auth_method: String::new(),
            bindings,
            mail_sync_enabled: true,
            contacts_sync_enabled: true,
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
        for proto in ["jmap", "google", "graph", "carddav"] {
            let b = for_account(&account(proto)).expect(proto);
            assert_eq!(b.protocol(), proto);
        }
    }

    #[test]
    fn legacy_o365_sync_type_maps_to_graph() {
        let b = for_sync_type("o365").unwrap();
        assert_eq!(b.protocol(), "graph");
    }

    #[test]
    fn unknown_or_empty_sync_type_is_none() {
        assert!(for_sync_type("gopher").is_none());
        assert!(for_sync_type("").is_none());
        assert!(for_account(&account("")).is_none());
    }
}
