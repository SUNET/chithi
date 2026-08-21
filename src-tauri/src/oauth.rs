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
    /// Scope sent in the `authorization_code` -> token exchange request
    /// ([`exchange_code_with_client`]), when the provider requires one.
    ///
    /// Microsoft's v2.0 token endpoint rejects a code redemption that
    /// omits `scope` with AADSTS70011 ("must include a 'scope' input
    /// parameter") whenever the authorization request spanned more than
    /// one resource — and `MICROSOFT.scopes` does (Graph +
    /// outlook.office.com). The redemption `scope` must name a SINGLE
    /// resource, so it can't just be `scopes.join(" ")`; Microsoft uses
    /// the Graph subset here. `None` (Google, Zoom) omits `scope` at
    /// exchange — those endpoints derive it from the code.
    pub token_exchange_scope: Option<&'static str>,
    /// Use PKCE (required for Microsoft public clients)
    pub use_pkce: bool,
    /// Loopback host the provider expects in the redirect URI.
    /// Microsoft requires `localhost`; Google accepts either;
    /// Zoom registers `http://127.0.0.1` and rejects `localhost`.
    /// Defaults to `"localhost"` for legacy providers.
    pub redirect_host: &'static str,
    /// Fixed loopback port the provider's redirect URI uses, when
    /// the provider doesn't honor RFC 8252's "ignore the port on
    /// loopback" rule and requires an exact match against what's
    /// registered. `None` lets `get_auth_url` bind a random free
    /// port (Google / Microsoft). Zoom Marketplace pins
    /// `http://127.0.0.1:<port>` exactly so we set this for Zoom.
    pub redirect_fixed_port: Option<u16>,
    /// Optional full-URL redirect that overrides the loopback
    /// `http://<host>:<port>` we'd otherwise build from
    /// `redirect_host` / `redirect_fixed_port`. Used when the
    /// provider's production policy refuses loopback redirects
    /// outright (Zoom production OAuth) and forces us to register
    /// an HTTPS bounce. The bounce is a static page on
    /// `chithi.org` whose only job is to forward the OAuth
    /// `code` + `state` query string to the loopback listener
    /// Chithi binds locally — so the listener still runs at
    /// `redirect_host:redirect_fixed_port`, but Zoom only ever
    /// sees the override URL.
    ///
    /// `None` keeps the legacy direct-loopback behaviour (Google,
    /// Microsoft, dev-mode Zoom builds without the bounce).
    pub redirect_url_override: Option<&'static str>,
}

pub const GOOGLE: OAuthProvider = OAuthProvider {
    name: "google",
    client_id: "96507156934-tb0mgeovj7dhpaabjc4ipm5lukhmebmg.apps.googleusercontent.com",
    client_secret: "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", // Desktop app — not truly confidential per Google docs
    auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    scopes: &[
        "https://www.googleapis.com/auth/calendar",
        "https://www.googleapis.com/auth/contacts",
    ],
    token_exchange_scope: None,
    use_pkce: true,
    redirect_host: "localhost",
    redirect_fixed_port: None,
    redirect_url_override: None,
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
    // Every Graph scope here must appear in MICROSOFT_GRAPH_SCOPES *or*
    // MICROSOFT_GRAPH_ROOM_SCOPES, or the corresponding token refresh fails
    // with AADSTS65001 (consent_required). `Place.Read.All` is deliberately
    // kept out of the baseline MICROSOFT_GRAPH_SCOPES — see that constant.
    scopes: &[
        "https://outlook.office.com/IMAP.AccessAsUser.All",
        "https://outlook.office.com/SMTP.Send",
        "User.Read",
        "Mail.ReadWrite",
        "Calendars.ReadWrite",
        "Contacts.ReadWrite",
        "Place.Read.All",
        "offline_access",
        "openid",
        "profile",
        "email",
    ],
    // Code redemption must name a single resource — see the field doc
    // on `OAuthProvider::token_exchange_scope`. The Graph subset is what
    // the post-sign-in profile fetch needs first, and `offline_access`
    // in it yields the (resource-independent) refresh token.
    token_exchange_scope: Some(MICROSOFT_GRAPH_SCOPES),
    use_pkce: true,
    redirect_host: "localhost",
    redirect_fixed_port: None,
    redirect_url_override: None,
};

/// Microsoft Graph scopes — used for a separate token refresh for calendar/contacts.
///
/// Deliberately excludes `Place.Read.All`: accounts that signed in before
/// room support was added never consented to it, so requesting it on every
/// `get_graph_token()` refresh would fail those accounts with consent_required
/// (AADSTS65001) and break mail/calendar/contacts sync even when the user
/// never touches room suggestions. Room features use
/// [`MICROSOFT_GRAPH_ROOM_SCOPES`] via a separate, room-specific token
/// request that is allowed to fail gracefully.
pub const MICROSOFT_GRAPH_SCOPES: &str =
    "User.Read Mail.ReadWrite Calendars.ReadWrite Contacts.ReadWrite offline_access";

/// Graph scopes for room discovery/availability — the baseline scopes plus
/// `Place.Read.All`. Requested only by the room-specific token path; a
/// refresh failure here means rooms are unavailable for that account (an
/// account that predates room support and never consented), and callers
/// must treat that as a soft failure rather than propagating it.
pub const MICROSOFT_GRAPH_ROOM_SCOPES: &str =
    "User.Read Mail.ReadWrite Calendars.ReadWrite Contacts.ReadWrite Place.Read.All offline_access";

