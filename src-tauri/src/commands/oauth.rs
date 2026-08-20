use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::Mutex;
use std::time::{Duration, Instant};
#[cfg(target_os = "android")]
use tauri::Manager;
use tauri::{AppHandle, Runtime, State};

use crate::error::{Error, Result};
use crate::oauth;
use crate::state::AppState;

/// Maximum time an OAuth session is valid before being evicted.
const SESSION_TTL: Duration = Duration::from_secs(300);

/// Per-session state stored between oauth_start and oauth_complete.
struct OAuthSession {
    verifier: Option<String>,
    state: String,
    listener: TcpListener,
    created_at: Instant,
}

static OAUTH_SESSIONS: Mutex<Option<HashMap<u16, OAuthSession>>> = Mutex::new(None);

fn store_session(port: u16, session: OAuthSession) {
    let mut guard = OAUTH_SESSIONS.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    // Evict expired sessions to prevent unbounded growth
    let now = Instant::now();
    map.retain(|_, s| now.duration_since(s.created_at) < SESSION_TTL);
    map.insert(port, session);
}

fn take_session(port: u16) -> Option<OAuthSession> {
    let mut guard = OAUTH_SESSIONS.lock().unwrap();
    guard.as_mut().and_then(|map| {
        let session = map.remove(&port)?;
        // Reject expired sessions
        if Instant::now().duration_since(session.created_at) >= SESSION_TTL {
            log::warn!("OAuth session on port {} expired", port);
            return None;
        }
        Some(session)
    })
}

fn get_provider(name: &str) -> Result<&'static oauth::OAuthProvider> {
    match name {
        "google" => Ok(&oauth::GOOGLE),
        "microsoft" => Ok(&oauth::MICROSOFT),
        _ => Err(Error::Other(format!("Unknown OAuth provider: {}", name))),
    }
}

/// Start the OAuth2 flow for a provider. Returns the auth URL to open in the browser.
#[tauri::command]
pub async fn oauth_start(provider: String) -> Result<OAuthStartResult> {
    let prov = get_provider(&provider)?;

    let (url, listener, code_verifier, state) = oauth::get_auth_url(prov)?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::Other(format!("Failed to get port: {}", e)))?
        .port();

    store_session(
        port,
        OAuthSession {
            verifier: code_verifier,
            state,
            listener,
            created_at: Instant::now(),
        },
    );

    log::info!("OAuth2: started {} flow on port {}", provider, port);
    Ok(OAuthStartResult { url, port })
}

#[derive(serde::Serialize)]
pub struct OAuthStartResult {
    pub url: String,
    pub port: u16,
}

/// Wait for the OAuth2 callback and exchange the code for tokens.
/// This blocks until the user completes the browser flow or 5 minutes elapse.
#[tauri::command]
pub async fn oauth_complete(
    state: State<'_, AppState>,
    provider: String,
    port: u16,
    account_id: String,
) -> Result<()> {
    let prov = get_provider(&provider)?;

    let session = take_session(port).ok_or_else(|| {
        Error::Other(format!(
            "No OAuth session found for port {} (expired or never started)",
            port
        ))
    })?;

    let expected_state = session.state.clone();
    let code_verifier = session.verifier.clone();

    // Wait for callback in a blocking thread (TcpListener::accept blocks)
    let callback_expected_state = expected_state.clone();
    let result = tokio::task::spawn_blocking(move || {
        oauth::wait_for_callback(session.listener, &callback_expected_state)
    })
    .await
    .map_err(|e| Error::Other(format!("OAuth callback task failed: {}", e)))??;

    // Validate CSRF state parameter. Don't log the raw values — `state`
    // is a CSRF secret and logging it (or returning it in the error
    // message) can leak session secrets into the persistent log file.
    log::info!(
        "OAuth2[{}]: state validation has_returned={}",
        provider,
        result.state.is_some(),
    );
    match result.state {
        Some(ref returned_state) if returned_state == &expected_state => {
            log::debug!("OAuth2: state parameter validated");
        }
        Some(_) => {
            log::error!("OAuth2[{}]: state MISMATCH (possible CSRF)", provider);
            return Err(Error::Other("OAuth2 state mismatch (possible CSRF)".into()));
        }
        None => {
            return Err(Error::Other(
                "OAuth2 callback missing required state parameter".into(),
            ));
        }
    }

    // Exchange code for tokens
    let tokens = state
        .providers
        .token_endpoint()
        .exchange_code(prov, &result.code, port, code_verifier.as_deref())
        .await?;

    // Store tokens in keyring
    state.providers.token_store().store(&account_id, &tokens)?;

    log::info!(
        "OAuth2: completed {} flow for account {}",
        provider,
        account_id
    );
    Ok(())
}

/// Check if an account has OAuth tokens stored.
#[tauri::command]
pub async fn oauth_has_tokens(state: State<'_, AppState>, account_id: String) -> Result<bool> {
    Ok(state.providers.token_store().load(&account_id)?.is_some())
}

