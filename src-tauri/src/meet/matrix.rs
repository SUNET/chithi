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

const MATRIX_ERROR_BODY_LIMIT: usize = 8 * 1024;
const USER_AGENT: &str = concat!("Chithi/", env!("CARGO_PKG_VERSION"));
const SSO_CALLBACK_TIMEOUT_SECS: u64 = 300;

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
/// port, generates a random state nonce, and returns the SSO
/// redirect URL the caller should open in the user's browser
/// plus the listener and state that will pick up the
/// `loginToken` callback.
///
/// The state nonce is appended to the redirect URL as a query
/// parameter and validated by `await_login_token` against the
/// callback's matching `state=` parameter. Without this, any
/// local process that can guess the callback port could race a
/// genuine SSO completion by sending its own `loginToken=` and
/// have us exchange it. The nonce is unguessable (16 random
/// bytes via the same generator OAuth uses for PKCE verifiers)
/// so a TOCTOU between `meet_matrix_login_start` (which races
/// the listener registration) and the legitimate redirect is
/// what we're actually defending against.
pub fn sso_login_start(homeserver_url: &str) -> Result<(String, TcpListener, String)> {
    crate::mail::url_validation::require_https(homeserver_url)?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| Error::Other(format!("matrix sso bind: {}", e)))?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::Other(format!("matrix sso port: {}", e)))?
        .port();
    let state = crate::oauth::generate_code_verifier();
    // Matrix requires `localhost` in the Redirect URL to route back
    // through the SSO IdP cleanly; some IdPs reject 127.0.0.1.
    // The state lives as a query param on the redirect URL so it
    // round-trips back to us as `?loginToken=...&state=<nonce>`.
    let redirect = format!(
        "http://localhost:{}/?state={}",
        port,
        urlencoding::encode(&state),
    );
    let url = format!(
        "{}/_matrix/client/v3/login/sso/redirect?redirectUrl={}",
        normalize_base_url(homeserver_url),
        urlencoding::encode(&redirect),
    );
    Ok((url, listener, state))
}