/// Zoom OAuth (#148, video conferencing). Native app with PKCE —
/// no client_secret ships in the binary. Registered on Zoom
/// Marketplace as a "User-managed app" with redirect URI
/// **`http://127.0.0.1:47832` exactly** — Zoom does *not* honor
/// RFC 8252's "ignore the port on loopback" rule, so the
/// registered URI must match the runtime URI character-for-
/// character (port included). That's why `redirect_fixed_port`
/// is set: `get_auth_url` binds 47832 specifically. If the port
/// is held by another process the bind errors out with a clear
/// message and the user retries.
///
/// `localhost` callbacks are rejected by Zoom regardless, hence
/// `redirect_host` is `127.0.0.1` while Microsoft demands
/// `localhost`.
///
/// Scopes are Zoom's granular set:
/// - `meeting:write:meeting` for `POST /v2/users/me/meetings` (create)
/// - `meeting:update:meeting` for `PATCH /v2/meetings/{id}` (reschedule on event move)
/// - `meeting:delete:meeting` for `DELETE /v2/meetings/{id}` (cancel on event delete)
/// - `user:read:user` for `GET /v2/users/me` (bind reauthentication to the
///   original Zoom principal)
///
/// The meeting scopes and user-profile scope must be checked under
/// Marketplace, Scopes on the registered app. Adding a scope after
/// the app is already
/// published forces existing users to re-authorize; without that,
/// the access token keeps the old narrower scope set and the
/// PATCH/DELETE calls 401.
pub const ZOOM: OAuthProvider = OAuthProvider {
    name: "zoom",
    // Build-time override via `CHITHI_ZOOM_CLIENT_ID`. The baked-
    // in default is a free-tier Marketplace registration without
    // admin-approval gating, so out-of-the-box `cargo run` works
    // without an env var. Forks shipping under their own Zoom
    // organisation register their own app and pass that through
    // the env at build time.
    client_id: match option_env!("CHITHI_ZOOM_CLIENT_ID") {
        Some(s) => s,
        None => "VOqJx9G3Q1mM80wIAmlFNw",
    },
    client_secret: "", // Public client — PKCE only
    auth_url: "https://zoom.us/oauth/authorize",
    token_url: "https://zoom.us/oauth/token",
    scopes: &[
        "meeting:write:meeting",
        "meeting:update:meeting",
        "meeting:delete:meeting",
        "user:read:user",
    ],
    token_exchange_scope: None,
    use_pkce: true,
    redirect_host: "127.0.0.1",
    // Pinned port that the local listener binds to. The bounce
    // at chithi.org/oauth/zoom/ forwards back here — see
    // `redirect_url_override` below. If the port is held by
    // another program at runtime the bind fails with a clear
    // error and the user retries.
    redirect_fixed_port: Some(47832),
    // Production redirect URI registered on Zoom Marketplace.
    // Zoom production OAuth refuses loopback URLs entirely, so
    // the user-visible redirect goes through the static HTTPS
    // bounce hosted at `web/oauth/zoom/index.html` on
    // chithi.org, which JS-redirects back to the loopback
    // listener bound by `redirect_fixed_port` above.
    //
    // Build-time override `CHITHI_ZOOM_REDIRECT_URI` lets forks
    // point at their own bounce (or a `http://127.0.0.1:47832`
    // dev-mode app registered directly on Marketplace, sidestepping
    // the bounce). Note: const matching on `&str` isn't stable, so
    // there's no "set this to empty to disable the override" path —
    // pass an explicit URL.
    redirect_url_override: Some(match option_env!("CHITHI_ZOOM_REDIRECT_URI") {
        Some(s) => s,
        None => "https://chithi.org/oauth/zoom",
    }),
};

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
/// Returns (url, listener, code_verifier, state).
///
/// The listener is kept open to prevent port hijacking between URL
/// generation and callback handling (TOCTOU). The state parameter
/// provides CSRF protection and must be validated in the callback.
pub fn get_auth_url(
    provider: &OAuthProvider,
) -> Result<(String, TcpListener, Option<String>, String)> {
    // Honor the provider's pinned-port setting when present
    // (Zoom requires an exact-match redirect URI registered in
    // Marketplace). Otherwise bind a random free port — the
    // common case for Google / Microsoft.
    let bind_port = provider.redirect_fixed_port.unwrap_or(0);
    let listener = TcpListener::bind(format!("127.0.0.1:{}", bind_port))
        .map_err(|e| {
            if provider.redirect_fixed_port.is_some() {
                Error::Other(format!(
                    "{} requires loopback port {} but it's already in use ({}). Close the program holding it and retry.",
                    provider.name, bind_port, e,
                ))
            } else {
                Error::Other(format!("Failed to bind local server: {}", e))
            }
        })?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::Other(format!("Failed to get port: {}", e)))?
        .port();

    // What we send to the provider. When `redirect_url_override`
    // is set we use that verbatim — Zoom production refuses
    // loopback URIs so the registered redirect is the static
    // bounce on `chithi.org/oauth/zoom`, which forwards
    // `?code=&state=` to the listener bound just above.
    // Otherwise we build the legacy `http://<host>:<port>` form:
    // Microsoft demands `localhost`, Zoom dev mode wants
    // `127.0.0.1`, Google accepts either.
    let redirect_uri = match provider.redirect_url_override {
        Some(override_url) => override_url.to_string(),
        None => format!("http://{}:{}", provider.redirect_host, port),
    };

    // Generate a random state parameter for CSRF protection
    let state = generate_code_verifier();

    let mut url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}",
        provider.auth_url,
        urlencoding::encode(provider.client_id),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(&provider.scopes.join(" ")),
        urlencoding::encode(&state),
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
        None
    };

    // Google requires access_type=offline to return a refresh token
    if provider.name == "google" {
        url.push_str("&access_type=offline&prompt=consent");
    }

    // Microsoft: show account picker but honor existing consent (admin or user).
    if provider.name == "microsoft" {
        url.push_str("&prompt=select_account");
    }

    // Don't log `state` or `url` — both contain CSRF / PKCE material that
    // would land in the persistent log file.
    log::info!(
        "OAuth2[{}]: get_auth_url generated redirect_uri={} port={} pkce={}",
        provider.name,
        redirect_uri,
        port,
        code_verifier.is_some(),
    );

    Ok((url, listener, code_verifier, state))
}

/// Result from the OAuth2 callback, including the authorization code
/// and optional state parameter for CSRF validation.
pub struct CallbackResult {
    pub code: String,
    pub state: Option<String>,
}

enum CallbackPage {
    Success,
    AccessDenied,
    Failure,
}

fn write_callback_page(stream: &mut impl Write, page: CallbackPage) {
    let (status, title, message) = match page {
        CallbackPage::Success => (
            "200 OK",
            "Authorization successful!",
            "You can close this window and return to Chithi.",
        ),
        CallbackPage::AccessDenied => (
            "400 Bad Request",
            "Authorization not completed",
            "Access was not granted. Return to Chithi and try again.",
        ),
        CallbackPage::Failure => (
            "400 Bad Request",
            "Authorization failed",
            "The provider did not complete authorization. Return to Chithi and try again.",
        ),
    };
    let body = format!(
        "<!doctype html><html><head><meta charset='utf-8'><title>{title}</title></head>\
         <body><main><h2>{title}</h2><p>{message}</p></main></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Content-Security-Policy: default-src 'none'\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\r\n\
         {body}",
        body.len(),
    );
    // The browser may already have closed the tab. Callback processing must
    // not fail merely because the informational response cannot be written.
    stream.write_all(response.as_bytes()).ok();
}

