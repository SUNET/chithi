use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

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
        "SELECT id, display_name, email, provider, mail_protocol, imap_host, imap_port, smtp_host, smtp_port, jmap_url, caldav_url, username, use_tls, enabled, signature FROM accounts WHERE id = ?1",
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
            })
        },
    ).map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => crate::error::Error::AccountNotFound(id.to_string()),
        other => crate::error::Error::Database(other),
    })?;

    // Fetch password from the system keyring
    match crate::keyring::get_password(&account.id) {
        Ok(pw) => account.password = pw,
        Err(e) => {
            log::warn!("Could not read password from keyring for account {}: {}", account.id, e);
        }
    }

    Ok(account)
}

pub fn insert_account(conn: &Connection, id: &str, config: &AccountConfig) -> Result<()> {
    // Store password in system keyring
    crate::keyring::set_password(id, &config.password)?;

    conn.execute(
        "INSERT INTO accounts (id, display_name, email, provider, mail_protocol, imap_host, imap_port, smtp_host, smtp_port, jmap_url, caldav_url, username, use_tls, signature)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
        ],
    )?;
    Ok(())
}

pub fn update_account(conn: &Connection, id: &str, config: &AccountConfig) -> Result<()> {
    // Only update keyring if a new password was provided (non-empty).
    // Empty means "keep existing" — the frontend never receives the stored password.
    if !config.password.is_empty() {
        crate::keyring::set_password(id, &config.password)?;
    }

    let rows = conn.execute(
        "UPDATE accounts SET display_name=?1, email=?2, provider=?3, mail_protocol=?4,
         imap_host=?5, imap_port=?6, smtp_host=?7, smtp_port=?8, jmap_url=?9,
         caldav_url=?10, username=?11, use_tls=?12, signature=?13, updated_at=CURRENT_TIMESTAMP
         WHERE id=?14",
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
            id,
        ],
    )?;
    if rows == 0 {
        return Err(crate::error::Error::AccountNotFound(id.to_string()));
    }
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
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
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
