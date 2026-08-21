//! Mail backends: one implementor per mail protocol.
//!
//! Covers the provider entry points commands dispatch on (account sync,
//! per-folder sync, body prefetch, server search, draft save) plus capability
//! flags for the command-layer concurrency machinery (IDLE
//! suspension, background folder sync). Queued user operations
//! (move/copy/delete/flag/send) go through the ops worker; the
//! protocol-specific push/idle starters (`start_imap_idle`,
//! `start_jmap_push`) stay per-provider like meet auth flows.

use async_trait::async_trait;

use crate::account::MailAccountConfig;
use crate::db::pool::DbPool;
use crate::error::Result;
use crate::event::SharedEventSink;
use crate::message::{BackendMessageRef, BodyLocation, SearchHit, SearchQuery};
use crate::ops::queue::MailOp;
use crate::provider::{MailCredentials, ProviderServices};

pub mod graph;
pub mod imap;
pub mod jmap;

/// Everything a mail operation needs besides the account: application event
/// delivery, the DB pool, the Maildir root, and shared provider services.
#[derive(Clone)]
pub struct MailSyncCtx {
    pub events: SharedEventSink,
    pub db: std::sync::Arc<DbPool>,
    pub data_dir: std::path::PathBuf,
    pub providers: std::sync::Arc<ProviderServices>,
}

/// State retained after SMTP accepts a replayed message. Only the IMAP
/// executor consumes it for its best-effort APPEND-to-Sent hook.
struct RawSmtpDelivery {
    account: MailAccountConfig,
    credentials: MailCredentials,
    raw_message: Vec<u8>,
}

/// Replay a persisted raw send through the account's current SMTP settings.
///
/// The account and credentials are deliberately reloaded for each attempt so
/// password changes and Microsoft IMAP/SMTP-scoped OAuth tokens are current.
/// The persisted envelope and RFC 5322 bytes are passed to SMTP unchanged.
async fn replay_send_raw_via_smtp(
    ctx: &MailSyncCtx,
    account_id: &str,
    op: MailOp,
) -> Result<RawSmtpDelivery> {
    let MailOp::SendRaw {
        raw_message,
        from,
        to,
        cc,
        bcc,
        ..
    } = op
    else {
        return Err(crate::error::Error::Other(
            "SMTP replay received a non-SendRaw operation".into(),
        ));
    };

    let account = {
        let conn = ctx.db.reader();
        crate::db::accounts::get_account_full(&conn, account_id)?
    }
    .mail_config();
    let credentials = ctx
        .providers
        .credentials()
        .mail_credentials_for(&account)
        .await?;

    crate::mail::smtp::send_raw(
        &account.smtp_host,
        account.smtp_port,
        &account.username,
        &credentials.secret,
        account.use_tls,
        credentials.use_xoauth2,
        &from,
        &to,
        &cc,
        &bcc,
        &raw_message,
    )
    .await?;

    Ok(RawSmtpDelivery {
        account,
        credentials,
        raw_message,
    })
}

/// Provider-neutral inputs for fetching one raw RFC 822 body into Maildir.
#[derive(Debug, Clone)]
pub struct BodyFetchRequest {
    pub message_id: String,
    pub message_ref: BackendMessageRef,
    pub folder_path: String,
    pub flags: Vec<String>,
    pub body_location: BodyLocation,
}

/// Representation a provider accepts when creating a server-side draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftStorageFormat {
    /// The provider stores the complete RFC 5322 message verbatim.
    RawMime,
    /// The provider accepts separate envelope and plaintext-body fields.
    StructuredText,
}

/// Provider-neutral inputs for creating a server-side draft.
///
/// IMAP and JMAP consume `raw_message`. Graph consumes the structured fields
/// until its raw-MIME draft endpoint is implemented.
#[derive(Debug, Clone)]
pub struct DraftSaveRequest {
    pub raw_message: Vec<u8>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: String,
}

