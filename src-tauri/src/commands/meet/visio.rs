//! Tauri/webview controller for La Suite Visio authentication.
//!
//! Provider API behavior lives in [`crate::meet::visio`]. This module owns the
//! application-specific boundary: restricted auth windows, in-flight session
//! state, account persistence, token lifecycle, cancellation, and expiry.

use serde::Serialize;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use tauri::Manager;
use tauri::State;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::db;
use crate::error::{Error, Result};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::meet;
use crate::state::AppState;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
const LOGIN_SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(180);
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const CLOSED_AFTER_SUCCESS_GRACE: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const LOGIN_ACTIVE: u8 = 0;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const LOGIN_CANCELLED: u8 = 1;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const LOGIN_COMMITTING: u8 = 2;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const LOGIN_DONE: u8 = 3;

#[derive(Debug, Serialize)]
pub struct VisioLoginStart {
    pub session_id: String,
}

/// Initialize Meet's add-on exchange and open a dedicated Visio/OIDC webview.
/// The remote window has no Tauri capability entry. Its document-start bridge
/// runs only on the exact Visio transit page and keeps the transit token out of
/// URLs and renderer IPC.
#[tauri::command]
pub async fn meet_visio_login_start(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    server_url: String,
    account_id: Option<String>,
) -> Result<VisioLoginStart> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = (app, state, server_url, account_id);
        return Err(Error::Other(
            "Visio sign-in is currently supported only on desktop".into(),
        ));
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        login_start_desktop(app, state, server_url, account_id).await
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
async fn login_start_desktop(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    server_url: String,
    account_id: Option<String>,
) -> Result<VisioLoginStart> {
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::Arc;
    use tauri::webview::{NewWindowResponse, PageLoadEvent};

    let instance = meet::visio::VisioInstance::parse(&server_url)?;
    if let Some(id) = account_id.as_deref() {
        let account = {
            let conn = state.db.reader();
            db::accounts::get_account_full(&conn, id)?
        };
        let binding = account
            .meet_binding()
            .filter(|binding| binding.protocol == "visio")
            .ok_or_else(|| Error::Other(format!("account {id} is not bound to Visio")))?;
        let configured = meet::visio::VisioInstance::parse(&binding.meet_config()?.url)?;
        if configured.origin() != instance.origin() {
            return Err(Error::Other(
                "Visio reauthentication must use the account's configured instance".into(),
            ));
        }
    }

    let http = meet::visio::build_login_client()?;
    let start = meet::visio::init_addon_session_with_client(&instance, &http).await?;
    let bootstrap_script = instance.transit_bootstrap_script(&start.transit_token)?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let window_label = format!("visio-auth-{session_id}");
    let window_state = Arc::new(crate::state::VisioAuthWindowState {
        closed: AtomicBool::new(false),
        success_loaded: AtomicBool::new(false),
    });

    let navigation_instance = instance.clone();
    let page_instance = instance.clone();
    let page_state = window_state.clone();
    let window = tauri::WebviewWindowBuilder::new(
        &app,
        &window_label,
        tauri::WebviewUrl::External(instance.transit_url()),
    )
    .title("Sign in to La Suite Visio")
    .inner_size(760.0, 720.0)
    .center()
    .incognito(true)
    .devtools(false)
    .initialization_script(bootstrap_script)
    .on_navigation(move |url| navigation_instance.allows_auth_navigation(url))
    .on_new_window(|_url, _features| NewWindowResponse::Deny)
    .on_page_load(move |_window, payload| {
        if payload.event() == PageLoadEvent::Started && page_instance.is_success_url(payload.url())
        {
            page_state.success_loaded.store(true, Ordering::Release);
        }
    })
    .build()
    .map_err(|error| Error::Other(format!("Could not open Visio sign-in window: {error}")))?;
    let close_state = window_state.clone();
    window.on_window_event(move |event| {
        if matches!(
            event,
            tauri::WindowEvent::CloseRequested { .. } | tauri::WindowEvent::Destroyed
        ) {
            close_state.closed.store(true, Ordering::Release);
        }
    });

    let session = Arc::new(crate::state::VisioLoginSession {
        created: std::time::Instant::now(),
        instance,
        http,
        csrf_token: start.csrf_token,
        account_id,
        window_label,
        window_state,
        cancellation: tokio_util::sync::CancellationToken::new(),
        phase: AtomicU8::new(LOGIN_ACTIVE),
    });
    let stale_sessions = {
        let mut sessions = state.visio_login_sessions.lock().unwrap();
        let now = std::time::Instant::now();
        let mut stale_sessions = Vec::new();
        sessions.retain(|_, candidate| {
            let keep = now.duration_since(candidate.created) < LOGIN_SESSION_TTL;
            if !keep {
                stale_sessions.push(candidate.clone());
            }
            keep
        });
        sessions.insert(session_id.clone(), session.clone());
        stale_sessions
    };
    for stale in stale_sessions {
        mark_login_cancelled(&stale);
        destroy_auth_window(&app, &stale.window_label);
    }

    // Expire this flow without waiting for another login attempt. This covers
    // renderer reloads between `_start` and `_complete`.
    let expiry_app = app.clone();
    let expiry_session_id = session_id.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(LOGIN_SESSION_TTL).await;
        let expired = {
            let state = expiry_app.state::<AppState>();
            let expired = state
                .visio_login_sessions
                .lock()
                .unwrap()
                .remove(&expiry_session_id);
            expired
        };
        if let Some(expired) = expired {
            mark_login_cancelled(&expired);
            destroy_auth_window(&expiry_app, &expired.window_label);
        }
    });

    Ok(VisioLoginStart { session_id })
}