/// Listen on the given listener for the OAuth2 redirect callback.
/// Times out after 5 minutes to prevent indefinite resource holding.
/// Validates the state parameter before reporting browser success and returns
/// the authorization code plus the validated state.
pub fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<CallbackResult> {
    use std::time::{Duration, Instant};

    let timeout = Duration::from_secs(300);
    let deadline = Instant::now() + timeout;
    listener
        .set_nonblocking(true)
        .map_err(|e| Error::Other(format!("Failed to set non-blocking: {}", e)))?;

    log::info!(
        "OAuth2: waiting for callback (timeout={}s)",
        timeout.as_secs()
    );

    let (mut stream, _) = loop {
        match listener.accept() {
            Ok(conn) => break conn,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(Error::Other(
                        "OAuth2 callback timed out after 5 minutes".into(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => {
                return Err(Error::Other(format!("Failed to accept connection: {}", e)));
            }
        }
    };

    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|e| Error::Other(format!("Stream clone failed: {}", e)))?,
    );

    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|e| Error::Other(format!("Failed to read request: {}", e)))?;

    // Parse query parameters from: GET /?code=xxx&state=yyy HTTP/1.1
    let query_params: HashMap<String, String> = request_line
        .split_whitespace()
        .nth(1)
        .and_then(|path| path.split('?').nth(1))
        .map(|query| {
            query
                .split('&')
                .filter_map(|param| {
                    let mut parts = param.splitn(2, '=');
                    let key = parts.next()?;
                    let val = parts.next().unwrap_or("");
                    // URL-decode values (code/state may be percent-encoded)
                    let decoded = urlencoding::decode(val)
                        .unwrap_or_else(|_| val.into())
                        .into_owned();
                    Some((key.to_string(), decoded))
                })
                .collect()
        })
        .unwrap_or_default();

    let state = query_params.get("state").cloned();

    // Don't log the authorization `code` or `state` — both are secrets.
    log::info!(
        "OAuth2: received callback has_state={} other_keys={:?}",
        state.is_some(),
        query_params
            .keys()
            .filter(|k| k.as_str() != "code" && k.as_str() != "state")
            .collect::<Vec<_>>(),
    );

    match state.as_deref() {
        Some(returned_state) if returned_state == expected_state => {}
        Some(_) => {
            write_callback_page(&mut stream, CallbackPage::Failure);
            return Err(Error::OAuthStateMismatch);
        }
        None => {
            write_callback_page(&mut stream, CallbackPage::Failure);
            return Err(Error::OAuthStateMissing);
        }
    }

    if let Some(error) = query_params.get("error").map(String::as_str) {
        let access_denied = error == "access_denied";
        write_callback_page(
            &mut stream,
            if access_denied {
                CallbackPage::AccessDenied
            } else {
                CallbackPage::Failure
            },
        );
        let message = if access_denied {
            "OAuth2 authorization was denied or cancelled"
        } else {
            "OAuth2 authorization failed"
        };
        return Err(Error::Other(message.into()));
    }

    let code = match query_params.get("code").filter(|code| !code.is_empty()) {
        Some(code) => code.clone(),
        None => {
            write_callback_page(&mut stream, CallbackPage::Failure);
            return Err(Error::Other(
                "OAuth2 callback did not contain an authorization code".into(),
            ));
        }
    };

    write_callback_page(&mut stream, CallbackPage::Success);
    Ok(CallbackResult { code, state })
}

/// Exchange an authorization code for access and refresh tokens using the
/// provided HTTP client.
///
/// `code_verifier` must be the verifier paired with the authorization request.
/// A non-empty provider client secret is sent in addition to the verifier.
pub async fn exchange_code_with_client(
    provider: &OAuthProvider,
    code: &str,
    port: u16,
    code_verifier: Option<&str>,
    client: &reqwest::Client,
) -> Result<OAuthTokens> {
    // Must match exactly what was sent in the auth request, or
    // the token endpoint returns `invalid_grant`. See
    // `get_auth_url` for the matching construction logic.
    let redirect_uri = match provider.redirect_url_override {
        Some(override_url) => override_url.to_string(),
        None => format!("http://{}:{}", provider.redirect_host, port),
    };

    let mut params = HashMap::new();
    params.insert("client_id", provider.client_id.to_string());
    params.insert("code", code.to_string());
    params.insert("redirect_uri", redirect_uri);
    params.insert("grant_type", "authorization_code".to_string());

    // Microsoft's v2.0 token endpoint rejects a code redemption with no
    // `scope` (AADSTS70011) when the authorize request spanned multiple
    // resources. Send the provider's single-resource exchange scope
    // when it defines one; Google/Zoom derive scope from the code.
    if let Some(scope) = provider.token_exchange_scope {
        params.insert("scope", scope.to_string());
    }

    if let Some(verifier) = code_verifier {
        params.insert("code_verifier", verifier.to_string());
    }
    // Send client_secret if present (some providers require it even with PKCE)
    if !provider.client_secret.is_empty() {
        params.insert("client_secret", provider.client_secret.to_string());
    }

    // Don't log `code` or `code_verifier` — both are single-use secrets.
    log::info!(
        "OAuth2[{}]: exchange_code POST {} client_id={} redirect_uri={} pkce={} client_secret={}",
        provider.name,
        provider.token_url,
        provider.client_id,
        params.get("redirect_uri").cloned().unwrap_or_default(),
        code_verifier.is_some(),
        if provider.client_secret.is_empty() {
            "<none>"
        } else {
            "<set>"
        },
    );

    let resp = client
        .post(provider.token_url)
        .form(&params)
        .send()
        .await
        .map_err(|e| {
            log::error!(
                "OAuth2[{}]: exchange_code transport error: {}",
                provider.name,
                e
            );
            Error::Other(format!("Token exchange failed: {}", e))
        })?;

    let status = resp.status();
    log::info!(
        "OAuth2[{}]: exchange_code response status={}",
        provider.name,
        status
    );
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        log::error!(
            "OAuth2[{}]: exchange_code error status={} body={}",
            provider.name,
            status,
            body
        );
        return Err(Error::Other(format!("Token exchange error: {}", body)));
    }

    let token_resp: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Other(format!("Token parse failed: {}", e)))?;

    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| Error::Other("No access_token in response".into()))?
        .to_string();

    let refresh_token = token_resp["refresh_token"].as_str().map(|s| s.to_string());

    let expires_in = token_resp["expires_in"].as_i64().unwrap_or(3600);
    let expires_at = chrono::Utc::now().timestamp() + expires_in;

    // Don't log access / refresh tokens — log only whether a refresh token
    // was issued and the access token's lifetime.
    log::info!(
        "OAuth2[{}]: exchange_code OK has_refresh={} expires_in={}s",
        provider.name,
        refresh_token.is_some(),
        expires_in,
    );

    Ok(OAuthTokens {
        access_token,
        refresh_token,
        expires_at: Some(expires_at),
    })
}

/// Refresh an expired access token using a refresh token and the provided HTTP
/// client.
pub async fn refresh_access_token_with_client(
    provider: &OAuthProvider,
    refresh_token: &str,
    client: &reqwest::Client,
) -> Result<OAuthTokens> {
    let mut params = HashMap::new();
    params.insert("client_id", provider.client_id.to_string());
    params.insert("refresh_token", refresh_token.to_string());
    params.insert("grant_type", "refresh_token".to_string());

    if !provider.client_secret.is_empty() {
        params.insert("client_secret", provider.client_secret.to_string());
    }

    // Don't log the refresh token — it's a long-lived credential.
    log::info!(
        "OAuth2[{}]: refresh_access_token POST {} client_id={} client_secret={}",
        provider.name,
        provider.token_url,
        provider.client_id,
        if provider.client_secret.is_empty() {
            "<none>"
        } else {
            "<set>"
        },
    );

    let resp = client
        .post(provider.token_url)
        .form(&params)
        .send()
        .await
        .map_err(|e| {
            log::error!("OAuth2[{}]: refresh transport error: {}", provider.name, e);
            Error::Other(format!("Token refresh failed: {}", e))
        })?;

    let status = resp.status();
    log::info!(
        "OAuth2[{}]: refresh response status={}",
        provider.name,
        status
    );
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        log::error!(
            "OAuth2[{}]: refresh error status={} body={}",
            provider.name,
            status,
            body
        );
        return Err(Error::Other(format!("Token refresh error: {}", body)));
    }

    let token_resp: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Other(format!("Token refresh parse failed: {}", e)))?;

    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| Error::Other("No access_token in refresh response".into()))?
        .to_string();

    let expires_in = token_resp["expires_in"].as_i64().unwrap_or(3600);
    let expires_at = chrono::Utc::now().timestamp() + expires_in;

    // Microsoft may rotate the refresh token — use the new one if provided
    let new_refresh = token_resp["refresh_token"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| refresh_token.to_string());

    let rotated = token_resp["refresh_token"].is_string();
    // Don't log tokens — log only that refresh succeeded and whether the
    // refresh token was rotated.
    log::info!(
        "OAuth2[{}]: refresh OK rotated={} expires_in={}s",
        provider.name,
        rotated,
        expires_in,
    );

    Ok(OAuthTokens {
        access_token,
        refresh_token: Some(new_refresh),
        expires_at: Some(expires_at),
    })
}

