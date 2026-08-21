//! Microsoft Graph contact backend (O365 / Outlook).
//!
//! Note: this backend's book rows carry the legacy
//! `sync_type = 'o365'` value (see `for_sync_type` and ADR 0050).

use async_trait::async_trait;
use rusqlite::OptionalExtension;

use crate::contact::reconcile::{
    capture_remote_id_baseline, reconcile_remote_id_snapshot, repair_duplicate_managed_remote_ids,
    CompleteRemoteIdSnapshot, RemoteContactPatch, RemoteField,
};
use crate::contact::Contact;
use crate::db::accounts::AccountFull;
use crate::error::Result;
use crate::mail::graph::contact_to_graph_json;
use crate::provider::GraphTokenPurpose;

use super::{BookRef, ContactBackend, ContactBackendCtx, PushedContact};

pub struct GraphContactBackend;

const GRAPH_BOOK_SYNC_TYPE: &str = "o365";

#[async_trait]
impl ContactBackend for GraphContactBackend {
    fn protocol(&self) -> &'static str {
        "graph"
    }

    async fn sync(&self, ctx: &ContactBackendCtx<'_>, account: &AccountFull) -> Result<()> {
        let account_id = account.id.as_str();
        log::info!("sync_contacts_graph: starting for account {}", account_id);

        let client = match ctx
            .providers
            .graph_client(account_id, GraphTokenPurpose::Baseline)
            .await
        {
            Ok(client) => client,
            Err(e) => {
                log::error!("sync_contacts_graph: failed to get token: {}", e);
                return Err(e);
            }
        };

        let book_id = {
            let conn = ctx.db.writer().await;
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM contact_books
                     WHERE account_id = ?1 AND sync_type = ?2
                     ORDER BY created_at, id LIMIT 1",
                    rusqlite::params![account_id, GRAPH_BOOK_SYNC_TYPE],
                    |row| row.get(0),
                )
                .optional()?;

            match existing {
                Some(id) => id,
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO contact_books (id, account_id, name, sync_type) VALUES (?1, ?2, 'Outlook Contacts', 'o365')",
                        rusqlite::params![id, account_id],
                    )?;
                    log::info!("sync_contacts_graph: created contact book 'Outlook Contacts'");
                    id
                }
            }
        };

        let graph_contacts = match client.list_contacts().await {
            Ok(c) => c,
            Err(e) => {
                log::error!("sync_contacts_graph: list_contacts failed: {}", e);
                return Err(e);
            }
        };
        log::info!(
            "sync_contacts_graph: fetched {} contacts",
            graph_contacts.len()
        );

        let snapshot = CompleteRemoteIdSnapshot::new(
            graph_contacts
                .into_iter()
                .map(|contact| RemoteContactPatch {
                    uid: RemoteField::Preserve,
                    display_name: RemoteField::Set(contact.display_name),
                    emails_json: RemoteField::Set(contact.emails_json),
                    phones_json: RemoteField::Set(contact.phones_json),
                    addresses_json: RemoteField::Preserve,
                    organization: RemoteField::Set(contact.organization),
                    title: RemoteField::Set(contact.title),
                    notes: RemoteField::Preserve,
                    vcard_data: RemoteField::Preserve,
                    remote_id: RemoteField::Set(Some(contact.id)),
                    etag: RemoteField::Preserve,
                })
                .collect(),
        )?;
        let (repair_report, baseline) = {
            let mut conn = ctx.db.writer().await;
            let repair_report = repair_duplicate_managed_remote_ids(
                &mut conn,
                account_id,
                &book_id,
                GRAPH_BOOK_SYNC_TYPE,
            )?;
            let baseline =
                capture_remote_id_baseline(&conn, account_id, &book_id, GRAPH_BOOK_SYNC_TYPE)?;
            (repair_report, baseline)
        };
        if repair_report.detached > 0 {
            log::warn!(
                "sync_contacts_graph: detached {} duplicate managed remote ID assignments across \
                 {} remote IDs in book {}",
                repair_report.detached,
                repair_report.duplicate_remote_ids,
                book_id
            );
        }
        let report = {
            let mut conn = ctx.db.writer().await;
            reconcile_remote_id_snapshot(&mut conn, &baseline, snapshot)?
        };
        log::info!(
            "sync_contacts_graph: reconciled inserted={}, updated={}, deleted={}, \
             unchanged_or_stale={}",
            report.inserted,
            report.updated,
            report.deleted,
            report.unchanged_or_stale
        );

        log::info!("sync_contacts_graph: completed for account {}", account_id);
        Ok(())
    }

    async fn push_created_contact(
        &self,
        ctx: &ContactBackendCtx<'_>,
        account: &AccountFull,
        _book: &BookRef<'_>,
        contact: &Contact,
    ) -> Result<Option<PushedContact>> {
        let client = ctx
            .providers
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?;
        let gc = contact_to_graph_json(
            &contact.display_name,
            &contact.emails_json,
            &contact.phones_json,
            contact.organization.as_deref(),
            contact.title.as_deref(),
        )?;
        let remote_id = client.create_contact(&gc).await?;
        Ok(Some(PushedContact {
            remote_id: Some(remote_id),
            etag: None,
            vcard: None,
        }))
    }

    async fn push_updated_contact(
        &self,
        ctx: &ContactBackendCtx<'_>,
        account: &AccountFull,
        _book: &BookRef<'_>,
        contact: &Contact,
        remote_id: &str,
    ) -> Result<Option<PushedContact>> {
        let client = ctx
            .providers
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?;
        let gc = contact_to_graph_json(
            &contact.display_name,
            &contact.emails_json,
            &contact.phones_json,
            contact.organization.as_deref(),
            contact.title.as_deref(),
        )?;
        client.update_contact(remote_id, &gc).await?;
        Ok(None)
    }

    async fn push_deleted_contact(
        &self,
        ctx: &ContactBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
    ) -> Result<()> {
        let client = ctx
            .providers
            .graph_client(&account.id, GraphTokenPurpose::Baseline)
            .await?;
        client.delete_contact(remote_id).await
    }
}
