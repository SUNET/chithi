//! La Suite Visio integration.
//!
//! Authentication reuses Meet's Outlook add-on exchange without application
//! credentials: Chithi initializes a pending add-on session, seeds its one-time
//! transit token into a restricted Visio-origin auth webview, and polls for the
//! resulting short-lived `rooms:create` JWT. The JWT is stored in the system
//! token store and used only from Rust.
//!
//! The current external API is create-only. Room names are server-generated,
//! and deleting a local calendar event cannot delete the persistent Visio room.

use serde::Deserialize;

use crate::error::{Error, Result};

#[cfg(not(any(target_os = "android", target_os = "ios")))]
const INIT_PATH: &str = "api/v1.0/addons/sessions/init/";
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const POLL_PATH: &str = "api/v1.0/addons/sessions/poll/";
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const AUTHENTICATE_PATH: &str = "api/v1.0/authenticate/";
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const TRANSIT_PATH: &str = "addons/outlook/transit.html";
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const SUCCESS_PATH: &str = "addons/outlook/success.html";
const ROOMS_PATH: &str = "external-api/v1.0/rooms/";
const RESPONSE_DIAGNOSTIC_LIMIT: usize = 500;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const MAX_ACCESS_TOKEN_TTL_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone)]
pub struct VisioInstance {
    base: url::Url,
}

impl VisioInstance {
    /// Parse a Visio instance root. Add-on assets and APIs are fixed relative
    /// to the origin, so path-prefixed roots are rejected rather than silently
    /// producing endpoints that upstream does not serve.
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        crate::mail::url_validation::require_https(input)?;
        let parsed = url::Url::parse(input)
            .map_err(|error| Error::Other(format!("Invalid Visio instance URL: {error}")))?;
        if parsed.scheme() != "https" && !cfg!(test) {
            return Err(Error::Other(
                "Visio instance URL must use https://, including in debug builds".into(),
            ));
        }
        if parsed.host_str().is_none() {
            return Err(Error::Other(
                "Visio instance URL must include a host".into(),
            ));
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(Error::Other(
                "Visio instance URL must not contain user information".into(),
            ));
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(Error::Other(
                "Visio instance URL must not contain a query or fragment".into(),
            ));
        }
        if parsed.path() != "/" && !parsed.path().is_empty() {
            return Err(Error::Other(
                "Visio instance URL must be the site root without a path".into(),
            ));
        }

        let origin = parsed.origin().ascii_serialization();
        let base = url::Url::parse(&format!("{origin}/"))
            .map_err(|error| Error::Other(format!("Invalid Visio origin: {error}")))?;
        Ok(Self { base })
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn base_url(&self) -> String {
        self.base.as_str().trim_end_matches('/').to_string()
    }

    pub fn origin(&self) -> String {
        self.base.origin().ascii_serialization()
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn host_label(&self) -> String {
        self.base
            .host_str()
            .map(str::to_string)
            .unwrap_or_else(|| "Visio".into())
    }

    fn endpoint(&self, path: &str) -> url::Url {
        self.base
            .join(path)
            .expect("fixed Visio endpoint paths are valid")
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn transit_url(&self) -> url::Url {
        self.endpoint(TRANSIT_PATH)
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn success_url(&self) -> url::Url {
        self.endpoint(SUCCESS_PATH)
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn authenticate_url(&self) -> url::Url {
        let mut url = self.endpoint(AUTHENTICATE_PATH);
        url.query_pairs_mut()
            .append_pair("returnTo", self.success_url().as_str());
        url
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn is_success_url(&self, candidate: &url::Url) -> bool {
        candidate.origin() == self.base.origin() && candidate.path() == self.success_url().path()
    }

    /// OIDC redirects may cross arbitrary HTTPS origins, but no credentials are
    /// injected there. Cleartext is accepted only for the exact debug-loopback
    /// Visio origin already approved by `require_https`.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn allows_auth_navigation(&self, candidate: &url::Url) -> bool {
        if !candidate.username().is_empty() || candidate.password().is_some() {
            return false;
        }
        candidate.scheme() == "https"
            || (cfg!(debug_assertions)
                && candidate.scheme() == "http"
                && candidate.origin() == self.base.origin())
    }

    /// Document-start bridge replacing the Outlook-only Office Dialog API.
    /// Values are JSON-encoded so neither a token nor a configured URL can
    /// escape into executable script syntax.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn transit_bootstrap_script(&self, transit_token: &str) -> Result<String> {
        let origin = serde_json::to_string(&self.origin())
            .map_err(|error| Error::Other(format!("Visio origin encoding: {error}")))?;
        let transit_path = serde_json::to_string(self.transit_url().path())
            .map_err(|error| Error::Other(format!("Visio path encoding: {error}")))?;
        let token = serde_json::to_string(transit_token)
            .map_err(|error| Error::Other(format!("Visio token encoding: {error}")))?;
        let authenticate_url = serde_json::to_string(self.authenticate_url().as_str())
            .map_err(|error| Error::Other(format!("Visio auth URL encoding: {error}")))?;
        Ok(format!(
            r#"(() => {{
  if (window.top !== window || window.location.origin !== {origin}) return;
  if (window.location.pathname !== {transit_path}) return;
  const marker = "__chithiVisioTransitInstalled";
  if (window.sessionStorage.getItem(marker) === "1") return;
  window.sessionStorage.setItem("transitToken", {token});
  window.sessionStorage.setItem(marker, "1");
  window.location.replace({authenticate_url});
}})();"#,
        ))
    }
}

