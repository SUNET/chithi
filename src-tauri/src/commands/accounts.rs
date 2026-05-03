use serde::{Deserialize, Serialize};
use tauri::State;

use crate::commands::calendar::random_calendar_color;
use crate::db;
use crate::db::calendar::NewCalendar;
use crate::error::Result;
use crate::state::AppState;

/// Combined autoconfig result returned to the Settings UI when the user
/// clicks "Auto-discover". Carries both the IMAP/SMTP server settings
/// (Thunderbird-style autoconfig + MX fallback) and the CalDAV/CardDAV
/// URLs from `.well-known` probing. Empty strings on any field mean
/// "not found" — the UI keeps whatever was already in the form. The
/// `source` string is purely informational ("isp-db", "domain-autoconfig",
/// "well-known", "mx", or "" if no autoconfig source matched).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AutoconfigResult {
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_use_tls: bool,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_use_tls: bool,
    pub caldav_url: String,
    pub carddav_url: String,
    pub source: String,
}

/// Run Thunderbird-style email autoconfig (Mozilla ISP DB / provider
/// autoconfig / `.well-known` / MX fallback) followed by CalDAV /
/// CardDAV `.well-known` probing across every candidate hostname we
/// know about (#43). Returns an `AutoconfigResult` with whichever
/// fields could be discovered; failures degrade to empty strings so
/// the UI can still save the account without auto-fill.
#[tauri::command]
pub async fn probe_dav_endpoints(
    email: String,
    username: String,
    password: String,
    imap_host: Option<String>,
    smtp_host: Option<String>,
) -> Result<AutoconfigResult> {
    log::info!(
        "probe_dav_endpoints: email={} imap_host={:?} smtp_host={:?}",
        email,
        imap_host,
        smtp_host
    );

    // 1. Mail-server autoconfig. Soft-fails to None.
    let (servers, source) = match crate::mail::autoconfig::discover(&email).await {
        Ok(Some((s, src))) => (Some(s), src.to_string()),
        Ok(None) => (None, String::new()),
        Err(e) => {
            log::debug!("autoconfig: discover errored: {}", e);
            (None, String::new())
        }
    };

    // 2. Build the DAV-probe hostname list. Order matters: we try the
    // email domain first (cheapest, standards-compliant setups), then
    // mail.<domain>, then any host autoconfig surfaced, then the user's
    // entered IMAP / SMTP hosts. Dedup along the way.
    let domain = email.rsplit('@').next().unwrap_or("").to_string();
    let mut hosts: Vec<String> = Vec::new();
    let mut push = |h: String| {
        if !h.is_empty() && !hosts.contains(&h) {
            hosts.push(h);
        }
    };
    if !domain.is_empty() {
        push(domain.clone());
        push(format!("mail.{}", domain));
    }
    if let Some(s) = &servers {
        push(s.imap_host.clone());
        push(s.smtp_host.clone());
    }
    if let Some(h) = imap_host {
        push(h);
    }
    if let Some(h) = smtp_host {
        push(h);
    }

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| crate::error::Error::Other(format!("http client: {}", e)))?;
    let auth = crate::mail::caldav::DavAuth::Basic { username, password };

    let caldav_url =
        match crate::mail::caldav::CalDavClient::auto_discover_hosts(&http, &auth, &hosts).await {
            Ok(url) => {
                log::info!("probe_dav_endpoints: caldav={}", url);
                url
            }
            Err(e) => {
                log::debug!("probe_dav_endpoints: caldav probe failed: {}", e);
                String::new()
            }
        };
    let carddav_url = match crate::mail::carddav::auto_discover_hosts(&http, &auth, &hosts).await {
        Ok(url) => {
            log::info!("probe_dav_endpoints: carddav={}", url);
            url
        }
        Err(e) => {
            log::debug!("probe_dav_endpoints: carddav probe failed: {}", e);
            String::new()
        }
    };

    let s = servers.unwrap_or_default();
    Ok(AutoconfigResult {
        imap_host: s.imap_host,
        imap_port: s.imap_port,
        imap_use_tls: s.imap_use_tls,
        smtp_host: s.smtp_host,
        smtp_port: s.smtp_port,
        smtp_use_tls: s.smtp_use_tls,
        caldav_url,
        carddav_url,
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
        if let Ok(Some(tokens)) = crate::oauth::load_tokens(temp_id) {
            crate::oauth::store_tokens(&id, &tokens)?;
            crate::oauth::delete_tokens(temp_id).ok();
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
    })
}

#[tauri::command]
pub async fn update_account(
    state: State<'_, AppState>,
    account_id: String,
    config: db::accounts::AccountConfig,
) -> Result<()> {
    log::info!("Updating account {} ({})", account_id, config.email);
    let conn = state.db.writer().await;
    db::accounts::update_account(&conn, &account_id, &config)?;
    Ok(())
}

#[tauri::command]
pub async fn delete_account(state: State<'_, AppState>, account_id: String) -> Result<()> {
    log::info!("Deleting account {}", account_id);
    let conn = state.db.writer().await;
    db::accounts::delete_account(&conn, &account_id)?;
    // Also remove Maildir
    let maildir_path = state.data_dir.join(&account_id);
    if maildir_path.exists() {
        log::info!("Removing maildir at {}", maildir_path.display());
        std::fs::remove_dir_all(&maildir_path).ok();
    }
    log::info!("Account {} deleted", account_id);
    Ok(())
}
