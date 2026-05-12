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
/// `type=2` is "scheduled meeting". When `start_time` is supplied
/// it goes through verbatim (must be ISO 8601 UTC like
/// `2026-05-12T14:00:00Z`) so the meeting lands on the host's
/// schedule on the right day. Without it, Zoom treats the meeting
/// as joinable any time today, which is what made every meeting
/// show up as scheduled for today regardless of the calendar
/// event's date.
pub async fn create_meeting(
    access_token: &str,
    topic: &str,
    start_time: Option<&str>,
    duration_minutes: Option<u32>,
) -> Result<crate::meet::MeetCreateResult> {
    let topic = if topic.trim().is_empty() {
        "Meeting"
    } else {
        topic
    };
    let mut body = serde_json::json!({
        "topic": topic,
        "type": 2,
        "settings": {
            "join_before_host": true,
            "waiting_room": false,
        },
    });
    if let Some(start) = start_time {
        body["start_time"] = serde_json::Value::String(start.to_string());
        // `start_time` ending in `Z` is GMT per Zoom's API. Pin
        // `timezone` to UTC so Zoom doesn't reinterpret the slot
        // against the account's default zone.
        body["timezone"] = serde_json::Value::String("UTC".to_string());
    }
    if let Some(minutes) = duration_minutes {
        body["duration"] = serde_json::Value::Number(minutes.into());
    }
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
    // Zoom returns the meeting id as a JSON number (e.g. `1234567890`)
    // or, on some account types, a string. Normalise to a string here
    // so the meeting_id is comparable as a path segment when we later
    // call DELETE / PATCH /v2/meetings/{id}.
    let meeting_id = match &payload.id {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    Ok(crate::meet::MeetCreateResult {
        join_url: payload.join_url,
        meeting_id,
    })
}

/// Delete a Zoom meeting. The 404-OK fallthrough handles the case
/// where the user (or another client) already removed the meeting
/// in Zoom's own UI: we still want the local cleanup to succeed.
async fn api_delete_meeting(access_token: &str, meeting_id: &str) -> Result<()> {
    let url = format!(
        "https://api.zoom.us/v2/meetings/{}",
        urlencoding::encode(meeting_id),
    );
    let resp = http_client()?
        .delete(&url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| Error::Other(format!("zoom delete_meeting request: {}", e)))?;
    let status = resp.status();
    if status.is_success() || status.as_u16() == 204 || status.as_u16() == 404 {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(Error::Other(format!(
        "zoom delete_meeting: {} ({})",
        status,
        body.chars().take(500).collect::<String>(),
    )))
}

/// Patch a Zoom meeting's start time + duration. Pinned to UTC for
/// the same reason as `create_meeting` (the caller hands us an ISO
/// UTC timestamp). Returns Ok on 204 No Content, which is what
/// Zoom emits on a successful PATCH.
async fn api_update_meeting_schedule(
    access_token: &str,
    meeting_id: &str,
    start_time: &str,
    duration_minutes: u32,
) -> Result<()> {
    let url = format!(
        "https://api.zoom.us/v2/meetings/{}",
        urlencoding::encode(meeting_id),
    );
    let body = serde_json::json!({
        "start_time": start_time,
        "duration": duration_minutes,
        "timezone": "UTC",
    });
    let resp = http_client()?
        .patch(&url)
        .bearer_auth(access_token)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Other(format!("zoom update_meeting request: {}", e)))?;
    let status = resp.status();
    if status.is_success() || status.as_u16() == 204 {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(Error::Other(format!(
        "zoom update_meeting: {} ({})",
        status,
        body.chars().take(500).collect::<String>(),
    )))
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
        start_time: Option<&str>,
        duration_minutes: Option<u32>,
    ) -> Result<crate::meet::MeetCreateResult> {
        let access_token = get_access_token(&account.id).await?;
        create_meeting(&access_token, name, start_time, duration_minutes).await
    }

    async fn delete_meeting(
        &self,
        account: &crate::db::accounts::AccountFull,
        meeting_id: &str,
    ) -> Result<()> {
        let access_token = get_access_token(&account.id).await?;
        api_delete_meeting(&access_token, meeting_id).await
    }

    async fn reschedule_meeting(
        &self,
        account: &crate::db::accounts::AccountFull,
        meeting_id: &str,
        start_time: &str,
        duration_minutes: u32,
    ) -> Result<()> {
        let access_token = get_access_token(&account.id).await?;
        api_update_meeting_schedule(&access_token, meeting_id, start_time, duration_minutes).await
    }
}