/// Refresh an access token with specific scopes using the provided HTTP client.
///
/// The same refresh token can obtain tokens for different resources by
/// specifying different scopes, as required by providers such as Microsoft.
pub async fn refresh_with_scopes_with_client(
    provider: &OAuthProvider,
    refresh_token: &str,
    scopes: &str,
    client: &reqwest::Client,
) -> Result<OAuthTokens> {
    let mut params = HashMap::new();
    params.insert("client_id", provider.client_id.to_string());
    params.insert("refresh_token", refresh_token.to_string());
    params.insert("grant_type", "refresh_token".to_string());
    params.insert("scope", scopes.to_string());

    if !provider.client_secret.is_empty() {
        params.insert("client_secret", provider.client_secret.to_string());
    }

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

    let token_resp: serde_json::Value = resp
        .json()
        .await
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

    log::info!(
        "OAuth2: token refreshed with scopes, expires in {}s",
        expires_in
    );

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

/// Discover OIDC endpoints from a JMAP server's
/// `.well-known/openid-configuration` using the provided HTTP client.
///
/// `base_url` should be like `https://mail.example.com`. Discovered token,
/// device-authorization, and registration endpoints must use HTTPS.
pub async fn discover_oidc_with_client(
    base_url: &str,
    client: &reqwest::Client,
) -> Result<OidcEndpoints> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        base_url.trim_end_matches('/')
    );
    log::info!("OIDC: discovering endpoints from {}", url);

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| Error::Other(format!("OIDC discovery failed: {}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status();
        return Err(Error::Other(format!(
            "OIDC discovery returned {}: server may not support OIDC",
            status
        )));
    }

    let config: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Other(format!("OIDC discovery parse error: {}", e)))?;

    let token_endpoint = config["token_endpoint"]
        .as_str()
        .ok_or_else(|| Error::Other("OIDC: no token_endpoint in discovery".into()))?
        .to_string();

    if !token_endpoint.starts_with("https://") {
        return Err(Error::Other(format!(
            "OIDC: token_endpoint must use HTTPS, got: {}",
            token_endpoint
        )));
    }

    let device_authorization_endpoint = config["device_authorization_endpoint"]
        .as_str()
        .map(|s| s.to_string());

    if let Some(ref ep) = device_authorization_endpoint {
        if !ep.starts_with("https://") {
            return Err(Error::Other(format!(
                "OIDC: device_authorization_endpoint must use HTTPS, got: {}",
                ep
            )));
        }
    }

    let registration_endpoint = config["registration_endpoint"]
        .as_str()
        .map(|s| s.to_string());

    if let Some(ref ep) = registration_endpoint {
        if !ep.starts_with("https://") {
            return Err(Error::Other(format!(
                "OIDC: registration_endpoint must use HTTPS, got: {}",
                ep
            )));
        }
    }

    log::info!(
        "OIDC: discovered token={}, device_auth={:?}, registration={:?}",
        token_endpoint,
        device_authorization_endpoint,
        registration_endpoint
    );

    Ok(OidcEndpoints {
        token_endpoint,
        device_authorization_endpoint,
        registration_endpoint,
    })
}

/// Register a public OIDC client dynamically via RFC 7591 using the provided
/// HTTP client.
///
/// Returns the assigned `client_id`. The registration requests device-code and
/// refresh-token grants with no token-endpoint client authentication.
pub async fn register_oidc_client_with_client(
    registration_endpoint: &str,
    client: &reqwest::Client,
) -> Result<String> {
    let body = serde_json::json!({
        "client_name": "Chithi Mail",
        "redirect_uris": [],
        "grant_types": ["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"],
        "response_types": [],
        "token_endpoint_auth_method": "none",
    });

    log::info!("OIDC: registering client at {}", registration_endpoint);

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
            "OIDC client registration returned {}: {}",
            status, resp_body
        )));
    }

    let reg_resp: serde_json::Value = resp
        .json()
        .await
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

fn default_interval() -> u64 {
    5
}
fn default_expires_in() -> u64 {
    600
}

