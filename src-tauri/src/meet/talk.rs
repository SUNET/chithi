//! Nextcloud Talk integration (#148).
//!
//! Two flows:
//! - `login_flow_v2_start_with_client` / `login_flow_v2_poll_with_client`
//!   implement the
//!   [Nextcloud Login Flow v2 spec][lf2]. The server hands us a
//!   browser-facing `login` URL plus a `poll` endpoint+token; we
//!   open the URL in the user's browser, then poll until the user
//!   finishes the in-browser flow. The poll response carries a
//!   long-lived **app password** tied to the user — never the
//!   real account password — which is what we keep in the keyring.
//! - `create_room_with_client` posts to the OCS Spreed v4 conversation
//!   endpoint to create a fresh group room and returns the join URL.
//!
//! All HTTP calls use the caller's injected `reqwest::Client`. The
//! production provider transport shares its connection pool across
//! the poll loop and applies a 30-second request timeout.
//!
//! [lf2]: https://docs.nextcloud.com/server/latest/developer_manual/client_apis/LoginFlow/index.html

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const DELETE_RESPONSE_LIMIT: usize = 64 * 1024;
const DELETE_DIAGNOSTIC_LIMIT: usize = 500;

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
/// (`https://example.com/cloud`). Uses the explicit HTTP client
/// supplied by the caller.
pub async fn login_flow_v2_start_with_client(
    server_url: &str,
    client: &reqwest::Client,
) -> Result<LoginFlowStart> {
    crate::mail::url_validation::require_https(server_url)?;
    let url = format!("{}/index.php/login/v2", normalize_base_url(server_url));
    let resp = client
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
    let flow = resp
        .json::<LoginFlowStart>()
        .await
        .map_err(|e| Error::Other(format!("talk login_flow_v2_start parse: {}", e)))?;
    validate_poll_endpoint(server_url, &flow.poll.endpoint)?;
    Ok(flow)
}

