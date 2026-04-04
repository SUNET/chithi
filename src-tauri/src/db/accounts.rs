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
    pub mail_protocol: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub jmap_url: String,
    pub username: String,
    pub password: String,
    pub use_tls: bool,
    pub enabled: bool,
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
    let account = conn.query_row(
        "SELECT id, display_name, email, provider, mail_protocol, imap_host, imap_port, smtp_host, smtp_port, jmap_url, username, password, use_tls, enabled FROM accounts WHERE id = ?1",
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
                username: row.get(10)?,
                password: row.get(11)?,
                use_tls: row.get(12)?,
                enabled: row.get(13)?,
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
        "INSERT INTO accounts (id, display_name, email, provider, mail_protocol, imap_host, imap_port, smtp_host, smtp_port, jmap_url, username, password, use_tls)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
            config.username,
            config.password,
            config.use_tls,
        ],
    )?;
    Ok(())
}

pub fn update_account(conn: &Connection, id: &str, config: &AccountConfig) -> Result<()> {
    let rows = conn.execute(
        "UPDATE accounts SET display_name=?1, email=?2, provider=?3, mail_protocol=?4,
         imap_host=?5, imap_port=?6, smtp_host=?7, smtp_port=?8, jmap_url=?9,
         username=?10, password=?11, use_tls=?12, updated_at=CURRENT_TIMESTAMP
         WHERE id=?13",
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
            config.username,
            config.password,
            config.use_tls,
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
    conn.execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
    Ok(())
}
