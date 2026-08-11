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
/// The frontend opens `login_url` in the user's default browser, then hands
/// only `session_id` back to `meet_talk_login_complete`. Poll credentials stay
/// in backend session state.
#[derive(Debug, Serialize)]
pub struct TalkLoginStart {
    pub login_url: String,
    pub session_id: String,
}

const TALK_LOGIN_SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(300);

#[tauri::command]
pub async fn meet_talk_login_start(
    state: State<'_, AppState>,
    server_url: String,
) -> Result<TalkLoginStart> {
    let flow = meet::talk::login_flow_v2_start_with_client(
        &server_url,
        &state.providers.transports.talk_http,
    )
    .await?;
    let session_id = uuid::Uuid::new_v4().to_string();
    {
        let mut sessions = state.talk_login_sessions.lock().unwrap();
        let now = std::time::Instant::now();
        sessions.retain(|_, session| now.duration_since(session.created) < TALK_LOGIN_SESSION_TTL);
        sessions.insert(
            session_id.clone(),
            crate::state::TalkLoginSession {
                created: now,
                flow: flow.clone(),
            },
        );
    }
    Ok(TalkLoginStart {
        login_url: flow.login,
        session_id,
    })
}

/// Drive Login Flow v2 to completion, then create the local
/// account row + meet binding + keyring entry. Returns the new
/// account id so the frontend can navigate back to its detail.
#[tauri::command]
pub async fn meet_talk_login_complete(
    state: State<'_, AppState>,
    session_id: String,
    display_name: Option<String>,
) -> Result<String> {
    let session = state
        .talk_login_sessions
        .lock()
        .unwrap()
        .remove(&session_id)
        .ok_or_else(|| Error::Other("Talk login session expired; start again".into()))?;
    if session.created.elapsed() >= TALK_LOGIN_SESSION_TTL {
        return Err(Error::Other(
            "Talk login session expired; start again".into(),
        ));
    }
    let creds = meet::talk::login_flow_v2_complete_with_client(
        &session.flow,
        300,
        &state.providers.transports.talk_http,
    )
    .await?;
    validate_returned_server_url(&creds.server)?;

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
        pgp_attach_pubkey_on_sign: true,
        pgp_autocrypt_header: true,
        pgp_encrypt_subject: true,
        pgp_encrypt_drafts: true,
    };
    let conn = state.db.writer().await;
    db::accounts::insert_account(&conn, &id, &config)?;
    drop(conn);

    state.providers.token_store().store(
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
                homeserver: homeserver_url,
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
    let homeserver_url = session.homeserver;
    let token = tokio::task::spawn_blocking(move || {
        meet::matrix::await_login_token(listener, &expected_state)
    })
    .await
    .map_err(|e| Error::Other(format!("matrix sso join: {}", e)))??;
    let result = meet::matrix::exchange_login_token_with_client(
        &homeserver_url,
        &token,
        &state.providers.transports.matrix_http,
    )
    .await?;
    validate_returned_server_url(&result.homeserver)?;

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
        pgp_attach_pubkey_on_sign: true,
        pgp_autocrypt_header: true,
        pgp_encrypt_subject: true,
        pgp_encrypt_drafts: true,
    };
    let conn = state.db.writer().await;
    db::accounts::insert_account(&conn, &id, &config)?;
    drop(conn);

    state.providers.token_store().store(
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

    // Don't log raw `state` values — they're CSRF secrets.
    log::info!(
        "zoom oauth state validation: has_returned={}",
        callback.state.is_some(),
    );
    match callback.state.as_deref() {
        Some(s) if s == expected_state => {
            log::info!("zoom oauth: state OK");
        }
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

    let tokens = state
        .providers
        .token_endpoint()
        .exchange_code(
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
        pgp_attach_pubkey_on_sign: true,
        pgp_autocrypt_header: true,
        pgp_encrypt_subject: true,
        pgp_encrypt_drafts: true,
    };
    let token_guard = state.providers.lock_zoom_tokens(&id).await;
    let conn = state.db.writer().await;
    crate::commands::accounts::insert_zoom_account(&conn, &id, &config, &tokens, &token_guard)?;
    Ok(id)
}

/// Provider-agnostic create. Looks up the account, finds its meet
/// provider via the registry, and returns the join URL plus the
/// provider-specific meeting id and the account/protocol used —
/// the frontend stashes these alongside the form state so the
/// matching `create_event` / `update_event` call can persist a
/// `meet_meetings` binding row. The binding is what lets
/// `delete_event` know which remote meeting to cancel and
/// `update_event` know which one to reschedule.
#[derive(Debug, Serialize)]
pub struct MeetCreateResponse {
    pub lifecycle_id: String,
    pub account_id: String,
    pub protocol: String,
    pub meeting_id: String,
    pub join_url: String,
}

#[tauri::command]
pub async fn meet_create_url(
    state: State<'_, AppState>,
    account_id: String,
    name: String,
    start_time: Option<String>,
    duration_minutes: Option<u32>,
) -> Result<MeetCreateResponse> {
    let account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };
    let provider = meet::provider_for(&account).ok_or_else(|| {
        Error::Other(format!("account {} has no usable meet binding", account_id))
    })?;
    let protocol = provider.protocol().to_string();
    let ctx = meet::MeetProviderCtx {
        services: &state.providers,
    };
    let res = provider
        .create_url(
            &ctx,
            &account,
            &name,
            start_time.as_deref(),
            duration_minutes,
        )
        .await?;
    let lifecycle_id = uuid::Uuid::new_v4().to_string();
    let pending = db::meet_pending_meetings::PendingMeeting {
        lifecycle_id: lifecycle_id.clone(),
        account_id: account.id.clone(),
        protocol: protocol.clone(),
        meeting_id: res.meeting_id.clone(),
        join_url: res.join_url.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let persist_result = {
        let conn = state.db.writer().await;
        db::meet_pending_meetings::insert(&conn, &pending)
    };
    if let Err(persist_error) = persist_result {
        // Provider creation and SQLite cannot share an atomic transaction.
        // Never return an untracked room: compensate immediately, and report
        // both failures if durable tracking and provider deletion both fail.
        let compensation = provider
            .delete_meeting(&ctx, &account, &res.meeting_id)
            .await;
        return match compensation {
            Ok(()) => Err(Error::Other(format!(
                "failed to track created meeting; remote meeting was removed: {persist_error}"
            ))),
            Err(delete_error) => Err(Error::Other(format!(
                "failed to track created meeting ({persist_error}); compensation also failed ({delete_error})"
            ))),
        };
    }
    Ok(MeetCreateResponse {
        lifecycle_id,
        account_id: account.id,
        protocol,
        meeting_id: res.meeting_id,
        join_url: res.join_url,
    })
}

/// Delete one backend-owned pending meeting. The lifecycle lock spans the
/// provider call, but database handles do not.
pub async fn discard_pending(state: &AppState, lifecycle_id: &str) -> Result<()> {
    let lifecycle_lock = state.meet_lifecycle.acquire(lifecycle_id)?;
    let _guard = lifecycle_lock.lock().await;
    let (pending, account) = {
        let conn = state.db.reader();
        let Some(pending) = db::meet_pending_meetings::get(&conn, lifecycle_id)? else {
            return Ok(());
        };
        let account =
            db::accounts::get_account_full(&conn, &pending.account_id).map_err(|error| {
                Error::Other(format!(
                    "pending meeting {} account {} unavailable: {}",
                    lifecycle_id, pending.account_id, error
                ))
            })?;
        (pending, account)
    };
    let provider = meet::provider_for(&account).ok_or_else(|| {
        Error::Other(format!(
            "pending meeting {} account {} has no meet provider",
            lifecycle_id, pending.account_id
        ))
    })?;
    if provider.protocol() != pending.protocol {
        return Err(Error::Other(format!(
            "pending meeting {} protocol '{}' does not match account provider '{}'",
            lifecycle_id,
            pending.protocol,
            provider.protocol()
        )));
    }
    let provider_result = provider
        .delete_meeting(
            &meet::MeetProviderCtx {
                services: &state.providers,
            },
            &account,
            &pending.meeting_id,
        )
        .await;
    let conn = state.db.writer().await;
    complete_pending_discard(&conn, lifecycle_id, provider_result)
}

fn complete_pending_discard(
    conn: &rusqlite::Connection,
    lifecycle_id: &str,
    provider_result: Result<()>,
) -> Result<()> {
    provider_result?;
    db::meet_pending_meetings::delete(conn, lifecycle_id)?;
    Ok(())
}

#[tauri::command]
pub async fn meet_discard_pending(state: State<'_, AppState>, lifecycle_id: String) -> Result<()> {
    discard_pending(&state, &lifecycle_id).await
}

pub fn pending_lifecycle_ids(state: &AppState) -> Vec<String> {
    let conn = state.db.reader();
    match db::meet_pending_meetings::list(&conn) {
        Ok(rows) => rows.into_iter().map(|row| row.lifecycle_id).collect(),
        Err(error) => {
            log::warn!(
                "failed to list pending meetings for startup cleanup: {}",
                error
            );
            Vec::new()
        }
    }
}

pub async fn sweep_pending(state: &AppState, lifecycle_ids: Vec<String>) {
    for lifecycle_id in lifecycle_ids {
        if let Err(error) = discard_pending(state, &lifecycle_id).await {
            log::warn!(
                "startup cleanup retained pending meeting {}: {}",
                lifecycle_id,
                error
            );
        }
    }
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

fn validate_returned_server_url(url: &str) -> Result<()> {
    crate::mail::url_validation::require_https(url)
}

#[cfg(test)]
mod tests {
    use super::{complete_pending_discard, validate_returned_server_url};
    use crate::db;
    use crate::error::Error;

    fn pending_connection() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE meet_pending_meetings (
                lifecycle_id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                protocol TEXT NOT NULL,
                meeting_id TEXT NOT NULL,
                join_url TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            INSERT INTO meet_pending_meetings VALUES
                ('lifecycle', 'account', 'zoom', 'meeting',
                 'https://example.test', '2026-08-11T20:00:00Z');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn returned_server_url_accepts_https_and_debug_loopback_http() {
        assert!(validate_returned_server_url("https://meet.example.com").is_ok());
        let loopback_urls = [
            "http://localhost:8080",
            "http://127.0.0.1:8080",
            "http://[::1]:8080",
        ];
        for url in loopback_urls {
            assert_eq!(
                validate_returned_server_url(url).is_ok(),
                cfg!(debug_assertions)
            );
        }
    }

    #[test]
    fn returned_server_url_rejects_malformed_and_public_cleartext_urls() {
        assert!(validate_returned_server_url("not a URL").is_err());
        assert!(validate_returned_server_url("http://meet.example.com").is_err());
        assert!(validate_returned_server_url("http://192.0.2.1").is_err());
    }

    #[test]
    fn provider_delete_failure_retains_pending_ownership() {
        let conn = pending_connection();
        let result = complete_pending_discard(
            &conn,
            "lifecycle",
            Err(Error::Other("provider failed".into())),
        );

        assert!(result.is_err());
        assert!(db::meet_pending_meetings::get(&conn, "lifecycle")
            .unwrap()
            .is_some());
    }

    #[test]
    fn provider_delete_success_removes_pending_ownership() {
        let conn = pending_connection();
        complete_pending_discard(&conn, "lifecycle", Ok(())).unwrap();

        assert!(db::meet_pending_meetings::get(&conn, "lifecycle")
            .unwrap()
            .is_none());
    }
}
