//! Nextcloud Talk integration (#148).
//!
//! Two flows:
//! - `login_flow_v2_start` / `login_flow_v2_poll` implement the
//!   [Nextcloud Login Flow v2 spec][lf2]. The server hands us a
//!   browser-facing `login` URL plus a `poll` endpoint+token; we
//!   open the URL in the user's browser, then poll until the user
//!   finishes the in-browser flow. The poll response carries a
//!   long-lived **app password** tied to the user — never the
//!   real account password — which is what we keep in the keyring.
//! - `create_room` posts to the OCS Spreed v4 conversation endpoint
//!   to create a fresh group room and returns the join URL.
//!
//! All HTTP calls go through one configured `reqwest::Client` with
//! a 30-second timeout so a wedged Nextcloud instance can't hang
//! the UI indefinitely.
//!
//! [lf2]: https://docs.nextcloud.com/server/latest/developer_manual/client_apis/LoginFlow/index.html

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const HTTP_TIMEOUT_SECS: u64 = 30;
const USER_AGENT: &str = concat!("Chithi/", env!("CARGO_PKG_VERSION"));

/// Single shared `reqwest::Client` for the whole Talk module so
/// the connection pool persists across the Login Flow v2 poll
/// loop (every 2s for up to five minutes per login). Building a
/// new `Client` for every call would discard the keep-alive
/// pool and re-do TLS setup each time. Initialised lazily on
/// first use; if it ever fails to build (TLS init catastrophe)
/// every call returns the same error.
fn http_client() -> Result<&'static reqwest::Client> {
    static CLIENT: std::sync::OnceLock<std::result::Result<reqwest::Client, String>> =
        std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
                .user_agent(USER_AGENT)
                .build()
                .map_err(|e| format!("talk http client: {}", e))
        })
        .as_ref()
        .map_err(|e| Error::Other(e.clone()))
}

/// Result of `POST /index.php/login/v2`. The `login` URL is what the
/// user opens in their browser; `poll.token` is what we send back to
/// `poll.endpoint` until the user finishes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginFlowStart {
    pub login: String,
    pub poll: LoginPoll,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginPoll {
    pub token: String,
    pub endpoint: String,
}

/// Successful poll result. `app_password` is what we keep in the
/// keyring; the user's real password is never seen by us.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginFlowResult {
    pub server: String,
    #[serde(rename = "loginName")]
    pub login_name: String,
    #[serde(rename = "appPassword")]
    pub app_password: String,
}

/// Kick off Nextcloud Login Flow v2 against `server_url`. The trailing
/// slash is normalized; the function works for both bare-host
/// (`https://cloud.example.com`) and path-prefixed installs
/// (`https://example.com/cloud`).
pub async fn login_flow_v2_start(server_url: &str) -> Result<LoginFlowStart> {
    let url = format!("{}/index.php/login/v2", normalize_base_url(server_url));
    let resp = http_client()?
        .post(&url)
        .send()
        .await
        .map_err(|e| Error::Other(format!("talk login_flow_v2_start request: {}", e)))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Other(format!(
            "talk login_flow_v2_start: {} ({})",
            status,
            body.chars().take(200).collect::<String>(),
        )));
    }
    resp.json::<LoginFlowStart>()
        .await
        .map_err(|e| Error::Other(format!("talk login_flow_v2_start parse: {}", e)))
}

/// Poll the login-flow endpoint once. Returns:
/// - `Ok(Some(result))` when the user has completed the in-browser
///   flow and the server has issued credentials.
/// - `Ok(None)` while the flow is still pending (HTTP 404 per the
///   spec — Nextcloud uses 404 to mean "not yet").
/// - `Err(_)` on any other failure.
pub async fn login_flow_v2_poll(
    poll_endpoint: &str,
    poll_token: &str,
) -> Result<Option<LoginFlowResult>> {
    let resp = http_client()?
        .post(poll_endpoint)
        .form(&[("token", poll_token)])
        .send()
        .await
        .map_err(|e| Error::Other(format!("talk login_flow_v2_poll request: {}", e)))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Other(format!(
            "talk login_flow_v2_poll: {} ({})",
            status,
            body.chars().take(200).collect::<String>(),
        )));
    }
    let result = resp
        .json::<LoginFlowResult>()
        .await
        .map_err(|e| Error::Other(format!("talk login_flow_v2_poll parse: {}", e)))?;
    Ok(Some(result))
}

