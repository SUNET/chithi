use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Provider configurations
// ---------------------------------------------------------------------------

pub struct OAuthProvider {
    pub name: &'static str,
    pub client_id: &'static str,
    pub client_secret: &'static str,
    pub auth_url: &'static str,
    pub token_url: &'static str,
    pub scopes: &'static [&'static str],
}

pub const GOOGLE: OAuthProvider = OAuthProvider {
    name: "google",
    client_id: "96507156934-tb0mgeovj7dhpaabjc4ipm5lukhmebmg.apps.googleusercontent.com",
    client_secret: "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",
    auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    scopes: &[
        "https://www.googleapis.com/auth/calendar",   // Google Calendar API v3 (read-write)
        "https://www.googleapis.com/auth/contacts",    // Google People API v1 (read-write)
    ],
};

// Microsoft O365 placeholder — fill in when registering the Entra app
// pub const MICROSOFT: OAuthProvider = OAuthProvider { ... };

// ---------------------------------------------------------------------------
// Token types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>, // Unix timestamp
}

impl OAuthTokens {
    pub fn is_expired(&self) -> bool {
        if let Some(expires_at) = self.expires_at {
            chrono::Utc::now().timestamp() >= expires_at - 60 // 60s buffer
        } else {
            true
        }
    }
}

// ---------------------------------------------------------------------------
// OAuth2 authorization code flow with local redirect
// ---------------------------------------------------------------------------

/// Start the OAuth2 flow:
/// 1. Start a local HTTP server on a random port
/// 2. Return the authorization URL for the user to open in their browser
/// 3. Wait for the redirect callback with the auth code
/// 4. Exchange the code for tokens
pub fn get_auth_url(provider: &OAuthProvider) -> Result<(String, u16)> {
    // Find a free port
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| Error::Other(format!("Failed to bind local server: {}", e)))?;
    let port = listener.local_addr()
        .map_err(|e| Error::Other(format!("Failed to get port: {}", e)))?
        .port();
    drop(listener);

    let redirect_uri = format!("http://127.0.0.1:{}", port);

    let url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent",
        provider.auth_url,
        urlencoding::encode(provider.client_id),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(&provider.scopes.join(" ")),
    );

    Ok((url, port))
}

/// Listen on the given port for the OAuth2 redirect callback.
/// Returns the authorization code.
pub fn wait_for_callback(port: u16) -> Result<String> {
    let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
        .map_err(|e| Error::Other(format!("Failed to bind on port {}: {}", port, e)))?;

    log::info!("OAuth2: waiting for callback on port {}", port);

    let (mut stream, _) = listener.accept()
        .map_err(|e| Error::Other(format!("Failed to accept connection: {}", e)))?;

    let mut reader = BufReader::new(stream.try_clone()
        .map_err(|e| Error::Other(format!("Stream clone failed: {}", e)))?);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)
        .map_err(|e| Error::Other(format!("Failed to read request: {}", e)))?;

    // Parse: GET /?code=xxx&scope=yyy HTTP/1.1
    let code = request_line
        .split_whitespace()
        .nth(1) // The path
        .and_then(|path| {
            let query = path.split('?').nth(1)?;
            query.split('&')
                .find(|p| p.starts_with("code="))
                .map(|p| p.trim_start_matches("code=").to_string())
        })
        .ok_or_else(|| {
            // Check for error
            let error = request_line
                .split_whitespace()
                .nth(1)
                .and_then(|path| path.split('?').nth(1))
                .and_then(|q| q.split('&').find(|p| p.starts_with("error=")))
                .map(|p| p.trim_start_matches("error=").to_string())
                .unwrap_or_else(|| "unknown".to_string());
            Error::Other(format!("OAuth2 authorization failed: {}", error))
        })?;

    // Send a success response to the browser
    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
        <html><body style='font-family:sans-serif;text-align:center;padding:60px'>\
        <h2>Authorization successful!</h2>\
        <p>You can close this window and return to Chithi.</p>\
        </body></html>";
    stream.write_all(response.as_bytes()).ok();

    log::info!("OAuth2: received authorization code");
    Ok(code)
}

