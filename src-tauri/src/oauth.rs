use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
    /// Use PKCE (required for Microsoft public clients)
    pub use_pkce: bool,
}

pub const GOOGLE: OAuthProvider = OAuthProvider {
    name: "google",
    client_id: "96507156934-tb0mgeovj7dhpaabjc4ipm5lukhmebmg.apps.googleusercontent.com",
    client_secret: "", // Desktop app — public client, uses PKCE instead
    auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    scopes: &[
        "https://www.googleapis.com/auth/calendar",
        "https://www.googleapis.com/auth/contacts",
    ],
    use_pkce: true,
};

pub const MICROSOFT: OAuthProvider = OAuthProvider {
    name: "microsoft",
    client_id: "b5941cd4-0385-40f1-953a-2c3b36f2a331",
    client_secret: "", // Public client — no secret
    auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
    token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
    // Request all scopes during authorization for consent.
    // IMAP/SMTP use outlook.office.com (not office365.com) for personal accounts.
    // Graph scopes use short form (resolved to graph.microsoft.com automatically).
    scopes: &[
        "https://outlook.office.com/IMAP.AccessAsUser.All",
        "https://outlook.office.com/SMTP.Send",
        "offline_access",
        "openid",
        "profile",
        "email",
    ],
    use_pkce: true,
};

/// Microsoft Graph scopes — used for a separate token refresh for calendar/contacts.
pub const MICROSOFT_GRAPH_SCOPES: &str = "User.Read Calendars.ReadWrite Contacts.ReadWrite offline_access";

/// Microsoft IMAP/SMTP scopes — used for token refresh for mail access.
/// Uses outlook.office.com (works for both personal and work/school accounts).
pub const MICROSOFT_IMAP_SCOPES: &str = "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send offline_access";

// ---------------------------------------------------------------------------
// PKCE support
// ---------------------------------------------------------------------------

/// Generate a PKCE code verifier (43-128 chars, base64url)
pub fn generate_code_verifier() -> String {
    use rand::Rng;
    let random_bytes: Vec<u8> = (0..64).map(|_| rand::rng().random::<u8>()).collect();
    base64url_encode(&random_bytes)
}

/// Compute the PKCE code challenge from a verifier: BASE64URL(SHA256(verifier))
pub fn compute_code_challenge(verifier: &str) -> String {
    let hash = Sha256::digest(verifier.as_bytes());
    base64url_encode(&hash)
}

fn base64url_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

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

/// Build the OAuth2 authorization URL with a local redirect server.
/// Returns `(url, port, code_verifier)` where code_verifier is present for PKCE providers.
pub fn get_auth_url(provider: &OAuthProvider) -> Result<(String, u16, Option<String>)> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| Error::Other(format!("Failed to bind local server: {}", e)))?;
    let port = listener.local_addr()
        .map_err(|e| Error::Other(format!("Failed to get port: {}", e)))?
        .port();
    drop(listener);

    // Microsoft requires http://localhost (not 127.0.0.1) for native client redirect.
    // Google works with either. Use localhost for both.
    let redirect_uri = format!("http://localhost:{}", port);

    let mut url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope={}",
        provider.auth_url,
        urlencoding::encode(provider.client_id),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(&provider.scopes.join(" ")),
    );

    let code_verifier = if provider.use_pkce {
        let verifier = generate_code_verifier();
        let challenge = compute_code_challenge(&verifier);
        url.push_str(&format!(
            "&code_challenge={}&code_challenge_method=S256",
            urlencoding::encode(&challenge)
        ));
        Some(verifier)
    } else {
        // Google uses access_type=offline&prompt=consent instead of PKCE
        url.push_str("&access_type=offline&prompt=consent");
        None
    };

    // Microsoft needs prompt=consent for first-time consent
    if provider.name == "microsoft" {
        url.push_str("&prompt=consent");
    }

    Ok((url, port, code_verifier))
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
    code_verifier: Option<&str>,
) -> Result<OAuthTokens> {
    // Microsoft requires http://localhost (not 127.0.0.1) for native client redirect.
    // Google works with either. Use localhost for both.
    let redirect_uri = format!("http://localhost:{}", port);

    let mut params = HashMap::new();
    params.insert("client_id", provider.client_id.to_string());
    params.insert("code", code.to_string());
    params.insert("redirect_uri", redirect_uri);
    params.insert("grant_type", "authorization_code".to_string());

    if let Some(verifier) = code_verifier {
        // PKCE flow (Microsoft) — no client_secret, use code_verifier
        params.insert("code_verifier", verifier.to_string());
    } else if !provider.client_secret.is_empty() {
        // Traditional flow (Google) — use client_secret
        params.insert("client_secret", provider.client_secret.to_string());
    }

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
    params.insert("client_id", provider.client_id.to_string());
    params.insert("refresh_token", refresh_token.to_string());
    params.insert("grant_type", "refresh_token".to_string());

    if !provider.client_secret.is_empty() {
        params.insert("client_secret", provider.client_secret.to_string());
    }

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

    // Microsoft may rotate the refresh token — use the new one if provided
    let new_refresh = token_resp["refresh_token"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| refresh_token.to_string());

    Ok(OAuthTokens {
        access_token,
        refresh_token: Some(new_refresh),
        expires_at: Some(expires_at),
    })
}