#[derive(Debug, Deserialize)]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub struct AddonSessionStart {
    pub transit_token: String,
    pub csrf_token: String,
}

#[derive(Debug, Deserialize)]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
struct AddonSessionPoll {
    state: String,
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    token_type: String,
    #[serde(default)]
    expires_in: i64,
    #[serde(default)]
    scope: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub struct VisioAccessToken {
    pub access_token: String,
    pub expires_in: i64,
    pub user_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub enum PollResult {
    Pending,
    Authenticated(VisioAccessToken),
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn build_login_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("Chithi/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| Error::Other(format!("Visio login HTTP client: {error}")))
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub async fn init_addon_session_with_client(
    instance: &VisioInstance,
    client: &reqwest::Client,
) -> Result<AddonSessionStart> {
    let response = client
        .post(instance.endpoint(INIT_PATH))
        .send()
        .await
        .map_err(|error| Error::Other(format!("Visio add-on session init request: {error}")))?;
    if !response.status().is_success() {
        return Err(response_error("Visio add-on session init", response).await);
    }
    let start: AddonSessionStart = response
        .json()
        .await
        .map_err(|error| Error::Other(format!("Visio add-on session init response: {error}")))?;
    if start.transit_token.trim().is_empty() || start.csrf_token.trim().is_empty() {
        return Err(Error::Other(
            "Visio add-on session init returned incomplete credentials".into(),
        ));
    }
    Ok(start)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub async fn poll_addon_session_with_client(
    instance: &VisioInstance,
    csrf_token: &str,
    client: &reqwest::Client,
) -> Result<PollResult> {
    let response = client
        .post(instance.endpoint(POLL_PATH))
        .header("X-CSRFToken", csrf_token)
        .send()
        .await
        .map_err(|error| Error::Other(format!("Visio add-on session poll request: {error}")))?;
    if response.status() != reqwest::StatusCode::ACCEPTED && !response.status().is_success() {
        return Err(response_error("Visio add-on session poll", response).await);
    }
    let status = response.status();
    let payload: AddonSessionPoll = response
        .json()
        .await
        .map_err(|error| Error::Other(format!("Visio add-on session poll response: {error}")))?;
    if status == reqwest::StatusCode::ACCEPTED && payload.state == "pending" {
        return Ok(PollResult::Pending);
    }
    if payload.state != "authenticated"
        || payload.access_token.trim().is_empty()
        || payload.expires_in <= 0
        || payload.expires_in > MAX_ACCESS_TOKEN_TTL_SECS
    {
        return Err(Error::Other(
            "Visio add-on session returned an incomplete access token".into(),
        ));
    }
    if !payload
        .scope
        .split_ascii_whitespace()
        .any(|scope| scope == "rooms:create")
    {
        return Err(Error::Other(
            "Visio access token does not grant rooms:create".into(),
        ));
    }
    if !payload.token_type.eq_ignore_ascii_case("bearer") {
        return Err(Error::Other(format!(
            "Visio returned unsupported token type '{}'",
            payload.token_type
        )));
    }
    Ok(PollResult::Authenticated(VisioAccessToken {
        user_id: jwt_user_id(&payload.access_token)?,
        access_token: payload.access_token,
        expires_in: payload.expires_in,
    }))
}

/// Read the stable user UUID from the add-on JWT. Signature verification is
/// intentionally left to the Visio API; identity binding relies on this token
/// arriving from the configured origin over TLS and prevents accidental
/// reauthentication as a different user on that same instance.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn jwt_user_id(token: &str) -> Result<String> {
    use base64::Engine;

    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| Error::Other("Visio returned a malformed access token".into()))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| Error::Other("Visio returned a malformed access token".into()))?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| Error::Other("Visio returned malformed token claims".into()))?;
    let user_id = claims["user_id"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Other("Visio access token has no user identity".into()))?;
    Ok(user_id.to_string())
}

#[derive(Debug, Deserialize)]
struct CreateRoomResponse {
    id: serde_json::Value,
    url: String,
}

pub async fn create_room_with_client(
    instance: &VisioInstance,
    access_token: &str,
    client: &reqwest::Client,
) -> Result<crate::meet::MeetCreateResult> {
    let response = client
        .post(instance.endpoint(ROOMS_PATH))
        .bearer_auth(access_token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .send()
        .await
        .map_err(|error| Error::Other(format!("Visio room creation request: {error}")))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(Error::Other(
            "Visio room creation: the instance returned 404 Not Found for its external rooms API. Ask the Visio administrator to enable EXTERNAL_API_ENABLED and configure APPLICATION_BASE_URL"
                .into(),
        ));
    }
    if !response.status().is_success() {
        return Err(response_error("Visio room creation", response).await);
    }
    let payload: CreateRoomResponse = response
        .json()
        .await
        .map_err(|error| Error::Other(format!("Visio room creation response: {error}")))?;
    let meeting_id = match payload.id {
        serde_json::Value::String(value) if !value.trim().is_empty() => value,
        serde_json::Value::Number(value) => value.to_string(),
        _ => {
            return Err(Error::Other(
                "Visio room creation response is missing a room id".into(),
            ))
        }
    };
    crate::mail::url_validation::require_https(&payload.url)?;
    let join_url = url::Url::parse(&payload.url)
        .map_err(|error| Error::Other(format!("Invalid Visio room URL: {error}")))?;
    if join_url.origin() != instance.base.origin() {
        return Err(Error::Other(
            "Visio room URL must use the configured instance origin".into(),
        ));
    }
    Ok(crate::meet::MeetCreateResult {
        meeting_id,
        join_url: join_url.to_string(),
    })
}

async fn response_error(operation: &str, response: reqwest::Response) -> Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Error::Other(format!(
        "{operation}: {status} ({})",
        body.chars()
            .take(RESPONSE_DIAGNOSTIC_LIMIT)
            .collect::<String>()
    ))
}

