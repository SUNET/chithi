//! Microsoft Graph API client for O365 mail, calendar, and contacts.
//!
//! All operations go through `https://graph.microsoft.com/v1.0` with
//! Bearer token authentication. O365 mail delivery remains on SMTP+XOAUTH2.

use crate::error::{Error, Result};
use crate::mail::search::build_graph_kql;
use crate::message::{normalize_message_id, SearchHit, SearchQuery};
use serde::{Deserialize, Serialize};

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
const GRAPH_BETA_BASE: &str = "https://graph.microsoft.com/beta";

/// Graph JSON batching allows at most 20 sub-requests per `$batch` call.
const BATCH_SIZE: usize = 20;

/// Whether a Graph item-level failure means the requested object is absent.
pub(crate) fn is_item_not_found(error: &Error) -> bool {
    let message = error.to_string();
    message.contains("404") && message.contains("ErrorItemNotFound")
}

/// Fold a `$batch` response into per-item results. Sub-responses are keyed
/// by the request "id" we set to the item's global index; they may arrive
/// out of order.
///
/// A batch can return outer HTTP 200 while individual sub-responses are
/// throttled (429) or transient (503/504). Retryable items are NOT written
/// into `results`; they come back as `(index, retry_after_secs)` so the caller
/// can retry just those sub-requests. Non-idempotent callers can disable
/// retries for ambiguous 503/504 responses. Everything else is final.
fn apply_batch_responses(
    resp: &serde_json::Value,
    results: &mut [Result<()>],
    retry_transient_errors: bool,
) -> Vec<(usize, u64)> {
    let mut retryable = Vec::new();
    let Some(responses) = resp["responses"].as_array() else {
        return retryable;
    };
    for r in responses {
        let Some(idx) = r["id"].as_str().and_then(|s| s.parse::<usize>().ok()) else {
            continue;
        };
        if idx >= results.len() {
            continue;
        }
        let status = r["status"].as_u64().unwrap_or(0) as u16;
        if (200..300).contains(&status) {
            results[idx] = Ok(());
        } else if status == 429 || (retry_transient_errors && matches!(status, 503 | 504)) {
            let delay = batch_item_retry_after(r).unwrap_or(5);
            retryable.push((idx, delay));
        } else {
            let body = r["body"].to_string();
            results[idx] = Err(Error::Other(format!(
                "Graph $batch item returned {}: {}",
                status,
                truncate(&body, 300)
            )));
        }
    }
    retryable
}

/// Pull `Retry-After` (seconds) out of a `$batch` sub-response's headers,
/// tolerating header-name casing differences.
fn batch_item_retry_after(r: &serde_json::Value) -> Option<u64> {
    let headers = r["headers"].as_object()?;
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, v)| v.as_str())
        .and_then(|s| s.parse::<u64>().ok())
}

