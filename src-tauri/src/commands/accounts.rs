use serde::{Deserialize, Serialize};
use tauri::State;

use crate::commands::calendar::random_calendar_color;
use crate::db;
use crate::db::calendar::NewCalendar;
use crate::error::Result;
use crate::state::AppState;

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