impl BodyFetchRequest {
    pub fn from_db_row(
        account: &MailAccountConfig,
        message_id: &str,
        folder_path: &str,
        uid: u32,
        flags: Vec<String>,
        body_location: BodyLocation,
    ) -> Result<Self> {
        let protocol = account.protocol.as_str();
        let message_ref =
            BackendMessageRef::from_db_row(protocol, &account.id, message_id, folder_path, uid)
                .or_else(|| {
                    // Preserve the legacy JMAP body-fetch fallback for rows whose
                    // synthetic database id does not carry the expected prefix.
                    (protocol == "jmap").then(|| BackendMessageRef::jmap(folder_path, message_id))
                })
                .ok_or_else(|| {
                    crate::error::Error::Other(format!(
                        "Cannot recover {} message reference from database id {}",
                        protocol, message_id
                    ))
                })?;

        Ok(Self {
            message_id: message_id.to_string(),
            message_ref,
            folder_path: folder_path.to_string(),
            flags,
            body_location,
        })
    }
}

#[async_trait]
pub trait MailBackend: Send + Sync {
    /// Protocol discriminator stored on the mail service binding
    /// (`service_bindings.protocol`).
    fn protocol(&self) -> &'static str;

    /// O365 over IMAP allows one connection per account at a time, so
    /// IDLE must be suspended around any other server operation. The
    /// command owns the suspend/resume because it owns the idle-handle
    /// state.
    fn suspends_idle_for_ops(&self, _account: &MailAccountConfig) -> bool {
        false
    }

    /// Full account sync, invoked directly by the sync command. Emits its own
    /// `sync-started` / `sync-complete` progress events (inside the provider
    /// sync loops); errors are emitted as `sync-error` by the command.
    async fn sync_account(
        &self,
        ctx: &MailSyncCtx,
        account: &MailAccountConfig,
        current_folder: Option<String>,
    ) -> Result<()>;

    /// Sync one folder's envelopes; returns the new-message count.
    /// This must touch ONLY the requested folder — a user right-clicking
    /// "sync" on a folder expects exactly that folder to sync and then
    /// the operation to stop (Graph used to background a whole-account
    /// sync here; delta sync made a true per-folder fetch possible).
    async fn sync_folder(
        &self,
        ctx: &MailSyncCtx,
        account: &MailAccountConfig,
        folder_path: &str,
    ) -> Result<u32>;

    /// Prefetch message bodies into the Maildir. Default no-op for
    /// providers that fetch bodies on demand (JMAP) or during sync.
    async fn prefetch_bodies(
        &self,
        _ctx: &MailSyncCtx,
        _account: &MailAccountConfig,
    ) -> Result<u32> {
        Ok(0)
    }

    /// Fetch one raw RFC 822 body and persist it under the Maildir root.
    /// Returns the relative path; the command records it in the database.
    async fn fetch_body_to_disk(
        &self,
        ctx: &MailSyncCtx,
        account: &MailAccountConfig,
        request: &BodyFetchRequest,
    ) -> Result<String>;

    /// Search messages across the account on the provider server.
    async fn search_messages(
        &self,
        ctx: &MailSyncCtx,
        account: &MailAccountConfig,
        query: &SearchQuery,
    ) -> Result<Vec<SearchHit>>;

    /// Describe the draft representation this provider can persist.
    fn draft_storage_format(&self) -> DraftStorageFormat;

    /// Create a server-side draft.
    async fn save_draft(
        &self,
        ctx: &MailSyncCtx,
        account: &MailAccountConfig,
        request: &DraftSaveRequest,
    ) -> Result<()>;

    /// Create the per-account executor the ops worker drives queued
    /// [`MailOp`]s through. Called once per worker lifetime.
    fn op_executor(&self) -> Box<dyn MailOpExecutor>;
}

/// Per-account executor for queued [`MailOp`]s (move/delete/flag/copy/
/// send), created via [`MailBackend::op_executor`]. Unlike the
/// stateless backends, an executor may hold connection state across
/// calls — the IMAP executor keeps a persistent connection with
/// staleness tracking and reconnect backoff.
#[async_trait]
pub trait MailOpExecutor: Send {
    /// Execute one queued operation. Sync and replay operations never reach
    /// this; the worker handles them directly.
    async fn execute(&mut self, ctx: &MailSyncCtx, account_id: &str, op: MailOp) -> Result<()>;

    /// Called once when the worker shuts down; close connections.
    async fn shutdown(&mut self) {}
}

