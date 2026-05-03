use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::db::service_bindings::ServiceBinding;
use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub display_name: String,
    pub email: String,
    pub provider: String,
    pub mail_protocol: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
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
    /// mail binding (calendar-only / contacts-only). Replaces direct reads
    /// of `account.mail_protocol` at dispatch sites.
    pub fn mail_protocol_str(&self) -> &str {
        self.mail_binding()
            .map(|b| b.protocol.as_str())
            .unwrap_or("")
    }

    pub fn calendar_protocol_str(&self) -> &str {
        self.calendar_binding()
            .map(|b| b.protocol.as_str())
            .unwrap_or("")
    }

    pub fn contacts_protocol_str(&self) -> &str {
        self.contacts_binding()
            .map(|b| b.protocol.as_str())
            .unwrap_or("")
    }
}

pub fn list_accounts(conn: &Connection) -> Result<Vec<Account>> {
    let mut stmt = conn.prepare(
        "SELECT id, display_name, email, provider, mail_protocol, enabled FROM accounts ORDER BY display_name",
    )?;
    let accounts = stmt
        .query_map([], |row| {
            Ok(Account {
                id: row.get(0)?,
                display_name: row.get(1)?,
                email: row.get(2)?,
                provider: row.get(3)?,
                mail_protocol: row.get(4)?,
                enabled: row.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(accounts)
}

pub fn get_account_full(conn: &Connection, id: &str) -> Result<AccountFull> {
    let mut account = conn.query_row(
        "SELECT id, display_name, email, provider, mail_protocol, imap_host, imap_port, smtp_host, smtp_port, jmap_url, caldav_url, username, use_tls, enabled, signature, jmap_auth_method, oidc_token_endpoint, oidc_client_id, calendar_sync_enabled, auth_method FROM accounts WHERE id = ?1",
        params![id],
        |row| {
            Ok(AccountFull {
                id: row.get(0)?,
                display_name: row.get(1)?,
                email: row.get(2)?,
                provider: row.get(3)?,
                mail_protocol: row.get(4)?,
                imap_host: row.get(5)?,
                imap_port: row.get::<_, u32>(6)? as u16,
                smtp_host: row.get(7)?,
                smtp_port: row.get::<_, u32>(8)? as u16,
                jmap_url: row.get(9)?,
                caldav_url: row.get(10)?,
                username: row.get(11)?,
                password: String::new(),
                use_tls: row.get(12)?,
                enabled: row.get(13)?,
                signature: row.get(14)?,
                jmap_auth_method: row.get(15)?,
                oidc_token_endpoint: row.get(16)?,
                oidc_client_id: row.get(17)?,
                calendar_sync_enabled: row.get(18)?,
                auth_method: row.get(19)?,
                bindings: Vec::new(),
            })
        },
    ).map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => crate::error::Error::AccountNotFound(id.to_string()),
        other => crate::error::Error::Database(other),
    })?;

    // Phase-2: load service_bindings so dispatch helpers can read the
    // protocol-of-record. A missing or incomplete binding row falls back
    // to whatever the legacy column says via the *_str helpers.
    account.bindings = crate::db::service_bindings::list_for_account(conn, id)?;

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
        "INSERT INTO accounts (id, display_name, email, provider, mail_protocol, imap_host, imap_port, smtp_host, smtp_port, jmap_url, caldav_url, username, use_tls, signature, jmap_auth_method, oidc_token_endpoint, oidc_client_id, calendar_sync_enabled, auth_method)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            id,
            config.display_name,
            config.email,
            config.provider,
            config.mail_protocol,
            config.imap_host,
            config.imap_port,
            config.smtp_host,
            config.smtp_port,
            config.jmap_url,
            config.caldav_url,
            config.username,
            config.use_tls,
            config.signature,
            config.jmap_auth_method,
            config.oidc_token_endpoint,
            config.oidc_client_id,
            config.calendar_sync_enabled,
            auth_method,
        ],
    )?;
    crate::db::service_bindings::rebuild_for_account(conn, id)?;
    Ok(())
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
        "UPDATE accounts SET display_name=?1, email=?2, provider=?3, mail_protocol=?4,
         imap_host=?5, imap_port=?6, smtp_host=?7, smtp_port=?8, jmap_url=?9,
         caldav_url=?10, username=?11, use_tls=?12, signature=?13,
         jmap_auth_method=?14, oidc_token_endpoint=?15, oidc_client_id=?16,
         calendar_sync_enabled=?17, auth_method=?18,
         updated_at=CURRENT_TIMESTAMP
         WHERE id=?19",
        params![
            config.display_name,
            config.email,
            config.provider,
            config.mail_protocol,
            config.imap_host,
            config.imap_port,
            config.smtp_host,
            config.smtp_port,
            config.jmap_url,
            config.caldav_url,
            config.username,
            config.use_tls,
            config.signature,
            config.jmap_auth_method,
            config.oidc_token_endpoint,
            config.oidc_client_id,
            config.calendar_sync_enabled,
            auth_method,
            id,
        ],
    )?;
    if rows == 0 {
        return Err(crate::error::Error::AccountNotFound(id.to_string()));
    }
    crate::db::service_bindings::rebuild_for_account(conn, id)?;
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
        conn.execute_batch(
            "
            CREATE TABLE accounts (
                id TEXT PRIMARY KEY,
                display_name TEXT NOT NULL,
                email TEXT NOT NULL,
                provider TEXT NOT NULL,
                mail_protocol TEXT NOT NULL DEFAULT 'imap',
                imap_host TEXT NOT NULL DEFAULT '',
                imap_port INTEGER NOT NULL DEFAULT 993,
                smtp_host TEXT NOT NULL DEFAULT '',
                smtp_port INTEGER NOT NULL DEFAULT 587,
                jmap_url TEXT NOT NULL DEFAULT '',
                caldav_url TEXT NOT NULL DEFAULT '',
                username TEXT NOT NULL,
                use_tls INTEGER NOT NULL DEFAULT 1,
                enabled INTEGER NOT NULL DEFAULT 1,
                signature TEXT NOT NULL DEFAULT '',
                jmap_auth_method TEXT NOT NULL DEFAULT 'basic',
                oidc_token_endpoint TEXT NOT NULL DEFAULT '',
                oidc_client_id TEXT NOT NULL DEFAULT '',
                calendar_sync_enabled INTEGER NOT NULL DEFAULT 1,
                auth_method TEXT NOT NULL DEFAULT '',
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
        let config = make_config("alice@example.com", "Alice");
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
