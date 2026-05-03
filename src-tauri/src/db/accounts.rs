use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::db::service_bindings::{
    DavBindingConfig, ImapBindingConfig, JmapBindingConfig, ServiceBinding,
};
use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub display_name: String,
    pub email: String,
    pub provider: String,
    pub mail_protocol: String,
    pub enabled: bool,
    /// Phase-4 (#43): per-binding sync intervals so the frontend timers
    /// can honor user preferences without an extra get_account_config
    /// round-trip. `None` means "use the service's default cadence".
    #[serde(default)]
    pub mail_sync_interval_seconds: Option<i64>,
    #[serde(default)]
    pub calendar_sync_interval_seconds: Option<i64>,
    #[serde(default)]
    pub contacts_sync_interval_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub display_name: String,
    pub email: String,
    pub provider: String,
    /// Mail protocol. Empty string means "no mail binding" — used for
    /// standalone CalDAV / CardDAV / JMAP-cal-only accounts.
    pub mail_protocol: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub jmap_url: String,
    pub caldav_url: String,
    pub username: String,
    pub password: String,
    pub use_tls: bool,
    #[serde(default)]
    pub signature: String,
    #[serde(default = "default_basic")]
    pub jmap_auth_method: String,
    #[serde(default)]
    pub oidc_token_endpoint: String,
    #[serde(default)]
    pub oidc_client_id: String,
    #[serde(default = "default_true")]
    pub calendar_sync_enabled: bool,
    /// Whether the mail binding is enabled. Default `true`. Set `false` to
    /// keep a JMAP account's calendar/contacts sync running while turning
    /// off mail (the "JMAP cal-only" use case in #43). For a non-mail
    /// account (mail_protocol == "") this field is ignored.
    #[serde(default = "default_true")]
    pub mail_sync_enabled: bool,
    /// Whether the contacts binding is enabled. Default `true`. Lets the
    /// user disable CardDAV / Google People / Graph contacts for an
    /// account that has them otherwise.
    #[serde(default = "default_true")]
    pub contacts_sync_enabled: bool,
    /// Optional per-binding sync interval in seconds. `None` falls back to
    /// the default cadence for that service (see calendar / contacts
    /// stores on the frontend). Each service has its own field so the
    /// wire format stays explicit.
    #[serde(default)]
    pub mail_sync_interval_seconds: Option<i64>,
    #[serde(default)]
    pub calendar_sync_interval_seconds: Option<i64>,
    #[serde(default)]
    pub contacts_sync_interval_seconds: Option<i64>,
}