fn build_copy_batch_requests(
    message_ids: &[String],
    dest_folder_id: &str,
) -> Vec<serde_json::Value> {
    message_ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            serde_json::json!({
                "id": format!("{}", i),
                "method": "POST",
                "url": format!("/me/messages/{}/copy", id),
                "headers": { "Content-Type": "application/json" },
                "body": { "destinationId": dest_folder_id }
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Graph client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphEndpoints {
    pub v1_api_root: String,
    pub beta_api_root: String,
}

impl GraphEndpoints {
    pub fn new(v1_api_root: impl Into<String>, beta_api_root: impl Into<String>) -> Self {
        Self {
            v1_api_root: v1_api_root.into(),
            beta_api_root: beta_api_root.into(),
        }
    }

    fn v1_url(&self, path: &str) -> String {
        join_url(&self.v1_api_root, path)
    }

    fn beta_url(&self, path: &str) -> String {
        join_url(&self.beta_api_root, path)
    }
}

impl Default for GraphEndpoints {
    fn default() -> Self {
        Self::new(GRAPH_BASE, GRAPH_BETA_BASE)
    }
}

fn join_url(root: &str, path: &str) -> String {
    format!(
        "{}/{}",
        root.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

#[cfg(test)]
mod endpoint_tests {
    use super::{parse_graph_contact, GraphClient, GraphEndpoints};
    use reqwest::header::{HeaderMap, HeaderValue};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve_once(response_body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}/injected", listener.local_addr().unwrap());
        let request = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 1024];
                let count = socket.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..count]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(bytes).unwrap()
        });
        (root, request)
    }

    struct TestResponse {
        status: u16,
        retry_after: Option<&'static str>,
        body: String,
    }

    impl TestResponse {
        fn ok(body: impl Into<String>) -> Self {
            Self {
                status: 200,
                retry_after: None,
                body: body.into(),
            }
        }
    }

    async fn serve_responses(
        build_responses: impl FnOnce(&str) -> Vec<TestResponse>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}/injected", listener.local_addr().unwrap());
        let responses = build_responses(&root);
        let requests = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(responses.len());
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 1024];
                    let count = socket.read(&mut chunk).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let reason = match response.status {
                    200 => "OK",
                    400 => "Bad Request",
                    429 => "Too Many Requests",
                    503 => "Service Unavailable",
                    504 => "Gateway Timeout",
                    _ => "Test Response",
                };
                let retry_after = response
                    .retry_after
                    .map(|value| format!("Retry-After: {value}\r\n"))
                    .unwrap_or_default();
                let wire_response = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    reason,
                    retry_after,
                    response.body.len(),
                    response.body
                );
                socket.write_all(wire_response.as_bytes()).await.unwrap();
                requests.push(String::from_utf8(bytes).unwrap());
            }
            requests
        });
        (root, requests)
    }

    async fn serve_many(
        build_bodies: impl FnOnce(&str) -> Vec<String>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        serve_responses(|root| {
            build_bodies(root)
                .into_iter()
                .map(TestResponse::ok)
                .collect()
        })
        .await
    }

    fn test_client(root: &str) -> GraphClient {
        GraphClient::with_client(
            reqwest::Client::new(),
            "test-access-token",
            GraphEndpoints::new(root, "http://127.0.0.1:1/unused-beta"),
        )
    }

    fn complete_contact(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "displayName": format!("Contact {id}"),
            "emailAddresses": [{
                "address": format!("{id}@example.org"),
                "name": format!("Email {id}"),
            }],
            "mobilePhone": "+46000000001",
            "businessPhones": ["+46000000002"],
            "homePhones": ["+46000000003"],
            "companyName": "Example Org",
            "jobTitle": "Engineer",
        })
    }

    #[test]
    fn defaults_to_production_graph_roots() {
        let endpoints = GraphEndpoints::default();

        assert_eq!(endpoints.v1_api_root, "https://graph.microsoft.com/v1.0");
        assert_eq!(endpoints.beta_api_root, "https://graph.microsoft.com/beta");
    }

    #[test]
    fn joins_roots_and_paths_with_one_separator() {
        let endpoints =
            GraphEndpoints::new("http://localhost:8080/v1.0/", "http://localhost:8080/beta/");

        assert_eq!(
            endpoints.v1_url("/me/messages"),
            "http://localhost:8080/v1.0/me/messages"
        );
        assert_eq!(
            endpoints.beta_url("me/findRooms"),
            "http://localhost:8080/beta/me/findRooms"
        );
    }

    #[tokio::test]
    async fn calendar_request_uses_injected_root_client_and_graph_wire_format() {
        let (root, captured) = serve_once(r#"{"value":[]}"#).await;
        let mut headers = HeaderMap::new();
        headers.insert("x-injected-client", HeaderValue::from_static("graph-test"));
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap();
        let client = GraphClient::with_client(
            http,
            "test-access-token",
            GraphEndpoints::new(&root, "http://127.0.0.1:1/unused-beta"),
        );

        let events = client
            .list_events_for_calendar(
                "team@example.org",
                "2026-08-09T00:00:00Z",
                "2026-08-10T00:00:00Z",
            )
            .await
            .unwrap();
        assert!(events.is_empty());

        let request = captured.await.unwrap();
        let mut lines = request.lines();
        let request_line = lines.next().unwrap();
        let target = request_line.split_whitespace().nth(1).unwrap();
        let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();

        assert_eq!(request_line.split_whitespace().next(), Some("GET"));
        assert_eq!(
            url.path(),
            "/injected/me/calendars/team%40example.org/calendarView"
        );
        assert_eq!(query.get("startDateTime").unwrap(), "2026-08-09T00:00:00Z");
        assert_eq!(query.get("endDateTime").unwrap(), "2026-08-10T00:00:00Z");
        assert_eq!(query.get("$top").unwrap(), "100");
        assert_eq!(query.get("$orderby").unwrap(), "start/dateTime");
        assert!(query.get("$select").unwrap().contains("responseStatus"));

        let headers = request.to_ascii_lowercase();
        assert!(headers.contains("authorization: bearer test-access-token\r\n"));
        assert!(headers.contains("x-injected-client: graph-test\r\n"));
        assert!(headers.contains("prefer: outlook.timezone=\"utc\"\r\n"));
    }

    #[tokio::test]
    async fn contacts_reject_missing_or_non_array_value() {
        for body in [r#"{}"#, r#"{"value":{}}"#] {
            let (root, captured) = serve_once(body).await;
            let error = test_client(&root).list_contacts().await.unwrap_err();

            assert!(error
                .to_string()
                .contains("contacts response `value` must be an array"));
            captured.await.unwrap();
        }

        let (root, captured) = serve_many(|root| {
            vec![
                format!(r#"{{"value":[],"@odata.nextLink":"{root}/page-2"}}"#),
                r#"{"notValue":[]}"#.into(),
            ]
        })
        .await;
        let error = test_client(&root).list_contacts().await.unwrap_err();

        assert!(error
            .to_string()
            .contains("contacts response `value` must be an array"));
        assert_eq!(captured.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn contacts_reject_non_string_next_link() {
        let (root, captured) = serve_once(r#"{"value":[],"@odata.nextLink":42}"#).await;

        let error = test_client(&root).list_contacts().await.unwrap_err();

        assert!(error
            .to_string()
            .contains("contacts response `@odata.nextLink` must be a string"));
        captured.await.unwrap();
    }

    #[tokio::test]
    async fn contacts_accept_null_next_link() {
        let (root, captured) = serve_once(r#"{"value":[],"@odata.nextLink":null}"#).await;

        let contacts = test_client(&root).list_contacts().await.unwrap();

        assert!(contacts.is_empty());
        captured.await.unwrap();
    }

    #[tokio::test]
    async fn create_contact_rejects_missing_or_blank_id() {
        for body in [r#"{}"#, r#"{"id":null}"#, r#"{"id":"   "}"#] {
            let (root, captured) = serve_once(body).await;

            let error = test_client(&root)
                .create_contact(&serde_json::json!({"displayName": "Ada"}))
                .await
                .unwrap_err();

            assert!(error
                .to_string()
                .contains("create-contact response `id` must be non-empty"));
            let request = captured.await.unwrap();
            assert!(request.starts_with("POST /injected/me/contacts "));
        }
    }

    #[test]
    fn contacts_reject_each_omitted_owned_field() {
        for field in [
            "id",
            "displayName",
            "emailAddresses",
            "mobilePhone",
            "businessPhones",
            "homePhones",
            "companyName",
            "jobTitle",
        ] {
            let mut contact = complete_contact("one");
            contact.as_object_mut().unwrap().remove(field);

            let error = parse_graph_contact(&contact).unwrap_err();

            assert!(
                error.to_string().contains(&format!("`{field}` is missing")),
                "unexpected error for omitted {field}: {error}"
            );
        }
    }

    #[test]
    fn contacts_reject_malformed_item_shapes() {
        let mut malformed = vec![serde_json::json!(42)];
        for (field, value) in [
            ("id", serde_json::json!(42)),
            ("displayName", serde_json::json!(42)),
            ("mobilePhone", serde_json::json!({})),
            ("companyName", serde_json::json!(false)),
            ("jobTitle", serde_json::json!([])),
            ("emailAddresses", serde_json::json!({})),
            ("emailAddresses", serde_json::json!([42])),
            ("emailAddresses", serde_json::json!([{}])),
            ("emailAddresses", serde_json::json!([{"address": 42}])),
            (
                "emailAddresses",
                serde_json::json!([{"address": "one@example.org", "name": 42}]),
            ),
            ("businessPhones", serde_json::json!({})),
            ("businessPhones", serde_json::json!([42])),
            ("homePhones", serde_json::json!([{}])),
        ] {
            let mut contact = complete_contact("one");
            contact[field] = value;
            malformed.push(contact);
        }

        for contact in malformed {
            let error = parse_graph_contact(&contact).unwrap_err();
            assert!(
                error.to_string().contains("Graph contact"),
                "unexpected error for {contact}: {error}"
            );
        }
    }

    #[test]
    fn contacts_reject_blank_ids() {
        for id in ["", "   "] {
            let mut contact = complete_contact("one");
            contact["id"] = serde_json::json!(id);
            let error = parse_graph_contact(&contact).unwrap_err();

            assert!(error
                .to_string()
                .contains("`id` must be a non-empty string"));
        }
    }

    #[test]
    fn contacts_accept_null_owned_values() {
        let contact = serde_json::json!({
            "id": "one",
            "displayName": null,
            "emailAddresses": null,
            "mobilePhone": null,
            "businessPhones": null,
            "homePhones": null,
            "companyName": null,
            "jobTitle": null,
        });

        let contact = parse_graph_contact(&contact).unwrap();

        assert_eq!(contact.display_name, "");
        assert_eq!(contact.emails_json, "[]");
        assert_eq!(contact.phones_json, "[]");
        assert!(contact.organization.is_none());
        assert!(contact.title.is_none());
    }

    #[tokio::test]
    async fn contacts_fail_the_fetch_when_any_item_is_incomplete() {
        let (root, captured) = serve_many(|_| {
            let mut incomplete = complete_contact("two");
            incomplete.as_object_mut().unwrap().remove("jobTitle");
            vec![serde_json::json!({
                "value": [complete_contact("one"), incomplete],
            })
            .to_string()]
        })
        .await;

        let error = test_client(&root).list_contacts().await.unwrap_err();

        assert!(error.to_string().contains("`jobTitle` is missing"));
        assert_eq!(captured.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn contacts_follow_all_pages_with_authentication() {
        let (root, captured) = serve_many(|root| {
            vec![
                serde_json::json!({
                    "value": [complete_contact("one")],
                    "@odata.nextLink": format!("{root}/page-2"),
                })
                .to_string(),
                serde_json::json!({"value": [complete_contact("two")] }).to_string(),
            ]
        })
        .await;

        let contacts = test_client(&root).list_contacts().await.unwrap();

        assert_eq!(
            contacts
                .iter()
                .map(|contact| contact.id.as_str())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with("GET /injected/me/contacts?"));
        assert!(requests[1].starts_with("GET /injected/page-2 "));
        assert!(requests.iter().all(|request| request
            .to_ascii_lowercase()
            .contains("authorization: bearer test-access-token\r\n")));
    }

    #[tokio::test]
    async fn contacts_reject_cross_origin_next_link_without_requesting_it() {
        let (foreign_root, mut foreign_request) = serve_once(r#"{"value":[]}"#).await;
        let (root, captured) = serve_many(move |_| {
            vec![serde_json::json!({
                "value": [],
                "@odata.nextLink": format!("{foreign_root}/contacts"),
            })
            .to_string()]
        })
        .await;

        let error = test_client(&root).list_contacts().await.unwrap_err();

        assert!(error.to_string().contains("untrusted origin"));
        assert_eq!(captured.await.unwrap().len(), 1);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut foreign_request)
                .await
                .is_err(),
            "cross-origin server received a request"
        );
        foreign_request.abort();
    }

    #[tokio::test]
    async fn contacts_reject_credentialed_next_link() {
        let (root, captured) = serve_many(|root| {
            let credentialed = root.replacen("http://", "http://user:pass@", 1);
            vec![serde_json::json!({
                "value": [],
                "@odata.nextLink": format!("{credentialed}/contacts"),
            })
            .to_string()]
        })
        .await;

        let error = test_client(&root).list_contacts().await.unwrap_err();

        assert!(error.to_string().contains("must not contain credentials"));
        assert_eq!(captured.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn contacts_retry_later_page_throttles_and_transient_errors() {
        for status in [429, 503, 504] {
            let (root, captured) = serve_responses(|root| {
                vec![
                    TestResponse::ok(
                        serde_json::json!({
                            "value": [complete_contact("one")],
                            "@odata.nextLink": format!("{root}/page-2"),
                        })
                        .to_string(),
                    ),
                    TestResponse {
                        status,
                        retry_after: Some("0"),
                        body: r#"{"error":"retry later"}"#.into(),
                    },
                    TestResponse::ok(
                        serde_json::json!({"value": [complete_contact("two")] }).to_string(),
                    ),
                ]
            })
            .await;

            let contacts = test_client(&root).list_contacts().await.unwrap();

            assert_eq!(contacts.len(), 2, "status {status}");
            let requests = captured.await.unwrap();
            assert_eq!(requests.len(), 3, "status {status}");
            assert!(requests[1].starts_with("GET /injected/page-2 "));
            assert!(requests[2].starts_with("GET /injected/page-2 "));
            assert!(requests.iter().all(|request| request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-access-token\r\n")));
        }
    }

    #[tokio::test]
    async fn contacts_keep_later_page_status_and_body_errors() {
        let (root, captured) = serve_responses(|root| {
            vec![
                TestResponse::ok(
                    serde_json::json!({
                        "value": [],
                        "@odata.nextLink": format!("{root}/page-2"),
                    })
                    .to_string(),
                ),
                TestResponse {
                    status: 400,
                    retry_after: None,
                    body: r#"{"error":"bad continuation"}"#.into(),
                },
            ]
        })
        .await;

        let error = test_client(&root).list_contacts().await.unwrap_err();
        let message = error.to_string();

        assert!(message.contains("400 Bad Request"));
        assert!(message.contains("bad continuation"));
        assert_eq!(captured.await.unwrap().len(), 2);
    }
}

pub struct GraphClient {
    http: reqwest::Client,
    access_token: String,
    endpoints: GraphEndpoints,
}

/// Removes an incomplete Graph download even if its async owner is cancelled.
struct PartialFileGuard {
    path: std::path::PathBuf,
    committed: bool,
}

impl PartialFileGuard {
    fn new(path: std::path::PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for PartialFileGuard {
    fn drop(&mut self) {
        if !self.committed {
            if let Err(error) = std::fs::remove_file(&self.path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    log::warn!(
                        "Failed to remove partial Graph download {}: {}",
                        self.path.display(),
                        error
                    );
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphRoom {
    pub name: String,
    pub address: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphRoomAvailability {
    pub state: String,
    pub busy_start: Option<String>,
    pub busy_end: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GraphSchedule {
    pub email: String,
    pub available: bool,
    pub busy: Vec<GraphBusyPeriod>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GraphBusyPeriod {
    pub start: String,
    pub end: String,
}

impl GraphClient {
    pub fn with_client(
        http: reqwest::Client,
        access_token: &str,
        endpoints: GraphEndpoints,
    ) -> Self {
        Self {
            http,
            access_token: access_token.to_string(),
            endpoints,
        }
    }

    /// Send a request, retrying on throttled (429) and — for idempotent
    /// requests only — transient (503/504) responses, honoring
    /// `Retry-After`. Exchange throttles mailbox access aggressively;
    /// without this every 429 aborted the whole account sync and the
    /// retry storm made the throttling worse.
    ///
    /// `retry_transient` must be `false` for non-idempotent requests
    /// (POSTs like resource creation and `$batch` with moves):
    /// a gateway 503/504 can arrive after Graph has already committed the
    /// request, and a blind retry would duplicate mail or resources. 429
    /// is always safe to retry — it means the request was rejected before
    /// processing.
    async fn send_with_retry(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
        what: &str,
        retry_transient: bool,
    ) -> Result<reqwest::Response> {
        const MAX_ATTEMPTS: u32 = 3;
        const MAX_RETRY_AFTER_SECS: u64 = 120;

        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let resp = build()
                .send()
                .await
                .map_err(|e| Error::Other(format!("Graph {} failed: {}", what, e)))?;
            let code = resp.status().as_u16();
            let retryable = code == 429 || (retry_transient && matches!(code, 503 | 504));
            if retryable && attempt < MAX_ATTEMPTS {
                let retry_after = resp
                    .headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(5)
                    .min(MAX_RETRY_AFTER_SECS);
                log::warn!(
                    "Graph {} returned {} (attempt {}/{}), retrying after {}s",
                    what,
                    code,
                    attempt,
                    MAX_ATTEMPTS,
                    retry_after
                );
                tokio::time::sleep(std::time::Duration::from_secs(retry_after)).await;
                continue;
            }
            return Ok(resp);
        }
    }

    async fn get(&self, path: &str, params: &[(&str, &str)]) -> Result<serde_json::Value> {
        let url = self.endpoints.v1_url(path);
        let resp = self
            .send_with_retry(
                || {
                    self.http
                        .get(&url)
                        .bearer_auth(&self.access_token)
                        .query(params)
                },
                &format!("GET {}", path),
                true,
            )
            .await?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::Other(format!(
                "Graph GET {} returned {}: {}",
                path,
                status,
                truncate(&body, 500)
            )));
        }

        serde_json::from_str(&body)
            .map_err(|e| Error::Other(format!("Graph JSON parse failed: {}", e)))
    }

    async fn get_beta(&self, path: &str, params: &[(&str, &str)]) -> Result<serde_json::Value> {
        let url = self.endpoints.beta_url(path);
        let resp = self
            .send_with_retry(
                || {
                    self.http
                        .get(&url)
                        .bearer_auth(&self.access_token)
                        .query(params)
                },
                &format!("beta GET {}", path),
                true,
            )
            .await?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::Other(format!(
                "Graph beta GET {} returned {}: {}",
                path,
                status,
                truncate(&body, 500)
            )));
        }

        serde_json::from_str(&body)
            .map_err(|e| Error::Other(format!("Graph beta JSON parse failed: {}", e)))
    }

    /// GET an absolute Graph URL (used to follow `@odata.nextLink`,
    /// which Graph returns as a fully-qualified URL rather than a path).
    async fn get_absolute(&self, url: &str) -> Result<serde_json::Value> {
        let url = reqwest::Url::parse(url)
            .map_err(|error| Error::Other(format!("Invalid Graph continuation URL: {error}")))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::Other(
                "Graph continuation URL must not contain credentials".into(),
            ));
        }

        let mut trusted_origin = false;
        for root in [&self.endpoints.v1_api_root, &self.endpoints.beta_api_root] {
            let root = reqwest::Url::parse(root).map_err(|error| {
                Error::Other(format!("Invalid configured Graph API root: {error}"))
            })?;
            if url.scheme() == root.scheme()
                && url.host_str() == root.host_str()
                && url.port_or_known_default() == root.port_or_known_default()
            {
                trusted_origin = true;
                break;
            }
        }
        if !trusted_origin {
            return Err(Error::Other(format!(
                "Refusing Graph continuation URL with untrusted origin: {url}"
            )));
        }

        let resp = self
            .send_with_retry(
                || self.http.get(url.clone()).bearer_auth(&self.access_token),
                "GET (absolute)",
                true,
            )
            .await?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::Other(format!(
                "Graph GET {} returned {}: {}",
                url,
                status,
                truncate(&body, 500)
            )));
        }

        serde_json::from_str(&body)
            .map_err(|e| Error::Other(format!("Graph JSON parse failed: {}", e)))
    }

    /// GET a Graph collection endpoint and return every item across all
    /// pages, following `@odata.nextLink` until exhausted. Graph caps a
    /// single page at `$top` items, so endpoints like the room/room-list
    /// places APIs silently drop everything past the first page on large
    /// tenants unless pagination is followed (PR #173 review).
    async fn get_all(&self, path: &str, params: &[(&str, &str)]) -> Result<Vec<serde_json::Value>> {
        let mut items = Vec::new();
        let mut page = self.get(path, params).await?;
        loop {
            if let Some(values) = page["value"].as_array() {
                items.extend(values.iter().cloned());
            }
            match page["@odata.nextLink"].as_str() {
                Some(next) => page = self.get_absolute(next).await?,
                None => break,
            }
        }
        Ok(items)
    }

    /// Stream a Graph API response directly to a file on disk.
    /// Returns the number of bytes written. Avoids buffering the entire
    /// response in memory — critical for large emails with attachments.
    async fn stream_to_file(&self, path: &str, dest: &std::path::Path) -> Result<u64> {
        use tokio::io::AsyncWriteExt;

        let url = self.endpoints.v1_url(path);
        let resp = self
            .send_with_retry(
                || self.http.get(&url).bearer_auth(&self.access_token),
                &format!("GET {}", path),
                true,
            )
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Graph GET {} returned {}: {}",
                path,
                status,
                truncate(&body, 500)
            )));
        }

        let temp_path =
            dest.with_file_name(format!(".chithi-{}.partial", uuid::Uuid::new_v4().simple()));
        let mut partial = PartialFileGuard::new(temp_path);
        // Create synchronously before the first await. Tokio's async create
        // runs on its blocking pool, so cancellation while it is pending can
        // otherwise drop the guard before the background open creates the
        // file, leaving an unowned partial behind.
        let standard_file = std::fs::File::create(partial.path()).map_err(|e| {
            Error::Other(format!(
                "Failed to create temporary file for {}: {}",
                dest.display(),
                e
            ))
        })?;
        let mut file = tokio::fs::File::from_std(standard_file);
        let result: Result<u64> = async {
            let mut stream = resp.bytes_stream();
            let mut total: u64 = 0;

            use futures::StreamExt;
            while let Some(chunk) = stream.next().await {
                let chunk =
                    chunk.map_err(|e| Error::Other(format!("Graph stream read failed: {}", e)))?;
                file.write_all(&chunk).await.map_err(|e| {
                    Error::Other(format!("Failed to write to {}: {}", dest.display(), e))
                })?;
                total += chunk.len() as u64;
            }

            file.flush()
                .await
                .map_err(|e| Error::Other(format!("Failed to flush {}: {}", dest.display(), e)))?;

            Ok(total)
        }
        .await;

        drop(file);
        let total = result?;
        tokio::fs::rename(partial.path(), dest).await.map_err(|e| {
            Error::Other(format!(
                "Failed to finalize Graph download {}: {}",
                dest.display(),
                e
            ))
        })?;
        partial.commit();
        Ok(total)
    }

    async fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let url = self.endpoints.v1_url(path);
        let resp = self
            .send_with_retry(
                || {
                    self.http
                        .post(&url)
                        .bearer_auth(&self.access_token)
                        .json(body)
                },
                &format!("POST {}", path),
                // POST is not idempotent (resource creation and $batch
                // moves): 429-only retry.
                false,
            )
            .await?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::Other(format!(
                "Graph POST {} returned {}: {}",
                path,
                status,
                truncate(&text, 500)
            )));
        }

        if text.is_empty() {
            Ok(serde_json::Value::Null)
        } else {
            serde_json::from_str(&text)
                .map_err(|e| Error::Other(format!("Graph POST parse failed: {}", e)))
        }
    }

    async fn patch_json(&self, path: &str, body: &serde_json::Value) -> Result<()> {
        let url = self.endpoints.v1_url(path);
        let resp = self
            .send_with_retry(
                || {
                    self.http
                        .patch(&url)
                        .bearer_auth(&self.access_token)
                        .json(body)
                },
                &format!("PATCH {}", path),
                true,
            )
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Graph PATCH {} returned {}: {}",
                path,
                status,
                truncate(&text, 500)
            )));
        }
        Ok(())
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let url = self.endpoints.v1_url(path);
        let resp = self
            .send_with_retry(
                || self.http.delete(&url).bearer_auth(&self.access_token),
                &format!("DELETE {}", path),
                true,
            )
            .await?;

        let status = resp.status();
        if !status.is_success() && status.as_u16() != 204 {
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Graph DELETE {} returned {}: {}",
                path,
                status,
                truncate(&text, 500)
            )));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // User profile
    // -----------------------------------------------------------------------

    /// Get the signed-in user's profile (email, display name).
    pub async fn get_me(&self) -> Result<GraphUser> {
        let resp = self
            .get(
                "/me",
                &[("$select", "id,displayName,userPrincipalName,mail")],
            )
            .await?;

        let display_name = resp["displayName"].as_str().unwrap_or("").to_string();
        let user_principal_name = resp["userPrincipalName"].as_str().unwrap_or("");
        let mut email = profile_email_from_me(resp["mail"].as_str(), Some(user_principal_name));
        let login_email = if looks_like_smtp_address(user_principal_name) {
            user_principal_name.trim().to_string()
        } else {
            email.clone()
        };
        log::info!(
            "Graph /me: displayName={}, login_email={}",
            display_name,
            login_email
        );

        // For personal Microsoft accounts, the login email (e.g., gmail.com) may differ
        // from the actual Outlook mailbox address. Try multiple sources:

        // 1. Check To address of inbox messages (catches user-configured aliases like chithiapp@outlook.com)
        if let Ok(inbox_resp) = self
            .get(
                "/me/mailFolders('Inbox')/messages",
                &[("$top", "1"), ("$select", "toRecipients")],
            )
            .await
        {
            if let Some(to_addr) = inbox_resp["value"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|m| m["toRecipients"].as_array())
                .and_then(|r| r.first())
                .and_then(|r| r["emailAddress"]["address"].as_str())
            {
                if to_addr != email
                    && looks_like_smtp_address(to_addr)
                    && (to_addr.contains("outlook.")
                        || to_addr.contains("hotmail.")
                        || to_addr.contains("live."))
                {
                    log::info!("Graph: mailbox email from Inbox To: {}", to_addr);
                    email = to_addr.to_string();
                }
            }
        }

        // 2. Fallback: check From address of sent messages
        if email == login_email {
            if let Ok(sent_resp) = self
                .get(
                    "/me/mailFolders('SentItems')/messages",
                    &[("$top", "1"), ("$select", "from")],
                )
                .await
            {
                if let Some(from_addr) = sent_resp["value"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|m| m["from"]["emailAddress"]["address"].as_str())
                {
                    // Exchange Online frequently reports the Sent `from`
                    // as a legacy X.500 / "EX" DN rather than an SMTP
                    // address — never let that overwrite the real
                    // address `/me` already returned.
                    if from_addr != email && looks_like_smtp_address(from_addr) {
                        log::info!("Graph: mailbox email from Sent: {}", from_addr);
                        email = from_addr.to_string();
                    } else if !looks_like_smtp_address(from_addr) {
                        log::debug!(
                            "Graph: ignoring non-SMTP Sent `from` address: {}",
                            from_addr
                        );
                    }
                }
            }
        }

        Ok(GraphUser {
            display_name,
            email,
            login_email,
        })
    }

    // -----------------------------------------------------------------------
    // Mail folders
    // -----------------------------------------------------------------------

    /// List all mail folders, walking the entire hierarchy.
    ///
    /// Graph's `/me/mailFolders` returns only top-level folders, so children
    /// are fetched per parent (breadth-first) with full pagination. The old
    /// implementation fetched exactly one level of children with no `$top`
    /// and no `nextLink` follow, which silently dropped grandchildren and
    /// any child past Graph's default page size (10) — folders created in a
    /// nested position on the web never appeared locally.
    pub async fn list_mail_folders(&self) -> Result<Vec<GraphMailFolder>> {
        const FOLDER_SELECT: &str =
            "id,displayName,totalItemCount,unreadItemCount,parentFolderId,childFolderCount";

        let mut folders = Vec::new();
        // Parents whose children still need fetching; None = top level.
        let mut pending_parents: Vec<Option<String>> = vec![None];

        while let Some(parent) = pending_parents.pop() {
            let path = match &parent {
                None => "/me/mailFolders".to_string(),
                Some(pid) => format!("/me/mailFolders/{}/childFolders", pid),
            };

            let mut page = self
                .get(
                    &path,
                    &[
                        ("$select", FOLDER_SELECT),
                        ("$top", "100"),
                        ("includeHiddenFolders", "true"),
                    ],
                )
                .await?;

            loop {
                if let Some(values) = page["value"].as_array() {
                    for f in values {
                        let id = f["id"].as_str().unwrap_or("").to_string();
                        if f["childFolderCount"].as_i64().unwrap_or(0) > 0 && !id.is_empty() {
                            pending_parents.push(Some(id.clone()));
                        }
                        folders.push(GraphMailFolder {
                            id,
                            display_name: f["displayName"].as_str().unwrap_or("").to_string(),
                            total_count: f["totalItemCount"].as_i64().unwrap_or(0),
                            unread_count: f["unreadItemCount"].as_i64().unwrap_or(0),
                            parent_folder_id: f["parentFolderId"].as_str().map(|s| s.to_string()),
                        });
                    }
                }
                let next = page["@odata.nextLink"].as_str().map(String::from);
                match next {
                    Some(next) => page = self.get_absolute(&next).await?,
                    None => break,
                }
            }
        }

        log::info!("Graph: found {} mail folders", folders.len());
        Ok(folders)
    }

    /// Fetch a single mail folder (fresh display name and counts).
    /// Used by per-folder sync so it works even for folders the local DB
    /// hasn't seen yet.
    pub async fn get_mail_folder(&self, folder_id: &str) -> Result<GraphMailFolder> {
        let f = self
            .get(
                &format!("/me/mailFolders/{}", folder_id),
                &[(
                    "$select",
                    "id,displayName,totalItemCount,unreadItemCount,parentFolderId",
                )],
            )
            .await?;
        Ok(GraphMailFolder {
            id: f["id"].as_str().unwrap_or(folder_id).to_string(),
            display_name: f["displayName"].as_str().unwrap_or("").to_string(),
            total_count: f["totalItemCount"].as_i64().unwrap_or(0),
            unread_count: f["unreadItemCount"].as_i64().unwrap_or(0),
            parent_folder_id: f["parentFolderId"].as_str().map(|s| s.to_string()),
        })
    }

    // -----------------------------------------------------------------------
    // Messages
    // -----------------------------------------------------------------------

    /// Fetch one page of a messages delta query for a folder.
    ///
    /// With `link == None` this starts a fresh (full) enumeration; with a
    /// stored `@odata.nextLink`/`@odata.deltaLink` it resumes/continues
    /// incremental sync. `$select` deliberately omits
    /// `internetMessageHeaders`: it forces Exchange to open the full
    /// property bag per item, and threading on Graph uses `conversationId`.
    ///
    /// A stored link that the server has expired (HTTP 410) surfaces as an
    /// error matched by [`is_delta_resync_required`]; the caller must clear
    /// its stored link and restart a full enumeration.
    pub async fn messages_delta_page(
        &self,
        folder_id: &str,
        link: Option<&str>,
    ) -> Result<GraphDeltaPage> {
        const DELTA_SELECT: &str = "id,subject,from,toRecipients,ccRecipients,receivedDateTime,\
                                    isRead,hasAttachments,flag,internetMessageId,conversationId,\
                                    bodyPreview";

        let what = format!("GET messages/delta for {}", folder_id);
        let resp = self
            .send_with_retry(
                || {
                    let req = match link {
                        Some(url) => self.http.get(url),
                        None => {
                            self.http
                                .get(self.endpoints.v1_url(&format!(
                                    "/me/mailFolders/{}/messages/delta",
                                    folder_id
                                )))
                                .query(&[("$select", DELTA_SELECT)])
                        }
                    };
                    req.bearer_auth(&self.access_token)
                        .header("Prefer", "odata.maxpagesize=200")
                },
                &what,
                true,
            )
            .await?;

        let status = resp.status();
        if status.as_u16() == 410 {
            return Err(Error::Other(format!(
                "{DELTA_RESYNC_MARKER} for folder {folder_id}"
            )));
        }
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::Other(format!(
                "Graph {} returned {}: {}",
                what,
                status,
                truncate(&body, 500)
            )));
        }
        let resp: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| Error::Other(format!("Graph JSON parse failed: {}", e)))?;

        Ok(parse_delta_page(&resp))
    }

    /// Search messages across all folders using `$search` (KQL).
    /// Graph requires `ConsistencyLevel: eventual` for `$search`. Cannot be
    /// combined with `$orderby` or `$filter`. On HTTP 429 (throttled),
    /// honors `Retry-After` once and returns whatever was retrieved.
    pub async fn search_messages(
        &self,
        account_id: &str,
        query: &SearchQuery,
    ) -> Result<Vec<SearchHit>> {
        let kql = match build_graph_kql(query) {
            Some(k) => k,
            None => return Ok(vec![]),
        };

        let url = self.endpoints.v1_url("/me/messages");
        // Graph $search REQUIRES the value to be wrapped in double quotes,
        // exactly once. `build_graph_kql` returns the bare KQL.
        let search_value = format!("\"{}\"", kql);
        let params = [
            (
                "$select",
                "id,subject,from,receivedDateTime,bodyPreview,internetMessageId,parentFolderId",
            ),
            ("$top", "50"),
            ("$search", search_value.as_str()),
        ];

        let mut attempts = 0u8;
        let body = loop {
            attempts += 1;
            let resp = self
                .http
                .get(&url)
                .bearer_auth(&self.access_token)
                .header("ConsistencyLevel", "eventual")
                .query(&params)
                .send()
                .await
                .map_err(|e| Error::Other(format!("Graph $search failed: {}", e)))?;

            let status = resp.status();
            if status.as_u16() == 429 && attempts < 2 {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(2)
                    .min(10);
                // Drain the body so reqwest can return the connection to the
                // pool; otherwise the next request opens a fresh socket.
                let _ = resp.bytes().await;
                log::warn!("Graph $search throttled, retrying after {}s", retry_after);
                tokio::time::sleep(std::time::Duration::from_secs(retry_after)).await;
                continue;
            }

            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(Error::Other(format!(
                    "Graph $search returned {}: {}",
                    status,
                    truncate(&text, 500)
                )));
            }
            break text;
        };

        let resp: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| Error::Other(format!("Graph $search parse failed: {}", e)))?;

        let mut hits = Vec::new();
        if let Some(values) = resp["value"].as_array() {
            for m in values {
                hits.push(parse_graph_search_hit(account_id, m));
            }
        }
        Ok(hits)
    }

    /// Download the raw RFC 5322 MIME message and stream it directly to a file.
    /// Returns the number of bytes written. Never buffers the full message in memory.
    pub async fn download_mime_to_file(
        &self,
        message_id: &str,
        dest: &std::path::Path,
    ) -> Result<u64> {
        self.stream_to_file(&format!("/me/messages/{}/$value", message_id), dest)
            .await
    }

    pub async fn save_draft(&self, message: &GraphDraftMessage) -> Result<()> {
        let body = graph_draft_json(message);

        self.post_json("/me/messages", &body).await?;
        log::info!("Graph: draft saved successfully");
        Ok(())
    }

    /// Execute pre-built `$batch` sub-requests (each carrying an `id` set to
    /// its global index), chunked at [`BATCH_SIZE`]. A 429 is retried after its
    /// per-item `Retry-After` delay. Callers may also enable 503/504 retries
    /// for convergent operations such as move and delete.
    async fn execute_batch_with_retry(
        &self,
        requests: Vec<serde_json::Value>,
        retry_transient_errors: bool,
    ) -> Result<Vec<Result<()>>> {
        const MAX_ROUNDS: u32 = 3;
        const MAX_RETRY_AFTER_SECS: u64 = 120;

        let total = requests.len();
        let mut results: Vec<Result<()>> = (0..total)
            .map(|_| Err(Error::Other("no $batch response for item".into())))
            .collect();

        let mut pending = requests;
        let mut round = 0u32;
        while !pending.is_empty() {
            round += 1;
            let mut retry_indices: Vec<(usize, u64)> = Vec::new();
            for chunk in pending.chunks(BATCH_SIZE) {
                let resp = self
                    .post_json("/$batch", &serde_json::json!({ "requests": chunk }))
                    .await?;
                retry_indices.extend(apply_batch_responses(
                    &resp,
                    &mut results,
                    retry_transient_errors,
                ));
            }

            if round >= MAX_ROUNDS {
                for (idx, _) in &retry_indices {
                    if *idx < total {
                        results[*idx] = Err(Error::Other(
                            "Graph $batch item still throttled after retries".into(),
                        ));
                    }
                }
                break;
            }

            let next: Vec<serde_json::Value> = pending
                .iter()
                .filter(|req| {
                    req["id"]
                        .as_str()
                        .and_then(|s| s.parse::<usize>().ok())
                        .map(|idx| retry_indices.iter().any(|(i, _)| *i == idx))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            if !next.is_empty() {
                let delay = retry_indices
                    .iter()
                    .map(|(_, d)| *d)
                    .max()
                    .unwrap_or(5)
                    .min(MAX_RETRY_AFTER_SECS);
                log::warn!(
                    "Graph $batch: {} throttled sub-request(s), retrying after {}s (round {}/{})",
                    next.len(),
                    delay,
                    round,
                    MAX_ROUNDS
                );
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            }
            pending = next;
        }

        Ok(results)
    }

    /// Move messages to a destination folder using JSON batching (20
    /// sub-requests per round trip instead of one round trip per message).
    /// Returns one outcome per input id, in order. Item-level failures
    /// carry the sub-response status and body, so a stale id shows up as
    /// `404 ... ErrorItemNotFound` exactly like the single-message call.
    pub async fn move_messages_batch(
        &self,
        message_ids: &[String],
        dest_folder_id: &str,
    ) -> Result<Vec<Result<()>>> {
        let requests: Vec<serde_json::Value> = message_ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                serde_json::json!({
                    "id": format!("{}", i),
                    "method": "POST",
                    "url": format!("/me/messages/{}/move", id),
                    "headers": { "Content-Type": "application/json" },
                    "body": { "destinationId": dest_folder_id }
                })
            })
            .collect();
        self.execute_batch_with_retry(requests, true).await
    }

    /// Copy messages to a destination folder using JSON batching.
    ///
    /// Unlike move and delete, copy is not idempotent. Retry definite 429
    /// throttling responses, but leave ambiguous 503/504 outcomes as errors so
    /// this call does not immediately create a second copy.
    pub async fn copy_messages_batch(
        &self,
        message_ids: &[String],
        dest_folder_id: &str,
    ) -> Result<Vec<Result<()>>> {
        let requests = build_copy_batch_requests(message_ids, dest_folder_id);
        self.execute_batch_with_retry(requests, false).await
    }

    /// Delete messages using JSON batching. Same contract as
    /// [`Self::move_messages_batch`].
    pub async fn delete_messages_batch(&self, message_ids: &[String]) -> Result<Vec<Result<()>>> {
        let requests: Vec<serde_json::Value> = message_ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                serde_json::json!({
                    "id": format!("{}", i),
                    "method": "DELETE",
                    "url": format!("/me/messages/{}", id),
                })
            })
            .collect();
        self.execute_batch_with_retry(requests, true).await
    }

    /// Delete a mail folder.
    pub async fn delete_mail_folder(&self, folder_id: &str) -> Result<()> {
        self.delete(&format!("/me/mailFolders/{}", folder_id)).await
    }

    /// Create a mail folder. When `parent_id` is `Some`, creates a child folder
    /// under that parent; otherwise creates a top-level folder. Returns the new
    /// folder's Graph ID.
    pub async fn create_mail_folder(&self, name: &str, parent_id: Option<&str>) -> Result<String> {
        log::info!(
            "Graph creating mail folder: {} (parent={:?})",
            name,
            parent_id
        );
        let body = serde_json::json!({ "displayName": name });
        let path = match parent_id {
            Some(pid) => format!("/me/mailFolders/{}/childFolders", pid),
            None => "/me/mailFolders".to_string(),
        };
        let resp = self.post_json(&path, &body).await?;
        let id = resp["id"].as_str().unwrap_or("").to_string();
        if id.is_empty() {
            return Err(Error::Other(
                "Graph mailFolders create returned no id".into(),
            ));
        }
        log::info!("Graph mail folder created: id={}", id);
        Ok(id)
    }

    /// Mark messages as read or unread and return one outcome per input id.
    pub async fn set_read_status_batch(
        &self,
        message_ids: &[String],
        is_read: bool,
    ) -> Result<Vec<Result<()>>> {
        let requests: Vec<serde_json::Value> = message_ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                serde_json::json!({
                    "id": format!("{}", i),
                    "method": "PATCH",
                    "url": format!("/me/messages/{}", id),
                    "headers": { "Content-Type": "application/json" },
                    "body": { "isRead": is_read }
                })
            })
            .collect();
        self.execute_batch_with_retry(requests, true).await
    }

    /// Set supported mail flags through Graph JSON batching.
    pub async fn set_flags(
        &self,
        message_ids: &[String],
        flags: &[String],
        add: bool,
    ) -> Result<()> {
        let updates = graph_flag_updates(flags, add);
        if updates.as_object().is_none_or(serde_json::Map::is_empty) {
            return Ok(());
        }
        let requests: Vec<serde_json::Value> = message_ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                serde_json::json!({
                    "id": i.to_string(),
                    "method": "PATCH",
                    "url": format!("/me/messages/{}", id),
                    "headers": { "Content-Type": "application/json" },
                    "body": updates,
                })
            })
            .collect();
        for outcome in self.execute_batch_with_retry(requests, true).await? {
            outcome.map_err(|error| {
                Error::Other(format!("Graph set-flags batch item failed: {}", error))
            })?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Calendar
    // -----------------------------------------------------------------------

    /// List all calendars for the signed-in user.
    pub async fn list_calendars(&self) -> Result<Vec<GraphCalendar>> {
        let resp = self
            .get(
                "/me/calendars",
                &[("$select", "id,name,color,isDefaultCalendar")],
            )
            .await?;
        let items = resp["value"].as_array().cloned().unwrap_or_default();
        Ok(items
            .iter()
            .map(|c| GraphCalendar {
                id: c["id"].as_str().unwrap_or("").to_string(),
                name: c["name"].as_str().unwrap_or("Calendar").to_string(),
                color: graph_color_to_hex(c["color"].as_str().unwrap_or("")),
                is_default: c["isDefaultCalendar"].as_bool().unwrap_or(false),
            })
            .collect())
    }

    /// List meeting rooms for O365 event creation via the beta room-list API.
    pub async fn list_rooms(&self) -> Result<Vec<GraphRoom>> {
        let mut rooms = Vec::new();

        log::info!("Graph rooms: listing room lists via v1.0 /places/microsoft.graph.roomlist");
        let room_lists = match self
            .get_all(
                "/places/microsoft.graph.roomlist",
                &[("$top", "200"), ("$select", "displayName,emailAddress")],
            )
            .await
        {
            Ok(value) => Some(value),
            Err(e) => {
                log::warn!(
                    "Graph rooms: v1.0 /places/microsoft.graph.roomlist failed, falling back to beta room-list lookup: {}",
                    e
                );
                None
            }
        };

        if let Some(room_lists) = room_lists {
            let lists = parse_graph_named_addresses(&serde_json::Value::Array(room_lists));
            log::info!("Graph rooms: found {} room lists", lists.len());
            for (name, address) in lists {
                log::info!("Graph rooms: using room list '{}' <{}>", name, address);

                let path = format!(
                    "/places/{}/microsoft.graph.roomlist/rooms",
                    urlencoding::encode(&address)
                );
                log::debug!(
                    "Graph rooms: fetching rooms for list '{}' <{}> via Places",
                    name,
                    address
                );

                match self
                    .get_all(
                        &path,
                        &[("$top", "200"), ("$select", "displayName,emailAddress")],
                    )
                    .await
                {
                    Ok(resp) => {
                        let mut list_rooms = parse_graph_rooms(&serde_json::Value::Array(resp));
                        log::info!(
                            "Graph rooms: list '{}' returned {} rooms",
                            if name.is_empty() { address } else { name },
                            list_rooms.len()
                        );
                        rooms.append(&mut list_rooms);
                    }
                    Err(e) => {
                        log::warn!(
                            "Graph rooms: Places rooms failed for list '{}' <{}>: {}",
                            name,
                            address,
                            e
                        );
                    }
                }
            }
        }

        if rooms.is_empty() {
            log::info!("Graph rooms: falling back to v1.0 /places/microsoft.graph.room");
            match self
                .get_all(
                    "/places/microsoft.graph.room",
                    &[("$top", "200"), ("$select", "displayName,emailAddress")],
                )
                .await
            {
                Ok(resp) => {
                    let mut place_rooms = parse_graph_rooms(&serde_json::Value::Array(resp));
                    log::info!(
                        "Graph rooms: places direct rooms returned {} rooms",
                        place_rooms.len()
                    );
                    rooms.append(&mut place_rooms);
                }
                Err(e) => {
                    log::warn!(
                        "Graph rooms: v1.0 /places/microsoft.graph.room failed, falling back to beta /me/findRoomLists: {}",
                        e
                    );
                }
            }
        }

        if rooms.is_empty() {
            log::info!("Graph rooms: listing room lists via beta /me/findRoomLists");
            let room_lists = match self.get_beta("/me/findRoomLists", &[]).await {
                Ok(value) => Some(value),
                Err(e) => {
                    log::warn!(
                        "Graph rooms: beta /me/findRoomLists failed, falling back to direct rooms lookup: {}",
                        e
                    );
                    None
                }
            };

            if let Some(room_lists) = room_lists {
                let lists = parse_graph_named_addresses(&room_lists["value"]);
                log::info!("Graph rooms: found {} beta room lists", lists.len());
                for (name, address) in lists {
                    let path = format!("/me/findRooms(RoomList='{}')", address.replace('\'', "''"));
                    log::debug!(
                        "Graph rooms: fetching beta rooms for list '{}' <{}>",
                        name,
                        address
                    );

                    match self.get_beta(&path, &[]).await {
                        Ok(resp) => {
                            let mut list_rooms = parse_graph_rooms(&resp["value"]);
                            log::info!(
                                "Graph rooms: beta list '{}' returned {} rooms",
                                if name.is_empty() { address } else { name },
                                list_rooms.len()
                            );
                            rooms.append(&mut list_rooms);
                        }
                        Err(e) => {
                            log::warn!(
                                "Graph rooms: beta findRooms failed for list '{}' <{}>: {}",
                                name,
                                address,
                                e
                            );
                        }
                    }
                }
            }
        }

        if rooms.is_empty() {
            log::info!("Graph rooms: falling back to beta /me/findRooms");
            match self.get_beta("/me/findRooms", &[]).await {
                Ok(resp) => {
                    let mut direct_rooms = parse_graph_rooms(&resp["value"]);
                    log::info!(
                        "Graph rooms: direct findRooms returned {} rooms",
                        direct_rooms.len()
                    );
                    rooms.append(&mut direct_rooms);
                }
                Err(e) => {
                    log::warn!("Graph rooms: beta /me/findRooms failed: {}", e);
                }
            }
        }

        let unique = dedupe_graph_rooms(rooms);
        log::info!("Graph rooms: returning {} normalized rooms", unique.len());
        Ok(unique)
    }

    /// Check whether a room resource is free for a specific time range.
    pub async fn get_room_availability(
        &self,
        room_address: &str,
        start: &str,
        end: &str,
    ) -> Result<GraphRoomAvailability> {
        let start_utc = normalize_schedule_datetime(start)?;
        let end_utc = normalize_schedule_datetime(end)?;

        log::debug!(
            "Graph rooms: getSchedule for {} from {} to {}",
            room_address,
            start_utc,
            end_utc
        );

        let body = serde_json::json!({
            "schedules": [room_address],
            "startTime": {
                "dateTime": start_utc,
                "timeZone": "UTC",
            },
            "endTime": {
                "dateTime": end_utc,
                "timeZone": "UTC",
            },
            "availabilityViewInterval": 30,
        });

        let resp = self.post_json("/me/calendar/getSchedule", &body).await?;
        Ok(parse_graph_room_availability(&resp))
    }

    /// Fetch free/busy data for meeting participants. Per-recipient
    /// Graph errors are represented as `available: false` with no busy
    /// periods so callers never mistake unavailable data for free time.
    pub async fn get_schedules(
        &self,
        emails: &[String],
        start: &str,
        end: &str,
    ) -> Result<Vec<GraphSchedule>> {
        let start_utc = normalize_schedule_datetime(start)?;
        let end_utc = normalize_schedule_datetime(end)?;
        let mut schedules = Vec::with_capacity(emails.len());
        // Graph limits getSchedule to 20 addresses per request.
        for batch in emails.chunks(20) {
            let body = serde_json::json!({
                "schedules": batch,
                "startTime": { "dateTime": start_utc, "timeZone": "UTC" },
                "endTime": { "dateTime": end_utc, "timeZone": "UTC" },
                "availabilityViewInterval": 30,
            });
            let resp = self.post_json("/me/calendar/getSchedule", &body).await?;
            schedules.extend(parse_graph_schedules(&resp));
        }
        Ok(schedules)
    }

    /// Rename a calendar via PATCH /me/calendars/{id}.
    pub async fn rename_calendar(&self, calendar_id: &str, new_name: &str) -> Result<()> {
        log::info!("Graph rename calendar: id={} -> {}", calendar_id, new_name);
        let path = format!("/me/calendars/{}", urlencoding::encode(calendar_id));
        self.patch_json(&path, &serde_json::json!({ "name": new_name }))
            .await
    }

    /// Set a calendar's color via PATCH /me/calendars/{id}. The
    /// writable property is the `color` field, whose value comes
    /// from the constrained `calendarColor` enum (`auto`,
    /// `lightBlue`, `lightGreen`, …). Translate the user's hex to
    /// the nearest of those names via simple RGB Euclidean distance.
    /// `maxColor` is excluded from the candidate set — Microsoft
    /// documents it as a sentinel ordinal and PATCHing with it
    /// returns 500 ISE in practice. The caller keeps the original
    /// hex in our local DB so the sidebar shows what the user
    /// actually picked even when Graph snapped it to a neighbour.
    pub async fn set_calendar_color(&self, calendar_id: &str, hex: &str) -> Result<()> {
        let named = nearest_outlook_color(hex);
        log::info!(
            "Graph set color: id={} hex={} -> {}",
            calendar_id,
            hex,
            named
        );
        let path = format!("/me/calendars/{}", urlencoding::encode(calendar_id));
        self.patch_json(&path, &serde_json::json!({ "color": named }))
            .await
    }

    /// Fetch events for a specific calendar via `GET /me/calendars/{id}/calendarView`.
    /// Uses `Prefer: outlook.timezone="UTC"` and follows `@odata.nextLink`.
    pub async fn list_events_for_calendar(
        &self,
        calendar_id: &str,
        start: &str,
        end: &str,
    ) -> Result<Vec<GraphCalendarEvent>> {
        let mut events = Vec::new();
        let mut next_path: Option<String> = None;
        loop {
            let resp: serde_json::Value = match next_path.take() {
                Some(path) => {
                    let resp = self
                        .http
                        .get(&path)
                        .bearer_auth(&self.access_token)
                        .header("Prefer", "outlook.timezone=\"UTC\"")
                        .send()
                        .await
                        .map_err(|e| Error::Other(format!("Graph GET failed: {}", e)))?;
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    if !status.is_success() {
                        return Err(Error::Other(format!(
                            "Graph GET returned {}: {}",
                            status,
                            truncate(&body, 500)
                        )));
                    }
                    serde_json::from_str(&body)
                        .map_err(|e| Error::Other(format!("Graph JSON parse failed: {}", e)))?
                }
                None => {
                    let url = self.endpoints.v1_url(&format!(
                        "/me/calendars/{}/calendarView",
                        urlencoding::encode(calendar_id)
                    ));
                    let resp = self.http
                        .get(&url)
                        .bearer_auth(&self.access_token)
                        .header("Prefer", "outlook.timezone=\"UTC\"")
                        .query(&[
                            ("startDateTime", start),
                            ("endDateTime", end),
                            ("$select", "id,subject,bodyPreview,start,end,location,isAllDay,organizer,attendees,iCalUId,responseStatus"),
                            ("$top", "100"),
                            ("$orderby", "start/dateTime"),
                        ])
                        .send()
                        .await
                        .map_err(|e| Error::Other(format!("Graph GET /me/calendars/{}/calendarView failed: {}", calendar_id, e)))?;
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    if !status.is_success() {
                        return Err(Error::Other(format!(
                            "Graph GET /me/calendars/{}/calendarView returned {}: {}",
                            calendar_id,
                            status,
                            truncate(&body, 500)
                        )));
                    }
                    serde_json::from_str(&body)
                        .map_err(|e| Error::Other(format!("Graph JSON parse failed: {}", e)))?
                }
            };
            if let Some(items) = resp["value"].as_array() {
                for e in items {
                    events.push(parse_graph_event(e));
                }
            }
            let next_link = resp["@odata.nextLink"]
                .as_str()
                .map(|s: &str| s.to_string());
            match next_link {
                Some(next) => next_path = Some(next),
                None => break,
            }
        }
        Ok(events)
    }

    /// Create a calendar event.
    /// Create a calendar event. Returns (graph_id, iCalUid).
    pub async fn create_event(
        &self,
        event: &serde_json::Value,
    ) -> Result<(String, Option<String>)> {
        let resp = self.post_json("/me/events", event).await?;
        let id = resp["id"].as_str().unwrap_or("").to_string();
        let ical_uid = resp["iCalUId"].as_str().map(|s| s.to_string());
        Ok((id, ical_uid))
    }

    /// Update a calendar event.
    pub async fn update_event(&self, event_id: &str, updates: &serde_json::Value) -> Result<()> {
        self.patch_json(&format!("/me/events/{}", event_id), updates)
            .await
    }

    /// Delete a calendar event.
    pub async fn delete_event(&self, event_id: &str) -> Result<()> {
        self.delete(&format!("/me/events/{}", event_id)).await
    }

    /// Find an event by its iCalUId. Returns the Graph event ID if found.
    pub async fn find_event_by_ical_uid(&self, ical_uid: &str) -> Result<Option<String>> {
        // Escape single quotes per OData rules to prevent filter injection.
        let escaped_uid = ical_uid.replace('\'', "''");
        let filter = format!("iCalUId eq '{}'", escaped_uid);
        let resp = self
            .get(
                "/me/events",
                &[("$filter", filter.as_str()), ("$select", "id")],
            )
            .await?;
        Ok(resp["value"]
            .as_array()
            .and_then(|a: &Vec<serde_json::Value>| a.first())
            .and_then(|e: &serde_json::Value| e["id"].as_str())
            .map(|s: &str| s.to_string()))
    }

    /// RSVP to an event (accept, tentativelyAccept, or decline).
    pub async fn rsvp_event(&self, event_id: &str, response: &str, comment: &str) -> Result<()> {
        let action = match response {
            "accepted" => "accept",
            "tentative" => "tentativelyAccept",
            "declined" => "decline",
            other => other,
        };
        let body = serde_json::json!({
            "comment": comment,
            "sendResponse": true,
        });
        self.post_json(&format!("/me/events/{}/{}", event_id, action), &body)
            .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Contacts
    // -----------------------------------------------------------------------

    /// List all contacts for the signed-in user.
    pub async fn list_contacts(&self) -> Result<Vec<GraphContact>> {
        let mut contacts = Vec::new();
        let mut next_path: Option<String> = None;
        loop {
            let resp: serde_json::Value = match next_path.take() {
                Some(path) => self.get_absolute(&path).await?,
                None => {
                    self.get(
                        "/me/contacts",
                        &[
                            ("$select", "id,displayName,emailAddresses,mobilePhone,businessPhones,homePhones,companyName,jobTitle"),
                            ("$top", "500"),
                            ("$orderby", "displayName"),
                        ],
                    ).await?
                }
            };
            let items = resp
                .get("value")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    Error::Other("Graph contacts response `value` must be an array".into())
                })?;
            for contact in items {
                contacts.push(parse_graph_contact(contact)?);
            }
            match resp.get("@odata.nextLink") {
                None | Some(serde_json::Value::Null) => break,
                Some(serde_json::Value::String(next)) if !next.trim().is_empty() => {
                    next_path = Some(next.clone());
                }
                Some(serde_json::Value::String(_)) => {
                    return Err(Error::Other(
                        "Graph contacts response `@odata.nextLink` must not be empty".into(),
                    ));
                }
                Some(_) => {
                    return Err(Error::Other(
                        "Graph contacts response `@odata.nextLink` must be a string or null".into(),
                    ));
                }
            }
        }
        Ok(contacts)
    }

    /// Create a contact.
    pub async fn create_contact(&self, contact: &serde_json::Value) -> Result<String> {
        let resp = self.post_json("/me/contacts", contact).await?;
        let id = resp
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| {
                Error::Other("Graph create-contact response `id` must be non-empty".into())
            })?;
        Ok(id.to_string())
    }

    /// Update a contact.
    pub async fn update_contact(
        &self,
        contact_id: &str,
        updates: &serde_json::Value,
    ) -> Result<()> {
        self.patch_json(&format!("/me/contacts/{}", contact_id), updates)
            .await
    }

    /// Delete a contact.
    pub async fn delete_contact(&self, contact_id: &str) -> Result<()> {
        self.delete(&format!("/me/contacts/{}", contact_id)).await
    }
}