pub struct VisioProvider;

#[async_trait::async_trait]
impl crate::meet::MeetProvider for VisioProvider {
    fn protocol(&self) -> &'static str {
        "visio"
    }

    async fn create_url(
        &self,
        ctx: &crate::meet::MeetProviderCtx<'_>,
        account: &crate::db::accounts::AccountFull,
        _name: &str,
        _start_time: Option<&str>,
        _duration_minutes: Option<u32>,
    ) -> Result<crate::meet::MeetCreateResult> {
        let instance = VisioInstance::parse(&account.meet_url)?;
        let tokens = ctx
            .services
            .token_store()
            .load(&account.id)?
            .ok_or_else(|| Error::Other("Visio: sign in again in Settings".into()))?;
        if tokens.is_expired() {
            return Err(Error::Other(
                "Visio session expired; sign in again in Settings".into(),
            ));
        }
        create_room_with_client(
            &instance,
            &tokens.access_token,
            &ctx.services.transports.visio_http,
        )
        .await
    }

    async fn delete_meeting(
        &self,
        _ctx: &crate::meet::MeetProviderCtx<'_>,
        account: &crate::db::accounts::AccountFull,
        meeting_id: &str,
    ) -> Result<crate::meet::MeetDeleteOutcome> {
        log::info!(
            "Visio room {} for account {} remains remote because the external API has no delete operation",
            meeting_id,
            account.id,
        );
        Ok(crate::meet::MeetDeleteOutcome::RetainedByDesign)
    }

    async fn update_topic(
        &self,
        _ctx: &crate::meet::MeetProviderCtx<'_>,
        _account: &crate::db::accounts::AccountFull,
        _meeting_id: &str,
        _topic: &str,
    ) -> Result<()> {
        // Visio's external Room serializer currently exposes generated names
        // as read-only, so there is no remote topic to synchronize.
        Ok(())
    }
}