fn default_basic() -> String {
    "basic".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct AccountFull {
    pub id: String,
    pub display_name: String,
    pub email: String,
    pub provider: String,
    pub mail_protocol: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub jmap_url: String,
    pub caldav_url: String,
    pub username: String,
    pub password: String,
    pub use_tls: bool,
    pub enabled: bool,
    pub signature: String,
    pub jmap_auth_method: String,
    pub oidc_token_endpoint: String,
    pub oidc_client_id: String,
    pub calendar_sync_enabled: bool,
    /// Phase-2 field: how the user authenticates with this identity.
    /// One of "password" | "oauth-google" | "oauth-microsoft" |
    /// "oauth-jmap-oidc". Populated by the auth_method backfill migration.
    pub auth_method: String,
    /// Phase-2 field: bindings for this account, loaded via
    /// `service_bindings::list_for_account` during `get_account_full`.
    /// Dispatch code uses the helper methods below rather than reading
    /// this field directly.
    pub bindings: Vec<ServiceBinding>,
    /// Phase-4 wire-format mirrors of binding state. Populated from
    /// `bindings` on fetch so the Settings edit form can round-trip the
    /// per-binding `enabled` flags and sync intervals.
    pub mail_sync_enabled: bool,
    pub contacts_sync_enabled: bool,
    pub mail_sync_interval_seconds: Option<i64>,
    pub calendar_sync_interval_seconds: Option<i64>,
    pub contacts_sync_interval_seconds: Option<i64>,
}

impl AccountFull {
    /// Look up the binding for a given service ("mail" | "calendar" |
    /// "contacts"). Returns `None` if the account has no binding for that
    /// service (e.g. a CalDAV-only account has no mail binding).
    pub fn binding_for(&self, service: &str) -> Option<&ServiceBinding> {
        self.bindings.iter().find(|b| b.service == service)
    }

    pub fn mail_binding(&self) -> Option<&ServiceBinding> {
        self.binding_for("mail")
    }

    pub fn calendar_binding(&self) -> Option<&ServiceBinding> {
        self.binding_for("calendar")
    }

    pub fn contacts_binding(&self) -> Option<&ServiceBinding> {
        self.binding_for("contacts")
    }

    /// Mail protocol as a string slice. Returns `""` for accounts with no
    /// mail binding (calendar-only / contacts-only) AND for accounts whose
    /// mail binding is explicitly disabled (e.g. a JMAP server used for
    /// calendar/contacts only). Replaces direct reads of
    /// `account.mail_protocol` at dispatch sites — a disabled binding
    /// short-circuits every protocol-specific branch.
    pub fn mail_protocol_str(&self) -> &str {
        self.mail_binding()
            .filter(|b| b.enabled)
            .map(|b| b.protocol.as_str())
            .unwrap_or("")
    }

    pub fn calendar_protocol_str(&self) -> &str {
        self.calendar_binding()
            .filter(|b| b.enabled)
            .map(|b| b.protocol.as_str())
            .unwrap_or("")
    }

    pub fn contacts_protocol_str(&self) -> &str {
        self.contacts_binding()
            .filter(|b| b.enabled)
            .map(|b| b.protocol.as_str())
            .unwrap_or("")
    }

    /// Parsed IMAP/SMTP config from the mail binding, if it's an IMAP binding.
    /// Returns `None` for non-IMAP mail accounts (graph/jmap) so callers can
    /// pattern-match cleanly. Returns `Some(default)` if the binding exists
    /// but the JSON parses with all defaults — that shouldn't happen in
    /// practice but won't panic.
    pub fn mail_imap_config(&self) -> Option<ImapBindingConfig> {
        self.mail_binding()
            .filter(|b| b.protocol == "imap")
            .and_then(|b| b.imap_config().ok())
    }

    pub fn mail_jmap_config(&self) -> Option<JmapBindingConfig> {
        self.mail_binding()
            .filter(|b| b.protocol == "jmap")
            .and_then(|b| b.jmap_config().ok())
    }

    pub fn calendar_caldav_config(&self) -> Option<DavBindingConfig> {
        self.calendar_binding()
            .filter(|b| b.protocol == "caldav")
            .and_then(|b| b.dav_config().ok())
    }

    pub fn contacts_carddav_config(&self) -> Option<DavBindingConfig> {
        self.contacts_binding()
            .filter(|b| b.protocol == "carddav")
            .and_then(|b| b.dav_config().ok())
    }

    /// Calendar URL for any DAV-style calendar (caldav today; future
    /// caldav-over-google-fallback could land here too).
    pub fn calendar_dav_url(&self) -> Option<String> {
        self.calendar_caldav_config().map(|c| c.url)
    }

    pub fn contacts_dav_url(&self) -> Option<String> {
        self.contacts_carddav_config().map(|c| c.url)
    }

    /// Whether the calendar binding is enabled (replaces `calendar_sync_enabled`
    /// on the legacy schema). Returns `false` if the account has no calendar
    /// binding at all.
    pub fn calendar_enabled(&self) -> bool {
        self.calendar_binding().is_some_and(|b| b.enabled)
    }

    /// Populate the legacy per-protocol fields (`provider`, `mail_protocol`,
    /// `imap_host`, ...) from the loaded `bindings` and `auth_method`.
    /// Phase 3 dropped these columns from the database, but the fields are
    /// still part of `AccountFull` so the wire format and the dispatch sites
    /// touched in earlier phases keep working unchanged.
    pub fn populate_legacy_from_bindings(&mut self) {
        self.provider = match self.auth_method.as_str() {
            "oauth-google" => "gmail",
            "oauth-microsoft" => "o365",
            _ => "generic",
        }
        .to_string();

        self.jmap_auth_method = if self.auth_method == "oauth-jmap-oidc" {
            "oidc"
        } else {
            "basic"
        }
        .to_string();

        self.mail_protocol = self
            .mail_binding()
            .map(|b| b.protocol.clone())
            .unwrap_or_default();

        if let Some(c) = self.mail_imap_config() {
            self.imap_host = c.imap_host;
            self.imap_port = c.imap_port;
            self.smtp_host = c.smtp_host;
            self.smtp_port = c.smtp_port;
            self.use_tls = c.use_tls;
        } else {
            // Sensible defaults for non-IMAP accounts.
            self.imap_host = String::new();
            self.imap_port = 993;
            self.smtp_host = String::new();
            self.smtp_port = 587;
            self.use_tls = true;
        }

        self.jmap_url = self
            .mail_jmap_config()
            .map(|c| c.url)
            .unwrap_or_default();

        // The legacy `caldav_url` column was a single string used for both
        // CalDAV and CardDAV (same server in practice). Phase 3 splits it into
        // independent bindings, so we surface the calendar URL here and fall
        // back to the contacts URL for accounts that have only carddav.
        self.caldav_url = self
            .calendar_dav_url()
            .or_else(|| self.contacts_dav_url())
            .unwrap_or_default();

        self.calendar_sync_enabled = self
            .calendar_binding()
            .map(|b| b.enabled)
            .unwrap_or(true);

        // Phase-4: surface per-binding state on the wire format so the
        // Settings edit form sees the toggles' current value.
        self.mail_sync_enabled = self
            .mail_binding()
            .map(|b| b.enabled)
            .unwrap_or(true);
        self.contacts_sync_enabled = self
            .contacts_binding()
            .map(|b| b.enabled)
            .unwrap_or(true);
        self.mail_sync_interval_seconds =
            self.mail_binding().and_then(|b| b.sync_interval_seconds);
        self.calendar_sync_interval_seconds =
            self.calendar_binding().and_then(|b| b.sync_interval_seconds);
        self.contacts_sync_interval_seconds =
            self.contacts_binding().and_then(|b| b.sync_interval_seconds);
    }
}

pub fn list_accounts(conn: &Connection) -> Result<Vec<Account>> {
    // mail_protocol comes from the mail service binding (LEFT JOIN so
    // calendar-only accounts show up with an empty mail_protocol).
    // provider is derived from auth_method on the way out so the wire
    // format stays compatible with the frontend.
    // Pull the mail protocol and per-binding sync intervals via correlated
    // subqueries against service_bindings so the lightweight Account
    // summary doesn't need a separate per-account query for the
    // periodic-sync timers.
    let mut stmt = conn.prepare(
        "SELECT a.id, a.display_name, a.email, a.auth_method, a.enabled,
                COALESCE(
                    (SELECT b.protocol FROM service_bindings b
                     WHERE b.account_id = a.id
                       AND b.service = 'mail'
                       AND b.enabled = 1
                     LIMIT 1),
                    ''
                ) AS mail_protocol,
                (SELECT b.sync_interval_seconds FROM service_bindings b
                 WHERE b.account_id = a.id AND b.service = 'mail' LIMIT 1)
                    AS mail_sync_interval,
                (SELECT b.sync_interval_seconds FROM service_bindings b
                 WHERE b.account_id = a.id AND b.service = 'calendar' LIMIT 1)
                    AS calendar_sync_interval,
                (SELECT b.sync_interval_seconds FROM service_bindings b
                 WHERE b.account_id = a.id AND b.service = 'contacts' LIMIT 1)
                    AS contacts_sync_interval
         FROM accounts a
         ORDER BY a.display_name",
    )?;
    let accounts = stmt
        .query_map([], |row| {
            let auth_method: String = row.get(3)?;
            let provider = match auth_method.as_str() {
                "oauth-google" => "gmail",
                "oauth-microsoft" => "o365",
                _ => "generic",
            }
            .to_string();
            Ok(Account {
                id: row.get(0)?,
                display_name: row.get(1)?,
                email: row.get(2)?,
                provider,
                mail_protocol: row.get(5)?,
                enabled: row.get(4)?,
                mail_sync_interval_seconds: row.get(6)?,
                calendar_sync_interval_seconds: row.get(7)?,
                contacts_sync_interval_seconds: row.get(8)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(accounts)
}

pub fn get_account_full(conn: &Connection, id: &str) -> Result<AccountFull> {
    let mut account = conn.query_row(
        "SELECT id, display_name, email, username, enabled, signature,
                oidc_token_endpoint, oidc_client_id, auth_method
         FROM accounts WHERE id = ?1",
        params![id],
        |row| {
            Ok(AccountFull {
                id: row.get(0)?,
                display_name: row.get(1)?,
                email: row.get(2)?,
                username: row.get(3)?,
                enabled: row.get(4)?,
                signature: row.get(5)?,
                oidc_token_endpoint: row.get(6)?,
                oidc_client_id: row.get(7)?,
                auth_method: row.get(8)?,
                // Legacy fields populated below from bindings + auth_method.
                provider: String::new(),
                mail_protocol: String::new(),
                jmap_auth_method: String::new(),
                imap_host: String::new(),
                imap_port: 993,
                smtp_host: String::new(),
                smtp_port: 587,
                jmap_url: String::new(),
                caldav_url: String::new(),
                password: String::new(),
                use_tls: true,
                calendar_sync_enabled: true,
                bindings: Vec::new(),
                mail_sync_enabled: true,
                contacts_sync_enabled: true,
                mail_sync_interval_seconds: None,
                calendar_sync_interval_seconds: None,
                contacts_sync_interval_seconds: None,
            })
        },
    ).map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => crate::error::Error::AccountNotFound(id.to_string()),
        other => crate::error::Error::Database(other),
    })?;

    // Phase-3: bindings are the source of truth. Load them, then populate
    // the legacy AccountFull fields from the bindings + auth_method so the
    // wire format stays unchanged.
    account.bindings = crate::db::service_bindings::list_for_account(conn, id)?;
    account.populate_legacy_from_bindings();

    // Fetch password from the system keyring. OIDC/OAuth accounts don't
    // store a keyring password here — their tokens live under the
    // `.oauth` service — so a missing entry is expected, not an error.
    match crate::keyring::get_password(&account.id) {
        Ok(Some(pw)) => account.password = pw,
        Ok(None) => {
            log::debug!("No keyring password for account {}", account.id);
        }
        Err(e) => {
            log::warn!(
                "Could not read password from keyring for account {}: {}",
                account.id,
                e
            );
        }
    }

    Ok(account)
}

pub fn insert_account(conn: &Connection, id: &str, config: &AccountConfig) -> Result<()> {
    // Store real passwords in system keyring; skip OIDC accounts and oauth2 migration markers
    if !config.password.is_empty()
        && config.jmap_auth_method != "oidc"
        && !config.password.starts_with("oauth2:")
    {
        crate::keyring::set_password(id, &config.password)?;
    }

    let auth_method =
        crate::db::service_bindings::auth_method_for(&config.provider, &config.jmap_auth_method);

    conn.execute(
        "INSERT INTO accounts (id, display_name, email, username, signature,
                               oidc_token_endpoint, oidc_client_id, auth_method)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            config.display_name,
            config.email,
            config.username,
            config.signature,
            config.oidc_token_endpoint,
            config.oidc_client_id,
            auth_method,
        ],
    )?;
    crate::db::service_bindings::rebuild_for_account(
        conn,
        id,
        config_to_legacy_fields(id, true, config),
    )?;
    Ok(())
}

/// Build a `LegacyBindingFields` view over an `AccountConfig`. Used by
/// `insert_account` / `update_account` so writes go through the same
/// `derive_bindings` rules as the Phase-1 populate migration.
fn config_to_legacy_fields<'a>(
    account_id: &'a str,
    enabled: bool,
    config: &'a AccountConfig,
) -> crate::db::service_bindings::LegacyBindingFields<'a> {
    crate::db::service_bindings::LegacyBindingFields {
        account_id,
        enabled,
        provider: &config.provider,
        mail_protocol: &config.mail_protocol,
        imap_host: &config.imap_host,
        imap_port: config.imap_port,
        smtp_host: &config.smtp_host,
        smtp_port: config.smtp_port,
        use_tls: config.use_tls,
        jmap_url: &config.jmap_url,
        jmap_auth_method: &config.jmap_auth_method,
        oidc_token_endpoint: &config.oidc_token_endpoint,
        oidc_client_id: &config.oidc_client_id,
        caldav_url: &config.caldav_url,
        calendar_sync_enabled: config.calendar_sync_enabled,
        mail_sync_enabled: Some(config.mail_sync_enabled),
        contacts_sync_enabled: Some(config.contacts_sync_enabled),
        mail_sync_interval_seconds: config.mail_sync_interval_seconds,
        calendar_sync_interval_seconds: config.calendar_sync_interval_seconds,
        contacts_sync_interval_seconds: config.contacts_sync_interval_seconds,
    }
}