/// Fetch the user's profile (display name + email) from Microsoft Graph.
#[tauri::command]
pub async fn oauth_get_ms_profile(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<MsProfile> {
    let client = state
        .providers
        .graph_client(&account_id, crate::provider::GraphTokenPurpose::Baseline)
        .await?;
    let user = client.get_me().await?;

    Ok(MsProfile {
        display_name: user.display_name,
        email: user.email,
        login_email: user.login_email,
    })
}

#[derive(serde::Serialize)]
pub struct MsProfile {
    pub display_name: String,
    pub email: String,
    pub login_email: String,
}

/// Start the JMAP OIDC device flow.
#[tauri::command]
pub async fn jmap_oidc_start(
    state: State<'_, AppState>,
    jmap_url: String,
    email: String,
    client_id: String,
) -> Result<JmapOidcStartResult> {
    let base_url = if !jmap_url.is_empty() {
        jmap_url.trim_end_matches('/').to_string()
    } else {
        let domain = email
            .rsplit_once('@')
            .map(|(_, d)| d)
            .ok_or_else(|| Error::Other(format!("Cannot extract domain from '{}'", email)))?;
        let candidates = [
            format!("https://{}", domain),
            format!("https://mail.{}", domain),
            format!("https://jmap.{}", domain),
        ];
        let mut found = None;
        for c in &candidates {
            let url = format!("{}/.well-known/openid-configuration", c);
            if let Ok(resp) = state
                .providers
                .transports
                .jmap_discovery_http
                .get(&url)
                .send()
                .await
            {
                if resp.status().is_success() {
                    found = Some(c.clone());
                    break;
                }
            }
        }
        found.ok_or_else(|| {
            Error::Other(format!(
                "OIDC auto-discovery failed for {} (tried {}, mail.{}, jmap.{})",
                domain, domain, domain, domain
            ))
        })?
    };

    let endpoints =
        crate::oauth::discover_oidc_with_client(&base_url, &state.providers.transports.oidc_http)
            .await?;

    let device_auth_endpoint = endpoints
        .device_authorization_endpoint
        .ok_or_else(|| Error::Other("Server does not support device authorization flow".into()))?;

    let effective_client_id = if !client_id.trim().is_empty() {
        client_id.trim().to_string()
    } else if let Some(ref reg_endpoint) = endpoints.registration_endpoint {
        crate::oauth::register_oidc_client_with_client(
            reg_endpoint,
            &state.providers.transports.oidc_http,
        )
        .await?
    } else {
        return Err(Error::Other(
            "OIDC requires a client_id but none was provided and the server does not support dynamic client registration.".into()
        ));
    };

    let device_resp = crate::oauth::device_auth_start_with_client(
        &device_auth_endpoint,
        &effective_client_id,
        &state.providers.transports.oidc_http,
    )
    .await?;

    Ok(JmapOidcStartResult {
        verification_uri: device_resp.verification_uri.clone(),
        verification_uri_complete: device_resp.verification_uri_complete.clone(),
        user_code: device_resp.user_code.clone(),
        device_code: device_resp.device_code.clone(),
        interval: device_resp.interval,
        expires_in: device_resp.expires_in,
        token_endpoint: endpoints.token_endpoint,
        client_id: effective_client_id,
    })
}

#[derive(serde::Serialize)]
pub struct JmapOidcStartResult {
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
    pub device_code: String,
    pub interval: u64,
    pub expires_in: u64,
    pub token_endpoint: String,
    pub client_id: String,
}

/// Poll the token endpoint until the user completes device authorization.
#[tauri::command]
pub async fn jmap_oidc_complete(
    state: State<'_, AppState>,
    device_code: String,
    token_endpoint: String,
    interval: u64,
    expires_in: u64,
    account_id: String,
    client_id: String,
) -> Result<()> {
    let tokens = crate::oauth::device_auth_poll_with_client(
        &token_endpoint,
        &device_code,
        interval,
        expires_in,
        &client_id,
        &state.providers.transports.oidc_poll_http,
    )
    .await?;

    state.providers.token_store().store(&account_id, &tokens)?;

    log::info!(
        "JMAP OIDC: device flow completed for account {}",
        account_id
    );
    Ok(())
}

/// Open an OAuth verification URL in a way that keeps the host app foreground.
///
/// Android: launches an `androidx.browser.customtabs.CustomTabsIntent` against
/// the MainActivity, which overlays the tab on top of the app so the device
/// process isn't paused while the user is authorizing.
///
/// Other platforms: defers to `tauri_plugin_opener::open_url`.
#[tauri::command]
pub async fn open_oauth_url<R: Runtime>(app: AppHandle<R>, url: String) -> Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(Error::Other(format!(
            "Refusing to open non-http(s) URL: {}",
            url
        )));
    }

    #[cfg(target_os = "android")]
    {
        use jni::objects::{JObject, JValue};
        use jni::JNIEnv;

        let webview_window = app
            .get_webview_window("main")
            .ok_or_else(|| Error::Other("No main webview window".into()))?;
        let url_for_jni = url.clone();
        webview_window
            .with_webview(move |platform_webview| {
                platform_webview.jni_handle().exec(
                    move |env: &mut JNIEnv, activity: &JObject, _webview: &JObject| {
                        let j_url = match env.new_string(&url_for_jni) {
                            Ok(s) => s,
                            Err(e) => {
                                log::error!("openCustomTab: new_string failed: {e}");
                                return;
                            }
                        };
                        let j_obj: &JObject = j_url.as_ref();
                        if let Err(e) = env.call_method(
                            activity,
                            "openCustomTab",
                            "(Ljava/lang/String;)V",
                            &[JValue::Object(j_obj)],
                        ) {
                            log::error!("openCustomTab call failed: {e}");
                        }
                    },
                );
            })
            .map_err(|e| Error::Other(format!("with_webview failed: {}", e)))?;
        return Ok(());
    }

    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        tauri_plugin_opener::open_url(&url, None::<&str>)
            .map_err(|e| Error::Other(format!("open_url failed: {}", e)))
    }
}