fn graph_draft_json(message: &GraphDraftMessage) -> serde_json::Value {
    serde_json::json!({
        "subject": message.subject,
        "body": {
            "contentType": "Text",
            "content": message.body_text
        },
        "toRecipients": message.to.iter().map(|e| {
            serde_json::json!({ "emailAddress": { "address": e } })
        }).collect::<Vec<_>>(),
        "ccRecipients": message.cc.iter().map(|e| {
            serde_json::json!({ "emailAddress": { "address": e } })
        }).collect::<Vec<_>>(),
        "bccRecipients": message.bcc.iter().map(|e| {
            serde_json::json!({ "emailAddress": { "address": e } })
        }).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod draft_tests {
    use super::{graph_draft_json, GraphDraftMessage};

    #[test]
    fn structured_draft_preserves_all_fields() {
        let message = GraphDraftMessage {
            to: vec!["to@example.com".into()],
            cc: vec!["cc@example.com".into()],
            bcc: vec!["bcc@example.com".into()],
            subject: "Draft subject".into(),
            body_text: "Draft body".into(),
        };

        assert_eq!(
            graph_draft_json(&message),
            serde_json::json!({
                "subject": "Draft subject",
                "body": {
                    "contentType": "Text",
                    "content": "Draft body"
                },
                "toRecipients": [{
                    "emailAddress": { "address": "to@example.com" }
                }],
                "ccRecipients": [{
                    "emailAddress": { "address": "cc@example.com" }
                }],
                "bccRecipients": [{
                    "emailAddress": { "address": "bcc@example.com" }
                }]
            })
        );
    }
}

fn graph_flag_updates(flags: &[String], add: bool) -> serde_json::Value {
    let mut updates = serde_json::Map::new();
    for flag in flags {
        match flag.as_str() {
            "seen" => {
                updates.insert("isRead".into(), serde_json::Value::Bool(add));
            }
            "flagged" => {
                updates.insert(
                    "flag".into(),
                    serde_json::json!({
                        "flagStatus": if add { "flagged" } else { "notFlagged" }
                    }),
                );
            }
            local_only => {
                log::debug!(
                    "Graph keeps mail flag '{}' local-only because it has no remote mapping",
                    local_only
                );
            }
        }
    }
    serde_json::Value::Object(updates)
}

#[cfg(test)]
mod flag_update_tests {
    use super::graph_flag_updates;

    #[test]
    fn maps_seen_and_flagged_updates() {
        let flags = vec!["seen".to_string(), "flagged".to_string()];
        assert_eq!(
            graph_flag_updates(&flags, true),
            serde_json::json!({
                "isRead": true,
                "flag": { "flagStatus": "flagged" },
            })
        );
        assert_eq!(
            graph_flag_updates(&flags, false),
            serde_json::json!({
                "isRead": false,
                "flag": { "flagStatus": "notFlagged" },
            })
        );
    }

    #[test]
    fn keeps_unmapped_flags_local_only() {
        let updates = graph_flag_updates(&["answered".to_string()], true);
        assert_eq!(updates, serde_json::json!({}));
    }

    #[test]
    fn mixed_flags_still_apply_supported_updates() {
        let updates = graph_flag_updates(&["seen".to_string(), "answered".to_string()], true);
        assert_eq!(updates, serde_json::json!({ "isRead": true }));
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct GraphContact {
    pub id: String,
    pub display_name: String,
    pub emails_json: String,
    pub phones_json: String,
    pub organization: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GraphUser {
    pub display_name: String,
    /// The actual mailbox email (from Sent Items or /me)
    pub email: String,
    /// The Microsoft login identity (from /me — used for XOAUTH2)
    pub login_email: String,
}

/// Whether `s` is a plausible SMTP addr-spec (`local@domain.tld`).
///
/// Graph's `emailAddress.address` is NOT always an SMTP address. For an
/// Exchange Online mailbox the `from` of a Sent Items message (and
/// other internal-sender fields) is frequently a legacy X.500 / "EX"
/// distinguished name, e.g.
/// `/O=EXCHANGELABS/OU=EXCHANGE ADMINISTRATIVE GROUP (FYDIBOHF23SPDLT)/CN=RECIPIENTS/CN=...`.
/// The mailbox-address heuristic in `get_me` must reject those, or the
/// EX DN gets shown to the user as their email address and overwrites
/// the real SMTP address that `/me` already returned correctly.
fn looks_like_smtp_address(s: &str) -> bool {
    match s.trim().split_once('@') {
        Some((local, domain)) => !local.is_empty() && domain.contains('.'),
        None => false,
    }
}

/// One entry of a messages-delta response, in server order.
///
/// The same message id can appear several times in one delta response
/// (e.g. an update followed by a removal). Order matters: applying all
/// removals first and all upserts second would resurrect a message whose
/// final state is "removed".
#[derive(Debug, Clone)]
pub enum GraphDeltaEvent {
    /// Created or updated message (full selected properties). Boxed to
    /// keep the enum small next to the `Removed` variant.
    Upsert(Box<GraphMessage>),
    /// Message removed from the folder (deleted or moved out).
    Removed(String),
}

/// One page of a Graph messages-delta response.
#[derive(Debug, Clone)]
pub struct GraphDeltaPage {
    /// Delta entries in the exact order the server returned them.
    pub events: Vec<GraphDeltaEvent>,
    /// More pages are available right now (`@odata.nextLink`).
    pub next_link: Option<String>,
    /// Checkpoint to store for the next sync cycle (`@odata.deltaLink`).
    /// Present only on the final page of a round.
    pub delta_link: Option<String>,
}

/// Parse a delta-response body into an ordered page. Preserves server
/// order: the same id can appear as an update and then a removal in one
/// response, and the LAST event must win when applied sequentially.
fn parse_delta_page(resp: &serde_json::Value) -> GraphDeltaPage {
    let mut events = Vec::new();
    if let Some(values) = resp["value"].as_array() {
        for m in values {
            if m.get("@removed").is_some() {
                if let Some(id) = m["id"].as_str() {
                    events.push(GraphDeltaEvent::Removed(id.to_string()));
                }
            } else {
                events.push(GraphDeltaEvent::Upsert(Box::new(parse_graph_message(m))));
            }
        }
    }
    GraphDeltaPage {
        events,
        next_link: resp["@odata.nextLink"].as_str().map(String::from),
        delta_link: resp["@odata.deltaLink"].as_str().map(String::from),
    }
}

/// Marker embedded in the error for HTTP 410 on a delta call: the stored
/// delta token expired server-side and the folder needs a full resync.
const DELTA_RESYNC_MARKER: &str = "graph delta resync required";

/// True if the error is a delta-state expiry (HTTP 410). The caller should
/// clear the stored delta link and restart a full enumeration.
pub fn is_delta_resync_required(err: &crate::error::Error) -> bool {
    err.to_string().contains(DELTA_RESYNC_MARKER)
}

fn profile_email_from_me(mail: Option<&str>, user_principal_name: Option<&str>) -> String {
    let mail = mail.unwrap_or("").trim();
    let user_principal_name = user_principal_name.unwrap_or("").trim();

    if looks_like_smtp_address(mail) {
        mail.to_string()
    } else if looks_like_smtp_address(user_principal_name) {
        user_principal_name.to_string()
    } else if !mail.is_empty() {
        mail.to_string()
    } else {
        user_principal_name.to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphMailFolder {
    pub id: String,
    pub display_name: String,
    pub total_count: i64,
    pub unread_count: i64,
    pub parent_folder_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GraphMessage {
    pub id: String,
    pub subject: Option<String>,
    pub from_name: Option<String>,
    pub from_email: Option<String>,
    pub to_addresses: String,
    pub cc_addresses: String,
    pub date: String,
    pub is_read: bool,
    /// Graph `flag.flagStatus == "flagged"` — the server-side star/flag.
    pub is_flagged: bool,
    pub has_attachments: bool,
    pub internet_message_id: Option<String>,
    pub conversation_id: Option<String>,
    pub preview: Option<String>,
}

pub struct GraphDraftMessage {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: String,
}

#[derive(Debug, Clone)]
pub struct GraphCalendar {
    pub id: String,
    pub name: String,
    pub color: String,
    pub is_default: bool,
}

#[derive(Debug, Clone)]
pub struct GraphCalendarEvent {
    pub id: String,
    pub subject: String,
    pub body_preview: Option<String>,
    pub start: String,
    pub end: String,
    pub all_day: bool,
    pub timezone: Option<String>,
    pub location: Option<String>,
    pub organizer_email: Option<String>,
    pub attendees_json: Option<String>,
    /// The signed-in user's own RSVP to this event, in iCal PARTSTAT
    /// vocabulary. `None` for events with no RSVP concept (events the user
    /// organized, or invites not yet responded to).
    pub my_status: Option<String>,
    pub ical_uid: Option<String>,
}

fn parse_graph_rooms(value: &serde_json::Value) -> Vec<GraphRoom> {
    parse_graph_named_addresses(value)
        .into_iter()
        .map(|(name, address)| GraphRoom { name, address })
        .collect()
}

fn normalize_schedule_datetime(datetime: &str) -> Result<String> {
    chrono::DateTime::parse_from_rfc3339(datetime)
        .map(|dt| {
            dt.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
        })
        .map_err(|e| Error::Other(format!("Invalid schedule datetime '{}': {}", datetime, e)))
}

fn parse_graph_room_availability(value: &serde_json::Value) -> GraphRoomAvailability {
    let Some(schedule) = value["value"]
        .as_array()
        .and_then(|entries| entries.first())
    else {
        return GraphRoomAvailability {
            state: "unknown".into(),
            busy_start: None,
            busy_end: None,
        };
    };

    // Graph getSchedule reports per-recipient failures (unresolvable
    // mailbox, free/busy not published, throttling) via an `error`
    // object on the schedule entry. Without `scheduleItems`/
    // `availabilityView` we genuinely don't know — never report "free".
    if !schedule["error"].is_null()
        || (schedule["scheduleItems"].as_array().is_none()
            && schedule["availabilityView"].as_str().is_none())
    {
        return GraphRoomAvailability {
            state: "unknown".into(),
            busy_start: None,
            busy_end: None,
        };
    }

    if let Some(item) = schedule["scheduleItems"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| {
            matches!(
                item["status"]
                    .as_str()
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .as_str(),
                "busy" | "tentative" | "oof" | "workingelsewhere"
            )
        })
    {
        return GraphRoomAvailability {
            state: "busy".into(),
            busy_start: item["start"]["dateTime"].as_str().map(|s| s.to_string()),
            busy_end: item["end"]["dateTime"].as_str().map(|s| s.to_string()),
        };
    }

    GraphRoomAvailability {
        state: "available".into(),
        busy_start: None,
        busy_end: None,
    }
}

fn parse_graph_schedules(value: &serde_json::Value) -> Vec<GraphSchedule> {
    value["value"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|schedule| {
            let email = schedule["scheduleId"].as_str()?.to_string();
            let items = schedule["scheduleItems"].as_array();
            let available = schedule["error"].is_null() && items.is_some();
            let busy = items
                .into_iter()
                .flatten()
                .filter(|item| {
                    matches!(
                        item["status"]
                            .as_str()
                            .unwrap_or("")
                            .to_ascii_lowercase()
                            .as_str(),
                        "busy" | "tentative" | "oof" | "workingelsewhere" | "unknown"
                    )
                })
                .filter_map(|item| {
                    Some(GraphBusyPeriod {
                        start: item["start"]["dateTime"].as_str()?.to_string(),
                        end: item["end"]["dateTime"].as_str()?.to_string(),
                    })
                })
                .collect();
            Some(GraphSchedule {
                email,
                available,
                busy,
            })
        })
        .collect()
}

#[cfg(test)]
mod schedule_tests {
    use super::*;

    #[test]
    fn parser_keeps_blocking_statuses_and_marks_errors_unknown() {
        let value = serde_json::json!({ "value": [
            { "scheduleId": "known@example.org", "scheduleItems": [
                { "status": "free", "start": { "dateTime": "2026-08-10T08:00:00" }, "end": { "dateTime": "2026-08-10T09:00:00" } },
                { "status": "tentative", "start": { "dateTime": "2026-08-10T09:00:00" }, "end": { "dateTime": "2026-08-10T10:00:00" } },
                { "status": "unknown", "start": { "dateTime": "2026-08-10T10:00:00" }, "end": { "dateTime": "2026-08-10T11:00:00" } }
            ] },
            { "scheduleId": "hidden@example.org", "error": { "code": "ErrorMailRecipientNotFound" } }
        ]});
        let schedules = parse_graph_schedules(&value);
        assert!(schedules[0].available);
        assert_eq!(schedules[0].busy.len(), 2);
        assert!(!schedules[1].available);
        assert!(schedules[1].busy.is_empty());
    }
}

fn parse_graph_named_addresses(value: &serde_json::Value) -> Vec<(String, String)> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let address = entry["address"]
                .as_str()
                .or_else(|| entry["emailAddress"].as_str())?
                .trim();
            if address.is_empty() {
                return None;
            }

            let name = entry["name"]
                .as_str()
                .or_else(|| entry["displayName"].as_str())
                .unwrap_or(address)
                .trim();
            Some((
                if name.is_empty() {
                    address.to_string()
                } else {
                    name.to_string()
                },
                address.to_string(),
            ))
        })
        .collect()
}

fn dedupe_graph_rooms(rooms: Vec<GraphRoom>) -> Vec<GraphRoom> {
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::new();

    for room in rooms {
        let key = room.address.to_ascii_lowercase();
        if seen.insert(key) {
            unique.push(room);
        }
    }

    unique.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
    unique
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_graph_search_hit(account_id: &str, m: &serde_json::Value) -> SearchHit {
    let id = m["id"].as_str().unwrap_or("").to_string();
    let subject = m["subject"].as_str().unwrap_or("").to_string();
    let from = &m["from"]["emailAddress"];
    let from_name = from["name"].as_str().map(|s| s.to_string());
    let from_email = from["address"].as_str().map(|s| s.to_string());

    let date = m["receivedDateTime"]
        .as_str()
        .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
        .map(|dt| dt.timestamp())
        .unwrap_or(0);

    let snippet = m["bodyPreview"].as_str().map(|s| s.to_string());
    let folder_path = m["parentFolderId"].as_str().unwrap_or("").to_string();
    let message_id = m["internetMessageId"]
        .as_str()
        .and_then(normalize_message_id);

    SearchHit {
        account_id: account_id.to_string(),
        folder_path,
        uid: None,
        message_id,
        backend_id: id,
        subject,
        from_name,
        from_email,
        date,
        snippet,
    }
}

fn parse_graph_message(m: &serde_json::Value) -> GraphMessage {
    let from = &m["from"]["emailAddress"];
    let from_name = from["name"].as_str().map(|s| s.to_string());
    let from_email = from["address"].as_str().map(|s| s.to_string());

    let to_addresses = parse_recipients(&m["toRecipients"]);
    let cc_addresses = parse_recipients(&m["ccRecipients"]);

    let date = m["receivedDateTime"]
        .as_str()
        .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
        .unwrap_or_default();

    GraphMessage {
        id: m["id"].as_str().unwrap_or("").to_string(),
        subject: m["subject"].as_str().map(|s| s.to_string()),
        from_name,
        from_email,
        to_addresses,
        cc_addresses,
        date,
        is_read: m["isRead"].as_bool().unwrap_or(false),
        is_flagged: m["flag"]["flagStatus"].as_str() == Some("flagged"),
        has_attachments: m["hasAttachments"].as_bool().unwrap_or(false),
        internet_message_id: m["internetMessageId"]
            .as_str()
            .and_then(normalize_message_id),
        conversation_id: m["conversationId"].as_str().map(|s| s.to_string()),
        preview: m["bodyPreview"].as_str().map(|s| s.to_string()),
    }
}

fn parse_recipients(arr: &serde_json::Value) -> String {
    let addrs: Vec<serde_json::Value> = arr
        .as_array()
        .map(|a| {
            a.iter()
                .map(|r| {
                    serde_json::json!({
                        "name": r["emailAddress"]["name"].as_str().unwrap_or(""),
                        "email": r["emailAddress"]["address"].as_str().unwrap_or(""),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::to_string(&addrs).unwrap_or_else(|_| "[]".to_string())
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}

/// Map well-known Graph folder display names to our folder_type.
pub fn guess_folder_type(display_name: &str) -> Option<&'static str> {
    match display_name {
        "Inbox" => Some("inbox"),
        "Sent Items" => Some("sent"),
        "Drafts" => Some("drafts"),
        "Deleted Items" => Some("trash"),
        "Junk Email" => Some("junk"),
        "Archive" => Some("archive"),
        _ => None,
    }
}

/// Translate a Microsoft Graph attendee `status.response` value into the
/// iCal PARTSTAT vocabulary the rest of the app (and the UI) uses. Graph
/// emits its own `responseType` enum (`none`, `organizer`, `accepted`,
/// `tentativelyAccepted`, `declined`, `notResponded`); storing those verbatim
/// makes the event popup show e.g. "tentativelyAccepted" instead of "Maybe".
fn graph_response_to_partstat(response: &str) -> &'static str {
    match response {
        "accepted" => "accepted",
        "tentativelyAccepted" => "tentative",
        "declined" => "declined",
        // The organizer implicitly accepts their own event.
        "organizer" => "accepted",
        // "none", "notResponded", and anything unrecognised.
        _ => "needs-action",
    }
}

/// Translate the event-level Graph `responseStatus.response` (the signed-in
/// user's own RSVP) into an `Option` of iCal PARTSTAT. Only a genuine RSVP
/// produces a value: `organizer`, `none`, and `notResponded` map to `None`
/// so the UI doesn't show a stray status badge on the user's own events.
fn graph_response_to_my_status(response: &str) -> Option<String> {
    match response {
        "accepted" => Some("accepted".to_string()),
        "tentativelyAccepted" => Some("tentative".to_string()),
        "declined" => Some("declined".to_string()),
        _ => None,
    }
}

fn parse_graph_event(e: &serde_json::Value) -> GraphCalendarEvent {
    let start_obj = &e["start"];
    let end_obj = &e["end"];
    let all_day = e["isAllDay"].as_bool().unwrap_or(false);

    // Graph returns {dateTime, timeZone} — normalize to UTC
    let start_tz = start_obj["timeZone"].as_str().unwrap_or("UTC");

    let start = if all_day {
        start_obj["dateTime"]
            .as_str()
            .unwrap_or("")
            .split('T')
            .next()
            .unwrap_or("")
            .to_string()
    } else {
        let dt = start_obj["dateTime"].as_str().unwrap_or("");
        crate::calendar::timezone::to_utc(dt, start_tz)
    };

    let end = if all_day {
        end_obj["dateTime"]
            .as_str()
            .unwrap_or("")
            .split('T')
            .next()
            .unwrap_or("")
            .to_string()
    } else {
        let dt = end_obj["dateTime"].as_str().unwrap_or("");
        let end_tz = end_obj["timeZone"].as_str().unwrap_or("UTC");
        crate::calendar::timezone::to_utc(dt, end_tz)
    };

    let timezone = if all_day {
        None
    } else {
        Some(start_tz.to_string())
    };

    let location = e["location"]["displayName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let organizer_email = e["organizer"]["emailAddress"]["address"]
        .as_str()
        .map(|s| s.to_string());

    let attendees_json = e["attendees"].as_array().map(|atts| {
        let parsed: Vec<serde_json::Value> = atts
            .iter()
            .map(|a| {
                serde_json::json!({
                    "name": a["emailAddress"]["name"].as_str().unwrap_or(""),
                    "email": a["emailAddress"]["address"].as_str().unwrap_or(""),
                    "status": graph_response_to_partstat(
                        a["status"]["response"].as_str().unwrap_or("none"),
                    ),
                })
            })
            .collect();
        serde_json::to_string(&parsed).unwrap_or_else(|_| "[]".to_string())
    });

    GraphCalendarEvent {
        id: e["id"].as_str().unwrap_or("").to_string(),
        subject: e["subject"].as_str().unwrap_or("(No title)").to_string(),
        body_preview: e["bodyPreview"].as_str().map(|s| s.to_string()),
        start,
        end,
        all_day,
        timezone,
        location,
        organizer_email,
        attendees_json,
        my_status: graph_response_to_my_status(
            e["responseStatus"]["response"].as_str().unwrap_or("none"),
        ),
        ical_uid: e["iCalUId"].as_str().map(|s| s.to_string()),
    }
}

fn graph_contact_string<'a>(
    contact: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Option<&'a str>> {
    match contact.get(field) {
        None => Err(Error::Other(format!(
            "Graph contact field `{field}` is missing"
        ))),
        Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(Error::Other(format!(
            "Graph contact field `{field}` must be a string or null"
        ))),
    }
}

fn graph_contact_array<'a>(
    contact: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<&'a [serde_json::Value]> {
    match contact.get(field) {
        None => Err(Error::Other(format!(
            "Graph contact field `{field}` is missing"
        ))),
        Some(serde_json::Value::Null) => Ok(&[]),
        Some(serde_json::Value::Array(values)) => Ok(values),
        Some(_) => Err(Error::Other(format!(
            "Graph contact field `{field}` must be an array or null"
        ))),
    }
}

fn parse_graph_contact(c: &serde_json::Value) -> Result<GraphContact> {
    let contact = c
        .as_object()
        .ok_or_else(|| Error::Other("Graph contact item must be an object".into()))?;
    let id = match contact.get("id") {
        None => {
            return Err(Error::Other("Graph contact field `id` is missing".into()));
        }
        Some(serde_json::Value::String(id)) if !id.trim().is_empty() => id.clone(),
        _ => {
            return Err(Error::Other(
                "Graph contact field `id` must be a non-empty string".into(),
            ));
        }
    };
    let display_name = graph_contact_string(contact, "displayName")?
        .unwrap_or("")
        .to_string();
    let organization = graph_contact_string(contact, "companyName")?
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let title = graph_contact_string(contact, "jobTitle")?
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    // Parse emails — Graph's "name" field is a display label, not work/home.
    // Use index-based labeling: first = "work", rest = "other".
    let mut emails = Vec::new();
    for (index, entry) in graph_contact_array(contact, "emailAddresses")?
        .iter()
        .enumerate()
    {
        let email = entry.as_object().ok_or_else(|| {
            Error::Other(format!(
                "Graph contact field `emailAddresses[{index}]` must be an object"
            ))
        })?;
        let address = email
            .get("address")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                Error::Other(format!(
                    "Graph contact field `emailAddresses[{index}].address` must be a string"
                ))
            })?;
        let name = match email.get("name") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(name)) => Some(name),
            Some(_) => {
                return Err(Error::Other(format!(
                    "Graph contact field `emailAddresses[{index}].name` must be a string or null"
                )));
            }
        };
        if !address.is_empty() {
            let label = if index == 0 { "work" } else { "other" };
            let mut local_email = serde_json::json!({"email": address, "label": label});
            if let Some(name) = name {
                local_email["name"] = serde_json::json!(name);
            }
            emails.push(local_email);
        }
    }
    let emails_json = serde_json::to_string(&emails)
        .map_err(|error| Error::Other(format!("Graph contact email encoding failed: {error}")))?;

    // Parse phones: Graph has mobilePhone (string), businessPhones (array), homePhones (array)
    let mut phones: Vec<serde_json::Value> = Vec::new();
    if let Some(mobile) = graph_contact_string(contact, "mobilePhone")?.filter(|s| !s.is_empty()) {
        phones.push(serde_json::json!({"number": mobile, "label": "mobile"}));
    }
    for (field, label) in [("businessPhones", "work"), ("homePhones", "home")] {
        for (index, entry) in graph_contact_array(contact, field)?.iter().enumerate() {
            let number = entry.as_str().ok_or_else(|| {
                Error::Other(format!(
                    "Graph contact field `{field}[{index}]` must be a string"
                ))
            })?;
            if !number.is_empty() {
                phones.push(serde_json::json!({"number": number, "label": label}));
            }
        }
    }
    let phones_json = serde_json::to_string(&phones)
        .map_err(|error| Error::Other(format!("Graph contact phone encoding failed: {error}")))?;

    Ok(GraphContact {
        id,
        display_name,
        emails_json,
        phones_json,
        organization,
        title,
    })
}

/// Anchor hexes for the Microsoft `calendarColor` enum. Picked to
/// match the app's UI palette in `random_calendar_color()` so that
/// (a) freshly-synced Graph calendars get a hex that the picker
/// already shows on its swatch row, and (b) `nearest_outlook_color`
/// (the inverse direction) round-trips exactly when the user picks
/// a palette colour. `maxColor` is intentionally absent — it's a
/// sentinel ordinal that Graph rejects with 500 ISE on PATCH.
const GRAPH_COLOR_ANCHORS: &[(&str, &str, (i32, i32, i32))] = &[
    ("lightBlue", "#4285f4", (0x42, 0x85, 0xf4)),
    ("lightGreen", "#0b8043", (0x0b, 0x80, 0x43)),
    ("lightOrange", "#f4511e", (0xf4, 0x51, 0x1e)),
    ("lightGray", "#616161", (0x61, 0x61, 0x61)),
    ("lightYellow", "#f6bf26", (0xf6, 0xbf, 0x26)),
    ("lightTeal", "#33b679", (0x33, 0xb6, 0x79)),
    ("lightPink", "#e67c73", (0xe6, 0x7c, 0x73)),
    ("lightBrown", "#8e24aa", (0x8e, 0x24, 0xaa)),
    ("lightRed", "#d50000", (0xd5, 0x00, 0x00)),
];

fn graph_color_to_hex(color: &str) -> String {
    if color == "auto" {
        return "#4285f4".to_string();
    }
    GRAPH_COLOR_ANCHORS
        .iter()
        .find(|(name, _, _)| *name == color)
        .map(|(_, hex, _)| (*hex).to_string())
        .unwrap_or_else(|| "#4285f4".to_string())
}

/// Pick the closest Microsoft `calendarColor` enum name for a given
/// CSS hex. Anchor hexes match the UI palette so a round-trip
/// through Graph keeps colors recognisable. Plain Euclidean
/// distance in RGB is "good enough" for a 9-bin nearest-neighbour
/// over a small palette.
fn nearest_outlook_color(hex: &str) -> &'static str {
    fn parse_hex(s: &str) -> Option<(i32, i32, i32)> {
        let h = s.trim().trim_start_matches('#');
        if h.len() != 6 {
            return None;
        }
        Some((
            i32::from_str_radix(&h[0..2], 16).ok()?,
            i32::from_str_radix(&h[2..4], 16).ok()?,
            i32::from_str_radix(&h[4..6], 16).ok()?,
        ))
    }
    let Some((r, g, b)) = parse_hex(hex) else {
        return "auto";
    };
    let mut best = GRAPH_COLOR_ANCHORS[0].0;
    let mut best_d = i32::MAX;
    for (name, _, (ar, ag, ab)) in GRAPH_COLOR_ANCHORS {
        let dr = r - ar;
        let dg = g - ag;
        let db = b - ab;
        let d = dr * dr + dg * dg + db * db;
        if d < best_d {
            best_d = d;
            best = name;
        }
    }
    best
}

// ---------------------------------------------------------------------------
// Payload builders (pure — unit-tested below)
// ---------------------------------------------------------------------------

/// Graph start/end object. All-day events must be midnight-anchored
/// (`isAllDay` requires `T00:00:00` boundaries).
fn graph_time_json(timestamp: &str, all_day: bool) -> serde_json::Value {
    if all_day {
        serde_json::json!({
            "dateTime": format!("{}T00:00:00", timestamp.split('T').next().unwrap_or_default()),
            "timeZone": "UTC",
        })
    } else {
        serde_json::json!({"dateTime": timestamp, "timeZone": "UTC"})
    }
}

/// Graph payload for creating an event. Includes the attendee list plus
/// the organizer as an attendee with `response: organizer` — Exchange
/// needs it to render the organizer row correctly.
pub fn event_to_graph_json(event: &crate::calendar::CalendarEvent) -> serde_json::Value {
    let mut graph_event = serde_json::json!({
        "subject": event.title,
        "start": graph_time_json(&event.start_time, event.all_day),
        "end": graph_time_json(&event.end_time, event.all_day),
        "isAllDay": event.all_day,
    });
    if let Some(ref desc) = event.description {
        graph_event["body"] = serde_json::json!({"contentType": "text", "content": desc});
    }
    if let Some(ref loc) = event.location {
        graph_event["location"] = serde_json::json!({"displayName": loc});
    }
    if let Some(ref att_json) = event.attendees_json {
        if let Ok(atts) = serde_json::from_str::<Vec<serde_json::Value>>(att_json) {
            let mut graph_atts: Vec<serde_json::Value> = atts
                .iter()
                .filter_map(|a| {
                    a["email"].as_str().map(|e| {
                        serde_json::json!({
                            "emailAddress": {"address": e, "name": a["name"].as_str().unwrap_or("")},
                            "type": "required",
                        })
                    })
                })
                .collect();
            // Add the organizer as an attendee with isOrganizer=true
            if let Some(ref org_email) = event.organizer_email {
                graph_atts.push(serde_json::json!({
                    "emailAddress": {"address": org_email, "name": ""},
                    "type": "required",
                    "status": {"response": "organizer"},
                }));
            }
            if !graph_atts.is_empty() {
                graph_event["attendees"] = serde_json::json!(graph_atts);
            }
        }
    }
    graph_event
}

/// Graph payload for patching an event. Narrower than the create
/// payload: raw timestamps (no all-day midnight anchoring) and no
/// attendee rewrite, matching what update_event has always sent.
pub fn event_patch_to_graph_json(event: &crate::calendar::CalendarEvent) -> serde_json::Value {
    let mut patch = serde_json::json!({
        "subject": event.title,
        "start": {"dateTime": event.start_time, "timeZone": "UTC"},
        "end": {"dateTime": event.end_time, "timeZone": "UTC"},
        "isAllDay": event.all_day,
    });
    if let Some(ref desc) = event.description {
        patch["body"] = serde_json::json!({"contentType": "text", "content": desc});
    }
    if let Some(ref loc) = event.location {
        patch["location"] = serde_json::json!({"displayName": loc});
    }
    patch
}

/// Graph `contact` payload from our contact fields. Phones split into
/// `mobilePhone` (first mobile-labelled number), `businessPhones`, and
/// `homePhones` because that is how Outlook models them. Every owned field is
/// emitted so an update can explicitly clear remote values.
pub fn contact_to_graph_json(
    display_name: &str,
    emails_json: &str,
    phones_json: &str,
    organization: Option<&str>,
    title: Option<&str>,
) -> Result<serde_json::Value> {
    let emails: Vec<serde_json::Value> = serde_json::from_str(emails_json)
        .map_err(|error| Error::Other(format!("Invalid local contact emails_json: {error}")))?;
    let mut graph_emails = Vec::with_capacity(emails.len());
    for (index, entry) in emails.iter().enumerate() {
        let email = entry.as_object().ok_or_else(|| {
            Error::Other(format!(
                "Invalid local contact emails_json entry {index}: expected an object"
            ))
        })?;
        let address = email
            .get("email")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                Error::Other(format!(
                    "Invalid local contact emails_json entry {index}: `email` must be a string"
                ))
            })?;
        let mut graph_email = serde_json::json!({"address": address});
        match email.get("name") {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::String(name)) => {
                graph_email["name"] = serde_json::json!(name);
            }
            Some(_) => {
                return Err(Error::Other(format!(
                    "Invalid local contact emails_json entry {index}: `name` must be a string or null"
                )));
            }
        }
        graph_emails.push(graph_email);
    }

    let phones: Vec<serde_json::Value> = serde_json::from_str(phones_json)
        .map_err(|error| Error::Other(format!("Invalid local contact phones_json: {error}")))?;
    let mut mobile_phone = None;
    let mut business_phones = Vec::new();
    let mut home_phones = Vec::new();
    for (index, entry) in phones.iter().enumerate() {
        let phone = entry.as_object().ok_or_else(|| {
            Error::Other(format!(
                "Invalid local contact phones_json entry {index}: expected an object"
            ))
        })?;
        let number = phone
            .get("number")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                Error::Other(format!(
                    "Invalid local contact phones_json entry {index}: `number` must be a string"
                ))
            })?;
        let label = match phone.get("label") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(label)) => Some(label.as_str()),
            Some(_) => {
                return Err(Error::Other(format!(
                    "Invalid local contact phones_json entry {index}: `label` must be a string or null"
                )));
            }
        };
        match label {
            Some("mobile") if mobile_phone.is_none() => mobile_phone = Some(number),
            Some("work") => business_phones.push(number),
            Some("home") => home_phones.push(number),
            _ => {}
        }
    }

    Ok(serde_json::json!({
        "displayName": display_name,
        "emailAddresses": graph_emails,
        "mobilePhone": mobile_phone,
        "businessPhones": business_phones,
        "homePhones": home_phones,
        "companyName": organization,
        "jobTitle": title,
    }))
}