/// Poll the login-flow endpoint once. Returns:
/// - `Ok(Some(result))` when the user has completed the in-browser
///   flow and the server has issued credentials.
/// - `Ok(None)` while the flow is still pending (HTTP 404 per the
///   spec — Nextcloud uses 404 to mean "not yet").
/// - `Err(_)` on any other failure.
///
/// Uses the explicit HTTP client supplied by the caller.
pub async fn login_flow_v2_poll_with_client(
    poll_endpoint: &str,
    poll_token: &str,
    client: &reqwest::Client,
) -> Result<Option<LoginFlowResult>> {
    crate::mail::url_validation::require_https(poll_endpoint)?;
    let resp = client
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
/// not to leave the UI hung if they walk away. The same explicit
/// HTTP client is reused for every poll so its connection pool is
/// retained.
pub async fn login_flow_v2_complete_with_client(
    flow: &LoginFlowStart,
    timeout_secs: u64,
    client: &reqwest::Client,
) -> Result<LoginFlowResult> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs.max(30));
    loop {
        if let Some(result) =
            login_flow_v2_poll_with_client(&flow.poll.endpoint, &flow.poll.token, client).await?
        {
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

/// Create a fresh Talk conversation (group room) and return both
/// the join URL and the room token (Spreed's per-conversation
/// identifier — what `DELETE /room/{token}` expects later when the
/// event is cancelled). `room_name` becomes the conversation title
/// in Nextcloud Talk; we pick a sensible default if the caller hands
/// us an empty string. Uses the explicit HTTP client supplied by
/// the caller.
pub async fn create_room_with_client(
    server: &str,
    login_name: &str,
    app_password: &str,
    room_name: &str,
    client: &reqwest::Client,
) -> Result<crate::meet::MeetCreateResult> {
    crate::mail::url_validation::require_https(server)?;
    let base = normalize_base_url(server);
    let url = format!("{}/ocs/v2.php/apps/spreed/api/v4/room", base);
    let name = if room_name.trim().is_empty() {
        "Meeting"
    } else {
        room_name
    };
    let resp = client
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
    Ok(crate::meet::MeetCreateResult {
        join_url: format!("{}/call/{}", base, token),
        meeting_id: token.to_string(),
    })
}

/// Rename an existing Talk conversation. Implemented against the
/// OCS Spreed v4 rename endpoint (PUT /room/{token}), which takes
/// a single `roomName` form field. 404 means the room is already
/// gone, in which case the rename is a no-op from our perspective.
/// Uses the explicit HTTP client supplied by the caller.
pub async fn rename_room_with_client(
    server: &str,
    login_name: &str,
    app_password: &str,
    token: &str,
    new_name: &str,
    client: &reqwest::Client,
) -> Result<()> {
    crate::mail::url_validation::require_https(server)?;
    let base = normalize_base_url(server);
    let url = format!(
        "{}/ocs/v2.php/apps/spreed/api/v4/room/{}",
        base,
        urlencoding::encode(token),
    );
    let name = if new_name.trim().is_empty() {
        "Meeting"
    } else {
        new_name
    };
    let resp = client
        .put(&url)
        .basic_auth(login_name, Some(app_password))
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .form(&[("roomName", name)])
        .send()
        .await
        .map_err(|e| Error::Other(format!("talk rename_room request: {}", e)))?;
    let status = resp.status();
    if status.is_success() || status.as_u16() == 404 {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(Error::Other(format!(
        "talk rename_room: {} ({})",
        status,
        body.chars().take(500).collect::<String>(),
    )))
}

#[derive(Deserialize)]
struct OcsDeleteEnvelope {
    ocs: OcsDeleteResponse,
}

#[derive(Deserialize)]
struct OcsDeleteResponse {
    meta: OcsDeleteMeta,
}

#[derive(Deserialize)]
struct OcsDeleteMeta {
    status: OcsDeleteStatus,
    statuscode: u16,
    message: String,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum OcsDeleteStatus {
    Ok,
    Failure,
}

#[derive(Debug, PartialEq, Eq)]
enum OcsDeleteMessageKind {
    Success,
    Unknown,
}

fn classify_ocs_delete_message(message: &str) -> OcsDeleteMessageKind {
    let message = message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if matches!(
        message.as_str(),
        "ok" | "conversation deleted successfully"
            | "room deleted successfully"
            | "conversation successfully deleted"
            | "room successfully deleted"
    ) {
        return OcsDeleteMessageKind::Success;
    }
    OcsDeleteMessageKind::Unknown
}

/// Delete a Talk conversation by token using the same auth pair as
/// `create_room_with_client` and an explicit HTTP client. Cleanup is
/// idempotent only when the OCS response positively reports success;
/// HTTP 404 or missing metadata alone is not enough evidence to drop
/// durable ownership, so those responses remain retryable errors.
pub async fn delete_room_with_client(
    server: &str,
    login_name: &str,
    app_password: &str,
    token: &str,
    client: &reqwest::Client,
) -> Result<()> {
    crate::mail::url_validation::require_https(server)?;
    let base = normalize_base_url(server);
    let url = format!(
        "{}/ocs/v2.php/apps/spreed/api/v4/room/{}",
        base,
        urlencoding::encode(token),
    );
    let resp = client
        .delete(&url)
        .basic_auth(login_name, Some(app_password))
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| Error::Other(format!("talk delete_room request: {}", e)))?;
    let status = resp.status();

    if resp
        .content_length()
        .is_some_and(|length| length > u64::try_from(DELETE_RESPONSE_LIMIT).unwrap_or(u64::MAX))
    {
        return Err(Error::Other(format!(
            "talk delete_room: {} response exceeds {} bytes",
            status, DELETE_RESPONSE_LIMIT,
        )));
    }

    let mut resp = resp;
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| Error::Other(format!("talk delete_room response: {}", e)))?
    {
        if body.len().saturating_add(chunk.len()) > DELETE_RESPONSE_LIMIT {
            let diagnostic = String::from_utf8_lossy(&body)
                .chars()
                .take(DELETE_DIAGNOSTIC_LIMIT)
                .collect::<String>();
            return Err(Error::Other(format!(
                "talk delete_room: {} response exceeds {} bytes ({})",
                status, DELETE_RESPONSE_LIMIT, diagnostic,
            )));
        }
        body.extend_from_slice(&chunk);
    }
    let diagnostic = String::from_utf8_lossy(&body)
        .chars()
        .take(DELETE_DIAGNOSTIC_LIMIT)
        .collect::<String>();

    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return Err(Error::Other(format!(
            "talk delete_room: authorization failed: {} ({})",
            status, diagnostic,
        )));
    }
    if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
        return Err(Error::Other(format!(
            "talk delete_room: {} ({})",
            status, diagnostic,
        )));
    }

    // OCS v2 communicates application success in the envelope, often while
    // returning HTTP 200. A 204/empty body is therefore not proof of deletion.
    let payload: OcsDeleteEnvelope = serde_json::from_slice(&body).map_err(|e| {
        Error::Other(format!(
            "talk delete_room: invalid OCS metadata: {} ({})",
            e, diagnostic,
        ))
    })?;
    let meta = payload.ocs.meta;
    let message_kind = classify_ocs_delete_message(&meta.message);
    if meta.status == OcsDeleteStatus::Ok
        && matches!(meta.statuscode, 100 | 200)
        && status == reqwest::StatusCode::OK
        && message_kind == OcsDeleteMessageKind::Success
    {
        return Ok(());
    }
    let ocs_status = match meta.status {
        OcsDeleteStatus::Ok => "ok",
        OcsDeleteStatus::Failure => "failure",
    };
    let message = meta
        .message
        .chars()
        .take(DELETE_DIAGNOSTIC_LIMIT)
        .collect::<String>();
    Err(Error::Other(format!(
        "talk delete_room: OCS {} {}: {} (HTTP {})",
        ocs_status, meta.statuscode, message, status,
    )))
}

