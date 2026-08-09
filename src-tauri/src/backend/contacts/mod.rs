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
use crate::provider::ProviderServices;

pub mod carddav;
pub mod google;
pub mod graph;
pub mod jmap;

pub struct ContactBackendCtx<'a> {
    pub db: &'a DbPool,
    pub providers: &'a ProviderServices,
}

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
    async fn sync(&self, ctx: &ContactBackendCtx<'_>, account: &AccountFull) -> Result<()>;

    /// Push a new local contact. `Ok(None)` means the provider defers
    /// the push (JMAP creates go out with the next sync's
    /// unpushed-rows pass) or has nowhere to put it (CardDAV book
    /// without a collection href).
    async fn push_created_contact(
        &self,
        ctx: &ContactBackendCtx<'_>,
        account: &AccountFull,
        book: &BookRef<'_>,
        contact: &Contact,
    ) -> Result<Option<PushedContact>>;

    /// Push field updates for a contact that already has a remote_id.
    async fn push_updated_contact(
        &self,
        ctx: &ContactBackendCtx<'_>,
        account: &AccountFull,
        book: &BookRef<'_>,
        contact: &Contact,
        remote_id: &str,
    ) -> Result<Option<PushedContact>>;

    /// Delete a contact on the server.
    async fn push_deleted_contact(
        &self,
        ctx: &ContactBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
    ) -> Result<()>;
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

    fn account(contacts_protocol: &str) -> AccountFull {
        crate::backend::testutil::account("contacts", contacts_protocol)
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

/// Per-provider semantics ADR 0050 calls load-bearing. The fixture
/// account has no credentials or server URLs, so any I/O attempt fails
/// before the network — which is exactly what these tests lean on:
/// deferred paths must succeed without I/O, swallowing backends must
/// turn the failure into `Ok`, propagating backends into `Err`.
#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::backend::testutil::{account, contact, temp_pool};

    fn providers() -> ProviderServices {
        ProviderServices::production().unwrap()
    }

    /// JMAP contact creation is deferred to the next sync's
    /// unpushed-rows pass — never pushed at create time.
    #[tokio::test]
    async fn jmap_defers_contact_creation_to_sync() {
        let book = BookRef {
            book_id: "b1",
            remote_id: Some("ab1"),
        };
        let (_dir, db) = temp_pool();
        let providers = providers();
        let ctx = ContactBackendCtx {
            db: &db,
            providers: &providers,
        };
        let pushed = jmap::JmapContactBackend
            .push_created_contact(&ctx, &account("contacts", "jmap"), &book, &contact())
            .await
            .unwrap();
        assert!(pushed.is_none());
    }

    /// A CardDAV book without a collection href has nowhere to push;
    /// the create is deferred instead of erroring.
    #[tokio::test]
    async fn carddav_create_without_collection_href_is_deferred() {
        let book = BookRef {
            book_id: "b1",
            remote_id: None,
        };
        let (_dir, db) = temp_pool();
        let providers = providers();
        let ctx = ContactBackendCtx {
            db: &db,
            providers: &providers,
        };
        let pushed = carddav::CardDavContactBackend
            .push_created_contact(&ctx, &account("contacts", "carddav"), &book, &contact())
            .await
            .unwrap();
        assert!(pushed.is_none());
    }

    /// Google and CardDAV contact syncs swallow failures (warn + Ok)
    /// so one broken provider can't fail the whole contacts sync.
    #[tokio::test]
    async fn google_and_carddav_sync_swallow_failures() {
        let (_dir, db) = temp_pool();
        let providers = providers();
        let ctx = ContactBackendCtx {
            db: &db,
            providers: &providers,
        };
        google::GoogleContactBackend
            .sync(&ctx, &account("contacts", "google"))
            .await
            .unwrap();
        let mut acc = account("contacts", "carddav");
        // Non-empty invalid URL: fails validation without triggering
        // live `.well-known` auto-discovery.
        acc.caldav_url = "not a url".into();
        carddav::CardDavContactBackend
            .sync(&ctx, &acc)
            .await
            .unwrap();
    }

    /// JMAP and Graph contact syncs propagate failures.
    #[tokio::test]
    async fn jmap_and_graph_sync_propagate_failures() {
        let (_dir, db) = temp_pool();
        let providers = providers();
        let ctx = ContactBackendCtx {
            db: &db,
            providers: &providers,
        };
        let mut acc = account("contacts", "jmap");
        acc.jmap_url = "not a url".into();
        assert!(jmap::JmapContactBackend.sync(&ctx, &acc).await.is_err());
        assert!(graph::GraphContactBackend
            .sync(&ctx, &account("contacts", "graph"))
            .await
            .is_err());
    }
}
