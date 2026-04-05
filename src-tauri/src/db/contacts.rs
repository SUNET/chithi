use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::Result;

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactBook {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub remote_id: Option<String>,
    pub sync_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub id: String,
    pub book_id: String,
    pub uid: Option<String>,
    pub display_name: String,
    pub emails_json: String,
    pub phones_json: String,
    pub addresses_json: String,
    pub organization: Option<String>,
    pub title: Option<String>,
    pub notes: Option<String>,
    pub vcard_data: Option<String>,
    pub remote_id: Option<String>,
    pub etag: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectedContact {
    pub id: i64,
    pub account_id: String,
    pub email: String,
    pub name: Option<String>,
    pub last_used: String,
    pub use_count: i64,
}

// ---------------------------------------------------------------------------
// Contact Books CRUD
// ---------------------------------------------------------------------------

pub fn list_contact_books(conn: &Connection, account_id: &str) -> Result<Vec<ContactBook>> {
    let mut stmt = conn.prepare(
        "SELECT id, account_id, name, remote_id, sync_type
         FROM contact_books WHERE account_id = ?1 ORDER BY name",
    )?;
    let rows = stmt
        .query_map(params![account_id], |row| {
            Ok(ContactBook {
                id: row.get(0)?,
                account_id: row.get(1)?,
                name: row.get(2)?,
                remote_id: row.get(3)?,
                sync_type: row.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn insert_contact_book(conn: &Connection, book: &ContactBook) -> Result<()> {
    conn.execute(
        "INSERT INTO contact_books (id, account_id, name, remote_id, sync_type)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![book.id, book.account_id, book.name, book.remote_id, book.sync_type],
    )?;
    Ok(())
}

pub fn get_or_create_collected_book(conn: &Connection, account_id: &str) -> Result<String> {
    // Check if a "Collected Contacts" book already exists
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM contact_books WHERE account_id = ?1 AND sync_type = 'local' AND name = 'Collected Contacts'",
            params![account_id],
            |row| row.get(0),
        )
        .ok();

    if let Some(id) = existing {
        return Ok(id);
    }

    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO contact_books (id, account_id, name, sync_type) VALUES (?1, ?2, 'Collected Contacts', 'local')",
        params![id, account_id],
    )?;
    Ok(id)
}

// ---------------------------------------------------------------------------
// Contacts CRUD
// ---------------------------------------------------------------------------

pub fn list_contacts(conn: &Connection, book_id: &str) -> Result<Vec<Contact>> {
    let mut stmt = conn.prepare(
        "SELECT id, book_id, uid, display_name, emails_json, phones_json, addresses_json,
                organization, title, notes, vcard_data, remote_id, etag
         FROM contacts WHERE book_id = ?1 ORDER BY display_name",
    )?;
    let rows = stmt
        .query_map(params![book_id], map_contact_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn get_contact(conn: &Connection, id: &str) -> Result<Contact> {
    conn.query_row(
        "SELECT id, book_id, uid, display_name, emails_json, phones_json, addresses_json,
                organization, title, notes, vcard_data, remote_id, etag
         FROM contacts WHERE id = ?1",
        params![id],
        map_contact_row,
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => {
            crate::error::Error::Other(format!("Contact not found: {}", id))
        }
        other => crate::error::Error::Database(other),
    })
}

pub fn insert_contact(conn: &Connection, contact: &Contact) -> Result<()> {
    conn.execute(
        "INSERT INTO contacts (id, book_id, uid, display_name, emails_json, phones_json,
         addresses_json, organization, title, notes, vcard_data, remote_id, etag)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            contact.id,
            contact.book_id,
            contact.uid,
            contact.display_name,
            contact.emails_json,
            contact.phones_json,
            contact.addresses_json,
            contact.organization,
            contact.title,
            contact.notes,
            contact.vcard_data,
            contact.remote_id,
            contact.etag,
        ],
    )?;
    Ok(())
}

pub fn update_contact(conn: &Connection, contact: &Contact) -> Result<()> {
    let rows = conn.execute(
        "UPDATE contacts SET book_id=?1, uid=?2, display_name=?3, emails_json=?4,
         phones_json=?5, addresses_json=?6, organization=?7, title=?8, notes=?9,
         vcard_data=?10, remote_id=?11, etag=?12, updated_at=CURRENT_TIMESTAMP
         WHERE id=?13",
        params![
            contact.book_id,
            contact.uid,
            contact.display_name,
            contact.emails_json,
            contact.phones_json,
            contact.addresses_json,
            contact.organization,
            contact.title,
            contact.notes,
            contact.vcard_data,
            contact.remote_id,
            contact.etag,
            contact.id,
        ],
    )?;
    if rows == 0 {
        return Err(crate::error::Error::Other(format!(
            "Contact not found: {}",
            contact.id
        )));
    }
    Ok(())
}

pub fn delete_contact(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM contacts WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn search_contacts(conn: &Connection, account_id: &str, query: &str) -> Result<Vec<Contact>> {
    let pattern = format!("%{}%", query);
    let mut stmt = conn.prepare(
        "SELECT c.id, c.book_id, c.uid, c.display_name, c.emails_json, c.phones_json,
                c.addresses_json, c.organization, c.title, c.notes, c.vcard_data,
                c.remote_id, c.etag
         FROM contacts c
         JOIN contact_books cb ON c.book_id = cb.id
         WHERE cb.account_id = ?1
           AND (c.display_name LIKE ?2 OR c.emails_json LIKE ?2
                OR c.phones_json LIKE ?2 OR c.organization LIKE ?2)
         ORDER BY c.display_name
         LIMIT 50",
    )?;
    let rows = stmt
        .query_map(params![account_id, pattern], map_contact_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Search contacts across ALL accounts for compose autocomplete.
pub fn search_all_contacts(conn: &Connection, query: &str) -> Result<Vec<Contact>> {
    let pattern = format!("%{}%", query);
    let mut stmt = conn.prepare(
        "SELECT c.id, c.book_id, c.uid, c.display_name, c.emails_json, c.phones_json,
                c.addresses_json, c.organization, c.title, c.notes, c.vcard_data,
                c.remote_id, c.etag
         FROM contacts c
         WHERE c.display_name LIKE ?1 OR c.emails_json LIKE ?1
         ORDER BY c.display_name
         LIMIT 50",
    )?;
    let rows = stmt
        .query_map(params![pattern], map_contact_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Collected Contacts
// ---------------------------------------------------------------------------

/// Upsert a collected contact — increment use_count if exists, insert if not.
pub fn collect_contact(conn: &Connection, account_id: &str, email: &str, name: Option<&str>) -> Result<()> {
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM collected_contacts WHERE account_id = ?1 AND email = ?2",
            params![account_id, email],
            |row| row.get(0),
        )
        .ok();

    if let Some(id) = existing {
        conn.execute(
            "UPDATE collected_contacts SET use_count = use_count + 1, last_used = CURRENT_TIMESTAMP, name = COALESCE(?1, name) WHERE id = ?2",
            params![name, id],
        )?;
    } else {
        conn.execute(
            "INSERT INTO collected_contacts (account_id, email, name) VALUES (?1, ?2, ?3)",
            params![account_id, email, name],
        )?;
    }
    Ok(())
}

/// Search collected contacts for autocomplete, ordered by use_count desc.
pub fn search_collected_contacts(conn: &Connection, query: &str) -> Result<Vec<CollectedContact>> {
    let pattern = format!("%{}%", query);
    let mut stmt = conn.prepare(
        "SELECT id, account_id, email, name, last_used, use_count
         FROM collected_contacts
         WHERE email LIKE ?1 OR name LIKE ?1
         ORDER BY use_count DESC, last_used DESC
         LIMIT 20",
    )?;
    let rows = stmt
        .query_map(params![pattern], |row| {
            Ok(CollectedContact {
                id: row.get(0)?,
                account_id: row.get(1)?,
                email: row.get(2)?,
                name: row.get(3)?,
                last_used: row.get(4)?,
                use_count: row.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn map_contact_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Contact> {
    Ok(Contact {
        id: row.get(0)?,
        book_id: row.get(1)?,
        uid: row.get(2)?,
        display_name: row.get(3)?,
        emails_json: row.get(4)?,
        phones_json: row.get(5)?,
        addresses_json: row.get(6)?,
        organization: row.get(7)?,
        title: row.get(8)?,
        notes: row.get(9)?,
        vcard_data: row.get(10)?,
        remote_id: row.get(11)?,
        etag: row.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE accounts (id TEXT PRIMARY KEY, display_name TEXT NOT NULL, email TEXT NOT NULL, provider TEXT NOT NULL, mail_protocol TEXT NOT NULL DEFAULT 'imap', username TEXT NOT NULL, use_tls INTEGER NOT NULL DEFAULT 1, enabled INTEGER NOT NULL DEFAULT 1, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, imap_host TEXT NOT NULL DEFAULT '', imap_port INTEGER NOT NULL DEFAULT 993, smtp_host TEXT NOT NULL DEFAULT '', smtp_port INTEGER NOT NULL DEFAULT 587, jmap_url TEXT NOT NULL DEFAULT '', caldav_url TEXT NOT NULL DEFAULT '');
            INSERT INTO accounts (id, display_name, email, provider, username) VALUES ('acc1', 'Test', 'test@example.com', 'generic', 'user');

            CREATE TABLE contact_books (id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, name TEXT NOT NULL, remote_id TEXT, sync_type TEXT NOT NULL DEFAULT 'local', created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);

            CREATE TABLE contacts (id TEXT PRIMARY KEY, book_id TEXT NOT NULL REFERENCES contact_books(id) ON DELETE CASCADE, uid TEXT, display_name TEXT NOT NULL, emails_json TEXT DEFAULT '[]', phones_json TEXT DEFAULT '[]', addresses_json TEXT DEFAULT '[]', organization TEXT, title TEXT, notes TEXT, vcard_data TEXT, remote_id TEXT, etag TEXT, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
            CREATE INDEX idx_contacts_book ON contacts(book_id);
            CREATE INDEX idx_contacts_name ON contacts(display_name);

            CREATE TABLE collected_contacts (id INTEGER PRIMARY KEY AUTOINCREMENT, account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, email TEXT NOT NULL, name TEXT, last_used TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, use_count INTEGER NOT NULL DEFAULT 1, UNIQUE(account_id, email));
            CREATE INDEX idx_collected_email ON collected_contacts(email);
            ",
        ).unwrap();
        conn
    }

    fn make_contact(id: &str, name: &str, book_id: &str) -> Contact {
        Contact {
            id: id.to_string(),
            book_id: book_id.to_string(),
            uid: None,
            display_name: name.to_string(),
            emails_json: format!("[{{\"email\":\"{}@example.com\",\"label\":\"work\"}}]", name.to_lowercase().replace(' ', ".")),
            phones_json: "[]".to_string(),
            addresses_json: "[]".to_string(),
            organization: None,
            title: None,
            notes: None,
            vcard_data: None,
            remote_id: None,
            etag: None,
        }
    }

    #[test]
    fn test_contact_book_crud() {
        let conn = setup_db();
        let book = ContactBook {
            id: "book1".to_string(),
            account_id: "acc1".to_string(),
            name: "Work".to_string(),
            remote_id: None,
            sync_type: "local".to_string(),
        };
        insert_contact_book(&conn, &book).unwrap();

        let books = list_contact_books(&conn, "acc1").unwrap();
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].name, "Work");
    }

    #[test]
    fn test_get_or_create_collected_book() {
        let conn = setup_db();
        let id1 = get_or_create_collected_book(&conn, "acc1").unwrap();
        let id2 = get_or_create_collected_book(&conn, "acc1").unwrap();
        assert_eq!(id1, id2, "Should return same book on second call");
    }

    #[test]
    fn test_contact_crud() {
        let conn = setup_db();
        let book = ContactBook {
            id: "book1".to_string(),
            account_id: "acc1".to_string(),
            name: "Personal".to_string(),
            remote_id: None,
            sync_type: "local".to_string(),
        };
        insert_contact_book(&conn, &book).unwrap();

        let contact = make_contact("c1", "Alice Smith", "book1");
        insert_contact(&conn, &contact).unwrap();

        let fetched = get_contact(&conn, "c1").unwrap();
        assert_eq!(fetched.display_name, "Alice Smith");

        let mut updated = fetched;
        updated.organization = Some("ACME Corp".to_string());
        update_contact(&conn, &updated).unwrap();

        let fetched2 = get_contact(&conn, "c1").unwrap();
        assert_eq!(fetched2.organization, Some("ACME Corp".to_string()));

        delete_contact(&conn, "c1").unwrap();
        assert!(get_contact(&conn, "c1").is_err());
    }

    #[test]
    fn test_search_contacts() {
        let conn = setup_db();
        let book = ContactBook {
            id: "book1".to_string(),
            account_id: "acc1".to_string(),
            name: "Work".to_string(),
            remote_id: None,
            sync_type: "local".to_string(),
        };
        insert_contact_book(&conn, &book).unwrap();

        insert_contact(&conn, &make_contact("c1", "Alice Smith", "book1")).unwrap();
        insert_contact(&conn, &make_contact("c2", "Bob Jones", "book1")).unwrap();
        insert_contact(&conn, &make_contact("c3", "Carol Alice", "book1")).unwrap();

        let results = search_contacts(&conn, "acc1", "alice").unwrap();
        assert_eq!(results.len(), 2); // Alice Smith and Carol Alice

        let results = search_all_contacts(&conn, "bob").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].display_name, "Bob Jones");
    }

    #[test]
    fn test_collect_contact_new() {
        let conn = setup_db();
        collect_contact(&conn, "acc1", "alice@example.com", Some("Alice")).unwrap();

        let results = search_collected_contacts(&conn, "alice").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].email, "alice@example.com");
        assert_eq!(results[0].use_count, 1);
    }

    #[test]
    fn test_collect_contact_increment() {
        let conn = setup_db();
        collect_contact(&conn, "acc1", "alice@example.com", Some("Alice")).unwrap();
        collect_contact(&conn, "acc1", "alice@example.com", None).unwrap();
        collect_contact(&conn, "acc1", "alice@example.com", None).unwrap();

        let results = search_collected_contacts(&conn, "alice").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].use_count, 3);
        assert_eq!(results[0].name, Some("Alice".to_string()));
    }

    #[test]
    fn test_collected_contacts_ranked_by_use() {
        let conn = setup_db();
        collect_contact(&conn, "acc1", "rare@example.com", Some("Rare")).unwrap();
        collect_contact(&conn, "acc1", "frequent@example.com", Some("Frequent")).unwrap();
        // Send to "frequent" 5 more times
        for _ in 0..5 {
            collect_contact(&conn, "acc1", "frequent@example.com", None).unwrap();
        }

        let results = search_collected_contacts(&conn, "example").unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].email, "frequent@example.com", "Most used should be first");
        assert_eq!(results[0].use_count, 6);
    }

    #[test]
    fn test_list_contacts_in_book() {
        let conn = setup_db();
        let book = ContactBook {
            id: "book1".to_string(),
            account_id: "acc1".to_string(),
            name: "Work".to_string(),
            remote_id: None,
            sync_type: "local".to_string(),
        };
        insert_contact_book(&conn, &book).unwrap();

        insert_contact(&conn, &make_contact("c1", "Alice", "book1")).unwrap();
        insert_contact(&conn, &make_contact("c2", "Bob", "book1")).unwrap();

        let contacts = list_contacts(&conn, "book1").unwrap();
        assert_eq!(contacts.len(), 2);
        assert_eq!(contacts[0].display_name, "Alice");
        assert_eq!(contacts[1].display_name, "Bob");
    }
}
