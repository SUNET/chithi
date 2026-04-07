use std::sync::Mutex;
use tauri::State;

use crate::error::{Error, Result};
use crate::oauth;
use crate::state::AppState;

/// Temporary storage for PKCE code verifiers (needed between start and complete).
static PKCE_VERIFIERS: Mutex<Option<std::collections::HashMap<u16, String>>> = Mutex::new(None);

fn store_verifier(port: u16, verifier: String) {
    let mut guard = PKCE_VERIFIERS.lock().unwrap();
    let map = guard.get_or_insert_with(std::collections::HashMap::new);
    map.insert(port, verifier);
}

fn take_verifier(port: u16) -> Option<String> {
    let mut guard = PKCE_VERIFIERS.lock().unwrap();
    guard.as_mut().and_then(|map| map.remove(&port))
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
pub async fn oauth_start(
    provider: String,
) -> Result<OAuthStartResult> {
    let prov = get_provider(&provider)?;

    let (url, port, code_verifier) = oauth::get_auth_url(prov)?;

    // Store the PKCE verifier for use in oauth_complete
    if let Some(verifier) = code_verifier {
        store_verifier(port, verifier);
    }

    log::info!("OAuth2: started {} flow on port {}", provider, port);
    Ok(OAuthStartResult { url, port })
}

#[derive(serde::Serialize)]
pub struct OAuthStartResult {
    pub url: String,
    pub port: u16,
}

/// Wait for the OAuth2 callback and exchange the code for tokens.
/// This blocks until the user completes the browser flow.
#[tauri::command]
pub async fn oauth_complete(
    _state: State<'_, AppState>,
    provider: String,
    port: u16,
    account_id: String,
) -> Result<()> {
    let prov = get_provider(&provider)?;

    // Retrieve the PKCE verifier if this provider uses PKCE
    let code_verifier = take_verifier(port);

    // Wait for callback in a blocking thread (TcpListener::accept blocks)
    let code = tokio::task::spawn_blocking(move || {
        oauth::wait_for_callback(port)
    })
    .await
    .map_err(|e| Error::Other(format!("OAuth callback task failed: {}", e)))??;

    // Exchange code for tokens
    let tokens = oauth::exchange_code(prov, &code, port, code_verifier.as_deref()).await?;

    // Store tokens in keyring
    oauth::store_tokens(&account_id, &tokens)?;

    log::info!("OAuth2: completed {} flow for account {}", provider, account_id);
    Ok(())
}

/// Get a valid access token for an account, refreshing if needed.
#[tauri::command]
pub async fn oauth_get_token(
    provider: String,
    account_id: String,
) -> Result<String> {
    let prov = get_provider(&provider)?;

    let tokens = oauth::load_tokens(&account_id)?
        .ok_or_else(|| Error::Other("No OAuth tokens found. Please sign in again.".into()))?;

    if !tokens.is_expired() {
        return Ok(tokens.access_token);
    }

    // Need to refresh
    let refresh_token = tokens.refresh_token
        .ok_or_else(|| Error::Other("No refresh token. Please sign in again.".into()))?;

    let new_tokens = oauth::refresh_access_token(prov, &refresh_token).await?;
    oauth::store_tokens(&account_id, &new_tokens)?;

    Ok(new_tokens.access_token)
}

/// Check if an account has OAuth tokens stored.
#[tauri::command]
pub async fn oauth_has_tokens(
    account_id: String,
) -> Result<bool> {
    Ok(oauth::load_tokens(&account_id)?.is_some())
}

/// Fetch the user's profile (display name + email) from Microsoft Graph.
/// Used to auto-fill the account form after OAuth sign-in.
/// The initial token may be IMAP-scoped, so we refresh with Graph scopes first.
#[tauri::command]
pub async fn oauth_get_ms_profile(
    account_id: String,
) -> Result<MsProfile> {
    let tokens = oauth::load_tokens(&account_id)?
        .ok_or_else(|| Error::Other("No tokens for profile fetch".into()))?;

    let refresh_token = tokens.refresh_token.as_deref()
        .ok_or_else(|| Error::Other("No refresh token for profile fetch".into()))?;

    // The initial token is IMAP-scoped. Get a Graph-scoped token for /me.
    let graph_tokens = oauth::refresh_with_scopes(
        &oauth::MICROSOFT,
        refresh_token,
        oauth::MICROSOFT_GRAPH_SCOPES,
    ).await?;

    // Save the potentially rotated refresh token
    oauth::store_tokens(&account_id, &oauth::OAuthTokens {
        access_token: tokens.access_token,
        refresh_token: graph_tokens.refresh_token,
        expires_at: tokens.expires_at,
    })?;

    let client = crate::mail::graph::GraphClient::new(&graph_tokens.access_token);
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
    /// The actual mailbox email (e.g., outlook_...@outlook.com)
    pub email: String,
    /// The Microsoft login identity (e.g., kushaldas@gmail.com) — used for IMAP XOAUTH2
    pub login_email: String,
}
