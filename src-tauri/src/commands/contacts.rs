use rusqlite::OptionalExtension;
use serde::Deserialize;
use tauri::State;

use crate::backend::contacts::{ContactBackend, PushedContact};
use crate::contact::Contact;
use crate::db;
use crate::db::accounts::AccountFull;
use crate::db::contacts::{CollectedContact, ContactBook};
use crate::error::{Error, Result};
use crate::state::AppState;

fn backend_ctx(state: &AppState) -> crate::backend::contacts::ContactBackendCtx<'_> {
    crate::backend::contacts::ContactBackendCtx {
        db: &state.db,
        providers: &state.providers,
    }
}

fn load_contact_book(conn: &rusqlite::Connection, book_id: &str) -> Result<ContactBook> {
    conn.query_row(
        "SELECT id, account_id, name, remote_id, sync_type
         FROM contact_books WHERE id = ?1",
        rusqlite::params![book_id],
        |row| {
            Ok(ContactBook {
                id: row.get(0)?,
                account_id: row.get(1)?,
                name: row.get(2)?,
                remote_id: row.get(3)?,
                sync_type: row.get(4)?,
            })
        },
    )
    .map_err(|error| match error {
        rusqlite::Error::QueryReturnedNoRows => {
            Error::Other(format!("Contact book not found: {book_id}"))
        }
        other => Error::Database(other),
    })
}