/// Start the RFC 8628 device authorization flow using the provided HTTP client.
///
/// Posts to the device authorization endpoint and returns the device code, user
/// code, and verification URL to show to the user.
pub async fn device_auth_start_with_client(
    device_auth_endpoint: &str,
    client_id: &str,
    client: &reqwest::Client,
) -> Result<DeviceAuthResponse> {
    if client_id.trim().is_empty() {
        return Err(Error::Other(
            "Device authorization requires a client_id".into(),
        ));
    }

    log::info!(
        "OIDC device flow: requesting device code from {}",
        device_auth_endpoint
    );

    let mut params = HashMap::new();
    params.insert("client_id", client_id.to_string());

    let resp = client
        .post(device_auth_endpoint)
        .form(&params)
        .send()
        .await
        .map_err(|e| Error::Other(format!("Device auth request failed: {}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        log::error!(
            "OIDC device flow: device_auth_start error status={} body={}",
            status,
            body
        );
        return Err(Error::Other(format!(
            "Device auth endpoint returned {}: {}",
            status, body
        )));
    }

    let auth_resp: DeviceAuthResponse = resp
        .json()
        .await
        .map_err(|e| Error::Other(format!("Device auth response parse error: {}", e)))?;

    // Don't log `device_code` (single-use secret used to poll the token
    // endpoint) or `user_code` (one-time credential the user types in).
    log::info!(
        "OIDC device flow: received verification_uri={} interval={}s expires_in={}s",
        auth_resp.verification_uri,
        auth_resp.interval,
        auth_resp.expires_in,
    );

    Ok(auth_resp)
}

/// Poll the token endpoint with the provided HTTP client until the user
/// completes RFC 8628 device authorization.
///
/// Returns tokens on success and errors on expiry or denial. Transient transport
/// errors are retried until the device-code deadline, and `slow_down` responses
/// increase the polling interval as required by RFC 8628.
pub async fn device_auth_poll_with_client(
    token_endpoint: &str,
    device_code: &str,
    interval: u64,
    expires_in: u64,
    client_id: &str,
    client: &reqwest::Client,
) -> Result<OAuthTokens> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(expires_in);
    let mut current_interval = std::time::Duration::from_secs(interval);
    let mut first_poll = true;

    loop {
        if std::time::Instant::now() >= deadline {
            return Err(Error::Other(
                "Device authorization timed out — user did not complete sign-in".into(),
            ));
        }

        // Sleep before polling (skip on first attempt per RFC 8628 §3.5)
        if first_poll {
            first_poll = false;
        } else {
            tokio::time::sleep(current_interval).await;
        }

        let mut params = HashMap::new();
        params.insert(
            "grant_type",
            "urn:ietf:params:oauth:grant-type:device_code".to_string(),
        );
        params.insert("device_code", device_code.to_string());
        if !client_id.is_empty() {
            params.insert("client_id", client_id.to_string());
        }

        let resp = match client.post(token_endpoint).form(&params).send().await {
            Ok(r) => r,
            Err(e) => {
                // `.send()` only returns network/body errors — no HTTP status
                // errors (those surface via `resp.status()` below). On mobile
                // these are virtually always transient: the app gets
                // backgrounded while the user is authorizing in Safari /
                // Custom Tabs and the in-flight socket gets torn down by the
                // OS. Keep polling until the overall device-code deadline.
                log::info!(
                    "OIDC device flow: poll send error ({e}) [timeout={} connect={} request={} body={} decode={}], retrying",
                    e.is_timeout(),
                    e.is_connect(),
                    e.is_request(),
                    e.is_body(),
                    e.is_decode(),
                );
                continue;
            }
        };

        if resp.status().is_success() {
            let token_resp: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| Error::Other(format!("Device token parse failed: {}", e)))?;

            let access_token = token_resp["access_token"]
                .as_str()
                .ok_or_else(|| Error::Other("No access_token in device token response".into()))?
                .to_string();

            let refresh_token = token_resp["refresh_token"].as_str().map(|s| s.to_string());

            let expires_in = token_resp["expires_in"].as_i64().unwrap_or(3600);
            let expires_at = chrono::Utc::now().timestamp() + expires_in;

            // Don't log access / refresh tokens.
            log::info!(
                "OIDC device flow: authorization complete has_refresh={} expires_in={}s expires_at={}",
                refresh_token.is_some(),
                expires_in,
                expires_at,
            );

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
                log::debug!(
                    "OIDC device flow: slow_down, interval now {}s",
                    current_interval.as_secs()
                );
                continue;
            }
            "access_denied" => {
                return Err(Error::Other("Device authorization denied by user".into()));
            }
            "expired_token" => {
                return Err(Error::Other(
                    "Device code expired — please try again".into(),
                ));
            }
            _ => {
                let desc = body["error_description"].as_str().unwrap_or("");
                return Err(Error::Other(format!(
                    "Device auth error: {} {}",
                    error, desc
                )));
            }
        }
    }
}

/// Refresh an access token at a dynamically discovered token endpoint using the
/// provided HTTP client.
///
/// Used for JMAP OIDC, where there is no static [`OAuthProvider`].
pub async fn refresh_token_dynamic_with_client(
    token_url: &str,
    refresh_token: &str,
    client_id: &str,
    client: &reqwest::Client,
) -> Result<OAuthTokens> {
    let mut params = HashMap::new();
    params.insert("client_id", client_id.to_string());
    params.insert("refresh_token", refresh_token.to_string());
    params.insert("grant_type", "refresh_token".to_string());

    // Don't log the refresh token.
    log::info!(
        "OIDC: refresh_token_dynamic POST {} client_id={}",
        token_url,
        client_id,
    );

    let resp = client
        .post(token_url)
        .form(&params)
        .send()
        .await
        .map_err(|e| {
            log::error!("OIDC: refresh transport error: {}", e);
            Error::Other(format!("OIDC token refresh failed: {}", e))
        })?;

    let status = resp.status();
    log::info!("OIDC: refresh response status={}", status);
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        log::error!(
            "OIDC: refresh error status={} token_url={} client_id={} body={}",
            status,
            token_url,
            client_id,
            body,
        );
        return Err(Error::Other(format!("OIDC token refresh error: {}", body)));
    }

    let token_resp: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Other(format!("OIDC token refresh parse failed: {}", e)))?;

    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| Error::Other("No access_token in OIDC refresh response".into()))?
        .to_string();

    let expires_in = token_resp["expires_in"].as_i64().unwrap_or(3600);
    let expires_at = chrono::Utc::now().timestamp() + expires_in;

    // IdP may rotate the refresh token
    let rotated = token_resp["refresh_token"].is_string();
    let new_refresh = token_resp["refresh_token"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| refresh_token.to_string());

    // Don't log access / refresh tokens.
    log::info!(
        "OIDC: refresh OK rotated={} expires_in={}s",
        rotated,
        expires_in,
    );

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

// Android note: the `keyring` crate v3 has no Android backend and silently
// falls back to an in-memory mock that doesn't persist across `Entry::new`
// calls. Every read returns NoEntry, which breaks the temp→real account-id
// migration in `commands::accounts::add_account`. Until we wire a real
// EncryptedSharedPreferences-backed credential store via JNI, fall back to
// a JSON file under `app_data_dir/oauth_tokens/`. The Android app sandbox
// scopes that directory to this UID.
#[cfg(target_os = "android")]
mod android_store {
    use super::{Error, OAuthTokens, Result};
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    static TOKEN_DIR: OnceLock<PathBuf> = OnceLock::new();

    pub fn init(data_dir: &Path) -> std::io::Result<()> {
        let dir = data_dir.join("oauth_tokens");
        std::fs::create_dir_all(&dir)?;
        let _ = TOKEN_DIR.set(dir);
        Ok(())
    }

    fn path_for(account_id: &str) -> Result<PathBuf> {
        let dir = TOKEN_DIR
            .get()
            .ok_or_else(|| Error::Other("oauth token store uninitialised".into()))?;
        // Sanitise so a crafted account id can't escape the directory.
        let safe: String = account_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Ok(dir.join(format!("{safe}.json")))
    }

    pub fn store(account_id: &str, tokens: &OAuthTokens) -> Result<()> {
        let json = serde_json::to_string(tokens)
            .map_err(|e| Error::Other(format!("Token serialize failed: {}", e)))?;
        let path = path_for(account_id)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())
            .map_err(|e| Error::Other(format!("Token file write failed: {}", e)))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| Error::Other(format!("Token file rename failed: {}", e)))?;
        Ok(())
    }

    pub fn load(account_id: &str) -> Result<Option<OAuthTokens>> {
        let path = path_for(account_id)?;
        match std::fs::read(&path) {
            Ok(bytes) => {
                let tokens: OAuthTokens = serde_json::from_slice(&bytes)
                    .map_err(|e| Error::Other(format!("Token deserialize failed: {}", e)))?;
                Ok(Some(tokens))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Other(format!("Token file read failed: {}", e))),
        }
    }

    pub fn delete(account_id: &str) -> Result<()> {
        let path = path_for(account_id)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Other(format!("Token file delete failed: {}", e))),
        }
    }
}

#[cfg(target_os = "android")]
pub fn init_token_store(data_dir: &std::path::Path) -> std::io::Result<()> {
    android_store::init(data_dir)
}

