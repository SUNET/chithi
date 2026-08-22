//! Google contact backend (People API v1).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::contact::reconcile::{
    reconcile_remote_id_delta_into_new_book_with_postcheck,
    reconcile_remote_id_delta_with_repair_and_postcheck, DuplicateRemoteIdRepairReport,
    RemoteContactPatch, RemoteField, RemoteIdDelta,
};
use crate::contact::Contact;
use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::mail::google::{
    contact_to_person_json, GoogleContact, GoogleContactChange, GoogleContactChanges,
    GoogleContactLookup, GoogleContactsSync,
};

use super::{BookRef, ContactBackend, ContactBackendCtx, PushedContact};

pub struct GoogleContactBackend;

const ABSENCE_CONFIRMATION_DELAY_SECONDS: i64 = 10 * 60;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct GoogleContactSyncState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sync_token: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pending_absences: BTreeMap<String, i64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pending_recoveries: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl GoogleContactBackend {
    async fn sync_inner(&self, ctx: &ContactBackendCtx<'_>, account: &AccountFull) -> Result<()> {
        let account_id = account.id.as_str();
        let state_key = google_contact_state_key(account_id);
        let (selected_book_id, known_remote_ids, mut state) = {
            let conn = ctx.db.reader();
            let state = load_google_contact_state(&conn, &state_key)?;
            match find_google_book(&conn, account_id)? {
                Some(book_id) => {
                    let known_remote_ids = load_google_remote_ids(&conn, &book_id)?;
                    (Some(book_id), known_remote_ids, state)
                }
                None => (None, Vec::new(), state),
            }
        };
        if selected_book_id.is_none() {
            state = GoogleContactSyncState::default();
        }
        let requested_sync_token = if state.pending_recoveries > 0 {
            None
        } else {
            state.sync_token.clone()
        };
        let client = ctx.providers.google_client(account_id).await?;
        let (changes, full_sync) = match client
            .list_contact_changes(requested_sync_token.as_deref())
            .await?
        {
            GoogleContactsSync::Changes(changes) => (changes, requested_sync_token.is_none()),
            GoogleContactsSync::SyncTokenExpired => {
                log::info!(
                    "sync_contacts_google: contact sync token expired for account {}; reseeding",
                    account_id
                );
                match client.list_contact_changes(None).await? {
                    GoogleContactsSync::Changes(changes) => (changes, true),
                    GoogleContactsSync::SyncTokenExpired => {
                        return Err(Error::Sync(
                            "Google People full contact sync unexpectedly expired".into(),
                        ));
                    }
                }
            }
        };
        let lookup_resource_names = google_lookup_resource_names(
            &changes.changes,
            &known_remote_ids,
            &state.pending_absences,
            full_sync,
        )?;
        let lookups = client.get_contacts_batch(&lookup_resource_names).await?;
        let (delta, next_state) = prepare_google_delta(
            changes,
            lookups,
            &known_remote_ids,
            state,
            chrono::Utc::now().timestamp(),
        )?;
        log::info!(
            "sync_contacts_google: validated {} detailed contacts",
            lookup_resource_names.len()
        );

        let (book_id, repair, report) = {
            let mut conn = ctx.db.writer().await;
            match selected_book_id {
                Some(book_id) => {
                    let (repair, report, ()) = reconcile_remote_id_delta_with_repair_and_postcheck(
                        &mut conn,
                        account_id,
                        &book_id,
                        "google",
                        delta,
                        |transaction| {
                            save_google_contact_state(transaction, &state_key, &next_state)
                        },
                    )?;
                    (book_id, repair, report)
                }
                None => {
                    if find_google_book(&conn, account_id)?.is_some() {
                        return Err(Error::Sync(
                            "Google contact book appeared during first sync; retry is required"
                                .into(),
                        ));
                    }
                    let book_id = uuid::Uuid::new_v4().to_string();
                    let (report, ()) = reconcile_remote_id_delta_into_new_book_with_postcheck(
                        &mut conn,
                        account_id,
                        &book_id,
                        "Google Contacts",
                        "google",
                        delta,
                        |transaction| {
                            save_google_contact_state(transaction, &state_key, &next_state)
                        },
                    )?;
                    (book_id, DuplicateRemoteIdRepairReport::default(), report)
                }
            }
        };
        if repair.detached > 0 {
            log::warn!(
                "sync_contacts_google: detached {} duplicate managed remote ID assignments across \
                 {} remote IDs in book {}",
                repair.detached,
                repair.duplicate_remote_ids,
                book_id
            );
        }
        log::info!(
            "sync_contacts_google: reconciled inserted={}, updated={}, deleted={}, \
             unchanged_or_stale={}",
            report.inserted,
            report.updated,
            report.deleted,
            report.unchanged_or_stale
        );
        log::info!("sync_contacts_google: completed for account {}", account_id);
        Ok(())
    }
}

