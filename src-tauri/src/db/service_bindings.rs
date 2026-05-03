use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};

// ---- Typed binding-config payloads ----------------------------------------
//
// Each binding stores its protocol-specific settings as JSON in `config_json`.
// These structs let callers go through `serde_json::from_str` once and then
// touch typed fields. A missing key falls through to its `Default`, so
// partial / older config rows still parse.

fn default_imap_port() -> u16 {
    993
}
fn default_smtp_port() -> u16 {
    587
}
fn default_use_tls() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImapBindingConfig {
    #[serde(default)]
    pub imap_host: String,
    #[serde(default = "default_imap_port")]
    pub imap_port: u16,
    #[serde(default)]
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    #[serde(default = "default_use_tls")]
    pub use_tls: bool,
}

impl Default for ImapBindingConfig {
    fn default() -> Self {
        Self {
            imap_host: String::new(),
            imap_port: default_imap_port(),
            smtp_host: String::new(),
            smtp_port: default_smtp_port(),
            use_tls: default_use_tls(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JmapBindingConfig {
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DavBindingConfig {
    #[serde(default)]
    pub url: String,
}

/// A per-service binding that says "this account talks to this protocol for
/// this service". One identity in the `accounts` table can carry multiple
/// bindings: a Gmail account has bindings for `mail/imap`, `calendar/google`,
/// and `contacts/google`; a Nextcloud-hosted IMAP account adds `calendar/caldav`
/// and `contacts/carddav` for the same identity.
///
/// Phase 1 of the bindings rollout: rows are populated alongside the existing
/// per-protocol columns on `accounts`, but no dispatch code reads from here
/// yet. Phase 2 switches reads, Phase 3 drops the old columns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceBinding {
    pub id: String,
    pub account_id: String,
    pub service: String,
    pub protocol: String,
    pub enabled: bool,
    pub sync_interval_seconds: Option<i64>,
    pub config_json: String,
}

impl ServiceBinding {
    pub fn imap_config(&self) -> Result<ImapBindingConfig> {
        serde_json::from_str(&self.config_json).map_err(|e| {
            Error::Other(format!(
                "binding {} has malformed imap config_json: {}",
                self.id, e
            ))
        })
    }

    pub fn jmap_config(&self) -> Result<JmapBindingConfig> {
        serde_json::from_str(&self.config_json).map_err(|e| {
            Error::Other(format!(
                "binding {} has malformed jmap config_json: {}",
                self.id, e
            ))
        })
    }

    pub fn dav_config(&self) -> Result<DavBindingConfig> {
        serde_json::from_str(&self.config_json).map_err(|e| {
            Error::Other(format!(
                "binding {} has malformed dav config_json: {}",
                self.id, e
            ))
        })
    }
}

pub fn list_for_account(conn: &Connection, account_id: &str) -> Result<Vec<ServiceBinding>> {
    let mut stmt = conn.prepare(
        "SELECT id, account_id, service, protocol, enabled, sync_interval_seconds, config_json
         FROM service_bindings
         WHERE account_id = ?1
         ORDER BY service, protocol",
    )?;
    let rows = stmt
        .query_map(params![account_id], |row| {
            Ok(ServiceBinding {
                id: row.get(0)?,
                account_id: row.get(1)?,
                service: row.get(2)?,
                protocol: row.get(3)?,
                enabled: row.get(4)?,
                sync_interval_seconds: row.get(5)?,
                config_json: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn list_all(conn: &Connection) -> Result<Vec<ServiceBinding>> {
    let mut stmt = conn.prepare(
        "SELECT id, account_id, service, protocol, enabled, sync_interval_seconds, config_json
         FROM service_bindings
         ORDER BY account_id, service, protocol",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ServiceBinding {
                id: row.get(0)?,
                account_id: row.get(1)?,
                service: row.get(2)?,
                protocol: row.get(3)?,
                enabled: row.get(4)?,
                sync_interval_seconds: row.get(5)?,
                config_json: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn insert(conn: &Connection, b: &ServiceBinding) -> Result<()> {
    conn.execute(
        "INSERT INTO service_bindings
         (id, account_id, service, protocol, enabled, sync_interval_seconds, config_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            b.id,
            b.account_id,
            b.service,
            b.protocol,
            b.enabled,
            b.sync_interval_seconds,
            b.config_json,
        ],
    )?;
    Ok(())
}

pub fn update(conn: &Connection, b: &ServiceBinding) -> Result<()> {
    let rows = conn.execute(
        "UPDATE service_bindings
         SET enabled = ?1, sync_interval_seconds = ?2, config_json = ?3,
             updated_at = CURRENT_TIMESTAMP
         WHERE id = ?4",
        params![b.enabled, b.sync_interval_seconds, b.config_json, b.id],
    )?;
    if rows == 0 {
        return Err(crate::error::Error::Other(format!(
            "service_binding {} not found",
            b.id
        )));
    }
    Ok(())
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM service_bindings WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn delete_for_account(conn: &Connection, account_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM service_bindings WHERE account_id = ?1",
        params![account_id],
    )?;
    Ok(())
}

/// Snapshot of the legacy per-protocol fields needed to derive bindings.
/// `AccountFull` and `AccountConfig` both supply these; the migration
/// reads them straight out of SQL.
pub struct LegacyBindingFields<'a> {
    pub account_id: &'a str,
    /// Default for the mail binding's `enabled` flag when no explicit
    /// `mail_sync_enabled` override is supplied. The migration sets this
    /// to the row's `accounts.enabled` value to preserve old behavior.
    pub enabled: bool,
    pub provider: &'a str,
    pub mail_protocol: &'a str,
    pub imap_host: &'a str,
    pub imap_port: u16,
    pub smtp_host: &'a str,
    pub smtp_port: u16,
    pub use_tls: bool,
    pub jmap_url: &'a str,
    pub jmap_auth_method: &'a str,
    pub oidc_token_endpoint: &'a str,
    pub oidc_client_id: &'a str,
    pub caldav_url: &'a str,
    pub calendar_sync_enabled: bool,
    /// Phase 4 additions. If `None`, falls back to `enabled` for the mail
    /// binding (legacy behavior) or `true` for contacts.
    pub mail_sync_enabled: Option<bool>,
    pub contacts_sync_enabled: Option<bool>,
    pub mail_sync_interval_seconds: Option<i64>,
    pub calendar_sync_interval_seconds: Option<i64>,
    pub contacts_sync_interval_seconds: Option<i64>,
}

/// Derive the set of bindings implied by a `(provider, mail_protocol, ...)`
/// tuple. Used both by the one-time Phase-1 populate migration (which
/// reads the soon-to-be-dropped legacy columns) and by `insert_account` /
/// `update_account` to build bindings from the wire-format `AccountConfig`.
pub fn derive_bindings(f: LegacyBindingFields<'_>) -> Vec<ServiceBinding> {
    let mut out = Vec::new();
    let aid = f.account_id;

    let mail_config = match f.mail_protocol {
        "imap" => serde_json::json!({
            "imap_host": f.imap_host,
            "imap_port": f.imap_port,
            "smtp_host": f.smtp_host,
            "smtp_port": f.smtp_port,
            "use_tls": f.use_tls,
        }),
        "jmap" => serde_json::json!({
            "url": f.jmap_url,
            "auth_method": f.jmap_auth_method,
            "oidc_token_endpoint": f.oidc_token_endpoint,
            "oidc_client_id": f.oidc_client_id,
        }),
        _ => serde_json::json!({}),
    };
    if !f.mail_protocol.is_empty() {
        out.push(ServiceBinding {
            id: format!("{aid}-mail"),
            account_id: aid.into(),
            service: "mail".into(),
            protocol: f.mail_protocol.into(),
            enabled: f.mail_sync_enabled.unwrap_or(f.enabled),
            sync_interval_seconds: f.mail_sync_interval_seconds,
            config_json: mail_config.to_string(),
        });
    }

    // Calendar binding (mirrors the dispatch in commands/calendar.rs).
    let cal: Option<(&str, serde_json::Value)> = if f.mail_protocol == "jmap" {
        Some(("jmap", serde_json::json!({})))
    } else if f.provider == "gmail" {
        Some(("google", serde_json::json!({})))
    } else if f.provider == "o365" {
        Some(("graph", serde_json::json!({})))
    } else if !f.caldav_url.is_empty() {
        Some(("caldav", serde_json::json!({ "url": f.caldav_url })))
    } else {
        None
    };
    if let Some((proto, cfg)) = cal {
        out.push(ServiceBinding {
            id: format!("{aid}-calendar"),
            account_id: aid.into(),
            service: "calendar".into(),
            protocol: proto.into(),
            enabled: f.calendar_sync_enabled,
            sync_interval_seconds: f.calendar_sync_interval_seconds,
            config_json: cfg.to_string(),
        });
    }

    // Contacts binding (mirrors the dispatch in commands/contacts.rs).
    let contacts: Option<(&str, serde_json::Value)> = if f.mail_protocol == "jmap" {
        Some(("jmap", serde_json::json!({})))
    } else if f.provider == "gmail" {
        Some(("google", serde_json::json!({})))
    } else if f.provider == "o365" {
        Some(("graph", serde_json::json!({})))
    } else if !f.caldav_url.is_empty() {
        Some(("carddav", serde_json::json!({ "url": f.caldav_url })))
    } else {
        None
    };
    if let Some((proto, cfg)) = contacts {
        out.push(ServiceBinding {
            id: format!("{aid}-contacts"),
            account_id: aid.into(),
            service: "contacts".into(),
            protocol: proto.into(),
            enabled: f.contacts_sync_enabled.unwrap_or(true),
            sync_interval_seconds: f.contacts_sync_interval_seconds,
            config_json: cfg.to_string(),
        });
    }

    out
}

/// Convenience wrapper used by the unit tests that fed the previous API.
#[cfg(test)]
pub fn derive_bindings_from_account(account: &AccountFull) -> Vec<ServiceBinding> {
    derive_bindings(LegacyBindingFields {
        account_id: &account.id,
        enabled: account.enabled,
        provider: &account.provider,
        mail_protocol: &account.mail_protocol,
        imap_host: &account.imap_host,
        imap_port: account.imap_port,
        smtp_host: &account.smtp_host,
        smtp_port: account.smtp_port,
        use_tls: account.use_tls,
        jmap_url: &account.jmap_url,
        jmap_auth_method: &account.jmap_auth_method,
        oidc_token_endpoint: &account.oidc_token_endpoint,
        oidc_client_id: &account.oidc_client_id,
        caldav_url: &account.caldav_url,
        calendar_sync_enabled: account.calendar_sync_enabled,
        mail_sync_enabled: None,
        contacts_sync_enabled: None,
        mail_sync_interval_seconds: None,
        calendar_sync_interval_seconds: None,
        contacts_sync_interval_seconds: None,
    })
}

/// Re-derive the bindings for a single account from the supplied legacy
/// fields and persist them. Wipes existing rows for `account_id` first so
/// the table converges to the latest derivation rules. Called from
/// `insert_account` / `update_account` (which build the fields from the
/// wire-format `AccountConfig`).
pub fn rebuild_for_account(
    conn: &Connection,
    account_id: &str,
    fields: LegacyBindingFields<'_>,
) -> Result<()> {
    delete_for_account(conn, account_id)?;
    for binding in derive_bindings(fields) {
        insert(conn, &binding)?;
    }
    Ok(())
}

/// Map an old-schema `(provider, jmap_auth_method)` pair to the new
/// `auth_method` value stored on `accounts`. Pure function so the migration
/// and unit tests can both call it.
pub fn auth_method_for(provider: &str, jmap_auth_method: &str) -> &'static str {
    match provider {
        "gmail" => "oauth-google",
        "o365" => "oauth-microsoft",
        _ if jmap_auth_method == "oidc" => "oauth-jmap-oidc",
        _ => "password",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::accounts::AccountFull;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (
                id TEXT PRIMARY KEY,
                display_name TEXT NOT NULL DEFAULT '',
                email TEXT NOT NULL DEFAULT '',
                provider TEXT NOT NULL DEFAULT '',
                mail_protocol TEXT NOT NULL DEFAULT 'imap',
                imap_host TEXT NOT NULL DEFAULT '',
                imap_port INTEGER NOT NULL DEFAULT 993,
                smtp_host TEXT NOT NULL DEFAULT '',
                smtp_port INTEGER NOT NULL DEFAULT 587,
                jmap_url TEXT NOT NULL DEFAULT '',
                caldav_url TEXT NOT NULL DEFAULT '',
                username TEXT NOT NULL DEFAULT '',
                use_tls INTEGER NOT NULL DEFAULT 1,
                enabled INTEGER NOT NULL DEFAULT 1,
                signature TEXT NOT NULL DEFAULT '',
                jmap_auth_method TEXT NOT NULL DEFAULT 'basic',
                oidc_token_endpoint TEXT NOT NULL DEFAULT '',
                oidc_client_id TEXT NOT NULL DEFAULT '',
                calendar_sync_enabled INTEGER NOT NULL DEFAULT 1,
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
            );",
        )
        .unwrap();
        conn
    }

    fn account_row(id: &str) -> AccountFull {
        AccountFull {
            id: id.into(),
            display_name: "Test".into(),
            email: "test@example.com".into(),
            provider: "generic".into(),
            mail_protocol: "imap".into(),
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            jmap_url: String::new(),
            caldav_url: String::new(),
            username: "user".into(),
            password: String::new(),
            use_tls: true,
            enabled: true,
            signature: String::new(),
            jmap_auth_method: "basic".into(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
            calendar_sync_enabled: true,
            auth_method: String::new(),
            bindings: Vec::new(),
            mail_sync_enabled: true,
            contacts_sync_enabled: true,
            mail_sync_interval_seconds: None,
            calendar_sync_interval_seconds: None,
            contacts_sync_interval_seconds: None,
        }
    }

    #[test]
    fn auth_method_mapping() {
        assert_eq!(auth_method_for("gmail", "basic"), "oauth-google");
        assert_eq!(auth_method_for("o365", "basic"), "oauth-microsoft");
        assert_eq!(auth_method_for("generic", "oidc"), "oauth-jmap-oidc");
        assert_eq!(auth_method_for("generic", "basic"), "password");
        assert_eq!(auth_method_for("", ""), "password");
    }

    #[test]
    fn derive_generic_imap_no_dav() {
        let acc = account_row("a1");
        let bindings = derive_bindings_from_account(&acc);
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].service, "mail");
        assert_eq!(bindings[0].protocol, "imap");
        let cfg: serde_json::Value = serde_json::from_str(&bindings[0].config_json).unwrap();
        assert_eq!(cfg["imap_host"], "imap.example.com");
        assert_eq!(cfg["smtp_port"], 587);
    }

    #[test]
    fn derive_generic_imap_with_caldav_yields_three_bindings() {
        let mut acc = account_row("a1");
        acc.caldav_url = "https://nextcloud.example.com/dav".into();
        let bindings = derive_bindings_from_account(&acc);
        assert_eq!(bindings.len(), 3);
        let cal = bindings.iter().find(|b| b.service == "calendar").unwrap();
        assert_eq!(cal.protocol, "caldav");
        let cal_cfg: serde_json::Value = serde_json::from_str(&cal.config_json).unwrap();
        assert_eq!(cal_cfg["url"], "https://nextcloud.example.com/dav");
        let con = bindings.iter().find(|b| b.service == "contacts").unwrap();
        assert_eq!(con.protocol, "carddav");
    }

    #[test]
    fn derive_gmail_yields_imap_plus_google() {
        let mut acc = account_row("a1");
        acc.provider = "gmail".into();
        acc.imap_host = "imap.gmail.com".into();
        acc.smtp_host = "smtp.gmail.com".into();
        let bindings = derive_bindings_from_account(&acc);
        assert_eq!(bindings.len(), 3);
        let mail = bindings.iter().find(|b| b.service == "mail").unwrap();
        assert_eq!(mail.protocol, "imap");
        assert_eq!(
            bindings.iter().find(|b| b.service == "calendar").unwrap().protocol,
            "google"
        );
        assert_eq!(
            bindings.iter().find(|b| b.service == "contacts").unwrap().protocol,
            "google"
        );
    }

    #[test]
    fn derive_o365_imap_legacy_yields_imap_plus_graph() {
        let mut acc = account_row("a1");
        acc.provider = "o365".into();
        acc.mail_protocol = "imap".into();
        let bindings = derive_bindings_from_account(&acc);
        assert_eq!(bindings.len(), 3);
        assert_eq!(
            bindings.iter().find(|b| b.service == "mail").unwrap().protocol,
            "imap"
        );
        assert_eq!(
            bindings.iter().find(|b| b.service == "calendar").unwrap().protocol,
            "graph"
        );
        assert_eq!(
            bindings.iter().find(|b| b.service == "contacts").unwrap().protocol,
            "graph"
        );
    }

    #[test]
    fn derive_o365_graph_yields_all_graph() {
        let mut acc = account_row("a1");
        acc.provider = "o365".into();
        acc.mail_protocol = "graph".into();
        let bindings = derive_bindings_from_account(&acc);
        assert_eq!(bindings.len(), 3);
        for b in &bindings {
            assert_eq!(b.protocol, "graph", "service={}", b.service);
        }
    }

    #[test]
    fn derive_jmap_basic_yields_three_jmap_bindings() {
        let mut acc = account_row("a1");
        acc.mail_protocol = "jmap".into();
        acc.jmap_url = "https://jmap.example.com/jmap".into();
        let bindings = derive_bindings_from_account(&acc);
        assert_eq!(bindings.len(), 3);
        for b in &bindings {
            assert_eq!(b.protocol, "jmap", "service={}", b.service);
        }
        let mail_cfg: serde_json::Value = serde_json::from_str(
            &bindings.iter().find(|b| b.service == "mail").unwrap().config_json,
        )
        .unwrap();
        assert_eq!(mail_cfg["url"], "https://jmap.example.com/jmap");
        assert_eq!(mail_cfg["auth_method"], "basic");
    }

    #[test]
    fn derive_jmap_oidc_carries_oidc_metadata() {
        let mut acc = account_row("a1");
        acc.mail_protocol = "jmap".into();
        acc.jmap_url = "https://jmap.example.com/jmap".into();
        acc.jmap_auth_method = "oidc".into();
        acc.oidc_token_endpoint = "https://idp.example.com/token".into();
        acc.oidc_client_id = "client-123".into();
        let bindings = derive_bindings_from_account(&acc);
        let mail = bindings.iter().find(|b| b.service == "mail").unwrap();
        let mail_cfg: serde_json::Value = serde_json::from_str(&mail.config_json).unwrap();
        assert_eq!(mail_cfg["auth_method"], "oidc");
        assert_eq!(mail_cfg["oidc_token_endpoint"], "https://idp.example.com/token");
        assert_eq!(mail_cfg["oidc_client_id"], "client-123");
    }

    #[test]
    fn empty_mail_protocol_yields_no_mail_binding() {
        // CalDAV-only Nextcloud-style account: no mail at all, just the
        // shared dav URL. derive_bindings should still produce calendar +
        // contacts but skip the mail binding entirely.
        let mut acc = account_row("a1");
        acc.mail_protocol = String::new();
        acc.imap_host = String::new();
        acc.imap_port = 0;
        acc.smtp_host = String::new();
        acc.smtp_port = 0;
        acc.caldav_url = "https://nextcloud.example.com/dav".into();
        let bindings = derive_bindings_from_account(&acc);
        assert!(bindings.iter().all(|b| b.service != "mail"));
        assert_eq!(
            bindings.iter().find(|b| b.service == "calendar").unwrap().protocol,
            "caldav"
        );
        assert_eq!(
            bindings.iter().find(|b| b.service == "contacts").unwrap().protocol,
            "carddav"
        );
    }

    #[test]
    fn mail_sync_enabled_override_disables_mail_binding() {
        // JMAP cal-only: full JMAP account, but mail_sync_enabled=false
        // marks the mail binding disabled while leaving cal/contacts on.
        let mut acc = account_row("a1");
        acc.mail_protocol = "jmap".into();
        acc.jmap_url = "https://jmap.example.com".into();
        let bindings = derive_bindings(LegacyBindingFields {
            account_id: &acc.id,
            enabled: true,
            provider: &acc.provider,
            mail_protocol: &acc.mail_protocol,
            imap_host: &acc.imap_host,
            imap_port: acc.imap_port,
            smtp_host: &acc.smtp_host,
            smtp_port: acc.smtp_port,
            use_tls: acc.use_tls,
            jmap_url: &acc.jmap_url,
            jmap_auth_method: &acc.jmap_auth_method,
            oidc_token_endpoint: &acc.oidc_token_endpoint,
            oidc_client_id: &acc.oidc_client_id,
            caldav_url: &acc.caldav_url,
            calendar_sync_enabled: acc.calendar_sync_enabled,
            mail_sync_enabled: Some(false),
            contacts_sync_enabled: None,
            mail_sync_interval_seconds: None,
            calendar_sync_interval_seconds: None,
            contacts_sync_interval_seconds: None,
        });
        let mail = bindings.iter().find(|b| b.service == "mail").unwrap();
        assert_eq!(mail.protocol, "jmap");
        assert!(
            !mail.enabled,
            "mail_sync_enabled=false should disable the mail binding"
        );
        let cal = bindings.iter().find(|b| b.service == "calendar").unwrap();
        assert!(cal.enabled);
    }

    #[test]
    fn per_binding_sync_intervals_propagate() {
        let acc = account_row("a1");
        let bindings = derive_bindings(LegacyBindingFields {
            account_id: &acc.id,
            enabled: true,
            provider: &acc.provider,
            mail_protocol: &acc.mail_protocol,
            imap_host: &acc.imap_host,
            imap_port: acc.imap_port,
            smtp_host: &acc.smtp_host,
            smtp_port: acc.smtp_port,
            use_tls: acc.use_tls,
            jmap_url: &acc.jmap_url,
            jmap_auth_method: &acc.jmap_auth_method,
            oidc_token_endpoint: &acc.oidc_token_endpoint,
            oidc_client_id: &acc.oidc_client_id,
            caldav_url: "https://example.com/dav",
            calendar_sync_enabled: true,
            mail_sync_enabled: None,
            contacts_sync_enabled: None,
            mail_sync_interval_seconds: Some(120),
            calendar_sync_interval_seconds: Some(900),
            contacts_sync_interval_seconds: Some(1800),
        });
        let mail = bindings.iter().find(|b| b.service == "mail").unwrap();
        let cal = bindings.iter().find(|b| b.service == "calendar").unwrap();
        let con = bindings.iter().find(|b| b.service == "contacts").unwrap();
        assert_eq!(mail.sync_interval_seconds, Some(120));
        assert_eq!(cal.sync_interval_seconds, Some(900));
        assert_eq!(con.sync_interval_seconds, Some(1800));
    }

    #[test]
    fn calendar_disabled_propagates_to_binding() {
        let mut acc = account_row("a1");
        acc.caldav_url = "https://example.com/dav".into();
        acc.calendar_sync_enabled = false;
        let bindings = derive_bindings_from_account(&acc);
        let cal = bindings.iter().find(|b| b.service == "calendar").unwrap();
        assert!(!cal.enabled);
        // Mail/contacts should still be enabled.
        assert!(bindings.iter().find(|b| b.service == "mail").unwrap().enabled);
    }

    #[test]
    fn crud_round_trip() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO accounts (id, display_name, email, provider, username) VALUES ('a1', 'A', 'a@e', 'generic', 'u')",
            [],
        ).unwrap();
        let b = ServiceBinding {
            id: "a1-mail".into(),
            account_id: "a1".into(),
            service: "mail".into(),
            protocol: "imap".into(),
            enabled: true,
            sync_interval_seconds: Some(120),
            config_json: r#"{"imap_host":"x"}"#.into(),
        };
        insert(&conn, &b).unwrap();
        let listed = list_for_account(&conn, "a1").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].sync_interval_seconds, Some(120));

        let mut updated = listed[0].clone();
        updated.enabled = false;
        updated.sync_interval_seconds = None;
        update(&conn, &updated).unwrap();
        let after = list_for_account(&conn, "a1").unwrap();
        assert!(!after[0].enabled);
        assert_eq!(after[0].sync_interval_seconds, None);

        delete(&conn, "a1-mail").unwrap();
        assert!(list_for_account(&conn, "a1").unwrap().is_empty());
    }

    #[test]
    fn deleting_account_cascades_to_bindings() {
        let conn = setup_db();
        // Need foreign keys ON for cascade in this in-memory connection.
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute(
            "INSERT INTO accounts (id, display_name, email, provider, username) VALUES ('a1', 'A', 'a@e', 'generic', 'u')",
            [],
        ).unwrap();
        let b = ServiceBinding {
            id: "a1-mail".into(),
            account_id: "a1".into(),
            service: "mail".into(),
            protocol: "imap".into(),
            enabled: true,
            sync_interval_seconds: None,
            config_json: "{}".into(),
        };
        insert(&conn, &b).unwrap();
        conn.execute("DELETE FROM accounts WHERE id = 'a1'", []).unwrap();
        assert!(list_for_account(&conn, "a1").unwrap().is_empty());
    }
}