/// Refresh an access token with specific scopes (for multi-resource tokens like Microsoft).
/// The same refresh token can get tokens for different resources by specifying different scopes.
pub async fn refresh_with_scopes(
    provider: &OAuthProvider,
    refresh_token: &str,
    scopes: &str,
) -> Result<OAuthTokens> {
    let mut params = HashMap::new();
    params.insert("client_id", provider.client_id.to_string());
    params.insert("refresh_token", refresh_token.to_string());
    params.insert("grant_type", "refresh_token".to_string());
    params.insert("scope", scopes.to_string());

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

    // Microsoft may rotate the refresh token
    let new_refresh = token_resp["refresh_token"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| refresh_token.to_string());

    log::info!("OAuth2: token refreshed with scopes, expires in {}s", expires_in);

    Ok(OAuthTokens {
        access_token,
        refresh_token: Some(new_refresh),
        expires_at: Some(expires_at),
    })
}

// ---------------------------------------------------------------------------
// OIDC discovery for JMAP
// ---------------------------------------------------------------------------

/// Discovered OIDC endpoints from .well-known/openid-configuration.
pub struct OidcEndpoints {
    pub token_endpoint: String,
    pub device_authorization_endpoint: Option<String>,
    pub registration_endpoint: Option<String>,
}

/// Discover OIDC endpoints from a JMAP server's .well-known/openid-configuration.
/// `base_url` should be like "https://mail.example.com".
pub async fn discover_oidc(base_url: &str) -> Result<OidcEndpoints> {
    let url = format!("{}/.well-known/openid-configuration", base_url.trim_end_matches('/'));
    log::info!("OIDC: discovering endpoints from {}", url);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| Error::Other(format!("HTTP client build error: {}", e)))?;

    let resp = client.get(&url).send().await
        .map_err(|e| Error::Other(format!("OIDC discovery failed: {}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status();
        return Err(Error::Other(format!(
            "OIDC discovery returned {}: server may not support OIDC", status
        )));
    }

    let config: serde_json::Value = resp.json().await
        .map_err(|e| Error::Other(format!("OIDC discovery parse error: {}", e)))?;

    let token_endpoint = config["token_endpoint"]
        .as_str()
        .ok_or_else(|| Error::Other("OIDC: no token_endpoint in discovery".into()))?
        .to_string();

    let device_authorization_endpoint = config["device_authorization_endpoint"]
        .as_str()
        .map(|s| s.to_string());

    let registration_endpoint = config["registration_endpoint"]
        .as_str()
        .map(|s| s.to_string());

    log::info!("OIDC: discovered token={}, device_auth={:?}, registration={:?}",
        token_endpoint, device_authorization_endpoint, registration_endpoint);

    Ok(OidcEndpoints {
        token_endpoint,
        device_authorization_endpoint,
        registration_endpoint,
    })
}

/// Register a client dynamically via RFC 7591.
/// Returns the assigned `client_id`.
pub async fn register_oidc_client(registration_endpoint: &str) -> Result<String> {
    let body = serde_json::json!({
        "client_name": "Chithi Mail",
        "redirect_uris": [],
        "grant_types": ["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"],
        "response_types": [],
        "token_endpoint_auth_method": "none",
    });

    log::info!("OIDC: registering client at {}", registration_endpoint);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| Error::Other(format!("HTTP client error: {}", e)))?;

    let resp = client
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Other(format!("OIDC client registration failed: {}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let resp_body = resp.text().await.unwrap_or_default();
        return Err(Error::Other(format!(
            "OIDC client registration returned {}: {}", status, resp_body
        )));
    }

    let reg_resp: serde_json::Value = resp.json().await
        .map_err(|e| Error::Other(format!("OIDC registration parse error: {}", e)))?;

    let client_id = reg_resp["client_id"]
        .as_str()
        .ok_or_else(|| Error::Other("OIDC: no client_id in registration response".into()))?
        .to_string();

    log::info!("OIDC: registered client_id={}", client_id);

    Ok(client_id)
}

// ---------------------------------------------------------------------------
// OAuth2 Device Authorization Grant (RFC 8628) for JMAP OIDC
// ---------------------------------------------------------------------------

/// Response from the device authorization endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceAuthResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    /// Polling interval in seconds (default 5).
    #[serde(default = "default_interval")]
    pub interval: u64,
    /// Lifetime of the device code in seconds.
    #[serde(default = "default_expires_in")]
    pub expires_in: u64,
}