/// Wait for the browser to come back with a `loginToken` query
/// parameter, validating that its `state=` matches the value we
/// embedded in `sso_login_start`. Five-minute timeout matches
/// the OAuth path so a user who walked away doesn't leave the
/// listener wedged forever. A mismatched / missing state is
/// surfaced as an error rather than silently exchanged — see
/// the doc on `sso_login_start` for why this matters.
pub fn await_login_token(listener: TcpListener, expected_state: &str) -> Result<String> {
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

    // Validate state before pulling the loginToken. A missing or
    // mismatched state means the callback didn't originate from
    // our `sso_login_start` — possibly a local process racing the
    // listener. Return an error rather than exchange an attacker-
    // supplied loginToken.
    let got_state = params.get("state").map(|s| s.as_str()).unwrap_or("");
    if got_state != expected_state {
        return Err(Error::Other(
            "Matrix SSO: state mismatch on callback (cancelled or replay)".into(),
        ));
    }
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
/// `m.login.token` using an explicit HTTP client. The homeserver
/// returned in the response (or the user-supplied URL if the server
/// didn't echo it) is what we keep in the account row — that's the
/// authority for subsequent API calls.
pub async fn exchange_login_token_with_client(
    homeserver_url: &str,
    login_token: &str,
    client: &reqwest::Client,
) -> Result<LoginResult> {
    crate::mail::url_validation::require_https(homeserver_url)?;
    let url = format!(
        "{}/_matrix/client/v3/login",
        normalize_base_url(homeserver_url)
    );
    let body = serde_json::json!({
        "type": "m.login.token",
        "token": login_token,
        // USER_AGENT already encodes "Chithi/<version>"; using it
        // directly as the device label avoids the redundant
        // "Chithi (Chithi/<version>)" that Matrix clients would
        // otherwise show in the Active sessions panel.
        "initial_device_display_name": USER_AGENT,
    });
    let resp = client
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
/// The request uses the explicit HTTP client supplied by the caller.
///
/// [rtc-foci]: https://github.com/matrix-org/matrix-spec-proposals/pull/4143
pub async fn create_call_with_client(
    homeserver: &str,
    access_token: &str,
    room_name: &str,
    element_call_url: Option<&str>,
    client: &reqwest::Client,
) -> Result<crate::meet::MeetCreateResult> {
    crate::mail::url_validation::require_https(homeserver)?;
    let create_url = format!(
        "{}/_matrix/client/v3/createRoom",
        normalize_base_url(homeserver)
    );
    let body = serde_json::json!({
        "name": if room_name.trim().is_empty() { "Meeting" } else { room_name },
        "preset": "private_chat",
        "visibility": "private",
    });
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
    let join_url = format!(
        "https://matrix.to/#/{}?via={}",
        room_id,
        urlencoding::encode(&via),
    );
    Ok(crate::meet::MeetCreateResult {
        join_url,
        meeting_id: room_id,
    })
}

/// Update the room's display name. Matrix tracks the title as the
/// `m.room.name` state event, so renaming = PUT a new state event
/// with the new name. 403 (no power level) and 404 (room gone) are
/// treated as success: the rename is best-effort. The request uses
/// the explicit HTTP client supplied by the caller.
pub async fn rename_room_with_client(
    homeserver: &str,
    access_token: &str,
    room_id: &str,
    new_name: &str,
    client: &reqwest::Client,
) -> Result<()> {
    crate::mail::url_validation::require_https(homeserver)?;
    let url = format!(
        "{}/_matrix/client/v3/rooms/{}/state/m.room.name",
        normalize_base_url(homeserver),
        urlencoding::encode(room_id),
    );
    let name = if new_name.trim().is_empty() {
        "Meeting"
    } else {
        new_name
    };
    let body = serde_json::json!({ "name": name });
    let resp = client
        .put(&url)
        .bearer_auth(access_token)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Other(format!("matrix rename_room request: {}", e)))?;
    let status = resp.status();
    if status.is_success() || status.as_u16() == 403 || status.as_u16() == 404 {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(Error::Other(format!(
        "matrix rename_room: {} ({})",
        status,
        body.chars().take(500).collect::<String>(),
    )))
}

/// Leave a Matrix room the app created via `create_call_with_client`. Matrix
/// has no "delete room" API (rooms outlive everyone in them), but
/// leaving drops the room from this user's room list — which is
/// the closest analogue to "cancelled this call." Repeated leaves
/// are accepted only when the Matrix error explicitly proves absent
/// membership; ambiguous authorization failures remain errors so
/// durable ownership can be retried. The request uses the explicit
/// HTTP client supplied by the caller.
pub async fn leave_room_with_client(
    homeserver: &str,
    access_token: &str,
    room_id: &str,
    client: &reqwest::Client,
) -> Result<()> {
    crate::mail::url_validation::require_https(homeserver)?;
    let url = format!(
        "{}/_matrix/client/v3/rooms/{}/leave",
        normalize_base_url(homeserver),
        urlencoding::encode(room_id),
    );
    let mut resp = client
        .post(&url)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .map_err(|e| Error::Other(format!("matrix leave_room request: {}", e)))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }

    let (body, truncated) = read_bounded_matrix_error(&mut resp).await?;
    let matrix_error = if truncated {
        None
    } else {
        serde_json::from_slice::<MatrixError>(&body).ok()
    };

    // Matrix uses M_FORBIDDEN for both an idempotent repeated leave and real
    // authorization failures. Only explicit absent-membership wording is safe
    // to discard; an unknown 403 must keep durable ownership for a later retry.
    let already_gone = matrix_error.as_ref().is_some_and(|error| {
        (status.is_client_error() && error.errcode == "M_NOT_FOUND")
            || (status == reqwest::StatusCode::NOT_FOUND
                && error.errcode == "M_UNKNOWN"
                && error.error.trim() == "Not a known room")
            || (status == reqwest::StatusCode::FORBIDDEN
                && error.errcode == "M_FORBIDDEN"
                && is_absent_membership_error(&error.error, room_id))
    });
    if already_gone {
        return Ok(());
    }

    let body = String::from_utf8_lossy(&body);
    let truncation = if truncated { " [truncated]" } else { "" };
    Err(Error::Other(format!(
        "matrix leave_room: {} ({}{})",
        status,
        body.chars().take(500).collect::<String>(),
        truncation,
    )))
}

#[derive(Deserialize)]
struct MatrixError {
    errcode: String,
    error: String,
}

async fn read_bounded_matrix_error(response: &mut reqwest::Response) -> Result<(Vec<u8>, bool)> {
    let mut body = Vec::with_capacity(MATRIX_ERROR_BODY_LIMIT.min(1024));
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| Error::Other(format!("matrix leave_room response: {}", e)))?
    {
        let remaining = MATRIX_ERROR_BODY_LIMIT + 1 - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if body.len() > MATRIX_ERROR_BODY_LIMIT {
            body.truncate(MATRIX_ERROR_BODY_LIMIT);
            return Ok((body, true));
        }
    }
    Ok((body, false))
}