#[cfg(test)]
mod batch_tests {
    use super::{
        apply_batch_responses, build_copy_batch_requests, is_delta_resync_required,
        is_item_not_found, DELTA_RESYNC_MARKER,
    };
    use crate::error::Error;

    fn fresh_results(n: usize) -> Vec<crate::error::Result<()>> {
        (0..n)
            .map(|_| Err(Error::Other("no $batch response for item".into())))
            .collect()
    }

    #[test]
    fn batch_responses_map_out_of_order_by_id() {
        let resp = serde_json::json!({
            "responses": [
                { "id": "1", "status": 204 },
                { "id": "0", "status": 201 },
            ]
        });
        let mut results = fresh_results(2);
        apply_batch_responses(&resp, &mut results, true);
        assert!(results[0].is_ok());
        assert!(results[1].is_ok());
    }

    #[test]
    fn batch_responses_keep_item_errors_detectable() {
        let resp = serde_json::json!({
            "responses": [
                { "id": "0", "status": 201 },
                {
                    "id": "1",
                    "status": 404,
                    "body": { "error": { "code": "ErrorItemNotFound" } }
                },
            ]
        });
        let mut results = fresh_results(2);
        apply_batch_responses(&resp, &mut results, true);
        assert!(results[0].is_ok());
        // The stale-id detection in commands::filters matches on "404" +
        // "ErrorItemNotFound" appearing in the error text.
        let err = results[1].as_ref().unwrap_err().to_string();
        assert!(err.contains("404"), "missing status in: {err}");
        assert!(err.contains("ErrorItemNotFound"), "missing code in: {err}");
        assert!(is_item_not_found(results[1].as_ref().unwrap_err()));
        assert!(!is_item_not_found(&Error::Other(
            "Graph $batch item returned 403: forbidden".into()
        )));
    }

