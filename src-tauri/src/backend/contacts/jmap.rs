//! JMAP contact backend (RFC 9553 JSContact via `ContactCard/*`).

use async_trait::async_trait;

use crate::db::accounts::AccountFull;
use crate::db::contacts::Contact;
use crate::db::pool::DbPool;
use crate::error::Result;
use crate::mail::jmap::JmapConnection;

use super::{BookRef, ContactBackend, PushedContact};

pub struct JmapContactBackend;

#[async_trait]
impl ContactBackend for JmapContactBackend {
    fn protocol(&self) -> &'static str {
        "jmap"
    }

    async fn sync(&self, db: &DbPool, account: &AccountFull) -> Result<()> {
        let account_id = account.id.as_str();
        let jmap_config = crate::auth::build_jmap_config(account).await?;

        let jmap_conn = JmapConnection::connect(&jmap_config).await?;

        // Step 1: Fetch address books
        let address_books = jmap_conn.list_address_books(&jmap_config).await?;
        log::info!(
            "sync_contacts: fetched {} address books from JMAP",
            address_books.len()
        );

        let mut remote_to_local: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        {
            let conn = db.writer().await;
            for ab in &address_books {
                // Upsert contact book
                let existing: Option<String> = conn
                    .query_row(
                        "SELECT id FROM contact_books WHERE account_id = ?1 AND remote_id = ?2",
                        rusqlite::params![account_id, ab.id],
                        |row| row.get(0),
                    )
                    .ok();

                let local_id = if let Some(id) = existing {
                    conn.execute(
                        "UPDATE contact_books SET name = ?1 WHERE id = ?2",
                        rusqlite::params![ab.name, id],
                    )?;
                    id
                } else {
                    let id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO contact_books (id, account_id, name, remote_id, sync_type) VALUES (?1, ?2, ?3, ?4, 'jmap')",
                        rusqlite::params![id, account_id, ab.name, ab.id],
                    )?;
                    id
                };
                remote_to_local.insert(ab.id.clone(), local_id);
            }
        }

        // Step 2: Fetch contacts for each address book
        for ab in &address_books {
            let jmap_contacts = match jmap_conn.fetch_contacts(&jmap_config, Some(&ab.id)).await {
                Ok(c) => c,
                Err(e) => {
                    log::error!(
                        "sync_contacts: failed to fetch contacts for '{}': {}",
                        ab.name,
                        e
                    );
                    continue;
                }
            };

            log::info!(
                "sync_contacts: fetched {} contacts for '{}'",
                jmap_contacts.len(),
                ab.name
            );

            let local_book_id = remote_to_local.get(&ab.id).cloned().unwrap_or_default();
            let conn = db.writer().await;

            for jc in &jmap_contacts {
                // Upsert by remote_id
                let existing: Option<String> = conn
                    .query_row(
                        "SELECT id FROM contacts WHERE book_id = ?1 AND remote_id = ?2",
                        rusqlite::params![local_book_id, jc.id],
                        |row| row.get(0),
                    )
                    .ok();

                if let Some(id) = existing {
                    conn.execute(
                        "UPDATE contacts SET display_name=?1, emails_json=?2, phones_json=?3, organization=?4, title=?5, notes=?6, uid=?7, updated_at=CURRENT_TIMESTAMP WHERE id=?8",
                        rusqlite::params![jc.display_name, jc.emails_json, jc.phones_json, jc.organization, jc.title, jc.notes, jc.uid, id],
                    )?;
                } else {
                    let id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO contacts (id, book_id, uid, display_name, emails_json, phones_json, addresses_json, organization, title, notes, remote_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, '[]', ?7, ?8, ?9, ?10)",
                        rusqlite::params![id, local_book_id, jc.uid, jc.display_name, jc.emails_json, jc.phones_json, jc.organization, jc.title, jc.notes, jc.id],
                    )?;
                }
            }

            // Remove contacts deleted on server
            let server_ids: std::collections::HashSet<String> =
                jmap_contacts.iter().map(|c| c.id.clone()).collect();
            let local_synced: Vec<(String, String)> = conn
                .prepare(
                    "SELECT id, remote_id FROM contacts WHERE book_id = ?1 AND remote_id IS NOT NULL AND remote_id != ''",
                )
                .and_then(|mut stmt| {
                    stmt.query_map(rusqlite::params![local_book_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                })
                .unwrap_or_default();

            let mut deleted = 0u32;
            for (local_id, remote_id) in &local_synced {
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
                    "sync_contacts: removed {} server-deleted contacts from '{}'",
                    deleted,
                    ab.name
                );
            }

            // Push local contacts (no remote_id) to server
            type UnpushedRow = (
                String,
                String,
                String,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
            );
            let unpushed: Vec<UnpushedRow> = conn
                .prepare(
                    "SELECT id, display_name, emails_json, phones_json, organization, title, notes
                     FROM contacts WHERE book_id = ?1 AND (remote_id IS NULL OR remote_id = '')",
                )
                .and_then(|mut stmt| {
                    stmt.query_map(rusqlite::params![local_book_id], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, Option<String>>(6)?,
                        ))
                    })
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                })
                .unwrap_or_default();

            if !unpushed.is_empty() {
                log::info!(
                    "sync_contacts: pushing {} local contacts to JMAP for '{}'",
                    unpushed.len(),
                    ab.name
                );
                drop(conn); // Release lock for async calls

                for (local_id, name, emails, phones, org, title, notes) in &unpushed {
                    match jmap_conn
                        .create_contact_card(
                            &jmap_config,
                            &ab.id,
                            name,
                            emails,
                            phones,
                            org.as_deref(),
                            title.as_deref(),
                            notes.as_deref(),
                        )
                        .await
                    {
                        Ok(remote_id) => {
                            log::info!(
                                "sync_contacts: pushed contact '{}' to JMAP, remote_id={}",
                                name,
                                remote_id
                            );
                            let conn = db.writer().await;
                            conn.execute(
                                "UPDATE contacts SET remote_id = ?1 WHERE id = ?2",
                                rusqlite::params![remote_id, local_id],
                            )
                            .ok();
                        }
                        Err(e) => {
                            log::error!("sync_contacts: failed to push contact '{}': {}", name, e);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// JMAP contact creation is deferred to the next sync's
    /// unpushed-rows pass (see `sync`), not pushed at create time.
    async fn push_created_contact(
        &self,
        _account: &AccountFull,
        _book: &BookRef<'_>,
        _contact: &Contact,
    ) -> Result<Option<PushedContact>> {
        Ok(None)
    }

    async fn push_updated_contact(
        &self,
        account: &AccountFull,
        _book: &BookRef<'_>,
        contact: &Contact,
        remote_id: &str,
    ) -> Result<Option<PushedContact>> {
        let jmap_config = crate::auth::build_jmap_config(account).await?;
        let conn_jmap = JmapConnection::connect(&jmap_config).await?;
        conn_jmap
            .update_contact_card(
                &jmap_config,
                remote_id,
                &contact.display_name,
                &contact.emails_json,
                &contact.phones_json,
                contact.organization.as_deref(),
                contact.title.as_deref(),
                contact.notes.as_deref(),
            )
            .await?;
        Ok(None)
    }

    async fn push_deleted_contact(&self, account: &AccountFull, remote_id: &str) -> Result<()> {
        let jmap_config = crate::auth::build_jmap_config(account).await?;
        let conn_jmap = JmapConnection::connect(&jmap_config).await?;
        conn_jmap.delete_contact_card(&jmap_config, remote_id).await
    }
}