/// Static set of mail backends compiled into this build.
pub fn registry() -> &'static [&'static dyn MailBackend] {
    &[
        &imap::ImapMailBackend,
        &jmap::JmapMailBackend,
        &graph::GraphMailBackend,
    ]
}

/// Find the backend for the account's enabled mail binding.
///
/// A non-empty protocol that matches nothing falls back to IMAP — the
/// pre-trait dispatch chains all ended in an IMAP else-branch, and
/// pre-binding accounts rely on it. Empty (no enabled mail binding)
/// returns `None`; callers skip those accounts.
pub fn for_account(account: &MailAccountConfig) -> Option<&'static dyn MailBackend> {
    let proto = account.protocol.as_str();
    if proto.is_empty() {
        return None;
    }
    Some(
        registry()
            .iter()
            .copied()
            .find(|b| b.protocol() == proto)
            .unwrap_or(&imap::ImapMailBackend),
    )
}

#[cfg(test)]
mod registry_tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;

    use super::*;
    use crate::db::accounts::AccountFull;
    use crate::db::pool::DbPool;
    use crate::db::service_bindings::ServiceBinding;
    use crate::error::Error;
    use crate::event::{ApplicationEvent, EventSink};
    use crate::message::BodyLocation;
    use crate::oauth::{OAuthProvider, OAuthTokens};
    use crate::ops::offline::{mail_op_to_outbox, outbox_to_mail_op, OutboxEntry};
    use crate::provider::{
        OAuthTokenStore, ProviderCredentialService, ProviderTransports, TokenEndpointClient,
    };

    struct CapturedHttpRequest {
        method: String,
        path: String,
        body: Vec<u8>,
    }

    async fn read_http_request(stream: &mut TcpStream) -> CapturedHttpRequest {
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        let header_end = loop {
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "request ended before headers were complete");
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
        };

        let (method, path, content_length) = {
            let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut request_line = headers.lines().next().unwrap().split_whitespace();
            (
                request_line.next().unwrap().to_string(),
                request_line.next().unwrap().to_string(),
                content_length,
            )
        };
        while bytes.len() < header_end + content_length {
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "request ended before its body was complete");
            bytes.extend_from_slice(&chunk[..read]);
        }

        CapturedHttpRequest {
            method,
            path,
            body: bytes[header_end..header_end + content_length].to_vec(),
        }
    }

    async fn write_json_response(stream: &mut TcpStream, body: serde_json::Value) {
        let body = body.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    fn loopback_http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap()
    }

    async fn jmap_submission_server() -> (String, oneshot::Receiver<Vec<CapturedHttpRequest>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server_base_url = base_url.clone();
        let (requests_tx, requests_rx) = oneshot::channel();

        tokio::spawn(async move {
            let mut requests = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut stream).await;
                let (response, is_final) = if request.method == "GET" {
                    (
                        serde_json::json!({
                            "apiUrl": format!("{server_base_url}/api"),
                            "downloadUrl": format!(
                                "{server_base_url}/download/{{accountId}}/{{blobId}}/{{name}}"
                            ),
                            "uploadUrl": format!("{server_base_url}/upload/{{accountId}}"),
                            "capabilities": {
                                "urn:ietf:params:jmap:core": {},
                                "urn:ietf:params:jmap:mail": {},
                                "urn:ietf:params:jmap:submission": {}
                            },
                            "accounts": {
                                "account-1": {
                                    "accountCapabilities": {
                                        "urn:ietf:params:jmap:mail": {},
                                        "urn:ietf:params:jmap:submission": {
                                            "maxDelayedSend": 0,
                                            "submissionExtensions": {}
                                        }
                                    }
                                }
                            },
                            "primaryAccounts": {
                                "urn:ietf:params:jmap:mail": "account-1"
                            }
                        }),
                        false,
                    )
                } else if request.path == "/upload/account-1" {
                    (serde_json::json!({ "blobId": "blob-1" }), false)
                } else {
                    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                    match body["methodCalls"][0][0].as_str().unwrap() {
                        "Mailbox/get" => (
                            serde_json::json!({
                                "methodResponses": [["Mailbox/get", {
                                    "accountId": "account-1",
                                    "state": "mailbox-state",
                                    "list": [{ "id": "sent-1", "role": "sent" }],
                                    "notFound": []
                                }, "r1"]]
                            }),
                            false,
                        ),
                        "Identity/get" => (
                            serde_json::json!({
                                "methodResponses": [["Identity/get", {
                                    "accountId": "account-1",
                                    "state": "identity-state",
                                    "list": [{
                                        "id": "identity-1",
                                        "email": "Sender@example.test"
                                    }],
                                    "notFound": []
                                }, "id1"]]
                            }),
                            false,
                        ),
                        "Email/import" => (
                            serde_json::json!({
                                "methodResponses": [
                                    ["Email/import", {
                                        "accountId": "account-1",
                                        "oldState": "email-old",
                                        "newState": "email-new",
                                        "created": { "draft": { "id": "email-1" } }
                                    }, "i1"],
                                    ["EmailSubmission/set", {
                                        "accountId": "account-1",
                                        "oldState": "submission-old",
                                        "newState": "submission-new",
                                        "created": {
                                            "sub1": { "id": "submission-1" }
                                        }
                                    }, "s1"]
                                ]
                            }),
                            true,
                        ),
                        method => panic!("unexpected JMAP method: {method}"),
                    }
                };
                write_json_response(&mut stream, response).await;
                requests.push(request);
                if is_final {
                    break;
                }
            }
            requests_tx.send(requests).ok();
        });

        (base_url, requests_rx)
    }

    #[derive(Default)]
    struct MemoryTokenStore {
        tokens: Mutex<HashMap<String, OAuthTokens>>,
        loads: Mutex<Vec<String>>,
    }

    impl MemoryTokenStore {
        fn was_loaded(&self, account_id: &str) -> bool {
            self.loads
                .lock()
                .unwrap()
                .iter()
                .any(|loaded| loaded == account_id)
        }
    }

    impl OAuthTokenStore for MemoryTokenStore {
        fn load(&self, account_id: &str) -> Result<Option<OAuthTokens>> {
            self.loads.lock().unwrap().push(account_id.to_string());
            Ok(self.tokens.lock().unwrap().get(account_id).cloned())
        }

        fn store(&self, account_id: &str, tokens: &OAuthTokens) -> Result<()> {
            self.tokens
                .lock()
                .unwrap()
                .insert(account_id.to_string(), tokens.clone());
            Ok(())
        }

        fn delete(&self, account_id: &str) -> Result<()> {
            self.tokens.lock().unwrap().remove(account_id);
            Ok(())
        }
    }

    #[derive(Default)]
    struct ScopeRecordingEndpoint {
        scopes: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl TokenEndpointClient for ScopeRecordingEndpoint {
        async fn exchange_code(
            &self,
            _provider: &OAuthProvider,
            _code: &str,
            _port: u16,
            _code_verifier: Option<&str>,
        ) -> Result<OAuthTokens> {
            Err(Error::Other("unexpected code exchange".into()))
        }

        async fn refresh(
            &self,
            _provider: &OAuthProvider,
            _refresh_token: &str,
        ) -> Result<OAuthTokens> {
            Err(Error::Other("unexpected unscoped refresh".into()))
        }

        async fn refresh_scoped(
            &self,
            _provider: &OAuthProvider,
            _refresh_token: &str,
            scopes: &str,
        ) -> Result<OAuthTokens> {
            self.scopes.lock().unwrap().push(scopes.to_string());
            Ok(OAuthTokens {
                access_token: "smtp-access-token".into(),
                refresh_token: Some("rotated-refresh-token".into()),
                expires_at: Some(i64::MAX),
            })
        }

        async fn refresh_dynamic(
            &self,
            _token_url: &str,
            _refresh_token: &str,
            _client_id: &str,
        ) -> Result<OAuthTokens> {
            Err(Error::Other("unexpected dynamic refresh".into()))
        }
    }

    struct NoopEventSink;

    impl EventSink for NoopEventSink {
        fn publish(&self, _event: ApplicationEvent) {}
    }

    fn account(mail_protocol: &str, auth_method: &str) -> AccountFull {
        let bindings = if mail_protocol.is_empty() {
            Vec::new()
        } else {
            vec![ServiceBinding {
                id: "b1".into(),
                account_id: "acc1".into(),
                service: "mail".into(),
                protocol: mail_protocol.into(),
                enabled: true,
                sync_interval_seconds: None,
                config_json: "{}".into(),
            }]
        };
        AccountFull {
            id: "acc1".into(),
            display_name: "Test".into(),
            email: "u@example.com".into(),
            provider: "generic".into(),
            mail_protocol: mail_protocol.into(),
            imap_host: String::new(),
            imap_port: 0,
            smtp_host: String::new(),
            smtp_port: 0,
            jmap_url: String::new(),
            caldav_url: String::new(),
            meet_url: String::new(),
            meet_protocol: String::new(),
            username: "u@example.com".into(),
            password: String::new(),
            use_tls: true,
            enabled: true,
            signature: String::new(),
            jmap_auth_method: String::new(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
            calendar_sync_enabled: false,
            auth_method: auth_method.into(),
            bindings,
            mail_sync_enabled: true,
            contacts_sync_enabled: false,
            mail_sync_interval_seconds: None,
            calendar_sync_interval_seconds: None,
            contacts_sync_interval_seconds: None,
            pgp_attach_pubkey_on_sign: false,
            pgp_autocrypt_header: false,
            pgp_encrypt_subject: false,
            pgp_encrypt_drafts: false,
        }
    }

    async fn insert_executor_account(
        db: &DbPool,
        account_id: &str,
        protocol: &str,
        auth_method: &str,
    ) {
        let config_json = match protocol {
            "imap" => serde_json::json!({
                "imap_host": "imap.example.test",
                "imap_port": 993,
                "smtp_host": "smtp.example.test",
                "smtp_port": 587,
                "use_tls": true,
            }),
            "jmap" => serde_json::json!({
                "url": "http://example.test/jmap",
                "auth_method": "oidc",
            }),
            _ => serde_json::json!({}),
        };
        let conn = db.writer().await;
        conn.execute(
            "INSERT INTO accounts
             (id, display_name, email, username, auth_method)
             VALUES (?1, ?2, ?3, ?3, ?4)",
            rusqlite::params![
                account_id,
                format!("{protocol} executor"),
                format!("{protocol}@example.test"),
                auth_method,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO service_bindings
             (id, account_id, service, protocol, enabled, config_json)
             VALUES (?1, ?2, 'mail', ?3, 1, ?4)",
            rusqlite::params![
                format!("{account_id}-mail"),
                account_id,
                protocol,
                config_json.to_string(),
            ],
        )
        .unwrap();
    }

    fn persisted_send_with_invalid_sender(account_id: &str) -> MailOp {
        let original = MailOp::SendRaw {
            raw_message: b"From: sender@example.test\r\nTo: recipient@example.test\r\n\r\nbody"
                .to_vec(),
            from: "invalid sender".into(),
            to: vec!["recipient@example.test".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "routing fixture".into(),
        };
        let (action_type, payload) = mail_op_to_outbox(&original).unwrap();
        let entry = OutboxEntry {
            id: 1,
            account_id: account_id.into(),
            action_type: action_type.into(),
            payload_json: payload.to_string(),
            retry_count: 0,
        };
        let replayed = outbox_to_mail_op(&entry).expect("valid send must replay from outbox");
        assert_eq!(replayed, original);
        replayed
    }

    fn assert_invalid_sender_error(protocol: &str, error: &str) {
        assert!(
            error.starts_with("Invalid SMTP address 'invalid sender':"),
            "{protocol} SendRaw must reach SMTP sender validation: {error}"
        );
    }

    #[test]
    fn protocols_resolve_to_matching_backends() {
        for proto in ["imap", "jmap", "graph"] {
            let config = account(proto, "password").mail_config();
            let b = for_account(&config).expect(proto);
            assert_eq!(b.protocol(), proto);
        }
    }

    #[test]
    fn backends_report_their_draft_storage_format() {
        let cases = [
            ("imap", DraftStorageFormat::RawMime),
            ("jmap", DraftStorageFormat::RawMime),
            ("graph", DraftStorageFormat::StructuredText),
        ];

        for (protocol, expected) in cases {
            let config = account(protocol, "password").mail_config();
            let backend = for_account(&config).unwrap();
            assert_eq!(backend.draft_storage_format(), expected);
        }
    }

    #[test]
    fn unknown_protocol_falls_back_to_imap() {
        let config = account("pop3", "password").mail_config();
        let b = for_account(&config).unwrap();
        assert_eq!(b.protocol(), "imap");
    }

    #[test]
    fn empty_protocol_is_none() {
        assert!(for_account(&account("", "password").mail_config()).is_none());
    }

    #[test]
    fn focused_config_uses_enabled_binding_and_mail_fields() {
        let mut account = account("imap", "oauth-microsoft");
        account.imap_host = "imap.example.com".into();
        account.imap_port = 993;
        account.smtp_host = "smtp.example.com".into();
        account.smtp_port = 587;
        account.password = "keyring-secret".into();

        let config = account.mail_config();

        assert_eq!(config.protocol, "imap");
        assert_eq!(config.id, "acc1");
        assert_eq!(config.imap_host, "imap.example.com");
        assert_eq!(config.smtp_host, "smtp.example.com");
        assert_eq!(config.password, "keyring-secret");
        assert_eq!(config.auth_method, "oauth-microsoft");
    }

    #[test]
    fn focused_config_does_not_route_disabled_legacy_mail_protocol() {
        let mut account = account("imap", "password");
        account.bindings[0].enabled = false;

        let config = account.mail_config();

        assert!(config.protocol.is_empty());
        assert!(for_account(&config).is_none());
    }

    #[test]
    fn only_o365_imap_suspends_idle() {
        let imap_o365 = account("imap", "oauth-microsoft");
        let imap_plain = account("imap", "password");
        let graph = account("graph", "oauth-microsoft");
        let imap_o365 = imap_o365.mail_config();
        let imap_plain = imap_plain.mail_config();
        let graph = graph.mail_config();
        assert!(for_account(&imap_o365)
            .unwrap()
            .suspends_idle_for_ops(&imap_o365));
        assert!(!for_account(&imap_plain)
            .unwrap()
            .suspends_idle_for_ops(&imap_plain));
        assert!(!for_account(&graph).unwrap().suspends_idle_for_ops(&graph));
    }

    #[test]
    fn body_fetch_request_recovers_provider_references() {
        let cases = [
            ("imap", "acc1_INBOX_42", "INBOX", 42),
            ("jmap", "acc1_mailbox_email_with_underscores", "mailbox", 0),
            ("graph", "acc1_AAMk_opaque", "folder", 0),
        ];

        for (protocol, message_id, folder, uid) in cases {
            let account = account(protocol, "password").mail_config();
            let request = BodyFetchRequest::from_db_row(
                &account,
                message_id,
                folder,
                uid,
                Vec::new(),
                BodyLocation::NotFetched,
            )
            .unwrap();
            assert_eq!(request.message_ref.to_db_id("acc1"), message_id);
        }
    }

    #[test]
    fn body_fetch_request_preserves_legacy_jmap_raw_id_fallback() {
        let account = account("jmap", "password").mail_config();
        let request = BodyFetchRequest::from_db_row(
            &account,
            "raw_email_id",
            "mailbox",
            0,
            Vec::new(),
            BodyLocation::NotFetched,
        )
        .unwrap();
        assert_eq!(
            request.message_ref.into_jmap_email_id().as_deref(),
            Some("raw_email_id")
        );
    }

    #[tokio::test]
    async fn send_raw_routing_uses_smtp_for_graph_and_imap_but_jmap_submission() {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DbPool::new(&temp.path().join("mail-routing.db"), 1).unwrap());
        {
            let conn = db.writer().await;
            crate::db::schema::initialize(&conn).unwrap();
        }

        let suffix = uuid::Uuid::new_v4();
        let graph_id = format!("graph-{suffix}");
        let imap_id = format!("imap-{suffix}");
        let jmap_id = format!("jmap-{suffix}");
        insert_executor_account(&db, &graph_id, "graph", "oauth-microsoft").await;
        insert_executor_account(&db, &imap_id, "imap", "oauth-microsoft").await;
        insert_executor_account(&db, &jmap_id, "jmap", "oauth-jmap-oidc").await;

        let token_store = Arc::new(MemoryTokenStore::default());
        for account_id in [&graph_id, &imap_id] {
            token_store
                .store(
                    account_id,
                    &OAuthTokens {
                        access_token: "stale-access-token".into(),
                        refresh_token: Some("refresh-token".into()),
                        expires_at: Some(0),
                    },
                )
                .unwrap();
        }
        token_store
            .store(
                &jmap_id,
                &OAuthTokens {
                    access_token: "jmap-oidc-access-token".into(),
                    refresh_token: Some("jmap-refresh-token".into()),
                    expires_at: Some(i64::MAX),
                },
            )
            .unwrap();
        let endpoint = Arc::new(ScopeRecordingEndpoint::default());
        let credentials = Arc::new(ProviderCredentialService::new(
            token_store.clone(),
            endpoint.clone(),
        ));
        let providers = Arc::new(ProviderServices::new(
            credentials,
            token_store.clone(),
            endpoint.clone(),
            ProviderTransports::production().unwrap(),
        ));
        let ctx = MailSyncCtx {
            events: Arc::new(NoopEventSink),
            db: db.clone(),
            data_dir: temp.path().to_path_buf(),
            providers,
        };

        let graph_account = {
            let conn = db.reader();
            crate::db::accounts::get_account_full(&conn, &graph_id)
                .unwrap()
                .mail_config()
        };
        let mut graph = for_account(&graph_account).unwrap().op_executor();
        let graph_error = graph
            .execute(
                &ctx,
                &graph_id,
                persisted_send_with_invalid_sender(&graph_id),
            )
            .await
            .unwrap_err()
            .to_string();
        assert_invalid_sender_error("Graph", &graph_error);
        assert!(!graph_error.contains("Graph cannot send raw mail"));
        assert_eq!(
            *endpoint.scopes.lock().unwrap(),
            vec![crate::oauth::MICROSOFT_IMAP_SCOPES]
        );

        let imap_account = {
            let conn = db.reader();
            crate::db::accounts::get_account_full(&conn, &imap_id)
                .unwrap()
                .mail_config()
        };
        let mut imap = for_account(&imap_account).unwrap().op_executor();
        let imap_error = imap
            .execute(&ctx, &imap_id, persisted_send_with_invalid_sender(&imap_id))
            .await
            .unwrap_err()
            .to_string();
        assert_invalid_sender_error("IMAP", &imap_error);
        assert_eq!(
            *endpoint.scopes.lock().unwrap(),
            vec![
                crate::oauth::MICROSOFT_IMAP_SCOPES,
                crate::oauth::MICROSOFT_IMAP_SCOPES,
            ]
        );

        let jmap_account = {
            let conn = db.reader();
            crate::db::accounts::get_account_full(&conn, &jmap_id)
                .unwrap()
                .mail_config()
        };
        let mut jmap = for_account(&jmap_account).unwrap().op_executor();
        let jmap_error = jmap
            .execute(&ctx, &jmap_id, persisted_send_with_invalid_sender(&jmap_id))
            .await
            .unwrap_err()
            .to_string();
        // JMAP validates its mandatory explicit envelope before resolving
        // credentials or opening a connection. The distinct error proves the
        // replay stayed on the native JMAP path rather than SMTP.
        assert!(!token_store.was_loaded(&jmap_id));
        assert!(
            jmap_error.contains("Invalid JMAP submission mail-from address"),
            "JMAP SendRaw must retain native JMAP submission: {jmap_error}"
        );
        assert!(!jmap_error.contains("SMTP send_raw"));
        assert_eq!(
            *endpoint.scopes.lock().unwrap(),
            vec![
                crate::oauth::MICROSOFT_IMAP_SCOPES,
                crate::oauth::MICROSOFT_IMAP_SCOPES,
            ]
        );
    }

    #[tokio::test]
    async fn persisted_jmap_send_retains_bcc_in_explicit_envelope() {
        let (jmap_url, requests_rx) = jmap_submission_server().await;
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DbPool::new(&temp.path().join("jmap-outbox.db"), 1).unwrap());
        {
            let conn = db.writer().await;
            crate::db::schema::initialize(&conn).unwrap();
        }

        let account_id = format!("jmap-outbox-{}", uuid::Uuid::new_v4());
        insert_executor_account(&db, &account_id, "jmap", "password").await;
        {
            let conn = db.writer().await;
            conn.execute(
                "UPDATE service_bindings SET config_json = ?1
                 WHERE account_id = ?2 AND service = 'mail'",
                rusqlite::params![
                    serde_json::json!({
                        "url": jmap_url,
                        "auth_method": "basic",
                    })
                    .to_string(),
                    account_id,
                ],
            )
            .unwrap();
        }

        let token_store = Arc::new(MemoryTokenStore::default());
        let endpoint = Arc::new(ScopeRecordingEndpoint::default());
        let credentials = Arc::new(ProviderCredentialService::new(
            token_store.clone(),
            endpoint.clone(),
        ));
        let mut transports = ProviderTransports::production().unwrap();
        transports.jmap_api_http = loopback_http_client();
        transports.jmap_discovery_http = loopback_http_client();
        let providers = Arc::new(ProviderServices::new(
            credentials,
            token_store,
            endpoint,
            transports,
        ));
        let ctx = MailSyncCtx {
            events: Arc::new(NoopEventSink),
            db: db.clone(),
            data_dir: temp.path().to_path_buf(),
            providers,
        };

        let bcc = "Hidden Recipient <hidden@example.test>";
        let raw_message =
            b"From: Sender <Sender@Example.test>\r\nTo: visible@example.test\r\nSubject: outbox\r\n\r\nbody\r\n"
                .to_vec();
        assert!(!String::from_utf8_lossy(&raw_message).contains("hidden@example.test"));
        let original = MailOp::SendRaw {
            raw_message: raw_message.clone(),
            from: "Sender <Sender@Example.test>".into(),
            to: vec!["Visible Recipient <visible@example.test>".into()],
            cc: Vec::new(),
            bcc: vec![bcc.into()],
            subject: "outbox".into(),
        };
        let (action_type, payload) = mail_op_to_outbox(&original).unwrap();
        let replayed = outbox_to_mail_op(&OutboxEntry {
            id: 1,
            account_id: account_id.clone(),
            action_type: action_type.into(),
            payload_json: payload.to_string(),
            retry_count: 0,
        })
        .unwrap();
        assert_eq!(replayed, original);

        let account = {
            let conn = db.reader();
            crate::db::accounts::get_account_full(&conn, &account_id)
                .unwrap()
                .mail_config()
        };
        let mut executor = for_account(&account).unwrap().op_executor();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            executor.execute(&ctx, &account_id, replayed),
        )
        .await
        .expect("JMAP outbox execution timed out")
        .unwrap();

        let requests = tokio::time::timeout(std::time::Duration::from_secs(2), requests_rx)
            .await
            .unwrap()
            .unwrap();
        let upload = requests
            .iter()
            .find(|request| request.path == "/upload/account-1")
            .unwrap();
        assert_eq!(upload.body, raw_message);

        let identity_index = requests
            .iter()
            .position(|request| request.body.windows(12).any(|part| part == b"Identity/get"))
            .unwrap();
        let upload_index = requests
            .iter()
            .position(|request| request.path == "/upload/account-1")
            .unwrap();
        assert!(
            identity_index < upload_index,
            "identity must resolve before upload"
        );

        let mailbox_requests = requests
            .iter()
            .filter(|request| request.body.windows(11).any(|part| part == b"Mailbox/get"))
            .count();
        assert_eq!(
            mailbox_requests, 1,
            "Inbox fallback must stay lazy when Sent exists"
        );

        let submission = requests
            .iter()
            .filter(|request| request.path == "/api")
            .find_map(|request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                (body["methodCalls"][0][0] == "Email/import").then_some(body)
            })
            .unwrap();
        let envelope = &submission["methodCalls"][1][1]["create"]["sub1"]["envelope"];
        assert_eq!(
            envelope,
            &serde_json::json!({
                "mailFrom": {
                    "email": "Sender@Example.test",
                    "parameters": null
                },
                "rcptTo": [
                    { "email": "visible@example.test", "parameters": null },
                    { "email": "hidden@example.test", "parameters": null }
                ]
            })
        );
        assert!(submission["methodCalls"][1][1]
            .get("onSuccessUpdateEmail")
            .is_none());
        assert_eq!(
            serde_json::to_string(&submission)
                .unwrap()
                .matches("hidden@example.test")
                .count(),
            1,
            "Bcc must appear only in the explicit submission rcptTo"
        );
    }
}
