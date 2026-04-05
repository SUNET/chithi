use tauri::State;

use crate::error::{Error, Result};
use crate::oauth;
use crate::state::AppState;

/// Start the OAuth2 flow for a provider. Returns the auth URL to open in the browser.
#[tauri::command]
pub async fn oauth_start(
    provider: String,
) -> Result<OAuthStartResult> {
    let prov = match provider.as_str() {
        "google" => &oauth::GOOGLE,
        _ => return Err(Error::Other(format!("Unknown OAuth provider: {}", provider))),
    };

    let (url, port) = oauth::get_auth_url(prov)?;
    log::info!("OAuth2: started flow for {} on port {}", provider, port);
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
    let prov = match provider.as_str() {
        "google" => &oauth::GOOGLE,
        _ => return Err(Error::Other(format!("Unknown OAuth provider: {}", provider))),
    };

    // Wait for callback in a blocking thread (TcpListener::accept blocks)
    let code = tokio::task::spawn_blocking(move || {
        oauth::wait_for_callback(port)
    })
    .await
    .map_err(|e| Error::Other(format!("OAuth callback task failed: {}", e)))??;

    // Exchange code for tokens
    let tokens = oauth::exchange_code(prov, &code, port).await?;

    // Store tokens in keyring
    oauth::store_tokens(&account_id, &tokens)?;

    log::info!("OAuth2: completed flow for account {}", account_id);
    Ok(())
}

/// Get a valid access token for an account, refreshing if needed.
#[tauri::command]
pub async fn oauth_get_token(
    provider: String,
    account_id: String,
) -> Result<String> {
    let prov = match provider.as_str() {
        "google" => &oauth::GOOGLE,
        _ => return Err(Error::Other(format!("Unknown OAuth provider: {}", provider))),
    };

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
