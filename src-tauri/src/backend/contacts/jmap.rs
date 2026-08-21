//! JMAP Contacts backend (RFC 9610, with RFC 9553 JSContact cards).

use std::collections::{BTreeMap, HashSet};

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::contact::Contact;
use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::mail::jmap::{JmapAddressBook, JmapContact};

use super::{BookRef, ContactBackend, ContactBackendCtx, PushedContact};

pub struct JmapContactBackend;

#[derive(Debug)]
struct PendingCreate {
    local_id: String,
    local_book_id: String,
    remote_book_id: String,
    uid: Option<String>,
    display_name: String,
    emails_json: String,
    phones_json: String,
    organization: Option<String>,
    title: Option<String>,
    notes: Option<String>,
}

#[async_trait]
impl ContactBackend for JmapContactBackend {
    fn protocol(&self) -> &'static str {
        "jmap"
    }

    async fn sync(&self, ctx: &ContactBackendCtx<'_>, account: &AccountFull) -> Result<()> {
        let account_id = account.id.as_str();
        let (jmap_config, jmap_conn) = ctx.providers.jmap_client(account).await?;

        // Nothing local is changed until both remote collections have been
        // fetched and their cross-references form one valid complete snapshot.
        let address_books = jmap_conn.list_address_books(&jmap_config).await?;
        let contacts = jmap_conn.fetch_contacts(&jmap_config).await?;
        let contacts_by_book = partition_contacts(&address_books, contacts)?;
        log::info!(
            "sync_contacts: validated {} JMAP books and {} membership rows",
            address_books.len(),
            contacts_by_book.values().map(Vec::len).sum::<usize>()
        );

        let pending = {
            let conn = ctx.db.writer().await;
            let remote_to_local = upsert_address_books(&conn, account_id, &address_books)?;

            for address_book in &address_books {
                let local_book_id = remote_to_local.get(&address_book.id).ok_or_else(|| {
                    Error::Sync("validated JMAP address book was not mapped locally".into())
                })?;
                let remote_contacts = contacts_by_book.get(&address_book.id).ok_or_else(|| {
                    Error::Sync("validated JMAP address book has no partition".into())
                })?;
                reconcile_book(&conn, local_book_id, remote_contacts)?;
            }

            // Checkpoint 1 intentionally remains remote-id-only. UID matching
            // and interrupted-sync recovery belong to checkpoint 2.
            load_pending_creates(&conn, &address_books, &remote_to_local)?
        };

        if !pending.is_empty() {
            log::info!(
                "sync_contacts: pushing {} deferred local contacts to JMAP",
                pending.len()
            );
        }
        for pending in pending {
            let uid = {
                let mut conn = ctx.db.writer().await;
                ensure_contact_uid(
                    &mut conn,
                    &pending.local_book_id,
                    &pending.local_id,
                    pending.uid.as_deref(),
                )?
            };

            let remote_id = jmap_conn
                .create_contact_card(
                    &jmap_config,
                    &pending.remote_book_id,
                    &uid,
                    &pending.display_name,
                    &pending.emails_json,
                    &pending.phones_json,
                    pending.organization.as_deref(),
                    pending.title.as_deref(),
                    pending.notes.as_deref(),
                )
                .await?;

            let attachment = {
                let mut conn = ctx.db.writer().await;
                attach_remote_id(
                    &mut conn,
                    &pending.local_book_id,
                    &pending.local_id,
                    &remote_id,
                )
            };
            if let Err(error) = attachment {
                let cleanup_failed = jmap_conn
                    .delete_contact_card(&jmap_config, &remote_id)
                    .await
                    .is_err();
                let cleanup = if cleanup_failed {
                    "; compensating remote cleanup also failed"
                } else {
                    ""
                };
                return Err(Error::Sync(format!(
                    "created JMAP contact could not be attached locally: {error}{cleanup}"
                )));
            }
        }

        Ok(())
    }

    /// JMAP contact creation is deferred to the next sync's unpushed pass.
    async fn push_created_contact(
        &self,
        _ctx: &ContactBackendCtx<'_>,
        _account: &AccountFull,
        _book: &BookRef<'_>,
        _contact: &Contact,
    ) -> Result<Option<PushedContact>> {
        Ok(None)
    }

    async fn push_updated_contact(
        &self,
        ctx: &ContactBackendCtx<'_>,
        account: &AccountFull,
        _book: &BookRef<'_>,
        contact: &Contact,
        remote_id: &str,
    ) -> Result<Option<PushedContact>> {
        let (jmap_config, conn_jmap) = ctx.providers.jmap_client(account).await?;
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

    async fn push_deleted_contact(
        &self,
        ctx: &ContactBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
    ) -> Result<()> {
        let (jmap_config, conn_jmap) = ctx.providers.jmap_client(account).await?;
        conn_jmap.delete_contact_card(&jmap_config, remote_id).await
    }
}