    #[test]
    fn batch_responses_missing_item_stays_err() {
        let resp = serde_json::json!({ "responses": [ { "id": "0", "status": 200 } ] });
        let mut results = fresh_results(2);
        apply_batch_responses(&resp, &mut results, true);
        assert!(results[0].is_ok());
        assert!(
            results[1].is_err(),
            "unanswered items must not report success"
        );
    }

    /// Throttled sub-responses are returned for retry with their per-item
    /// `Retry-After` (header casing varies), not recorded as final errors.
    #[test]
    fn batch_responses_report_throttled_items_for_retry() {
        let resp = serde_json::json!({
            "responses": [
                { "id": "0", "status": 201 },
                {
                    "id": "1",
                    "status": 429,
                    "headers": { "Retry-After": "17" },
                    "body": { "error": { "code": "ApplicationThrottled" } }
                },
                { "id": "2", "status": 503, "headers": { "retry-after": "3" } },
            ]
        });
        let mut results = fresh_results(3);
        let retryable = apply_batch_responses(&resp, &mut results, true);
        assert!(results[0].is_ok());
        // Throttled items keep their placeholder (not a final outcome yet).
        assert!(results[1].is_err());
        assert!(results[2].is_err());
        assert_eq!(retryable, vec![(1, 17), (2, 3)]);
    }

