use serde::Deserialize;
use tauri::State;

use crate::db;
use crate::db::contacts::{CollectedContact, Contact, ContactBook};
use crate::error::Result;
use crate::state::AppState;

fn backend_ctx(state: &AppState) -> crate::backend::contacts::ContactBackendCtx<'_> {
    crate::backend::contacts::ContactBackendCtx {
        db: &state.db,
        providers: &state.providers,
    }
}

// ---------------------------------------------------------------------------
// Contact Books
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_contact_books(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Vec<ContactBook>> {
    let conn = state.db.reader();
    db::contacts::list_contact_books(&conn, &account_id)
}

// ---------------------------------------------------------------------------
// Contacts CRUD
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_contacts(state: State<'_, AppState>, book_id: String) -> Result<Vec<Contact>> {
    let conn = state.db.reader();
    db::contacts::list_contacts(&conn, &book_id)
}

#[tauri::command]
pub async fn get_contact(state: State<'_, AppState>, contact_id: String) -> Result<Contact> {
    let conn = state.db.reader();
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
    let conn = state.db.writer().await;
    db::contacts::insert_contact(&conn, &c)?;
    log::info!("Created contact {} '{}'", id, c.display_name);

    // Push to the book's provider, if it is a synced book. Best-effort:
    // the local insert above always stands; JMAP books return Ok(None)
    // here and push during the next sync instead.
    let book = conn
        .query_row(
            "SELECT cb.sync_type, cb.account_id, cb.remote_id FROM contact_books cb WHERE cb.id = ?1",
            rusqlite::params![c.book_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .ok();
    drop(conn);

    if let Some((sync_type, account_id, book_remote_id)) = book {
        if let Some(backend) = crate::backend::contacts::for_sync_type(&sync_type) {
            let account = {
                let conn = state.db.reader();
                db::accounts::get_account_full(&conn, &account_id)?
            };
            let book_ref = crate::backend::contacts::BookRef {
                book_id: &c.book_id,
                remote_id: book_remote_id.as_deref(),
            };
            match backend
                .push_created_contact(&backend_ctx(&state), &account, &book_ref, &c)
                .await
            {
                Ok(Some(pushed)) => {
                    let conn = state.db.writer().await;
                    conn.execute(
                        "UPDATE contacts SET remote_id = COALESCE(?1, remote_id),
                                 etag = COALESCE(?2, etag),
                                 vcard_data = COALESCE(?3, vcard_data)
                         WHERE id = ?4",
                        rusqlite::params![pushed.remote_id, pushed.etag, pushed.vcard, id],
                    )
                    .ok();
                    log::info!(
                        "create_contact: pushed via {}, remote_id={:?}",
                        backend.protocol(),
                        pushed.remote_id
                    );
                }
                Ok(None) => {} // provider defers the push to its next sync
                Err(e) => log::error!("create_contact: {} push failed: {}", backend.protocol(), e),
            }
        }
    }

    Ok(id)
}

#[tauri::command]
pub async fn update_contact(state: State<'_, AppState>, contact: Contact) -> Result<()> {
    let conn = state.db.writer().await;
    db::contacts::update_contact(&conn, &contact)?;
    log::info!("Updated contact {}", contact.id);

    // Push to the book's provider, if it is a synced book. Best-effort:
    // the local update above always stands.
    if let Some(ref remote_id) = contact.remote_id {
        if !remote_id.is_empty() {
            let book_info = conn
                .query_row(
                    "SELECT cb.sync_type, cb.account_id, cb.remote_id FROM contact_books cb WHERE cb.id = ?1",
                    rusqlite::params![contact.book_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .ok();
            drop(conn);

            if let Some((sync_type, account_id, book_remote_id)) = book_info {
                if let Some(backend) = crate::backend::contacts::for_sync_type(&sync_type) {
                    let account = {
                        let conn = state.db.reader();
                        db::accounts::get_account_full(&conn, &account_id)?
                    };
                    let book_ref = crate::backend::contacts::BookRef {
                        book_id: &contact.book_id,
                        remote_id: book_remote_id.as_deref(),
                    };
                    match backend
                        .push_updated_contact(
                            &backend_ctx(&state),
                            &account,
                            &book_ref,
                            &contact,
                            remote_id,
                        )
                        .await
                    {
                        Ok(pushed) => {
                            if let Some(pushed) = pushed {
                                let conn = state.db.writer().await;
                                conn.execute(
                                    "UPDATE contacts SET remote_id = COALESCE(?1, remote_id),
                                             etag = COALESCE(?2, etag),
                                             vcard_data = COALESCE(?3, vcard_data)
                                     WHERE id = ?4",
                                    rusqlite::params![
                                        pushed.remote_id,
                                        pushed.etag,
                                        pushed.vcard,
                                        contact.id
                                    ],
                                )
                                .ok();
                            }
                            log::info!(
                                "update_contact: pushed via {}: {}",
                                backend.protocol(),
                                remote_id
                            );
                        }
                        Err(e) => {
                            log::warn!("update_contact: {} push failed: {}", backend.protocol(), e)
                        }
                    }
                }
            }
            return Ok(());
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn delete_contact(state: State<'_, AppState>, contact_id: String) -> Result<()> {
    // Check if this contact has a Google remote_id before deleting
    let conn = state.db.writer().await;
    let remote_info = conn.query_row(
        "SELECT c.remote_id, cb.sync_type, cb.account_id FROM contacts c JOIN contact_books cb ON c.book_id = cb.id WHERE c.id = ?1",
        rusqlite::params![contact_id],
        |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
    ).ok();
    db::contacts::delete_contact(&conn, &contact_id)?;
    log::info!("Deleted contact {}", contact_id);
    drop(conn);

    // Delete on the book's provider. Best-effort: the local delete
    // above already happened.
    if let Some((Some(remote_id), sync_type, account_id)) = remote_info {
        if !remote_id.is_empty() {
            if let Some(backend) = crate::backend::contacts::for_sync_type(&sync_type) {
                let account = {
                    let conn = state.db.reader();
                    db::accounts::get_account_full(&conn, &account_id)?
                };
                match backend
                    .push_deleted_contact(&backend_ctx(&state), &account, &remote_id)
                    .await
                {
                    Ok(()) => log::info!(
                        "delete_contact: deleted from {}: {}",
                        backend.protocol(),
                        remote_id
                    ),
                    Err(e) => log::warn!(
                        "delete_contact: {} delete failed: {}",
                        backend.protocol(),
                        e
                    ),
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn sync_contacts(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
) -> Result<()> {
    log::info!("sync_contacts: account={}", account_id);

    let account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    // Per-provider sync lives in the backend impls; Google and CardDAV
    // swallow their own errors there (see backend/contacts/).
    match crate::backend::contacts::for_account(&account) {
        Some(backend) => backend.sync(&backend_ctx(&state), &account).await?,
        None => {
            log::debug!(
                "sync_contacts: skipping account {} (no contacts binding)",
                account_id
            );
        }
    }

    // Now that books may exist, fill in default-contact-book on any
    // sibling mail/calendar binding that is still unset (#137). Call
    // is idempotent — bindings the user has already pointed
    // somewhere are left alone.
    {
        let conn = state.db.writer().await;
        if let Err(e) =
            db::service_bindings::apply_default_contact_book_if_missing(&conn, &account_id)
        {
            log::warn!(
                "sync_contacts: apply_default_contact_book_if_missing failed for {}: {}",
                account_id,
                e
            );
        }
    }

    // Notify frontend that contact data has changed
    use tauri::Emitter;
    app.emit("contacts-changed", account_id.as_str()).ok();

    log::info!("sync_contacts: completed for account {}", account_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Search (for compose autocomplete)
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn search_contacts(state: State<'_, AppState>, query: String) -> Result<Vec<Contact>> {
    let conn = state.db.reader();
    db::contacts::search_all_contacts(&conn, &query)
}

/// Like `search_contacts` but resolves the account's default contact
/// book for the given service (`"mail"` for compose, `"calendar"`
/// for event attendees) and ranks matches in that book first. Other
/// books still appear, just below — see #137 for the UX intent.
/// `account_id = None` (or no default configured) degrades to the
/// plain alphabetical search.
#[tauri::command]
pub async fn search_contacts_for_account(
    state: State<'_, AppState>,
    query: String,
    account_id: Option<String>,
    service: Option<String>,
) -> Result<Vec<Contact>> {
    let preferred = match (account_id, service) {
        (Some(aid), Some(svc)) => {
            let conn = state.db.reader();
            db::service_bindings::get_default_contact_book(&conn, &aid, &svc)?
        }
        _ => None,
    };
    let conn = state.db.reader();
    db::contacts::search_all_contacts_ranked(&conn, &query, preferred.as_deref())
}

#[tauri::command]
pub async fn search_collected_contacts(
    state: State<'_, AppState>,
    query: String,
) -> Result<Vec<CollectedContact>> {
    let conn = state.db.reader();
    db::contacts::search_collected_contacts(&conn, &query)
}

/// Read the default contact book id for an account/service binding.
/// Returns null if no default is set or the binding doesn't exist.
/// Used by the settings UI to reflect current state.
/// Validate the `service` argument at the Tauri-command boundary.
/// Default-contact-book is only meaningful for `"mail"` (compose
/// recipient autocomplete) and `"calendar"` (event-attendee
/// autocomplete); anything else is rejected with a clear error
/// rather than silently no-op'd, so a typo'd renderer call surfaces
/// instead of writing junk into another binding's config_json.
fn validate_default_book_service(service: &str) -> Result<()> {
    if matches!(service, "mail" | "calendar") {
        Ok(())
    } else {
        Err(crate::error::Error::Other(format!(
            "default contact book only applies to mail or calendar bindings, got {:?}",
            service
        )))
    }
}

#[tauri::command]
pub async fn get_default_contact_book(
    state: State<'_, AppState>,
    account_id: String,
    service: String,
) -> Result<Option<String>> {
    validate_default_book_service(&service)?;
    let conn = state.db.reader();
    db::service_bindings::get_default_contact_book(&conn, &account_id, &service)
}

/// Set (or clear, when `book_id` is None) the default contact book
/// for an account's mail or calendar binding. The book may belong
/// to a different account than the binding — e.g. a personal CardDAV
/// book can be the default for a work IMAP account's compose
/// autocomplete.
#[tauri::command]
pub async fn set_default_contact_book(
    state: State<'_, AppState>,
    account_id: String,
    service: String,
    book_id: Option<String>,
) -> Result<()> {
    validate_default_book_service(&service)?;
    let conn = state.db.writer().await;
    db::service_bindings::set_default_contact_book(&conn, &account_id, &service, book_id.as_deref())
}

// ---------------------------------------------------------------------------
// Microsoft Graph contacts sync
// ---------------------------------------------------------------------------