/// Strip trailing slashes from a Nextcloud base URL so callers can
/// concatenate paths without producing `//` segments. Empty input
/// returns empty.
fn normalize_base_url(server: &str) -> String {
    server.trim_end_matches('/').to_string()
}

fn validate_poll_endpoint(server: &str, endpoint: &str) -> Result<()> {
    crate::mail::url_validation::require_https(endpoint)?;
    let server = url::Url::parse(server)
        .map_err(|e| Error::Other(format!("Invalid Nextcloud server URL: {}", e)))?;
    let endpoint = url::Url::parse(endpoint)
        .map_err(|e| Error::Other(format!("Invalid Nextcloud poll URL: {}", e)))?;
    if server.origin() != endpoint.origin() {
        return Err(Error::Other(
            "Nextcloud login poll endpoint must use the server origin".into(),
        ));
    }
    Ok(())
}

/// `MeetProvider` implementor for Nextcloud Talk. Stateless — each
/// `create_url` call reads the account's URL from its meet binding
/// and the app password from the injected provider services.
pub struct TalkProvider;

#[async_trait::async_trait]
impl crate::meet::MeetProvider for TalkProvider {
    fn protocol(&self) -> &'static str {
        "talk"
    }
    async fn create_url(
        &self,
        ctx: &crate::meet::MeetProviderCtx<'_>,
        account: &crate::db::accounts::AccountFull,
        name: &str,
        _start_time: Option<&str>,
        _duration_minutes: Option<u32>,
    ) -> Result<crate::meet::MeetCreateResult> {
        // Talk conversations are persistent rooms, not time-bound
        // slots, so the calendar event's start/duration are ignored.
        let url = account.meet_url.trim();
        if url.is_empty() {
            return Err(Error::Other(
                "Nextcloud Talk: account has no server URL configured".into(),
            ));
        }
        let app_password = ctx
            .services
            .credentials()
            .talk_app_password(&account.id)
            .await?;
        create_room_with_client(
            url,
            &account.username,
            &app_password,
            name,
            &ctx.services.transports.talk_http,
        )
        .await
    }

    async fn delete_meeting(
        &self,
        ctx: &crate::meet::MeetProviderCtx<'_>,
        account: &crate::db::accounts::AccountFull,
        meeting_id: &str,
    ) -> Result<()> {
        let url = account.meet_url.trim();
        if url.is_empty() {
            return Err(Error::Other(
                "Nextcloud Talk: account has no server URL configured".into(),
            ));
        }
        let app_password = ctx
            .services
            .credentials()
            .talk_app_password(&account.id)
            .await?;
        delete_room_with_client(
            url,
            &account.username,
            &app_password,
            meeting_id,
            &ctx.services.transports.talk_http,
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
        let url = account.meet_url.trim();
        if url.is_empty() {
            return Err(Error::Other(
                "Nextcloud Talk: account has no server URL configured".into(),
            ));
        }
        let app_password = ctx
            .services
            .credentials()
            .talk_app_password(&account.id)
            .await?;
        rename_room_with_client(
            url,
            &account.username,
            &app_password,
            meeting_id,
            topic,
            &ctx.services.transports.talk_http,
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

    fn mock_server_without_content_length(
        status: &str,
        body: &str,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let body = body.to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
            )
            .unwrap();
            String::from_utf8(request[..read].to_vec()).unwrap()
        });
        (format!("http://{address}"), server)
    }

    #[test]
    fn normalizes_trailing_slashes() {
        assert_eq!(normalize_base_url("https://x/"), "https://x");
        assert_eq!(normalize_base_url("https://x///"), "https://x");
        assert_eq!(normalize_base_url("https://x"), "https://x");
        assert_eq!(normalize_base_url(""), "");
    }

    #[tokio::test]
    async fn login_flow_start_uses_injected_server_path() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let root = format!("http://{address}");
        let body = format!(
            r#"{{"login":"{root}/cloud/login","poll":{{"token":"poll-token","endpoint":"{root}/cloud/index.php/login/v2/poll"}}}}"#,
        );
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
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            )
            .unwrap();
            String::from_utf8(request).unwrap()
        });
        let server_url = format!("{root}/cloud/");

        let result = login_flow_v2_start_with_client(&server_url, &reqwest::Client::new())
            .await
            .unwrap();
        let request = server.join().unwrap();

        assert!(request.starts_with("POST /cloud/index.php/login/v2 HTTP/1.1\r\n"));
        assert_eq!(result.poll.token, "poll-token");
        assert_eq!(
            result.poll.endpoint,
            format!("{root}/cloud/index.php/login/v2/poll")
        );
    }

    #[tokio::test]
    async fn login_flow_start_rejects_cross_origin_poll_endpoint() {
        let (root, server) = mock_server(
            "200 OK",
            r#"{"login":"https://cloud.example/login","poll":{"token":"poll-token","endpoint":"https://evil.example/poll"}}"#,
        );

        let error = login_flow_v2_start_with_client(&root, &reqwest::Client::new())
            .await
            .unwrap_err();
        server.join().unwrap();

        assert!(error.to_string().contains("server origin"));
    }

    #[tokio::test]
    async fn secret_requests_reject_public_http_urls() {
        let client = reqwest::Client::new();

        assert!(
            login_flow_v2_start_with_client("http://cloud.example", &client,)
                .await
                .is_err()
        );
        assert!(
            login_flow_v2_poll_with_client("http://cloud.example/poll", "poll-token", &client,)
                .await
                .is_err()
        );
        assert!(create_room_with_client(
            "http://cloud.example",
            "alice",
            "app-secret",
            "Meeting",
            &client,
        )
        .await
        .is_err());
        assert!(rename_room_with_client(
            "http://cloud.example",
            "alice",
            "app-secret",
            "room-token",
            "Renamed",
            &client,
        )
        .await
        .is_err());
        assert!(delete_room_with_client(
            "http://cloud.example",
            "alice",
            "app-secret",
            "room-token",
            &client,
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn login_flow_poll_posts_token_and_treats_404_as_pending() {
        let (root, server) = mock_server("404 Not Found", "");
        let endpoint = format!("{root}/index.php/login/v2/poll");

        let result =
            login_flow_v2_poll_with_client(&endpoint, "pending-token", &reqwest::Client::new())
                .await
                .unwrap();
        let request = server.join().unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();

        assert!(headers.starts_with("POST /index.php/login/v2/poll HTTP/1.1\r\n"));
        assert!(headers
            .to_ascii_lowercase()
            .contains("content-type: application/x-www-form-urlencoded"));
        assert_eq!(body, "token=pending-token");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn create_room_sends_basic_auth_and_ocs_semantics() {
        let (root, server) = mock_server(
            "200 OK",
            r#"{"ocs":{"meta":{"status":"ok"},"data":{"token":"room-token"}}}"#,
        );
        let server_url = format!("{root}/cloud/");

        let result = create_room_with_client(
            &server_url,
            "alice",
            "app-secret",
            "Team sync",
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        let request = server.join().unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        let headers = headers.to_ascii_lowercase();

        assert!(headers.starts_with("post /cloud/ocs/v2.php/apps/spreed/api/v4/room http/1.1\r\n"));
        assert!(headers.contains("authorization: basic ywxpy2u6yxbwlxnly3jlda=="));
        assert!(headers.contains("ocs-apirequest: true"));
        assert!(headers.contains("accept: application/json"));
        assert!(headers.contains("content-type: application/x-www-form-urlencoded"));
        assert_eq!(body, "roomType=2&roomName=Team+sync");
        assert_eq!(result.meeting_id, "room-token");
        assert_eq!(result.join_url, format!("{root}/cloud/call/room-token"));
    }

    #[tokio::test]
    async fn delete_room_validates_ocs_success_and_request() {
        let (root, server) = mock_server(
            "200 OK",
            r#"{"ocs":{"meta":{"status":"ok","statuscode":200,"message":"OK"},"data":[]}}"#,
        );
        let server_url = format!("{root}/cloud/");

        delete_room_with_client(
            &server_url,
            "alice",
            "app-secret",
            "room token/part",
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        let request = server.join().unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        let headers = headers.to_ascii_lowercase();

        assert!(headers.starts_with(
            "delete /cloud/ocs/v2.php/apps/spreed/api/v4/room/room%20token%2fpart \
             http/1.1\r\n"
        ));
        assert!(headers.contains("authorization: basic ywxpy2u6yxbwlxnly3jlda=="));
        assert!(headers.contains("ocs-apirequest: true"));
        assert!(headers.contains("accept: application/json"));
        assert_eq!(body, "");
    }

    #[tokio::test]
    async fn delete_room_rejects_missing_room_without_proof_of_deletion() {
        for http_status in ["200 OK", "404 Not Found"] {
            let (root, server) = mock_server(
                http_status,
                r#"{"ocs":{"meta":{"status":"failure","statuscode":404,"message":"Room not found"},"data":[]}}"#,
            );

            let error = delete_room_with_client(
                &root,
                "alice",
                "app-secret",
                "missing-room",
                &reqwest::Client::new(),
            )
            .await
            .unwrap_err();
            server.join().unwrap();

            assert!(error
                .to_string()
                .contains("talk delete_room: OCS failure 404: Room not found"));
        }
    }

    #[tokio::test]
    async fn delete_room_rejects_ocs_auth_failure_wrapped_in_http_200() {
        let (root, server) = mock_server(
            "200 OK",
            r#"{"ocs":{"meta":{"status":"failure","statuscode":403,"message":"Forbidden"},"data":[]}}"#,
        );

        let error = delete_room_with_client(
            &root,
            "alice",
            "bad-secret",
            "room-token",
            &reqwest::Client::new(),
        )
        .await
        .unwrap_err();
        server.join().unwrap();

        assert!(error.to_string().contains("OCS failure 403: Forbidden"));
    }

    #[tokio::test]
    async fn delete_room_rejects_http_auth_failure() {
        let (root, server) = mock_server(
            "401 Unauthorized",
            r#"{"ocs":{"meta":{"status":"ok","statuscode":200,"message":"OK"}}}"#,
        );

        let error = delete_room_with_client(
            &root,
            "alice",
            "bad-secret",
            "room-token",
            &reqwest::Client::new(),
        )
        .await
        .unwrap_err();
        server.join().unwrap();

        assert!(error.to_string().contains("authorization failed: 401"));
    }

    #[tokio::test]
    async fn delete_room_rejects_malformed_or_missing_ocs_metadata() {
        let bodies = [
            "",
            r#"{}"#,
            r#"{"ocs":{}}"#,
            r#"{"ocs":{"meta":{"status":"ok","statuscode":200}}}"#,
            r#"{"ocs":{"meta":{"status":"ok","statuscode":"200","message":"OK"}}}"#,
            "not json",
        ];

        for body in bodies {
            let (root, server) = mock_server("200 OK", body);
            let error = delete_room_with_client(
                &root,
                "alice",
                "app-secret",
                "room-token",
                &reqwest::Client::new(),
            )
            .await
            .unwrap_err();
            server.join().unwrap();

            assert!(error.to_string().contains("invalid OCS metadata"));
        }
    }

    #[tokio::test]
    async fn delete_room_rejects_contradictory_ocs_metadata() {
        let bodies = [
            r#"{"ocs":{"meta":{"status":"ok","statuscode":404,"message":"Not found"}}}"#,
            r#"{"ocs":{"meta":{"status":"failure","statuscode":200,"message":"Failed"}}}"#,
            r#"{"ocs":{"meta":{"status":"ok","statuscode":200,"message":"Forbidden"}}}"#,
            r#"{"ocs":{"meta":{"status":"failure","statuscode":404,"message":"Forbidden"}}}"#,
            r#"{"ocs":{"meta":{"status":"ok","statuscode":200,"message":"Done"}}}"#,
            r#"{"ocs":{"meta":{"status":"ok","statuscode":200,"message":"Room was not deleted successfully"}}}"#,
            r#"{"ocs":{"meta":{"status":"failure","statuscode":404,"message":"Room not found; authorization failed"}}}"#,
        ];

        for body in bodies {
            let (root, server) = mock_server("200 OK", body);
            let error = delete_room_with_client(
                &root,
                "alice",
                "app-secret",
                "room-token",
                &reqwest::Client::new(),
            )
            .await
            .unwrap_err();
            server.join().unwrap();

            assert!(error.to_string().contains("talk delete_room: OCS"));
        }
    }

    #[tokio::test]
    async fn delete_room_rejects_undocumented_success_combinations() {
        let cases = [
            (
                "202 Accepted",
                r#"{"ocs":{"meta":{"status":"ok","statuscode":200,"message":"Accepted"}}}"#,
            ),
            (
                "200 OK",
                r#"{"ocs":{"meta":{"status":"ok","statuscode":206,"message":"Partial"}}}"#,
            ),
            (
                "202 Accepted",
                r#"{"ocs":{"meta":{"status":"failure","statuscode":404,"message":"Missing"}}}"#,
            ),
        ];

        for (status, body) in cases {
            let (root, server) = mock_server(status, body);
            assert!(delete_room_with_client(
                &root,
                "alice",
                "app-secret",
                "room-token",
                &reqwest::Client::new(),
            )
            .await
            .is_err());
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn delete_room_rejects_declared_oversized_response() {
        let body = "x".repeat(DELETE_RESPONSE_LIMIT + 1);
        let (root, server) = mock_server("200 OK", &body);

        let error = delete_room_with_client(
            &root,
            "alice",
            "app-secret",
            "room-token",
            &reqwest::Client::new(),
        )
        .await
        .unwrap_err();
        server.join().unwrap();

        assert!(error.to_string().contains("response exceeds"));
    }

    #[tokio::test]
    async fn delete_room_streaming_guard_rejects_oversized_response() {
        let body = "x".repeat(DELETE_RESPONSE_LIMIT + 1);
        let (root, server) = mock_server_without_content_length("200 OK", &body);

        let error = delete_room_with_client(
            &root,
            "alice",
            "app-secret",
            "room-token",
            &reqwest::Client::new(),
        )
        .await
        .unwrap_err();
        server.join().unwrap();

        assert!(error.to_string().contains("response exceeds"));
    }

    #[tokio::test]
    async fn delete_room_accepts_response_at_exact_size_limit() {
        let base = r#"{"ocs":{"meta":{"status":"ok","statuscode":200,"message":"OK"}}}"#;
        let body = format!("{}{}", base, " ".repeat(DELETE_RESPONSE_LIMIT - base.len()));
        assert_eq!(body.len(), DELETE_RESPONSE_LIMIT);
        let (root, server) = mock_server_without_content_length("200 OK", &body);

        delete_room_with_client(
            &root,
            "alice",
            "app-secret",
            "room-token",
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        server.join().unwrap();
    }

    #[tokio::test]
    async fn delete_room_rejects_204_without_ocs_metadata() {
        let (root, server) = mock_server("204 No Content", "");

        let error = delete_room_with_client(
            &root,
            "alice",
            "app-secret",
            "room-token",
            &reqwest::Client::new(),
        )
        .await
        .unwrap_err();
        server.join().unwrap();

        assert!(error.to_string().contains("invalid OCS metadata"));
    }
}
