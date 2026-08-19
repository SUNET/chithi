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
//! Refresh: Zoom access tokens expire after 60 minutes. The injected
//! provider credential service serializes refresh and persistence per account.
//!
//! [create-meeting]: https://developers.zoom.us/docs/api/meetings/methods/#tag/meetings/POST/users/{userId}/meetings

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const HTTP_TIMEOUT_SECS: u64 = 30;
const USER_AGENT: &str = concat!("Chithi/", env!("CARGO_PKG_VERSION"));
const ZOOM_API_ROOT: &str = "https://api.zoom.us/v2";

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

/// Stable Zoom principal identifiers returned by `GET /users/me`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ZoomUserIdentity {
    #[serde(rename = "id")]
    pub user_id: String,
    pub account_id: String,
}

pub async fn current_user_identity_with_client(
    access_token: &str,
    client: &reqwest::Client,
    api_root: &str,
) -> Result<ZoomUserIdentity> {
    let response = client
        .get(zoom_api_url(api_root, "users/me"))
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|error| Error::Other(format!("zoom users/me request: {error}")))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(Error::Other(format!(
            "zoom users/me: {} ({})",
            status,
            body.chars().take(500).collect::<String>()
        )));
    }
    let identity: ZoomUserIdentity = response
        .json()
        .await
        .map_err(|error| Error::Other(format!("zoom users/me parse: {error}")))?;
    if identity.user_id.trim().is_empty() || identity.account_id.trim().is_empty() {
        return Err(Error::Other(
            "zoom users/me returned an incomplete identity".into(),
        ));
    }
    Ok(identity)
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
    create_meeting_with_client(
        access_token,
        topic,
        start_time,
        duration_minutes,
        http_client()?,
        ZOOM_API_ROOT,
    )
    .await
}

/// Create a Zoom meeting using an explicit HTTP client and API root.
pub async fn create_meeting_with_client(
    access_token: &str,
    topic: &str,
    start_time: Option<&str>,
    duration_minutes: Option<u32>,
    client: &reqwest::Client,
    api_root: &str,
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
    let resp = client
        .post(zoom_api_url(api_root, "users/me/meetings"))
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
    api_delete_meeting_with_client(access_token, meeting_id, http_client()?, ZOOM_API_ROOT).await
}

