//! Tauri commands for the video-conferencing integrations (#148).
//!
//! Two browser-assisted auth flows live here, plus a single
//! protocol-agnostic `meet_create_url` that dispatches via the
//! `MeetProvider` registry. Settings UI calls the auth pair to
//! create / pair an account; the event editor calls
//! `meet_create_url` once the account is set up.

use serde::Serialize;
use tauri::State;

use crate::db;
use crate::error::{Error, Result};
use crate::meet;
use crate::state::AppState;

/// Wire-format Tauri response for `meet_talk_login_start`. The
/// frontend opens `login_url` in the user's default browser, then
/// hands `poll_endpoint` + `poll_token` back to
/// `meet_talk_login_complete` so the backend can poll until the
/// user finishes the in-browser login flow.
#[derive(Debug, Serialize)]
pub struct TalkLoginStart {
    pub login_url: String,
    pub poll_endpoint: String,
    pub poll_token: String,
}

#[tauri::command]
pub async fn meet_talk_login_start(server_url: String) -> Result<TalkLoginStart> {
    let flow = meet::talk::login_flow_v2_start(&server_url).await?;
    Ok(TalkLoginStart {
        login_url: flow.login,
        poll_endpoint: flow.poll.endpoint,
        poll_token: flow.poll.token,
    })
}

/// Drive Login Flow v2 to completion, then create the local
/// account row + meet binding + keyring entry. Returns the new
/// account id so the frontend can navigate back to its detail.
#[tauri::command]
pub async fn meet_talk_login_complete(
    state: State<'_, AppState>,
    poll_endpoint: String,
    poll_token: String,
    display_name: Option<String>,
) -> Result<String> {
    let flow = meet::talk::LoginFlowStart {
        login: String::new(),
        poll: meet::talk::LoginPoll {
            token: poll_token,
            endpoint: poll_endpoint,
        },
    };
    let creds = meet::talk::login_flow_v2_complete(&flow, 300).await?;

    let id = uuid::Uuid::new_v4().to_string();
    let display = display_name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("Talk @ {}", short_host(&creds.server)));
    // Talk's loginName is a Nextcloud username, not an email. Keep
    // it in `username`; leave `email` empty so the listing falls
    // back to the username + "NEXTCLOUD TALK" label.
    let config = db::accounts::AccountConfig {
        display_name: display,
        email: String::new(),
        provider: "generic".into(),
        mail_protocol: String::new(),
        imap_host: String::new(),
        imap_port: 0,
        smtp_host: String::new(),
        smtp_port: 0,
        jmap_url: String::new(),
        caldav_url: String::new(),
        meet_url: creds.server.clone(),
        meet_protocol: "talk".into(),
        username: creds.login_name.clone(),
        password: String::new(),
        use_tls: true,
        signature: String::new(),
        jmap_auth_method: "basic".into(),
        oidc_token_endpoint: String::new(),
        oidc_client_id: String::new(),
        calendar_sync_enabled: false,
        mail_sync_enabled: false,
        contacts_sync_enabled: false,
        mail_sync_interval_seconds: None,
        calendar_sync_interval_seconds: None,
        contacts_sync_interval_seconds: None,
        has_calendar_binding: false,
        has_contacts_binding: false,
    };
    let conn = state.db.writer().await;
    db::accounts::insert_account(&conn, &id, &config)?;
    drop(conn);

    crate::oauth::store_tokens(
        &id,
        &crate::oauth::OAuthTokens {
            access_token: creds.app_password,
            refresh_token: None,
            expires_at: None,
        },
    )?;
    Ok(id)
}

#[derive(Debug, Serialize)]
pub struct MatrixLoginStart {
    pub login_url: String,
    pub port: u16,
}

/// Matches the SSO callback timeout in `meet/matrix.rs`. Sessions
/// older than this in `AppState.matrix_sso_listeners` are
/// definitely abandoned (the user closed their browser, the app
/// crashed, etc.) and get dropped on every fresh login_start
/// call so the map can't grow unboundedly.
const MATRIX_SSO_LISTENER_TTL: std::time::Duration = std::time::Duration::from_secs(360);

fn evict_stale_matrix_sessions(
    sessions: &mut std::collections::HashMap<u16, crate::state::MatrixSsoSession>,
) {
    let now = std::time::Instant::now();
    sessions.retain(|_port, s| now.duration_since(s.created) < MATRIX_SSO_LISTENER_TTL);
}

