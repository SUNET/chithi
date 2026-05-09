//! Zoom integration (#148). Implements the `MeetProvider` trait
//! against the [Zoom Meeting API][create-meeting] for user-managed
//! OAuth apps registered on Zoom Marketplace.
//!
//! Auth: standard OAuth 2.0 Authorization Code + PKCE. The Tauri
//! commands in `commands/meet.rs` drive `oauth::get_auth_url` /
//! `wait_for_callback` / `exchange_code` against `oauth::ZOOM`,
//! storing the resulting tokens in the keyring under the new
//! account id.
//!
//! Refresh: Zoom access tokens expire after 60 minutes. Each
//! `create_url` call inspects the cached `expires_at` and runs
//! `oauth::refresh_access_token(&oauth::ZOOM, refresh_token)`
//! when the access token is within a minute of expiry. The
//! refreshed pair is written back to the keyring so the next
//! call picks them up.
//!
//! [create-meeting]: https://developers.zoom.us/docs/api/meetings/methods/#tag/meetings/POST/users/{userId}/meetings

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const HTTP_TIMEOUT_SECS: u64 = 30;
const USER_AGENT: &str = concat!("Chithi/", env!("CARGO_PKG_VERSION"));

/// Single shared `reqwest::Client` for the Zoom module — same
/// pattern as in `talk.rs` / `matrix.rs`. Lazily initialised on
/// first call.
fn http_client() -> Result<&'static reqwest::Client> {
    static CLIENT: std::sync::OnceLock<std::result::Result<reqwest::Client, String>> =
        std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
                .user_agent(USER_AGENT)
                .build()
                .map_err(|e| format!("zoom http client: {}", e))
        })
        .as_ref()
        .map_err(|e| Error::Other(e.clone()))
}

/// Subset of the create-meeting response we care about.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct CreateMeetingResponse {
    pub id: serde_json::Value,
    pub join_url: String,
    #[serde(default)]
    pub password: String,
}

/// Create a scheduled meeting on the user's Zoom account and
/// return the join URL. `topic` becomes the meeting subject in
/// Zoom (visible in the user's meeting list); we pick a default
/// when the caller hands us an empty string.
///
/// `type=2` is "scheduled meeting"; we leave `start_time` and
/// `duration` unset so the meeting is open-ended (Zoom treats it
/// as joinable any time today). A future iteration could plumb
/// the calendar event's `start` / `end` through.
pub async fn create_meeting(access_token: &str, topic: &str) -> Result<String> {
    let topic = if topic.trim().is_empty() {
        "Meeting"
    } else {
        topic
    };
    let body = serde_json::json!({
        "topic": topic,
        "type": 2,
        "settings": {
            "join_before_host": true,
            "waiting_room": false,
        },
    });
    let resp = http_client()?
        .post("https://api.zoom.us/v2/users/me/meetings")
        .bearer_auth(access_token)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Other(format!("zoom create_meeting request: {}", e)))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Other(format!(
            "zoom create_meeting: {} ({})",
            status,
            body.chars().take(500).collect::<String>(),
        )));
    }
    let payload: CreateMeetingResponse = resp
        .json()
        .await
        .map_err(|e| Error::Other(format!("zoom create_meeting parse: {}", e)))?;
    Ok(payload.join_url)
}

/// Get a fresh Zoom access token for `account_id`, refreshing
/// against `oauth::ZOOM.token_url` when the cached one is
/// expired (or about to be — `is_expired` carries a 60-second
/// safety margin). Persists the new pair to the keyring.
pub async fn get_access_token(account_id: &str) -> Result<String> {
    let tokens = crate::oauth::load_tokens(account_id)?
        .ok_or_else(|| Error::Other("Zoom: no tokens in keyring; sign in again".into()))?;
    if !tokens.is_expired() {
        return Ok(tokens.access_token);
    }
    let refresh_token = tokens.refresh_token.ok_or_else(|| {
        Error::Other("Zoom: access token expired and no refresh token; sign in again".into())
    })?;
    let new_tokens =
        crate::oauth::refresh_access_token(&crate::oauth::ZOOM, &refresh_token).await?;
    crate::oauth::store_tokens(account_id, &new_tokens)?;
    Ok(new_tokens.access_token)
}

/// `MeetProvider` implementor for Zoom. Stateless — each call
/// reads tokens from the keyring under the account id.
pub struct ZoomProvider;

#[async_trait::async_trait]
impl crate::meet::MeetProvider for ZoomProvider {
    fn protocol(&self) -> &'static str {
        "zoom"
    }
    fn label(&self) -> &'static str {
        "Zoom"
    }
    async fn create_url(
        &self,
        account: &crate::db::accounts::AccountFull,
        name: &str,
    ) -> Result<String> {
        let access_token = get_access_token(&account.id).await?;
        create_meeting(&access_token, name).await
    }
}