async fn api_delete_meeting_with_client(
    access_token: &str,
    meeting_id: &str,
    client: &reqwest::Client,
    api_root: &str,
) -> Result<()> {
    let url = zoom_api_url(
        api_root,
        &format!("meetings/{}", urlencoding::encode(meeting_id)),
    );
    let resp = client
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

/// Rename a Zoom meeting. Same endpoint as the schedule PATCH but
/// with only the `topic` field set. Needs `meeting:update:meeting`.
async fn api_update_meeting_topic(access_token: &str, meeting_id: &str, topic: &str) -> Result<()> {
    api_update_meeting_topic_with_client(
        access_token,
        meeting_id,
        topic,
        http_client()?,
        ZOOM_API_ROOT,
    )
    .await
}

async fn api_update_meeting_topic_with_client(
    access_token: &str,
    meeting_id: &str,
    topic: &str,
    client: &reqwest::Client,
    api_root: &str,
) -> Result<()> {
    let url = zoom_api_url(
        api_root,
        &format!("meetings/{}", urlencoding::encode(meeting_id)),
    );
    let body = serde_json::json!({ "topic": topic });
    let resp = client
        .patch(&url)
        .bearer_auth(access_token)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Other(format!("zoom update_topic request: {}", e)))?;
    let status = resp.status();
    if status.is_success() || status.as_u16() == 204 {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(Error::Other(format!(
        "zoom update_topic: {} ({})",
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
    api_update_meeting_schedule_with_client(
        access_token,
        meeting_id,
        start_time,
        duration_minutes,
        http_client()?,
        ZOOM_API_ROOT,
    )
    .await
}

async fn api_update_meeting_schedule_with_client(
    access_token: &str,
    meeting_id: &str,
    start_time: &str,
    duration_minutes: u32,
    client: &reqwest::Client,
    api_root: &str,
) -> Result<()> {
    let url = zoom_api_url(
        api_root,
        &format!("meetings/{}", urlencoding::encode(meeting_id)),
    );
    let body = serde_json::json!({
        "start_time": start_time,
        "duration": duration_minutes,
        "timezone": "UTC",
    });
    let resp = client
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

fn zoom_api_url(api_root: &str, path: &str) -> String {
    format!(
        "{}/{}",
        api_root.trim_end_matches('/'),
        path.trim_start_matches('/'),
    )
}

/// `MeetProvider` implementor for Zoom. Stateless; each call obtains
/// credentials and transport settings from the injected provider services.
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
        ctx: &crate::meet::MeetProviderCtx<'_>,
        account: &crate::db::accounts::AccountFull,
        name: &str,
        start_time: Option<&str>,
        duration_minutes: Option<u32>,
    ) -> Result<crate::meet::MeetCreateResult> {
        let access_token = ctx
            .services
            .credentials()
            .zoom_access_token(&account.id)
            .await?;
        create_meeting_with_client(
            &access_token,
            name,
            start_time,
            duration_minutes,
            &ctx.services.transports.zoom_http,
            &ctx.services.transports.zoom_api_root,
        )
        .await
    }

    async fn delete_meeting(
        &self,
        ctx: &crate::meet::MeetProviderCtx<'_>,
        account: &crate::db::accounts::AccountFull,
        meeting_id: &str,
    ) -> Result<()> {
        let access_token = ctx
            .services
            .credentials()
            .zoom_access_token(&account.id)
            .await?;
        api_delete_meeting_with_client(
            &access_token,
            meeting_id,
            &ctx.services.transports.zoom_http,
            &ctx.services.transports.zoom_api_root,
        )
        .await
    }

    async fn reschedule_meeting(
        &self,
        ctx: &crate::meet::MeetProviderCtx<'_>,
        account: &crate::db::accounts::AccountFull,
        meeting_id: &str,
        start_time: &str,
        duration_minutes: u32,
    ) -> Result<()> {
        let access_token = ctx
            .services
            .credentials()
            .zoom_access_token(&account.id)
            .await?;
        api_update_meeting_schedule_with_client(
            &access_token,
            meeting_id,
            start_time,
            duration_minutes,
            &ctx.services.transports.zoom_http,
            &ctx.services.transports.zoom_api_root,
        )
        .await
    }

    async fn update_topic(
        &self,
        ctx: &crate::meet::MeetProviderCtx<'_>,
        account: &crate::db::accounts::AccountFull,
        meeting_id: &str,
        topic: &str,
    ) -> Result<()> {
        let access_token = ctx
            .services
            .credentials()
            .zoom_access_token(&account.id)
            .await?;
        api_update_meeting_topic_with_client(
            &access_token,
            meeting_id,
            topic,
            &ctx.services.transports.zoom_http,
            &ctx.services.transports.zoom_api_root,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn mock_server(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let body = body.to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
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
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            )
            .unwrap();
            String::from_utf8(request).unwrap()
        });
        (format!("http://{address}"), server)
    }

    #[test]
    fn zoom_api_url_normalizes_boundary_slashes() {
        assert_eq!(
            zoom_api_url("http://localhost:1234/v2/", "/users/me/meetings"),
            "http://localhost:1234/v2/users/me/meetings"
        );
    }

    #[tokio::test]
    async fn create_meeting_uses_injected_root_and_bearer_auth() {
        let (root, server) = mock_server(
            "201 Created",
            r#"{"id":123456,"join_url":"https://zoom.example/j/123456"}"#,
        );
        let api_root = format!("{root}/injected/v2/");

        let result = create_meeting_with_client(
            "zoom-token",
            "Planning",
            Some("2026-08-10T09:00:00Z"),
            Some(45),
            &reqwest::Client::new(),
            &api_root,
        )
        .await
        .unwrap();
        let request = server.join().unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        let payload: serde_json::Value = serde_json::from_str(body).unwrap();

        assert!(headers.starts_with("POST /injected/v2/users/me/meetings HTTP/1.1\r\n"));
        assert!(headers
            .to_ascii_lowercase()
            .contains("authorization: bearer zoom-token"));
        assert_eq!(payload["topic"], "Planning");
        assert_eq!(payload["type"], 2);
        assert_eq!(payload["start_time"], "2026-08-10T09:00:00Z");
        assert_eq!(payload["timezone"], "UTC");
        assert_eq!(payload["duration"], 45);
        assert_eq!(payload["settings"]["join_before_host"], true);
        assert_eq!(payload["settings"]["waiting_room"], false);
        assert_eq!(result.meeting_id, "123456");
        assert_eq!(result.join_url, "https://zoom.example/j/123456");
    }

    #[tokio::test]
    async fn current_user_identity_uses_injected_root_and_bearer_auth() {
        let (root, server) = mock_server(
            "200 OK",
            r#"{"id":"zoom-user","account_id":"zoom-account"}"#,
        );
        let api_root = format!("{root}/injected/v2/");

        let identity =
            current_user_identity_with_client("zoom-token", &reqwest::Client::new(), &api_root)
                .await
                .unwrap();
        let request = server.join().unwrap();

        assert!(request.starts_with("GET /injected/v2/users/me HTTP/1.1\r\n"));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer zoom-token"));
        assert_eq!(identity.user_id, "zoom-user");
        assert_eq!(identity.account_id, "zoom-account");
    }
}
