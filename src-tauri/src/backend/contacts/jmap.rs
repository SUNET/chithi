//! JMAP Contacts backend (RFC 9610, with RFC 9553 JSContact cards).

use std::collections::{BTreeMap, HashSet};

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::contact::reconcile::{
    capture_remote_id_baseline, reconcile_remote_id_snapshot_batch_with_postcheck,
    CompleteRemoteIdSnapshot, RemoteContactPatch, RemoteField,
};
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
        let remote_uids: HashSet<String> =
            contacts.iter().map(|contact| contact.uid.clone()).collect();
        let remote_ids: HashSet<&str> =
            contacts.iter().map(|contact| contact.id.as_str()).collect();
        if remote_uids.len() != contacts.len() || remote_ids.len() != contacts.len() {
            return Err(Error::Sync(
                "validated JMAP snapshot contains duplicate account-wide identities".into(),
            ));
        }
        let mut contacts_by_book = partition_contacts(&address_books, contacts)?;
        let membership_rows = contacts_by_book.values().map(Vec::len).sum::<usize>();
        let mut snapshots = BTreeMap::new();
        for address_book in &address_books {
            let contacts = contacts_by_book.remove(&address_book.id).ok_or_else(|| {
                Error::Sync("validated JMAP address book has no partition".into())
            })?;
            let snapshot = CompleteRemoteIdSnapshot::new_with_uid_fallback(
                contacts.into_iter().map(contact_patch).collect(),
            )?;
            snapshots.insert(address_book.id.clone(), snapshot);
        }
        log::info!(
            "sync_contacts: validated {} JMAP books and {} membership rows",
            address_books.len(),
            membership_rows
        );

        let (pending, reports, mut unavailable_uids) = {
            let mut conn = ctx.db.writer().await;
            let remote_to_local = upsert_address_books(&conn, account_id, &address_books)?;
            let mut reconciliations = Vec::with_capacity(address_books.len());
            for address_book in &address_books {
                let local_book_id = remote_to_local.get(&address_book.id).ok_or_else(|| {
                    Error::Sync("validated JMAP address book was not mapped locally".into())
                })?;
                let snapshot = snapshots.remove(&address_book.id).ok_or_else(|| {
                    Error::Sync("validated JMAP address book has no snapshot".into())
                })?;
                let baseline =
                    capture_remote_id_baseline(&conn, account_id, local_book_id, "jmap")?;
                reconciliations.push((baseline, snapshot));
            }
            let (reports, (pending, unavailable_uids)) =
                reconcile_remote_id_snapshot_batch_with_postcheck(
                    &mut conn,
                    reconciliations,
                    |transaction| {
                        let pending =
                            load_pending_creates(transaction, &address_books, &remote_to_local)?;
                        let unavailable_uids =
                            validate_pending_create_uids(&pending, &remote_uids)?;
                        Ok((pending, unavailable_uids))
                    },
                )?;
            (pending, reports, unavailable_uids)
        };
        for (address_book, report) in address_books.iter().zip(reports) {
            log::info!(
                "sync_contacts: reconciled JMAP book {:?}: inserted={}, updated={}, deleted={}, \
                 unchanged_or_stale={}",
                address_book.id,
                report.inserted,
                report.updated,
                report.deleted,
                report.unchanged_or_stale
            );
        }

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
                    &unavailable_uids,
                )?
            };
            unavailable_uids.insert(uid.clone());

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
                    &uid,
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

