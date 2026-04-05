use serde::Deserialize;
use tauri::State;

use crate::db;
use crate::db::contacts::{CollectedContact, Contact, ContactBook};
use crate::error::Result;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Contact Books
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_contact_books(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Vec<ContactBook>> {
    let conn = state.db.lock().await;
    db::contacts::list_contact_books(&conn, &account_id)
}

// ---------------------------------------------------------------------------
// Contacts CRUD
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_contacts(
    state: State<'_, AppState>,
    book_id: String,
) -> Result<Vec<Contact>> {
    let conn = state.db.lock().await;
    db::contacts::list_contacts(&conn, &book_id)
}

#[tauri::command]
pub async fn get_contact(
    state: State<'_, AppState>,
    contact_id: String,
) -> Result<Contact> {
    let conn = state.db.lock().await;
    db::contacts::get_contact(&conn, &contact_id)
}

#[derive(Debug, Deserialize)]
pub struct NewContactInput {
    pub book_id: String,
    pub display_name: String,
    pub emails_json: String,
    pub phones_json: String,
    pub addresses_json: String,
    pub organization: Option<String>,
    pub title: Option<String>,
    pub notes: Option<String>,
}

#[tauri::command]
pub async fn create_contact(
    state: State<'_, AppState>,
    contact: NewContactInput,
) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let c = Contact {
        id: id.clone(),
        book_id: contact.book_id,
        uid: Some(format!("{}@chithi", uuid::Uuid::new_v4())),
        display_name: contact.display_name,
        emails_json: contact.emails_json,
        phones_json: contact.phones_json,
        addresses_json: contact.addresses_json,
        organization: contact.organization,
        title: contact.title,
        notes: contact.notes,
        vcard_data: None,
        remote_id: None,
        etag: None,
    };
    let conn = state.db.lock().await;
    db::contacts::insert_contact(&conn, &c)?;
    log::info!("Created contact {} '{}'", id, c.display_name);
    Ok(id)
}

#[tauri::command]
pub async fn update_contact(
    state: State<'_, AppState>,
    contact: Contact,
) -> Result<()> {
    let conn = state.db.lock().await;
    db::contacts::update_contact(&conn, &contact)?;
    log::info!("Updated contact {}", contact.id);
    Ok(())
}

#[tauri::command]
pub async fn delete_contact(
    state: State<'_, AppState>,
    contact_id: String,
) -> Result<()> {
    let conn = state.db.lock().await;
    db::contacts::delete_contact(&conn, &contact_id)?;
    log::info!("Deleted contact {}", contact_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn sync_contacts(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<()> {
    log::info!("sync_contacts: account={}", account_id);

    let account = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)?
    };

    if account.mail_protocol == "jmap" {
        sync_contacts_jmap(&state, &account_id, &account).await?;
    } else {
        log::debug!("sync_contacts: skipping non-JMAP account {}", account_id);
    }

    log::info!("sync_contacts: completed for account {}", account_id);
    Ok(())
}

async fn sync_contacts_jmap(
    state: &State<'_, AppState>,
    account_id: &str,
    account: &db::accounts::AccountFull,
) -> Result<()> {
    use crate::mail::jmap::{JmapConfig, JmapConnection};

    let jmap_config = JmapConfig {
        jmap_url: account.jmap_url.clone(),
        email: account.email.clone(),
        username: account.username.clone(),
        password: account.password.clone(),
    };

    let jmap_conn = JmapConnection::connect(&jmap_config).await?;

    // Step 1: Fetch address books
    let address_books = jmap_conn.list_address_books(&jmap_config).await?;
    log::info!("sync_contacts: fetched {} address books from JMAP", address_books.len());

    let mut remote_to_local: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    {
        let conn = state.db.lock().await;
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
                log::error!("sync_contacts: failed to fetch contacts for '{}': {}", ab.name, e);
                continue;
            }
        };

        log::info!("sync_contacts: fetched {} contacts for '{}'", jmap_contacts.len(), ab.name);

        let local_book_id = remote_to_local.get(&ab.id).cloned().unwrap_or_default();
        let conn = state.db.lock().await;

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
                conn.execute("DELETE FROM contacts WHERE id = ?1", rusqlite::params![local_id]).ok();
                deleted += 1;
            }
        }
        if deleted > 0 {
            log::info!("sync_contacts: removed {} server-deleted contacts from '{}'", deleted, ab.name);
        }

        // Push local contacts (no remote_id) to server
        let unpushed: Vec<(String, String, String, String, Option<String>, Option<String>, Option<String>)> = conn
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
            log::info!("sync_contacts: pushing {} local contacts to JMAP for '{}'", unpushed.len(), ab.name);
            drop(conn); // Release lock for async calls

            for (local_id, name, emails, phones, org, title, notes) in &unpushed {
                match jmap_conn.create_contact_card(
                    &jmap_config,
                    &ab.id,
                    name,
                    emails,
                    phones,
                    org.as_deref(),
                    title.as_deref(),
                    notes.as_deref(),
                ).await {
                    Ok(remote_id) => {
                        log::info!("sync_contacts: pushed contact '{}' to JMAP, remote_id={}", name, remote_id);
                        let conn = state.db.lock().await;
                        conn.execute(
                            "UPDATE contacts SET remote_id = ?1 WHERE id = ?2",
                            rusqlite::params![remote_id, local_id],
                        ).ok();
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

// ---------------------------------------------------------------------------
// Search (for compose autocomplete)
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn search_contacts(
    state: State<'_, AppState>,
    query: String,
) -> Result<Vec<Contact>> {
    let conn = state.db.lock().await;
    db::contacts::search_all_contacts(&conn, &query)
}

#[tauri::command]
pub async fn search_collected_contacts(
    state: State<'_, AppState>,
    query: String,
) -> Result<Vec<CollectedContact>> {
    let conn = state.db.lock().await;
    db::contacts::search_collected_contacts(&conn, &query)
}