#[cfg(not(target_os = "android"))]
pub fn init_token_store(_data_dir: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

/// Accounts whose refresh token was rejected with `invalid_grant`.
///
/// Once a refresh token is dead (expired, revoked, or past a
/// conditional-access lifetime cap like AADSTS70043), every refresh attempt
/// fails the same way — but the periodic sync retried it every cycle,
/// hammering the token endpoint and burying the real problem in log noise.
/// This registry lets token getters fail fast until the user signs in
/// again; a successful `store_tokens` clears the flag.
static REAUTH_REQUIRED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

fn reauth_registry() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    REAUTH_REQUIRED.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Fail fast if the account is already known to need re-authentication.
pub fn ensure_not_reauth_required(account_id: &str) -> Result<()> {
    let flagged = reauth_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(account_id);
    if flagged {
        return Err(Error::AuthRequired(format!(
            "Sign-in expired for account {}. Open Settings and sign in again.",
            account_id
        )));
    }
    Ok(())
}

/// Inspect a token-refresh error: on a terminal `invalid_grant` mark the
/// account as requiring re-authentication and convert the error into
/// [`Error::AuthRequired`] so callers stop retrying. Other errors (network,
/// keyring, throttling) pass through unchanged and stay retryable.
///
/// Baseline Microsoft refreshes also encode scope-consent failures as
/// `invalid_grant` (AADSTS65001 / `"suberror":"consent_required"`). Those
/// still need user action and should latch here so periodic sync does not
/// keep retrying the same rejected refresh. Optional scope probes, such as
/// room lookup, bypass this helper instead of calling it.
pub fn auth_required_on_invalid_grant(account_id: &str, err: Error) -> Error {
    let msg = err.to_string();
    let error_code = msg
        .find('{')
        .and_then(|start| serde_json::from_str::<serde_json::Value>(&msg[start..]).ok())
        .and_then(|body| body["error"].as_str().map(str::to_owned));
    if error_code.as_deref() != Some("invalid_grant") {
        return err;
    }
    log::warn!(
        "OAuth2: refresh token rejected (invalid_grant) for account {}; re-authentication required",
        account_id
    );
    mark_reauth_required(account_id);
    Error::AuthRequired(format!(
        "Sign-in expired for account {}. Open Settings and sign in again.",
        account_id
    ))
}

pub(crate) fn clear_reauth_required(account_id: &str) {
    reauth_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(account_id);
}

pub(crate) fn is_reauth_required(account_id: &str) -> bool {
    reauth_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(account_id)
}

pub(crate) fn mark_reauth_required(account_id: &str) {
    reauth_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(account_id.to_string());
}

pub fn store_tokens(account_id: &str, tokens: &OAuthTokens) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        android_store::store(account_id, tokens)?;
        log::info!(
            "OAuth2: tokens stored in file store for account {}",
            account_id
        );
        // Fresh tokens (sign-in or successful refresh) mean the account no
        // longer needs re-authentication. Cleared only after persistence
        // succeeded — clearing on a failed store would re-open the refresh
        // retry loop with the old, rejected token still in place.
        clear_reauth_required(account_id);
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    {
        store_tokens_keyring(account_id, tokens)?;
        // See the android branch above: only a persisted fresh token set
        // clears the re-auth flag.
        clear_reauth_required(account_id);
        Ok(())
    }
}

#[cfg(not(target_os = "android"))]
fn store_tokens_keyring(account_id: &str, tokens: &OAuthTokens) -> Result<()> {
    let json = serde_json::to_string(tokens)
        .map_err(|e| Error::Other(format!("Token serialize failed: {}", e)))?;
    let entry = keyring::Entry::new(KEYRING_SERVICE, account_id)
        .map_err(|e| Error::Keyring(format!("Failed to create keyring entry: {}", e)))?;
    if let Err(first) = entry.set_password(&json) {
        // Secret Service backends (gnome-keyring on Linux) can return
        // "Object does not exist at path …" when their in-memory item
        // registry is desynced from disk after a daemon crash/restart:
        // SearchItems resolves a stale path and the SetSecret on it fails.
        // Drop the stale credential and retry once with a fresh CreateItem.
        log::warn!(
            "OAuth2: keyring set_password failed for {} ({}); attempting recovery",
            account_id,
            first,
        );
        let _ = entry.delete_credential();
        entry.set_password(&json).map_err(|e| {
            Error::Keyring(format!("Failed to store tokens (after recovery): {}", e))
        })?;
    }
    log::info!(
        "OAuth2: tokens stored in keyring for account {}",
        account_id
    );
    Ok(())
}

/// Read tokens from the keyring exactly once.
///
/// Distinguishes three outcomes so the caller can react correctly:
/// - `Ok(Some(_))` — tokens found.
/// - `Ok(None)` — keyring reachable, but no entry exists (`NoEntry`). The
///   account genuinely has no stored tokens; the user must sign in.
/// - `Err(Error::Keyring(_))` — the keyring could not be reached (e.g. the
///   Secret Service DBus connection dropped). This is *not* the same as
///   "no tokens" and must never be reported as such.
#[cfg(not(target_os = "android"))]
fn load_tokens_keyring_once(account_id: &str) -> Result<Option<OAuthTokens>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, account_id)
        .map_err(|e| Error::Keyring(format!("Failed to create keyring entry: {}", e)))?;
    match entry.get_password() {
        Ok(json) => {
            let tokens: OAuthTokens = serde_json::from_str(&json)
                .map_err(|e| Error::Other(format!("Token deserialize failed: {}", e)))?;
            Ok(Some(tokens))
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(Error::Keyring(format!("keyring read failed: {}", e))),
    }
}

pub fn load_tokens(account_id: &str) -> Result<Option<OAuthTokens>> {
    #[cfg(target_os = "android")]
    {
        return android_store::load(account_id);
    }
    #[cfg(not(target_os = "android"))]
    {
        // The Secret Service DBus connection can drop transiently
        // ("Remote peer disconnected") — e.g. when the gnome-keyring daemon
        // or session bus restarts. Retry once with a fresh connection before
        // surfacing the failure. Crucially, a keyring error is propagated as
        // an error (not Ok(None)): treating it as "no tokens" would wrongly
        // tell the user to sign in again when their tokens are still stored.
        match load_tokens_keyring_once(account_id) {
            Err(Error::Keyring(first)) => {
                log::warn!(
                    "OAuth2: keyring read failed for {} ({}); retrying once",
                    account_id,
                    first,
                );
                load_tokens_keyring_once(account_id).map_err(|e| {
                    log::error!(
                        "OAuth2: keyring read retry failed for {}: {}",
                        account_id,
                        e
                    );
                    e
                })
            }
            other => other,
        }
    }
}

