use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::db::accounts::AccountFull;
use crate::error::Result;

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

/// Derive the set of bindings implied by an existing `accounts` row.
///
/// Maps the provider + mail_protocol + caldav_url combinations that today's
/// dispatch code reads from `AccountFull` directly into one binding per
/// service. Used by the one-time `service_bindings_initial_populate`
/// migration; fresh accounts created after Phase 3 will write bindings
/// directly without going through this function.
pub fn derive_bindings_from_account(account: &AccountFull) -> Vec<ServiceBinding> {
    let mut out = Vec::new();
    let aid = &account.id;

    // --- Mail binding (one per account; matches mail_protocol exactly) ---
    let mail_protocol = account.mail_protocol.as_str();
    let mail_config = match mail_protocol {
        "imap" => serde_json::json!({
            "imap_host": account.imap_host,
            "imap_port": account.imap_port,
            "smtp_host": account.smtp_host,
            "smtp_port": account.smtp_port,
            "use_tls": account.use_tls,
        }),
        "jmap" => serde_json::json!({
            "url": account.jmap_url,
            "auth_method": account.jmap_auth_method,
            "oidc_token_endpoint": account.oidc_token_endpoint,
            "oidc_client_id": account.oidc_client_id,
        }),
        "graph" => serde_json::json!({}),
        _ => serde_json::json!({}),
    };
    out.push(ServiceBinding {
        id: format!("{aid}-mail"),
        account_id: aid.clone(),
        service: "mail".into(),
        protocol: mail_protocol.into(),
        enabled: account.enabled,
        sync_interval_seconds: None,
        config_json: mail_config.to_string(),
    });

    // --- Calendar binding ---
    // Mirrors the dispatch in commands/calendar.rs:sync_calendars.
    let cal: Option<(&str, serde_json::Value)> = if account.mail_protocol == "jmap" {
        Some(("jmap", serde_json::json!({})))
    } else if account.provider == "gmail" {
        Some(("google", serde_json::json!({})))
    } else if account.provider == "o365" {
        Some(("graph", serde_json::json!({})))
    } else if !account.caldav_url.is_empty() {
        Some(("caldav", serde_json::json!({ "url": account.caldav_url })))
    } else {
        None
    };
    if let Some((proto, cfg)) = cal {
        out.push(ServiceBinding {
            id: format!("{aid}-calendar"),
            account_id: aid.clone(),
            service: "calendar".into(),
            protocol: proto.into(),
            enabled: account.calendar_sync_enabled,
            sync_interval_seconds: None,
            config_json: cfg.to_string(),
        });
    }

    // --- Contacts binding ---
    // Mirrors the dispatch in commands/contacts.rs:sync_contacts.
    let contacts: Option<(&str, serde_json::Value)> = if account.mail_protocol == "jmap" {
        Some(("jmap", serde_json::json!({})))
    } else if account.provider == "gmail" {
        Some(("google", serde_json::json!({})))
    } else if account.provider == "o365" {
        // Graph contacts via Microsoft, even for legacy IMAP-mail O365 rows.
        Some(("graph", serde_json::json!({})))
    } else if !account.caldav_url.is_empty() {
        // Generic IMAP with a configured DAV URL: the same host serves CardDAV
        // (ADR 0022). Store the URL on the binding so future edits don't
        // reach back into the legacy column.
        Some(("carddav", serde_json::json!({ "url": account.caldav_url })))
    } else {
        None
    };
    if let Some((proto, cfg)) = contacts {
        out.push(ServiceBinding {
            id: format!("{aid}-contacts"),
            account_id: aid.clone(),
            service: "contacts".into(),
            protocol: proto.into(),
            enabled: true,
            sync_interval_seconds: None,
            config_json: cfg.to_string(),
        });
    }

    out
}

/// Re-derive the bindings for a single account from the legacy account
/// columns. Wipes existing rows for `account_id` and inserts the freshly
/// derived set. Called from `insert_account` / `update_account` so the
/// bindings table tracks settings edits during the parallel-population
/// phase, and from the one-time initial-populate migration.
pub fn rebuild_for_account(conn: &Connection, account_id: &str) -> Result<()> {
    // Pull just the columns derive_bindings_from_account needs. Avoid
    // get_account_full because it touches the OS keyring, which we don't
    // want during write paths that are already inside a DB transaction.
    let account = conn.query_row(
        "SELECT id, display_name, email, provider, mail_protocol, imap_host, imap_port,
                smtp_host, smtp_port, jmap_url, caldav_url, username, use_tls,
                enabled, signature, jmap_auth_method, oidc_token_endpoint, oidc_client_id,
                calendar_sync_enabled
         FROM accounts WHERE id = ?1",
        params![account_id],
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
                auth_method: String::new(),
                bindings: Vec::new(),
            })
        },
    )?;

    delete_for_account(conn, account_id)?;
    for binding in derive_bindings_from_account(&account) {
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
