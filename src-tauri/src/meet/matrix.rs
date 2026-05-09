//! Matrix integration (#148). Used to create a fresh Matrix room
//! and a corresponding [Element Call][ec] widget so the join URL
//! can be pasted into a calendar event.
//!
//! Auth uses the standard SSO redirect flow:
//! 1. Bind a local TCP listener on `127.0.0.1:0`.
//! 2. Build `<homeserver>/_matrix/client/v3/login/sso/redirect?redirectUrl=http://localhost:<port>/`.
//! 3. User opens that URL in their browser, completes SSO at the
//!    homeserver / IdP.
//! 4. Browser redirects to our local listener with `?loginToken=...`.
//! 5. We exchange the loginToken for an `access_token` via
//!    `m.login.token`.
//!
//! That mirrors what Element Web does and what `matrix.sunet.se`
//! advertises (`m.login.sso` with `oauth_aware_preferred: true`).
//!
//! [ec]: https://github.com/element-hq/element-call

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const HTTP_TIMEOUT_SECS: u64 = 30;
const USER_AGENT: &str = concat!("Chithi/", env!("CARGO_PKG_VERSION"));
const SSO_CALLBACK_TIMEOUT_SECS: u64 = 300;

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| Error::Other(format!("matrix http client: {}", e)))
}

/// Result of `m.login.token` exchange. Only carries fields the rest
/// of the app needs to keep around.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResult {
    pub access_token: String,
    pub user_id: String,
    pub device_id: String,
    /// The homeserver we logged into. Echoes `well_known.m.homeserver.base_url`
    /// when the homeserver advertises it; otherwise the user-supplied URL.
    pub homeserver: String,
}

/// Start the SSO flow against `homeserver_url` (e.g.
/// `https://matrix.sunet.se`). Binds a local listener on a random
/// port, returns the SSO redirect URL the caller should open in the
/// user's browser plus the listener that will pick up the
/// `loginToken` callback. The listener is returned so caller code
/// can hand it to `await_login_token` after telling the frontend
/// to open the URL.
pub fn sso_login_start(homeserver_url: &str) -> Result<(String, TcpListener)> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| Error::Other(format!("matrix sso bind: {}", e)))?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::Other(format!("matrix sso port: {}", e)))?
        .port();
    // Matrix requires `localhost` in the Redirect URL to route back
    // through the SSO IdP cleanly; some IdPs reject 127.0.0.1.
    let redirect = format!("http://localhost:{}/", port);
    let url = format!(
        "{}/_matrix/client/v3/login/sso/redirect?redirectUrl={}",
        normalize_base_url(homeserver_url),
        urlencoding::encode(&redirect),
    );
    Ok((url, listener))
}

/// Wait for the browser to come back with a `loginToken` query
/// parameter. Five-minute timeout matches the OAuth path so a user
/// who walked away doesn't leave the listener wedged forever.
pub fn await_login_token(listener: TcpListener) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(SSO_CALLBACK_TIMEOUT_SECS);
    listener
        .set_nonblocking(true)
        .map_err(|e| Error::Other(format!("matrix sso non-blocking: {}", e)))?;

    let (mut stream, _) = loop {
        match listener.accept() {
            Ok(conn) => break conn,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(Error::Other("Matrix SSO timed out after 5 minutes".into()));
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(Error::Other(format!("matrix sso accept: {}", e))),
        }
    };

    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|e| Error::Other(format!("matrix sso stream clone: {}", e)))?,
    );
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|e| Error::Other(format!("matrix sso read: {}", e)))?;

    let params: HashMap<String, String> = request_line
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
                    let decoded = urlencoding::decode(val)
                        .unwrap_or_else(|_| val.into())
                        .into_owned();
                    Some((key.to_string(), decoded))
                })
                .collect()
        })
        .unwrap_or_default();

    let token = params.get("loginToken").cloned().ok_or_else(|| {
        Error::Other(format!(
            "Matrix SSO: callback did not carry loginToken (got: {:?})",
            params.keys().collect::<Vec<_>>(),
        ))
    })?;

    // Ack the browser so the user sees a clean confirmation page.
    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
        <html><body style='font-family:sans-serif;text-align:center;padding:60px'>\
        <h2>Matrix sign-in successful</h2>\
        <p>You can close this window and return to Chithi.</p>\
        </body></html>",
    );
    Ok(token)
}