/// Poll the backend-owned add-on session, persist its JWT, and create or
/// reauthenticate the local meet account. The poll response, not webview
/// navigation, is the authority that authentication completed.
#[tauri::command]
pub async fn meet_visio_login_complete(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    display_name: Option<String>,
) -> Result<String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = (app, state, session_id, display_name);
        return Err(Error::Other(
            "Visio sign-in is currently supported only on desktop".into(),
        ));
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        login_complete_desktop(app, state, session_id, display_name).await
    }
}

/// Cancel an active Visio exchange. Cancellation wins atomically until token
/// persistence begins; once persistence is committing, the caller receives a
/// clear error instead of being told that cancellation succeeded.
#[tauri::command]
pub fn meet_visio_login_cancel(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
) -> Result<()> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = (app, state, session_id);
        return Ok(());
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let session = state
            .visio_login_sessions
            .lock()
            .unwrap()
            .remove(&session_id);
        if let Some(session) = session {
            if !mark_login_cancelled(&session) {
                return Err(Error::Other(
                    "Visio sign-in has already started saving the account".into(),
                ));
            }
            destroy_auth_window(&app, &session.window_label);
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
async fn login_complete_desktop(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    display_name: Option<String>,
) -> Result<String> {
    let session = state
        .visio_login_sessions
        .lock()
        .unwrap()
        .get(&session_id)
        .cloned()
        .ok_or_else(|| Error::Other("Visio login session expired; start again".into()))?;
    let result = complete_login(&state, &session, display_name).await;
    {
        let mut sessions = state.visio_login_sessions.lock().unwrap();
        if sessions
            .get(&session_id)
            .is_some_and(|current| std::sync::Arc::ptr_eq(current, &session))
        {
            sessions.remove(&session_id);
        }
    }
    destroy_auth_window(&app, &session.window_label);
    result
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
async fn complete_login(
    state: &AppState,
    session: &crate::state::VisioLoginSession,
    display_name: Option<String>,
) -> Result<String> {
    use std::sync::atomic::Ordering;

    if session.created.elapsed() >= LOGIN_SESSION_TTL {
        return Err(Error::Other(
            "Visio login session expired; start again".into(),
        ));
    }
    let access = poll_login(session).await?;
    if session.created.elapsed() >= LOGIN_SESSION_TTL {
        return Err(Error::Other(
            "Visio login session expired; start again".into(),
        ));
    }
    session
        .phase
        .compare_exchange(
            LOGIN_ACTIVE,
            LOGIN_COMMITTING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map_err(|_| Error::Other("Visio sign-in was cancelled".into()))?;
    let result = persist_login(state, session, access, display_name).await;
    session.phase.store(LOGIN_DONE, Ordering::Release);
    result
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
async fn persist_login(
    state: &AppState,
    session: &crate::state::VisioLoginSession,
    access: meet::visio::VisioAccessToken,
    display_name: Option<String>,
) -> Result<String> {
    let expires_at = chrono::Utc::now()
        .timestamp()
        .checked_add(access.expires_in)
        .ok_or_else(|| Error::Other("Visio returned an invalid token lifetime".into()))?;
    let tokens = crate::oauth::OAuthTokens {
        access_token: access.access_token,
        refresh_token: None,
        expires_at: Some(expires_at),
    };

    if let Some(id) = session.account_id.as_deref() {
        let account_lock = state.account_lifecycle.acquire(id);
        let _account_guard = account_lock.lock().await;
        let account = {
            let conn = state.db.reader();
            db::accounts::get_account_full(&conn, id)?
        };
        let binding = account
            .meet_binding()
            .filter(|binding| binding.protocol == "visio")
            .ok_or_else(|| Error::Other(format!("account {id} is not bound to Visio")))?;
        let configured = meet::visio::VisioInstance::parse(&binding.meet_config()?.url)?;
        if configured.origin() != session.instance.origin() {
            return Err(Error::Other(
                "Visio account instance changed during sign-in; no token was stored".into(),
            ));
        }
        let binding_config = binding.meet_config()?;
        validate_reauth_identity(&binding_config.visio_user_id, &access.user_id)?;
        let token_guard = state.providers.lock_zoom_tokens(id).await;
        let mut conn = state.db.writer().await;
        let transaction = conn.transaction()?;
        db::service_bindings::set_visio_identity(&transaction, id, &access.user_id)?;
        token_guard.replace_and_commit(&tokens, move || {
            transaction.commit()?;
            Ok(())
        })?;
        return Ok(id.to_string());
    }

    let id = uuid::Uuid::new_v4().to_string();
    let display = display_name
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("Visio @ {}", session.instance.host_label()));
    let config = standalone_account_config(display, session.instance.base_url());
    let account_lock = state.account_lifecycle.acquire(&id);
    let _account_guard = account_lock.lock().await;
    let token_guard = state.providers.lock_zoom_tokens(&id).await;
    let conn = state.db.writer().await;
    crate::commands::accounts::insert_visio_account(
        &conn,
        &id,
        &config,
        &tokens,
        &access.user_id,
        &token_guard,
    )?;
    Ok(id)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
async fn poll_login(
    session: &crate::state::VisioLoginSession,
) -> Result<meet::visio::VisioAccessToken> {
    use std::sync::atomic::Ordering;

    let deadline = session.created + LOGIN_SESSION_TTL;
    let mut closed_after_success_deadline = None;
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(Error::Other(
                "Visio sign-in timed out; finish authentication and try again".into(),
            ));
        }
        let poll = meet::visio::poll_addon_session_with_client(
            &session.instance,
            &session.csrf_token,
            &session.http,
        );
        let poll_result = tokio::select! {
            _ = session.cancellation.cancelled() => {
                return Err(Error::Other("Visio sign-in was cancelled".into()));
            }
            result = poll => result?,
        };
        match poll_result {
            meet::visio::PollResult::Authenticated(token) => {
                if std::time::Instant::now() >= deadline {
                    return Err(Error::Other(
                        "Visio sign-in timed out; finish authentication and try again".into(),
                    ));
                }
                return Ok(token);
            }
            meet::visio::PollResult::Pending => {}
        }

        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(Error::Other(
                "Visio sign-in timed out; finish authentication and try again".into(),
            ));
        }
        if session.window_state.closed.load(Ordering::Acquire) {
            if !session.window_state.success_loaded.load(Ordering::Acquire) {
                return Err(Error::Other("Visio sign-in window was closed".into()));
            }
            let grace_deadline =
                closed_after_success_deadline.get_or_insert(now + CLOSED_AFTER_SUCCESS_GRACE);
            if now >= *grace_deadline {
                return Err(Error::Other(
                    "Visio authentication completed but no access token was received".into(),
                ));
            }
        }
        tokio::select! {
            _ = session.cancellation.cancelled() => {
                return Err(Error::Other("Visio sign-in was cancelled".into()));
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn mark_login_cancelled(session: &crate::state::VisioLoginSession) -> bool {
    use std::sync::atomic::Ordering;

    if session
        .phase
        .compare_exchange(
            LOGIN_ACTIVE,
            LOGIN_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
        session.cancellation.cancel();
        true
    } else {
        false
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn validate_reauth_identity(stored_user_id: &str, authenticated_user_id: &str) -> Result<()> {
    if stored_user_id.is_empty() {
        return Err(Error::Other(
            "This Visio account predates identity-bound sign-in; remove it and add it again".into(),
        ));
    }
    if stored_user_id != authenticated_user_id {
        return Err(Error::Other(
            "Visio sign-in belongs to a different user; the existing account was not changed"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn destroy_auth_window(app: &tauri::AppHandle, label: &str) {
    if let Some(window) = app.get_webview_window(label) {
        if let Err(error) = window.destroy() {
            log::debug!("Could not destroy Visio auth window {label}: {error}");
        }
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn standalone_account_config(
    display_name: String,
    meet_url: String,
) -> db::accounts::AccountConfig {
    db::accounts::AccountConfig {
        display_name,
        sender_name: String::new(),
        email: String::new(),
        provider: "generic".into(),
        mail_protocol: String::new(),
        imap_host: String::new(),
        imap_port: 0,
        smtp_host: String::new(),
        smtp_port: 0,
        jmap_url: String::new(),
        caldav_url: String::new(),
        meet_url,
        meet_protocol: "visio".into(),
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
    }
}

#[cfg(all(test, not(any(target_os = "android", target_os = "ios"))))]
mod tests {
    use super::*;

    #[test]
    fn reauth_rejects_legacy_or_different_identity_and_accepts_the_same_user() {
        assert!(validate_reauth_identity("", "authenticated-user").is_err());
        assert!(validate_reauth_identity("visio-user", "visio-user").is_ok());
        assert!(validate_reauth_identity("visio-user", "different-user").is_err());
    }

    #[test]
    fn cancellation_claim_is_one_way() {
        use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
        use std::sync::Arc;

        let session = crate::state::VisioLoginSession {
            created: std::time::Instant::now(),
            instance: meet::visio::VisioInstance::parse("https://visio.example.org").unwrap(),
            http: meet::visio::build_login_client().unwrap(),
            csrf_token: "csrf".into(),
            account_id: None,
            window_label: "visio-test".into(),
            window_state: Arc::new(crate::state::VisioAuthWindowState {
                closed: AtomicBool::new(false),
                success_loaded: AtomicBool::new(false),
            }),
            cancellation: tokio_util::sync::CancellationToken::new(),
            phase: AtomicU8::new(LOGIN_ACTIVE),
        };

        assert!(mark_login_cancelled(&session));
        assert!(session.cancellation.is_cancelled());
        assert_eq!(session.phase.load(Ordering::Acquire), LOGIN_CANCELLED);
        assert!(!mark_login_cancelled(&session));
    }
}