fn default_interval() -> u64 { 5 }
fn default_expires_in() -> u64 { 600 }

/// Start the device authorization flow — POST to the device authorization endpoint.
/// Returns the device code, user code, and verification URL to show to the user.
pub async fn device_auth_start(
    device_auth_endpoint: &str,
    client_id: &str,
) -> Result<DeviceAuthResponse> {
    if client_id.trim().is_empty() {
        return Err(Error::Other("Device authorization requires a client_id".into()));
    }

    log::info!("OIDC device flow: requesting device code from {}", device_auth_endpoint);

    let mut params = HashMap::new();
    params.insert("client_id", client_id.to_string());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| Error::Other(format!("HTTP client error: {}", e)))?;

    let resp = client
        .post(device_auth_endpoint)
        .form(&params)
        .send()
        .await
        .map_err(|e| Error::Other(format!("Device auth request failed: {}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Other(format!(
            "Device auth endpoint returned {}: {}", status, body
        )));
    }

    let auth_resp: DeviceAuthResponse = resp.json().await
        .map_err(|e| Error::Other(format!("Device auth response parse error: {}", e)))?;

    log::info!("OIDC device flow: received user_code, verification_uri={}",
        auth_resp.verification_uri);

    Ok(auth_resp)
}

/// Poll the token endpoint until the user completes device authorization.
/// Returns tokens on success, or errors on expiry/denial.
pub async fn device_auth_poll(
    token_endpoint: &str,
    device_code: &str,
    interval: u64,
    expires_in: u64,
    client_id: &str,
) -> Result<OAuthTokens> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Other(format!("HTTP client error: {}", e)))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(expires_in);
    let mut current_interval = std::time::Duration::from_secs(interval);
    let mut first_poll = true;

    loop {
        if std::time::Instant::now() >= deadline {
            return Err(Error::Other("Device authorization timed out — user did not complete sign-in".into()));
        }

        // Sleep before polling (skip on first attempt per RFC 8628 §3.5)
        if first_poll {
            first_poll = false;
        } else {
            tokio::time::sleep(current_interval).await;
        }

        let mut params = HashMap::new();
        params.insert("grant_type", "urn:ietf:params:oauth:grant-type:device_code".to_string());
        params.insert("device_code", device_code.to_string());
        if !client_id.is_empty() {
            params.insert("client_id", client_id.to_string());
        }

        let resp = client
            .post(token_endpoint)
            .form(&params)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Device token poll failed: {}", e)))?;

        if resp.status().is_success() {
            let token_resp: serde_json::Value = resp.json().await
                .map_err(|e| Error::Other(format!("Device token parse failed: {}", e)))?;

            let access_token = token_resp["access_token"]
                .as_str()
                .ok_or_else(|| Error::Other("No access_token in device token response".into()))?
                .to_string();

            let refresh_token = token_resp["refresh_token"]
                .as_str()
                .map(|s| s.to_string());

            let expires_in = token_resp["expires_in"].as_i64().unwrap_or(3600);
            let expires_at = chrono::Utc::now().timestamp() + expires_in;

            log::info!("OIDC device flow: authorization complete, expires in {}s", expires_in);

            return Ok(OAuthTokens {
                access_token,
                refresh_token,
                expires_at: Some(expires_at),
            });
        }

        // Check error response
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let error = body["error"].as_str().unwrap_or("");

        match error {
            "authorization_pending" => {
                log::debug!("OIDC device flow: authorization pending, polling again...");
                continue;
            }
            "slow_down" => {
                // RFC 8628 §3.5: increase interval by 5 seconds
                current_interval += std::time::Duration::from_secs(5);
                log::debug!("OIDC device flow: slow_down, interval now {}s", current_interval.as_secs());
                continue;
            }
            "access_denied" => {
                return Err(Error::Other("Device authorization denied by user".into()));
            }
            "expired_token" => {
                return Err(Error::Other("Device code expired — please try again".into()));
            }
            _ => {
                let desc = body["error_description"].as_str().unwrap_or("");
                return Err(Error::Other(format!("Device auth error: {} {}", error, desc)));
            }
        }
    }
}