#[async_trait]
impl ContactBackend for GoogleContactBackend {
    fn protocol(&self) -> &'static str {
        "google"
    }

    fn prepare_local_mutation(&self, conn: &Connection, account: &AccountFull) -> Result<()> {
        if find_google_book(conn, &account.id)?.is_none() {
            return Err(Error::Sync(
                "Google contact mutation has no Google contact book".into(),
            ));
        }
        let key = google_contact_state_key(&account.id);
        let mut state = load_google_contact_state(conn, &key)?;
        state.pending_recoveries = state
            .pending_recoveries
            .checked_add(1)
            .ok_or_else(|| Error::Sync("Google contact recovery counter overflowed".into()))?;
        save_google_contact_state(conn, &key, &state)
    }

    fn complete_local_mutation(&self, conn: &Connection, account: &AccountFull) -> Result<()> {
        let key = google_contact_state_key(&account.id);
        let mut state = load_google_contact_state(conn, &key)?;
        state.pending_recoveries = state.pending_recoveries.checked_sub(1).ok_or_else(|| {
            Error::Sync("Google contact recovery completion has no pending mutation".into())
        })?;
        save_google_contact_state(conn, &key, &state)
    }

    /// People API sync. Failures are swallowed with a warning — Gmail
    /// accounts without calendar/contacts OAuth consent would
    /// otherwise fail every contacts sync outright.
    async fn sync(&self, ctx: &ContactBackendCtx<'_>, account: &AccountFull) -> Result<()> {
        if let Err(e) = self.sync_inner(ctx, account).await {
            log::warn!(
                "sync_contacts: Gmail People API failed (OAuth may not be set up): {}",
                e
            );
        }
        Ok(())
    }

    async fn push_created_contact(
        &self,
        ctx: &ContactBackendCtx<'_>,
        account: &AccountFull,
        _book: &BookRef<'_>,
        contact: &Contact,
    ) -> Result<Option<PushedContact>> {
        let client = ctx.providers.google_client(&account.id).await?;
        let person = contact_to_person_json(
            &contact.display_name,
            &contact.emails_json,
            &contact.phones_json,
            contact.organization.as_deref(),
            contact.title.as_deref(),
        )?;
        let pushed = client.create_contact(&person).await?;
        Ok(Some(PushedContact {
            remote_id: Some(pushed.resource_name),
            etag: Some(pushed.contact_source_etag),
            vcard: None,
        }))
    }

    async fn push_updated_contact(
        &self,
        ctx: &ContactBackendCtx<'_>,
        account: &AccountFull,
        _book: &BookRef<'_>,
        contact: &Contact,
        remote_id: &str,
    ) -> Result<Option<PushedContact>> {
        let client = ctx.providers.google_client(&account.id).await?;
        let person = contact_to_person_json(
            &contact.display_name,
            &contact.emails_json,
            &contact.phones_json,
            contact.organization.as_deref(),
            contact.title.as_deref(),
        )?;
        let pushed = client
            .update_contact(remote_id, contact.etag.as_deref(), &person)
            .await?;
        Ok(Some(PushedContact {
            remote_id: Some(pushed.resource_name),
            etag: Some(pushed.contact_source_etag),
            vcard: None,
        }))
    }

    async fn push_deleted_contact(
        &self,
        ctx: &ContactBackendCtx<'_>,
        account: &AccountFull,
        remote_id: &str,
    ) -> Result<()> {
        let client = ctx.providers.google_client(&account.id).await?;
        client.delete_contact(remote_id).await?;
        Ok(())
    }
}

pub(crate) fn google_contact_state_key(account_id: &str) -> String {
    format!("google_contact_sync_state:{account_id}")
}