/// Bind a local listener and return the SSO redirect URL the
/// frontend should open. The listener is moved into a background
/// task that waits for the callback (see `meet_matrix_login_complete`).
#[tauri::command]
pub async fn meet_matrix_login_start(
    state: State<'_, AppState>,
    homeserver_url: String,
) -> Result<MatrixLoginStart> {
    let (url, listener, sso_state) = meet::matrix::sso_login_start(&homeserver_url)?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::Other(format!("matrix listener port: {}", e)))?
        .port();
    {
        let mut map = state.matrix_sso_listeners.lock().unwrap();
        evict_stale_matrix_sessions(&mut map);
        map.insert(
            port,
            crate::state::MatrixSsoSession {
                created: std::time::Instant::now(),
                listener,
                state: sso_state,
            },
        );
    }
    Ok(MatrixLoginStart {
        login_url: url,
        port,
    })
}

/// Wait for the SSO callback on `port`, exchange the loginToken,
/// persist the account row + keyring entry, return the account id.
#[tauri::command]
pub async fn meet_matrix_login_complete(
    state: State<'_, AppState>,
    homeserver_url: String,
    port: u16,
    display_name: Option<String>,
) -> Result<String> {
    let session = state
        .matrix_sso_listeners
        .lock()
        .unwrap()
        .remove(&port)
        .ok_or_else(|| {
            Error::Other(format!(
                "Matrix SSO: no pending listener for port {} — start the flow again",
                port,
            ))
        })?;
    let listener = session.listener;
    let expected_state = session.state;
    let token = tokio::task::spawn_blocking(move || {
        meet::matrix::await_login_token(listener, &expected_state)
    })
    .await
    .map_err(|e| Error::Other(format!("matrix sso join: {}", e)))??;
    let result = meet::matrix::exchange_login_token(&homeserver_url, &token).await?;

    let id = uuid::Uuid::new_v4().to_string();
    let display = display_name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("Matrix ({})", result.user_id));
    // Matrix MXIDs (`@user:server.tld`) aren't emails. Same
    // treatment as Talk: empty email, MXID lives in `username`.
    let config = db::accounts::AccountConfig {
        display_name: display,
        email: String::new(),
        provider: "generic".into(),
        mail_protocol: String::new(),
        imap_host: String::new(),
        imap_port: 0,
        smtp_host: String::new(),
        smtp_port: 0,
        jmap_url: String::new(),
        caldav_url: String::new(),
        meet_url: result.homeserver.clone(),
        meet_protocol: "matrix".into(),
        username: result.user_id.clone(),
        password: String::new(),
        use_tls: true,
        signature: String::new(),
        jmap_auth_method: "basic".into(),
        oidc_token_endpoint: String::new(),
        oidc_client_id: String::new(),
        calendar_sync_enabled: false,
        mail_sync_enabled: false,
        contacts_sync_enabled: false,
        mail_sync_interval_seconds: None,
        calendar_sync_interval_seconds: None,
        contacts_sync_interval_seconds: None,
        has_calendar_binding: false,
        has_contacts_binding: false,
    };
    let conn = state.db.writer().await;
    db::accounts::insert_account(&conn, &id, &config)?;
    drop(conn);

    crate::oauth::store_tokens(
        &id,
        &crate::oauth::OAuthTokens {
            access_token: result.access_token,
            refresh_token: None,
            expires_at: None,
        },
    )?;
    Ok(id)
}

// --- Zoom OAuth flow (#148) -----------------------------------------------
//
// Standard OAuth 2.0 Authorization Code + PKCE against
// `oauth::ZOOM`. The flow stashes its session (listener + PKCE
// verifier + state nonce) in `state.zoom_oauth_sessions` between
// the start and complete commands. The session map is evicted on
// each insert by the same TTL pattern as the Matrix SSO map.

const ZOOM_OAUTH_TTL: std::time::Duration = std::time::Duration::from_secs(360);

fn evict_stale_zoom_sessions(
    sessions: &mut std::collections::HashMap<u16, crate::state::ZoomOAuthSession>,
) {
    let now = std::time::Instant::now();
    sessions.retain(|_port, s| now.duration_since(s.created) < ZOOM_OAUTH_TTL);
}

#[derive(Debug, Serialize)]
pub struct ZoomLoginStart {
    pub login_url: String,
    pub port: u16,
}