pub fn update_account(conn: &Connection, id: &str, config: &AccountConfig) -> Result<()> {
    // Only update keyring if a real password was provided; skip OIDC accounts and oauth2 markers.
    if !config.password.is_empty()
        && config.jmap_auth_method != "oidc"
        && !config.password.starts_with("oauth2:")
    {
        crate::keyring::set_password(id, &config.password)?;
    }

    let auth_method =
        crate::db::service_bindings::auth_method_for(&config.provider, &config.jmap_auth_method);

    let rows = conn.execute(
        "UPDATE accounts
         SET display_name=?1, email=?2, username=?3, signature=?4,
             oidc_token_endpoint=?5, oidc_client_id=?6, auth_method=?7,
             updated_at=CURRENT_TIMESTAMP
         WHERE id=?8",
        params![
            config.display_name,
            config.email,
            config.username,
            config.signature,
            config.oidc_token_endpoint,
            config.oidc_client_id,
            auth_method,
            id,
        ],
    )?;
    if rows == 0 {
        return Err(crate::error::Error::AccountNotFound(id.to_string()));
    }
    // Preserve the existing enabled flag — AccountConfig doesn't carry
    // enabled/disabled, so the previous binding's state would otherwise
    // get clobbered to `true` on every update.
    let enabled: bool = conn
        .query_row(
            "SELECT enabled FROM accounts WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .unwrap_or(true);
    crate::db::service_bindings::rebuild_for_account(
        conn,
        id,
        config_to_legacy_fields(id, enabled, config),
    )?;
    log::info!("Updated account {}", id);
    Ok(())
}

pub fn delete_account(conn: &Connection, id: &str) -> Result<()> {
    // Remove password from keyring (best-effort, don't block deletion)
    if let Err(e) = crate::keyring::delete_password(id) {
        log::warn!("Failed to remove keyring entry for account {}: {}", id, e);
    }
    conn.execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // Phase-3 schema: identity-only columns plus service_bindings.
        conn.execute_batch(
            "
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
            ",
        )
        .unwrap();
        conn
    }

    fn unique_id() -> String {
        format!("test-{}", uuid::Uuid::new_v4())
    }

    fn make_config(email: &str, name: &str) -> AccountConfig {
        AccountConfig {
            display_name: name.to_string(),
            email: email.to_string(),
            provider: "generic".to_string(),
            mail_protocol: "imap".to_string(),
            imap_host: "imap.example.com".to_string(),
            imap_port: 993,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            jmap_url: String::new(),
            caldav_url: String::new(),
            username: "user".to_string(),
            password: "secret123".to_string(),
            use_tls: true,
            signature: String::new(),
            jmap_auth_method: "basic".to_string(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
            calendar_sync_enabled: true,
            mail_sync_enabled: true,
            contacts_sync_enabled: true,
            mail_sync_interval_seconds: None,
            calendar_sync_interval_seconds: None,
            contacts_sync_interval_seconds: None,
        }
    }

    #[test]
    fn test_list_accounts_empty() {
        let conn = setup_db();
        let accounts = list_accounts(&conn).unwrap();
        assert!(accounts.is_empty());
    }

    #[test]
    fn test_insert_and_list_accounts() {
        let conn = setup_db();
        let id = unique_id();
        let config = make_config("alice@example.com", "Alice");
        insert_account(&conn, &id, &config).unwrap();

        let accounts = list_accounts(&conn).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].email, "alice@example.com");
        assert_eq!(accounts[0].display_name, "Alice");
        assert!(accounts[0].enabled);
        // Cleanup keyring
        crate::keyring::delete_password(&id).ok();
    }

    #[test]
    fn test_get_account_full_reads_all_fields() {
        let conn = setup_db();
        let id = unique_id();
        let config = make_config("alice@example.com", "Alice");
        insert_account(&conn, &id, &config).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert_eq!(full.email, "alice@example.com");
        assert_eq!(full.imap_host, "imap.example.com");
        assert_eq!(full.imap_port, 993);
        assert_eq!(full.smtp_host, "smtp.example.com");
        assert_eq!(full.smtp_port, 587);
        assert_eq!(full.username, "user");
        assert!(full.use_tls);
        crate::keyring::delete_password(&id).ok();
    }

    #[test]
    fn test_get_account_full_not_found() {
        let conn = setup_db();
        let result = get_account_full(&conn, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_update_account() {
        let conn = setup_db();
        let id = unique_id();
        let config = make_config("alice@example.com", "Alice");
        insert_account(&conn, &id, &config).unwrap();

        let mut updated = config.clone();
        updated.display_name = "Alice Updated".to_string();
        updated.imap_host = "new-imap.example.com".to_string();
        update_account(&conn, &id, &updated).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert_eq!(full.display_name, "Alice Updated");
        assert_eq!(full.imap_host, "new-imap.example.com");
        crate::keyring::delete_password(&id).ok();
    }

    #[test]
    fn test_update_nonexistent_account() {
        let conn = setup_db();
        let id = unique_id();
        let config = make_config("alice@example.com", "Alice");
        // Store password so keyring call doesn't fail
        crate::keyring::set_password(&id, "test").ok();
        let result = update_account(&conn, &id, &config);
        assert!(result.is_err());
        crate::keyring::delete_password(&id).ok();
    }

    #[test]
    fn test_delete_account() {
        let conn = setup_db();
        let id = unique_id();
        let config = make_config("alice@example.com", "Alice");
        insert_account(&conn, &id, &config).unwrap();

        delete_account(&conn, &id).unwrap();
        let accounts = list_accounts(&conn).unwrap();
        assert!(accounts.is_empty());
    }

    #[test]
    fn test_no_password_column_in_db() {
        let conn = setup_db();
        let has_password = conn
            .prepare("SELECT password FROM accounts LIMIT 0")
            .is_ok();
        assert!(!has_password, "DB should not have a password column");
    }

    #[test]
    fn test_multiple_accounts() {
        let conn = setup_db();
        let id1 = unique_id();
        let id2 = unique_id();
        insert_account(&conn, &id1, &make_config("alice@example.com", "Alice")).unwrap();
        insert_account(&conn, &id2, &make_config("bob@example.com", "Bob")).unwrap();

        let accounts = list_accounts(&conn).unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].display_name, "Alice");
        assert_eq!(accounts[1].display_name, "Bob");
        crate::keyring::delete_password(&id1).ok();
        crate::keyring::delete_password(&id2).ok();
    }

    #[test]
    fn test_signature_persists() {
        let conn = setup_db();
        let id = unique_id();
        let mut config = make_config("alice@example.com", "Alice");
        config.signature = "-- \nAlice Smith\nSenior Engineer".to_string();
        insert_account(&conn, &id, &config).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert_eq!(full.signature, "-- \nAlice Smith\nSenior Engineer");

        // Update signature
        let mut updated = config.clone();
        updated.signature = "-- \nAlice S.".to_string();
        update_account(&conn, &id, &updated).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert_eq!(full.signature, "-- \nAlice S.");
        crate::keyring::delete_password(&id).ok();
    }

    #[test]
    fn test_calendar_sync_enabled_serde_default_true() {
        // Older renderers may send AccountConfig payloads without the new
        // calendar_sync_enabled field; the serde default must yield true so
        // existing accounts keep syncing calendars.
        let json = r#"{
            "display_name": "Alice",
            "email": "a@example.com",
            "provider": "generic",
            "mail_protocol": "imap",
            "imap_host": "imap.example.com",
            "imap_port": 993,
            "smtp_host": "smtp.example.com",
            "smtp_port": 587,
            "jmap_url": "",
            "caldav_url": "",
            "username": "u",
            "password": "p",
            "use_tls": true
        }"#;
        let cfg: AccountConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.calendar_sync_enabled);
    }

    #[test]
    fn test_calendar_sync_enabled_defaults_true_and_persists_toggle() {
        let conn = setup_db();
        let id = unique_id();
        // calendar_sync_enabled now lives on the calendar binding's
        // `enabled` flag. Use an IMAP+CalDAV account so a calendar
        // binding actually gets created and the toggle has somewhere
        // to round-trip through.
        let mut config = make_config("alice@example.com", "Alice");
        config.caldav_url = "https://dav.example.com/cal".into();
        assert!(config.calendar_sync_enabled);
        insert_account(&conn, &id, &config).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert!(full.calendar_sync_enabled);

        let mut updated = config.clone();
        updated.calendar_sync_enabled = false;
        update_account(&conn, &id, &updated).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert!(!full.calendar_sync_enabled);
        crate::keyring::delete_password(&id).ok();
    }

    #[test]
    fn test_jmap_account() {
        let conn = setup_db();
        let id = unique_id();
        let mut config = make_config("kushal@example.com", "JMAP Account");
        config.mail_protocol = "jmap".to_string();
        config.jmap_url = "https://jmap.example.com".to_string();
        insert_account(&conn, &id, &config).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert_eq!(full.mail_protocol, "jmap");
        assert_eq!(full.jmap_url, "https://jmap.example.com");
        crate::keyring::delete_password(&id).ok();
    }
}