/// Exchange the SSO `loginToken` for a long-lived `access_token` via
/// `m.login.token`. The homeserver returned in the response (or the
/// user-supplied URL if the server didn't echo it) is what we keep
/// in the account row — that's the authority for subsequent API
/// calls.
pub async fn exchange_login_token(homeserver_url: &str, login_token: &str) -> Result<LoginResult> {
    let url = format!(
        "{}/_matrix/client/v3/login",
        normalize_base_url(homeserver_url)
    );
    let body = serde_json::json!({
        "type": "m.login.token",
        "token": login_token,
        "initial_device_display_name": format!("Chithi ({})", USER_AGENT),
    });
    let resp = http_client()?
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Other(format!("matrix login request: {}", e)))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Other(format!(
            "matrix login: {} ({})",
            status,
            body.chars().take(500).collect::<String>(),
        )));
    }
    let payload: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Other(format!("matrix login parse: {}", e)))?;
    let access_token = payload["access_token"]
        .as_str()
        .ok_or_else(|| Error::Other("matrix login: missing access_token".into()))?
        .to_string();
    let user_id = payload["user_id"]
        .as_str()
        .ok_or_else(|| Error::Other("matrix login: missing user_id".into()))?
        .to_string();
    let device_id = payload["device_id"].as_str().unwrap_or("").to_string();
    let homeserver = payload["well_known"]["m.homeserver"]["base_url"]
        .as_str()
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| normalize_base_url(homeserver_url));
    Ok(LoginResult {
        access_token,
        user_id,
        device_id,
        homeserver,
    })
}

/// Create a fresh Matrix room for a meeting and post an Element
/// Call widget state event into it. Returns a `matrix.to` join
/// URL that opens the user's existing Matrix client (Element Web
/// at their organization, the desktop app, etc.) on the freshly
/// created room — the call is then driven by the embedded
/// widget, which uses the homeserver's own LiveKit SFU advertised
/// via the [`org.matrix.msc4143.rtc_foci`][rtc-foci] well-known.
///
/// We deliberately do **not** return a `https://call.element.io`
/// URL: that hosts the *frontend* of Element Call but doesn't
/// know about the user's homeserver, so opening it forced a
/// second, unrelated login at element.io. The matrix.to redirect
/// keeps the user on their own infrastructure (#148).
///
/// `room_name` becomes the room's `name` in Matrix (visible in any
/// Matrix client). `element_call_url` is the Element Call frontend
/// referenced by the widget; defaults to `https://call.element.io`
/// since that's the canonical implementation. Self-hosted Element
/// Call instances can be passed here later via a per-account
/// override if/when we add that setting.
///
/// [rtc-foci]: https://github.com/matrix-org/matrix-spec-proposals/pull/4143
pub async fn create_call(
    homeserver: &str,
    access_token: &str,
    room_name: &str,
    element_call_url: Option<&str>,
) -> Result<String> {
    let create_url = format!(
        "{}/_matrix/client/v3/createRoom",
        normalize_base_url(homeserver)
    );
    let body = serde_json::json!({
        "name": if room_name.trim().is_empty() { "Meeting" } else { room_name },
        "preset": "private_chat",
        "visibility": "private",
    });
    let client = http_client()?;
    let resp = client
        .post(&create_url)
        .bearer_auth(access_token)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Other(format!("matrix createRoom request: {}", e)))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Other(format!(
            "matrix createRoom: {} ({})",
            status,
            body.chars().take(500).collect::<String>(),
        )));
    }
    let payload: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Other(format!("matrix createRoom parse: {}", e)))?;
    let room_id = payload["room_id"]
        .as_str()
        .ok_or_else(|| Error::Other("matrix createRoom: missing room_id".into()))?
        .to_string();

    // Attach an Element Call widget so any Matrix client opening the
    // room sees a one-click join button. Failing to write the widget
    // doesn't fail the whole call: the room itself is a usable
    // fallback, and the join URL we return below works without it.
    let call_base = element_call_url
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "https://call.element.io".to_string());
    let widget_url = format!(
        "{}/room?roomId={}&embed=true&hideHeader=true",
        call_base,
        urlencoding::encode(&room_id),
    );
    let widget_state_url = format!(
        "{}/_matrix/client/v3/rooms/{}/state/im.vector.modular.widgets/{}",
        normalize_base_url(homeserver),
        urlencoding::encode(&room_id),
        urlencoding::encode("element-call"),
    );
    let widget_body = serde_json::json!({
        "type": "m.custom",
        "url": widget_url,
        "name": "Element Call",
        "data": { "domain": call_base },
    });
    if let Err(e) = client
        .put(&widget_state_url)
        .bearer_auth(access_token)
        .json(&widget_body)
        .send()
        .await
    {
        log::warn!(
            "matrix create_call: widget state event failed (room is still usable): {}",
            e
        );
    }

    // matrix.to URL — the standard cross-client room-link
    // redirector. Format is `https://matrix.to/#/<roomId>?via=<server>`,
    // where `<roomId>` is the literal room id including its `!` and
    // `:` (matrix.to keeps those unencoded by spec) and `via` hints
    // which homeserver to route through. The homeserver fragment
    // we extract from the user-supplied URL — for matrix.sunet.se
    // that's just `matrix.sunet.se`.
    let via = host_only(homeserver);
    Ok(format!(
        "https://matrix.to/#/{}?via={}",
        room_id,
        urlencoding::encode(&via),
    ))
}