    #[test]
    fn copy_batch_requests_use_copy_endpoint_and_destination() {
        let requests = build_copy_batch_requests(&["message_1".into()], "archive_1");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["method"], "POST");
        assert_eq!(requests[0]["url"], "/me/messages/message_1/copy");
        assert_eq!(requests[0]["body"]["destinationId"], "archive_1");
    }

    #[test]
    fn copy_batch_does_not_retry_ambiguous_transient_errors() {
        let resp = serde_json::json!({
            "responses": [
                { "id": "0", "status": 429, "headers": { "Retry-After": "2" } },
                { "id": "1", "status": 503 },
                { "id": "2", "status": 504 },
            ]
        });
        let mut results = fresh_results(3);
        let retryable = apply_batch_responses(&resp, &mut results, false);
        assert_eq!(retryable, vec![(0, 2)]);
        assert!(results[1].as_ref().unwrap_err().to_string().contains("503"));
        assert!(results[2].as_ref().unwrap_err().to_string().contains("504"));
    }

    /// Regression: delta events must keep server order. An update followed
    /// by an @removed tombstone for the same id must yield Upsert then
    /// Removed — applying removals first would resurrect the message.
    #[test]
    fn delta_page_preserves_event_order() {
        use super::{parse_delta_page, GraphDeltaEvent};
        let resp = serde_json::json!({
            "value": [
                { "id": "m1", "subject": "updated then deleted", "isRead": true },
                { "id": "m2", "subject": "still alive" },
                { "id": "m1", "@removed": { "reason": "deleted" } },
            ],
            "@odata.deltaLink": "https://graph.microsoft.com/v1.0/delta?$deltatoken=t1"
        });
        let page = parse_delta_page(&resp);
        assert_eq!(page.events.len(), 3);
        assert!(
            matches!(&page.events[0], GraphDeltaEvent::Upsert(m) if m.id == "m1"),
            "first event must be the m1 upsert"
        );
        assert!(matches!(&page.events[1], GraphDeltaEvent::Upsert(m) if m.id == "m2"));
        assert!(
            matches!(&page.events[2], GraphDeltaEvent::Removed(id) if id == "m1"),
            "the m1 removal must come after its upsert"
        );
        assert!(page.next_link.is_none());
        assert_eq!(
            page.delta_link.as_deref(),
            Some("https://graph.microsoft.com/v1.0/delta?$deltatoken=t1")
        );
    }

    #[test]
    fn delta_resync_marker_is_detected() {
        let err = Error::Other(format!("{DELTA_RESYNC_MARKER} for folder abc"));
        assert!(is_delta_resync_required(&err));
        assert!(!is_delta_resync_required(&Error::Other(
            "Graph GET x returned 429".into()
        )));
    }
}