#[cfg(all(test, not(any(target_os = "android", target_os = "ios"))))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    type MockResponse = (
        &'static str,
        &'static str,
        Vec<(&'static str, &'static str)>,
    );

    fn mock_server(responses: Vec<MockResponse>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body, extra_headers) in responses {
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
                let mut headers = String::new();
                for (name, value) in extra_headers {
                    headers.push_str(&format!("{name}: {value}\r\n"));
                }
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                )
                .unwrap();
                requests.push(String::from_utf8(request).unwrap());
            }
            requests
        });
        (format!("http://{address}"), server)
    }

    #[test]
    fn instance_normalizes_root_and_builds_expected_urls() {
        let instance = VisioInstance::parse("https://visio.example.org/").unwrap();
        assert_eq!(instance.base_url(), "https://visio.example.org");
        assert_eq!(
            instance.transit_url().as_str(),
            "https://visio.example.org/addons/outlook/transit.html"
        );
        assert_eq!(
            instance.success_url().as_str(),
            "https://visio.example.org/addons/outlook/success.html"
        );
        assert_eq!(
            instance
                .authenticate_url()
                .query_pairs()
                .find(|(name, _)| name == "returnTo")
                .unwrap()
                .1,
            "https://visio.example.org/addons/outlook/success.html"
        );
    }

    #[test]
    fn instance_rejects_paths_userinfo_and_public_http() {
        assert!(VisioInstance::parse("https://visio.example.org/prefix").is_err());
        assert!(VisioInstance::parse("https://user@visio.example.org").is_err());
        assert!(VisioInstance::parse("http://visio.example.org").is_err());
    }

    #[test]
    fn bootstrap_keeps_token_out_of_authentication_url() {
        let instance = VisioInstance::parse("https://visio.example.org").unwrap();
        let script = instance
            .transit_bootstrap_script("secret-token-with-'quote")
            .unwrap();
        assert!(script.contains("sessionStorage.setItem(\"transitToken\""));
        assert!(script.contains("secret-token-with-'quote"));
        assert!(!instance
            .authenticate_url()
            .as_str()
            .contains("secret-token"));
    }

    #[tokio::test]
    async fn init_and_poll_reuse_cookie_and_custom_csrf() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJ1c2VyX2lkIjoidmlzaW8tdXNlciJ9.signature";
        let (root, server) = mock_server(vec![
            (
                "201 Created",
                r#"{"transit_token":"transit","csrf_token":"csrf"}"#,
                vec![(
                    "Set-Cookie",
                    "addonsSid=session-cookie; Path=/; HttpOnly; SameSite=None",
                )],
            ),
            (
                "200 OK",
                Box::leak(format!(r#"{{"state":"authenticated","access_token":"{jwt}","token_type":"Bearer","expires_in":7200,"scope":"rooms:create"}}"#).into_boxed_str()),
                vec![],
            ),
        ]);
        let instance = VisioInstance::parse(&root).unwrap();
        let client = build_login_client().unwrap();

        let start = init_addon_session_with_client(&instance, &client)
            .await
            .unwrap();
        let result = poll_addon_session_with_client(&instance, &start.csrf_token, &client)
            .await
            .unwrap();
        let requests = server.join().unwrap();

        assert!(requests[0].starts_with("POST /api/v1.0/addons/sessions/init/ HTTP/1.1"));
        assert!(requests[1].starts_with("POST /api/v1.0/addons/sessions/poll/ HTTP/1.1"));
        let poll_headers = requests[1].to_ascii_lowercase();
        assert!(poll_headers.contains("cookie: addonssid=session-cookie"));
        assert!(poll_headers.contains("x-csrftoken: csrf"));
        assert_eq!(
            result,
            PollResult::Authenticated(VisioAccessToken {
                access_token: jwt.into(),
                expires_in: 7200,
                user_id: "visio-user".into(),
            })
        );
    }

    #[test]
    fn extracts_stable_user_id_from_addon_jwt() {
        let token = "header.eyJ1c2VyX2lkIjoiMTIzNCJ9.signature";
        assert_eq!(jwt_user_id(token).unwrap(), "1234");
        assert!(jwt_user_id("opaque-token").is_err());
    }

    #[tokio::test]
    async fn creates_room_with_bearer_token() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let root = format!("http://{address}");
        let response_body = format!(r#"{{"id":"room-id","url":"{root}/rooms/room-id"}}"#);
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0_u8; 4096];
            let read = stream.read(&mut bytes).unwrap();
            write!(
                stream,
                "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body,
            )
            .unwrap();
            String::from_utf8(bytes[..read].to_vec()).unwrap()
        });
        let instance = VisioInstance::parse(&root).unwrap();
        let result = create_room_with_client(&instance, "visio-jwt", &reqwest::Client::new())
            .await
            .unwrap();
        let request = thread.join().unwrap();
        assert!(request.starts_with("POST /external-api/v1.0/rooms/ HTTP/1.1"));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer visio-jwt"));
        assert_eq!(result.meeting_id, "room-id");
        assert_eq!(result.join_url, format!("{root}/rooms/room-id"));
    }

    #[tokio::test]
    async fn room_creation_404_explains_external_api_configuration() {
        let response_body = "<!doctype html><title>Not Found</title>";
        let (root, server) = mock_server(vec![("404 Not Found", response_body, vec![])]);
        let instance = VisioInstance::parse(&root).unwrap();

        let error = create_room_with_client(&instance, "visio-jwt", &reqwest::Client::new())
            .await
            .unwrap_err()
            .to_string();
        let requests = server.join().unwrap();

        assert!(requests[0].starts_with("POST /external-api/v1.0/rooms/ HTTP/1.1"));
        assert!(error.contains("external rooms API"));
        assert!(error.contains("EXTERNAL_API_ENABLED"));
        assert!(error.contains("APPLICATION_BASE_URL"));
        assert!(!error.contains(response_body));
    }
}
