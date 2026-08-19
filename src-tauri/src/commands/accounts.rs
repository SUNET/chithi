use serde::{Deserialize, Serialize};
use tauri::State;

use crate::commands::calendar::random_calendar_color;
use crate::db;
use crate::db::calendar::NewCalendar;
use crate::error::Result;
use crate::oauth::OAuthTokens;
use crate::provider::ZoomTokenLifecycleGuard;
use crate::state::AppState;

pub(crate) fn insert_zoom_account(
    conn: &rusqlite::Connection,
    account_id: &str,
    config: &db::accounts::AccountConfig,
    tokens: &OAuthTokens,
    token_guard: &ZoomTokenLifecycleGuard<'_>,
) -> Result<()> {
    let transaction = conn.unchecked_transaction()?;
    db::accounts::insert_account(&transaction, account_id, config)?;
    token_guard.store_and_commit(tokens, move || {
        transaction.commit()?;
        Ok(())
    })
}

fn delete_account_data(
    conn: &rusqlite::Connection,
    account_id: &str,
    token_guard: &ZoomTokenLifecycleGuard<'_>,
) -> Result<Vec<String>> {
    let transaction = conn.unchecked_transaction()?;
    let bindings = db::service_bindings::list_for_account(&transaction, account_id)?;
    let has_zoom = bindings
        .iter()
        .any(|binding| binding.service == "meet" && binding.protocol == "zoom");
    let is_standalone_zoom = bindings.len() == 1 && has_zoom;
    if has_zoom && !is_standalone_zoom {
        return Err(crate::error::Error::Other(
            "Cannot delete Zoom credentials: the account has additional service bindings".into(),
        ));
    }
    // Account deletion cascades through calendars and events. Move any bound
    // meetings into durable cleanup ownership first. If this same account
    // owns one of those meetings, `delete_account` rejects below and the
    // transaction rolls the event transfer back together with the deletion.
    let cleanup_ids = db::calendar_event_deletion::delete_account_events(&transaction, account_id)?
        .cleanup_lifecycle_ids;
    db::accounts::delete_account(&transaction, account_id)?;

    if is_standalone_zoom {
        token_guard.delete_and_commit(move || {
            transaction.commit()?;
            Ok(())
        })?;
    } else {
        transaction.commit()?;
    }
    Ok(cleanup_ids)
}

/// Result of a Thunderbird-style mail-server discovery on the IMAP
/// tab. Empty strings / zero ports mean "not found" — the UI keeps
/// whatever was already in the form. `source` is informational
/// ("isp-db", "domain-autoconfig", "well-known", "mx", or empty when
/// no source matched).
///
/// Note that this struct used to carry CalDAV / CardDAV URLs too,
/// but mixing those into an "IMAP account" turned out to be a
/// footgun: the discovered URL silently turned a mail-only IMAP
/// account into a calendar+contacts account, which then collided
/// with the dedicated CalDAV / CardDAV account types and produced
/// duplicate calendars on sync. IMAP / CalDAV / CardDAV are now
/// strictly separate accounts; if you want all three on one
/// identity, use the JMAP / Gmail / O365 tabs which natively
/// bundle them.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AutoconfigResult {
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_use_tls: bool,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_use_tls: bool,
    pub source: String,
}