fn partition_contacts(
    address_books: &[JmapAddressBook],
    contacts: Vec<JmapContact>,
) -> Result<BTreeMap<String, Vec<JmapContact>>> {
    let mut by_book = BTreeMap::new();
    for address_book in address_books {
        if by_book
            .insert(address_book.id.clone(), Vec::new())
            .is_some()
        {
            return Err(Error::Sync(
                "validated JMAP snapshot contains duplicate address books".into(),
            ));
        }
    }
    for contact in contacts {
        for address_book_id in &contact.address_book_ids {
            let partition = by_book.get_mut(address_book_id).ok_or_else(|| {
                Error::Sync(
                    "JMAP contact references an address book absent from AddressBook/get".into(),
                )
            })?;
            partition.push(contact.clone());
        }
    }
    for contacts in by_book.values_mut() {
        contacts.sort_by(|left, right| left.id.cmp(&right.id));
    }
    Ok(by_book)
}

fn upsert_address_books(
    conn: &Connection,
    account_id: &str,
    address_books: &[JmapAddressBook],
) -> Result<BTreeMap<String, String>> {
    let mut remote_to_local = BTreeMap::new();
    for address_book in address_books {
        let existing = conn
            .query_row(
                "SELECT id FROM contact_books
                 WHERE account_id = ?1 AND sync_type = 'jmap' AND remote_id = ?2
                 ORDER BY created_at, id LIMIT 1",
                rusqlite::params![account_id, address_book.id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let local_id = if let Some(local_id) = existing {
            let updated = conn.execute(
                "UPDATE contact_books SET name = ?1
                 WHERE id = ?2 AND account_id = ?3
                   AND sync_type = 'jmap' AND remote_id = ?4",
                rusqlite::params![address_book.name, local_id, account_id, address_book.id],
            )?;
            if updated != 1 {
                return Err(Error::Sync(
                    "JMAP contact book changed during deterministic upsert".into(),
                ));
            }
            local_id
        } else {
            let local_id = uuid::Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO contact_books
                     (id, account_id, name, remote_id, sync_type)
                 VALUES (?1, ?2, ?3, ?4, 'jmap')",
                rusqlite::params![local_id, account_id, address_book.name, address_book.id],
            )?;
            local_id
        };
        remote_to_local.insert(address_book.id.clone(), local_id);
    }
    Ok(remote_to_local)
}

fn reconcile_book(
    conn: &Connection,
    local_book_id: &str,
    remote_contacts: &[JmapContact],
) -> Result<()> {
    for contact in remote_contacts {
        let existing = conn
            .query_row(
                "SELECT id FROM contacts
                 WHERE book_id = ?1 AND remote_id = ?2
                 ORDER BY created_at, id LIMIT 1",
                rusqlite::params![local_book_id, contact.id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(local_id) = existing {
            let updated = conn.execute(
                "UPDATE contacts
                 SET display_name = ?1, emails_json = ?2, phones_json = ?3,
                     organization = ?4, title = ?5, notes = ?6, uid = ?7,
                     updated_at = CURRENT_TIMESTAMP
                 WHERE id = ?8 AND book_id = ?9 AND remote_id = ?10",
                rusqlite::params![
                    contact.display_name,
                    contact.emails_json,
                    contact.phones_json,
                    contact.organization,
                    contact.title,
                    contact.notes,
                    contact.uid,
                    local_id,
                    local_book_id,
                    contact.id
                ],
            )?;
            if updated != 1 {
                return Err(Error::Sync(
                    "JMAP contact changed during deterministic upsert".into(),
                ));
            }
        } else {
            conn.execute(
                "INSERT INTO contacts
                     (id, book_id, uid, display_name, emails_json, phones_json,
                      addresses_json, organization, title, notes, remote_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, '[]', ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    uuid::Uuid::new_v4().to_string(),
                    local_book_id,
                    contact.uid,
                    contact.display_name,
                    contact.emails_json,
                    contact.phones_json,
                    contact.organization,
                    contact.title,
                    contact.notes,
                    contact.id
                ],
            )?;
        }
    }

    let server_ids: HashSet<&str> = remote_contacts
        .iter()
        .map(|contact| contact.id.as_str())
        .collect();
    let local_synced = {
        let mut statement = conn.prepare(
            "SELECT id, remote_id FROM contacts
             WHERE book_id = ?1
             ORDER BY created_at, id",
        )?;
        let rows = statement
            .query_map(rusqlite::params![local_book_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };
    for (local_id, remote_id) in local_synced {
        let Some(remote_id) = remote_id.filter(|remote_id| !is_blank(remote_id)) else {
            continue;
        };
        if !server_ids.contains(remote_id.as_str()) {
            let deleted = conn.execute(
                "DELETE FROM contacts WHERE id = ?1 AND book_id = ?2",
                rusqlite::params![local_id, local_book_id],
            )?;
            if deleted != 1 {
                return Err(Error::Sync(
                    "JMAP contact changed while pruning a complete snapshot".into(),
                ));
            }
        }
    }
    Ok(())
}

fn load_pending_creates(
    conn: &Connection,
    address_books: &[JmapAddressBook],
    remote_to_local: &BTreeMap<String, String>,
) -> Result<Vec<PendingCreate>> {
    let mut pending = Vec::new();
    for address_book in address_books {
        let local_book_id = remote_to_local.get(&address_book.id).ok_or_else(|| {
            Error::Sync("validated JMAP address book was not mapped locally".into())
        })?;
        let mut statement = conn.prepare(
            "SELECT id, uid, display_name, emails_json, phones_json,
                    organization, title, notes, remote_id
             FROM contacts
             WHERE book_id = ?1
             ORDER BY created_at, id",
        )?;
        let rows = statement
            .query_map(rusqlite::params![local_book_id], |row| {
                Ok((
                    PendingCreate {
                        local_id: row.get(0)?,
                        local_book_id: local_book_id.clone(),
                        remote_book_id: address_book.id.clone(),
                        uid: row.get(1)?,
                        display_name: row.get(2)?,
                        emails_json: row.get(3)?,
                        phones_json: row.get(4)?,
                        organization: row.get(5)?,
                        title: row.get(6)?,
                        notes: row.get(7)?,
                    },
                    row.get::<_, Option<String>>(8)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        pending.extend(
            rows.into_iter()
                .filter(|(_, remote_id)| remote_id.as_deref().is_none_or(is_blank))
                .map(|(pending, _)| pending),
        );
    }
    Ok(pending)
}

fn ensure_contact_uid(
    conn: &mut Connection,
    local_book_id: &str,
    local_id: &str,
    snapshot_uid: Option<&str>,
) -> Result<String> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = transaction
        .query_row(
            "SELECT uid, remote_id FROM contacts WHERE id = ?1 AND book_id = ?2",
            rusqlite::params![local_id, local_book_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| Error::Sync("deferred JMAP contact no longer exists".into()))?;
    if current
        .1
        .as_deref()
        .is_some_and(|remote_id| !is_blank(remote_id))
    {
        return Err(Error::Sync(
            "deferred JMAP contact is no longer local-only".into(),
        ));
    }

    let snapshot_had_uid = snapshot_uid.is_some_and(|uid| !uid.trim().is_empty());
    let uid = if let Some(uid) = current.0.filter(|uid| !uid.trim().is_empty()) {
        uid
    } else {
        if snapshot_had_uid {
            return Err(Error::Sync(
                "deferred JMAP contact uid changed before assignment".into(),
            ));
        }
        let uid = format!("{}@chithi", uuid::Uuid::new_v4());
        let updated = transaction.execute(
            "UPDATE contacts SET uid = ?1, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND book_id = ?3",
            rusqlite::params![uid, local_id, local_book_id],
        )?;
        if updated != 1 {
            return Err(Error::Sync(
                "deferred JMAP contact changed before uid assignment".into(),
            ));
        }
        uid
    };
    transaction.commit()?;
    Ok(uid)
}

fn attach_remote_id(
    conn: &mut Connection,
    local_book_id: &str,
    local_id: &str,
    remote_id: &str,
) -> Result<()> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current_remote_id = transaction
        .query_row(
            "SELECT remote_id FROM contacts WHERE id = ?1 AND book_id = ?2",
            rusqlite::params![local_id, local_book_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .ok_or_else(|| Error::Sync("deferred JMAP contact no longer exists".into()))?;
    if current_remote_id
        .as_deref()
        .is_some_and(|current| !is_blank(current))
    {
        return Err(Error::Sync(
            "deferred JMAP contact changed before remote-id attachment".into(),
        ));
    }

    let collision = {
        let mut statement = transaction
            .prepare("SELECT remote_id FROM contacts WHERE book_id = ?1 AND id <> ?2")?;
        let remote_ids = statement
            .query_map(rusqlite::params![local_book_id, local_id], |row| {
                row.get::<_, Option<String>>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        remote_ids
            .into_iter()
            .flatten()
            .any(|stored| !is_blank(&stored) && stored.trim() == remote_id)
    };
    if collision {
        return Err(Error::Sync(
            "created JMAP id collides with another contact in the same book".into(),
        ));
    }
    let updated = transaction.execute(
        "UPDATE contacts SET remote_id = ?1, updated_at = CURRENT_TIMESTAMP
         WHERE id = ?2 AND book_id = ?3",
        rusqlite::params![remote_id, local_id, local_book_id],
    )?;
    if updated != 1 {
        return Err(Error::Sync(
            "deferred JMAP contact changed before remote-id attachment".into(),
        ));
    }
    transaction.commit()?;
    Ok(())
}

fn is_blank(value: &str) -> bool {
    value.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(id: &str, memberships: &[&str]) -> JmapContact {
        JmapContact {
            id: id.into(),
            uid: format!("uid-{id}"),
            address_book_ids: memberships.iter().map(|id| (*id).into()).collect(),
            display_name: id.into(),
            emails_json: "[]".into(),
            phones_json: "[]".into(),
            organization: None,
            title: None,
            notes: None,
        }
    }

    #[test]
    fn partitions_one_remote_card_into_every_membership() {
        let books = vec![
            JmapAddressBook {
                id: "a".into(),
                name: "A".into(),
            },
            JmapAddressBook {
                id: "b".into(),
                name: "B".into(),
            },
        ];
        let partitioned = partition_contacts(&books, vec![contact("card", &["b", "a"])]).unwrap();

        assert_eq!(partitioned["a"].len(), 1);
        assert_eq!(partitioned["b"].len(), 1);
        assert_eq!(partitioned["a"][0].id, "card");
        assert_eq!(partitioned["b"][0].id, "card");
    }

    #[test]
    fn rejects_membership_missing_from_validated_books() {
        let books = vec![JmapAddressBook {
            id: "known".into(),
            name: "Known".into(),
        }];
        assert!(partition_contacts(&books, vec![contact("card", &["missing"])]).is_err());
    }

    fn attachment_db() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE contacts (
                     id TEXT PRIMARY KEY,
                     book_id TEXT NOT NULL,
                     uid TEXT,
                     remote_id TEXT,
                     updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                 );
                 INSERT INTO contacts (id, book_id, remote_id) VALUES
                     ('target', 'book-a', NULL),
                     ('same-book', 'book-a', 'taken'),
                     ('other-book', 'book-b', 'shared');",
            )
            .unwrap();
        connection
    }

    #[test]
    fn remote_id_attachment_is_conditional_and_book_scoped() {
        let mut connection = attachment_db();
        attach_remote_id(&mut connection, "book-a", "target", "shared").unwrap();
        let attached: String = connection
            .query_row(
                "SELECT remote_id FROM contacts WHERE id = 'target'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attached, "shared");
        assert!(attach_remote_id(&mut connection, "book-a", "target", "later").is_err());
        assert!(attach_remote_id(&mut connection, "book-a", "missing", "later").is_err());
    }

    #[test]
    fn remote_id_attachment_rejects_same_book_collision() {
        let mut connection = attachment_db();
        assert!(attach_remote_id(&mut connection, "book-a", "target", "taken").is_err());
        let remote_id: Option<String> = connection
            .query_row(
                "SELECT remote_id FROM contacts WHERE id = 'target'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(remote_id.is_none());
    }

    #[test]
    fn remote_id_attachment_treats_unicode_whitespace_as_local_only() {
        let mut connection = attachment_db();
        connection
            .execute(
                "UPDATE contacts SET remote_id = ?1 WHERE id = 'target'",
                ["\u{2003}\t\n"],
            )
            .unwrap();

        attach_remote_id(&mut connection, "book-a", "target", "shared").unwrap();
        let attached: String = connection
            .query_row(
                "SELECT remote_id FROM contacts WHERE id = 'target'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attached, "shared");
    }

    #[test]
    fn uid_assignment_rechecks_unicode_blank_values_in_a_transaction() {
        let mut connection = attachment_db();
        connection
            .execute(
                "UPDATE contacts SET uid = ?1, remote_id = ?2 WHERE id = 'target'",
                rusqlite::params!["\t\n", "\u{2003}"],
            )
            .unwrap();

        let uid = ensure_contact_uid(&mut connection, "book-a", "target", None).unwrap();
        assert!(uid.ends_with("@chithi"));
        let stored: String = connection
            .query_row("SELECT uid FROM contacts WHERE id = 'target'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(stored, uid);

        connection
            .execute(
                "UPDATE contacts SET uid = NULL, remote_id = 'managed' WHERE id = 'target'",
                [],
            )
            .unwrap();
        assert!(ensure_contact_uid(&mut connection, "book-a", "target", None).is_err());
    }

    fn reconciliation_db() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE contacts (
                     id TEXT PRIMARY KEY,
                     book_id TEXT NOT NULL,
                     uid TEXT,
                     display_name TEXT NOT NULL,
                     emails_json TEXT NOT NULL,
                     phones_json TEXT NOT NULL,
                     addresses_json TEXT NOT NULL,
                     organization TEXT,
                     title TEXT,
                     notes TEXT,
                     remote_id TEXT,
                     created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                     updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                 );",
            )
            .unwrap();
        for (id, remote_id) in [
            ("null", None),
            ("ascii-space", Some("\t\n")),
            ("unicode-space", Some("\u{2003}")),
            ("managed", Some("stale")),
        ] {
            connection
                .execute(
                    "INSERT INTO contacts (
                         id, book_id, display_name, emails_json, phones_json,
                         addresses_json, remote_id
                     ) VALUES (?1, 'local-book', ?1, '[]', '[]', '[]', ?2)",
                    rusqlite::params![id, remote_id],
                )
                .unwrap();
        }
        connection
    }

    #[test]
    fn pruning_and_deferred_selection_use_rust_blank_classification() {
        let connection = reconciliation_db();
        reconcile_book(&connection, "local-book", &[]).unwrap();

        let remaining = {
            let mut statement = connection
                .prepare("SELECT id FROM contacts ORDER BY id")
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(remaining, ["ascii-space", "null", "unicode-space"]);

        let books = vec![JmapAddressBook {
            id: "remote-book".into(),
            name: "Book".into(),
        }];
        let mapping = BTreeMap::from([("remote-book".into(), "local-book".into())]);
        let pending = load_pending_creates(&connection, &books, &mapping).unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|contact| contact.local_id.as_str())
                .collect::<Vec<_>>(),
            ["ascii-space", "null", "unicode-space"]
        );
    }
}
