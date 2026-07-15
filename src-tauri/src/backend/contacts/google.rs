//! Google contact backend (People API v1).

use async_trait::async_trait;

use crate::auth::get_google_token;
use crate::db::accounts::AccountFull;
use crate::db::contacts::Contact;
use crate::db::pool::DbPool;
use crate::error::Result;
use crate::mail::google::{contact_to_person_json, GoogleClient};

use super::{BookRef, ContactBackend, PushedContact};

pub struct GoogleContactBackend;

#[async_trait]
impl ContactBackend for GoogleContactBackend {
    fn protocol(&self) -> &'static str {
        "google"
    }

    /// People API sync. Failures are swallowed with a warning — Gmail
    /// accounts without calendar/contacts OAuth consent would
    /// otherwise fail every contacts sync outright.
    async fn sync(&self, db: &DbPool, account: &AccountFull) -> Result<()> {
        let account_id = account.id.as_str();
        let result: Result<()> = async {
            // Get a valid OAuth2 access token
            let access_token = get_google_token(account_id).await?;

            let conn = db.writer().await;
            let book_id = {
                let existing: Option<String> = conn
                    .query_row(
                        "SELECT id FROM contact_books WHERE account_id = ?1 AND sync_type = 'google'",
                        rusqlite::params![account_id],
                        |row| row.get(0),
                    )
                    .ok();

                if let Some(id) = existing {
                    id
                } else {
                    let id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO contact_books (id, account_id, name, sync_type) VALUES (?1, ?2, 'Google Contacts', 'google')",
                        rusqlite::params![id, account_id],
                    )?;
                    id
                }
            };
            drop(conn);

            // Fetch contacts using Google People API (more reliable than CardDAV for Google)
            let client = GoogleClient::new(&access_token);
            let data = client.list_connections().await?;
            let connections = data["connections"].as_array();
            let count = connections.map(|c| c.len()).unwrap_or(0);
            log::info!("sync_contacts_google: fetched {} contacts", count);

            let conn = db.writer().await;

            if let Some(people) = connections {
                for person in people {
                    let resource_name = person["resourceName"].as_str().unwrap_or_default();

                    // Parse name
                    let display_name = person["names"]
                        .as_array()
                        .and_then(|names| names.first())
                        .and_then(|n| n["displayName"].as_str())
                        .unwrap_or("(No name)")
                        .to_string();

                    // Parse emails
                    let mut emails = Vec::new();
                    if let Some(email_list) = person["emailAddresses"].as_array() {
                        for em in email_list {
                            let addr = em["value"].as_str().unwrap_or_default();
                            let label = em["type"].as_str().unwrap_or("other");
                            if !addr.is_empty() {
                                emails.push(serde_json::json!({"email": addr, "label": label}));
                            }
                        }
                    }

                    // Parse phones
                    let mut phones = Vec::new();
                    if let Some(phone_list) = person["phoneNumbers"].as_array() {
                        for ph in phone_list {
                            let number = ph["value"].as_str().unwrap_or_default();
                            let label = ph["type"].as_str().unwrap_or("mobile");
                            if !number.is_empty() {
                                phones.push(serde_json::json!({"number": number, "label": label}));
                            }
                        }
                    }

                    // Parse organization
                    let organization = person["organizations"]
                        .as_array()
                        .and_then(|orgs| orgs.first())
                        .and_then(|o| o["name"].as_str())
                        .map(|s| s.to_string());

                    let title = person["organizations"]
                        .as_array()
                        .and_then(|orgs| orgs.first())
                        .and_then(|o| o["title"].as_str())
                        .map(|s| s.to_string());

                    // Upsert by remote_id
                    let existing: Option<String> = conn
                        .query_row(
                            "SELECT id FROM contacts WHERE book_id = ?1 AND remote_id = ?2",
                            rusqlite::params![book_id, resource_name],
                            |row| row.get(0),
                        )
                        .ok();

                    let emails_json =
                        serde_json::to_string(&emails).unwrap_or_else(|_| "[]".to_string());
                    let phones_json =
                        serde_json::to_string(&phones).unwrap_or_else(|_| "[]".to_string());

                    if let Some(id) = existing {
                        conn.execute(
                            "UPDATE contacts SET display_name=?1, emails_json=?2, phones_json=?3, organization=?4, title=?5, updated_at=CURRENT_TIMESTAMP WHERE id=?6",
                            rusqlite::params![display_name, emails_json, phones_json, organization, title, id],
                        )?;
                    } else {
                        let id = uuid::Uuid::new_v4().to_string();
                        conn.execute(
                            "INSERT INTO contacts (id, book_id, display_name, emails_json, phones_json, addresses_json, organization, title, remote_id) VALUES (?1, ?2, ?3, ?4, ?5, '[]', ?6, ?7, ?8)",
                            rusqlite::params![id, book_id, display_name, emails_json, phones_json, organization, title, resource_name],
                        )?;
                    }
                }
            }

            log::info!("sync_contacts_google: completed for account {}", account_id);
            Ok(())
        }
        .await;

        if let Err(e) = result {
            log::warn!(
                "sync_contacts: Gmail People API failed (OAuth may not be set up): {}",
                e
            );
        }
        Ok(())
    }

    async fn push_created_contact(
        &self,
        account: &AccountFull,
        _book: &BookRef<'_>,
        contact: &Contact,
    ) -> Result<Option<PushedContact>> {
        let token = get_google_token(&account.id).await?;
        let client = GoogleClient::new(&token);
        let person = contact_to_person_json(
            &contact.display_name,
            &contact.emails_json,
            &contact.phones_json,
        );
        let resource_name = client.create_contact(&person).await?;
        Ok(Some(PushedContact {
            remote_id: Some(resource_name),
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
        let token = get_google_token(&account.id).await?;
        let client = GoogleClient::new(&token);
        let person = contact_to_person_json(
            &contact.display_name,
            &contact.emails_json,
            &contact.phones_json,
        );
        client.update_contact(remote_id, &person).await?;
        Ok(None)
    }

    async fn push_deleted_contact(&self, account: &AccountFull, remote_id: &str) -> Result<()> {
        let token = get_google_token(&account.id).await?;
        let client = GoogleClient::new(&token);
        client.delete_contact(remote_id).await
    }
}