/// Refresh an access token using a dynamically discovered token endpoint.
/// Used for JMAP OIDC where there is no static OAuthProvider.
pub async fn refresh_token_dynamic(
    token_url: &str,
    refresh_token: &str,
    client_id: &str,
) -> Result<OAuthTokens> {
    let mut params = HashMap::new();
    params.insert("client_id", client_id.to_string());
    params.insert("refresh_token", refresh_token.to_string());
    params.insert("grant_type", "refresh_token".to_string());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Other(format!("HTTP client error: {}", e)))?;
    let resp = client
        .post(token_url)
        .form(&params)
        .send()
        .await
        .map_err(|e| Error::Other(format!("OIDC token refresh failed: {}", e)))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Other(format!("OIDC token refresh error: {}", body)));
    }

    let token_resp: serde_json::Value = resp.json().await
        .map_err(|e| Error::Other(format!("OIDC token refresh parse failed: {}", e)))?;

    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| Error::Other("No access_token in OIDC refresh response".into()))?
        .to_string();

    let expires_in = token_resp["expires_in"].as_i64().unwrap_or(3600);
    let expires_at = chrono::Utc::now().timestamp() + expires_in;

    // IdP may rotate the refresh token
    let new_refresh = token_resp["refresh_token"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| refresh_token.to_string());

    log::info!("OIDC: token refreshed, expires in {}s", expires_in);

    Ok(OAuthTokens {
        access_token,
        refresh_token: Some(new_refresh),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pkce_verifier_is_valid_length() {
        let verifier = generate_code_verifier();
        // RFC 7636: 43-128 characters
        assert!(verifier.len() >= 43, "verifier too short: {}", verifier.len());
        assert!(verifier.len() <= 128, "verifier too long: {}", verifier.len());
    }

    #[test]
    fn test_pkce_verifier_is_base64url() {
        let verifier = generate_code_verifier();
        // base64url chars: A-Z, a-z, 0-9, -, _
        for c in verifier.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "invalid char in verifier: '{}'", c
            );
        }
    }

    #[test]
    fn test_pkce_challenge_differs_from_verifier() {
        let verifier = generate_code_verifier();
        let challenge = compute_code_challenge(&verifier);
        assert_ne!(verifier, challenge);
    }

    #[test]
    fn test_pkce_challenge_is_deterministic() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let c1 = compute_code_challenge(verifier);
        let c2 = compute_code_challenge(verifier);
        assert_eq!(c1, c2);
    }

    #[test]
    fn test_pkce_challenge_is_base64url() {
        let verifier = generate_code_verifier();
        let challenge = compute_code_challenge(&verifier);
        for c in challenge.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "invalid char in challenge: '{}'", c
            );
        }
        // No padding
        assert!(!challenge.contains('='));
    }

    #[test]
    fn test_microsoft_provider_config() {
        assert_eq!(MICROSOFT.name, "microsoft");
        assert!(MICROSOFT.use_pkce);
        assert!(MICROSOFT.client_secret.is_empty());
        assert!(MICROSOFT.auth_url.contains("login.microsoftonline.com"));
        assert!(MICROSOFT.token_url.contains("login.microsoftonline.com"));
    }

    #[test]
    fn test_google_provider_pkce() {
        assert_eq!(GOOGLE.name, "google");
        assert!(GOOGLE.use_pkce);
        assert!(GOOGLE.client_secret.is_empty());
    }

    #[test]
    fn test_token_expiry_check() {
        let expired = OAuthTokens {
            access_token: "test".to_string(),
            refresh_token: None,
            expires_at: Some(0), // epoch = definitely expired
        };
        assert!(expired.is_expired());

        let fresh = OAuthTokens {
            access_token: "test".to_string(),
            refresh_token: None,
            expires_at: Some(chrono::Utc::now().timestamp() + 3600),
        };
        assert!(!fresh.is_expired());

        let no_expiry = OAuthTokens {
            access_token: "test".to_string(),
            refresh_token: None,
            expires_at: None,
        };
        assert!(no_expiry.is_expired()); // No expiry = treat as expired
    }

    #[test]
    fn test_imap_scopes_use_outlook_office_com() {
        assert!(MICROSOFT_IMAP_SCOPES.contains("outlook.office.com"));
        assert!(MICROSOFT_IMAP_SCOPES.contains("IMAP.AccessAsUser.All"));
        assert!(MICROSOFT_IMAP_SCOPES.contains("SMTP.Send"));
    }

    #[test]
    fn test_graph_scopes_use_graph_microsoft_com() {
        assert!(MICROSOFT_GRAPH_SCOPES.contains("User.Read"));
        assert!(MICROSOFT_GRAPH_SCOPES.contains("Calendars.ReadWrite"));
        assert!(MICROSOFT_GRAPH_SCOPES.contains("Contacts.ReadWrite"));
    }

    #[test]
    fn test_device_auth_response_defaults() {
        let json = r#"{"device_code":"dc","user_code":"UC","verification_uri":"https://example.com/device"}"#;
        let resp: DeviceAuthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.device_code, "dc");
        assert_eq!(resp.user_code, "UC");
        assert_eq!(resp.verification_uri, "https://example.com/device");
        assert_eq!(resp.interval, 5); // default
        assert_eq!(resp.expires_in, 600); // default
        assert!(resp.verification_uri_complete.is_none());
    }
}