fn load_google_contact_state(conn: &Connection, key: &str) -> Result<GoogleContactSyncState> {
    let value = conn
        .query_row(
            "SELECT value FROM app_metadata WHERE key = ?1",
            [key],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let state = match value {
        Some(value) => serde_json::from_str(&value).map_err(|error| {
            Error::Sync(format!(
                "persisted Google contact sync state is invalid: {error}"
            ))
        })?,
        None => GoogleContactSyncState::default(),
    };
    validate_google_contact_state(&state)?;
    Ok(state)
}

fn validate_google_contact_state(state: &GoogleContactSyncState) -> Result<()> {
    if state
        .sync_token
        .as_deref()
        .is_some_and(|token| token.trim().is_empty())
    {
        return Err(Error::Sync(
            "persisted Google contact sync token is blank".into(),
        ));
    }
    for (resource_name, first_observed) in &state.pending_absences {
        if !is_people_resource_name(resource_name) || *first_observed < 0 {
            return Err(Error::Sync(
                "persisted Google contact absence candidate is invalid".into(),
            ));
        }
    }
    Ok(())
}

fn save_google_contact_state(
    conn: &Connection,
    key: &str,
    state: &GoogleContactSyncState,
) -> Result<()> {
    validate_google_contact_state(state)?;
    let value = serde_json::to_string(state)
        .map_err(|error| Error::Sync(format!("Google contact state encoding failed: {error}")))?;
    let written = conn.execute(
        "INSERT OR REPLACE INTO app_metadata (key, value) VALUES (?1, ?2)",
        rusqlite::params![key, value],
    )?;
    if written != 1 {
        return Err(Error::Sync(
            "Google contact sync state write affected an unexpected number of rows".into(),
        ));
    }
    Ok(())
}

fn is_people_resource_name(value: &str) -> bool {
    value
        .strip_prefix("people/")
        .is_some_and(|suffix| !suffix.trim().is_empty() && !suffix.contains('/'))
}

fn google_lookup_resource_names(
    changes: &[GoogleContactChange],
    known_remote_ids: &[String],
    pending_absences: &BTreeMap<String, i64>,
    full_sync: bool,
) -> Result<Vec<String>> {
    validate_google_change_identities(changes)?;
    let known: HashSet<&str> = known_remote_ids.iter().map(String::as_str).collect();
    let mut names = BTreeSet::new();
    let mut alive = BTreeSet::new();
    let mut deleted = BTreeSet::new();
    for change in changes {
        if change.deleted {
            deleted.insert(change.resource_name.clone());
            deleted.extend(change.previous_resource_names.iter().cloned());
        } else {
            alive.insert(change.resource_name.clone());
            names.insert(change.resource_name.clone());
        }
    }
    if alive.iter().any(|identity| deleted.contains(identity)) {
        return Err(Error::Sync(
            "Google contact changes contain conflicting live and deleted identities".into(),
        ));
    }
    if full_sync {
        names.extend(known_remote_ids.iter().cloned());
    } else {
        names.extend(
            pending_absences
                .keys()
                .filter(|identity| known.contains(identity.as_str()))
                .cloned(),
        );
    }
    names.retain(|identity| !deleted.contains(identity));
    if alive.iter().any(|identity| !names.contains(identity)) {
        return Err(Error::Sync(
            "Google contact lookup planning removed a live identity".into(),
        ));
    }
    Ok(names.into_iter().collect())
}

fn validate_google_change_identities(changes: &[GoogleContactChange]) -> Result<()> {
    let mut claimed = HashMap::new();
    for (index, change) in changes.iter().enumerate() {
        for identity in
            std::iter::once(&change.resource_name).chain(change.previous_resource_names.iter())
        {
            if claimed.insert(identity.as_str(), index).is_some() {
                return Err(Error::Sync(format!(
                    "Google contact changes contain a duplicate or crossed identity {identity:?}"
                )));
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct GoogleContactAggregate {
    contact: GoogleContact,
    aliases: BTreeSet<String>,
}

fn prepare_google_delta(
    changes: GoogleContactChanges,
    lookups: Vec<GoogleContactLookup>,
    known_remote_ids: &[String],
    mut state: GoogleContactSyncState,
    now: i64,
) -> Result<(RemoteIdDelta, GoogleContactSyncState)> {
    if now < 0 {
        return Err(Error::Sync(
            "Google contact sync clock is before the Unix epoch".into(),
        ));
    }
    validate_google_change_identities(&changes.changes)?;
    let known: HashSet<&str> = known_remote_ids.iter().map(String::as_str).collect();
    state
        .pending_absences
        .retain(|identity, _| known.contains(identity.as_str()));
    let previous_absences = state.pending_absences.clone();

    let mut live_changes = HashMap::new();
    let mut deleted_identities = BTreeSet::new();
    for change in changes.changes {
        if change.deleted {
            deleted_identities.insert(change.resource_name);
            deleted_identities.extend(change.previous_resource_names);
        } else if live_changes
            .insert(change.resource_name.clone(), change)
            .is_some()
        {
            return Err(Error::Sync(
                "Google contact delta contains a duplicate live identity".into(),
            ));
        }
    }

    let mut aggregates: HashMap<String, GoogleContactAggregate> = HashMap::new();
    let mut lookup_actual_ids = HashMap::with_capacity(lookups.len());
    let mut absent = BTreeSet::new();
    for lookup in lookups {
        if lookup_actual_ids.contains_key(&lookup.requested_resource_name)
            || absent.contains(&lookup.requested_resource_name)
        {
            return Err(Error::Sync(
                "Google contact details contain a duplicate correlation identity".into(),
            ));
        }
        match lookup.contact {
            Some(contact) => {
                if deleted_identities.contains(&lookup.requested_resource_name)
                    || deleted_identities.contains(&contact.resource_name)
                {
                    return Err(Error::Sync(
                        "Google contact details conflict with a deletion tombstone".into(),
                    ));
                }
                let actual_id = contact.resource_name.clone();
                lookup_actual_ids.insert(lookup.requested_resource_name.clone(), actual_id.clone());
                state
                    .pending_absences
                    .remove(&lookup.requested_resource_name);
                state.pending_absences.remove(&actual_id);
                let aggregate =
                    aggregates
                        .entry(actual_id.clone())
                        .or_insert_with(|| GoogleContactAggregate {
                            contact: contact.clone(),
                            aliases: BTreeSet::new(),
                        });
                if aggregate.contact != contact {
                    return Err(Error::Sync(format!(
                        "Google People returned inconsistent details for {actual_id:?}"
                    )));
                }
                if lookup.requested_resource_name != actual_id {
                    aggregate.aliases.insert(lookup.requested_resource_name);
                }
            }
            None => {
                absent.insert(lookup.requested_resource_name);
            }
        }
    }

    for (listed_id, change) in live_changes {
        let actual_id = lookup_actual_ids.get(&listed_id).ok_or_else(|| {
            Error::Sync(format!(
                "Google People omitted details for live contact {listed_id:?}"
            ))
        })?;
        let aggregate = aggregates.get_mut(actual_id).ok_or_else(|| {
            Error::Sync("Google contact detail correlation lost its aggregate".into())
        })?;
        if listed_id != *actual_id {
            aggregate.aliases.insert(listed_id);
        }
        for previous_resource_name in change.previous_resource_names {
            state.pending_absences.remove(&previous_resource_name);
            aggregate.aliases.insert(previous_resource_name);
        }
    }

    let claimed_aliases: HashSet<&str> = aggregates
        .values()
        .flat_map(|aggregate| aggregate.aliases.iter().map(String::as_str))
        .collect();
    for identity in &deleted_identities {
        state.pending_absences.remove(identity);
    }
    for identity in absent {
        if claimed_aliases.contains(identity.as_str()) {
            state.pending_absences.remove(&identity);
            continue;
        }
        if !known.contains(identity.as_str()) {
            return Err(Error::Sync(format!(
                "Google People reported unexpected absent identity {identity:?}"
            )));
        }
        match previous_absences.get(&identity) {
            Some(first_observed)
                if now >= *first_observed
                    && now - *first_observed >= ABSENCE_CONFIRMATION_DELAY_SECONDS =>
            {
                state.pending_absences.remove(&identity);
                deleted_identities.insert(identity);
            }
            Some(_) => {}
            None => {
                state.pending_absences.insert(identity, now);
            }
        }
    }

    let upserts = aggregates
        .into_values()
        .map(|aggregate| {
            let GoogleContactAggregate { contact, aliases } = aggregate;
            let patch = RemoteContactPatch {
                uid: RemoteField::Preserve,
                display_name: RemoteField::Set(contact.display_name),
                emails_json: RemoteField::Set(contact.emails_json),
                phones_json: RemoteField::Set(contact.phones_json),
                addresses_json: RemoteField::Preserve,
                organization: RemoteField::Set(contact.organization),
                title: RemoteField::Set(contact.title),
                notes: RemoteField::Preserve,
                vcard_data: RemoteField::Preserve,
                remote_id: RemoteField::Set(Some(contact.resource_name)),
                etag: RemoteField::Set(Some(contact.contact_source_etag)),
            };
            (patch, aliases.into_iter().collect())
        })
        .collect();
    state.sync_token = Some(changes.next_sync_token);
    state.pending_recoveries = 0;
    let delta = RemoteIdDelta::new(upserts, deleted_identities.into_iter().collect())?;
    Ok((delta, state))
}

fn find_google_book(conn: &Connection, account_id: &str) -> Result<Option<String>> {
    let mut statement = conn.prepare(
        "SELECT id FROM contact_books
         WHERE account_id = ?1 AND sync_type = 'google'
         ORDER BY created_at, id",
    )?;
    let books = statement
        .query_map(rusqlite::params![account_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if books.len() > 1 {
        return Err(Error::Sync(format!(
            "account {account_id:?} has multiple Google contact books"
        )));
    }
    Ok(books.into_iter().next())
}

fn load_google_remote_ids(conn: &Connection, book_id: &str) -> Result<Vec<String>> {
    let mut statement = conn.prepare(
        "SELECT remote_id FROM contacts
         WHERE book_id = ?1 AND remote_id IS NOT NULL",
    )?;
    let remote_ids = statement
        .query_map([book_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<BTreeSet<_>, _>>()?;
    Ok(remote_ids
        .into_iter()
        .filter(|remote_id| !remote_id.trim().is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::contact::reconcile::ReconcileReport;
    use crate::provider::{
        GraphTokenPurpose, MailCredentials, OAuthTokenStore, ProviderCredentials, ProviderServices,
        ProviderTransports, TokenEndpointClient,
    };
    use async_trait::async_trait;
    use rusqlite::OptionalExtension;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    struct TestCredentials {
        fail_google: bool,
    }

    #[async_trait]
    impl ProviderCredentials for TestCredentials {
        async fn google_access_token(&self, _account_id: &str) -> Result<String> {
            if self.fail_google {
                Err(Error::Other("injected Google credential failure".into()))
            } else {
                Ok("test-google-token".into())
            }
        }

        async fn graph_access_token(
            &self,
            _account_id: &str,
            _purpose: GraphTokenPurpose,
        ) -> Result<String> {
            Err(Error::Other("unused Graph credentials".into()))
        }

        async fn mail_credentials_for(
            &self,
            _account: &crate::account::MailAccountConfig,
        ) -> Result<MailCredentials> {
            Err(Error::Other("unused mail credentials".into()))
        }

        async fn jmap_config_for(
            &self,
            _account: &crate::account::MailAccountConfig,
        ) -> Result<crate::mail::jmap::JmapConfig> {
            Err(Error::Other("unused JMAP credentials".into()))
        }

        async fn jmap_push_access_token(
            &self,
            _account_id: &str,
            _token_endpoint: &str,
            _client_id: &str,
        ) -> Result<Option<String>> {
            Err(Error::Other("unused JMAP push credentials".into()))
        }

        async fn zoom_access_token(&self, _account_id: &str) -> Result<String> {
            Err(Error::Other("unused Zoom credentials".into()))
        }

        async fn matrix_access_token(&self, _account_id: &str) -> Result<String> {
            Err(Error::Other("unused Matrix credentials".into()))
        }

        async fn talk_app_password(&self, _account_id: &str) -> Result<String> {
            Err(Error::Other("unused Talk credentials".into()))
        }
    }

    struct EmptyTokenStore;

    impl OAuthTokenStore for EmptyTokenStore {
        fn load(&self, _account_id: &str) -> Result<Option<crate::oauth::OAuthTokens>> {
            Ok(None)
        }

        fn store(&self, _account_id: &str, _tokens: &crate::oauth::OAuthTokens) -> Result<()> {
            Ok(())
        }

        fn delete(&self, _account_id: &str) -> Result<()> {
            Ok(())
        }
    }

    struct UnusedTokenEndpoint;

    #[async_trait]
    impl TokenEndpointClient for UnusedTokenEndpoint {
        async fn exchange_code(
            &self,
            _provider: &crate::oauth::OAuthProvider,
            _code: &str,
            _port: u16,
            _code_verifier: Option<&str>,
        ) -> Result<crate::oauth::OAuthTokens> {
            Err(Error::Other("unused token exchange".into()))
        }

        async fn refresh(
            &self,
            _provider: &crate::oauth::OAuthProvider,
            _refresh_token: &str,
        ) -> Result<crate::oauth::OAuthTokens> {
            Err(Error::Other("unused token refresh".into()))
        }

        async fn refresh_scoped(
            &self,
            _provider: &crate::oauth::OAuthProvider,
            _refresh_token: &str,
            _scopes: &str,
        ) -> Result<crate::oauth::OAuthTokens> {
            Err(Error::Other("unused scoped token refresh".into()))
        }

        async fn refresh_dynamic(
            &self,
            _token_url: &str,
            _refresh_token: &str,
            _client_id: &str,
        ) -> Result<crate::oauth::OAuthTokens> {
            Err(Error::Other("unused dynamic token refresh".into()))
        }
    }

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
                  CREATE TABLE app_metadata (
                      key TEXT PRIMARY KEY,
                      value TEXT NOT NULL
                  );",
            )
            .unwrap();
        connection
    }

    fn wire_person(id: &str) -> serde_json::Value {
        serde_json::json!({
            "resourceName": format!("people/{id}"),
            "etag": format!("person-etag-{id}"),
            "metadata": {"sources": [{
                "type": "CONTACT",
                "id": format!("source-{id}"),
                "etag": format!("source-etag-{id}"),
            }]},
            "names": [{
                "displayName": format!("Name {id}"),
                "metadata": {"primary": true},
            }],
            "emailAddresses": [{
                "value": format!("{id}@example.test"),
                "type": "work",
                "metadata": {"primary": true},
            }],
            "phoneNumbers": [],
            "organizations": [],
        })
    }

    async fn serve_people_responses(
        responses: Vec<(u16, String)>,
    ) -> (String, oneshot::Receiver<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}/people-api", listener.local_addr().unwrap());
        let (requests_tx, requests_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut requests = Vec::with_capacity(responses.len());
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0, "request ended before its headers");
                    bytes.extend_from_slice(&chunk[..count]);
                    if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                let reason = if status == 200 { "OK" } else { "Error" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests_tx.send(requests).unwrap();
        });
        (root, requests_rx)
    }

    fn test_provider_services(people_api_root: &str, fail_google: bool) -> ProviderServices {
        let mut transports = ProviderTransports::production().unwrap();
        transports.google_http = reqwest::Client::builder().no_proxy().build().unwrap();
        transports.google_endpoints.people_api_root = people_api_root.into();
        let token_store: Arc<dyn OAuthTokenStore> = Arc::new(EmptyTokenStore);
        ProviderServices::new(
            Arc::new(TestCredentials { fail_google }),
            token_store,
            Arc::new(UnusedTokenEndpoint),
            transports,
        )
    }

    fn remote(resource_name: &str, display_name: &str) -> GoogleContact {
        GoogleContact {
            resource_name: resource_name.into(),
            contact_source_etag: format!("etag-{resource_name}"),
            display_name: display_name.into(),
            emails_json: format!(r#"[{{"email":"{display_name}@example.test"}}]"#),
            phones_json: r#"[{"number":"+46"}]"#.into(),
            organization: Some("Remote Org".into()),
            title: Some("Remote Title".into()),
        }
    }

    fn add_book(
        connection: &Connection,
        id: &str,
        account_id: &str,
        sync_type: &str,
        created_at: &str,
    ) {
        connection
            .execute(
                "INSERT INTO contact_books
                     (id, account_id, name, sync_type, created_at)
                 VALUES (?1, ?2, ?1, ?3, ?4)",
                rusqlite::params![id, account_id, sync_type, created_at],
            )
            .unwrap();
    }

    fn add_contact(
        connection: &Connection,
        id: &str,
        book_id: &str,
        uid: Option<&str>,
        remote_id: Option<&str>,
        display_name: &str,
    ) {
        connection
            .execute(
                r#"INSERT INTO contacts (
                     id, book_id, uid, display_name, emails_json, phones_json, addresses_json,
                     organization, title, notes, vcard_data, remote_id, etag
                 ) VALUES (
                     ?1, ?2, ?3, ?4, '[{"email":"local@example.test"}]',
                     '[{"number":"local"}]', '[{"address":"Local"}]', 'Local Org',
                     'Local Title', 'Local notes', 'LOCAL VCARD', ?5, 'local-etag'
                 )"#,
                rusqlite::params![id, book_id, uid, display_name, remote_id],
            )
            .unwrap();
    }

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

    fn contact_values(connection: &Connection, id: &str) -> Option<ContactValues> {
        connection
            .query_row(
                "SELECT uid, display_name, emails_json, phones_json, addresses_json,
                        organization, title, notes, vcard_data, remote_id, etag
                 FROM contacts WHERE id = ?1",
                [id],
                |row| {
                    Ok(ContactValues {
                        uid: row.get(0)?,
                        display_name: row.get(1)?,
                        emails_json: row.get(2)?,
                        phones_json: row.get(3)?,
                        addresses_json: row.get(4)?,
                        organization: row.get(5)?,
                        title: row.get(6)?,
                        notes: row.get(7)?,
                        vcard_data: row.get(8)?,
                        remote_id: row.get(9)?,
                        etag: row.get(10)?,
                    })
                },
            )
            .optional()
            .unwrap()
    }

    fn live_change(resource_name: &str, previous: &[&str]) -> GoogleContactChange {
        GoogleContactChange {
            resource_name: resource_name.into(),
            previous_resource_names: previous.iter().map(|value| (*value).into()).collect(),
            deleted: false,
        }
    }

    fn deleted_change(resource_name: &str, previous: &[&str]) -> GoogleContactChange {
        GoogleContactChange {
            resource_name: resource_name.into(),
            previous_resource_names: previous.iter().map(|value| (*value).into()).collect(),
            deleted: true,
        }
    }

    fn found(requested_resource_name: &str, contact: GoogleContact) -> GoogleContactLookup {
        GoogleContactLookup {
            requested_resource_name: requested_resource_name.into(),
            contact: Some(contact),
        }
    }

    fn missing(requested_resource_name: &str) -> GoogleContactLookup {
        GoogleContactLookup {
            requested_resource_name: requested_resource_name.into(),
            contact: None,
        }
    }

    fn apply_google_sync(
        connection: &mut Connection,
        account_id: &str,
        changes: Vec<GoogleContactChange>,
        lookups: Vec<GoogleContactLookup>,
        state: GoogleContactSyncState,
        now: i64,
    ) -> (
        String,
        DuplicateRemoteIdRepairReport,
        ReconcileReport,
        GoogleContactSyncState,
    ) {
        let book_id = find_google_book(connection, account_id)
            .unwrap()
            .expect("test Google book");
        let known_remote_ids = load_google_remote_ids(connection, &book_id).unwrap();
        let (delta, next_state) = prepare_google_delta(
            GoogleContactChanges {
                changes,
                next_sync_token: format!("token-{now}"),
            },
            lookups,
            &known_remote_ids,
            state,
            now,
        )
        .unwrap();
        let state_key = google_contact_state_key(account_id);
        let (repair, report, ()) = reconcile_remote_id_delta_with_repair_and_postcheck(
            connection,
            account_id,
            &book_id,
            "google",
            delta,
            |transaction| save_google_contact_state(transaction, &state_key, &next_state),
        )
        .unwrap();
        let persisted = load_google_contact_state(connection, &state_key).unwrap();
        assert_eq!(persisted, next_state);
        (book_id, repair, report, next_state)
    }

    #[tokio::test]
    async fn production_sync_handles_initial_incremental_and_expired_token_cycles() {
        let full_change = serde_json::json!({
            "connections": [{
                "resourceName": "people/one",
                "metadata": {"sources": [{
                    "type": "CONTACT",
                    "id": "source-one",
                    "etag": "source-etag-one",
                }]},
            }],
            "nextSyncToken": "token-one",
        });
        let batch = serde_json::json!({"responses": [{
            "requestedResourceName": "people/one",
            "person": wire_person("one"),
            "status": {},
        }]});
        let tombstone = serde_json::json!({
            "connections": [{
                "resourceName": "people/one",
                "metadata": {"deleted": true},
            }],
            "nextSyncToken": "token-two",
        });
        let reseed = serde_json::json!({
            "connections": null,
            "nextSyncToken": "token-three",
        });
        let recovery_reseed = serde_json::json!({
            "connections": null,
            "nextSyncToken": "token-four",
        });
        let (root, requests) = serve_people_responses(vec![
            (200, full_change.to_string()),
            (200, batch.to_string()),
            (200, tombstone.to_string()),
            (410, "{}".into()),
            (200, reseed.to_string()),
            (200, recovery_reseed.to_string()),
        ])
        .await;
        let (_directory, db) = crate::backend::testutil::temp_pool();
        {
            let conn = db.writer().await;
            crate::db::schema::initialize(&conn).unwrap();
            conn.execute(
                "INSERT INTO accounts (id, display_name, email, username)
                 VALUES ('acc1', 'Test', 'test@example.test', 'test@example.test')",
                [],
            )
            .unwrap();
        }
        let providers = test_provider_services(&root, false);
        let context = ContactBackendCtx {
            db: &db,
            providers: &providers,
        };
        let account = crate::backend::testutil::account("contacts", "google");

        GoogleContactBackend.sync(&context, &account).await.unwrap();

        let state_key = google_contact_state_key("acc1");
        {
            let conn = db.reader();
            let contact: (String, String, String) = conn
                .query_row(
                    "SELECT c.display_name, c.remote_id, c.etag
                     FROM contacts c
                     JOIN contact_books cb ON cb.id = c.book_id
                     WHERE cb.account_id = 'acc1'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(contact.0, "Name one");
            assert_eq!(contact.1, "people/one");
            assert_eq!(contact.2, "source-etag-one");
            assert_eq!(
                load_google_contact_state(&conn, &state_key)
                    .unwrap()
                    .sync_token
                    .as_deref(),
                Some("token-one")
            );
        }

        GoogleContactBackend.sync(&context, &account).await.unwrap();

        {
            let conn = db.reader();
            let contacts: i64 = conn
                .query_row("SELECT COUNT(*) FROM contacts", [], |row| row.get(0))
                .unwrap();
            assert_eq!(contacts, 0);
            assert_eq!(
                load_google_contact_state(&conn, &state_key)
                    .unwrap()
                    .sync_token
                    .as_deref(),
                Some("token-two")
            );
        }
        GoogleContactBackend.sync(&context, &account).await.unwrap();
        {
            let conn = db.reader();
            assert_eq!(
                load_google_contact_state(&conn, &state_key)
                    .unwrap()
                    .sync_token
                    .as_deref(),
                Some("token-three")
            );
            let books: i64 = conn
                .query_row("SELECT COUNT(*) FROM contact_books", [], |row| row.get(0))
                .unwrap();
            assert_eq!(books, 1);
        }
        {
            let conn = db.writer().await;
            let mut state = load_google_contact_state(&conn, &state_key).unwrap();
            state.pending_recoveries = 1;
            save_google_contact_state(&conn, &state_key, &state).unwrap();
        }
        GoogleContactBackend.sync(&context, &account).await.unwrap();
        {
            let conn = db.reader();
            let state = load_google_contact_state(&conn, &state_key).unwrap();
            assert_eq!(state.sync_token.as_deref(), Some("token-four"));
            assert_eq!(state.pending_recoveries, 0);
        }
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 6);
        assert!(requests[0].starts_with("GET /people-api/people/me/connections?"));
        assert!(requests[1].starts_with("GET /people-api/people:batchGet?"));
        let target = requests[2]
            .lines()
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap();
        let query = reqwest::Url::parse(&format!("http://localhost{target}"))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(query["syncToken"], "token-one");
        let target = requests[3]
            .lines()
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap();
        let expired_query = reqwest::Url::parse(&format!("http://localhost{target}"))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(expired_query["syncToken"], "token-two");
        let target = requests[4]
            .lines()
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap();
        let reseed_query = reqwest::Url::parse(&format!("http://localhost{target}"))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert!(!reseed_query.contains_key("syncToken"));
        let target = requests[5]
            .lines()
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap();
        let recovery_query = reqwest::Url::parse(&format!("http://localhost{target}"))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert!(!recovery_query.contains_key("syncToken"));
    }

    #[tokio::test]
    async fn failed_remote_push_keeps_the_durable_recovery_marker() {
        let (root, requests) =
            serve_people_responses(vec![(500, "{}".into()), (200, "{}".into())]).await;
        let (_directory, db) = crate::backend::testutil::temp_pool();
        let state_key = google_contact_state_key("acc1");
        {
            let conn = db.writer().await;
            crate::db::schema::initialize(&conn).unwrap();
            conn.execute(
                "INSERT INTO accounts (id, display_name, email, username)
                 VALUES ('acc1', 'Test', 'test@example.test', 'test@example.test')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO contact_books (id, account_id, name, sync_type)
                 VALUES ('google-book', 'acc1', 'Google Contacts', 'google')",
                [],
            )
            .unwrap();
            save_google_contact_state(
                &conn,
                &state_key,
                &GoogleContactSyncState {
                    sync_token: Some("cursor".into()),
                    pending_absences: BTreeMap::new(),
                    pending_recoveries: 0,
                },
            )
            .unwrap();
        }
        let providers = test_provider_services(&root, false);
        let context = ContactBackendCtx {
            db: &db,
            providers: &providers,
        };
        let account = crate::backend::testutil::account("contacts", "google");
        {
            let conn = db.writer().await;
            let transaction = conn.unchecked_transaction().unwrap();
            GoogleContactBackend
                .prepare_local_mutation(&transaction, &account)
                .unwrap();
        }
        assert_eq!(
            load_google_contact_state(&db.reader(), &state_key)
                .unwrap()
                .pending_recoveries,
            0
        );
        {
            let conn = db.writer().await;
            let transaction = conn.unchecked_transaction().unwrap();
            GoogleContactBackend
                .prepare_local_mutation(&transaction, &account)
                .unwrap();
            transaction.commit().unwrap();
        }

        assert!(GoogleContactBackend
            .push_deleted_contact(&context, &account, "people/one")
            .await
            .is_err());
        let failing_providers = test_provider_services(&root, true);
        let failing_context = ContactBackendCtx {
            db: &db,
            providers: &failing_providers,
        };
        assert!(GoogleContactBackend
            .push_deleted_contact(&failing_context, &account, "people/one")
            .await
            .is_err());
        let mut malformed = crate::backend::testutil::contact();
        malformed.book_id = "google-book".into();
        malformed.emails_json = "not JSON".into();
        assert!(GoogleContactBackend
            .push_created_contact(&context, &account, &BookRef { remote_id: None }, &malformed,)
            .await
            .is_err());

        let conn = db.reader();
        let state = load_google_contact_state(&conn, &state_key).unwrap();
        assert_eq!(state.sync_token.as_deref(), Some("cursor"));
        assert_eq!(state.pending_recoveries, 1);
        drop(conn);
        {
            let conn = db.writer().await;
            let transaction = conn.unchecked_transaction().unwrap();
            GoogleContactBackend
                .prepare_local_mutation(&transaction, &account)
                .unwrap();
            transaction.commit().unwrap();
        }
        GoogleContactBackend
            .push_deleted_contact(&context, &account, "people/one")
            .await
            .unwrap();
        {
            let conn = db.writer().await;
            let transaction = conn.unchecked_transaction().unwrap();
            GoogleContactBackend
                .complete_local_mutation(&transaction, &account)
                .unwrap();
            transaction.commit().unwrap();
        }
        assert_eq!(
            load_google_contact_state(&db.reader(), &state_key)
                .unwrap()
                .pending_recoveries,
            1
        );
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with("DELETE /people-api/people/one:deleteContact"));
    }

    #[test]
    fn delta_reconciliation_preserves_local_fields_and_applies_explicit_tombstones() {
        let mut connection = setup_db();
        add_book(
            &connection,
            "google-book",
            "account-a",
            "google",
            "2020-01-01 00:00:00",
        );
        add_contact(
            &connection,
            "keep",
            "google-book",
            Some("local-uid"),
            Some("people/keep"),
            "Before",
        );
        add_contact(
            &connection,
            "remove",
            "google-book",
            None,
            Some("people/remove"),
            "Remove",
        );
        add_contact(
            &connection,
            "local",
            "google-book",
            Some("draft-uid"),
            None,
            "Local",
        );
        let mut kept = remote("people/keep", "Kept");
        kept.organization = None;
        kept.title = None;

        let (book_id, repair, report, state) = apply_google_sync(
            &mut connection,
            "account-a",
            vec![
                live_change("people/keep", &[]),
                live_change("people/new", &[]),
                deleted_change("people/remove", &[]),
            ],
            vec![
                found("people/keep", kept.clone()),
                found("people/new", remote("people/new", "New")),
            ],
            GoogleContactSyncState::default(),
            1_000,
        );

        assert_eq!(book_id, "google-book");
        assert_eq!(repair, DuplicateRemoteIdRepairReport::default());
        assert_eq!(report.inserted, 1);
        assert_eq!(report.updated, 1);
        assert_eq!(report.deleted, 1);
        assert!(contact_values(&connection, "remove").is_none());
        assert!(contact_values(&connection, "local").is_some());
        let stored = contact_values(&connection, "keep").unwrap();
        assert_eq!(stored.uid.as_deref(), Some("local-uid"));
        assert_eq!(stored.display_name, "Kept");
        assert_eq!(stored.emails_json, kept.emails_json);
        assert_eq!(stored.phones_json, kept.phones_json);
        assert_eq!(stored.addresses_json, r#"[{"address":"Local"}]"#);
        assert_eq!(stored.organization, None);
        assert_eq!(stored.title, None);
        assert_eq!(stored.notes.as_deref(), Some("Local notes"));
        assert_eq!(stored.vcard_data.as_deref(), Some("LOCAL VCARD"));
        assert_eq!(stored.remote_id.as_deref(), Some("people/keep"));
        assert_eq!(stored.etag.as_deref(), Some("etag-people/keep"));

        let (_, _, repeated, _) = apply_google_sync(
            &mut connection,
            "account-a",
            vec![
                live_change("people/keep", &[]),
                live_change("people/new", &[]),
            ],
            vec![
                found("people/keep", kept),
                found("people/new", remote("people/new", "New")),
            ],
            state,
            1_001,
        );
        assert_eq!(repeated.unchanged_or_stale, 2);
        assert_eq!(repeated.inserted + repeated.updated + repeated.deleted, 0);
    }

    #[test]
    fn previous_resource_name_migrates_the_existing_row_without_losing_local_fields() {
        let mut connection = setup_db();
        add_book(
            &connection,
            "google-book",
            "account-a",
            "google",
            "2020-01-01 00:00:00",
        );
        add_contact(
            &connection,
            "existing",
            "google-book",
            Some("local-uid"),
            Some("people/old"),
            "Before",
        );
        let mut state = GoogleContactSyncState::default();
        state.pending_absences.insert("people/old".into(), 500);

        let (_, _, report, state) = apply_google_sync(
            &mut connection,
            "account-a",
            vec![live_change("people/current", &["people/old"])],
            vec![found("people/current", remote("people/current", "Current"))],
            state,
            1_000,
        );

        assert_eq!(report.updated, 1);
        assert_eq!(report.inserted + report.deleted, 0);
        assert!(state.pending_absences.is_empty());
        let stored = contact_values(&connection, "existing").unwrap();
        assert_eq!(stored.uid.as_deref(), Some("local-uid"));
        assert_eq!(stored.display_name, "Current");
        assert_eq!(stored.addresses_json, r#"[{"address":"Local"}]"#);
        assert_eq!(stored.notes.as_deref(), Some("Local notes"));
        assert_eq!(stored.vcard_data.as_deref(), Some("LOCAL VCARD"));
        assert_eq!(stored.remote_id.as_deref(), Some("people/current"));
    }

    #[test]
    fn batch_requested_name_can_migrate_to_a_different_returned_resource_name() {
        let mut connection = setup_db();
        add_book(
            &connection,
            "google-book",
            "account-a",
            "google",
            "2020-01-01 00:00:00",
        );
        add_contact(
            &connection,
            "existing",
            "google-book",
            None,
            Some("people/old"),
            "Before",
        );

        let (_, _, report, _) = apply_google_sync(
            &mut connection,
            "account-a",
            vec![live_change("people/current", &[])],
            vec![
                found("people/current", remote("people/current", "Current")),
                found("people/old", remote("people/current", "Current")),
            ],
            GoogleContactSyncState::default(),
            1_000,
        );

        assert_eq!(report.updated, 1);
        assert_eq!(report.inserted + report.deleted, 0);
        assert_eq!(
            contact_values(&connection, "existing")
                .unwrap()
                .remote_id
                .as_deref(),
            Some("people/current")
        );
    }

    #[test]
    fn full_sync_absence_requires_reobservation_after_the_propagation_delay() {
        let mut connection = setup_db();
        add_book(
            &connection,
            "google-book",
            "account-a",
            "google",
            "2020-01-01 00:00:00",
        );
        for (id, remote_id) in [
            ("managed", Some("people/managed")),
            ("null", None),
            ("empty", Some("")),
            ("blank", Some("\u{2003}")),
        ] {
            add_contact(&connection, id, "google-book", None, remote_id, id);
        }

        let (_, _, first, state) = apply_google_sync(
            &mut connection,
            "account-a",
            Vec::new(),
            vec![missing("people/managed")],
            GoogleContactSyncState::default(),
            1_000,
        );
        assert_eq!(first, ReconcileReport::default());
        assert!(contact_values(&connection, "managed").is_some());
        assert_eq!(state.pending_absences["people/managed"], 1_000);

        let (_, _, second, state) = apply_google_sync(
            &mut connection,
            "account-a",
            Vec::new(),
            vec![missing("people/managed")],
            state,
            1_599,
        );
        assert_eq!(second, ReconcileReport::default());
        assert!(contact_values(&connection, "managed").is_some());

        let (_, _, third, state) = apply_google_sync(
            &mut connection,
            "account-a",
            Vec::new(),
            vec![missing("people/managed")],
            state,
            1_600,
        );
        assert_eq!(third.deleted, 1);
        assert!(contact_values(&connection, "managed").is_none());
        assert!(state.pending_absences.is_empty());
        for id in ["null", "empty", "blank"] {
            assert!(contact_values(&connection, id).is_some());
        }
    }

    #[test]
    fn token_write_failure_rolls_back_contact_delta() {
        let mut connection = setup_db();
        add_book(
            &connection,
            "google-book",
            "account-a",
            "google",
            "2020-01-01 00:00:00",
        );
        add_contact(
            &connection,
            "managed",
            "google-book",
            None,
            Some("people/managed"),
            "Before",
        );
        let known_remote_ids = load_google_remote_ids(&connection, "google-book").unwrap();
        let (delta, state) = prepare_google_delta(
            GoogleContactChanges {
                changes: vec![live_change("people/managed", &[])],
                next_sync_token: "next-token".into(),
            },
            vec![found("people/managed", remote("people/managed", "After"))],
            &known_remote_ids,
            GoogleContactSyncState::default(),
            1_000,
        )
        .unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_google_contact_state
                 BEFORE INSERT ON app_metadata
                 BEGIN
                     SELECT RAISE(ABORT, 'injected state failure');
                 END;",
            )
            .unwrap();

        assert!(reconcile_remote_id_delta_with_repair_and_postcheck(
            &mut connection,
            "account-a",
            "google-book",
            "google",
            delta,
            |transaction| {
                save_google_contact_state(
                    transaction,
                    &google_contact_state_key("account-a"),
                    &state,
                )
            },
        )
        .is_err());

        assert_eq!(
            contact_values(&connection, "managed").unwrap().display_name,
            "Before"
        );
        assert!(
            load_google_contact_state(&connection, &google_contact_state_key("account-a"))
                .unwrap()
                .sync_token
                .is_none()
        );
    }

    #[test]
    fn lookup_planning_is_full_or_pending_only_and_excludes_tombstones() {
        let changes = vec![
            live_change("people/live", &[]),
            deleted_change("people/deleted", &["people/old-deleted"]),
        ];
        let known = vec![
            "people/known".into(),
            "people/deleted".into(),
            "people/pending".into(),
        ];
        let pending = BTreeMap::from([
            ("people/pending".into(), 1),
            ("people/stale-state".into(), 1),
        ]);

        assert_eq!(
            google_lookup_resource_names(&changes, &known, &pending, true).unwrap(),
            ["people/known", "people/live", "people/pending"]
        );
        assert_eq!(
            google_lookup_resource_names(&changes, &known, &pending, false).unwrap(),
            ["people/live", "people/pending"]
        );
    }

    #[test]
    fn malformed_persisted_state_fails_closed() {
        let connection = setup_db();
        let key = google_contact_state_key("account-a");
        for value in [
            "not-json",
            r#"{"sync_token":" "}"#,
            r#"{"pending_absences":{"other/id":1}}"#,
            r#"{"pending_absences":{"people/id":-1}}"#,
        ] {
            connection
                .execute(
                    "INSERT OR REPLACE INTO app_metadata (key, value) VALUES (?1, ?2)",
                    rusqlite::params![key, value],
                )
                .unwrap();
            assert!(load_google_contact_state(&connection, &key).is_err());
        }
    }

    #[test]
    fn duplicate_live_changes_fail_before_database_mutation() {
        let connection = setup_db();
        let error = prepare_google_delta(
            GoogleContactChanges {
                changes: vec![
                    live_change("people/same", &[]),
                    live_change("people/same", &[]),
                ],
                next_sync_token: "token".into(),
            },
            vec![found("people/same", remote("people/same", "Same"))],
            &[],
            GoogleContactSyncState::default(),
            1_000,
        )
        .unwrap_err();

        assert!(error.to_string().contains("duplicate or crossed identity"));
        let error = prepare_google_delta(
            GoogleContactChanges {
                changes: vec![
                    deleted_change("people/one", &["people/shared"]),
                    deleted_change("people/two", &["people/shared"]),
                ],
                next_sync_token: "token".into(),
            },
            Vec::new(),
            &[],
            GoogleContactSyncState::default(),
            1_000,
        )
        .unwrap_err();
        assert!(error.to_string().contains("duplicate or crossed identity"));
        let books: i64 = connection
            .query_row("SELECT COUNT(*) FROM contact_books", [], |row| row.get(0))
            .unwrap();
        assert_eq!(books, 0);
    }

    #[test]
    fn successful_first_sync_creates_the_google_book_contact_and_state() {
        let mut connection = setup_db();
        let (delta, state) = prepare_google_delta(
            GoogleContactChanges {
                changes: vec![live_change("people/new", &[])],
                next_sync_token: "token-1000".into(),
            },
            vec![found("people/new", remote("people/new", "New"))],
            &[],
            GoogleContactSyncState::default(),
            1_000,
        )
        .unwrap();
        let book_id = "first-google-book";
        let state_key = google_contact_state_key("account-a");
        let (report, ()) = reconcile_remote_id_delta_into_new_book_with_postcheck(
            &mut connection,
            "account-a",
            book_id,
            "Google Contacts",
            "google",
            delta,
            |transaction| save_google_contact_state(transaction, &state_key, &state),
        )
        .unwrap();

        assert_eq!(report.inserted, 1);
        assert_eq!(state.sync_token.as_deref(), Some("token-1000"));
        assert_eq!(
            load_google_contact_state(&connection, &state_key).unwrap(),
            state
        );
        assert!(contact_values(&connection, "people/new").is_none());
        let stored_book: String = connection
            .query_row(
                "SELECT book_id FROM contacts WHERE remote_id = 'people/new'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_book, book_id);
    }

    #[test]
    fn duplicate_managed_ids_are_repaired_without_losing_rows() {
        let mut connection = setup_db();
        add_book(
            &connection,
            "google-book",
            "account-a",
            "google",
            "2020-01-01 00:00:00",
        );
        add_contact(
            &connection,
            "winner",
            "google-book",
            Some("local-uid"),
            Some("people/same"),
            "Winner",
        );
        add_contact(
            &connection,
            "detached",
            "google-book",
            None,
            Some("people/same"),
            "Detached",
        );

        let (_, repair, report, _) = apply_google_sync(
            &mut connection,
            "account-a",
            vec![live_change("people/same", &[])],
            vec![found("people/same", remote("people/same", "Remote"))],
            GoogleContactSyncState::default(),
            1_000,
        );

        assert_eq!(repair.detached, 1);
        assert_eq!(report.updated, 1);
        assert_eq!(
            contact_values(&connection, "winner").unwrap().display_name,
            "Remote"
        );
        assert_eq!(
            contact_values(&connection, "detached").unwrap().remote_id,
            None
        );
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM contacts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn multiple_google_books_fail_closed_without_cross_scope_mutation() {
        let connection = setup_db();
        add_book(
            &connection,
            "newer-google",
            "account-a",
            "google",
            "2022-01-01 00:00:00",
        );
        add_book(
            &connection,
            "oldest-google",
            "account-a",
            "google",
            "2020-01-01 00:00:00",
        );
        add_book(
            &connection,
            "other-account",
            "account-b",
            "google",
            "2010-01-01 00:00:00",
        );
        add_book(
            &connection,
            "other-sync",
            "account-a",
            "carddav",
            "2010-01-01 00:00:00",
        );

        let error = find_google_book(&connection, "account-a").unwrap_err();
        assert!(error.to_string().contains("multiple Google contact books"));
        let mut account = crate::backend::testutil::account("contacts", "google");
        account.id = "account-a".into();
        assert!(GoogleContactBackend
            .prepare_local_mutation(&connection, &account)
            .is_err());
        assert_eq!(
            find_google_book(&connection, "account-b")
                .unwrap()
                .as_deref(),
            Some("other-account")
        );
        let contacts: i64 = connection
            .query_row("SELECT COUNT(*) FROM contacts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(contacts, 0);
    }
}