/// Run Thunderbird-style email autoconfig (Mozilla ISP DB /
/// provider autoconfig / `.well-known` / MX fallback) and return
/// the discovered IMAP / SMTP host+port+TLS settings. Mail-only;
/// no CalDAV / CardDAV probing here — see the doc comment on
/// `AutoconfigResult` for why.
///
/// `imap_host_hint` / `smtp_host_hint` are the host values already
/// typed in the form. **Hints win over autoconfig** for any service
/// they cover — the user knows their actual IMAP / submission host;
/// autoconfig's MX fallback only knows the inbound-mail routing and
/// can name a totally different relay. So we probe the hints first,
/// and only run autoconfig for sides where no hint was supplied. If
/// a hint is set but its standard ports are unreachable, we leave
/// that side blank rather than substitute a different host the user
/// didn't ask for.
#[tauri::command]
pub async fn discover_mail_servers(
    email: String,
    imap_host_hint: Option<String>,
    smtp_host_hint: Option<String>,
) -> Result<AutoconfigResult> {
    log::info!(
        "discover_mail_servers: email={} imap_hint={:?} smtp_hint={:?}",
        email,
        imap_host_hint,
        smtp_host_hint
    );

    let imap_hint = imap_host_hint.as_deref().filter(|h| !h.is_empty());
    let smtp_hint = smtp_host_hint.as_deref().filter(|h| !h.is_empty());

    let mut result = crate::mail::autoconfig::AutoconfigServers::default();
    let mut source = String::new();

    // Step 1: probe user-typed hosts first.
    if let Some(host) = imap_hint {
        if let Some((port, use_tls)) = crate::mail::autoconfig::probe_imap_port(host).await {
            result.imap_host = host.to_string();
            result.imap_port = port;
            result.imap_use_tls = use_tls;
            source = "host-probe".into();
        }
    }
    if let Some(host) = smtp_hint {
        if let Some((port, use_tls)) = crate::mail::autoconfig::probe_smtp_port(host).await {
            result.smtp_host = host.to_string();
            result.smtp_port = port;
            result.smtp_use_tls = use_tls;
            if source.is_empty() {
                source = "host-probe".into();
            }
        }
    }

    // Step 2: run autoconfig only for sides with no hint at all.
    // Pre-filled or unreachable-hint sides are left as-is.
    let need_autoconfig_imap = imap_hint.is_none() && result.imap_host.is_empty();
    let need_autoconfig_smtp = smtp_hint.is_none() && result.smtp_host.is_empty();
    if need_autoconfig_imap || need_autoconfig_smtp {
        match crate::mail::autoconfig::discover(&email).await {
            Ok(Some((s, src))) => {
                if need_autoconfig_imap && !s.imap_host.is_empty() {
                    result.imap_host = s.imap_host;
                    result.imap_port = s.imap_port;
                    result.imap_use_tls = s.imap_use_tls;
                    if source.is_empty() {
                        source = src.to_string();
                    }
                }
                if need_autoconfig_smtp && !s.smtp_host.is_empty() {
                    result.smtp_host = s.smtp_host;
                    result.smtp_port = s.smtp_port;
                    result.smtp_use_tls = s.smtp_use_tls;
                    if source.is_empty() {
                        source = src.to_string();
                    }
                }
            }
            Ok(None) => {}
            Err(e) => log::debug!("autoconfig: discover errored: {}", e),
        }
    }

    Ok(AutoconfigResult {
        imap_host: result.imap_host,
        imap_port: result.imap_port,
        imap_use_tls: result.imap_use_tls,
        smtp_host: result.smtp_host,
        smtp_port: result.smtp_port,
        smtp_use_tls: result.smtp_use_tls,
        source,
    })
}

#[tauri::command]
pub async fn list_accounts(state: State<'_, AppState>) -> Result<Vec<db::accounts::Account>> {
    log::debug!("Listing accounts");
    let conn = state.db.reader();
    let accounts = db::accounts::list_accounts(&conn)?;
    log::debug!("Found {} accounts", accounts.len());
    Ok(accounts)
}