#[cfg(test)]
mod color_tests {
    use super::{
        dedupe_graph_rooms, graph_color_to_hex, graph_response_to_my_status,
        graph_response_to_partstat, looks_like_smtp_address, nearest_outlook_color,
        normalize_schedule_datetime, parse_graph_event, parse_graph_named_addresses,
        parse_graph_room_availability, parse_graph_rooms, profile_email_from_me, GraphRoom,
    };

    /// Regression: `get_me`'s mailbox-address heuristic must reject a
    /// legacy Exchange X.500 / "EX" distinguished name. Graph returns
    /// one as the Sent `from` address for Exchange Online mailboxes;
    /// without this guard it was shown to the user as their email
    /// address, replacing the correct SMTP address from `/me`.
    #[test]
    fn ex_distinguished_name_is_not_an_smtp_address() {
        let ex_dn = "/O=EXCHANGELABS/OU=EXCHANGE ADMINISTRATIVE GROUP \
                     (FYDIBOHF23SPDLT)/CN=RECIPIENTS/CN=abc123";
        assert!(!looks_like_smtp_address(ex_dn));
        // Real SMTP addresses still pass.
        assert!(looks_like_smtp_address("chithiapp@outlook.com"));
        assert!(looks_like_smtp_address("kushal.das@example.co.uk"));
        assert!(looks_like_smtp_address(" kushal.das@example.co.uk "));
        // Degenerate inputs are rejected.
        assert!(!looks_like_smtp_address(""));
        assert!(!looks_like_smtp_address("noatsign"));
        assert!(!looks_like_smtp_address("@outlook.com"));
        assert!(!looks_like_smtp_address("user@localhost"));
    }

    #[test]
    fn profile_email_from_me_falls_back_from_ex_dn_to_upn() {
        let ex_dn = "/O=EXCHANGELABS/OU=EXCHANGE ADMINISTRATIVE GROUP \
                     (FYDIBOHF23SPDLT)/CN=RECIPIENTS/CN=abc123";
        assert_eq!(
            profile_email_from_me(Some(ex_dn), Some("kano@sunet.se")),
            "kano@sunet.se"
        );
        assert_eq!(
            profile_email_from_me(Some("alias@sunet.se"), Some("login@sunet.se")),
            "alias@sunet.se"
        );
        assert_eq!(
            profile_email_from_me(None, Some("login@sunet.se")),
            "login@sunet.se"
        );
        assert_eq!(
            profile_email_from_me(None, Some("non-email-upn")),
            "non-email-upn"
        );
        assert_eq!(
            profile_email_from_me(Some("  "), Some("non-email-upn")),
            "non-email-upn"
        );
    }

    // Graph emits its own responseType enum; the UI only understands iCal
    // PARTSTAT values. Storing Graph's vocabulary verbatim made the event
    // popup show "tentativelyAccepted" / "notResponded" next to attendees.
    #[test]
    fn graph_response_types_map_to_partstat() {
        assert_eq!(graph_response_to_partstat("accepted"), "accepted");
        assert_eq!(
            graph_response_to_partstat("tentativelyAccepted"),
            "tentative"
        );
        assert_eq!(graph_response_to_partstat("declined"), "declined");
        assert_eq!(graph_response_to_partstat("organizer"), "accepted");
        assert_eq!(graph_response_to_partstat("none"), "needs-action");
        assert_eq!(graph_response_to_partstat("notResponded"), "needs-action");
        // Unknown / future values fall back safely.
        assert_eq!(graph_response_to_partstat("somethingNew"), "needs-action");
    }

    // A synced Graph event's attendee list must carry translated statuses.
    #[test]
    fn parse_graph_event_translates_attendee_status() {
        let raw = serde_json::json!({
            "id": "evt1",
            "subject": "Linux docs",
            "start": { "dateTime": "2026-05-19T11:00:00.0000000", "timeZone": "UTC" },
            "end": { "dateTime": "2026-05-19T11:30:00.0000000", "timeZone": "UTC" },
            "isAllDay": false,
            "organizer": { "emailAddress": { "address": "chithiapp@outlook.com" } },
            "attendees": [
                {
                    "emailAddress": { "name": "Kushal", "address": "kushal@sunet.se" },
                    "status": { "response": "tentativelyAccepted" }
                }
            ]
        });
        let parsed = parse_graph_event(&raw);
        let atts: serde_json::Value =
            serde_json::from_str(&parsed.attendees_json.unwrap()).unwrap();
        assert_eq!(atts[0]["status"], "tentative");
    }

    // The signed-in user's own RSVP comes from the event-level
    // responseStatus; only a genuine RSVP yields a my_status badge.
    #[test]
    fn graph_response_maps_to_my_status() {
        assert_eq!(
            graph_response_to_my_status("tentativelyAccepted").as_deref(),
            Some("tentative")
        );
        assert_eq!(
            graph_response_to_my_status("accepted").as_deref(),
            Some("accepted")
        );
        assert_eq!(
            graph_response_to_my_status("declined").as_deref(),
            Some("declined")
        );
        // No RSVP concept — the user's own events / unanswered invites.
        assert_eq!(graph_response_to_my_status("organizer"), None);
        assert_eq!(graph_response_to_my_status("none"), None);
        assert_eq!(graph_response_to_my_status("notResponded"), None);
    }