fn contact_patch(contact: JmapContact) -> RemoteContactPatch {
    RemoteContactPatch {
        uid: RemoteField::Set(Some(contact.uid)),
        display_name: RemoteField::Set(contact.display_name),
        emails_json: RemoteField::Set(contact.emails_json),
        phones_json: RemoteField::Set(contact.phones_json),
        addresses_json: RemoteField::Preserve,
        organization: RemoteField::Set(contact.organization),
        title: RemoteField::Set(contact.title),
        notes: RemoteField::Set(contact.notes),
        vcard_data: RemoteField::Preserve,
        remote_id: RemoteField::Set(Some(contact.id)),
        etag: RemoteField::Preserve,
    }
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

fn validate_pending_create_uids(
    pending: &[PendingCreate],
    remote_uids: &HashSet<String>,
) -> Result<HashSet<String>> {
    let mut unavailable = remote_uids.clone();
    let mut pending_uids = HashSet::new();
    for contact in pending {
        let Some(uid) = contact.uid.as_deref().filter(|uid| !is_blank(uid)) else {
            continue;
        };
        if remote_uids.contains(uid) {
            return Err(Error::Sync(
                "deferred JMAP contact UID already exists remotely".into(),
            ));
        }
        if !pending_uids.insert(uid) {
            return Err(Error::Sync(
                "deferred JMAP contacts contain a duplicate UID".into(),
            ));
        }
        unavailable.insert(uid.to_string());
    }
    Ok(unavailable)
}

fn ensure_contact_uid(
    conn: &mut Connection,
    local_book_id: &str,
    local_id: &str,
    snapshot_uid: Option<&str>,
    unavailable_uids: &HashSet<String>,
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

    let snapshot_uid = snapshot_uid.filter(|uid| !is_blank(uid));
    let current_uid = current.0.as_deref().filter(|uid| !is_blank(uid));
    let uid = match (snapshot_uid, current_uid) {
        (Some(expected), Some(current)) if expected == current => current.to_string(),
        (None, None) => {
            let uid = loop {
                let candidate = format!("{}@chithi", uuid::Uuid::new_v4());
                if !unavailable_uids.contains(&candidate) {
                    break candidate;
                }
            };
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
        }
        _ => {
            return Err(Error::Sync(
                "deferred JMAP contact uid changed before assignment".into(),
            ));
        }
    };
    transaction.commit()?;
    Ok(uid)
}

fn attach_remote_id(
    conn: &mut Connection,
    local_book_id: &str,
    local_id: &str,
    expected_uid: &str,
    remote_id: &str,
) -> Result<()> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (current_uid, current_remote_id) = transaction
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
    if current_uid.as_deref() != Some(expected_uid) {
        return Err(Error::Sync(
            "deferred JMAP contact uid changed before remote-id attachment".into(),
        ));
    }
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
            .any(|stored| !is_blank(&stored) && stored == remote_id)
    };
    if collision {
        return Err(Error::Sync(
            "created JMAP id collides with another contact in the same book".into(),
        ));
    }
    let updated = transaction.execute(
        "UPDATE contacts SET remote_id = ?1, updated_at = CURRENT_TIMESTAMP
         WHERE id = ?2 AND book_id = ?3 AND uid = ?4",
        rusqlite::params![remote_id, local_id, local_book_id, expected_uid],
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

    fn pending(id: &str, uid: Option<&str>) -> PendingCreate {
        PendingCreate {
            local_id: id.into(),
            local_book_id: "local-book".into(),
            remote_book_id: "remote-book".into(),
            uid: uid.map(str::to_string),
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
                 INSERT INTO contacts (id, book_id, uid, remote_id) VALUES
                     ('target', 'book-a', 'uid-target', NULL),
                     ('same-book', 'book-a', 'uid-taken', 'taken'),
                     ('other-book', 'book-b', 'uid-other', 'shared');",
            )
            .unwrap();
        connection
    }

    #[test]
    fn remote_id_attachment_is_conditional_and_book_scoped() {
        let mut connection = attachment_db();
        assert!(
            attach_remote_id(&mut connection, "book-a", "target", "wrong-uid", "shared").is_err()
        );
        attach_remote_id(&mut connection, "book-a", "target", "uid-target", "shared").unwrap();
        let attached: String = connection
            .query_row(
                "SELECT remote_id FROM contacts WHERE id = 'target'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attached, "shared");
        assert!(
            attach_remote_id(&mut connection, "book-a", "target", "uid-target", "later").is_err()
        );
        assert!(
            attach_remote_id(&mut connection, "book-a", "missing", "uid-target", "later").is_err()
        );
    }

    #[test]
    fn pending_uid_validation_is_account_wide_and_fail_closed() {
        let remote_uids = HashSet::from(["remote-uid".to_string()]);
        assert!(
            validate_pending_create_uids(&[pending("one", Some("remote-uid"))], &remote_uids)
                .is_err()
        );
        assert!(validate_pending_create_uids(
            &[
                pending("one", Some("duplicate")),
                pending("two", Some("duplicate")),
            ],
            &remote_uids,
        )
        .is_err());

        let unavailable = validate_pending_create_uids(
            &[
                pending("one", Some("local-uid")),
                pending("two", None),
                pending("three", Some("\u{2003}")),
            ],
            &remote_uids,
        )
        .unwrap();
        assert!(unavailable.contains("remote-uid"));
        assert!(unavailable.contains("local-uid"));
        assert!(!unavailable.contains("\u{2003}"));
    }

    #[test]
    fn remote_id_attachment_rejects_same_book_collision() {
        let mut connection = attachment_db();
        assert!(
            attach_remote_id(&mut connection, "book-a", "target", "uid-target", "taken").is_err()
        );
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

        attach_remote_id(&mut connection, "book-a", "target", "uid-target", "shared").unwrap();
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

        let uid =
            ensure_contact_uid(&mut connection, "book-a", "target", None, &HashSet::new()).unwrap();
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
        assert!(
            ensure_contact_uid(&mut connection, "book-a", "target", None, &HashSet::new()).is_err()
        );
    }

    #[test]
    fn uid_assignment_requires_the_captured_nonblank_uid() {
        let mut connection = attachment_db();
        assert!(ensure_contact_uid(
            &mut connection,
            "book-a",
            "target",
            Some("different"),
            &HashSet::new(),
        )
        .is_err());
        assert!(
            ensure_contact_uid(&mut connection, "book-a", "target", None, &HashSet::new(),)
                .is_err()
        );
        assert_eq!(
            ensure_contact_uid(
                &mut connection,
                "book-a",
                "target",
                Some("uid-target"),
                &HashSet::new(),
            )
            .unwrap(),
            "uid-target"
        );
    }

    fn reconciliation_db() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE accounts (id TEXT PRIMARY KEY);
                 INSERT INTO accounts (id) VALUES ('account');
                 CREATE TABLE contact_books (
                     id TEXT PRIMARY KEY,
                     account_id TEXT NOT NULL,
                     name TEXT NOT NULL,
                     remote_id TEXT,
                     sync_type TEXT NOT NULL,
                     created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                 );
                 INSERT INTO contact_books
                     (id, account_id, name, remote_id, sync_type)
                 VALUES ('local-book', 'account', 'Book', 'remote-book', 'jmap');
                 CREATE TABLE contacts (
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
                     vcard_data TEXT,
                     remote_id TEXT,
                     etag TEXT,
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
        let mut connection = reconciliation_db();
        let baseline =
            capture_remote_id_baseline(&connection, "account", "local-book", "jmap").unwrap();
        let snapshot = CompleteRemoteIdSnapshot::new_with_uid_fallback(Vec::new()).unwrap();
        crate::contact::reconcile::reconcile_remote_id_snapshot_batch(
            &mut connection,
            vec![(baseline, snapshot)],
        )
        .unwrap();

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

    #[test]
    fn shared_uid_reconciliation_recovers_an_interrupted_create() {
        let mut connection = reconciliation_db();
        connection.execute("DELETE FROM contacts", []).unwrap();
        connection
            .execute(
                "INSERT INTO contacts (
                     id, book_id, uid, display_name, emails_json, phones_json, addresses_json
                 ) VALUES ('draft', 'local-book', 'uid-card', 'Local', '[]', '[]', '[]')",
                [],
            )
            .unwrap();
        let baseline =
            capture_remote_id_baseline(&connection, "account", "local-book", "jmap").unwrap();
        let snapshot = CompleteRemoteIdSnapshot::new_with_uid_fallback(vec![contact_patch(
            contact("card", &["remote-book"]),
        )])
        .unwrap();
        let books = vec![JmapAddressBook {
            id: "remote-book".into(),
            name: "Book".into(),
        }];
        let mapping = BTreeMap::from([("remote-book".into(), "local-book".into())]);

        let (reports, pending) = reconcile_remote_id_snapshot_batch_with_postcheck(
            &mut connection,
            vec![(baseline, snapshot)],
            |transaction| load_pending_creates(transaction, &books, &mapping),
        )
        .unwrap();

        assert_eq!(reports[0].updated, 1);
        assert!(pending.is_empty());
        let stored = connection
            .query_row(
                "SELECT uid, remote_id, display_name FROM contacts WHERE id = 'draft'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(stored, ("uid-card".into(), "card".into(), "card".into()));
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM contacts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
