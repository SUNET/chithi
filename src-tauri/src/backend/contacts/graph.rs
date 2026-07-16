//! Microsoft Graph contact backend (O365 / Outlook).
//!
//! Note: this backend's book rows carry the legacy
//! `sync_type = 'o365'` value (see `for_sync_type` and ADR 0050).

use async_trait::async_trait;

use crate::db::accounts::AccountFull;
use crate::db::contacts::Contact;
use crate::db::pool::DbPool;
use crate::error::Result;
use crate::mail::graph::{contact_to_graph_json, get_graph_token, GraphClient};

use super::{BookRef, ContactBackend, PushedContact};

pub struct GraphContactBackend;

#[async_trait]
impl ContactBackend for GraphContactBackend {
    fn protocol(&self) -> &'static str {
        "graph"
    }

    async fn sync(&self, db: &DbPool, account: &AccountFull) -> Result<()> {
        let account_id = account.id.as_str();
        log::info!("sync_contacts_graph: starting for account {}", account_id);

        let token = match get_graph_token(account_id).await {
            Ok(t) => t,
            Err(e) => {
                log::error!("sync_contacts_graph: failed to get token: {}", e);
                return Err(e);
            }
        };
        let client = GraphClient::new(&token);

        // 1. Ensure contact book exists
        let book_id = {
            let conn = db.writer().await;
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM contact_books WHERE account_id = ?1 AND sync_type = 'o365'",
                    rusqlite::params![account_id],
                    |row| row.get(0),
                )
                .ok();

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

        // 2. Fetch contacts from Graph
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

        let conn = db.writer().await;

        // Build set of server IDs for reconciliation
        let server_ids: std::collections::HashSet<String> =
            graph_contacts.iter().map(|c| c.id.clone()).collect();

        // 3. Upsert contacts
        for gc in &graph_contacts {
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM contacts WHERE book_id = ?1 AND remote_id = ?2",
                    rusqlite::params![book_id, gc.id],
                    |row| row.get(0),
                )
                .ok();

            match existing {
                Some(local_id) => {
                    // Update existing contact
                    conn.execute(
                        "UPDATE contacts SET display_name = ?1, emails_json = ?2, phones_json = ?3,
                         organization = ?4, title = ?5, updated_at = CURRENT_TIMESTAMP
                         WHERE id = ?6",
                        rusqlite::params![
                            gc.display_name,
                            gc.emails_json,
                            gc.phones_json,
                            gc.organization,
                            gc.title,
                            local_id,
                        ],
                    )
                    .ok();
                }
                None => {
                    // Insert new contact
                    let id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO contacts (id, book_id, display_name, emails_json, phones_json, organization, title, remote_id)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        rusqlite::params![
                            id,
                            book_id,
                            gc.display_name,
                            gc.emails_json,
                            gc.phones_json,
                            gc.organization,
                            gc.title,
                            gc.id,
                        ],
                    )?;
                }
            }
        }

        // 4. Remove contacts deleted on server
        let local_contacts: Vec<(String, String)> = conn
            .prepare(
                "SELECT id, remote_id FROM contacts WHERE book_id = ?1 AND remote_id IS NOT NULL AND remote_id != ''",
            )?
            .query_map(rusqlite::params![book_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let mut deleted = 0;
        for (local_id, remote_id) in &local_contacts {
            if !server_ids.contains(remote_id) {
                conn.execute(
                    "DELETE FROM contacts WHERE id = ?1",
                    rusqlite::params![local_id],
                )
                .ok();
                deleted += 1;
            }
        }
        if deleted > 0 {
            log::info!(
                "sync_contacts_graph: removed {} server-deleted contacts",
                deleted
            );
        }

        log::info!("sync_contacts_graph: completed for account {}", account_id);
        Ok(())
    }

    async fn push_created_contact(
        &self,
        account: &AccountFull,
        _book: &BookRef<'_>,
        contact: &Contact,
    ) -> Result<Option<PushedContact>> {
        let token = get_graph_token(&account.id).await?;
        let client = GraphClient::new(&token);
        let gc = contact_to_graph_json(
            &contact.display_name,
            &contact.emails_json,
            &contact.phones_json,
            contact.organization.as_deref(),
            contact.title.as_deref(),
        );
        let remote_id = client.create_contact(&gc).await?;
        Ok(Some(PushedContact {
            remote_id: Some(remote_id),
            etag: None,
            vcard: None,
        }))
    }

    async fn push_updated_contact(
        &self,
        account: &AccountFull,
        _book: &BookRef<'_>,
        contact: &Contact,
        remote_id: &str,
    ) -> Result<Option<PushedContact>> {
        let token = get_graph_token(&account.id).await?;
        let client = GraphClient::new(&token);
        let gc = contact_to_graph_json(
            &contact.display_name,
            &contact.emails_json,
            &contact.phones_json,
            contact.organization.as_deref(),
            contact.title.as_deref(),
        );
        client.update_contact(remote_id, &gc).await?;
        Ok(None)
    }

    async fn push_deleted_contact(&self, account: &AccountFull, remote_id: &str) -> Result<()> {
        let token = get_graph_token(&account.id).await?;
        let client = GraphClient::new(&token);
        client.delete_contact(remote_id).await
    }
}