    // A synced Graph event carries the user's RSVP from responseStatus.
    #[test]
    fn parse_graph_event_extracts_my_status() {
        let raw = serde_json::json!({
            "id": "evt2",
            "subject": "Linux docs",
            "start": { "dateTime": "2026-05-19T11:00:00.0000000", "timeZone": "UTC" },
            "end": { "dateTime": "2026-05-19T11:30:00.0000000", "timeZone": "UTC" },
            "isAllDay": false,
            "responseStatus": { "response": "tentativelyAccepted" }
        });
        assert_eq!(
            parse_graph_event(&raw).my_status.as_deref(),
            Some("tentative")
        );

        // An event the user organized has no RSVP badge.
        let own = serde_json::json!({
            "id": "evt3",
            "subject": "My own event",
            "start": { "dateTime": "2026-05-19T11:00:00.0000000", "timeZone": "UTC" },
            "end": { "dateTime": "2026-05-19T11:30:00.0000000", "timeZone": "UTC" },
            "isAllDay": false,
            "responseStatus": { "response": "organizer" }
        });
        assert_eq!(parse_graph_event(&own).my_status, None);
    }

    #[test]
    fn anchor_round_trip() {
        // Each UI-palette anchor must map to its expected enum name
        // *and* enum-name → hex must round-trip back. This is the
        // contract that keeps colors recognisable when the user
        // picks one of the 10 swatches and it survives a Graph
        // resync.
        let cases = [
            ("#4285f4", "lightBlue"),
            ("#0b8043", "lightGreen"),
            ("#f4511e", "lightOrange"),
            ("#616161", "lightGray"),
            ("#f6bf26", "lightYellow"),
            ("#33b679", "lightTeal"),
            ("#e67c73", "lightPink"),
            ("#8e24aa", "lightBrown"),
            ("#d50000", "lightRed"),
        ];
        for (hex, name) in cases {
            assert_eq!(nearest_outlook_color(hex), name, "hex {} -> name", hex);
            assert_eq!(graph_color_to_hex(name), hex, "name {} -> hex", name);
        }
    }

    #[test]
    fn off_palette_picks_a_neighbour() {
        // Pure red should pick lightRed (the closest anchor).
        assert_eq!(nearest_outlook_color("#ff0000"), "lightRed");
        // Pure white falls equidistant-ish but never panics.
        let _ = nearest_outlook_color("#ffffff");
    }

    #[test]
    fn invalid_hex_returns_auto() {
        assert_eq!(nearest_outlook_color("not-a-color"), "auto");
        assert_eq!(nearest_outlook_color("#abc"), "auto");
        assert_eq!(nearest_outlook_color(""), "auto");
    }

    #[test]
    fn maxcolor_is_not_a_target() {
        // Graph rejects PATCH with `color: maxColor` (500 ISE in
        // practice — Microsoft documents it as a sentinel ordinal).
        // No input hex should produce it.
        for &(_, hex, _) in super::GRAPH_COLOR_ANCHORS {
            assert_ne!(nearest_outlook_color(hex), "maxColor");
        }
        assert_ne!(nearest_outlook_color("#000000"), "maxColor");
        assert_ne!(nearest_outlook_color("#ffffff"), "maxColor");
        assert_ne!(nearest_outlook_color("#8b5cf6"), "maxColor");
    }

    #[test]
    fn parse_graph_rooms_normalizes_name_and_address() {
        let value = serde_json::json!([
            {"name": "Board Room", "address": "board@example.com"},
            {"name": "", "address": "fallback@example.com"},
            {"name": "Ignored", "address": ""}
        ]);

        let rooms = parse_graph_rooms(&value);
        assert_eq!(rooms.len(), 2);
        assert_eq!(rooms[0].name, "Board Room");
        assert_eq!(rooms[0].address, "board@example.com");
        assert_eq!(rooms[1].name, "fallback@example.com");
        assert_eq!(rooms[1].address, "fallback@example.com");
    }

    #[test]
    fn parse_graph_named_addresses_filters_blank_addresses() {
        let value = serde_json::json!([
            {"name": "Building 1 Rooms", "address": "building1@example.com"},
            {"name": "", "address": "fallback@example.com"},
            {"name": "Ignored", "address": ""}
        ]);

        let items = parse_graph_named_addresses(&value);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0],
            (
                "Building 1 Rooms".to_string(),
                "building1@example.com".to_string()
            )
        );
        assert_eq!(
            items[1],
            (
                "fallback@example.com".to_string(),
                "fallback@example.com".to_string()
            )
        );
    }

    #[test]
    fn parse_graph_named_addresses_supports_places_payload() {
        let value = serde_json::json!([
            {"displayName": "Building 1 Rooms", "emailAddress": "building1@example.com"},
            {"displayName": "", "emailAddress": "fallback@example.com"},
            {"displayName": "Ignored", "emailAddress": ""}
        ]);

        let items = parse_graph_named_addresses(&value);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0],
            (
                "Building 1 Rooms".to_string(),
                "building1@example.com".to_string()
            )
        );
        assert_eq!(
            items[1],
            (
                "fallback@example.com".to_string(),
                "fallback@example.com".to_string()
            )
        );
    }

    #[test]
    fn dedupe_graph_rooms_keeps_unique_addresses() {
        let rooms = vec![
            GraphRoom {
                name: "Zebra".into(),
                address: "room@example.com".into(),
            },
            GraphRoom {
                name: "Alpha".into(),
                address: "ROOM@example.com".into(),
            },
            GraphRoom {
                name: "Beta".into(),
                address: "beta@example.com".into(),
            },
        ];

        let unique = dedupe_graph_rooms(rooms);
        assert_eq!(unique.len(), 2);
        assert_eq!(unique[0].name, "Beta");
        assert_eq!(unique[1].address, "room@example.com");
    }

    #[test]
    fn normalize_schedule_datetime_converts_to_utc_naive_format() {
        assert_eq!(
            normalize_schedule_datetime("2026-05-19T10:00:00+02:00").unwrap(),
            "2026-05-19T08:00:00"
        );
    }

    #[test]
    fn parse_graph_room_availability_detects_busy_slots() {
        let value = serde_json::json!({
            "value": [
                {
                    "scheduleItems": [
                        {
                            "status": "free",
                            "start": { "dateTime": "2026-05-19T09:00:00.0000000" },
                            "end": { "dateTime": "2026-05-19T10:00:00.0000000" }
                        },
                        {
                            "status": "busy",
                            "start": { "dateTime": "2026-05-19T10:30:00.0000000" },
                            "end": { "dateTime": "2026-05-19T11:00:00.0000000" }
                        }
                    ]
                }
            ]
        });

        let availability = parse_graph_room_availability(&value);
        assert_eq!(availability.state, "busy");
        assert_eq!(
            availability.busy_start.as_deref(),
            Some("2026-05-19T10:30:00.0000000")
        );
        assert_eq!(
            availability.busy_end.as_deref(),
            Some("2026-05-19T11:00:00.0000000")
        );
    }

    #[test]
    fn parse_graph_room_availability_defaults_to_available_without_blocks() {
        let value = serde_json::json!({
            "value": [
                {
                    "scheduleItems": [
                        {
                            "status": "free",
                            "start": { "dateTime": "2026-05-19T09:00:00.0000000" },
                            "end": { "dateTime": "2026-05-19T10:00:00.0000000" }
                        }
                    ]
                }
            ]
        });

        let availability = parse_graph_room_availability(&value);
        assert_eq!(availability.state, "available");
        assert!(availability.busy_start.is_none());
        assert!(availability.busy_end.is_none());
    }

    #[test]
    fn parse_graph_room_availability_reports_unknown_on_schedule_error() {
        // A per-recipient `error` (e.g. mailbox not found, free/busy
        // not published) must surface as "unknown" — never "available",
        // which would tell the user a room is free when Graph failed.
        let value = serde_json::json!({
            "value": [
                {
                    "scheduleId": "board@example.com",
                    "error": {
                        "message": "ErrorMailRecipientNotFound",
                        "responseCode": "MailRecipientNotFound"
                    }
                }
            ]
        });

        let availability = parse_graph_room_availability(&value);
        assert_eq!(availability.state, "unknown");
        assert!(availability.busy_start.is_none());
        assert!(availability.busy_end.is_none());
    }

    #[test]
    fn parse_graph_room_availability_reports_unknown_without_free_busy_data() {
        // A schedule entry carrying neither scheduleItems nor an
        // availabilityView gives us nothing to judge on — "unknown".
        let value = serde_json::json!({
            "value": [
                { "scheduleId": "board@example.com" }
            ]
        });

        let availability = parse_graph_room_availability(&value);
        assert_eq!(availability.state, "unknown");
    }
}

#[cfg(test)]
mod builder_tests {
    use super::{
        contact_to_graph_json, event_patch_to_graph_json, event_to_graph_json, parse_graph_contact,
    };
    use crate::calendar::CalendarEvent;

    fn event(all_day: bool, attendees_json: Option<&str>) -> CalendarEvent {
        CalendarEvent {
            id: "local-id".into(),
            account_id: "acct".into(),
            calendar_id: "cal".into(),
            uid: Some("uid-1@chithi".into()),
            title: "Standup".into(),
            description: None,
            location: Some("Room 1".into()),
            start_time: "2026-07-14T09:00:00Z".into(),
            end_time: "2026-07-14T09:15:00Z".into(),
            all_day,
            timezone: None,
            recurrence_rule: None,
            organizer_email: Some("me@example.org".into()),
            attendees_json: attendees_json.map(|s| s.to_string()),
            my_status: None,
            source_message_id: None,
            ical_data: None,
            remote_id: None,
            etag: None,
        }
    }

    #[test]
    fn all_day_event_is_midnight_anchored() {
        let v = event_to_graph_json(&event(true, None));
        assert_eq!(v["start"]["dateTime"], "2026-07-14T00:00:00");
        assert_eq!(v["isAllDay"], true);
    }

    #[test]
    fn create_appends_organizer_as_attendee() {
        let v = event_to_graph_json(&event(false, Some(r#"[{"email":"a@x.org","name":"A"}]"#)));
        let atts = v["attendees"].as_array().unwrap();
        assert_eq!(atts.len(), 2);
        assert_eq!(atts[0]["emailAddress"]["address"], "a@x.org");
        assert_eq!(atts[1]["emailAddress"]["address"], "me@example.org");
        assert_eq!(atts[1]["status"]["response"], "organizer");
    }

    #[test]
    fn patch_keeps_raw_times_and_no_attendees() {
        let v = event_patch_to_graph_json(&event(true, Some(r#"[{"email":"a@x.org"}]"#)));
        // The patch path has never midnight-anchored all-day times.
        assert_eq!(v["start"]["dateTime"], "2026-07-14T09:00:00Z");
        assert!(v["attendees"].is_null());
    }

    #[test]
    fn contact_splits_mobile_and_business_phones() {
        let v = contact_to_graph_json(
            "Ada",
            r#"[{"email":"ada@x.org"}]"#,
            r#"[{"number":"+4670","label":"mobile"},{"number":"+4608","label":"work"}]"#,
            Some("Analytical Engines"),
            None,
        )
        .unwrap();
        assert_eq!(v["mobilePhone"], "+4670");
        assert_eq!(v["businessPhones"][0], "+4608");
        assert_eq!(v["companyName"], "Analytical Engines");
        assert_eq!(v["emailAddresses"][0]["address"], "ada@x.org");
        assert!(v["jobTitle"].is_null());
    }

    #[test]
    fn contact_handles_malformed_json() {
        for (emails, phones, field) in [
            ("not json", "[]", "emails_json"),
            ("[]", "not json", "phones_json"),
            (r#"[{"email":42}]"#, "[]", "emails_json"),
            (
                r#"[{"email":"x@example.org","name":42}]"#,
                "[]",
                "emails_json",
            ),
            ("[]", r#"[{"number":42,"label":"work"}]"#, "phones_json"),
        ] {
            let error = contact_to_graph_json("X", emails, phones, None, None).unwrap_err();
            assert!(
                error.to_string().contains(field),
                "unexpected error for {field}: {error}"
            );
        }
    }

    #[test]
    fn contact_includes_explicit_clears() {
        let value = contact_to_graph_json("X", "[]", "[]", None, None).unwrap();

        assert_eq!(
            value,
            serde_json::json!({
                "displayName": "X",
                "emailAddresses": [],
                "mobilePhone": null,
                "businessPhones": [],
                "homePhones": [],
                "companyName": null,
                "jobTitle": null,
            })
        );
    }

    #[test]
    fn contact_emits_home_phones() {
        let value = contact_to_graph_json(
            "X",
            "[]",
            r#"[{"number":"+4611","label":"home"},{"number":"+4622","label":"home"}]"#,
            None,
            None,
        )
        .unwrap();

        assert_eq!(value["homePhones"], serde_json::json!(["+4611", "+4622"]));
    }

    #[test]
    fn contact_email_name_round_trips() {
        let graph_contact = serde_json::json!({
            "id": "one",
            "displayName": "Ada",
            "emailAddresses": [
                {
                    "address": "ada@example.org",
                    "name": "Ada Lovelace",
                },
                {
                    "address": "ada.null@example.org",
                    "name": null,
                },
                {
                    "address": "ada.missing@example.org",
                },
            ],
            "mobilePhone": null,
            "businessPhones": [],
            "homePhones": [],
            "companyName": null,
            "jobTitle": null,
        });

        let local = parse_graph_contact(&graph_contact).unwrap();
        let local_emails: serde_json::Value = serde_json::from_str(&local.emails_json).unwrap();
        let rebuilt = contact_to_graph_json(
            &local.display_name,
            &local.emails_json,
            &local.phones_json,
            local.organization.as_deref(),
            local.title.as_deref(),
        )
        .unwrap();

        assert_eq!(local_emails[0]["name"], "Ada Lovelace");
        assert_eq!(rebuilt["emailAddresses"][0]["name"], "Ada Lovelace");
        for index in [1, 2] {
            assert!(local_emails[index].get("name").is_none());
            assert!(rebuilt["emailAddresses"][index].get("name").is_none());
        }
    }

    #[test]
    fn contact_omits_email_name_when_absent() {
        for emails_json in [
            r#"[{"email":"ada@example.org"}]"#,
            r#"[{"email":"ada@example.org","name":null}]"#,
        ] {
            let value = contact_to_graph_json("Ada", emails_json, "[]", None, None).unwrap();
            let email = value["emailAddresses"][0].as_object().unwrap();

            assert!(!email.contains_key("name"));
        }
    }
}

#[cfg(test)]
mod partial_file_tests {
    use super::PartialFileGuard;

    #[test]
    fn dropped_guard_removes_precreated_partial_download() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("body.partial");
        std::fs::write(&path, b"incomplete").unwrap();
        drop(PartialFileGuard::new(path.clone()));
        assert!(!path.exists());
    }

    #[test]
    fn committed_guard_keeps_download() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("body.eml");
        std::fs::write(&path, b"complete").unwrap();
        let mut guard = PartialFileGuard::new(path.clone());
        guard.commit();
        drop(guard);
        assert!(path.exists());
    }
}
