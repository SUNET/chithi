use std::collections::{HashMap, HashSet};

use rusqlite::{
    params, params_from_iter, types::Value, Connection, OptionalExtension, TransactionBehavior,
};

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteField<T> {
    Preserve,
    Set(T),
}

impl<T: Clone> RemoteField<T> {
    fn resolve(&self, preserved: &T) -> T {
        match self {
            Self::Preserve => preserved.clone(),
            Self::Set(value) => value.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteContactPatch {
    pub uid: RemoteField<Option<String>>,
    pub display_name: RemoteField<String>,
    pub emails_json: RemoteField<String>,
    pub phones_json: RemoteField<String>,
    pub addresses_json: RemoteField<String>,
    pub organization: RemoteField<Option<String>>,
    pub title: RemoteField<Option<String>>,
    pub notes: RemoteField<Option<String>>,
    pub vcard_data: RemoteField<Option<String>>,
    pub remote_id: RemoteField<Option<String>>,
    pub etag: RemoteField<Option<String>>,
}

impl RemoteContactPatch {
    fn validated_remote_id(&self) -> Result<&str> {
        match &self.remote_id {
            RemoteField::Set(Some(remote_id)) if !remote_id.trim().is_empty() => Ok(remote_id),
            _ => Err(Error::Sync(
                "complete contact snapshot contains a missing remote ID".into(),
            )),
        }
    }

    fn apply(&self, preserved: &ContactValues) -> ContactValues {
        ContactValues {
            uid: self.uid.resolve(&preserved.uid),
            display_name: self.display_name.resolve(&preserved.display_name),
            emails_json: self.emails_json.resolve(&preserved.emails_json),
            phones_json: self.phones_json.resolve(&preserved.phones_json),
            addresses_json: self.addresses_json.resolve(&preserved.addresses_json),
            organization: self.organization.resolve(&preserved.organization),
            title: self.title.resolve(&preserved.title),
            notes: self.notes.resolve(&preserved.notes),
            vcard_data: self.vcard_data.resolve(&preserved.vcard_data),
            remote_id: self.remote_id.resolve(&preserved.remote_id),
            etag: self.etag.resolve(&preserved.etag),
        }
    }

    fn insert_values(&self) -> ContactValues {
        self.apply(&ContactValues {
            uid: None,
            display_name: String::new(),
            emails_json: "[]".into(),
            phones_json: "[]".into(),
            addresses_json: "[]".into(),
            organization: None,
            title: None,
            notes: None,
            vcard_data: None,
            remote_id: None,
            etag: None,
        })
    }
}

#[derive(Debug, Clone)]
struct SnapshotRecord {
    remote_id: String,
    patch: RemoteContactPatch,
}

#[derive(Debug, Clone)]
pub(crate) struct CompleteRemoteIdSnapshot {
    records: Vec<SnapshotRecord>,
    remote_ids: HashSet<String>,
}

impl CompleteRemoteIdSnapshot {
    pub(crate) fn new(patches: Vec<RemoteContactPatch>) -> Result<Self> {
        let mut records = Vec::with_capacity(patches.len());
        let mut remote_ids = HashSet::with_capacity(patches.len());

        for patch in patches {
            let remote_id = patch.validated_remote_id()?.to_string();
            if !remote_ids.insert(remote_id.clone()) {
                return Err(Error::Sync(format!(
                    "complete contact snapshot contains duplicate remote ID {remote_id:?}"
                )));
            }
            records.push(SnapshotRecord { remote_id, patch });
        }

        Ok(Self {
            records,
            remote_ids,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteIdBaseline {
    account_id: String,
    book_id: String,
    sync_type: String,
    rows: Vec<StoredContact>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ReconcileReport {
    pub inserted: usize,
    pub updated: usize,
    pub deleted: usize,
    pub unchanged_or_stale: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DuplicateRemoteIdRepairReport {
    pub duplicate_remote_ids: usize,
    pub detached: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContactValues {
    uid: Option<String>,
    display_name: String,
    emails_json: String,
    phones_json: String,
    addresses_json: String,
    organization: Option<String>,
    title: Option<String>,
    notes: Option<String>,
    vcard_data: Option<String>,
    remote_id: Option<String>,
    etag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredContact {
    id: String,
    book_id: String,
    values: ContactValues,
    created_at: String,
    updated_at: String,
}

/// Detach ambiguous legacy identity assignments without deleting contact data.
/// A nonblank UID wins, then the oldest creation timestamp and lowest ID. The
/// repair commits independently before the caller captures its sync baseline;
/// Graph is the first adopter, while duplicate detection remains fail-closed.
pub(crate) fn repair_duplicate_managed_remote_ids(
    conn: &mut Connection,
    account_id: &str,
    book_id: &str,
    expected_sync_type: &str,
) -> Result<DuplicateRemoteIdRepairReport> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    verify_book_scope(&transaction, account_id, book_id, expected_sync_type)?;

    let contacts = {
        let mut statement = transaction.prepare(
            "SELECT id, uid, remote_id, created_at
             FROM contacts
             WHERE book_id = ?1 AND remote_id IS NOT NULL",
        )?;
        let contacts = statement
            .query_map(params![book_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        contacts
    };

    let mut candidates_by_remote_id = HashMap::new();
    for (id, uid, remote_id, created_at) in contacts {
        if remote_id.trim().is_empty() {
            continue;
        }
        candidates_by_remote_id
            .entry(remote_id)
            .or_insert_with(Vec::new)
            .push((id, uid, created_at));
    }

    let mut report = DuplicateRemoteIdRepairReport::default();
    for (remote_id, mut candidates) in candidates_by_remote_id {
        if candidates.len() < 2 {
            continue;
        }

        candidates.sort_by(|left, right| {
            let left_has_uid = left.1.as_deref().is_some_and(|uid| !uid.trim().is_empty());
            let right_has_uid = right.1.as_deref().is_some_and(|uid| !uid.trim().is_empty());
            right_has_uid
                .cmp(&left_has_uid)
                .then_with(|| left.2.cmp(&right.2))
                .then_with(|| left.0.cmp(&right.0))
        });

        report.duplicate_remote_ids += 1;
        for (id, _, _) in candidates.iter().skip(1) {
            let detached = transaction.execute(
                "UPDATE contacts
                 SET remote_id = NULL, updated_at = CURRENT_TIMESTAMP
                 WHERE id = ?1 AND book_id = ?2 AND remote_id = ?3",
                params![id, book_id, remote_id],
            )?;
            if detached != 1 {
                return Err(Error::Sync(format!(
                    "contact {id:?} changed while detaching duplicate remote ID"
                )));
            }
            report.detached += 1;
        }
    }

    transaction.commit()?;
    Ok(report)
}

pub(crate) fn capture_remote_id_baseline(
    conn: &Connection,
    account_id: &str,
    book_id: &str,
    expected_sync_type: &str,
) -> Result<RemoteIdBaseline> {
    let transaction = conn.unchecked_transaction()?;
    verify_book_scope(&transaction, account_id, book_id, expected_sync_type)?;
    let rows = load_contacts(&transaction, book_id)?;
    transaction.commit()?;

    Ok(RemoteIdBaseline {
        account_id: account_id.into(),
        book_id: book_id.into(),
        sync_type: expected_sync_type.into(),
        rows,
    })
}

pub(crate) fn reconcile_remote_id_snapshot(
    conn: &mut Connection,
    baseline: &RemoteIdBaseline,
    snapshot: CompleteRemoteIdSnapshot,
) -> Result<ReconcileReport> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    verify_book_scope(
        &transaction,
        &baseline.account_id,
        &baseline.book_id,
        &baseline.sync_type,
    )?;

    let current_rows = load_contacts(&transaction, &baseline.book_id)?;
    let current_by_remote = index_managed_rows(&current_rows, "current")?;
    let baseline_by_remote = index_managed_rows(&baseline.rows, "baseline")?;
    let current_by_id: HashMap<&str, &StoredContact> = current_rows
        .iter()
        .map(|contact| (contact.id.as_str(), contact))
        .collect();

    let CompleteRemoteIdSnapshot {
        records,
        remote_ids,
    } = snapshot;
    let mut report = ReconcileReport::default();

    for record in records {
        let baseline_row = baseline_by_remote.get(record.remote_id.as_str()).copied();
        let current_row = current_by_remote.get(record.remote_id.as_str()).copied();

        match (baseline_row, current_row) {
            (Some(baseline_row), Some(current_row))
                if baseline_row.id == current_row.id && baseline_row == current_row =>
            {
                let updated_values = record.patch.apply(&current_row.values);
                if updated_values == current_row.values {
                    report.unchanged_or_stale += 1;
                } else {
                    update_owned_fields(
                        &transaction,
                        &baseline.book_id,
                        &current_row.id,
                        &record.patch,
                    )?;
                    report.updated += 1;
                }
            }
            (None, None) => {
                insert_contact(
                    &transaction,
                    &baseline.book_id,
                    &record.patch.insert_values(),
                )?;
                report.inserted += 1;
            }
            _ => {
                report.unchanged_or_stale += 1;
            }
        }
    }

    for (remote_id, baseline_row) in baseline_by_remote {
        if remote_ids.contains(remote_id) {
            continue;
        }

        match current_by_id.get(baseline_row.id.as_str()).copied() {
            Some(current_row) if current_row == baseline_row => {
                let deleted = transaction.execute(
                    "DELETE FROM contacts WHERE id = ?1 AND book_id = ?2",
                    params![baseline_row.id, baseline.book_id],
                )?;
                if deleted != 1 {
                    return Err(Error::Sync(format!(
                        "contact {} changed while deleting",
                        baseline_row.id
                    )));
                }
                report.deleted += 1;
            }
            _ => report.unchanged_or_stale += 1,
        }
    }

    transaction.commit()?;
    Ok(report)
}

fn verify_book_scope(
    conn: &Connection,
    account_id: &str,
    book_id: &str,
    expected_sync_type: &str,
) -> Result<()> {
    let actual = conn
        .query_row(
            "SELECT account_id, sync_type FROM contact_books WHERE id = ?1",
            params![book_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;

    match actual {
        Some((actual_account_id, actual_sync_type))
            if actual_account_id == account_id && actual_sync_type == expected_sync_type =>
        {
            Ok(())
        }
        Some((actual_account_id, actual_sync_type)) => Err(Error::Sync(format!(
            "contact book {book_id:?} scope changed: expected account {account_id:?} and sync type \
             {expected_sync_type:?}, found account {actual_account_id:?} and sync type \
             {actual_sync_type:?}"
        ))),
        None => Err(Error::Sync(format!(
            "contact book {book_id:?} no longer exists"
        ))),
    }
}

fn load_contacts(conn: &Connection, book_id: &str) -> Result<Vec<StoredContact>> {
    let mut statement = conn.prepare(
        "SELECT id, book_id, uid, display_name, emails_json, phones_json, addresses_json,
                organization, title, notes, vcard_data, remote_id, etag, created_at, updated_at
         FROM contacts WHERE book_id = ?1",
    )?;
    let contacts = statement
        .query_map(params![book_id], |row| {
            Ok(StoredContact {
                id: row.get(0)?,
                book_id: row.get(1)?,
                values: ContactValues {
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
                },
                created_at: row.get(13)?,
                updated_at: row.get(14)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(contacts)
}

fn index_managed_rows<'a>(
    rows: &'a [StoredContact],
    state: &str,
) -> Result<HashMap<&'a str, &'a StoredContact>> {
    let mut by_remote_id = HashMap::new();
    for row in rows {
        let Some(remote_id) = row
            .values
            .remote_id
            .as_deref()
            .filter(|remote_id| !remote_id.trim().is_empty())
        else {
            continue;
        };
        if by_remote_id.insert(remote_id, row).is_some() {
            return Err(Error::Sync(format!(
                "duplicate local managed remote ID {remote_id:?} in {state} contact rows"
            )));
        }
    }
    Ok(by_remote_id)
}

fn update_owned_fields(
    conn: &Connection,
    book_id: &str,
    contact_id: &str,
    patch: &RemoteContactPatch,
) -> Result<()> {
    let mut assignments = Vec::new();
    let mut values = Vec::new();

    push_nullable_update(&mut assignments, &mut values, "uid", &patch.uid);
    push_text_update(
        &mut assignments,
        &mut values,
        "display_name",
        &patch.display_name,
    );
    push_text_update(
        &mut assignments,
        &mut values,
        "emails_json",
        &patch.emails_json,
    );
    push_text_update(
        &mut assignments,
        &mut values,
        "phones_json",
        &patch.phones_json,
    );
    push_text_update(
        &mut assignments,
        &mut values,
        "addresses_json",
        &patch.addresses_json,
    );
    push_nullable_update(
        &mut assignments,
        &mut values,
        "organization",
        &patch.organization,
    );
    push_nullable_update(&mut assignments, &mut values, "title", &patch.title);
    push_nullable_update(&mut assignments, &mut values, "notes", &patch.notes);
    push_nullable_update(
        &mut assignments,
        &mut values,
        "vcard_data",
        &patch.vcard_data,
    );
    push_nullable_update(&mut assignments, &mut values, "remote_id", &patch.remote_id);
    push_nullable_update(&mut assignments, &mut values, "etag", &patch.etag);
    assignments.push("updated_at = CURRENT_TIMESTAMP".into());

    values.push(Value::Text(contact_id.into()));
    let contact_parameter = values.len();
    values.push(Value::Text(book_id.into()));
    let book_parameter = values.len();
    let sql = format!(
        "UPDATE contacts SET {} WHERE id = ?{contact_parameter} AND book_id = ?{book_parameter}",
        assignments.join(", ")
    );
    let updated = conn.execute(&sql, params_from_iter(values))?;
    if updated != 1 {
        return Err(Error::Sync(format!(
            "contact {contact_id:?} changed while updating"
        )));
    }
    Ok(())
}

fn push_text_update(
    assignments: &mut Vec<String>,
    values: &mut Vec<Value>,
    column: &str,
    field: &RemoteField<String>,
) {
    if let RemoteField::Set(value) = field {
        values.push(Value::Text(value.clone()));
        assignments.push(format!("{column} = ?{}", values.len()));
    }
}

fn push_nullable_update(
    assignments: &mut Vec<String>,
    values: &mut Vec<Value>,
    column: &str,
    field: &RemoteField<Option<String>>,
) {
    if let RemoteField::Set(value) = field {
        values.push(match value {
            Some(value) => Value::Text(value.clone()),
            None => Value::Null,
        });
        assignments.push(format!("{column} = ?{}", values.len()));
    }
}

fn insert_contact(conn: &Connection, book_id: &str, values: &ContactValues) -> Result<()> {
    conn.execute(
        "INSERT INTO contacts (
             id, book_id, uid, display_name, emails_json, phones_json, addresses_json,
             organization, title, notes, vcard_data, remote_id, etag
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            uuid::Uuid::new_v4().to_string(),
            book_id,
            values.uid,
            values.display_name,
            values.emails_json,
            values.phones_json,
            values.addresses_json,
            values.organization,
            values.title,
            values.notes,
            values.vcard_data,
            values.remote_id,
            values.etag,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE accounts (id TEXT PRIMARY KEY);
                 INSERT INTO accounts (id) VALUES ('account-a'), ('account-b');
                 CREATE TABLE contact_books (
                     id TEXT PRIMARY KEY,
                     account_id TEXT NOT NULL REFERENCES accounts(id),
                     name TEXT NOT NULL,
                     remote_id TEXT,
                     sync_type TEXT NOT NULL DEFAULT 'local',
                     created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                 );
                 CREATE TABLE contacts (
                     id TEXT PRIMARY KEY,
                     book_id TEXT NOT NULL REFERENCES contact_books(id) ON DELETE CASCADE,
                     uid TEXT,
                     display_name TEXT NOT NULL,
                     emails_json TEXT DEFAULT '[]',
                     phones_json TEXT DEFAULT '[]',
                     addresses_json TEXT DEFAULT '[]',
                     organization TEXT,
                     title TEXT,
                     notes TEXT,
                     vcard_data TEXT,
                     remote_id TEXT,
                     etag TEXT,
                     created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                     updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                 );
                 INSERT INTO contact_books (id, account_id, name, sync_type) VALUES
                     ('book-a', 'account-a', 'A', 'o365'),
                     ('book-a-other', 'account-a', 'A2', 'o365'),
                     ('book-b', 'account-b', 'B', 'o365');",
            )
            .unwrap();
        connection
    }

    fn add_contact(
        connection: &Connection,
        id: &str,
        book_id: &str,
        remote_id: Option<&str>,
        display_name: &str,
    ) {
        connection
            .execute(
                "INSERT INTO contacts (
                     id, book_id, display_name, emails_json, phones_json, addresses_json, remote_id
                 ) VALUES (?1, ?2, ?3, '[]', '[]', '[]', ?4)",
                params![id, book_id, display_name, remote_id],
            )
            .unwrap();
    }

    fn add_repair_contact(
        connection: &Connection,
        id: &str,
        book_id: &str,
        uid: Option<&str>,
        remote_id: Option<&str>,
        created_at: &str,
    ) {
        connection
            .execute(
                "INSERT INTO contacts (
                     id, book_id, uid, display_name, emails_json, phones_json, addresses_json,
                     organization, title, notes, vcard_data, remote_id, etag, created_at,
                     updated_at
                 ) VALUES (
                     ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                     '2000-01-01 00:00:00'
                 )",
                params![
                    id,
                    book_id,
                    uid,
                    format!("{id} name"),
                    format!(r#"[{{"email":"{id}@example.test"}}]"#),
                    format!(r#"[{{"number":"{id}"}}]"#),
                    format!(r#"[{{"address":"{id}"}}]"#),
                    format!("{id} organization"),
                    format!("{id} title"),
                    format!("{id} notes"),
                    format!("{id} vcard"),
                    remote_id,
                    format!("{id} etag"),
                    created_at,
                ],
            )
            .unwrap();
    }

    fn graph_patch(remote_id: &str, display_name: &str) -> RemoteContactPatch {
        RemoteContactPatch {
            uid: RemoteField::Preserve,
            display_name: RemoteField::Set(display_name.into()),
            emails_json: RemoteField::Set(format!(r#"[{{"email":"{remote_id}@example.test"}}]"#)),
            phones_json: RemoteField::Set("[]".into()),
            addresses_json: RemoteField::Preserve,
            organization: RemoteField::Set(Some("Remote Org".into())),
            title: RemoteField::Set(Some("Remote Title".into())),
            notes: RemoteField::Preserve,
            vcard_data: RemoteField::Preserve,
            remote_id: RemoteField::Set(Some(remote_id.into())),
            etag: RemoteField::Preserve,
        }
    }

    fn contact(connection: &Connection, book_id: &str, id: &str) -> Option<StoredContact> {
        load_contacts(connection, book_id)
            .unwrap()
            .into_iter()
            .find(|contact| contact.id == id)
    }

    fn contact_by_remote(
        connection: &Connection,
        book_id: &str,
        remote_id: &str,
    ) -> Option<StoredContact> {
        load_contacts(connection, book_id)
            .unwrap()
            .into_iter()
            .find(|contact| contact.values.remote_id.as_deref() == Some(remote_id))
    }

    #[test]
    fn graph_owned_fields_update_while_local_fields_are_preserved() {
        let mut connection = setup_db();
        connection
            .execute(
                "INSERT INTO contacts (
                     id, book_id, uid, display_name, emails_json, phones_json, addresses_json,
                     organization, title, notes, vcard_data, remote_id, etag
                 ) VALUES (
                     'local-id', 'book-a', 'local-uid', 'Old', '[1]', '[2]', '[3]',
                     'Old Org', 'Old Title', 'Local notes', 'VCARD', 'remote-1', 'etag-1'
                 )",
                [],
            )
            .unwrap();
        let baseline =
            capture_remote_id_baseline(&connection, "account-a", "book-a", "o365").unwrap();
        let mut patch = graph_patch("remote-1", "Remote Name");
        patch.organization = RemoteField::Set(None);
        patch.title = RemoteField::Set(None);
        let snapshot = CompleteRemoteIdSnapshot::new(vec![patch]).unwrap();

        let report = reconcile_remote_id_snapshot(&mut connection, &baseline, snapshot).unwrap();

        assert_eq!(report.updated, 1);
        let stored = contact(&connection, "book-a", "local-id").unwrap();
        assert_eq!(stored.id, "local-id");
        assert_eq!(stored.book_id, "book-a");
        assert_eq!(stored.values.uid.as_deref(), Some("local-uid"));
        assert_eq!(stored.values.display_name, "Remote Name");
        assert_eq!(
            stored.values.emails_json,
            r#"[{"email":"remote-1@example.test"}]"#
        );
        assert_eq!(stored.values.phones_json, "[]");
        assert_eq!(stored.values.addresses_json, "[3]");
        assert_eq!(stored.values.organization, None);
        assert_eq!(stored.values.title, None);
        assert_eq!(stored.values.notes.as_deref(), Some("Local notes"));
        assert_eq!(stored.values.vcard_data.as_deref(), Some("VCARD"));
        assert_eq!(stored.values.remote_id.as_deref(), Some("remote-1"));
        assert_eq!(stored.values.etag.as_deref(), Some("etag-1"));
    }

    #[test]
    fn insert_update_delete_and_repeat_are_idempotent() {
        let mut connection = setup_db();
        add_contact(&connection, "kept", "book-a", Some("keep"), "Old");
        add_contact(&connection, "removed", "book-a", Some("remove"), "Removed");
        add_contact(&connection, "local", "book-a", None, "Local only");
        let baseline =
            capture_remote_id_baseline(&connection, "account-a", "book-a", "o365").unwrap();
        let snapshot = CompleteRemoteIdSnapshot::new(vec![
            graph_patch("keep", "Updated"),
            graph_patch("new", "Inserted"),
        ])
        .unwrap();

        let report = reconcile_remote_id_snapshot(&mut connection, &baseline, snapshot).unwrap();

        assert_eq!(
            report,
            ReconcileReport {
                inserted: 1,
                updated: 1,
                deleted: 1,
                unchanged_or_stale: 0,
            }
        );
        assert_eq!(
            contact(&connection, "book-a", "kept")
                .unwrap()
                .values
                .display_name,
            "Updated"
        );
        assert!(contact(&connection, "book-a", "removed").is_none());
        assert!(contact(&connection, "book-a", "local").is_some());
        let inserted_id = contact_by_remote(&connection, "book-a", "new").unwrap().id;

        let next_baseline =
            capture_remote_id_baseline(&connection, "account-a", "book-a", "o365").unwrap();
        let same_snapshot = CompleteRemoteIdSnapshot::new(vec![
            graph_patch("keep", "Updated"),
            graph_patch("new", "Inserted"),
        ])
        .unwrap();
        let repeated =
            reconcile_remote_id_snapshot(&mut connection, &next_baseline, same_snapshot).unwrap();

        assert_eq!(
            repeated,
            ReconcileReport {
                unchanged_or_stale: 2,
                ..ReconcileReport::default()
            }
        );
        assert_eq!(
            contact_by_remote(&connection, "book-a", "new").unwrap().id,
            inserted_id
        );
    }

    #[test]
    fn validated_empty_snapshot_is_authoritative_but_preserves_local_only_rows() {
        let mut connection = setup_db();
        add_contact(&connection, "managed", "book-a", Some("managed"), "Managed");
        add_contact(&connection, "null", "book-a", None, "Null remote");
        add_contact(&connection, "empty", "book-a", Some(""), "Empty remote");
        let baseline =
            capture_remote_id_baseline(&connection, "account-a", "book-a", "o365").unwrap();
        let snapshot = CompleteRemoteIdSnapshot::new(Vec::new()).unwrap();

        let report = reconcile_remote_id_snapshot(&mut connection, &baseline, snapshot).unwrap();

        assert_eq!(report.deleted, 1);
        assert!(contact(&connection, "book-a", "managed").is_none());
        assert!(contact(&connection, "book-a", "null").is_some());
        assert!(contact(&connection, "book-a", "empty").is_some());
    }

    #[test]
    fn invalid_or_duplicate_snapshot_keys_fail_before_reconciliation() {
        let connection = setup_db();
        add_contact(&connection, "managed", "book-a", Some("managed"), "Before");

        for remote_id in ["", "   "] {
            assert!(CompleteRemoteIdSnapshot::new(vec![graph_patch(remote_id, "Bad")]).is_err());
        }
        let mut missing = graph_patch("placeholder", "Bad");
        missing.remote_id = RemoteField::Set(None);
        assert!(CompleteRemoteIdSnapshot::new(vec![missing]).is_err());
        let duplicate = CompleteRemoteIdSnapshot::new(vec![
            graph_patch("same", "One"),
            graph_patch("same", "Two"),
        ]);
        assert!(duplicate.is_err());
        assert_eq!(
            contact(&connection, "book-a", "managed")
                .unwrap()
                .values
                .display_name,
            "Before"
        );
    }

    #[test]
    fn book_scope_is_verified_at_capture_and_revalidated_before_writes() {
        let mut connection = setup_db();
        add_contact(&connection, "managed", "book-a", Some("managed"), "Before");
        assert!(capture_remote_id_baseline(&connection, "account-b", "book-a", "o365").is_err());
        assert!(capture_remote_id_baseline(&connection, "account-a", "missing", "o365").is_err());
        assert!(capture_remote_id_baseline(&connection, "account-a", "book-a", "google").is_err());

        let baseline =
            capture_remote_id_baseline(&connection, "account-a", "book-a", "o365").unwrap();
        connection
            .execute(
                "UPDATE contact_books SET account_id = 'account-b' WHERE id = 'book-a'",
                [],
            )
            .unwrap();
        let snapshot =
            CompleteRemoteIdSnapshot::new(vec![graph_patch("managed", "Remote")]).unwrap();
        assert!(reconcile_remote_id_snapshot(&mut connection, &baseline, snapshot).is_err());

        connection
            .execute(
                "UPDATE contact_books SET account_id = 'account-a', sync_type = 'google'
                 WHERE id = 'book-a'",
                [],
            )
            .unwrap();
        let snapshot =
            CompleteRemoteIdSnapshot::new(vec![graph_patch("managed", "Remote")]).unwrap();
        assert!(reconcile_remote_id_snapshot(&mut connection, &baseline, snapshot).is_err());
        assert_eq!(
            contact(&connection, "book-a", "managed")
                .unwrap()
                .values
                .display_name,
            "Before"
        );
    }

    #[test]
    fn reconciliation_is_book_isolated_even_for_the_same_remote_id() {
        let mut connection = setup_db();
        add_contact(&connection, "target", "book-a", Some("same"), "Target");
        add_contact(
            &connection,
            "other-book",
            "book-a-other",
            Some("same"),
            "Other book",
        );
        add_contact(
            &connection,
            "other-account",
            "book-b",
            Some("same"),
            "Other account",
        );
        let baseline =
            capture_remote_id_baseline(&connection, "account-a", "book-a", "o365").unwrap();
        let snapshot =
            CompleteRemoteIdSnapshot::new(vec![graph_patch("same", "Updated target")]).unwrap();

        reconcile_remote_id_snapshot(&mut connection, &baseline, snapshot).unwrap();

        assert_eq!(
            contact(&connection, "book-a", "target")
                .unwrap()
                .values
                .display_name,
            "Updated target"
        );
        assert_eq!(
            contact(&connection, "book-a-other", "other-book")
                .unwrap()
                .values
                .display_name,
            "Other book"
        );
        assert_eq!(
            contact(&connection, "book-b", "other-account")
                .unwrap()
                .values
                .display_name,
            "Other account"
        );
    }

    #[test]
    fn stale_baseline_protects_update_delete_and_new_remote_attachment() {
        let mut connection = setup_db();
        add_contact(&connection, "updated", "book-a", Some("u"), "Before");
        add_contact(&connection, "deleted", "book-a", Some("d"), "Before");
        add_contact(&connection, "attached", "book-a", None, "Local draft");
        let baseline =
            capture_remote_id_baseline(&connection, "account-a", "book-a", "o365").unwrap();

        connection
            .execute(
                "UPDATE contacts SET display_name = 'Local edit' WHERE id = 'updated'",
                [],
            )
            .unwrap();
        connection
            .execute("DELETE FROM contacts WHERE id = 'deleted'", [])
            .unwrap();
        connection
            .execute(
                "UPDATE contacts SET remote_id = 'a', display_name = 'Locally attached'
                 WHERE id = 'attached'",
                [],
            )
            .unwrap();
        let snapshot = CompleteRemoteIdSnapshot::new(vec![
            graph_patch("u", "Remote update"),
            graph_patch("d", "Remote restore"),
            graph_patch("a", "Remote attached"),
        ])
        .unwrap();

        let report = reconcile_remote_id_snapshot(&mut connection, &baseline, snapshot).unwrap();

        assert_eq!(report.unchanged_or_stale, 3);
        assert_eq!(report.inserted + report.updated + report.deleted, 0);
        assert_eq!(
            contact(&connection, "book-a", "updated")
                .unwrap()
                .values
                .display_name,
            "Local edit"
        );
        assert!(contact(&connection, "book-a", "deleted").is_none());
        assert_eq!(
            contact(&connection, "book-a", "attached")
                .unwrap()
                .values
                .display_name,
            "Locally attached"
        );
    }

    #[test]
    fn duplicate_repair_is_lossless_and_uses_uid_then_age_then_id() {
        let mut connection = setup_db();
        add_repair_contact(
            &connection,
            "uid-canonical",
            "book-a",
            Some("local-uid"),
            Some("uid-duplicate"),
            "2024-01-01 00:00:00",
        );
        add_repair_contact(
            &connection,
            "uid-older",
            "book-a",
            Some("   "),
            Some("uid-duplicate"),
            "2020-01-01 00:00:00",
        );
        add_repair_contact(
            &connection,
            "z-oldest",
            "book-a",
            None,
            Some("age-duplicate"),
            "2020-01-01 00:00:00",
        );
        add_repair_contact(
            &connection,
            "a-newer",
            "book-a",
            None,
            Some("age-duplicate"),
            "2021-01-01 00:00:00",
        );
        add_repair_contact(
            &connection,
            "a-tie",
            "book-a",
            None,
            Some("id-duplicate"),
            "2022-01-01 00:00:00",
        );
        add_repair_contact(
            &connection,
            "z-tie",
            "book-a",
            None,
            Some("id-duplicate"),
            "2022-01-01 00:00:00",
        );
        let before = load_contacts(&connection, "book-a").unwrap();

        let report =
            repair_duplicate_managed_remote_ids(&mut connection, "account-a", "book-a", "o365")
                .unwrap();

        assert_eq!(
            report,
            DuplicateRemoteIdRepairReport {
                duplicate_remote_ids: 3,
                detached: 3,
            }
        );
        assert_eq!(
            contact_by_remote(&connection, "book-a", "uid-duplicate")
                .unwrap()
                .id,
            "uid-canonical"
        );
        assert_eq!(
            contact_by_remote(&connection, "book-a", "age-duplicate")
                .unwrap()
                .id,
            "z-oldest"
        );
        assert_eq!(
            contact_by_remote(&connection, "book-a", "id-duplicate")
                .unwrap()
                .id,
            "a-tie"
        );

        let after = load_contacts(&connection, "book-a").unwrap();
        assert_eq!(after.len(), before.len());
        for before_contact in before {
            let after_contact = after
                .iter()
                .find(|contact| contact.id == before_contact.id)
                .unwrap();
            let was_detached = matches!(
                before_contact.id.as_str(),
                "uid-older" | "a-newer" | "z-tie"
            );
            let mut expected_values = before_contact.values.clone();
            if was_detached {
                expected_values.remote_id = None;
            }

            assert_eq!(after_contact.book_id, before_contact.book_id);
            assert_eq!(after_contact.values, expected_values);
            assert_eq!(after_contact.created_at, before_contact.created_at);
            if was_detached {
                assert_ne!(after_contact.updated_at, before_contact.updated_at);
            } else {
                assert_eq!(after_contact.updated_at, before_contact.updated_at);
            }
        }
    }

    #[test]
    fn duplicate_repair_verifies_scope_and_is_book_isolated() {
        let mut connection = setup_db();
        for (id, book_id) in [
            ("target-a", "book-a"),
            ("target-b", "book-a"),
            ("other-book-a", "book-a-other"),
            ("other-book-b", "book-a-other"),
            ("other-account-a", "book-b"),
            ("other-account-b", "book-b"),
        ] {
            add_repair_contact(
                &connection,
                id,
                book_id,
                None,
                Some("shared-remote"),
                "2020-01-01 00:00:00",
            );
        }
        let target_before = load_contacts(&connection, "book-a").unwrap();

        assert!(repair_duplicate_managed_remote_ids(
            &mut connection,
            "account-b",
            "book-a",
            "o365"
        )
        .is_err());
        assert!(repair_duplicate_managed_remote_ids(
            &mut connection,
            "account-a",
            "book-a",
            "google"
        )
        .is_err());
        assert!(repair_duplicate_managed_remote_ids(
            &mut connection,
            "account-a",
            "missing",
            "o365"
        )
        .is_err());
        assert_eq!(load_contacts(&connection, "book-a").unwrap(), target_before);

        let report =
            repair_duplicate_managed_remote_ids(&mut connection, "account-a", "book-a", "o365")
                .unwrap();

        assert_eq!(report.duplicate_remote_ids, 1);
        assert_eq!(report.detached, 1);
        assert_eq!(
            contact_by_remote(&connection, "book-a", "shared-remote")
                .unwrap()
                .id,
            "target-a"
        );
        for (book_id, ids) in [
            ("book-a-other", ["other-book-a", "other-book-b"]),
            ("book-b", ["other-account-a", "other-account-b"]),
        ] {
            for id in ids {
                assert_eq!(
                    contact(&connection, book_id, id)
                        .unwrap()
                        .values
                        .remote_id
                        .as_deref(),
                    Some("shared-remote")
                );
            }
        }
    }

    #[test]
    fn duplicate_free_and_blank_remote_ids_are_unchanged_by_repair() {
        let mut connection = setup_db();
        for (id, remote_id) in [
            ("unique-a", Some("remote-a")),
            ("unique-b", Some("remote-b")),
            ("null", None),
            ("empty-a", Some("")),
            ("empty-b", Some("")),
            ("blank-a", Some("  \t")),
            ("blank-b", Some("  \t")),
        ] {
            add_repair_contact(
                &connection,
                id,
                "book-a",
                None,
                remote_id,
                "2020-01-01 00:00:00",
            );
        }
        let mut before = load_contacts(&connection, "book-a").unwrap();
        before.sort_by(|left, right| left.id.cmp(&right.id));

        let report =
            repair_duplicate_managed_remote_ids(&mut connection, "account-a", "book-a", "o365")
                .unwrap();

        let mut after = load_contacts(&connection, "book-a").unwrap();
        after.sort_by(|left, right| left.id.cmp(&right.id));
        assert_eq!(report, DuplicateRemoteIdRepairReport::default());
        assert_eq!(after, before);
    }

    #[test]
    fn reconciliation_succeeds_after_duplicate_repair() {
        let mut connection = setup_db();
        add_repair_contact(
            &connection,
            "local-created",
            "book-a",
            Some("local-uid"),
            Some("same"),
            "2024-01-01 00:00:00",
        );
        add_repair_contact(
            &connection,
            "sync-created",
            "book-a",
            None,
            Some("same"),
            "2020-01-01 00:00:00",
        );

        let repair =
            repair_duplicate_managed_remote_ids(&mut connection, "account-a", "book-a", "o365")
                .unwrap();
        let baseline =
            capture_remote_id_baseline(&connection, "account-a", "book-a", "o365").unwrap();
        let snapshot = CompleteRemoteIdSnapshot::new(vec![graph_patch("same", "Remote")]).unwrap();

        let report = reconcile_remote_id_snapshot(&mut connection, &baseline, snapshot).unwrap();

        assert_eq!(repair.detached, 1);
        assert_eq!(
            report,
            ReconcileReport {
                updated: 1,
                ..ReconcileReport::default()
            }
        );
        let canonical = contact(&connection, "book-a", "local-created").unwrap();
        assert_eq!(canonical.values.remote_id.as_deref(), Some("same"));
        assert_eq!(canonical.values.uid.as_deref(), Some("local-uid"));
        assert_eq!(canonical.values.display_name, "Remote");
        let detached = contact(&connection, "book-a", "sync-created").unwrap();
        assert_eq!(detached.values.remote_id, None);
        assert_eq!(detached.values.display_name, "sync-created name");
        assert_eq!(load_contacts(&connection, "book-a").unwrap().len(), 2);
    }

    #[test]
    fn duplicate_repair_propagates_errors_and_rolls_back_all_detachments() {
        let mut connection = setup_db();
        add_repair_contact(
            &connection,
            "canonical",
            "book-a",
            Some("local-uid"),
            Some("same"),
            "2024-01-01 00:00:00",
        );
        add_repair_contact(
            &connection,
            "loser-a",
            "book-a",
            None,
            Some("same"),
            "2020-01-01 00:00:00",
        );
        add_repair_contact(
            &connection,
            "loser-b",
            "book-a",
            None,
            Some("same"),
            "2021-01-01 00:00:00",
        );
        connection
            .execute_batch(
                "CREATE TRIGGER fail_duplicate_detach
                 BEFORE UPDATE OF remote_id ON contacts
                 WHEN OLD.id = 'loser-b' AND NEW.remote_id IS NULL
                 BEGIN
                     SELECT RAISE(ABORT, 'injected duplicate repair failure');
                 END;",
            )
            .unwrap();
        let mut before = load_contacts(&connection, "book-a").unwrap();
        before.sort_by(|left, right| left.id.cmp(&right.id));

        let error =
            repair_duplicate_managed_remote_ids(&mut connection, "account-a", "book-a", "o365")
                .unwrap_err();

        let mut after = load_contacts(&connection, "book-a").unwrap();
        after.sort_by(|left, right| left.id.cmp(&right.id));
        assert!(matches!(error, Error::Database(_)));
        assert_eq!(after, before);
    }

    #[test]
    fn duplicate_local_managed_ids_abort_before_mutation() {
        let mut connection = setup_db();
        add_contact(&connection, "one", "book-a", Some("same"), "One");
        add_contact(&connection, "two", "book-a", Some("same"), "Two");
        let baseline =
            capture_remote_id_baseline(&connection, "account-a", "book-a", "o365").unwrap();
        let snapshot = CompleteRemoteIdSnapshot::new(vec![graph_patch("same", "Remote")]).unwrap();

        let error = reconcile_remote_id_snapshot(&mut connection, &baseline, snapshot).unwrap_err();

        assert!(error
            .to_string()
            .contains("duplicate local managed remote ID"));
        assert_eq!(
            contact(&connection, "book-a", "one")
                .unwrap()
                .values
                .display_name,
            "One"
        );
        assert_eq!(
            contact(&connection, "book-a", "two")
                .unwrap()
                .values
                .display_name,
            "Two"
        );
    }

    #[test]
    fn injected_mid_batch_failure_rolls_back_every_change() {
        let mut connection = setup_db();
        add_contact(
            &connection,
            "existing",
            "book-a",
            Some("existing"),
            "Before",
        );
        let baseline =
            capture_remote_id_baseline(&connection, "account-a", "book-a", "o365").unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_contact_insert
                 BEFORE INSERT ON contacts
                 WHEN NEW.remote_id = 'boom'
                 BEGIN
                     SELECT RAISE(ABORT, 'injected contact failure');
                 END;",
            )
            .unwrap();
        let snapshot = CompleteRemoteIdSnapshot::new(vec![
            graph_patch("existing", "Updated"),
            graph_patch("inserted-first", "Inserted first"),
            graph_patch("boom", "Fails"),
        ])
        .unwrap();

        assert!(reconcile_remote_id_snapshot(&mut connection, &baseline, snapshot).is_err());

        assert_eq!(
            contact(&connection, "book-a", "existing")
                .unwrap()
                .values
                .display_name,
            "Before"
        );
        assert!(contact_by_remote(&connection, "book-a", "inserted-first").is_none());
        assert!(contact_by_remote(&connection, "book-a", "boom").is_none());
    }
}
