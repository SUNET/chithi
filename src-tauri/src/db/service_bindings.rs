use rusqlite::{params, Connection, OptionalExtension};
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
    /// Mirror of `AccountFull::jmap_auth_method` so the
    /// "basic" / "bearer" / "oidc" choice round-trips. Older bindings
    /// written before bearer support land here as empty strings; the
    /// reader treats empty as "basic" for back-compat.
    #[serde(default, rename = "auth_method")]
    pub auth_method: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DavBindingConfig {
    #[serde(default)]
    pub url: String,
}

/// Config payload for a `meet` binding (#148). Talk and Matrix both
/// store just a base URL here — the credential lives in the keyring
/// under the account id, not in this config_json.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MeetBindingConfig {
    #[serde(default)]
    pub url: String,
    /// Stable Zoom principal identifiers. Empty for non-Zoom and bindings
    /// created before identity-bound reauthentication support.
    #[serde(default)]
    pub zoom_user_id: String,
    #[serde(default)]
    pub zoom_account_id: String,
    /// Stable user UUID from La Suite Meet's add-on JWT. Empty for non-Visio
    /// and legacy bindings created before identity-bound reauthentication.
    #[serde(default)]
    pub visio_user_id: String,
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

    pub fn meet_config(&self) -> Result<MeetBindingConfig> {
        serde_json::from_str(&self.config_json).map_err(|e| {
            Error::Other(format!(
                "binding {} has malformed meet config_json: {}",
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
    /// #148: meet binding inputs. Both empty means no meet
    /// binding is emitted. `meet_protocol` carries the
    /// provider's discriminator (`talk`, `matrix`, `zoom`, …) —
    /// the actual list is whatever providers `meet::registry()`
    /// exposes, not enumerated here so the comment doesn't go
    /// stale on every new provider.
    pub meet_url: &'a str,
    pub meet_protocol: &'a str,
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

    // Meet binding (#148). Emitted only when both fields are set —
    // an account that has neither (the common case) gets no meet
    // binding, and an inconsistent half-set state is caught by the
    // explicit guard rather than silently emitting a half-broken
    // entry. The protocol value comes from `meet::registry()`
    // and isn't enumerated here so this comment doesn't drift on
    // every new provider.
    if !f.meet_url.is_empty() && !f.meet_protocol.is_empty() {
        out.push(ServiceBinding {
            id: format!("{aid}-meet"),
            account_id: aid.into(),
            service: "meet".into(),
            protocol: f.meet_protocol.into(),
            enabled: f.enabled,
            sync_interval_seconds: None,
            config_json: serde_json::json!({ "url": f.meet_url }).to_string(),
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
        meet_url: &account.meet_url,
        meet_protocol: &account.meet_protocol,
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
///
/// User-set and provider-owned fields stored in `config_json` that the legacy
/// derivation doesn't know about (contact-book defaults and meet identities) are
/// snapshotted before the wipe and re-applied after the re-insert so a
/// trivial account-edit doesn't drop them.
pub fn rebuild_for_account(
    conn: &Connection,
    account_id: &str,
    fields: LegacyBindingFields<'_>,
) -> Result<()> {
    let preserved = snapshot_user_set_config(conn, account_id)?;
    let zoom_identity = snapshot_zoom_identity(conn, account_id)?;
    let visio_identity = snapshot_visio_identity(conn, account_id)?;
    delete_for_account(conn, account_id)?;
    for binding in derive_bindings(fields) {
        insert(conn, &binding)?;
    }
    for (service, book_id) in preserved {
        // Tolerate the binding having disappeared from the new
        // derivation: e.g. switching a mail-having account to a
        // standalone CalDAV / CardDAV shape drops the mail binding,
        // and the preserved mail default has nowhere to land.
        // Failing the whole rebuild here would block the account
        // edit; logging and moving on lets the edit succeed and
        // simply forgets the orphan default.
        if let Err(e) = set_default_contact_book(conn, account_id, &service, Some(&book_id)) {
            log::debug!(
                "rebuild_for_account: drop preserved default for missing {} binding ({}): {}",
                service,
                account_id,
                e
            );
        }
    }
    if let Some((user_id, zoom_account_id)) = zoom_identity {
        if let Err(error) = set_zoom_identity(conn, account_id, &user_id, &zoom_account_id) {
            log::debug!(
                "rebuild_for_account: drop preserved Zoom identity for account {}: {}",
                account_id,
                error
            );
        }
    }
    if let Some(user_id) = visio_identity {
        if let Err(error) = set_visio_identity(conn, account_id, &user_id) {
            log::debug!(
                "rebuild_for_account: drop preserved Visio identity for account {}: {}",
                account_id,
                error
            );
        }
    }
    apply_default_contact_book_if_missing(conn, account_id)?;
    Ok(())
}

pub fn set_zoom_identity(
    conn: &Connection,
    account_id: &str,
    user_id: &str,
    zoom_account_id: &str,
) -> Result<()> {
    if user_id.trim().is_empty() || zoom_account_id.trim().is_empty() {
        return Err(Error::Other("Zoom identity must not be empty".into()));
    }
    let config: Option<String> = conn
        .query_row(
            "SELECT config_json FROM service_bindings
             WHERE account_id = ?1 AND service = 'meet' AND protocol = 'zoom'",
            params![account_id],
            |row| row.get(0),
        )
        .optional()?;
    let config = config.ok_or_else(|| {
        Error::Other(format!(
            "no Zoom meet binding for account {} — cannot store identity",
            account_id
        ))
    })?;
    let mut value: serde_json::Value = serde_json::from_str(&config)
        .map_err(|error| Error::Other(format!("malformed Zoom binding config: {error}")))?;
    let object = value.as_object_mut().ok_or_else(|| {
        Error::Other(format!(
            "Zoom binding for account {} has non-object config_json",
            account_id
        ))
    })?;
    object.insert("zoom_user_id".into(), user_id.into());
    object.insert("zoom_account_id".into(), zoom_account_id.into());
    let serialized = serde_json::to_string(&value)
        .map_err(|error| Error::Other(format!("serialize Zoom binding config: {error}")))?;
    conn.execute(
        "UPDATE service_bindings SET config_json = ?1, updated_at = CURRENT_TIMESTAMP
         WHERE account_id = ?2 AND service = 'meet' AND protocol = 'zoom'",
        params![serialized, account_id],
    )?;
    Ok(())
}

fn snapshot_zoom_identity(conn: &Connection, account_id: &str) -> Result<Option<(String, String)>> {
    let config: Option<String> = conn
        .query_row(
            "SELECT config_json FROM service_bindings
             WHERE account_id = ?1 AND service = 'meet' AND protocol = 'zoom'",
            params![account_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(config) = config else {
        return Ok(None);
    };
    let config: MeetBindingConfig = serde_json::from_str(&config)
        .map_err(|error| Error::Other(format!("malformed Zoom binding config: {error}")))?;
    if config.zoom_user_id.is_empty() || config.zoom_account_id.is_empty() {
        return Ok(None);
    }
    Ok(Some((config.zoom_user_id, config.zoom_account_id)))
}

pub fn set_visio_identity(conn: &Connection, account_id: &str, user_id: &str) -> Result<()> {
    if user_id.trim().is_empty() {
        return Err(Error::Other("Visio identity must not be empty".into()));
    }
    let config: Option<String> = conn
        .query_row(
            "SELECT config_json FROM service_bindings
             WHERE account_id = ?1 AND service = 'meet' AND protocol = 'visio'",
            params![account_id],
            |row| row.get(0),
        )
        .optional()?;
    let config = config.ok_or_else(|| {
        Error::Other(format!(
            "no Visio meet binding for account {} — cannot store identity",
            account_id
        ))
    })?;
    let mut value: serde_json::Value = serde_json::from_str(&config)
        .map_err(|error| Error::Other(format!("malformed Visio binding config: {error}")))?;
    let object = value.as_object_mut().ok_or_else(|| {
        Error::Other(format!(
            "Visio binding for account {} has non-object config_json",
            account_id
        ))
    })?;
    object.insert("visio_user_id".into(), user_id.into());
    let serialized = serde_json::to_string(&value)
        .map_err(|error| Error::Other(format!("serialize Visio binding config: {error}")))?;
    conn.execute(
        "UPDATE service_bindings SET config_json = ?1, updated_at = CURRENT_TIMESTAMP
         WHERE account_id = ?2 AND service = 'meet' AND protocol = 'visio'",
        params![serialized, account_id],
    )?;
    Ok(())
}

fn snapshot_visio_identity(conn: &Connection, account_id: &str) -> Result<Option<String>> {
    let config: Option<String> = conn
        .query_row(
            "SELECT config_json FROM service_bindings
             WHERE account_id = ?1 AND service = 'meet' AND protocol = 'visio'",
            params![account_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(config) = config else {
        return Ok(None);
    };
    let config: MeetBindingConfig = serde_json::from_str(&config)
        .map_err(|error| Error::Other(format!("malformed Visio binding config: {error}")))?;
    if config.visio_user_id.is_empty() {
        return Ok(None);
    }
    Ok(Some(config.visio_user_id))
}

/// Field name in `config_json` that holds the default contact book
/// id for a mail or calendar binding. Stored under the binding (not
/// the account) so the same identity can route compose autocomplete
/// to one book and event-attendee autocomplete to a different one.
const DEFAULT_CONTACT_BOOK_KEY: &str = "default_contact_book_id";

/// Services that can carry a `default_contact_book_id`. The contacts
/// binding *is* the source of contact books, so a default-book field
/// on it is meaningless; we reject it explicitly rather than allow a
/// silent no-op write to its config_json.
fn is_valid_default_book_service(service: &str) -> bool {
    matches!(service, "mail" | "calendar")
}

/// Read the `default_contact_book_id` from a binding's `config_json`.
/// Returns `Ok(None)` if there is no binding for the (account, service)
/// pair, the field is absent, or the field is null/empty. Only valid
/// for `service` values where a default makes sense — `"mail"` or
/// `"calendar"`. Other services (notably `"contacts"`) return
/// `Ok(None)` without touching the DB.
pub fn get_default_contact_book(
    conn: &Connection,
    account_id: &str,
    service: &str,
) -> Result<Option<String>> {
    if !is_valid_default_book_service(service) {
        return Ok(None);
    }
    let cfg: Option<String> = conn
        .query_row(
            "SELECT config_json FROM service_bindings
             WHERE account_id = ?1 AND service = ?2",
            params![account_id, service],
            |row| row.get(0),
        )
        .ok();
    let Some(cfg) = cfg else {
        return Ok(None);
    };
    let v: serde_json::Value = serde_json::from_str(&cfg).unwrap_or(serde_json::json!({}));
    Ok(v.get(DEFAULT_CONTACT_BOOK_KEY)
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string()))
}

/// Write (or clear) the `default_contact_book_id` on a binding's
/// `config_json`. Pass `book_id = None` to remove the field. Returns
/// `Err` if no binding exists for the (account, service) pair, or
/// if `service` is not one of `"mail"` / `"calendar"` — callers
/// should only set defaults for services they know exist.
pub fn set_default_contact_book(
    conn: &Connection,
    account_id: &str,
    service: &str,
    book_id: Option<&str>,
) -> Result<()> {
    if !is_valid_default_book_service(service) {
        return Err(Error::Other(format!(
            "service {:?} cannot carry a default contact book",
            service
        )));
    }
    let cfg: Option<String> = conn
        .query_row(
            "SELECT config_json FROM service_bindings
             WHERE account_id = ?1 AND service = ?2",
            params![account_id, service],
            |row| row.get(0),
        )
        .ok();
    let cfg = cfg.ok_or_else(|| {
        Error::Other(format!(
            "no {} binding for account {} — cannot set default contact book",
            service, account_id
        ))
    })?;
    let mut v: serde_json::Value = serde_json::from_str(&cfg).unwrap_or(serde_json::json!({}));
    let obj = v.as_object_mut().ok_or_else(|| {
        Error::Other(format!(
            "binding for account {} service {} has non-object config_json",
            account_id, service
        ))
    })?;
    match book_id {
        Some(id) => {
            obj.insert(
                DEFAULT_CONTACT_BOOK_KEY.into(),
                serde_json::Value::String(id.to_string()),
            );
        }
        None => {
            obj.remove(DEFAULT_CONTACT_BOOK_KEY);
        }
    }
    let new_cfg = serde_json::to_string(&v)
        .map_err(|e| Error::Other(format!("re-serialize config_json: {}", e)))?;
    let rows = conn.execute(
        "UPDATE service_bindings
         SET config_json = ?1, updated_at = CURRENT_TIMESTAMP
         WHERE account_id = ?2 AND service = ?3",
        params![new_cfg, account_id, service],
    )?;
    if rows == 0 {
        return Err(Error::Other(format!(
            "no {} binding for account {} (race?)",
            service, account_id
        )));
    }
    Ok(())
}

/// For each mail/calendar binding on `account_id` that has no
/// `default_contact_book_id`, pick one from the same account's
/// contact books and store it. Idempotent: bindings that already
/// have a default are left alone, so the user's manual pick from
/// the settings UI sticks. Safe to call from any path that might
/// have just created bindings or synced books — currently
/// `rebuild_for_account` and the end of `sync_contacts`.
///
/// "Same account" is intentional: when the user wires a different
/// account's book as the default, they do that explicitly through
/// the settings dropdown and we never touch it here.
pub fn apply_default_contact_book_if_missing(conn: &Connection, account_id: &str) -> Result<()> {
    let Some(primary) = pick_primary_contact_book(conn, account_id)? else {
        return Ok(());
    };
    for service in ["mail", "calendar"] {
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM service_bindings WHERE account_id = ?1 AND service = ?2",
                params![account_id, service],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !exists {
            continue;
        }
        if get_default_contact_book(conn, account_id, service)?.is_some() {
            continue;
        }
        set_default_contact_book(conn, account_id, service, Some(&primary))?;
    }
    Ok(())
}

/// Pick the most natural "default" contact book for an account.
/// Synced books beat the local "Collected Contacts" silo because
/// the latter is auto-populated from outgoing mail and isn't where
/// the user keeps curated entries; alphabetical order picks the
/// same book on every machine when multiple synced books exist.
fn pick_primary_contact_book(conn: &Connection, account_id: &str) -> Result<Option<String>> {
    let synced: Option<String> = conn
        .query_row(
            "SELECT id FROM contact_books
             WHERE account_id = ?1 AND sync_type != 'local'
             ORDER BY name LIMIT 1",
            params![account_id],
            |row| row.get(0),
        )
        .ok();
    if synced.is_some() {
        return Ok(synced);
    }
    let any: Option<String> = conn
        .query_row(
            "SELECT id FROM contact_books WHERE account_id = ?1 ORDER BY name LIMIT 1",
            params![account_id],
            |row| row.get(0),
        )
        .ok();
    Ok(any)
}

/// Snapshot of `(service, default_contact_book_id)` pairs for an
/// account. Used by `rebuild_for_account` to carry user-set defaults
/// across the wipe-and-rederive cycle.
fn snapshot_user_set_config(conn: &Connection, account_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT service, config_json FROM service_bindings
         WHERE account_id = ?1",
    )?;
    let rows = stmt
        .query_map(params![account_id], |row| {
            let service: String = row.get(0)?;
            let cfg: String = row.get(1)?;
            Ok((service, cfg))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut out = Vec::new();
    for (service, cfg) in rows {
        let v: serde_json::Value = serde_json::from_str(&cfg).unwrap_or(serde_json::json!({}));
        if let Some(id) = v
            .get(DEFAULT_CONTACT_BOOK_KEY)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
        {
            out.push((service, id.to_string()));
        }
    }
    Ok(out)
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
            meet_url: String::new(),
            meet_protocol: String::new(),
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
            pgp_attach_pubkey_on_sign: true,
            pgp_autocrypt_header: true,
            pgp_encrypt_subject: true,
            pgp_encrypt_drafts: true,
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
            bindings
                .iter()
                .find(|b| b.service == "calendar")
                .unwrap()
                .protocol,
            "google"
        );
        assert_eq!(
            bindings
                .iter()
                .find(|b| b.service == "contacts")
                .unwrap()
                .protocol,
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
            bindings
                .iter()
                .find(|b| b.service == "mail")
                .unwrap()
                .protocol,
            "imap"
        );
        assert_eq!(
            bindings
                .iter()
                .find(|b| b.service == "calendar")
                .unwrap()
                .protocol,
            "graph"
        );
        assert_eq!(
            bindings
                .iter()
                .find(|b| b.service == "contacts")
                .unwrap()
                .protocol,
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
            &bindings
                .iter()
                .find(|b| b.service == "mail")
                .unwrap()
                .config_json,
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
        assert_eq!(
            mail_cfg["oidc_token_endpoint"],
            "https://idp.example.com/token"
        );
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
            bindings
                .iter()
                .find(|b| b.service == "calendar")
                .unwrap()
                .protocol,
            "caldav"
        );
        assert_eq!(
            bindings
                .iter()
                .find(|b| b.service == "contacts")
                .unwrap()
                .protocol,
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
            meet_url: &acc.meet_url,
            meet_protocol: &acc.meet_protocol,
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
            meet_url: "",
            meet_protocol: "",
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
        assert!(
            bindings
                .iter()
                .find(|b| b.service == "mail")
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn insert_and_list_round_trip() {
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
        conn.execute("DELETE FROM accounts WHERE id = 'a1'", [])
            .unwrap();
        assert!(list_for_account(&conn, "a1").unwrap().is_empty());
    }

    fn setup_db_with_books() -> Connection {
        let conn = setup_db();
        conn.execute_batch(
            "CREATE TABLE contact_books (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                name TEXT NOT NULL,
                remote_id TEXT,
                sync_type TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );",
        )
        .unwrap();
        conn
    }

    fn insert_account(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO accounts (id, email) VALUES (?1, ?1)",
            params![id],
        )
        .unwrap();
    }

    fn insert_binding(conn: &Connection, account_id: &str, service: &str, protocol: &str) {
        let id = format!("{account_id}-{service}");
        let b = ServiceBinding {
            id,
            account_id: account_id.into(),
            service: service.into(),
            protocol: protocol.into(),
            enabled: true,
            sync_interval_seconds: None,
            config_json: "{}".into(),
        };
        insert(conn, &b).unwrap();
    }

    fn insert_book(conn: &Connection, id: &str, account_id: &str, name: &str, sync_type: &str) {
        conn.execute(
            "INSERT INTO contact_books (id, account_id, name, sync_type) VALUES (?1, ?2, ?3, ?4)",
            params![id, account_id, name, sync_type],
        )
        .unwrap();
    }

    #[test]
    fn default_book_round_trip() {
        let conn = setup_db_with_books();
        insert_account(&conn, "a1");
        insert_binding(&conn, "a1", "mail", "imap");
        // Round-trip: write, read, clear.
        assert!(get_default_contact_book(&conn, "a1", "mail")
            .unwrap()
            .is_none());
        set_default_contact_book(&conn, "a1", "mail", Some("book-1")).unwrap();
        assert_eq!(
            get_default_contact_book(&conn, "a1", "mail").unwrap(),
            Some("book-1".into())
        );
        set_default_contact_book(&conn, "a1", "mail", None).unwrap();
        assert!(get_default_contact_book(&conn, "a1", "mail")
            .unwrap()
            .is_none());
    }

    #[test]
    fn default_book_set_errors_when_no_binding() {
        let conn = setup_db_with_books();
        insert_account(&conn, "a1");
        // No binding inserted — set must fail rather than silently no-op.
        let res = set_default_contact_book(&conn, "a1", "mail", Some("book-1"));
        assert!(res.is_err());
    }

    #[test]
    fn default_book_preserves_other_config_fields() {
        // Writing the default must not trample existing keys (imap host etc).
        let conn = setup_db_with_books();
        insert_account(&conn, "a1");
        let b = ServiceBinding {
            id: "a1-mail".into(),
            account_id: "a1".into(),
            service: "mail".into(),
            protocol: "imap".into(),
            enabled: true,
            sync_interval_seconds: None,
            config_json: r#"{"imap_host":"imap.example.com","imap_port":993}"#.into(),
        };
        insert(&conn, &b).unwrap();

        set_default_contact_book(&conn, "a1", "mail", Some("book-1")).unwrap();

        let after = list_for_account(&conn, "a1").unwrap();
        let cfg: serde_json::Value = serde_json::from_str(&after[0].config_json).unwrap();
        assert_eq!(cfg["imap_host"], "imap.example.com");
        assert_eq!(cfg["imap_port"], 993);
        assert_eq!(cfg["default_contact_book_id"], "book-1");
    }

    #[test]
    fn apply_default_picks_synced_book_over_collected() {
        // Mail+contacts bindings exist, account has both a "Collected
        // Contacts" local book and a synced book. Auto-default should
        // pick the synced one.
        let conn = setup_db_with_books();
        insert_account(&conn, "a1");
        insert_binding(&conn, "a1", "mail", "imap");
        insert_binding(&conn, "a1", "contacts", "carddav");
        insert_book(&conn, "collected", "a1", "Collected Contacts", "local");
        insert_book(&conn, "personal", "a1", "Personal", "carddav");

        apply_default_contact_book_if_missing(&conn, "a1").unwrap();

        assert_eq!(
            get_default_contact_book(&conn, "a1", "mail").unwrap(),
            Some("personal".into())
        );
    }

    #[test]
    fn apply_default_does_not_overwrite_user_pick() {
        // If a default is already set, idempotent reapply must leave it.
        let conn = setup_db_with_books();
        insert_account(&conn, "a1");
        insert_binding(&conn, "a1", "mail", "imap");
        insert_binding(&conn, "a1", "contacts", "carddav");
        insert_book(&conn, "personal", "a1", "Personal", "carddav");
        insert_book(&conn, "work", "a1", "Work", "carddav");

        // Pretend the user picked "work" through the settings UI.
        set_default_contact_book(&conn, "a1", "mail", Some("work")).unwrap();

        apply_default_contact_book_if_missing(&conn, "a1").unwrap();

        assert_eq!(
            get_default_contact_book(&conn, "a1", "mail").unwrap(),
            Some("work".into())
        );
    }

    #[test]
    fn apply_default_skips_when_no_books_exist_yet() {
        let conn = setup_db_with_books();
        insert_account(&conn, "a1");
        insert_binding(&conn, "a1", "mail", "imap");

        apply_default_contact_book_if_missing(&conn, "a1").unwrap();
        // No books → nothing to set, no error.
        assert!(get_default_contact_book(&conn, "a1", "mail")
            .unwrap()
            .is_none());
    }
}