pub fn delete_tokens(account_id: &str) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        android_store::delete(account_id)?;
    }
    #[cfg(not(target_os = "android"))]
    {
        let entry = keyring::Entry::new(KEYRING_SERVICE, account_id)
            .map_err(|e| Error::Keyring(format!("Failed to create keyring entry: {}", e)))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(Error::Keyring(format!("Failed to delete tokens: {}", e))),
        };
    }
    // A successful explicit deletion, including an already-absent credential,
    // completes re-authentication cleanup. Failed deletion leaves the latch in
    // place so background work cannot resume against rejected credentials.
    clear_reauth_required(account_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::thread::JoinHandle;

    struct TokenRequest {
        request_line: String,
        form: Vec<(String, String)>,
    }

    fn token_server(response_body: &'static str) -> (String, JoinHandle<TokenRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();

            let mut content_length = None;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" {
                    break;
                }
                if let Some(value) = header
                    .strip_prefix("Content-Length:")
                    .or_else(|| header.strip_prefix("content-length:"))
                {
                    content_length = Some(value.trim().parse::<usize>().unwrap());
                }
            }

            let mut body = vec![0; content_length.expect("request must have Content-Length")];
            reader.read_exact(&mut body).unwrap();
            let mut form: Vec<_> = url::form_urlencoded::parse(&body).into_owned().collect();
            form.sort();

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).unwrap();

            TokenRequest {
                request_line: request_line.trim_end().to_string(),
                form,
            }
        });
        (format!("http://{address}/token"), handle)
    }

    fn callback_request(target: &str, expected_state: &str) -> (Result<CallbackResult>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let expected_state = expected_state.to_string();
        let callback = std::thread::spawn(move || wait_for_callback(listener, &expected_state));

        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream
            .write_all(
                format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();

        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        (callback.join().unwrap(), response)
    }

    fn assert_callback_response(response: &str, status: &str) {
        let (headers, body) = response
            .split_once("\r\n\r\n")
            .expect("callback response has a header terminator");
        assert!(headers.starts_with(&format!("HTTP/1.1 {status}\r\n")));
        assert!(headers.contains("Content-Type: text/html; charset=utf-8\r\n"));
        assert!(headers.contains("Cache-Control: no-store\r\n"));
        assert!(headers.contains("Content-Security-Policy: default-src 'none'\r\n"));
        assert!(headers.contains("X-Content-Type-Options: nosniff\r\n"));
        assert!(headers.lines().any(|line| line == "Connection: close"));
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .expect("callback response has Content-Length")
            .parse::<usize>()
            .unwrap();
        assert_eq!(content_length, body.len());
    }

    #[test]
    fn callback_success_returns_code_and_browser_page() {
        let (result, response) =
            callback_request("/?code=test-code&state=test-state", "test-state");
        let callback = result.unwrap();

        assert_eq!(callback.code, "test-code");
        assert_eq!(callback.state.as_deref(), Some("test-state"));
        assert_callback_response(&response, "200 OK");
        assert!(response.contains("Authorization successful!"));
        assert!(!response.contains("test-code"));
        assert!(!response.contains("test-state"));
    }

    #[test]
    fn callback_access_denied_returns_safe_browser_page() {
        let (result, response) = callback_request(
            "/?error=access_denied&error_description=provider-marker-%3Cscript%3Ebad%3C%2Fscript%3E&state=test-state",
            "test-state",
        );
        let error = match result {
            Ok(_) => panic!("access_denied must fail"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("denied or cancelled"));
        assert_callback_response(&response, "400 Bad Request");
        assert!(response.contains("Authorization not completed"));
        assert!(response.contains("Access was not granted"));
        assert!(!error.contains("provider-marker"));
        assert!(!response.contains("provider-marker"));
        assert!(!response.contains("<script>"));
        assert!(!response.contains("test-state"));
    }

    #[test]
    fn callback_without_code_returns_generic_browser_page() {
        let (result, response) = callback_request("/?state=test-state", "test-state");
        let error = match result {
            Ok(_) => panic!("missing code must fail"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("did not contain an authorization code"));
        assert_callback_response(&response, "400 Bad Request");
        assert!(response.contains("Authorization failed"));
        assert!(!response.contains("test-state"));
    }

    #[test]
    fn callback_rejects_empty_code_and_unverified_state() {
        for (target, expected_state, expected_error) in [
            (
                "/?code=&state=test-state",
                "test-state",
                "did not contain an authorization code",
            ),
            (
                "/?code=test-code",
                "test-state",
                "missing required state parameter",
            ),
            (
                "/?code=test-code&state=wrong-state",
                "test-state",
                "state mismatch",
            ),
        ] {
            let (result, response) = callback_request(target, expected_state);
            let error = match result {
                Ok(_) => panic!("invalid callback must fail"),
                Err(error) => error.to_string(),
            };

            assert!(error.contains(expected_error));
            assert_callback_response(&response, "400 Bad Request");
            assert!(response.contains("Authorization failed"));
            assert!(!response.contains("test-code"));
            assert!(!response.contains("test-state"));
            assert!(!response.contains("wrong-state"));
        }
    }

    fn provider_at(
        provider: &OAuthProvider,
        token_url: String,
        client_id: &'static str,
        client_secret: &'static str,
    ) -> OAuthProvider {
        OAuthProvider {
            name: provider.name,
            client_id,
            client_secret,
            auth_url: provider.auth_url,
            token_url: Box::leak(token_url.into_boxed_str()),
            scopes: provider.scopes,
            token_exchange_scope: provider.token_exchange_scope,
            use_pkce: provider.use_pkce,
            redirect_host: provider.redirect_host,
            redirect_fixed_port: provider.redirect_fixed_port,
            redirect_url_override: provider.redirect_url_override,
        }
    }

    fn sorted_form(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut form: Vec<_> = entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        form.sort();
        form
    }

    fn is_flagged(account_id: &str) -> bool {
        reauth_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(account_id)
    }

    /// A terminal invalid_grant (dead refresh token, e.g. AADSTS70043)
    /// latches the re-auth flag and converts to AuthRequired.
    #[test]
    fn invalid_grant_latches_reauth() {
        let acc = "test-latch-terminal";
        let err = Error::Other(
            "Token refresh error: {\"error\":\"invalid_grant\",\
             \"error_description\":\"AADSTS70043: The refresh token has expired\",\
             \"suberror\":\"token_expired\"}"
                .into(),
        );
        let out = auth_required_on_invalid_grant(acc, err);
        assert!(matches!(out, Error::AuthRequired(_)));
        assert!(is_flagged(acc));
        assert!(ensure_not_reauth_required(acc).is_err());
        clear_reauth_required(acc);
    }

    /// Baseline scope-consent failures (AADSTS65001 / consent_required)
    /// are invalid_grant responses too and need user action. Optional
    /// room-scope probes avoid latching by not calling this helper.
    #[test]
    fn consent_required_latches_reauth_for_baseline_refresh() {
        let acc = "test-latch-consent";
        let err = Error::Other(
            "Token refresh error: {\"error\":\"invalid_grant\",\
             \"error_description\":\"AADSTS65001: The user or administrator has not \
             consented to use the application\",\"suberror\":\"consent_required\"}"
                .into(),
        );
        let out = auth_required_on_invalid_grant(acc, err);
        assert!(matches!(out, Error::AuthRequired(_)));
        assert!(is_flagged(acc));
        assert!(ensure_not_reauth_required(acc).is_err());
        clear_reauth_required(acc);
    }

    /// Unrelated errors (network, throttling) pass through untouched.
    #[test]
    fn other_errors_do_not_latch_reauth() {
        let acc = "test-latch-other";
        let out =
            auth_required_on_invalid_grant(acc, Error::Other("Token refresh failed: 503".into()));
        assert!(!matches!(out, Error::AuthRequired(_)));
        assert!(!is_flagged(acc));
    }

    #[test]
    fn invalid_grant_text_outside_error_code_does_not_latch_reauth() {
        let acc = "test-latch-description-only";
        let err = Error::Other(
            "Token refresh error: {\"error\":\"temporarily_unavailable\",\
             \"error_description\":\"retry after prior invalid_grant\"}"
                .into(),
        );

        let out = auth_required_on_invalid_grant(acc, err);

        assert!(!matches!(out, Error::AuthRequired(_)));
        assert!(!is_flagged(acc));
    }

    #[test]
    fn test_pkce_verifier_is_valid_length() {
        let verifier = generate_code_verifier();
        // RFC 7636: 43-128 characters
        assert!(
            verifier.len() >= 43,
            "verifier too short: {}",
            verifier.len()
        );
        assert!(
            verifier.len() <= 128,
            "verifier too long: {}",
            verifier.len()
        );
    }

    #[test]
    fn test_pkce_verifier_is_base64url() {
        let verifier = generate_code_verifier();
        // base64url chars: A-Z, a-z, 0-9, -, _
        for c in verifier.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "invalid char in verifier: '{}'",
                c
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
                "invalid char in challenge: '{}'",
                c
            );
        }
        // No padding
        assert!(!challenge.contains('='));
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn test_microsoft_provider_config() {
        assert_eq!(MICROSOFT.name, "microsoft");
        assert!(MICROSOFT.use_pkce);
        assert!(MICROSOFT.client_secret.is_empty());
        assert!(MICROSOFT.auth_url.contains("login.microsoftonline.com"));
        assert!(MICROSOFT.token_url.contains("login.microsoftonline.com"));
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn test_google_provider_pkce() {
        assert_eq!(GOOGLE.name, "google");
        assert!(GOOGLE.use_pkce);
        // Google Desktop app clients have a secret (not truly confidential)
        assert!(!GOOGLE.client_secret.is_empty());
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

    /// Microsoft authorizes across two resources (Graph +
    /// outlook.office.com), so its `authorization_code` redemption must
    /// send a `scope` or the v2.0 token endpoint rejects it with
    /// AADSTS70011. The exchange scope must be single-resource (no
    /// outlook.office.com mixed in) and every scope must be one the
    /// authorize request already consented to.
    #[test]
    fn test_microsoft_has_single_resource_token_exchange_scope() {
        let scope = MICROSOFT
            .token_exchange_scope
            .expect("Microsoft must define a token-exchange scope");
        assert!(
            !scope.contains("outlook.office.com"),
            "exchange scope must name a single resource (Graph only)"
        );
        for s in scope.split_whitespace() {
            assert!(
                MICROSOFT.scopes.contains(&s),
                "exchange scope {s:?} must be a subset of the consented scopes"
            );
        }
    }

    /// Google and Zoom derive scope from the code; they must not send
    /// one at exchange time.
    #[test]
    fn test_non_microsoft_providers_omit_token_exchange_scope() {
        assert!(GOOGLE.token_exchange_scope.is_none());
        assert!(ZOOM.token_exchange_scope.is_none());
    }

    #[test]
    fn zoom_requests_identity_scope_for_safe_reauthentication() {
        assert!(ZOOM.scopes.contains(&"user:read:user"));
    }

    #[tokio::test]
    async fn google_code_exchange_sends_exact_form_without_scope() {
        let (token_url, request) = token_server(
            r#"{"access_token":"test-access","refresh_token":"test-refresh","expires_in":3600}"#,
        );
        let provider = provider_at(
            &GOOGLE,
            token_url,
            "test-google-client",
            "test-google-secret",
        );
        let client = reqwest::Client::new();

        let tokens =
            exchange_code_with_client(&provider, "test-code", 4567, Some("test-verifier"), &client)
                .await
                .unwrap();

        assert_eq!(tokens.refresh_token.as_deref(), Some("test-refresh"));
        let request = request.join().unwrap();
        assert_eq!(request.request_line, "POST /token HTTP/1.1");
        assert_eq!(
            request.form,
            sorted_form(&[
                ("client_id", "test-google-client"),
                ("client_secret", "test-google-secret"),
                ("code", "test-code"),
                ("code_verifier", "test-verifier"),
                ("grant_type", "authorization_code"),
                ("redirect_uri", "http://localhost:4567"),
            ])
        );
    }

    #[tokio::test]
    async fn microsoft_code_exchange_sends_graph_scope_without_secret() {
        let (token_url, request) = token_server(r#"{"access_token":"test-access"}"#);
        let provider = provider_at(&MICROSOFT, token_url, "test-ms-client", "");
        let client = reqwest::Client::new();

        exchange_code_with_client(&provider, "test-code", 7654, Some("test-verifier"), &client)
            .await
            .unwrap();

        let request = request.join().unwrap();
        assert_eq!(request.request_line, "POST /token HTTP/1.1");
        assert_eq!(
            request.form,
            sorted_form(&[
                ("client_id", "test-ms-client"),
                ("code", "test-code"),
                ("code_verifier", "test-verifier"),
                ("grant_type", "authorization_code"),
                ("redirect_uri", "http://localhost:7654"),
                ("scope", MICROSOFT_GRAPH_SCOPES),
            ])
        );
    }

    #[tokio::test]
    async fn scoped_outlook_refresh_preserves_refresh_token() {
        let (token_url, request) = token_server(r#"{"access_token":"test-access"}"#);
        let provider = provider_at(&MICROSOFT, token_url, "test-ms-client", "");
        let client = reqwest::Client::new();

        let tokens = refresh_with_scopes_with_client(
            &provider,
            "test-original-refresh",
            MICROSOFT_IMAP_SCOPES,
            &client,
        )
        .await
        .unwrap();

        assert_eq!(
            tokens.refresh_token.as_deref(),
            Some("test-original-refresh")
        );
        let request = request.join().unwrap();
        assert_eq!(request.request_line, "POST /token HTTP/1.1");
        assert_eq!(
            request.form,
            sorted_form(&[
                ("client_id", "test-ms-client"),
                ("grant_type", "refresh_token"),
                ("refresh_token", "test-original-refresh"),
                ("scope", MICROSOFT_IMAP_SCOPES),
            ])
        );
    }

    #[tokio::test]
    async fn scoped_graph_refresh_uses_rotated_refresh_token() {
        let (token_url, request) = token_server(
            r#"{"access_token":"test-access","refresh_token":"test-rotated-refresh"}"#,
        );
        let provider = provider_at(&MICROSOFT, token_url, "test-ms-client", "");
        let client = reqwest::Client::new();

        let tokens = refresh_with_scopes_with_client(
            &provider,
            "test-original-refresh",
            MICROSOFT_GRAPH_SCOPES,
            &client,
        )
        .await
        .unwrap();

        assert_eq!(
            tokens.refresh_token.as_deref(),
            Some("test-rotated-refresh")
        );
        let request = request.join().unwrap();
        assert_eq!(request.request_line, "POST /token HTTP/1.1");
        assert_eq!(
            request.form,
            sorted_form(&[
                ("client_id", "test-ms-client"),
                ("grant_type", "refresh_token"),
                ("refresh_token", "test-original-refresh"),
                ("scope", MICROSOFT_GRAPH_SCOPES),
            ])
        );
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
