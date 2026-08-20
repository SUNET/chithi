//! CardDAV contact backend (RFC 6352).

use async_trait::async_trait;

use crate::contact::Contact;
use crate::db;
use crate::db::accounts::AccountFull;
use crate::error::Result;
use crate::mail::carddav::{contact_to_vcard, parse_vcard};

use super::{BookRef, ContactBackend, ContactBackendCtx, PushedContact};

pub struct CardDavContactBackend;

/// Connect with the account's DAV coordinates. Uses `caldav_url` for
/// CardDAV too (the same server usually hosts both); if empty,
/// auto-discovery tries `.well-known/carddav`.
async fn connect(
    ctx: &ContactBackendCtx<'_>,
    account: &AccountFull,
) -> Result<crate::mail::carddav::CardDavClient> {
    ctx.providers
        .carddav_client(
            &account.caldav_url,
            &account.username,
            &account.password,
            &account.email,
        )
        .await
}

#[async_trait]
impl ContactBackend for CardDavContactBackend {
    fn protocol(&self) -> &'static str {
        "carddav"
    }

    /// CardDAV sync. Failures are swallowed with a warning — DAV
    /// servers without an addressbook collection would otherwise fail
    /// the whole contacts sync for the account.
    async fn sync(&self, ctx: &ContactBackendCtx<'_>, account: &AccountFull) -> Result<()> {
        let account_id = account.id.as_str();
        let result: Result<()> = async {
            log::info!("sync_contacts_carddav: starting for account {}", account_id);

            let client = connect(ctx, account).await?;

            let address_books = client.list_addressbooks().await?;
            log::info!(
                "sync_contacts_carddav: found {} address books",
                address_books.len()
            );

            for ab in &address_books {
                // Upsert contact book in DB
                let book_id = {
                    let conn = ctx.db.writer().await;
                    let book = db::contacts::ContactBook {
                        id: uuid::Uuid::new_v4().to_string(),
                        account_id: account_id.to_string(),
                        name: ab.name.clone(),
                        remote_id: Some(ab.href.clone()),
                        sync_type: "carddav".to_string(),
                    };

                    // Check if book already exists by remote_id
                    let existing = db::contacts::list_contact_books(&conn, account_id)?;
                    let found = existing
                        .iter()
                        .find(|b| b.remote_id.as_deref() == Some(&ab.href));
                    if let Some(existing_book) = found {
                        existing_book.id.clone()
                    } else {
                        db::contacts::insert_contact_book(&conn, &book)?;
                        book.id
                    }
                };

                // Fetch contacts from server
                let server_contacts = match client.fetch_contacts(&ab.href).await {
                    Ok(c) => c,
                    Err(e) => {
                        log::error!(
                            "sync_contacts_carddav: failed to fetch contacts from '{}': {}",
                            ab.name,
                            e
                        );
                        continue;
                    }
                };

                log::info!(
                    "sync_contacts_carddav: fetched {} contacts from '{}'",
                    server_contacts.len(),
                    ab.name
                );

                let conn = ctx.db.writer().await;

                // Get existing local contacts for this book
                let local_contacts = db::contacts::list_contacts(&conn, &book_id)?;
                let mut local_by_uid: std::collections::HashMap<String, Contact> = local_contacts
                    .into_iter()
                    .filter_map(|c| c.uid.clone().map(|uid| (uid, c)))
                    .collect();

                // Upsert server contacts
                for sc in &server_contacts {
                    let parsed = parse_vcard(&sc.vcard_data);

                    let emails_json =
                        serde_json::to_string(&parsed.emails).unwrap_or_else(|_| "[]".to_string());
                    let phones_json =
                        serde_json::to_string(&parsed.phones).unwrap_or_else(|_| "[]".to_string());

                    if let Some(existing) = local_by_uid.remove(&sc.uid) {
                        // Update if etag changed
                        if existing.etag.as_deref() != Some(&sc.etag) {
                            let updated = Contact {
                                display_name: parsed.display_name,
                                emails_json,
                                phones_json,
                                organization: parsed.organization,
                                title: parsed.title,
                                notes: parsed.note,
                                vcard_data: Some(sc.vcard_data.clone()),
                                etag: Some(sc.etag.clone()),
                                remote_id: Some(sc.href.clone()),
                                ..existing
                            };
                            db::contacts::update_contact(&conn, &updated)?;
                        }
                    } else {
                        // New contact from server
                        let contact = Contact {
                            id: uuid::Uuid::new_v4().to_string(),
                            book_id: book_id.clone(),
                            uid: Some(sc.uid.clone()),
                            display_name: parsed.display_name,
                            emails_json,
                            phones_json,
                            addresses_json: "[]".to_string(),
                            organization: parsed.organization,
                            title: parsed.title,
                            notes: parsed.note,
                            vcard_data: Some(sc.vcard_data.clone()),
                            remote_id: Some(sc.href.clone()),
                            etag: Some(sc.etag.clone()),
                        };
                        db::contacts::insert_contact(&conn, &contact)?;
                    }
                }

                // Remove contacts deleted on server
                let deleted: usize = local_by_uid.len();
                for orphan in local_by_uid.values() {
                    // Only delete if it had a remote_id (was synced from server)
                    if orphan.remote_id.is_some() {
                        db::contacts::delete_contact(&conn, &orphan.id)?;
                    }
                }
                if deleted > 0 {
                    log::info!(
                        "sync_contacts_carddav: removed {} server-deleted contacts from '{}'",
                        deleted,
                        ab.name
                    );
                }
            }

            log::info!(
                "sync_contacts_carddav: completed for account {}",
                account_id
            );
            Ok(())
        }
        .await;

        if let Err(e) = result {
            log::warn!("sync_contacts: CardDAV failed for {}: {}", account_id, e);
        }
        Ok(())
    }

    /// PUT the vCard into the book's collection. `Ok(None)` when the
    /// book has no collection href (nothing to push to).
    async fn push_created_contact(
        &self,
        ctx: &ContactBackendCtx<'_>,
        account: &AccountFull,
        book: &BookRef<'_>,
        contact: &Contact,
    ) -> Result<Option<PushedContact>> {
        let Some(href) = book.remote_id else {
            return Ok(None);
        };
        let client = connect(ctx, account).await?;
        let uid = contact.uid.as_deref().unwrap_or(&contact.id);
        let vcard = contact_to_vcard(uid, contact);
        let etag = client.put_contact(href, uid, &vcard).await?;
        let remote_id = format!("{}/{}.vcf", href.trim_end_matches('/'), uid);
        Ok(Some(PushedContact {
            remote_id: Some(remote_id),
            etag: Some(etag),
            vcard: Some(vcard),
        }))
    }

    async fn push_updated_contact(
        &self,
        ctx: &ContactBackendCtx<'_>,
        account: &AccountFull,
        book: &BookRef<'_>,
        contact: &Contact,
        _remote_id: &str,
    ) -> Result<Option<PushedContact>> {
        let client = connect(ctx, account).await?;
        let uid = contact.uid.as_deref().unwrap_or(&contact.id);
        let book_href = book.remote_id.unwrap_or_default();
        let vcard = contact_to_vcard(uid, contact);
        let etag = client.put_contact(book_href, uid, &vcard).await?;
        Ok(Some(PushedContact {
            remote_id: None,
            etag: Some(etag),
            vcard: Some(vcard),
        }))
    }

    async fn push_deleted_contact(
        &self,
        ctx: &ContactBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
    ) -> Result<()> {
        let client = connect(ctx, account).await?;
        client.delete_contact(remote_id).await
    }
}