/// Drive the full Login Flow v2 to completion. Polls every 2 seconds
/// up to a configurable timeout (default 5 minutes) — long enough
/// for the user to finish a browser-based SSO redirect, short enough
/// not to leave the UI hung if they walk away.
pub async fn login_flow_v2_complete(
    flow: &LoginFlowStart,
    timeout_secs: u64,
) -> Result<LoginFlowResult> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs.max(30));
    loop {
        if let Some(result) = login_flow_v2_poll(&flow.poll.endpoint, &flow.poll.token).await? {
            return Ok(result);
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::Other(
                "Nextcloud Talk sign-in timed out — finish the browser login and try again".into(),
            ));
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Create a fresh Talk conversation (group room) and return the
/// browser-facing join URL. `room_name` becomes the conversation
/// title in Nextcloud Talk; we pick a sensible default if the caller
/// hands us an empty string.
pub async fn create_room(
    server: &str,
    login_name: &str,
    app_password: &str,
    room_name: &str,
) -> Result<String> {
    let base = normalize_base_url(server);
    let url = format!("{}/ocs/v2.php/apps/spreed/api/v4/room", base);
    let name = if room_name.trim().is_empty() {
        "Meeting"
    } else {
        room_name
    };
    let resp = http_client()?
        .post(&url)
        .basic_auth(login_name, Some(app_password))
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .form(&[
            ("roomType", "2"), // 2 = group conversation
            ("roomName", name),
        ])
        .send()
        .await
        .map_err(|e| Error::Other(format!("talk create_room request: {}", e)))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Other(format!(
            "talk create_room: {} ({})",
            status,
            body.chars().take(500).collect::<String>(),
        )));
    }
    // OCS responses wrap the payload as {ocs: {meta: {...}, data: {...}}}.
    // We only need data.token; the join URL is base + /call/<token>.
    let payload: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Other(format!("talk create_room parse: {}", e)))?;
    let token = payload["ocs"]["data"]["token"].as_str().ok_or_else(|| {
        Error::Other(format!(
            "talk create_room: response missing ocs.data.token: {}",
            payload.to_string().chars().take(300).collect::<String>(),
        ))
    })?;
    Ok(format!("{}/call/{}", base, token))
}

/// Strip trailing slashes from a Nextcloud base URL so callers can
/// concatenate paths without producing `//` segments. Empty input
/// returns empty.
fn normalize_base_url(server: &str) -> String {
    server.trim_end_matches('/').to_string()
}

/// `MeetProvider` implementor for Nextcloud Talk. Stateless — each
/// `create_url` call reads the account's URL from its meet binding
/// and the app password from the keyring.
pub struct TalkProvider;

#[async_trait::async_trait]
impl crate::meet::MeetProvider for TalkProvider {
    fn protocol(&self) -> &'static str {
        "talk"
    }
    fn label(&self) -> &'static str {
        "Nextcloud Talk"
    }
    async fn create_url(
        &self,
        account: &crate::db::accounts::AccountFull,
        name: &str,
    ) -> Result<String> {
        let url = account.meet_url.trim();
        if url.is_empty() {
            return Err(Error::Other(
                "Nextcloud Talk: account has no server URL configured".into(),
            ));
        }
        let app_password = match crate::oauth::load_tokens(&account.id)? {
            Some(t) => t.access_token,
            None => {
                return Err(Error::Other(
                    "Nextcloud Talk: no app password in keyring; sign in again".into(),
                ));
            }
        };
        create_room(url, &account.username, &app_password, name).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_trailing_slashes() {
        assert_eq!(normalize_base_url("https://x/"), "https://x");
        assert_eq!(normalize_base_url("https://x///"), "https://x");
        assert_eq!(normalize_base_url("https://x"), "https://x");
        assert_eq!(normalize_base_url(""), "");
    }
}