/// Build the Zoom OAuth authorize URL, bind the local callback
/// listener, and stash the PKCE verifier + state for the
/// matching `meet_zoom_login_complete`.
#[tauri::command]
pub async fn meet_zoom_login_start(state: State<'_, AppState>) -> Result<ZoomLoginStart> {
    // Evict any session that's still parked on the fixed Zoom
    // port BEFORE we ask the OS to bind it. Without this, an
    // abandoned previous flow (browser closed, renderer reload
    // mid-flow, user cancels) keeps the listener and the next
    // start fails with EADDRINUSE — and the eviction sweep that
    // runs after the bind never reaches the stale entry.
    if let Some(fixed_port) = crate::oauth::ZOOM.redirect_fixed_port {
        let mut map = state.zoom_oauth_sessions.lock().unwrap();
        evict_stale_zoom_sessions(&mut map);
        if let Some(prev) = map.remove(&fixed_port) {
            log::info!(
                "meet_zoom_login_start: dropping previous session on port {} (age {:?}) so the new one can bind",
                fixed_port,
                std::time::Instant::now().duration_since(prev.created),
            );
            // `prev` (and its listener) drop here, releasing the
            // port before we hit `get_auth_url` below.
        }
    }
    let (url, listener, code_verifier, oauth_state) =
        crate::oauth::get_auth_url(&crate::oauth::ZOOM)?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::Other(format!("zoom listener port: {}", e)))?
        .port();
    {
        let mut map = state.zoom_oauth_sessions.lock().unwrap();
        map.insert(
            port,
            crate::state::ZoomOAuthSession {
                created: std::time::Instant::now(),
                listener,
                verifier: code_verifier,
                state: oauth_state,
            },
        );
    }
    Ok(ZoomLoginStart {
        login_url: url,
        port,
    })
}

/// Wait for the Zoom OAuth callback, validate state, exchange
/// the code for tokens, persist the account row + keyring entry,
/// return the new account id.
#[tauri::command]
pub async fn meet_zoom_login_complete(
    state: State<'_, AppState>,
    port: u16,
    display_name: Option<String>,
) -> Result<String> {
    let session = state
        .zoom_oauth_sessions
        .lock()
        .unwrap()
        .remove(&port)
        .ok_or_else(|| {
            Error::Other(format!(
                "Zoom OAuth: no pending session for port {} — start the flow again",
                port,
            ))
        })?;
    let crate::state::ZoomOAuthSession {
        listener,
        verifier,
        state: expected_state,
        ..
    } = session;

    let callback = tokio::task::spawn_blocking(move || crate::oauth::wait_for_callback(listener))
        .await
        .map_err(|e| Error::Other(format!("zoom oauth join: {}", e)))??;

    match callback.state.as_deref() {
        Some(s) if s == expected_state => {}
        Some(got) => {
            // The raw nonces are unguessable random strings — no
            // value to a human, and confusing in a UI toast.
            // Log them at debug level for diagnostics; surface a
            // short message to the user.
            log::debug!(
                "zoom oauth state mismatch: expected={} got={}",
                expected_state,
                got
            );
            return Err(Error::Other(
                "Zoom sign-in could not be verified — please try again.".into(),
            ));
        }
        None => {
            return Err(Error::Other(
                "Zoom sign-in could not be verified — please try again.".into(),
            ));
        }
    }

    let tokens = crate::oauth::exchange_code(
        &crate::oauth::ZOOM,
        &callback.code,
        port,
        verifier.as_deref(),
    )
    .await?;

    let id = uuid::Uuid::new_v4().to_string();
    let display = display_name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Zoom".into());
    // Zoom is hosted by Zoom — there's no per-user server URL to
    // remember. The meet binding stores a stable marker URL so
    // `derive_bindings`'s "both fields set" guard still emits
    // the binding; `create_url` ignores it.
    let config = db::accounts::AccountConfig {
        display_name: display,
        email: String::new(),
        provider: "generic".into(),
        mail_protocol: String::new(),
        imap_host: String::new(),
        imap_port: 0,
        smtp_host: String::new(),
        smtp_port: 0,
        jmap_url: String::new(),
        caldav_url: String::new(),
        meet_url: "https://zoom.us".into(),
        meet_protocol: "zoom".into(),
        username: String::new(),
        password: String::new(),
        use_tls: true,
        signature: String::new(),
        jmap_auth_method: "basic".into(),
        oidc_token_endpoint: String::new(),
        oidc_client_id: String::new(),
        calendar_sync_enabled: false,
        mail_sync_enabled: false,
        contacts_sync_enabled: false,
        mail_sync_interval_seconds: None,
        calendar_sync_interval_seconds: None,
        contacts_sync_interval_seconds: None,
        has_calendar_binding: false,
        has_contacts_binding: false,
    };
    let conn = state.db.writer().await;
    db::accounts::insert_account(&conn, &id, &config)?;
    drop(conn);

    crate::oauth::store_tokens(&id, &tokens)?;
    Ok(id)
}

/// Provider-agnostic create. Looks up the account, finds its meet
/// provider via the registry, and returns the join URL. The event
/// editor calls this with the event's title as `name`.
#[tauri::command]
pub async fn meet_create_url(
    state: State<'_, AppState>,
    account_id: String,
    name: String,
) -> Result<String> {
    let account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };
    let provider = meet::provider_for(&account).ok_or_else(|| {
        Error::Other(format!("account {} has no usable meet binding", account_id))
    })?;
    provider.create_url(&account, &name).await
}

/// Strip scheme + path from a URL for use as a fallback display
/// name. `https://cloud.smolnet.org/` -> `cloud.smolnet.org`.
fn short_host(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}