fn is_absent_membership_error(error: &str, room_id: &str) -> bool {
    let normalized = error.split_whitespace().collect::<Vec<_>>().join(" ");
    let Some(prefix) = normalized.strip_suffix(room_id) else {
        return false;
    };
    let prefix = prefix.trim_end().to_ascii_lowercase();
    if matches!(
        prefix.as_str(),
        "you are not in room"
            | "you are not in the room"
            | "you are not joined to room"
            | "you are not joined to the room"
            | "you already left room"
            | "you already left the room"
    ) {
        return true;
    }

    ["user ", "the user ", "requesting user "]
        .iter()
        .find_map(|subject| prefix.strip_prefix(subject))
        .and_then(|user_and_state| user_and_state.split_once(' '))
        .is_some_and(|(user_id, state)| {
            !user_id.is_empty()
                && matches!(
                    state,
                    "not in room"
                        | "not in the room"
                        | "is not in room"
                        | "is not in the room"
                        | "not joined to room"
                        | "not joined to the room"
                        | "is not joined to room"
                        | "is not joined to the room"
                        | "no longer in room"
                        | "no longer in the room"
                        | "already left room"
                        | "already left the room"
                )
        })
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
/// binding and the access token from the injected provider services.
pub struct MatrixProvider;

#[async_trait::async_trait]
impl crate::meet::MeetProvider for MatrixProvider {
    fn protocol(&self) -> &'static str {
        "matrix"
    }
    async fn create_url(
        &self,
        ctx: &crate::meet::MeetProviderCtx<'_>,
        account: &crate::db::accounts::AccountFull,
        name: &str,
        _start_time: Option<&str>,
        _duration_minutes: Option<u32>,
    ) -> Result<crate::meet::MeetCreateResult> {
        // Element Call rooms are persistent (joinable any time),
        // so the calendar event's start/duration are ignored.
        let homeserver = account.meet_url.trim();
        if homeserver.is_empty() {
            return Err(Error::Other(
                "Matrix: account has no homeserver URL configured".into(),
            ));
        }
        let access_token = ctx
            .services
            .credentials()
            .matrix_access_token(&account.id)
            .await?;
        create_call_with_client(
            homeserver,
            &access_token,
            name,
            None,
            &ctx.services.transports.matrix_http,
        )
        .await
    }

    async fn delete_meeting(
        &self,
        ctx: &crate::meet::MeetProviderCtx<'_>,
        account: &crate::db::accounts::AccountFull,
        meeting_id: &str,
    ) -> Result<crate::meet::MeetDeleteOutcome> {
        let homeserver = account.meet_url.trim();
        if homeserver.is_empty() {
            return Err(Error::Other(
                "Matrix: account has no homeserver URL configured".into(),
            ));
        }
        let access_token = ctx
            .services
            .credentials()
            .matrix_access_token(&account.id)
            .await?;
        leave_room_with_client(
            homeserver,
            &access_token,
            meeting_id,
            &ctx.services.transports.matrix_http,
        )
        .await?;
        Ok(crate::meet::MeetDeleteOutcome::Deleted)
    }

    async fn update_topic(
        &self,
        ctx: &crate::meet::MeetProviderCtx<'_>,
        account: &crate::db::accounts::AccountFull,
        meeting_id: &str,
        topic: &str,
    ) -> Result<()> {
        let homeserver = account.meet_url.trim();
        if homeserver.is_empty() {
            return Err(Error::Other(
                "Matrix: account has no homeserver URL configured".into(),
            ));
        }
        let access_token = ctx
            .services
            .credentials()
            .matrix_access_token(&account.id)
            .await?;
        rename_room_with_client(
            homeserver,
            &access_token,
            meeting_id,
            topic,
            &ctx.services.transports.matrix_http,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn read_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            request.extend_from_slice(&buffer[..read]);
            let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let header_end = header_end + 4;
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if request.len() >= header_end + content_length {
                return String::from_utf8(request).unwrap();
            }
        }
    }

    fn mock_server(
        responses: Vec<(&'static str, &'static str)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        mock_server_owned(
            responses
                .into_iter()
                .map(|(status, body)| (status.to_string(), body.to_string()))
                .collect(),
        )
    }

    fn mock_server_owned(
        responses: Vec<(String, String)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            responses
                .into_iter()
                .map(|(status, body)| {
                    let (mut stream, _) = listener.accept().unwrap();
                    let request = read_request(&mut stream);
                    write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len(),
                    )
                    .unwrap();
                    request
                })
                .collect()
        });
        (format!("http://{address}"), server)
    }

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

    #[test]
    fn sso_login_rejects_public_http_homeserver() {
        let error = sso_login_start("http://matrix.example").unwrap_err();
        assert!(error.to_string().contains("https://"));
    }

    #[tokio::test]
    async fn secret_requests_reject_public_http_homeserver() {
        let client = reqwest::Client::new();

        assert!(
            exchange_login_token_with_client("http://matrix.example", "sso-token", &client,)
                .await
                .is_err()
        );
        assert!(create_call_with_client(
            "http://matrix.example",
            "matrix-token",
            "Meeting",
            None,
            &client,
        )
        .await
        .is_err());
        assert!(rename_room_with_client(
            "http://matrix.example",
            "matrix-token",
            "!room:matrix.example",
            "Renamed",
            &client,
        )
        .await
        .is_err());
        assert!(leave_room_with_client(
            "http://matrix.example",
            "matrix-token",
            "!room:matrix.example",
            &client,
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn exchange_login_token_uses_injected_homeserver_path_and_payload() {
        let (root, server) = mock_server(vec![(
            "200 OK",
            r#"{"access_token":"matrix-token","user_id":"@alice:matrix.example","device_id":"DEVICE","well_known":{"m.homeserver":{"base_url":"https://canonical.example/"}}}"#,
        )]);
        let homeserver = format!("{root}/tenant/");

        let result =
            exchange_login_token_with_client(&homeserver, "sso-token", &reqwest::Client::new())
                .await
                .unwrap();
        let requests = server.join().unwrap();
        let (headers, body) = requests[0].split_once("\r\n\r\n").unwrap();
        let payload: serde_json::Value = serde_json::from_str(body).unwrap();

        assert!(headers.starts_with("POST /tenant/_matrix/client/v3/login HTTP/1.1\r\n"));
        assert_eq!(payload["type"], "m.login.token");
        assert_eq!(payload["token"], "sso-token");
        assert_eq!(payload["initial_device_display_name"], USER_AGENT);
        assert_eq!(result.access_token, "matrix-token");
        assert_eq!(result.homeserver, "https://canonical.example");
    }

    #[tokio::test]
    async fn create_call_sends_bearer_auth_and_room_payload() {
        let (root, server) = mock_server(vec![
            ("200 OK", r#"{"room_id":"!room:matrix.example"}"#),
            ("200 OK", "{}"),
        ]);

        let result = create_call_with_client(
            &root,
            "matrix-token",
            "Weekly sync",
            Some("https://call.example/"),
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        let requests = server.join().unwrap();
        let (headers, body) = requests[0].split_once("\r\n\r\n").unwrap();
        let payload: serde_json::Value = serde_json::from_str(body).unwrap();

        assert!(headers.starts_with("POST /_matrix/client/v3/createRoom HTTP/1.1\r\n"));
        assert!(headers
            .to_ascii_lowercase()
            .contains("authorization: bearer matrix-token"));
        assert_eq!(payload["name"], "Weekly sync");
        assert_eq!(payload["preset"], "private_chat");
        assert_eq!(payload["visibility"], "private");
        assert!(
            requests[1].starts_with("PUT /_matrix/client/v3/rooms/%21room%3Amatrix.example/state/")
        );
        assert!(requests[1]
            .to_ascii_lowercase()
            .contains("authorization: bearer matrix-token"));
        assert_eq!(result.meeting_id, "!room:matrix.example");
    }

    #[tokio::test]
    async fn leave_room_accepts_success() {
        let (root, server) = mock_server(vec![("204 No Content", "")]);

        leave_room_with_client(
            &root,
            "matrix-token",
            "!room:matrix.example",
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        server.join().unwrap();
    }

    #[tokio::test]
    async fn leave_room_accepts_repeated_leave() {
        let (root, server) = mock_server(vec![(
            "403 Forbidden",
            r#"{"errcode":"M_FORBIDDEN","error":"User @alice:matrix.example not in room !room:matrix.example"}"#,
        )]);

        leave_room_with_client(
            &root,
            "matrix-token",
            "!room:matrix.example",
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        server.join().unwrap();
    }

    #[tokio::test]
    async fn leave_room_accepts_missing_room() {
        let (root, server) = mock_server(vec![(
            "404 Not Found",
            r#"{"errcode":"M_NOT_FOUND","error":"Room not found"}"#,
        )]);

        leave_room_with_client(
            &root,
            "matrix-token",
            "!missing:matrix.example",
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        server.join().unwrap();
    }

    #[tokio::test]
    async fn leave_room_accepts_synapse_unknown_missing_room() {
        let (root, server) = mock_server(vec![(
            "404 Not Found",
            r#"{"errcode":"M_UNKNOWN","error":"Not a known room"}"#,
        )]);

        leave_room_with_client(
            &root,
            "matrix-token",
            "!missing:matrix.example",
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        server.join().unwrap();
    }

    #[tokio::test]
    async fn leave_room_accepts_matrix_not_found_on_other_client_error() {
        let (root, server) = mock_server(vec![(
            "400 Bad Request",
            r#"{"errcode":"M_NOT_FOUND","error":"Room not found"}"#,
        )]);

        leave_room_with_client(
            &root,
            "matrix-token",
            "!missing:matrix.example",
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        server.join().unwrap();
    }

    #[tokio::test]
    async fn leave_room_rejects_genuine_authorization_failure() {
        let (root, server) = mock_server(vec![(
            "403 Forbidden",
            r#"{"errcode":"M_FORBIDDEN","error":"You do not have permission to leave this room"}"#,
        )]);

        let error = leave_room_with_client(
            &root,
            "matrix-token",
            "!room:matrix.example",
            &reqwest::Client::new(),
        )
        .await
        .unwrap_err();
        server.join().unwrap();

        assert!(error.to_string().contains("403 Forbidden"));
    }

    #[tokio::test]
    async fn leave_room_rejects_malformed_and_unknown_forbidden_errors() {
        let (root, server) = mock_server(vec![
            ("403 Forbidden", "not json"),
            (
                "403 Forbidden",
                r#"{"errcode":"M_FORBIDDEN","error":"Insufficient power level"}"#,
            ),
            (
                "403 Forbidden",
                r#"{"errcode":"M_FORBIDDEN","error":"You are not in the room's moderator list for !room:matrix.example"}"#,
            ),
        ]);
        let client = reqwest::Client::new();

        assert!(
            leave_room_with_client(&root, "matrix-token", "!room:matrix.example", &client,)
                .await
                .is_err()
        );
        assert!(
            leave_room_with_client(&root, "matrix-token", "!room:matrix.example", &client,)
                .await
                .is_err()
        );
        assert!(
            leave_room_with_client(&root, "matrix-token", "!room:matrix.example", &client,)
                .await
                .is_err()
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn leave_room_rejects_oversized_error_body() {
        let body = format!(
            r#"{{"errcode":"M_NOT_FOUND","error":"{}"}}"#,
            "x".repeat(MATRIX_ERROR_BODY_LIMIT)
        );
        let (root, server) = mock_server_owned(vec![("404 Not Found".to_string(), body)]);

        let error = leave_room_with_client(
            &root,
            "matrix-token",
            "!room:matrix.example",
            &reqwest::Client::new(),
        )
        .await
        .unwrap_err();
        server.join().unwrap();

        assert!(error.to_string().contains("[truncated]"));
    }

    #[tokio::test]
    async fn leave_room_sends_expected_request() {
        let (root, server) = mock_server(vec![("200 OK", "{}")]);
        let homeserver = format!("{root}/tenant/");

        leave_room_with_client(
            &homeserver,
            "matrix-token",
            "!room:matrix.example",
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        let requests = server.join().unwrap();
        let (headers, body) = requests[0].split_once("\r\n\r\n").unwrap();

        assert!(headers.starts_with(
            "POST /tenant/_matrix/client/v3/rooms/%21room%3Amatrix.example/leave HTTP/1.1\r\n"
        ));
        assert!(headers
            .to_ascii_lowercase()
            .contains("authorization: bearer matrix-token"));
        assert_eq!(body, "{}");
    }
}