/// Exchange an authorization code for access + refresh tokens.
pub async fn exchange_code(
    provider: &OAuthProvider,
    code: &str,
    port: u16,
) -> Result<OAuthTokens> {
    let redirect_uri = format!("http://127.0.0.1:{}", port);

    let mut params = HashMap::new();
    params.insert("client_id", provider.client_id);
    params.insert("client_secret", provider.client_secret);
    params.insert("code", code);
    params.insert("redirect_uri", redirect_uri.as_str());
    params.insert("grant_type", "authorization_code");

    let client = reqwest::Client::new();
    let resp = client
        .post(provider.token_url)
        .form(&params)
        .send()
        .await
        .map_err(|e| Error::Other(format!("Token exchange failed: {}", e)))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Other(format!("Token exchange error: {}", body)));
    }

    let token_resp: serde_json::Value = resp.json().await
        .map_err(|e| Error::Other(format!("Token parse failed: {}", e)))?;

    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| Error::Other("No access_token in response".into()))?
        .to_string();

    let refresh_token = token_resp["refresh_token"]
        .as_str()
        .map(|s| s.to_string());

    let expires_in = token_resp["expires_in"].as_i64().unwrap_or(3600);
    let expires_at = chrono::Utc::now().timestamp() + expires_in;

    log::info!("OAuth2: token exchange successful, expires in {}s", expires_in);

    Ok(OAuthTokens {
        access_token,
        refresh_token,
        expires_at: Some(expires_at),
    })
}

/// Refresh an expired access token using a refresh token.
pub async fn refresh_access_token(
    provider: &OAuthProvider,
    refresh_token: &str,
) -> Result<OAuthTokens> {
    let mut params = HashMap::new();
    params.insert("client_id", provider.client_id);
    params.insert("client_secret", provider.client_secret);
    params.insert("refresh_token", refresh_token);
    params.insert("grant_type", "refresh_token");

    let client = reqwest::Client::new();
    let resp = client
        .post(provider.token_url)
        .form(&params)
        .send()
        .await
        .map_err(|e| Error::Other(format!("Token refresh failed: {}", e)))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Other(format!("Token refresh error: {}", body)));
    }

    let token_resp: serde_json::Value = resp.json().await
        .map_err(|e| Error::Other(format!("Token refresh parse failed: {}", e)))?;

    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| Error::Other("No access_token in refresh response".into()))?
        .to_string();

    let expires_in = token_resp["expires_in"].as_i64().unwrap_or(3600);
    let expires_at = chrono::Utc::now().timestamp() + expires_in;

    log::info!("OAuth2: token refreshed, expires in {}s", expires_in);

    Ok(OAuthTokens {
        access_token,
        refresh_token: Some(refresh_token.to_string()),
        expires_at: Some(expires_at),
    })
}

// ---------------------------------------------------------------------------
// Keyring storage for OAuth tokens
// ---------------------------------------------------------------------------

const KEYRING_SERVICE: &str = "in.kushaldas.chithi.oauth";

pub fn store_tokens(account_id: &str, tokens: &OAuthTokens) -> Result<()> {
    let json = serde_json::to_string(tokens)
        .map_err(|e| Error::Other(format!("Token serialize failed: {}", e)))?;
    let entry = keyring::Entry::new(KEYRING_SERVICE, account_id)
        .map_err(|e| Error::Keyring(format!("Failed to create keyring entry: {}", e)))?;
    entry.set_password(&json)
        .map_err(|e| Error::Keyring(format!("Failed to store tokens: {}", e)))?;
    log::info!("OAuth2: tokens stored in keyring for account {}", account_id);
    Ok(())
}

pub fn load_tokens(account_id: &str) -> Result<Option<OAuthTokens>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, account_id)
        .map_err(|e| Error::Keyring(format!("Failed to create keyring entry: {}", e)))?;
    match entry.get_password() {
        Ok(json) => {
            let tokens: OAuthTokens = serde_json::from_str(&json)
                .map_err(|e| Error::Other(format!("Token deserialize failed: {}", e)))?;
            Ok(Some(tokens))
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => {
            log::warn!("OAuth2: keyring read failed for {}: {}", account_id, e);
            Ok(None)
        }
    }
}

pub fn delete_tokens(account_id: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, account_id)
        .map_err(|e| Error::Keyring(format!("Failed to create keyring entry: {}", e)))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(Error::Keyring(format!("Failed to delete tokens: {}", e))),
    }
}