#[tauri::command]
pub async fn add_account(
    state: State<'_, AppState>,
    config: db::accounts::AccountConfig,
) -> Result<String> {
    log::info!(
        "Adding account: {} ({}) provider={} imap={}:{}",
        config.display_name,
        config.email,
        config.provider,
        config.imap_host,
        config.imap_port,
    );
    let id = uuid::Uuid::new_v4().to_string();

    // Migrate OAuth tokens from temporary ID to real account ID.
    // During OAuth flow, tokens are stored under a temp ID like "o365-pending-123"
    // or "gmail-pending-123", referenced via password field "oauth2:{temp_id}".
    if let Some(temp_id) = config.password.strip_prefix("oauth2:") {
        if let Ok(Some(tokens)) = state.providers.token_store().load(temp_id) {
            state.providers.token_store().store(&id, &tokens)?;
            state.providers.token_store().delete(temp_id).ok();
            log::info!("Migrated OAuth tokens from {} to {}", temp_id, id);
        }
    }

    let conn = state.db.writer().await;
    db::accounts::insert_account(&conn, &id, &config)?;
    log::info!("Account created with id={}", id);

    // Create a default local calendar only if the account has an enabled
    // calendar binding to attach it to. Plain IMAP accounts where DAV
    // discovery turned up nothing get no calendar binding and therefore
    // no calendar row — the calendar view simply won't list them.
    let bindings = crate::db::service_bindings::list_for_account(&conn, &id)?;
    let has_calendar_binding = bindings
        .iter()
        .any(|b| b.service == "calendar" && b.enabled);
    if has_calendar_binding {
        let cal_id = uuid::Uuid::new_v4().to_string();
        let default_calendar = NewCalendar {
            account_id: id.clone(),
            name: "Calendar".to_string(),
            color: random_calendar_color(),
            is_default: true,
        };
        db::calendar::insert_calendar(&conn, &cal_id, &default_calendar)?;
        log::info!(
            "Default calendar created with id={} for account={}",
            cal_id,
            id
        );
    } else {
        log::info!(
            "No calendar binding for account {}; skipping default calendar",
            id
        );
    }

    Ok(id)
}