/// Pull just the host out of a homeserver URL, so the matrix.to
/// `via=` parameter gets `matrix.example.com`, not the scheme or
/// trailing slash. Falls back to the input string for malformed
/// URLs — matrix.to tolerates that and shows its picker UI.
fn host_only(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

fn normalize_base_url(server: &str) -> String {
    server.trim_end_matches('/').to_string()
}

/// `MeetProvider` implementor for Matrix / Element Call. Stateless;
/// each `create_url` reads the homeserver from the account's meet
/// binding and the access token from the keyring.
pub struct MatrixProvider;

#[async_trait::async_trait]
impl crate::meet::MeetProvider for MatrixProvider {
    fn protocol(&self) -> &'static str {
        "matrix"
    }
    fn label(&self) -> &'static str {
        "Matrix"
    }
    async fn create_url(
        &self,
        account: &crate::db::accounts::AccountFull,
        name: &str,
    ) -> Result<String> {
        let homeserver = account.meet_url.trim();
        if homeserver.is_empty() {
            return Err(Error::Other(
                "Matrix: account has no homeserver URL configured".into(),
            ));
        }
        let access_token = match crate::oauth::load_tokens(&account.id)? {
            Some(t) => t.access_token,
            None => {
                return Err(Error::Other(
                    "Matrix: no access token in keyring; sign in again".into(),
                ));
            }
        };
        create_call(homeserver, &access_token, name, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_only_strips_scheme_and_path() {
        assert_eq!(host_only("https://matrix.sunet.se"), "matrix.sunet.se");
        assert_eq!(host_only("https://matrix.sunet.se/"), "matrix.sunet.se");
        assert_eq!(
            host_only("http://matrix.example.org/foo"),
            "matrix.example.org"
        );
        // Pathological input: pass it through so matrix.to can
        // surface its picker UI rather than crashing the call.
        assert_eq!(host_only("matrix.sunet.se"), "matrix.sunet.se");
    }

    #[test]
    fn normalizes_trailing_slashes() {
        assert_eq!(
            normalize_base_url("https://m.example.com/"),
            "https://m.example.com"
        );
        assert_eq!(
            normalize_base_url("https://m.example.com///"),
            "https://m.example.com"
        );
    }
}