fn load_contact_account_id(
    conn: &rusqlite::Connection,
    contact_id: &str,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT cb.account_id
         FROM contacts c
         LEFT JOIN contact_books cb ON cb.id = c.book_id
         WHERE c.id = ?1",
        rusqlite::params![contact_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(Error::Database)
}

fn non_empty_remote_id(remote_id: Option<&str>) -> Option<&str> {
    remote_id.filter(|remote_id| !remote_id.trim().is_empty())
}

fn created_remote_id(pushed: &PushedContact) -> Option<&str> {
    non_empty_remote_id(pushed.remote_id.as_deref())
}

fn another_contact_uses_remote_id(
    conn: &rusqlite::Connection,
    contact_id: &str,
    book_id: &str,
    remote_id: &str,
) -> Result<bool> {
    let count = conn.query_row(
        "SELECT COUNT(*) FROM contacts
         WHERE book_id = ?1 AND remote_id = ?2 AND id != ?3",
        rusqlite::params![book_id, remote_id, contact_id],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(count > 0)
}

fn merge_contact_update(mut replacement: Contact, persisted: &Contact) -> Result<Contact> {
    if replacement.book_id != persisted.book_id
        && non_empty_remote_id(persisted.remote_id.as_deref()).is_some()
    {
        return Err(Error::Other(format!(
            "Remote-backed contact {} cannot be moved to another contact book",
            persisted.id
        )));
    }

    replacement.uid = persisted.uid.clone();
    replacement.remote_id = persisted.remote_id.clone();
    replacement.etag = persisted.etag.clone();
    replacement.vcard_data = persisted.vcard_data.clone();
    Ok(replacement)
}

fn persist_pushed_metadata(
    conn: &rusqlite::Connection,
    contact_id: &str,
    pushed: &PushedContact,
) -> Result<()> {
    let updated = conn.execute(
        "UPDATE contacts SET remote_id = COALESCE(?1, remote_id),
                 etag = COALESCE(?2, etag),
                 vcard_data = COALESCE(?3, vcard_data)
         WHERE id = ?4",
        rusqlite::params![
            pushed.remote_id.as_deref(),
            pushed.etag.as_deref(),
            pushed.vcard.as_deref(),
            contact_id
        ],
    )?;
    if updated != 1 {
        return Err(Error::Other(format!(
            "Contact not found while persisting provider metadata: {contact_id}"
        )));
    }
    Ok(())
}

async fn push_created_contact_best_effort(
    state: &AppState,
    backend: &dyn ContactBackend,
    account: &AccountFull,
    book: &ContactBook,
    contact: &Contact,
    operation: &str,
) {
    let book_ref = crate::backend::contacts::BookRef {
        remote_id: book.remote_id.as_deref(),
    };
    match backend
        .push_created_contact(&backend_ctx(state), account, &book_ref, contact)
        .await
    {
        Ok(Some(pushed)) => {
            let Some(remote_id) = created_remote_id(&pushed) else {
                log::error!(
                    "{}: {} create returned missing or blank remote_id for contact {}",
                    operation,
                    backend.protocol(),
                    contact.id
                );
                return;
            };

            let persistence = {
                let conn = state.db.writer().await;
                persist_pushed_metadata(&conn, &contact.id, &pushed)
            };
            if let Err(error) = persistence {
                log::error!(
                    "{}: failed to persist {} create metadata for contact {}: {}",
                    operation,
                    backend.protocol(),
                    contact.id,
                    error
                );
                match backend
                    .push_deleted_contact(&backend_ctx(state), account, remote_id)
                    .await
                {
                    Ok(()) => log::warn!(
                        "{}: compensating {} delete succeeded for {}",
                        operation,
                        backend.protocol(),
                        remote_id
                    ),
                    Err(cleanup_error) => log::error!(
                        "{}: compensating {} delete failed for {}: {}",
                        operation,
                        backend.protocol(),
                        remote_id,
                        cleanup_error
                    ),
                }
                return;
            }

            log::info!(
                "{}: pushed via {}, remote_id={}",
                operation,
                backend.protocol(),
                remote_id
            );
        }
        Ok(None) => {}
        Err(error) => log::warn!(
            "{}: {} create push failed: {}",
            operation,
            backend.protocol(),
            error
        ),
    }
}

fn ordered_account_ids(left: String, right: String) -> (String, Option<String>) {
    if left == right {
        (left, None)
    } else if left < right {
        (left, Some(right))
    } else {
        (right, Some(left))
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
    let target_book_id = contact.book_id.clone();
    let account_id = {
        let conn = state.db.reader();
        load_contact_book(&conn, &target_book_id)?.account_id
    };
    let account_lock = state.account_lifecycle.acquire(&account_id);
    let _account_guard = account_lock.lock().await;

    let book = {
        let conn = state.db.reader();
        load_contact_book(&conn, &target_book_id)?
    };
    if book.account_id != account_id {
        return Err(Error::Other(format!(
            "Contact book {} changed accounts while waiting to create a contact; retry the create",
            book.id
        )));
    }

    let backend = crate::backend::contacts::for_sync_type(&book.sync_type);
    let account = if backend.is_some() {
        let conn = state.db.reader();
        Some(db::accounts::get_account_full(&conn, &book.account_id)?)
    } else {
        None
    };

    let id = uuid::Uuid::new_v4().to_string();
    let c = Contact {
        id: id.clone(),
        book_id: book.id.clone(),
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
    {
        let conn = state.db.writer().await;
        db::contacts::insert_contact(&conn, &c)?;
    }
    log::info!("Created contact {} '{}'", id, c.display_name);

    // Push to the book's provider, if it is a synced book. Best-effort:
    // the local insert above always stands; JMAP books return Ok(None)
    // here and push during the next sync instead.
    if let (Some(backend), Some(account)) = (backend, account.as_ref()) {
        push_created_contact_best_effort(&state, backend, account, &book, &c, "create_contact")
            .await;
    }

    Ok(id)
}

#[tauri::command]
pub async fn update_contact(state: State<'_, AppState>, contact: Contact) -> Result<()> {
    let (source_account_id, target_account_id) = {
        let conn = state.db.reader();
        let persisted = db::contacts::get_contact(&conn, &contact.id)?;
        let source_book = load_contact_book(&conn, &persisted.book_id)?;
        let target_book = if persisted.book_id == contact.book_id {
            source_book.clone()
        } else {
            load_contact_book(&conn, &contact.book_id)?
        };
        (source_book.account_id, target_book.account_id)
    };
    let (first_account_id, second_account_id) =
        ordered_account_ids(source_account_id, target_account_id);
    let first_account_lock = state.account_lifecycle.acquire(&first_account_id);
    let second_account_lock = second_account_id
        .as_deref()
        .map(|account_id| state.account_lifecycle.acquire(account_id));
    let _first_account_guard = first_account_lock.lock().await;
    let _second_account_guard = match second_account_lock.as_ref() {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };

    let (persisted, source_book, target_book) = {
        let conn = state.db.reader();
        let persisted = db::contacts::get_contact(&conn, &contact.id)?;
        let source_book = load_contact_book(&conn, &persisted.book_id)?;
        let target_book = if persisted.book_id == contact.book_id {
            source_book.clone()
        } else {
            load_contact_book(&conn, &contact.book_id)?
        };
        (persisted, source_book, target_book)
    };
    let account_is_locked = |account_id: &str| {
        account_id == first_account_id || second_account_id.as_deref() == Some(account_id)
    };
    if !account_is_locked(&source_book.account_id) || !account_is_locked(&target_book.account_id) {
        return Err(Error::Other(format!(
            "Contact or contact book changed accounts while waiting to update {}; retry the update",
            persisted.id
        )));
    }

    let updated = merge_contact_update(contact, &persisted)?;
    let remote_id = non_empty_remote_id(updated.remote_id.as_deref()).map(str::to_owned);
    let backend = crate::backend::contacts::for_sync_type(&target_book.sync_type);
    let account = if backend.is_some() {
        let conn = state.db.reader();
        Some(db::accounts::get_account_full(
            &conn,
            &target_book.account_id,
        )?)
    } else {
        None
    };

    {
        let conn = state.db.writer().await;
        db::contacts::update_contact(&conn, &updated)?;
    }
    log::info!("Updated contact {}", updated.id);

    // Push to the book's provider, if it is a synced book. Best-effort:
    // the local update above always stands.
    if let (Some(backend), Some(account)) = (backend, account.as_ref()) {
        if let Some(remote_id) = remote_id.as_deref() {
            let book_ref = crate::backend::contacts::BookRef {
                remote_id: target_book.remote_id.as_deref(),
            };
            match backend
                .push_updated_contact(
                    &backend_ctx(&state),
                    account,
                    &book_ref,
                    &updated,
                    remote_id,
                )
                .await
            {
                Ok(pushed) => {
                    if let Some(pushed) = pushed {
                        let persistence = {
                            let conn = state.db.writer().await;
                            persist_pushed_metadata(&conn, &updated.id, &pushed)
                        };
                        persistence?;
                    }
                    log::info!(
                        "update_contact: pushed via {}: {}",
                        backend.protocol(),
                        remote_id
                    );
                }
                Err(e) => log::warn!("update_contact: {} push failed: {}", backend.protocol(), e),
            }
        } else {
            push_created_contact_best_effort(
                &state,
                backend,
                account,
                &target_book,
                &updated,
                "update_contact",
            )
            .await;
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn delete_contact(state: State<'_, AppState>, contact_id: String) -> Result<()> {
    let account_id = {
        let conn = state.db.reader();
        let Some(account_id) = load_contact_account_id(&conn, &contact_id)? else {
            return Ok(());
        };
        account_id
    };
    let account_lock = state.account_lifecycle.acquire(&account_id);
    let _account_guard = account_lock.lock().await;

    let authoritative_account_id = {
        let conn = state.db.reader();
        let Some(account_id) = load_contact_account_id(&conn, &contact_id)? else {
            return Ok(());
        };
        account_id
    };
    if authoritative_account_id != account_id {
        return Err(Error::Other(format!(
            "Contact {} changed accounts while waiting to delete; retry the delete",
            contact_id
        )));
    }

    let (contact, book) = {
        let conn = state.db.reader();
        let contact = db::contacts::get_contact(&conn, &contact_id)?;
        let book = load_contact_book(&conn, &contact.book_id)?;
        (contact, book)
    };
    if book.account_id != account_id {
        return Err(Error::Other(format!(
            "Contact {} changed accounts while waiting to delete; retry the delete",
            contact.id
        )));
    }

    let remote_id = non_empty_remote_id(contact.remote_id.as_deref()).map(str::to_owned);
    let delete_remote = match remote_id.as_deref() {
        Some(remote_id) => {
            let conn = state.db.reader();
            !another_contact_uses_remote_id(&conn, &contact.id, &book.id, remote_id)?
        }
        None => false,
    };
    let backend = delete_remote
        .then(|| crate::backend::contacts::for_sync_type(&book.sync_type))
        .flatten();
    let account = if backend.is_some() {
        let conn = state.db.reader();
        Some(db::accounts::get_account_full(&conn, &book.account_id)?)
    } else {
        None
    };

    {
        let conn = state.db.writer().await;
        db::contacts::delete_contact(&conn, &contact_id)?;
    }
    log::info!("Deleted contact {}", contact_id);

    if remote_id.is_some() && !delete_remote {
        log::warn!(
            "delete_contact: preserved the remote contact because another row in book {} uses \
             the same remote ID",
            book.id
        );
    }

    // Delete on the book's provider. Best-effort: the local delete
    // above already happened.
    if let (Some(remote_id), Some(backend), Some(account)) =
        (remote_id.as_deref(), backend, account.as_ref())
    {
        match backend
            .push_deleted_contact(&backend_ctx(&state), account, remote_id)
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

    {
        let account_lock = state.account_lifecycle.acquire(&account_id);
        let _account_guard = account_lock.lock().await;
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
        let conn = state.db.writer().await;
        if let Err(error) =
            db::service_bindings::apply_default_contact_book_if_missing(&conn, &account_id)
        {
            log::warn!(
                "sync_contacts: apply_default_contact_book_if_missing failed for {}: {}",
                account_id,
                error
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stored_contact() -> Contact {
        Contact {
            id: "contact-1".into(),
            book_id: "book-a".into(),
            uid: Some("stored-uid".into()),
            display_name: "Stored Name".into(),
            emails_json: "[]".into(),
            phones_json: "[]".into(),
            addresses_json: "[]".into(),
            organization: None,
            title: None,
            notes: None,
            vcard_data: Some("stored-vcard".into()),
            remote_id: Some("stored-remote".into()),
            etag: Some("stored-etag".into()),
        }
    }

    fn pushed_contact(remote_id: Option<&str>) -> PushedContact {
        PushedContact {
            remote_id: remote_id.map(str::to_owned),
            etag: Some("etag-1".into()),
            vcard: Some("vcard-1".into()),
        }
    }

    #[test]
    fn contact_account_lookup_is_optional_only_for_absent_contacts() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE contact_books (
                 id TEXT PRIMARY KEY,
                 account_id TEXT NOT NULL
             );
             CREATE TABLE contacts (
                 id TEXT PRIMARY KEY,
                 book_id TEXT NOT NULL
             );",
        )
        .unwrap();

        assert_eq!(load_contact_account_id(&conn, "missing").unwrap(), None);

        conn.execute(
            "INSERT INTO contact_books (id, account_id) VALUES ('book-1', 'account-1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO contacts (id, book_id) VALUES ('contact-1', 'book-1')",
            [],
        )
        .unwrap();
        assert_eq!(
            load_contact_account_id(&conn, "contact-1").unwrap(),
            Some("account-1".into())
        );

        conn.execute("DELETE FROM contact_books WHERE id = 'book-1'", [])
            .unwrap();
        assert!(matches!(
            load_contact_account_id(&conn, "contact-1"),
            Err(Error::Database(_))
        ));
    }

    #[test]
    fn create_push_accepts_nonblank_remote_metadata() {
        let pushed = pushed_contact(Some("remote-1"));

        assert_eq!(created_remote_id(&pushed), Some("remote-1"));
    }

    #[test]
    fn create_push_rejects_missing_or_blank_remote_metadata() {
        for remote_id in [None, Some(""), Some(" \t\n")] {
            let pushed = pushed_contact(remote_id);

            assert!(created_remote_id(&pushed).is_none());
        }
    }

    #[test]
    fn remote_delete_is_skipped_only_for_duplicates_in_the_same_book() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE contacts (
                 id TEXT PRIMARY KEY,
                 book_id TEXT NOT NULL,
                 remote_id TEXT
             );
             INSERT INTO contacts (id, book_id, remote_id) VALUES
                 ('target', 'book-a', 'remote-1'),
                 ('duplicate', 'book-a', 'remote-1'),
                 ('other-book', 'book-b', 'remote-1');",
        )
        .unwrap();

        assert!(another_contact_uses_remote_id(&conn, "target", "book-a", "remote-1").unwrap());

        conn.execute("DELETE FROM contacts WHERE id = 'duplicate'", [])
            .unwrap();
        assert!(!another_contact_uses_remote_id(&conn, "target", "book-a", "remote-1").unwrap());
    }

    #[test]
    fn renderer_update_preserves_backend_owned_fields() {
        let persisted = stored_contact();
        let mut replacement = persisted.clone();
        replacement.display_name = "Edited Name".into();
        replacement.uid = Some("renderer-uid".into());
        replacement.remote_id = Some("renderer-remote".into());
        replacement.etag = Some("renderer-etag".into());
        replacement.vcard_data = Some("renderer-vcard".into());

        let merged = merge_contact_update(replacement, &persisted).unwrap();

        assert_eq!(merged.display_name, "Edited Name");
        assert_eq!(merged.uid, persisted.uid);
        assert_eq!(merged.remote_id, persisted.remote_id);
        assert_eq!(merged.etag, persisted.etag);
        assert_eq!(merged.vcard_data, persisted.vcard_data);
    }

    #[test]
    fn remote_backed_contact_move_is_rejected() {
        let persisted = stored_contact();
        let mut replacement = persisted.clone();
        replacement.book_id = "book-b".into();

        let error = merge_contact_update(replacement, &persisted).unwrap_err();

        assert!(error
            .to_string()
            .contains("Remote-backed contact contact-1 cannot be moved"));
    }

    #[test]
    fn local_only_contact_move_is_allowed() {
        let mut persisted = stored_contact();
        persisted.remote_id = None;
        let mut replacement = persisted.clone();
        replacement.book_id = "book-b".into();
        replacement.uid = Some("renderer-uid".into());

        let merged = merge_contact_update(replacement, &persisted).unwrap();

        assert_eq!(merged.book_id, "book-b");
        assert_eq!(merged.uid.as_deref(), Some("stored-uid"));
    }

    #[test]
    fn account_lock_order_is_lexical_and_deduplicated() {
        assert_eq!(
            ordered_account_ids("z-account".into(), "a-account".into()),
            ("a-account".into(), Some("z-account".into()))
        );
        assert_eq!(
            ordered_account_ids("same".into(), "same".into()),
            ("same".into(), None)
        );
    }
}

// ---------------------------------------------------------------------------
// Microsoft Graph contacts sync
// ---------------------------------------------------------------------------