#[tauri::command]
pub async fn get_account_config(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<db::accounts::AccountConfig> {
    log::debug!("Getting config for account {}", account_id);
    let conn = state.db.reader();
    let full = db::accounts::get_account_full(&conn, &account_id)?;
    // Compute binding-presence flags before we partial-move `full`'s
    // String fields into the AccountConfig literal below.
    // Enabled-aware so the edit form reopens on the right tab for
    // CalDAV-only / CardDAV-only accounts — those carry both
    // bindings (one disabled) under the legacy single-`caldav_url`
    // schema, and existence-only flags would always pick the same
    // branch.
    let has_calendar_binding = full.calendar_binding().is_some_and(|b| b.enabled);
    let has_contacts_binding = full.contacts_binding().is_some_and(|b| b.enabled);
    // Never return the actual password to the frontend.
    // The edit form shows a placeholder; empty on save means "keep existing".
    Ok(db::accounts::AccountConfig {
        display_name: full.display_name,
        email: full.email,
        provider: full.provider,
        mail_protocol: full.mail_protocol,
        imap_host: full.imap_host,
        imap_port: full.imap_port,
        smtp_host: full.smtp_host,
        smtp_port: full.smtp_port,
        jmap_url: full.jmap_url,
        caldav_url: full.caldav_url,
        meet_url: full.meet_url,
        meet_protocol: full.meet_protocol,
        username: full.username,
        password: String::new(),
        use_tls: full.use_tls,
        signature: full.signature,
        jmap_auth_method: full.jmap_auth_method,
        oidc_token_endpoint: full.oidc_token_endpoint,
        oidc_client_id: full.oidc_client_id,
        calendar_sync_enabled: full.calendar_sync_enabled,
        mail_sync_enabled: full.mail_sync_enabled,
        contacts_sync_enabled: full.contacts_sync_enabled,
        mail_sync_interval_seconds: full.mail_sync_interval_seconds,
        calendar_sync_interval_seconds: full.calendar_sync_interval_seconds,
        contacts_sync_interval_seconds: full.contacts_sync_interval_seconds,
        has_calendar_binding,
        has_contacts_binding,
        pgp_attach_pubkey_on_sign: full.pgp_attach_pubkey_on_sign,
        pgp_autocrypt_header: full.pgp_autocrypt_header,
        pgp_encrypt_subject: full.pgp_encrypt_subject,
        pgp_encrypt_drafts: full.pgp_encrypt_drafts,
    })
}

#[tauri::command]
pub async fn update_account(
    state: State<'_, AppState>,
    account_id: String,
    config: db::accounts::AccountConfig,
) -> Result<()> {
    log::info!("Updating account {} ({})", account_id, config.email);
    state
        .with_op_worker_stopped(&account_id, || async {
            let account_lock = state.account_lifecycle.acquire(&account_id);
            let _account_guard = account_lock.lock().await;
            let conn = state.db.writer().await;
            db::accounts::update_account(&conn, &account_id, &config)
        })
        .await
}

#[tauri::command]
pub async fn delete_account(state: State<'_, AppState>, account_id: String) -> Result<()> {
    log::info!("Deleting account {}", account_id);
    let cleanup_ids = state
        .with_op_worker_stopped(&account_id, || async {
            // Global order is optional meeting lifecycle, account lifecycle,
            // then provider credentials. Account deletion starts at account.
            let account_lock = state.account_lifecycle.acquire(&account_id);
            let _account_guard = account_lock.lock().await;
            let token_guard = state.providers.lock_zoom_tokens(&account_id).await;
            let cleanup_ids = {
                let conn = state.db.writer().await;
                delete_account_data(&conn, &account_id, &token_guard)?
            };
            // Also remove Maildir
            let maildir_path = state.data_dir.join(&account_id);
            if maildir_path.exists() {
                log::info!("Removing maildir at {}", maildir_path.display());
                std::fs::remove_dir_all(&maildir_path).ok();
            }
            Result::<Vec<String>>::Ok(cleanup_ids)
        })
        .await?;
    crate::commands::meet::sweep_pending(&state, cleanup_ids).await;
    log::info!("Account {} deleted", account_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use rusqlite::Connection;

    use super::*;
    use crate::error::Error;
    use crate::oauth::OAuthProvider;
    use crate::provider::{
        OAuthTokenStore, ProviderCredentialService, ProviderServices, ProviderTransports,
        TokenEndpointClient,
    };

    #[derive(Default)]
    struct FakeTokenStore {
        tokens: Mutex<HashMap<String, OAuthTokens>>,
        fail_store_after_write: AtomicBool,
        fail_delete: AtomicBool,
        fail_delete_after_remove: AtomicBool,
        deletes: AtomicUsize,
    }

    impl OAuthTokenStore for FakeTokenStore {
        fn load(&self, account_id: &str) -> Result<Option<OAuthTokens>> {
            Ok(self.tokens.lock().unwrap().get(account_id).cloned())
        }

        fn store(&self, account_id: &str, tokens: &OAuthTokens) -> Result<()> {
            self.tokens
                .lock()
                .unwrap()
                .insert(account_id.to_string(), tokens.clone());
            if self.fail_store_after_write.load(Ordering::SeqCst) {
                return Err(Error::Other("injected token store failure".into()));
            }
            Ok(())
        }

        fn delete(&self, account_id: &str) -> Result<()> {
            self.deletes.fetch_add(1, Ordering::SeqCst);
            if self.fail_delete.load(Ordering::SeqCst) {
                return Err(Error::Other("injected token delete failure".into()));
            }
            self.tokens.lock().unwrap().remove(account_id);
            if self.fail_delete_after_remove.load(Ordering::SeqCst) {
                return Err(Error::Other("injected token failure after deletion".into()));
            }
            Ok(())
        }
    }

    struct UnusedEndpoint;

    #[async_trait]
    impl TokenEndpointClient for UnusedEndpoint {
        async fn exchange_code(
            &self,
            _provider: &OAuthProvider,
            _code: &str,
            _port: u16,
            _code_verifier: Option<&str>,
        ) -> Result<OAuthTokens> {
            unreachable!()
        }

        async fn refresh(
            &self,
            _provider: &OAuthProvider,
            _refresh_token: &str,
        ) -> Result<OAuthTokens> {
            unreachable!()
        }

        async fn refresh_scoped(
            &self,
            _provider: &OAuthProvider,
            _refresh_token: &str,
            _scopes: &str,
        ) -> Result<OAuthTokens> {
            unreachable!()
        }

        async fn refresh_dynamic(
            &self,
            _token_url: &str,
            _refresh_token: &str,
            _client_id: &str,
        ) -> Result<OAuthTokens> {
            unreachable!()
        }
    }

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            PRAGMA foreign_keys=ON;
            CREATE TABLE accounts (
                id TEXT PRIMARY KEY,
                display_name TEXT NOT NULL,
                email TEXT NOT NULL,
                username TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                signature TEXT NOT NULL DEFAULT '',
                auth_method TEXT NOT NULL DEFAULT '',
                oidc_token_endpoint TEXT NOT NULL DEFAULT '',
                oidc_client_id TEXT NOT NULL DEFAULT '',
                pgp_attach_pubkey_on_sign INTEGER NOT NULL DEFAULT 1,
                pgp_autocrypt_header INTEGER NOT NULL DEFAULT 1,
                pgp_encrypt_subject INTEGER NOT NULL DEFAULT 1,
                pgp_encrypt_drafts INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE service_bindings (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                service TEXT NOT NULL,
                protocol TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                sync_interval_seconds INTEGER,
                config_json TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(account_id, service, protocol)
            );
            CREATE TABLE meet_meetings (
                event_id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                protocol TEXT NOT NULL,
                meeting_id TEXT NOT NULL,
                join_url TEXT NOT NULL
            );
            CREATE TABLE meet_pending_meetings (
                lifecycle_id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                protocol TEXT NOT NULL,
                meeting_id TEXT NOT NULL,
                join_url TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                cleanup_requested INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE calendars (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                name TEXT NOT NULL
            );
            CREATE TABLE calendar_events (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                calendar_id TEXT NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
                title TEXT NOT NULL,
                start_time TEXT NOT NULL,
                end_time TEXT NOT NULL
            );
            ",
        )
        .unwrap();
        conn
    }

    fn providers(store: Arc<FakeTokenStore>) -> ProviderServices {
        let endpoint: Arc<dyn TokenEndpointClient> = Arc::new(UnusedEndpoint);
        let credentials = Arc::new(ProviderCredentialService::new(
            store.clone(),
            endpoint.clone(),
        ));
        ProviderServices::new(
            credentials,
            store,
            endpoint,
            ProviderTransports::production().unwrap(),
        )
    }

    fn tokens() -> OAuthTokens {
        OAuthTokens {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at: Some(i64::MAX),
        }
    }

    fn account_config(protocol: &str) -> db::accounts::AccountConfig {
        db::accounts::AccountConfig {
            display_name: protocol.to_string(),
            email: String::new(),
            provider: "generic".into(),
            mail_protocol: String::new(),
            imap_host: String::new(),
            imap_port: 0,
            smtp_host: String::new(),
            smtp_port: 0,
            jmap_url: String::new(),
            caldav_url: String::new(),
            meet_url: if protocol.is_empty() {
                String::new()
            } else {
                "https://example.com".into()
            },
            meet_protocol: protocol.into(),
            username: String::new(),
            password: String::new(),
            use_tls: true,
            signature: String::new(),
            jmap_auth_method: "basic".into(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
            calendar_sync_enabled: false,
            mail_sync_enabled: false,
            contacts_sync_enabled: false,
            mail_sync_interval_seconds: None,
            calendar_sync_interval_seconds: None,
            contacts_sync_interval_seconds: None,
            has_calendar_binding: false,
            has_contacts_binding: false,
            pgp_attach_pubkey_on_sign: true,
            pgp_autocrypt_header: true,
            pgp_encrypt_subject: true,
            pgp_encrypt_drafts: true,
        }
    }

    fn account_exists(conn: &Connection, account_id: &str) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?1)",
            [account_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn zoom_creation_and_deletion_persist_as_one_lifecycle() {
        let conn = setup_db();
        let store = Arc::new(FakeTokenStore::default());
        let providers = providers(store.clone());
        let token_guard = providers.lock_zoom_tokens("zoom").await;

        insert_zoom_account(
            &conn,
            "zoom",
            &account_config("zoom"),
            &tokens(),
            &token_guard,
        )
        .unwrap();
        assert!(account_exists(&conn, "zoom"));
        assert!(store.load("zoom").unwrap().is_some());

        delete_account_data(&conn, "zoom", &token_guard).unwrap();
        assert!(!account_exists(&conn, "zoom"));
        assert!(store.load("zoom").unwrap().is_none());
    }

    #[tokio::test]
    async fn partial_token_store_failure_rolls_back_zoom_account_and_token() {
        let conn = setup_db();
        let store = Arc::new(FakeTokenStore::default());
        store.fail_store_after_write.store(true, Ordering::SeqCst);
        let providers = providers(store.clone());
        let token_guard = providers.lock_zoom_tokens("zoom").await;

        let result = insert_zoom_account(
            &conn,
            "zoom",
            &account_config("zoom"),
            &tokens(),
            &token_guard,
        );

        assert!(result.is_err());
        assert!(!account_exists(&conn, "zoom"));
        assert!(store.load("zoom").unwrap().is_none());
    }

    #[tokio::test]
    async fn zoom_token_delete_failure_retains_account_and_binding() {
        let conn = setup_db();
        let store = Arc::new(FakeTokenStore::default());
        let providers = providers(store.clone());
        let token_guard = providers.lock_zoom_tokens("zoom").await;
        insert_zoom_account(
            &conn,
            "zoom",
            &account_config("zoom"),
            &tokens(),
            &token_guard,
        )
        .unwrap();
        store.fail_delete.store(true, Ordering::SeqCst);

        assert!(delete_account_data(&conn, "zoom", &token_guard).is_err());
        assert!(account_exists(&conn, "zoom"));
        assert_eq!(
            db::service_bindings::list_for_account(&conn, "zoom")
                .unwrap()
                .len(),
            1
        );
        assert!(store.load("zoom").unwrap().is_some());
    }

    #[tokio::test]
    async fn bound_meeting_blocks_zoom_account_and_token_deletion() {
        let conn = setup_db();
        let store = Arc::new(FakeTokenStore::default());
        let providers = providers(store.clone());
        let token_guard = providers.lock_zoom_tokens("zoom").await;
        insert_zoom_account(
            &conn,
            "zoom",
            &account_config("zoom"),
            &tokens(),
            &token_guard,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meet_meetings
                (event_id, account_id, protocol, meeting_id, join_url)
             VALUES ('event', 'zoom', 'zoom', 'meeting', 'https://example.test')",
            [],
        )
        .unwrap();

        let error = delete_account_data(&conn, "zoom", &token_guard).unwrap_err();

        assert!(error.to_string().contains("meetings still require it"));
        assert!(account_exists(&conn, "zoom"));
        assert!(store.load("zoom").unwrap().is_some());
        assert_eq!(store.deletes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn pending_meeting_blocks_zoom_account_and_token_deletion() {
        let conn = setup_db();
        let store = Arc::new(FakeTokenStore::default());
        let providers = providers(store.clone());
        let token_guard = providers.lock_zoom_tokens("zoom").await;
        insert_zoom_account(
            &conn,
            "zoom",
            &account_config("zoom"),
            &tokens(),
            &token_guard,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meet_pending_meetings
                (lifecycle_id, account_id, protocol, meeting_id, join_url)
             VALUES ('lifecycle', 'zoom', 'zoom', 'meeting', 'https://example.test')",
            [],
        )
        .unwrap();

        let error = delete_account_data(&conn, "zoom", &token_guard).unwrap_err();

        assert!(error.to_string().contains("meetings still require it"));
        assert!(account_exists(&conn, "zoom"));
        assert!(store.load("zoom").unwrap().is_some());
        assert_eq!(store.deletes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unrelated_meeting_does_not_block_zoom_account_deletion() {
        let conn = setup_db();
        let store = Arc::new(FakeTokenStore::default());
        let providers = providers(store.clone());
        let token_guard = providers.lock_zoom_tokens("zoom").await;
        insert_zoom_account(
            &conn,
            "zoom",
            &account_config("zoom"),
            &tokens(),
            &token_guard,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meet_pending_meetings
                (lifecycle_id, account_id, protocol, meeting_id, join_url)
             VALUES ('lifecycle', 'other', 'zoom', 'meeting', 'https://example.test')",
            [],
        )
        .unwrap();

        delete_account_data(&conn, "zoom", &token_guard).unwrap();

        assert!(!account_exists(&conn, "zoom"));
        assert!(store.load("zoom").unwrap().is_none());
    }

    #[tokio::test]
    async fn account_deletion_queues_meeting_owned_by_another_account() {
        let conn = setup_db();
        let store = Arc::new(FakeTokenStore::default());
        let providers = providers(store.clone());
        let token_guard = providers.lock_zoom_tokens("calendar").await;
        insert_zoom_account(
            &conn,
            "calendar",
            &account_config(""),
            &tokens(),
            &token_guard,
        )
        .unwrap();
        db::accounts::insert_account(&conn, "meet", &account_config("zoom")).unwrap();
        conn.execute(
            "INSERT INTO calendars (id, account_id, name)
             VALUES ('calendar-id', 'calendar', 'Calendar')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendar_events
                (id, account_id, calendar_id, title, start_time, end_time)
             VALUES ('event', 'calendar', 'calendar-id', 'Event', 'start', 'end')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meet_meetings
                (event_id, account_id, protocol, meeting_id, join_url)
             VALUES ('event', 'meet', 'zoom', 'meeting', 'https://example.test')",
            [],
        )
        .unwrap();

        let cleanup_ids = delete_account_data(&conn, "calendar", &token_guard).unwrap();

        assert!(!account_exists(&conn, "calendar"));
        assert!(account_exists(&conn, "meet"));
        assert_eq!(cleanup_ids.len(), 1);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM meet_pending_meetings
                 WHERE account_id = 'meet' AND meeting_id = 'meeting'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        assert!(
            db::meet_pending_meetings::get(&conn, &cleanup_ids[0])
                .unwrap()
                .unwrap()
                .cleanup_requested
        );
    }

    #[tokio::test]
    async fn same_account_bound_event_rolls_back_account_event_and_token_deletion() {
        let conn = setup_db();
        let store = Arc::new(FakeTokenStore::default());
        let providers = providers(store.clone());
        let token_guard = providers.lock_zoom_tokens("zoom").await;
        insert_zoom_account(
            &conn,
            "zoom",
            &account_config("zoom"),
            &tokens(),
            &token_guard,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendars (id, account_id, name)
             VALUES ('calendar-id', 'zoom', 'Calendar')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendar_events
                (id, account_id, calendar_id, title, start_time, end_time)
             VALUES ('event', 'zoom', 'calendar-id', 'Event', 'start', 'end')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meet_meetings
                (event_id, account_id, protocol, meeting_id, join_url)
             VALUES ('event', 'zoom', 'zoom', 'meeting', 'https://example.test')",
            [],
        )
        .unwrap();

        assert!(delete_account_data(&conn, "zoom", &token_guard).is_err());

        assert!(account_exists(&conn, "zoom"));
        assert!(store.load("zoom").unwrap().is_some());
        assert_eq!(store.deletes.load(Ordering::SeqCst), 0);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM calendar_events WHERE id = 'event'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM meet_pending_meetings", [], |row| row
                .get::<_, i64>(
                0
            ),)
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn failure_after_token_delete_restores_snapshot_and_account() {
        let conn = setup_db();
        let store = Arc::new(FakeTokenStore::default());
        let providers = providers(store.clone());
        let token_guard = providers.lock_zoom_tokens("zoom").await;
        let expected = tokens();
        insert_zoom_account(
            &conn,
            "zoom",
            &account_config("zoom"),
            &expected,
            &token_guard,
        )
        .unwrap();
        store.fail_delete_after_remove.store(true, Ordering::SeqCst);

        assert!(delete_account_data(&conn, "zoom", &token_guard).is_err());
        assert!(account_exists(&conn, "zoom"));
        assert_eq!(
            db::service_bindings::list_for_account(&conn, "zoom")
                .unwrap()
                .len(),
            1
        );
        let restored = store.load("zoom").unwrap().unwrap();
        assert_eq!(restored.access_token, expected.access_token);
        assert_eq!(restored.refresh_token, expected.refresh_token);
        assert_eq!(restored.expires_at, expected.expires_at);
    }

    #[tokio::test]
    async fn disabled_zoom_binding_still_deletes_tokens() {
        let conn = setup_db();
        let store = Arc::new(FakeTokenStore::default());
        let providers = providers(store.clone());
        let token_guard = providers.lock_zoom_tokens("zoom").await;
        insert_zoom_account(
            &conn,
            "zoom",
            &account_config("zoom"),
            &tokens(),
            &token_guard,
        )
        .unwrap();
        conn.execute(
            "UPDATE service_bindings SET enabled = 0 WHERE account_id = 'zoom'",
            [],
        )
        .unwrap();

        delete_account_data(&conn, "zoom", &token_guard).unwrap();
        assert!(store.load("zoom").unwrap().is_none());
    }

    #[tokio::test]
    async fn disabled_zoom_with_oauth_mail_binding_is_not_deleted() {
        let conn = setup_db();
        let store = Arc::new(FakeTokenStore::default());
        let providers = providers(store.clone());
        let token_guard = providers.lock_zoom_tokens("mixed").await;
        let mut config = account_config("zoom");
        config.provider = "gmail".into();
        config.mail_protocol = "imap".into();
        config.imap_host = "imap.gmail.com".into();
        insert_zoom_account(&conn, "mixed", &config, &tokens(), &token_guard).unwrap();
        conn.execute(
            "UPDATE service_bindings SET enabled = 0
             WHERE account_id = 'mixed' AND service = 'meet' AND protocol = 'zoom'",
            [],
        )
        .unwrap();

        let error = delete_account_data(&conn, "mixed", &token_guard).unwrap_err();
        assert!(error.to_string().contains("additional service bindings"));
        assert!(account_exists(&conn, "mixed"));
        assert!(
            db::service_bindings::list_for_account(&conn, "mixed")
                .unwrap()
                .len()
                > 1
        );
        assert!(store.load("mixed").unwrap().is_some());
        assert_eq!(store.deletes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn non_zoom_accounts_do_not_touch_provider_token_entries() {
        let conn = setup_db();
        let store = Arc::new(FakeTokenStore::default());
        let providers = providers(store.clone());

        for id in ["gmail", "microsoft", "jmap", "talk", "matrix", "password"] {
            let mut config = account_config("");
            match id {
                "gmail" => {
                    config.provider = "gmail".into();
                    config.mail_protocol = "imap".into();
                    config.imap_host = "imap.gmail.com".into();
                }
                "microsoft" => {
                    config.provider = "o365".into();
                    config.mail_protocol = "graph".into();
                }
                "jmap" => {
                    config.mail_protocol = "jmap".into();
                    config.jmap_url = "https://jmap.example.com".into();
                    config.jmap_auth_method = "oidc".into();
                }
                "talk" | "matrix" => {
                    config.meet_url = "https://meet.example.com".into();
                    config.meet_protocol = id.into();
                }
                "password" => {
                    config.mail_protocol = "imap".into();
                    config.imap_host = "imap.example.com".into();
                }
                _ => unreachable!(),
            }
            db::accounts::insert_account(&conn, id, &config).unwrap();
            store.store(id, &tokens()).unwrap();
            let token_guard = providers.lock_zoom_tokens(id).await;
            delete_account_data(&conn, id, &token_guard).unwrap();
            assert!(store.load(id).unwrap().is_some(), "token changed for {id}");
        }
        assert_eq!(store.deletes.load(Ordering::SeqCst), 0);
    }
}
