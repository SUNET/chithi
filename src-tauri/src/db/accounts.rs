use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub display_name: String,
    pub email: String,
    pub provider: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub display_name: String,
    pub email: String,
    pub provider: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub username: String,
    pub password: String,
    pub use_tls: bool,
}

#[derive(Debug, Clone)]
pub struct AccountFull {
    pub id: String,
    pub display_name: String,
    pub email: String,
    pub provider: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub username: String,
    pub password: String,
    pub use_tls: bool,
    pub enabled: bool,
}

pub fn list_accounts(conn: &Connection) -> Result<Vec<Account>> {
    let mut stmt = conn.prepare(
        "SELECT id, display_name, email, provider, enabled FROM accounts ORDER BY display_name",
    )?;
    let accounts = stmt
        .query_map([], |row| {
            Ok(Account {
                id: row.get(0)?,
                display_name: row.get(1)?,
                email: row.get(2)?,
                provider: row.get(3)?,
                enabled: row.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(accounts)
}

pub fn get_account_full(conn: &Connection, id: &str) -> Result<AccountFull> {
    let account = conn.query_row(
        "SELECT id, display_name, email, provider, imap_host, imap_port, smtp_host, smtp_port, username, password, use_tls, enabled FROM accounts WHERE id = ?1",
        params![id],
        |row| {
            Ok(AccountFull {
                id: row.get(0)?,
                display_name: row.get(1)?,
                email: row.get(2)?,
                provider: row.get(3)?,
                imap_host: row.get(4)?,
                imap_port: row.get::<_, u32>(5)? as u16,
                smtp_host: row.get(6)?,
                smtp_port: row.get::<_, u32>(7)? as u16,
                username: row.get(8)?,
                password: row.get(9)?,
                use_tls: row.get(10)?,
                enabled: row.get(11)?,
            })
        },
    ).map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => crate::error::Error::AccountNotFound(id.to_string()),
        other => crate::error::Error::Database(other),
    })?;
    Ok(account)
}

pub fn insert_account(conn: &Connection, id: &str, config: &AccountConfig) -> Result<()> {
    conn.execute(
        "INSERT INTO accounts (id, display_name, email, provider, mail_protocol, imap_host, imap_port, smtp_host, smtp_port, username, password, use_tls)
         VALUES (?1, ?2, ?3, ?4, 'imap', ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            id,
            config.display_name,
            config.email,
            config.provider,
            config.imap_host,
            config.imap_port,
            config.smtp_host,
            config.smtp_port,
            config.username,
            config.password,
            config.use_tls,
        ],
    )?;
    Ok(())
}

pub fn delete_account(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
    Ok(())
}
