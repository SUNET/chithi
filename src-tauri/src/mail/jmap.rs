use crate::error::{Error, Result};
use crate::mail::msgid::normalize_message_id;
use crate::mail::search::{build_jmap_filter, SearchHit, SearchQuery};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// JMAP Calendar types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JmapCalendar {
    pub id: String,
    pub name: String,
    pub color: Option<String>,
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JmapCalendarEvent {
    pub id: String,
    pub calendar_id: String,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start: String, // ISO 8601
    pub end: String,
    pub all_day: bool,
    pub timezone: Option<String>,
    pub recurrence_rule: Option<String>,
    pub uid: Option<String>,
    pub organizer_email: Option<String>,
    pub attendees_json: Option<String>,
}

#[derive(Clone)]
pub struct JmapConfig {
    pub jmap_url: String,
    pub email: String,
    pub username: String,
    pub password: String,
    pub access_token: Option<String>,
    /// One of `"basic"`, `"bearer"`, or `"oidc"`. Carried explicitly
    /// (not just inferred from `access_token.is_some()`) so `connect()`
    /// can fail fast when bearer mode is selected but no token is
    /// available — otherwise the request silently downgrades to HTTP
    /// Basic with an empty password and the user sees a generic 401
    /// instead of "your API token is missing".
    pub auth_method: String,
    /// OIDC metadata for token refresh (used by push loop on reconnect)
    pub oidc_token_endpoint: String,
    pub oidc_client_id: String,
}

#[cfg(test)]
mod connect_tests {
    use super::*;

    fn http_config() -> JmapConfig {
        JmapConfig {
            jmap_url: "http://example.com/jmap".into(),
            email: "u@example.com".into(),
            username: "u".into(),
            password: "p".into(),
            access_token: None,
            auth_method: "basic".into(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
        }
    }

    #[tokio::test]
    async fn connect_rejects_http_url() {
        let msg = match JmapConnection::connect(&http_config()).await {
            Ok(_) => String::new(),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("https"), "expected scheme error, got: {}", msg);
    }

    /// Regression: a Fastmail account configured with bearer auth but
    /// no token (keyring entry missing, or the user saved the form
    /// with an empty API token before the save-time guard landed) used
    /// to silently fall through to HTTP Basic with an empty password.
    /// Stalwart and Fastmail both reject that with a generic 401, so
    /// the user saw a confusing auth failure instead of "your token is
    /// missing". connect() now fails fast with an explicit error.
    #[tokio::test]
    async fn connect_rejects_bearer_without_token() {
        let cfg = JmapConfig {
            jmap_url: "https://api.fastmail.com".into(),
            email: "u@fastmail.com".into(),
            username: "u".into(),
            password: String::new(),
            access_token: None,
            auth_method: "bearer".into(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
        };
        let msg = match JmapConnection::connect(&cfg).await {
            Ok(_) => String::new(),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("bearer") && msg.contains("token"),
            "expected bearer/token error, got: {}",
            msg
        );
    }
}

impl JmapConfig {
    pub fn from_account(account: &crate::db::accounts::AccountFull) -> Self {
        // "bearer" mode stores the API token in the password field but the
        // server (e.g. Fastmail) expects Authorization: Bearer <token>, not
        // Basic auth. Promote it to access_token so apply_auth() routes it
        // through the bearer branch alongside OIDC tokens.
        //
        // Trim whitespace — reqwest's bearer_auth() embeds the token
        // verbatim into the header, so a trailing newline from paste
        // turns "Bearer fmu1-xxx\n" into a malformed header and Fastmail
        // returns "Invalid Authorization bearer parameters, not valid
        // format". The token format itself never contains whitespace, so
        // trimming is always safe.
        let trimmed = account.password.trim();
        let (password, access_token) =
            if account.jmap_auth_method == "bearer" && !trimmed.is_empty() {
                (String::new(), Some(trimmed.to_string()))
            } else {
                (account.password.clone(), None)
            };
        Self {
            jmap_url: account.jmap_url.clone(),
            email: account.email.clone(),
            username: account.username.clone(),
            password,
            access_token,
            auth_method: account.jmap_auth_method.clone(),
            oidc_token_endpoint: account.oidc_token_endpoint.clone(),
            oidc_client_id: account.oidc_client_id.clone(),
        }
    }

    /// Apply authentication to a reqwest RequestBuilder.
    /// Uses Bearer auth if access_token is set, otherwise Basic auth.
    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(ref token) = self.access_token {
            req.bearer_auth(token)
        } else {
            req.basic_auth(&self.username, Some(&self.password))
        }
    }
}

#[cfg(test)]
mod from_account_tests {
    use super::*;
    use crate::db::accounts::AccountFull;

    fn account_with(auth: &str, password: &str) -> AccountFull {
        AccountFull {
            id: "acc1".into(),
            display_name: "Test".into(),
            email: "u@example.com".into(),
            provider: "generic".into(),
            mail_protocol: "jmap".into(),
            imap_host: String::new(),
            imap_port: 0,
            smtp_host: String::new(),
            smtp_port: 0,
            jmap_url: "https://api.example.com".into(),
            caldav_url: String::new(),
            meet_url: String::new(),
            meet_protocol: String::new(),
            username: "u@example.com".into(),
            password: password.into(),
            use_tls: true,
            enabled: true,
            signature: String::new(),
            jmap_auth_method: auth.into(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
            calendar_sync_enabled: false,
            auth_method: String::new(),
            bindings: Vec::new(),
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

    #[test]
    fn basic_mode_keeps_password_clears_access_token() {
        let cfg = JmapConfig::from_account(&account_with("basic", "hunter2"));
        assert_eq!(cfg.password, "hunter2");
        assert!(cfg.access_token.is_none());
    }

    #[test]
    fn bearer_mode_moves_password_to_access_token() {
        let cfg = JmapConfig::from_account(&account_with("bearer", "fmu1-secret-api-token"));
        assert_eq!(cfg.password, "");
        assert_eq!(cfg.access_token.as_deref(), Some("fmu1-secret-api-token"));
    }

    #[test]
    fn bearer_mode_trims_whitespace() {
        // Paste from settings page can carry a trailing newline.
        // reqwest::bearer_auth embeds the value verbatim, so a
        // newline turns "Bearer fmu1-xxx\n" into a malformed header
        // and Fastmail returns "Invalid Authorization bearer
        // parameters, not valid format". Verify the trim happens.
        let cfg = JmapConfig::from_account(&account_with("bearer", "  fmu1-secret-api-token\n"));
        assert_eq!(cfg.access_token.as_deref(), Some("fmu1-secret-api-token"));
        assert_eq!(cfg.password, "");
    }

    #[test]
    fn bearer_mode_with_empty_password_falls_through() {
        // Token-less bearer (editing account form, password preserved in
        // keyring) must not promote an empty string to access_token —
        // apply_auth would then send "Bearer " with no value.
        let cfg = JmapConfig::from_account(&account_with("bearer", ""));
        assert_eq!(cfg.password, "");
        assert!(cfg.access_token.is_none());
    }

    #[test]
    fn oidc_mode_leaves_access_token_for_caller() {
        // OIDC populates access_token at the call site (sync_cmd / push
        // loop) after refresh — from_account itself should leave it None.
        let cfg = JmapConfig::from_account(&account_with("oidc", ""));
        assert!(cfg.access_token.is_none());
    }

    #[tokio::test]
    async fn apply_auth_routes_bearer_for_token() {
        let cfg = JmapConfig {
            jmap_url: "https://api.example.com".into(),
            email: "u@example.com".into(),
            username: "u".into(),
            password: String::new(),
            access_token: Some("tok".into()),
            auth_method: "bearer".into(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
        };
        let client = reqwest::Client::new();
        let req = cfg
            .apply_auth(client.get("https://api.example.com/"))
            .build()
            .unwrap();
        let auth = req
            .headers()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(auth, "Bearer tok");
    }

    #[tokio::test]
    async fn apply_auth_routes_basic_for_password() {
        let cfg = JmapConfig {
            jmap_url: "https://api.example.com".into(),
            email: "u@example.com".into(),
            username: "u".into(),
            password: "p".into(),
            access_token: None,
            auth_method: "basic".into(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
        };
        let client = reqwest::Client::new();
        let req = cfg
            .apply_auth(client.get("https://api.example.com/"))
            .build()
            .unwrap();
        let auth = req
            .headers()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            auth.starts_with("Basic "),
            "expected Basic auth, got: {}",
            auth
        );
    }
}

/// Result of `JmapConnection::fetch_emails`. `is_full` distinguishes a
/// full mailbox scan (where `destroyed` is empty and the caller should
/// reconcile deletions by comparing against the full server-side set)
/// from a delta sync (where `destroyed` lists exactly the IDs the
/// server removed).
#[derive(Debug, Clone)]
pub struct JmapFetchResult {
    pub emails: Vec<JmapEmail>,
    pub destroyed: Vec<String>,
    pub state: String,
    pub is_full: bool,
}

#[derive(Debug, Clone)]
pub struct JmapEmail {
    pub id: String,
    pub subject: Option<String>,
    pub from_name: Option<String>,
    pub from_email: Option<String>,
    pub to_addresses: String,
    pub cc_addresses: String,
    pub date: String,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    /// Full RFC 5322 References chain, root first. Used at insert time
    /// to thread mailing-list patch series back to their root discussion.
    pub references: Vec<String>,
    pub size: u64,
    pub has_attachments: bool,
    pub flags: Vec<String>,
    pub preview: Option<String>,
    /// JMAP mailbox IDs this email is in (an Email can be in multiple
    /// mailboxes — "labels" in Gmail-style accounts). Used by the delta
    /// sync path to filter `Email/changes` results, since that method
    /// returns changes for the whole account, not a single mailbox.
    pub mailbox_ids: Vec<String>,
}

/// JMAP connection that uses raw HTTP requests through the HTTPS proxy.
/// This avoids issues with jmap-client following internal URLs from the
/// session that aren't accessible externally (e.g., http://host:8080).
pub struct JmapConnection {
    http: reqwest::Client,
    api_url: String,
    download_url_template: String,
    upload_url_template: String,
    event_source_url_template: Option<String>,
    account_id: String,
}

#[derive(Deserialize)]
struct JmapSession {
    #[serde(rename = "apiUrl")]
    api_url: String,
    #[serde(rename = "downloadUrl")]
    download_url: String,
    #[serde(rename = "uploadUrl")]
    upload_url: String,
    #[serde(rename = "eventSourceUrl", default)]
    event_source_url: Option<String>,
    #[serde(rename = "primaryAccounts")]
    primary_accounts: std::collections::HashMap<String, String>,
}

impl JmapConnection {
    pub async fn connect(config: &JmapConfig) -> Result<Self> {
        // Fail fast if bearer mode was selected but no token resolved.
        // Without this guard, apply_auth() would silently fall through
        // to HTTP Basic with an empty password, the server would 401,
        // and the user would see a generic auth failure instead of
        // "your API token is missing or empty". The same fast-fail is
        // wanted for OIDC — bearer_auth("") would otherwise emit an
        // "Authorization: Bearer " header with no value.
        if (config.auth_method == "bearer" || config.auth_method == "oidc")
            && config.access_token.as_deref().unwrap_or("").is_empty()
        {
            return Err(Error::Other(format!(
                "JMAP {} mode is selected but no access token is available — \
                 the keyring entry is missing or the API token is empty",
                config.auth_method
            )));
        }

        let base_url = if !config.jmap_url.is_empty() {
            let url = config.jmap_url.trim_end_matches('/').to_string();
            let url = url.trim_end_matches("/.well-known/jmap").to_string();
            crate::mail::url_validation::require_https(&url)?;
            url
        } else {
            // Auto-discover
            let domain = config
                .email
                .rsplit_once('@')
                .map(|(_, d)| d)
                .ok_or_else(|| {
                    Error::Other(format!("Cannot extract domain from '{}'", config.email))
                })?;
            let candidates = [
                format!("https://{}", domain),
                format!("https://mail.{}", domain),
                format!("https://jmap.{}", domain),
            ];
            let http = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .map_err(|e| Error::Other(e.to_string()))?;
            let mut found = None;
            for c in &candidates {
                let url = format!("{}/.well-known/jmap", c);
                if let Ok(resp) = http.get(&url).send().await {
                    if resp.status().is_success() || resp.status().as_u16() == 401 {
                        found = Some(c.clone());
                        break;
                    }
                }
            }
            found
                .ok_or_else(|| Error::Other(format!("JMAP auto-discovery failed for {}", domain)))?
        };

        // Diagnostic: enough to tell whether bearer was selected and to spot
        // truncated/empty secrets without leaking any part of the token. The
        // earlier version logged the first 4 characters of the bearer token
        // as a "prefix" — even a partial secret can leak via logs, so log
        // only mode + length now.
        match config.access_token.as_ref() {
            Some(t) => log::info!(
                "JMAP connecting to {} as {} [auth=bearer token_len={}]",
                base_url,
                config.username,
                t.len(),
            ),
            None => log::info!(
                "JMAP connecting to {} as {} [auth=basic password_len={}]",
                base_url,
                config.username,
                config.password.len(),
            ),
        }

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Other(e.to_string()))?;

        // Fetch session with authentication
        let well_known = format!("{}/.well-known/jmap", base_url);
        let resp = config
            .apply_auth(http.get(&well_known))
            .send()
            .await
            .map_err(|e| Error::Other(format!("JMAP session fetch failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!("JMAP session: {} {}", status, body)));
        }

        let session: JmapSession = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("JMAP session parse failed: {}", e)))?;

        // Get the default account ID
        let account_id = session
            .primary_accounts
            .values()
            .next()
            .cloned()
            .ok_or_else(|| Error::Other("No primary account in JMAP session".into()))?;

        // Rewrite URLs to go through the HTTPS proxy instead of internal URLs.
        // e.g., "http://mail.example.com:8080/jmap/" → "https://mail.example.com/jmap/"
        let api_url = rewrite_url(&session.api_url, &base_url);
        let download_url = rewrite_url(&session.download_url, &base_url);
        let upload_url = rewrite_url(&session.upload_url, &base_url);
        let event_source_url = session
            .event_source_url
            .as_deref()
            .map(|u| rewrite_url(u, &base_url));

        log::info!(
            "JMAP connected: account={}, api={}, eventSource={:?}",
            account_id,
            api_url,
            event_source_url
        );

        Ok(Self {
            http,
            api_url,
            download_url_template: download_url,
            upload_url_template: upload_url,
            event_source_url_template: event_source_url,
            account_id,
        })
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// Build the EventSource URL for push notifications.
    /// The template uses `{types}`, `{closeafter}`, and `{ping}` placeholders
    /// per RFC 8620 §7.3.
    pub fn event_source_url(&self, types: &str, ping: u32) -> Option<String> {
        self.event_source_url_template.as_ref().map(|tpl| {
            tpl.replace("{types}", types)
                .replace("{closeafter}", "no")
                .replace("{ping}", &ping.to_string())
        })
    }

    /// Send a JMAP API request and return the response JSON.
    async fn api_request(
        &self,
        body: &serde_json::Value,
        config: &JmapConfig,
    ) -> Result<serde_json::Value> {
        let resp = config
            .apply_auth(self.http.post(&self.api_url))
            .json(body)
            .send()
            .await
            .map_err(|e| Error::Other(format!("JMAP request failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!("JMAP API error {}: {}", status, body)));
        }

        resp.json()
            .await
            .map_err(|e| Error::Other(format!("JMAP response parse error: {}", e)))
    }

    pub async fn list_folders(
        &self,
        config: &JmapConfig,
    ) -> Result<Vec<(String, String, Option<&'static str>, Option<String>)>> {
        log::debug!("JMAP listing mailboxes");
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Mailbox/get", {
                    "accountId": self.account_id,
                    "properties": ["id", "name", "role", "totalEmails", "unreadEmails", "parentId"]
                }, "m1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        let mailboxes = resp["methodResponses"][0][1]["list"]
            .as_array()
            .ok_or_else(|| Error::Other("Invalid Mailbox/get response".into()))?;

        let mut folders = Vec::new();
        for mb in mailboxes {
            let id = mb["id"].as_str().unwrap_or("").to_string();
            let name = mb["name"].as_str().unwrap_or("Unknown").to_string();
            let role = mb["role"].as_str();
            let folder_type = match role {
                Some("inbox") => Some("inbox"),
                Some("drafts") => Some("drafts"),
                Some("sent") => Some("sent"),
                Some("trash") => Some("trash"),
                Some("junk") => Some("junk"),
                Some("archive") => Some("archive"),
                _ => None,
            };
            let parent_id = mb["parentId"].as_str().map(|s| s.to_string());
            log::debug!(
                "  mailbox: {} ({}) role={:?} parentId={:?}",
                name,
                id,
                role,
                parent_id
            );
            folders.push((name, id, folder_type, parent_id));
        }
        log::info!("JMAP found {} mailboxes", folders.len());
        Ok(folders)
    }

    /// Fetch page size for JMAP Email/query.
    const JMAP_PAGE_SIZE: u64 = 500;

    pub async fn fetch_emails(
        &self,
        config: &JmapConfig,
        mailbox_id: &str,
        since_state: Option<&str>,
    ) -> Result<JmapFetchResult> {
        // Try a delta sync first if we have a state token. On success we
        // skip the full pagination scan entirely — the common case after
        // the first sync, where 9000 envelopes shouldn't be re-fetched
        // just to discover 0–5 changed.
        //
        // Three signals fall through to a full re-sync rather than
        // propagating an error:
        //   * cannotCalculateChanges / invalidArguments — the server
        //     forgot the state (TTL'd) or the state is from a different
        //     account (RFC 8620 §5.2).
        //   * Email/changes exceeded the page cap — too many changes to
        //     paginate; faster to re-list the mailbox.
        //   * hasMoreChanges: true with newState unchanged — server is
        //     misbehaving and can't make progress.
        // All three mean "delta path cannot finish"; the safe answer is
        // the same in every case.
        if let Some(state) = since_state.filter(|s| !s.is_empty()) {
            match self.fetch_email_changes(config, state).await {
                Ok(delta) => return Ok(delta),
                Err(Error::Other(msg))
                    if msg.contains("cannotCalculateChanges")
                        || msg.contains("invalidArguments")
                        || msg.contains("Email/changes exceeded")
                        || msg.contains("hasMoreChanges but newState") =>
                {
                    log::info!(
                        "JMAP Email/changes could not complete ({}); falling back to full sync of {}",
                        msg,
                        mailbox_id
                    );
                }
                Err(e) => return Err(e),
            }
        }

        self.fetch_emails_full(config, mailbox_id).await
    }

    /// Delta sync via `Email/changes` (RFC 8621 §4.3). Returns
    /// `is_full: false` so the caller knows to reconcile deletions
    /// using `destroyed` rather than the full-server-set comparison
    /// the initial-sync path needs.
    ///
    /// `Email/changes` caps each response at `maxChanges` and sets
    /// `hasMoreChanges: true` when there is more to fetch. We loop,
    /// advancing `sinceState` to the previous response's `newState`,
    /// until the server reports no more. Without the loop a mailbox
    /// that accumulated more than one page of changes between syncs
    /// would silently drop created/updated/destroyed entries past the
    /// first page even though state would advance to the end. A hard
    /// cap on iterations prevents an infinite loop if a server keeps
    /// reporting `hasMoreChanges` without advancing state.
    async fn fetch_email_changes(
        &self,
        config: &JmapConfig,
        since_state: &str,
    ) -> Result<JmapFetchResult> {
        log::debug!("JMAP Email/changes since state={}", since_state);

        const MAX_CHANGES_PER_PAGE: u64 = 5000;
        // 100 * 5000 = 500k changes per sync, far beyond anything a
        // real mailbox produces between polls. If we hit this the
        // server is misbehaving — bail to a full re-sync.
        const MAX_PAGES: usize = 100;

        let mut emails = Vec::new();
        let mut destroyed: Vec<String> = Vec::new();
        let mut cursor = since_state.to_string();
        let final_state: String;
        let mut pages = 0usize;

        loop {
            let request = serde_json::json!({
                "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
                "methodCalls": [
                    ["Email/changes", {
                        "accountId": self.account_id,
                        // Pass as &str (not the owned String) so the json!
                        // macro's Value::from path borrows cleanly and
                        // `cursor` stays available for the hasMoreChanges
                        // comparison + reassignment after the request.
                        "sinceState": cursor.as_str(),
                        "maxChanges": MAX_CHANGES_PER_PAGE
                    }, "c1"],
                    ["Email/get", {
                        "#ids": { "resultOf": "c1", "name": "Email/changes", "path": "/created" },
                        "accountId": self.account_id,
                        "properties": ["id", "subject", "from", "to", "cc", "receivedAt",
                                       "size", "keywords", "messageId", "inReplyTo",
                                       "references", "hasAttachment", "preview", "mailboxIds"]
                    }, "g1"],
                    // Fetch full envelope properties for updated emails too,
                    // not just id+keywords: a message can appear in "updated"
                    // because its mailboxIds changed (moved into this folder),
                    // in which case the caller needs to insert it as a new
                    // row with the real subject/from/date, not an empty one.
                    ["Email/get", {
                        "#ids": { "resultOf": "c1", "name": "Email/changes", "path": "/updated" },
                        "accountId": self.account_id,
                        "properties": ["id", "subject", "from", "to", "cc", "receivedAt",
                                       "size", "keywords", "messageId", "inReplyTo",
                                       "references", "hasAttachment", "preview", "mailboxIds"]
                    }, "g2"]
                ]
            });

            let resp = self.api_request(&request, config).await?;

            // Surface server errors (cannotCalculateChanges, invalidArguments, …)
            // so the caller can decide whether to fall back to a full sync.
            if let Some(err_type) = resp["methodResponses"][0][1]["type"].as_str() {
                return Err(Error::Other(format!("Email/changes error: {}", err_type)));
            }

            let changes = &resp["methodResponses"][0][1];
            let new_state = changes["newState"].as_str().unwrap_or("").to_string();
            let has_more = changes["hasMoreChanges"].as_bool().unwrap_or(false);

            if let Some(arr) = changes["destroyed"].as_array() {
                destroyed.extend(arr.iter().filter_map(|v| v.as_str().map(String::from)));
            }

            for list_idx in [1usize, 2] {
                if let Some(arr) = resp["methodResponses"][list_idx][1]["list"].as_array() {
                    for e in arr {
                        emails.push(self.parse_jmap_email(e));
                    }
                }
            }

            if !has_more {
                final_state = new_state;
                break;
            }

            pages += 1;
            if pages >= MAX_PAGES {
                return Err(Error::Other(format!(
                    "Email/changes exceeded {} pages without finishing; falling back to full sync",
                    MAX_PAGES
                )));
            }

            // Guard against a server that returns hasMoreChanges: true
            // without advancing state — would loop forever otherwise.
            if new_state == cursor {
                return Err(Error::Other(
                    "Email/changes returned hasMoreChanges but newState == sinceState".into(),
                ));
            }
            cursor = new_state;
        }

        log::info!(
            "JMAP delta: {} created/updated, {} destroyed (pages={}, newState={})",
            emails.len(),
            destroyed.len(),
            pages + 1,
            final_state
        );

        Ok(JmapFetchResult {
            emails,
            destroyed,
            state: final_state,
            is_full: false,
        })
    }

    /// Full mailbox scan via paged `Email/query` + `Email/get`. Used on
    /// first sync (no state) and as the fallback when the server cannot
    /// calculate changes since the stored state.
    async fn fetch_emails_full(
        &self,
        config: &JmapConfig,
        mailbox_id: &str,
    ) -> Result<JmapFetchResult> {
        log::debug!("JMAP full fetch from mailbox {}", mailbox_id);

        let mut all_emails = Vec::new();
        let mut position: u64 = 0;
        let mut state = String::new();

        loop {
            let request = serde_json::json!({
                "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
                "methodCalls": [
                    ["Email/query", {
                        "accountId": self.account_id,
                        "filter": { "inMailbox": mailbox_id },
                        "sort": [{ "property": "receivedAt", "isAscending": false }],
                        "position": position,
                        "limit": Self::JMAP_PAGE_SIZE
                    }, "q1"],
                    ["Email/get", {
                        "#ids": { "resultOf": "q1", "name": "Email/query", "path": "/ids" },
                        "accountId": self.account_id,
                        "properties": ["id", "subject", "from", "to", "cc", "receivedAt",
                                       "size", "keywords", "messageId", "inReplyTo",
                                       "references", "hasAttachment", "preview", "mailboxIds"]
                    }, "g1"]
                ]
            });

            let resp = self.api_request(&request, config).await?;

            // For state continuity across syncs we want the Email/get state
            // (which is what Email/changes works against), not the queryState
            // (which only tracks the ordered query result and can't be passed
            // to Email/changes). Capture from the first page.
            if state.is_empty() {
                state = resp["methodResponses"][1][1]["state"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                log::debug!(
                    "JMAP captured Email/get state for {}: {:?} (used for next sync's Email/changes)",
                    mailbox_id,
                    state
                );
            }

            let emails_json = resp["methodResponses"][1][1]["list"]
                .as_array()
                .ok_or_else(|| Error::Other("Invalid Email/get response".into()))?;

            let page_count = emails_json.len() as u64;

            for e in emails_json {
                let email = self.parse_jmap_email(e);
                all_emails.push(email);
            }

            log::debug!(
                "JMAP fetched page at position {}: {} emails (total so far: {})",
                position,
                page_count,
                all_emails.len()
            );

            if page_count < Self::JMAP_PAGE_SIZE {
                break;
            }
            position += page_count;
        }

        log::info!(
            "JMAP full sync: {} emails from mailbox {}",
            all_emails.len(),
            mailbox_id
        );
        Ok(JmapFetchResult {
            emails: all_emails,
            destroyed: Vec::new(),
            state,
            is_full: true,
        })
    }

    /// Parse a single JMAP email JSON object into a JmapEmail struct.
    fn parse_jmap_email(&self, e: &serde_json::Value) -> JmapEmail {
        let id = e["id"].as_str().unwrap_or("").to_string();
        let subject = e["subject"].as_str().map(|s| s.to_string());

        let (from_name, from_email) = e["from"]
            .as_array()
            .and_then(|a| a.first())
            .map(|f| {
                (
                    f["name"].as_str().map(|s| s.to_string()),
                    f["email"].as_str().map(|s| s.to_string()),
                )
            })
            .unwrap_or((None, None));

        let to_addresses = addresses_to_json(e["to"].as_array());
        let cc_addresses = addresses_to_json(e["cc"].as_array());

        let date = e["receivedAt"]
            .as_str()
            .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
            .unwrap_or_default();
        let size = e["size"].as_u64().unwrap_or(0);
        // JMAP returns Message-IDs without angle brackets; canonicalize each
        // through `normalize_message_id` so the stored form matches what the
        // IMAP and Graph paths produce.
        let message_id = e["messageId"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .and_then(normalize_message_id);
        let in_reply_to = e["inReplyTo"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .and_then(normalize_message_id);
        let references: Vec<String> = e["references"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter_map(normalize_message_id)
                    .collect()
            })
            .unwrap_or_default();
        let has_attachments = e["hasAttachment"].as_bool().unwrap_or(false);
        let preview = e["preview"].as_str().map(|s| s.to_string());

        let keywords = e["keywords"].as_object();
        let mut flags = Vec::new();
        if let Some(kw) = keywords {
            if kw.contains_key("$seen") {
                flags.push("seen".to_string());
            }
            if kw.contains_key("$flagged") {
                flags.push("flagged".to_string());
            }
            if kw.contains_key("$answered") {
                flags.push("answered".to_string());
            }
            if kw.contains_key("$draft") {
                flags.push("draft".to_string());
            }
        }

        // mailboxIds is an Id[Boolean] object per RFC 8621 §4.1.4: each
        // key is a mailbox id, value is always `true`. Collect the keys.
        let mailbox_ids: Vec<String> = e["mailboxIds"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();

        JmapEmail {
            id,
            subject,
            from_name,
            from_email,
            to_addresses,
            cc_addresses,
            date,
            message_id,
            in_reply_to,
            references,
            size,
            has_attachments,
            flags,
            preview,
            mailbox_ids,
        }
    }

    pub async fn fetch_email_body(
        &self,
        config: &JmapConfig,
        email_id: &str,
    ) -> Result<Option<Vec<u8>>> {
        log::debug!("JMAP fetching body for email {}", email_id);

        // First get the blobId
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Email/get", {
                    "accountId": self.account_id,
                    "ids": [email_id],
                    "properties": ["blobId"]
                }, "b1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;
        let blob_id = resp["methodResponses"][0][1]["list"][0]["blobId"]
            .as_str()
            .ok_or_else(|| Error::Other(format!("No blobId for email {}", email_id)))?;

        // Download the blob
        let download_url = self
            .download_url_template
            .replace("{accountId}", &self.account_id)
            .replace("{blobId}", blob_id)
            .replace("{name}", "message.eml")
            .replace("{type}", "application/octet-stream");

        log::debug!("JMAP downloading blob from {}", download_url);
        let resp = config
            .apply_auth(self.http.get(&download_url))
            .send()
            .await
            .map_err(|e| Error::Other(format!("JMAP download failed: {}", e)))?;

        if !resp.status().is_success() {
            return Err(Error::Other(format!(
                "JMAP download error: {}",
                resp.status()
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::Other(format!("JMAP download read error: {}", e)))?;
        log::debug!(
            "JMAP downloaded {} bytes for email {}",
            bytes.len(),
            email_id
        );
        Ok(Some(bytes.to_vec()))
    }

    pub async fn set_flags(
        &self,
        config: &JmapConfig,
        email_ids: &[String],
        flags: &[&str],
        add: bool,
    ) -> Result<()> {
        log::debug!(
            "JMAP set_flags: {:?} add={} on {} emails",
            flags,
            add,
            email_ids.len()
        );

        let mut update = serde_json::Map::new();
        for id in email_ids {
            let mut patch = serde_json::Map::new();
            for flag in flags {
                let keyword = flag_to_keyword(flag);
                let key = format!("keywords/{}", keyword);
                patch.insert(
                    key,
                    if add {
                        serde_json::json!(true)
                    } else {
                        serde_json::json!(null)
                    },
                );
            }
            update.insert(id.clone(), serde_json::Value::Object(patch));
        }

        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Email/set", {
                    "accountId": self.account_id,
                    "update": update
                }, "s1"]
            ]
        });

        self.api_request(&request, config).await?;
        Ok(())
    }

    pub async fn delete_emails(&self, config: &JmapConfig, email_ids: &[String]) -> Result<()> {
        log::debug!("JMAP deleting {} emails", email_ids.len());
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Email/set", {
                    "accountId": self.account_id,
                    "destroy": email_ids
                }, "d1"]
            ]
        });
        self.api_request(&request, config).await?;
        Ok(())
    }

    /// Search emails across the account using `Email/query` + `Email/get`.
    /// Folder scope: account-wide (no `inMailbox` filter). Cap: 200 hits.
    pub async fn search_account(
        &self,
        config: &JmapConfig,
        account_id: &str,
        query: &SearchQuery,
    ) -> Result<Vec<SearchHit>> {
        let filter = match build_jmap_filter(query) {
            Some(f) => f,
            None => return Ok(vec![]),
        };

        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Email/query", {
                    "accountId": self.account_id,
                    "filter": filter,
                    "sort": [{ "property": "receivedAt", "isAscending": false }],
                    "limit": 200u64,
                }, "sq"],
                ["Email/get", {
                    "#ids": { "resultOf": "sq", "name": "Email/query", "path": "/ids" },
                    "accountId": self.account_id,
                    "properties": ["id", "subject", "from", "receivedAt", "preview", "messageId", "mailboxIds"]
                }, "sg"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        let emails = resp["methodResponses"][1][1]["list"]
            .as_array()
            .ok_or_else(|| Error::Other("Invalid Email/get response in search".into()))?;

        let mut hits = Vec::with_capacity(emails.len());
        for e in emails {
            hits.push(parse_jmap_search_hit(account_id, e));
        }
        Ok(hits)
    }

    pub async fn move_emails(
        &self,
        config: &JmapConfig,
        email_ids: &[String],
        from_mailbox: &str,
        to_mailbox: &str,
    ) -> Result<()> {
        log::debug!(
            "JMAP moving {} emails from {} to {}",
            email_ids.len(),
            from_mailbox,
            to_mailbox
        );
        let mut update = serde_json::Map::new();
        for id in email_ids {
            update.insert(
                id.clone(),
                serde_json::json!({
                    format!("mailboxIds/{}", from_mailbox): null,
                    format!("mailboxIds/{}", to_mailbox): true,
                }),
            );
        }
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Email/set", {
                    "accountId": self.account_id,
                    "update": update
                }, "mv1"]
            ]
        });
        self.api_request(&request, config).await?;
        Ok(())
    }
    /// Import a raw RFC822 email into a specific mailbox on this account.
    ///
    /// Uploads the message as a blob, then issues Email/import so the server
    /// stores it with the given mailbox membership and keywords. Used by
    /// cross-account move.
    pub async fn import_email_to_mailbox(
        &self,
        config: &JmapConfig,
        raw_message: &[u8],
        mailbox_id: &str,
        seen: bool,
    ) -> Result<()> {
        log::debug!(
            "JMAP importing {} bytes into mailbox {}",
            raw_message.len(),
            mailbox_id
        );

        let upload_url = self
            .upload_url_template
            .replace("{accountId}", &self.account_id);
        let resp = config
            .apply_auth(self.http.post(&upload_url))
            .header("Content-Type", "message/rfc822")
            .body(raw_message.to_vec())
            .send()
            .await
            .map_err(|e| Error::Other(format!("JMAP upload failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "JMAP upload error {}: {}",
                status, body
            )));
        }

        let upload_resp: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("JMAP upload response parse error: {}", e)))?;
        let blob_id = upload_resp["blobId"]
            .as_str()
            .ok_or_else(|| Error::Other("No blobId in upload response".into()))?
            .to_string();

        let mut keywords = serde_json::Map::new();
        if seen {
            keywords.insert("$seen".to_string(), serde_json::json!(true));
        }

        // Use a HashMap for mailboxIds so the key is the runtime value of
        // mailbox_id, not the literal string "mailbox_id".
        let mut mailbox_ids = serde_json::Map::new();
        mailbox_ids.insert(mailbox_id.to_string(), serde_json::json!(true));

        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Email/import", {
                    "accountId": self.account_id,
                    "emails": {
                        "e1": {
                            "blobId": blob_id,
                            "mailboxIds": mailbox_ids,
                            "keywords": keywords
                        }
                    }
                }, "i1"]
            ]
        });
        self.api_request(&request, config).await?;
        Ok(())
    }

    pub async fn send_email(&self, config: &JmapConfig, raw_message: &[u8]) -> Result<()> {
        log::info!("JMAP sending email ({} bytes)", raw_message.len());

        // Step 1: Upload the raw message as a blob
        let upload_url = self
            .upload_url_template
            .replace("{accountId}", &self.account_id);
        log::debug!("JMAP uploading blob to {}", upload_url);

        let resp = config
            .apply_auth(self.http.post(&upload_url))
            .header("Content-Type", "message/rfc822")
            .body(raw_message.to_vec())
            .send()
            .await
            .map_err(|e| Error::Other(format!("JMAP upload failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "JMAP upload error {}: {}",
                status, body
            )));
        }

        let upload_resp: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("JMAP upload response parse error: {}", e)))?;
        let blob_id = upload_resp["blobId"]
            .as_str()
            .ok_or_else(|| Error::Other("No blobId in upload response".into()))?
            .to_string();
        log::debug!("JMAP blob uploaded: {}", blob_id);

        // Step 2: Find the Sent mailbox (or Inbox as fallback) to store the email
        let sent_mailbox_id = self
            .find_mailbox_by_role(config, "sent")
            .await?
            .or(self.find_mailbox_by_role(config, "inbox").await?)
            .ok_or_else(|| Error::Other("No Sent or Inbox mailbox found".into()))?;
        log::debug!("JMAP using mailbox {} for sent email", sent_mailbox_id);

        // Step 3: Get the identity ID for submission
        let identity_id = self.find_identity_id(config).await?;
        log::debug!("JMAP using identity {} for submission", identity_id);

        // Step 4: Import the email into the Sent folder and submit it
        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:mail",
                "urn:ietf:params:jmap:submission"
            ],
            "methodCalls": [
                ["Email/import", {
                    "accountId": self.account_id,
                    "emails": {
                        "draft": {
                            "blobId": blob_id,
                            "mailboxIds": { sent_mailbox_id.clone(): true },
                            "keywords": { "$seen": true }
                        }
                    }
                }, "i1"],
                ["EmailSubmission/set", {
                    "accountId": self.account_id,
                    "create": {
                        "sub1": {
                            "emailId": "#draft",
                            "identityId": identity_id
                        }
                    },
                    "onSuccessUpdateEmail": {
                        "#sub1": {
                            "keywords/$draft": null,
                            "keywords/$seen": true
                        }
                    }
                }, "s1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;
        log::debug!(
            "JMAP send response: {}",
            serde_json::to_string_pretty(&resp).unwrap_or_default()
        );

        // Check for import errors
        if let Some(err) = resp["methodResponses"][0][1]["notCreated"]["draft"].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!("JMAP email import failed: {}", desc)));
        }

        // Get the imported email ID for cleanup if submission fails
        let imported_id = resp["methodResponses"][0][1]["created"]["draft"]["id"]
            .as_str()
            .map(|s| s.to_string());

        // Check for submission errors — clean up imported email on failure
        let submission_failed = if resp["methodResponses"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0)
            > 1
        {
            if resp["methodResponses"][1][0].as_str() == Some("error") {
                let desc = resp["methodResponses"][1][1]["description"]
                    .as_str()
                    .unwrap_or("Unknown error");
                Some(format!("JMAP submission failed: {}", desc))
            } else if let Some(err) =
                resp["methodResponses"][1][1]["notCreated"]["sub1"].as_object()
            {
                let desc = err
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("Unknown error");
                Some(format!("JMAP submission failed: {}", desc))
            } else {
                None
            }
        } else {
            None
        };

        if let Some(error_msg) = submission_failed {
            // Clean up the imported email that wasn't submitted
            if let Some(ref email_id) = imported_id {
                log::warn!(
                    "JMAP cleaning up imported email {} after submission failure",
                    email_id
                );
                let cleanup = serde_json::json!({
                    "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
                    "methodCalls": [
                        ["Email/set", {
                            "accountId": self.account_id,
                            "destroy": [email_id]
                        }, "cleanup"]
                    ]
                });
                let _ = self.api_request(&cleanup, config).await;
            }
            return Err(Error::Other(error_msg));
        }

        log::info!("JMAP email sent successfully");
        Ok(())
    }

    /// Save a draft email to the Drafts mailbox via JMAP Email/import.
    pub async fn save_draft(&self, config: &JmapConfig, raw_message: &[u8]) -> Result<()> {
        log::info!("JMAP saving draft ({} bytes)", raw_message.len());

        // Upload the raw message as a blob
        let upload_url = self
            .upload_url_template
            .replace("{accountId}", &self.account_id);

        let resp = config
            .apply_auth(self.http.post(&upload_url))
            .header("Content-Type", "message/rfc822")
            .body(raw_message.to_vec())
            .send()
            .await
            .map_err(|e| Error::Other(format!("JMAP draft upload failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "JMAP draft upload error {}: {}",
                status, body
            )));
        }

        let upload_resp: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("JMAP draft upload parse error: {}", e)))?;
        let blob_id = upload_resp["blobId"]
            .as_str()
            .ok_or_else(|| Error::Other("No blobId in draft upload response".into()))?
            .to_string();

        // Find the Drafts mailbox
        let drafts_mailbox_id = self
            .find_mailbox_by_role(config, "drafts")
            .await?
            .ok_or_else(|| Error::Other("No Drafts mailbox found".into()))?;

        // Import into Drafts with $draft keyword
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Email/import", {
                    "accountId": self.account_id,
                    "emails": {
                        "draft": {
                            "blobId": blob_id,
                            "mailboxIds": { drafts_mailbox_id: true },
                            "keywords": { "$seen": true, "$draft": true }
                        }
                    }
                }, "i1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notImported"]["draft"].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!("JMAP draft import failed: {}", desc)));
        }

        log::info!("JMAP draft saved successfully");
        Ok(())
    }

    /// Find the identity ID for email submission.
    async fn find_identity_id(&self, config: &JmapConfig) -> Result<String> {
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:submission"],
            "methodCalls": [
                ["Identity/get", {
                    "accountId": self.account_id
                }, "id1"]
            ]
        });
        let resp = self.api_request(&request, config).await?;
        if let Some(identities) = resp["methodResponses"][0][1]["list"].as_array() {
            if let Some(first) = identities.first() {
                if let Some(id) = first["id"].as_str() {
                    return Ok(id.to_string());
                }
            }
        }
        Err(Error::Other("No JMAP identity found for submission".into()))
    }

    // -----------------------------------------------------------------------
    // JMAP Calendar methods
    // -----------------------------------------------------------------------

    /// List all JMAP calendars for the account.
    pub async fn list_jmap_calendars(&self, config: &JmapConfig) -> Result<Vec<JmapCalendar>> {
        log::debug!("JMAP listing calendars");
        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["Calendar/get", {
                    "accountId": self.account_id,
                    "properties": ["id", "name", "color", "isDefault"]
                }, "c1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        let calendars_json = resp["methodResponses"][0][1]["list"]
            .as_array()
            .ok_or_else(|| Error::Other("Invalid Calendar/get response".into()))?;

        let mut calendars = Vec::new();
        for cal in calendars_json {
            let id = cal["id"].as_str().unwrap_or("").to_string();
            let name = cal["name"].as_str().unwrap_or("Untitled").to_string();
            let color = cal["color"].as_str().map(|s| s.to_string());
            let is_default = cal["isDefault"].as_bool().unwrap_or(false);

            log::debug!("  calendar: {} ({}) default={}", name, id, is_default);
            calendars.push(JmapCalendar {
                id,
                name,
                color,
                is_default,
            });
        }
        log::info!("JMAP found {} calendars", calendars.len());
        Ok(calendars)
    }

    /// Update the JMAP `color` property on a calendar via
    /// `Calendar/set`. JMAP calendars (RFC 8984 / "JSCalendar") store
    /// color as a CSS-format string, conventionally a `#RRGGBB` hex.
    /// Stalwart and Cyrus both honor the property; servers that
    /// don't will surface the rejection in `notUpdated` and we
    /// return that as an error so the caller can roll back.
    pub async fn set_calendar_color(
        &self,
        config: &JmapConfig,
        calendar_id: &str,
        hex: &str,
    ) -> Result<()> {
        log::info!("JMAP set color for calendar {} -> {}", calendar_id, hex);

        let mut update = serde_json::Map::new();
        update.insert(calendar_id.to_string(), serde_json::json!({ "color": hex }));

        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["Calendar/set", {
                    "accountId": self.account_id,
                    "update": update
                }, "c1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notUpdated"][calendar_id].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP Calendar/set rejected color update: {}",
                desc
            )));
        }

        log::info!("JMAP color set for calendar {}", calendar_id);
        Ok(())
    }

    /// Rename a JMAP calendar via `Calendar/set` with an update
    /// entry whose `name` field carries the new display name.
    pub async fn rename_calendar(
        &self,
        config: &JmapConfig,
        calendar_id: &str,
        new_name: &str,
    ) -> Result<()> {
        log::info!("JMAP rename calendar: id={} -> {}", calendar_id, new_name);

        // Build the update map by hand: `serde_json::json!({ calendar_id: ... })`
        // would emit the literal key "calendar_id", not the id's value.
        let mut update = serde_json::Map::new();
        update.insert(
            calendar_id.to_string(),
            serde_json::json!({ "name": new_name }),
        );

        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["Calendar/set", {
                    "accountId": self.account_id,
                    "update": update
                }, "c1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notUpdated"][calendar_id].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP Calendar/set rejected rename: {}",
                desc
            )));
        }

        log::info!("JMAP renamed calendar {}", calendar_id);
        Ok(())
    }

    /// Fetch calendar events, optionally filtered by calendar_id.
    /// Uses CalendarEvent/query + CalendarEvent/get with JSCalendar format.
    pub async fn fetch_calendar_events(
        &self,
        config: &JmapConfig,
        calendar_id: Option<&str>,
    ) -> Result<Vec<JmapCalendarEvent>> {
        log::debug!("JMAP fetching calendar events (calendar={:?})", calendar_id);

        // Note: Stalwart doesn't support "inCalendars" filter, so we fetch all
        // events and filter by calendarIds client-side.
        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["CalendarEvent/query", {
                    "accountId": self.account_id,
                    "limit": 1000
                }, "q1"],
                ["CalendarEvent/get", {
                    "#ids": { "resultOf": "q1", "name": "CalendarEvent/query", "path": "/ids" },
                    "accountId": self.account_id,
                    "properties": ["id", "calendarIds", "title", "description",
                                   "start", "duration", "showWithoutTime",
                                   "timeZone", "recurrenceRules", "uid", "locations",
                                   "participants", "@type"]
                }, "g1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;
        log::debug!(
            "JMAP CalendarEvent response: {}",
            serde_json::to_string(&resp).unwrap_or_default()
        );

        // Check if the query returned an error
        if resp["methodResponses"][0][0].as_str() == Some("error") {
            let desc = resp["methodResponses"][0][1]["description"]
                .as_str()
                .unwrap_or("Unknown");
            log::error!("JMAP CalendarEvent/query error: {}", desc);
            return Ok(vec![]);
        }

        // The get response might be at index 1 or could be missing if query returned no IDs
        let events_json = match resp["methodResponses"][1][1]["list"].as_array() {
            Some(list) => list.clone(),
            None => {
                log::debug!("JMAP CalendarEvent/get returned no list, possibly empty");
                return Ok(vec![]);
            }
        };

        let mut events = Vec::new();
        for ev in events_json {
            let id = ev["id"].as_str().unwrap_or("").to_string();
            let title = ev["title"].as_str().unwrap_or("(No title)").to_string();
            let description = ev["description"].as_str().map(|s| s.to_string());
            let uid = ev["uid"].as_str().map(|s| s.to_string());

            // calendarIds is a map { "cal-id": true, ... } — pick the first key
            let cal_id = ev["calendarIds"]
                .as_object()
                .and_then(|m| m.keys().next().cloned())
                .unwrap_or_default();

            // Location: JSCalendar uses "locations" as a map { id: { name: "..." } }
            let location = ev["locations"]
                .as_object()
                .and_then(|m| m.values().next())
                .and_then(|loc| loc["name"].as_str())
                .map(|s| s.to_string());

            // Start datetime — JSCalendar uses "start" as local time + "timeZone" as IANA id.
            let raw_start = ev["start"].as_str().unwrap_or("").to_string();
            let event_tz = ev["timeZone"].as_str().unwrap_or("").to_string();
            let start = if raw_start.is_empty() {
                raw_start.clone()
            } else {
                crate::calendar::timezone::to_utc(&raw_start, &event_tz)
            };

            let all_day = ev["showWithoutTime"].as_bool().unwrap_or(false);

            let duration_str = ev["duration"].as_str().unwrap_or("PT1H");
            let end = {
                let e = compute_end_from_duration(start.trim_end_matches('Z'), duration_str);
                if start.ends_with('Z') && !e.ends_with('Z') {
                    format!("{}Z", e)
                } else {
                    e
                }
            };

            let event_tz_opt = if event_tz.is_empty() {
                None
            } else {
                Some(event_tz)
            };

            // Recurrence rules: JSCalendar uses an array of recurrence rule objects
            let recurrence_rule = ev["recurrenceRules"]
                .as_array()
                .filter(|a| !a.is_empty())
                .map(|a| serde_json::to_string(a).unwrap_or_default());

            // Participants: supports both JSCalendar-bis (calendarAddress) and old format (sendTo.imip)
            let mut organizer_email = None;
            let mut attendees: Vec<serde_json::Value> = Vec::new();
            if let Some(participants) = ev["participants"].as_object() {
                for (_pid, p) in participants {
                    // Try calendarAddress (JSCalendar-bis), then sendTo.imip (old), then email
                    let email = p["calendarAddress"]
                        .as_str()
                        .map(|s| s.trim_start_matches("mailto:").to_string())
                        .or_else(|| {
                            p["sendTo"]
                                .as_object()
                                .and_then(|s| s.get("imip"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.trim_start_matches("mailto:").to_string())
                        })
                        .or_else(|| p["email"].as_str().map(|s| s.to_string()));
                    let name = p["name"].as_str().map(|s| s.to_string());
                    let mut status = p["participationStatus"]
                        .as_str()
                        .unwrap_or("needs-action")
                        .to_string();
                    let roles = p["roles"].as_object();
                    let is_owner = roles.map(|r| r.contains_key("owner")).unwrap_or(false);

                    if is_owner {
                        organizer_email = email.clone();
                        // Organizer is implicitly "accepted" — they created the event
                        if status == "needs-action" {
                            status = "accepted".to_string();
                        }
                    }
                    if let Some(ref em) = email {
                        attendees.push(serde_json::json!({
                            "email": em,
                            "name": name,
                            "status": status,
                        }));
                    }
                }
            }
            let attendees_json = if attendees.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&attendees).unwrap_or_default())
            };

            log::debug!(
                "  event: {} ({}) start={} end={} attendees={}",
                title,
                id,
                start,
                end,
                attendees.len()
            );
            events.push(JmapCalendarEvent {
                id,
                calendar_id: cal_id,
                title,
                description,
                location,
                start,
                end,
                all_day,
                timezone: event_tz_opt,
                recurrence_rule,
                uid,
                organizer_email,
                attendees_json,
            });
        }

        // Client-side filter by calendar if requested
        let filtered = if let Some(cal_id) = calendar_id {
            events
                .into_iter()
                .filter(|e| e.calendar_id == cal_id)
                .collect()
        } else {
            events
        };

        log::info!("JMAP fetched {} calendar events", filtered.len());
        Ok(filtered)
    }

    /// Create a calendar event on the server via CalendarEvent/set.
    /// Returns the server-assigned event ID.
    pub async fn create_calendar_event(
        &self,
        config: &JmapConfig,
        event: &JmapCalendarEvent,
    ) -> Result<String> {
        log::info!(
            "JMAP creating calendar event: '{}' organizer={:?} attendees={:?}",
            event.title,
            event.organizer_email,
            event.attendees_json
        );

        let uid = event
            .uid
            .clone()
            .unwrap_or_else(|| format!("{}@chithi", uuid::Uuid::new_v4()));

        let duration = compute_duration(&event.start, &event.end);

        let mut event_obj = serde_json::json!({
            "@type": "Event",
            "calendarIds": { &event.calendar_id: true },
            "title": event.title,
            "start": event.start,
            "duration": duration,
            "showWithoutTime": event.all_day,
            "uid": uid,
        });

        if let Some(ref desc) = event.description {
            event_obj["description"] = serde_json::json!(desc);
        }
        if let Some(ref loc) = event.location {
            event_obj["locations"] = serde_json::json!({
                "loc1": { "@type": "Location", "name": loc }
            });
        }
        if let Some(ref rrule) = event.recurrence_rule {
            if let Ok(rules) = serde_json::from_str::<serde_json::Value>(rrule) {
                event_obj["recurrenceRules"] = rules;
            }
        }

        // Add participants (organizer + attendees)
        // Uses JSCalendar-bis format (draft-ietf-calext-jscalendarbis-14):
        // - "calendarAddress" instead of "sendTo"
        // - No "replyTo" on the event
        let mut participants = serde_json::Map::new();
        if let Some(ref org_email) = event.organizer_email {
            if !org_email.is_empty() {
                participants.insert(
                    "organizer".to_string(),
                    serde_json::json!({
                        "@type": "Participant",
                        "calendarAddress": format!("mailto:{}", org_email),
                        "roles": {"owner": true, "attendee": true},
                        "participationStatus": "accepted",
                        "expectReply": false,
                    }),
                );
            }
        }
        if let Some(ref att_json) = event.attendees_json {
            if let Ok(attendees) = serde_json::from_str::<Vec<serde_json::Value>>(att_json) {
                for (i, att) in attendees.iter().enumerate() {
                    let email = att["email"].as_str().unwrap_or_default();
                    if !email.is_empty() {
                        let status = att["status"].as_str().unwrap_or("needs-action");
                        participants.insert(
                            format!("att{}", i),
                            serde_json::json!({
                                "@type": "Participant",
                                "calendarAddress": format!("mailto:{}", email),
                                "roles": {"attendee": true},
                                "participationStatus": status,
                                "expectReply": true,
                            }),
                        );
                    }
                }
            }
        }
        if !participants.is_empty() {
            event_obj["participants"] = serde_json::Value::Object(participants);
        }

        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["CalendarEvent/set", {
                    "accountId": self.account_id,
                    "create": {
                        "new1": event_obj
                    }
                }, "s1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        // Check for creation errors
        if let Some(err) = resp["methodResponses"][0][1]["notCreated"]["new1"].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP create calendar event failed: {}",
                desc
            )));
        }

        let created_id = resp["methodResponses"][0][1]["created"]["new1"]["id"]
            .as_str()
            .ok_or_else(|| Error::Other("No id in CalendarEvent/set create response".into()))?
            .to_string();

        log::info!("JMAP created calendar event id={}", created_id);
        Ok(created_id)
    }

    /// Update a calendar event on the server via CalendarEvent/set.
    pub async fn update_calendar_event(
        &self,
        config: &JmapConfig,
        event_id: &str,
        event: &JmapCalendarEvent,
    ) -> Result<()> {
        log::info!("JMAP updating calendar event: id={}", event_id);

        let duration = compute_duration(&event.start, &event.end);

        let mut patch = serde_json::json!({
            "title": event.title,
            "start": event.start,
            "duration": duration,
            "showWithoutTime": event.all_day,
        });

        if let Some(ref desc) = event.description {
            patch["description"] = serde_json::json!(desc);
        }
        if let Some(ref loc) = event.location {
            patch["locations"] = serde_json::json!({
                "loc1": { "@type": "Location", "name": loc }
            });
        }
        if let Some(ref rrule) = event.recurrence_rule {
            if let Ok(rules) = serde_json::from_str::<serde_json::Value>(rrule) {
                patch["recurrenceRules"] = rules;
            }
        }

        let mut update = serde_json::Map::new();
        update.insert(event_id.to_string(), patch);

        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["CalendarEvent/set", {
                    "accountId": self.account_id,
                    "update": update
                }, "u1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notUpdated"][event_id].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP update calendar event failed: {}",
                desc
            )));
        }

        log::info!("JMAP updated calendar event id={}", event_id);
        Ok(())
    }

    /// Update a participant's status on a calendar event via JMAP patch.
    /// Uses the JSCalendar-bis path syntax: participants/<id>/participationStatus
    pub async fn update_participant_status(
        &self,
        config: &JmapConfig,
        event_id: &str,
        participant_key: &str,
        status: &str,
    ) -> Result<()> {
        log::info!(
            "JMAP updating participant {} status to {} on event {}",
            participant_key,
            status,
            event_id
        );

        let patch_key = format!("participants/{}/participationStatus", participant_key);
        let mut patch = serde_json::Map::new();
        patch.insert(patch_key, serde_json::json!(status));

        let mut update = serde_json::Map::new();
        update.insert(event_id.to_string(), serde_json::Value::Object(patch));

        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:calendars"],
            "methodCalls": [
                ["CalendarEvent/set", {
                    "accountId": self.account_id,
                    "update": update
                }, "u1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notUpdated"][event_id].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP update participant failed: {}",
                desc
            )));
        }

        log::info!("JMAP updated participant status on event {}", event_id);
        Ok(())
    }

    /// Delete a calendar event on the server via CalendarEvent/set.
    pub async fn delete_calendar_event(&self, config: &JmapConfig, event_id: &str) -> Result<()> {
        log::info!("JMAP deleting calendar event: id={}", event_id);

        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["CalendarEvent/set", {
                    "accountId": self.account_id,
                    "destroy": [event_id]
                }, "d1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notDestroyed"][event_id].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP delete calendar event failed: {}",
                desc
            )));
        }

        log::info!("JMAP deleted calendar event id={}", event_id);
        Ok(())
    }

    /// Find a mailbox by its JMAP role (inbox, sent, drafts, trash, junk).
    async fn find_mailbox_by_role(
        &self,
        config: &JmapConfig,
        role: &str,
    ) -> Result<Option<String>> {
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Mailbox/get", {
                    "accountId": self.account_id,
                    "properties": ["id", "role"]
                }, "r1"]
            ]
        });
        let resp = self.api_request(&request, config).await?;
        if let Some(mailboxes) = resp["methodResponses"][0][1]["list"].as_array() {
            for mb in mailboxes {
                if mb["role"].as_str() == Some(role) {
                    return Ok(mb["id"].as_str().map(|s| s.to_string()));
                }
            }
        }
        Ok(None)
    }
    /// Create a new mailbox on the JMAP server.
    pub async fn create_mailbox(
        &self,
        config: &JmapConfig,
        name: &str,
        parent_id: Option<&str>,
    ) -> Result<String> {
        log::info!("JMAP creating mailbox: {} (parent={:?})", name, parent_id);
        let create_id = "new-folder";
        let mut mailbox = serde_json::json!({ "name": name });
        if let Some(pid) = parent_id {
            mailbox["parentId"] = serde_json::json!(pid);
        }
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Mailbox/set", {
                    "accountId": self.account_id,
                    "create": {
                        create_id: mailbox
                    }
                }, "c1"]
            ]
        });
        let resp = self.api_request(&request, config).await?;
        let created_id = resp["methodResponses"][0][1]["created"][create_id]["id"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if created_id.is_empty() {
            let err = resp["methodResponses"][0][1]["notCreated"][create_id]["description"]
                .as_str()
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP Mailbox/set create failed: {}",
                err
            )));
        }
        log::info!("JMAP mailbox created: id={}", created_id);
        Ok(created_id)
    }

    pub async fn destroy_mailbox(
        &self,
        config: &JmapConfig,
        mailbox_id: &str,
        remove_messages: bool,
    ) -> Result<()> {
        log::info!(
            "JMAP destroying mailbox: {} (remove_messages={})",
            mailbox_id,
            remove_messages
        );
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Mailbox/set", {
                    "accountId": self.account_id,
                    "onDestroyRemoveEmails": remove_messages,
                    "destroy": [mailbox_id]
                }, "d1"]
            ]
        });
        let resp = self.api_request(&request, config).await?;
        let method_name = resp["methodResponses"][0][0]
            .as_str()
            .unwrap_or("<unknown>");
        if method_name != "Mailbox/set" {
            log::error!("Unexpected JMAP response to mailbox destroy: {}", resp);
            return Err(Error::Other(format!(
                "Unexpected JMAP response to mailbox destroy: {}",
                method_name,
            )));
        }

        let destroyed = resp["methodResponses"][0][1]["destroyed"]
            .as_array()
            .map(|a| a.iter().any(|v| v.as_str() == Some(mailbox_id)))
            .unwrap_or(false);
        if !destroyed {
            let err = resp["methodResponses"][0][1]["notDestroyed"][mailbox_id]["description"]
                .as_str()
                .unwrap_or("Unknown error");
            log::error!("JMAP mailbox destroy failed for {}: {}", mailbox_id, err);
            return Err(Error::Other(format!(
                "JMAP Mailbox/set destroy failed: {}",
                err
            )));
        }
        log::info!("JMAP mailbox destroyed: {}", mailbox_id);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Contacts (JSContact / JMAP Contacts)
    // -----------------------------------------------------------------------

    /// List address books from the JMAP server.
    pub async fn list_address_books(&self, config: &JmapConfig) -> Result<Vec<JmapAddressBook>> {
        log::info!("JMAP listing address books");
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"],
            "methodCalls": [
                ["AddressBook/get", {
                    "accountId": self.account_id,
                }, "ab1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;
        let mut books = Vec::new();

        if let Some(list) = resp["methodResponses"][0][1]["list"].as_array() {
            for ab in list {
                let id = ab["id"].as_str().unwrap_or_default().to_string();
                let name = ab["name"].as_str().unwrap_or("Contacts").to_string();
                let is_default = ab["isDefault"].as_bool().unwrap_or(false);
                books.push(JmapAddressBook {
                    id,
                    name,
                    is_default,
                });
            }
        }

        log::info!("JMAP found {} address books", books.len());
        Ok(books)
    }

    /// Fetch contacts from a JMAP address book.
    pub async fn fetch_contacts(
        &self,
        config: &JmapConfig,
        address_book_id: Option<&str>,
    ) -> Result<Vec<JmapContact>> {
        log::debug!("JMAP fetching contacts (addressBook={:?})", address_book_id);

        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"],
            "methodCalls": [
                ["ContactCard/get", {
                    "accountId": self.account_id,
                }, "c1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;
        log::debug!(
            "JMAP ContactCard response: {}",
            serde_json::to_string(&resp).unwrap_or_default()
        );

        let mut contacts = Vec::new();

        if let Some(list) = resp["methodResponses"][0][1]["list"].as_array() {
            for card in list {
                let id = card["id"].as_str().unwrap_or_default().to_string();
                let uid = card["uid"].as_str().map(|s| s.to_string());

                // Parse name — handles both JSContact formats:
                // 1. {full: "...", given: "...", surname: "..."} (simple)
                // 2. {components: [{kind:"given",value:"..."}, {kind:"surname",value:"..."}]} (Stalwart)
                let display_name = if let Some(name_obj) = card["name"].as_object() {
                    // Try "full" first
                    if let Some(full) = name_obj.get("full").and_then(|f| f.as_str()) {
                        full.to_string()
                    }
                    // Try "components" array (Stalwart JSContact format)
                    else if let Some(components) =
                        name_obj.get("components").and_then(|c| c.as_array())
                    {
                        let mut given = String::new();
                        let mut middle = String::new();
                        let mut surname = String::new();
                        for comp in components {
                            let kind = comp["kind"].as_str().unwrap_or("");
                            let value = comp["value"].as_str().unwrap_or("");
                            match kind {
                                "given" => given = value.to_string(),
                                "given2" | "middle" => middle = value.to_string(),
                                "surname" => surname = value.to_string(),
                                _ => {}
                            }
                        }
                        let parts: Vec<&str> = [given.as_str(), middle.as_str(), surname.as_str()]
                            .into_iter()
                            .filter(|s| !s.is_empty())
                            .collect();
                        if parts.is_empty() {
                            "(No name)".to_string()
                        } else {
                            parts.join(" ")
                        }
                    }
                    // Try direct given/surname
                    else {
                        let given = name_obj.get("given").and_then(|g| g.as_str()).unwrap_or("");
                        let surname = name_obj
                            .get("surname")
                            .and_then(|s| s.as_str())
                            .unwrap_or("");
                        let name = format!("{} {}", given, surname).trim().to_string();
                        if name.is_empty() {
                            "(No name)".to_string()
                        } else {
                            name
                        }
                    }
                } else {
                    "(No name)".to_string()
                };

                // Parse emails
                let mut emails = Vec::new();
                if let Some(emails_obj) = card["emails"].as_object() {
                    for (_key, em) in emails_obj {
                        let addr = em["address"].as_str().unwrap_or_default().to_string();
                        // Try label, then contexts keys, then default to "work"
                        let label = em["label"]
                            .as_str()
                            .map(|s| s.to_string())
                            .or_else(|| {
                                em["contexts"]
                                    .as_object()
                                    .and_then(|c| c.keys().next().cloned())
                            })
                            .unwrap_or_else(|| "work".to_string());
                        if !addr.is_empty() {
                            emails.push(serde_json::json!({"email": addr, "label": label}));
                        }
                    }
                }

                // Parse phones
                let mut phones = Vec::new();
                if let Some(phones_obj) = card["phones"].as_object() {
                    for (_key, ph) in phones_obj {
                        let number = ph["number"].as_str().unwrap_or_default().to_string();
                        let label = ph["label"].as_str().unwrap_or("mobile").to_string();
                        if !number.is_empty() {
                            phones.push(serde_json::json!({"number": number, "label": label}));
                        }
                    }
                }

                // Parse organization
                let organization = card["organizations"]
                    .as_object()
                    .and_then(|orgs| orgs.values().next())
                    .and_then(|org| org["name"].as_str())
                    .map(|s| s.to_string());

                let title = card["titles"]
                    .as_object()
                    .and_then(|titles| titles.values().next())
                    .and_then(|t| t["name"].as_str())
                    .map(|s| s.to_string());

                let notes = card["notes"]
                    .as_object()
                    .and_then(|n| n.values().next())
                    .and_then(|note| note["note"].as_str())
                    .map(|s| s.to_string());

                // Determine which address book this belongs to
                let ab_id = card["addressBookIds"]
                    .as_object()
                    .and_then(|abs| abs.keys().next())
                    .map(|s| s.to_string());

                // Filter by address book if specified
                if let Some(ref target_ab) = address_book_id {
                    if let Some(ref contact_ab) = ab_id {
                        if contact_ab != target_ab {
                            continue;
                        }
                    }
                }

                contacts.push(JmapContact {
                    id,
                    uid,
                    display_name,
                    emails_json: serde_json::to_string(&emails)
                        .unwrap_or_else(|_| "[]".to_string()),
                    phones_json: serde_json::to_string(&phones)
                        .unwrap_or_else(|_| "[]".to_string()),
                    organization,
                    title,
                    notes,
                    address_book_id: ab_id,
                });
            }
        }

        log::info!("JMAP fetched {} contacts", contacts.len());
        Ok(contacts)
    }
    /// Create a contact on the JMAP server. Returns the server-assigned ID.
    pub async fn create_contact_card(
        &self,
        config: &JmapConfig,
        address_book_id: &str,
        display_name: &str,
        emails_json: &str,
        phones_json: &str,
        organization: Option<&str>,
        title: Option<&str>,
        notes: Option<&str>,
    ) -> Result<String> {
        log::info!("JMAP creating contact: '{}'", display_name);

        // Build name components from display_name
        let name_parts: Vec<&str> = display_name.split_whitespace().collect();
        let mut components = Vec::new();
        if let Some(first) = name_parts.first() {
            components.push(serde_json::json!({"kind": "given", "value": first}));
        }
        if name_parts.len() > 2 {
            let middle = name_parts[1..name_parts.len() - 1].join(" ");
            components.push(serde_json::json!({"kind": "given2", "value": middle}));
        }
        if name_parts.len() >= 2 {
            components
                .push(serde_json::json!({"kind": "surname", "value": name_parts.last().unwrap()}));
        }

        let mut card = serde_json::json!({
            "@type": "Card",
            "version": "1.0",
            "name": {
                "components": components,
                "isOrdered": true,
            },
            "addressBookIds": { address_book_id: true },
        });

        // Add emails
        if let Ok(emails) = serde_json::from_str::<Vec<serde_json::Value>>(emails_json) {
            let mut emails_map = serde_json::Map::new();
            for (i, em) in emails.iter().enumerate() {
                let addr = em["email"].as_str().unwrap_or_default();
                if !addr.is_empty() {
                    emails_map.insert(format!("e{}", i), serde_json::json!({"address": addr}));
                }
            }
            if !emails_map.is_empty() {
                card["emails"] = serde_json::Value::Object(emails_map);
            }
        }

        // Add phones
        if let Ok(phones) = serde_json::from_str::<Vec<serde_json::Value>>(phones_json) {
            let mut phones_map = serde_json::Map::new();
            for (i, ph) in phones.iter().enumerate() {
                let number = ph["number"].as_str().unwrap_or_default();
                if !number.is_empty() {
                    phones_map.insert(format!("p{}", i), serde_json::json!({"number": number}));
                }
            }
            if !phones_map.is_empty() {
                card["phones"] = serde_json::Value::Object(phones_map);
            }
        }

        // Add organization
        if let Some(org) = organization {
            if !org.is_empty() {
                card["organizations"] = serde_json::json!({"o0": {"name": org}});
            }
        }

        // Add title
        if let Some(t) = title {
            if !t.is_empty() {
                card["titles"] = serde_json::json!({"t0": {"name": t}});
            }
        }

        // Add notes
        if let Some(n) = notes {
            if !n.is_empty() {
                card["notes"] = serde_json::json!({"n0": {"note": n}});
            }
        }

        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"],
            "methodCalls": [
                ["ContactCard/set", {
                    "accountId": self.account_id,
                    "create": {
                        "new1": card,
                    }
                }, "s1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notCreated"]["new1"].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP create contact failed: {}",
                desc
            )));
        }

        let remote_id = resp["methodResponses"][0][1]["created"]["new1"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        log::info!("JMAP created contact '{}' id={}", display_name, remote_id);
        Ok(remote_id)
    }

    /// Update a contact on the JMAP server via ContactCard/set.
    pub async fn update_contact_card(
        &self,
        config: &JmapConfig,
        remote_id: &str,
        display_name: &str,
        emails_json: &str,
        phones_json: &str,
        organization: Option<&str>,
        title: Option<&str>,
        notes: Option<&str>,
    ) -> Result<()> {
        log::info!("JMAP updating contact: '{}' ({})", display_name, remote_id);

        // Build name components from display_name
        let name_parts: Vec<&str> = display_name.split_whitespace().collect();
        let mut components = Vec::new();
        if let Some(first) = name_parts.first() {
            components.push(serde_json::json!({"kind": "given", "value": first}));
        }
        if name_parts.len() > 2 {
            let middle = name_parts[1..name_parts.len() - 1].join(" ");
            components.push(serde_json::json!({"kind": "given2", "value": middle}));
        }
        if name_parts.len() >= 2 {
            components
                .push(serde_json::json!({"kind": "surname", "value": name_parts.last().unwrap()}));
        }

        let mut updates = serde_json::json!({
            "name": {
                "components": components,
                "isOrdered": true,
            },
        });

        // Emails
        if let Ok(emails) = serde_json::from_str::<Vec<serde_json::Value>>(emails_json) {
            let mut emails_map = serde_json::Map::new();
            for (i, em) in emails.iter().enumerate() {
                let addr = em["email"].as_str().unwrap_or_default();
                if !addr.is_empty() {
                    emails_map.insert(format!("e{}", i), serde_json::json!({"address": addr}));
                }
            }
            updates["emails"] = serde_json::Value::Object(emails_map);
        }

        // Phones
        if let Ok(phones) = serde_json::from_str::<Vec<serde_json::Value>>(phones_json) {
            let mut phones_map = serde_json::Map::new();
            for (i, ph) in phones.iter().enumerate() {
                let number = ph["number"].as_str().unwrap_or_default();
                if !number.is_empty() {
                    phones_map.insert(format!("p{}", i), serde_json::json!({"number": number}));
                }
            }
            updates["phones"] = serde_json::Value::Object(phones_map);
        }

        // Organization
        if let Some(org) = organization.filter(|s| !s.is_empty()) {
            updates["organizations"] = serde_json::json!({"o0": {"name": org}});
        }

        // Title
        if let Some(t) = title.filter(|s| !s.is_empty()) {
            updates["titles"] = serde_json::json!({"t0": {"name": t}});
        }

        // Notes
        if let Some(n) = notes.filter(|s| !s.is_empty()) {
            updates["notes"] = serde_json::json!({"n0": {"note": n}});
        }

        let mut update_map = serde_json::Map::new();
        update_map.insert(remote_id.to_string(), updates);

        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"],
            "methodCalls": [
                ["ContactCard/set", {
                    "accountId": self.account_id,
                    "update": update_map,
                }, "u1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notUpdated"][remote_id].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP update contact failed: {}",
                desc
            )));
        }

        log::info!("JMAP updated contact '{}'", remote_id);
        Ok(())
    }

    /// Delete a contact on the JMAP server via ContactCard/set destroy.
    pub async fn delete_contact_card(&self, config: &JmapConfig, remote_id: &str) -> Result<()> {
        log::info!("JMAP deleting contact: {}", remote_id);

        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"],
            "methodCalls": [
                ["ContactCard/set", {
                    "accountId": self.account_id,
                    "destroy": [remote_id]
                }, "d1"]
            ]
        });

        self.api_request(&request, config).await?;
        log::info!("JMAP deleted contact '{}'", remote_id);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct JmapAddressBook {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug, Clone)]
pub struct JmapContact {
    pub id: String,
    pub uid: Option<String>,
    pub display_name: String,
    pub emails_json: String,
    pub phones_json: String,
    pub organization: Option<String>,
    pub title: Option<String>,
    pub notes: Option<String>,
    pub address_book_id: Option<String>,
}

/// Rewrite an internal URL to go through the HTTPS proxy.
/// e.g., "http://mail.example.com:8080/jmap/foo" → "https://mail.example.com/jmap/foo"
///
/// Only rewrites URLs whose host looks "internal" — loopback, RFC 1918,
/// or a non-standard port (the Stalwart-behind-nginx case ADR 0008 was
/// written for). Public hosts are left alone, otherwise Fastmail's
/// session URLs (apiUrl on `api.fastmail.com`, downloadUrl on
/// `www.fastmail.com` / `www.fastmailusercontent.com`) get force-merged
/// onto the base host and downloads return the Fastmail marketing
/// homepage instead of message bodies.
///
/// Uses simple string manipulation rather than `Url::set_host` so
/// template placeholders like `{accountId}` survive intact.
fn rewrite_url(internal_url: &str, base_url: &str) -> String {
    if !is_internal_url(internal_url) {
        return internal_url.to_string();
    }
    if let Some(scheme_end) = internal_url.find("://") {
        let after_scheme = &internal_url[scheme_end + 3..];
        if let Some(path_start) = after_scheme.find('/') {
            let path_and_query = &after_scheme[path_start..];
            let rewritten = format!("{}{}", base_url.trim_end_matches('/'), path_and_query);
            log::debug!("JMAP URL rewrite: {} → {}", internal_url, rewritten);
            return rewritten;
        }
    }
    internal_url.to_string()
}

/// Heuristic: does this URL point at a private/internal address that
/// would be reachable only from inside a reverse-proxy network?
///
/// `true` for:
///   - any URL with an explicit port (the proxied Stalwart case —
///     `http://host:8080/jmap/`)
///   - any URL whose host is `localhost`
///   - any URL whose host parses as loopback (`127.0.0.0/8`, `::1`),
///     RFC 1918 private IPv4 (`10/8`, `172.16/12`, `192.168/16`),
///     IPv4 link-local (`169.254.0.0/16`, RFC 3927),
///     IPv6 unique-local (`fc00::/7`, RFC 4193),
///     or IPv6 link-local (`fe80::/10`, RFC 4291).
///
/// Link-local addresses are treated as "internal" because a session
/// URL pointing at one only makes sense on the same L2 segment as
/// the server — exactly the deployment shape this rewrite is
/// designed for.
///
/// `false` for everything else (DNS hostnames without ports, on the
/// standard port for the scheme — Fastmail, public Stalwart, etc.).
fn is_internal_url(url_str: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url_str) else {
        return false;
    };
    // Any explicit port → Stalwart-behind-nginx pattern; rewrite.
    if parsed.port().is_some() {
        return true;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
            std::net::IpAddr::V6(v6) => {
                // IPv6: loopback (::1), link-local (fe80::/10),
                // unique-local (fc00::/7). `is_unique_local` is stable.
                v6.is_loopback()
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
            }
        };
    }
    false
}

#[cfg(test)]
mod url_rewrite_tests {
    use super::*;

    #[test]
    fn rewrites_internal_stalwart_url() {
        // ADR 0008 case: stalwart on a private port behind nginx.
        let rewritten = rewrite_url(
            "http://mail.internal:8080/jmap/download/{accountId}/{blobId}/{name}",
            "https://mail.example.com",
        );
        assert_eq!(
            rewritten,
            "https://mail.example.com/jmap/download/{accountId}/{blobId}/{name}"
        );
    }

    #[test]
    fn rewrites_localhost_url() {
        let rewritten = rewrite_url("http://localhost/jmap/api/", "https://mail.example.com");
        assert_eq!(rewritten, "https://mail.example.com/jmap/api/");
    }

    #[test]
    fn rewrites_rfc1918_url() {
        let rewritten = rewrite_url("https://10.0.0.5/jmap/api/", "https://mail.example.com");
        assert_eq!(rewritten, "https://mail.example.com/jmap/api/");
    }

    #[test]
    fn leaves_public_host_alone() {
        // Fastmail: downloadUrl points at a different public host than
        // the session URL, but both are public. Rewriting it onto the
        // base host (api.fastmail.com) routes downloads to a host that
        // returns Fastmail's marketing homepage instead of message
        // bodies — the bug this whole helper exists to prevent.
        let original =
            "https://www.fastmail.com/jmap/download/{accountId}/{blobId}/{name}?type={type}";
        let rewritten = rewrite_url(original, "https://api.fastmail.com");
        assert_eq!(rewritten, original);
    }

    #[test]
    fn leaves_fastmailusercontent_alone() {
        let original =
            "https://www.fastmailusercontent.com/jmap/download/{accountId}/{blobId}/{name}";
        let rewritten = rewrite_url(original, "https://api.fastmail.com");
        assert_eq!(rewritten, original);
    }

    #[test]
    fn leaves_public_host_with_https_no_port_alone() {
        let original = "https://api.example.com/jmap/api/";
        let rewritten = rewrite_url(original, "https://api.example.com");
        assert_eq!(rewritten, original);
    }
}

fn flag_to_keyword(flag: &str) -> &str {
    match flag {
        "seen" => "$seen",
        "flagged" => "$flagged",
        "answered" => "$answered",
        "draft" => "$draft",
        _ => flag,
    }
}

#[derive(Serialize)]
struct AddrJson {
    name: Option<String>,
    email: String,
}

/// Compute end datetime from a start datetime and an ISO 8601 duration string.
/// Handles simple cases like PT1H, PT30M, P1D, PT1H30M, etc.
/// Falls back to start + 1 hour if parsing fails.
fn compute_end_from_duration(start: &str, duration: &str) -> String {
    use chrono::{Duration, NaiveDate, NaiveDateTime};

    let total_seconds = parse_iso8601_duration_seconds(duration);

    // Try parsing as full datetime first, then as date-only
    if let Ok(dt) = NaiveDateTime::parse_from_str(start, "%Y-%m-%dT%H:%M:%S") {
        let end = dt + Duration::seconds(total_seconds);
        return end.format("%Y-%m-%dT%H:%M:%S").to_string();
    }
    if let Ok(d) = NaiveDate::parse_from_str(start, "%Y-%m-%d") {
        let dt = d.and_hms_opt(0, 0, 0).unwrap();
        let end = dt + Duration::seconds(total_seconds);
        if total_seconds % 86400 == 0 {
            return end.format("%Y-%m-%d").to_string();
        }
        return end.format("%Y-%m-%dT%H:%M:%S").to_string();
    }
    // Fallback: return start as-is
    start.to_string()
}

/// Compute an ISO 8601 duration string from start and end datetimes.
/// Returns "P1D" for full-day spans, "PT{n}H" / "PT{n}M" for shorter spans.
fn compute_duration(start: &str, end: &str) -> String {
    use chrono::NaiveDateTime;

    let start_dt = NaiveDateTime::parse_from_str(start, "%Y-%m-%dT%H:%M:%S");
    let end_dt = NaiveDateTime::parse_from_str(end, "%Y-%m-%dT%H:%M:%S");

    if let (Ok(s), Ok(e)) = (start_dt, end_dt) {
        let diff = e - s;
        let total_secs = diff.num_seconds();
        if total_secs <= 0 {
            return "PT1H".to_string();
        }
        let days = total_secs / 86400;
        let remaining = total_secs % 86400;
        let hours = remaining / 3600;
        let minutes = (remaining % 3600) / 60;
        let secs = remaining % 60;

        if remaining == 0 && days > 0 {
            return format!("P{}D", days);
        }
        let mut s = String::from("P");
        if days > 0 {
            s.push_str(&format!("{}D", days));
        }
        s.push('T');
        if hours > 0 {
            s.push_str(&format!("{}H", hours));
        }
        if minutes > 0 {
            s.push_str(&format!("{}M", minutes));
        }
        if secs > 0 {
            s.push_str(&format!("{}S", secs));
        }
        // Ensure we have at least something after 'T'
        if s.ends_with('T') {
            s.push_str("0S");
        }
        return s;
    }
    // Fallback
    "PT1H".to_string()
}

/// Parse a simple ISO 8601 duration like "P1D", "PT1H30M", "PT45M" into total seconds.
fn parse_iso8601_duration_seconds(dur: &str) -> i64 {
    let mut total: i64 = 0;
    let mut num_buf = String::new();
    let mut in_time = false;

    for ch in dur.chars() {
        match ch {
            'P' => {}
            'T' => {
                in_time = true;
            }
            '0'..='9' => {
                num_buf.push(ch);
            }
            'D' => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 86400;
                }
                num_buf.clear();
            }
            'H' if in_time => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 3600;
                }
                num_buf.clear();
            }
            'M' if in_time => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 60;
                }
                num_buf.clear();
            }
            'S' if in_time => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n;
                }
                num_buf.clear();
            }
            'W' => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 604800;
                }
                num_buf.clear();
            }
            _ => {
                num_buf.clear();
            }
        }
    }

    if total == 0 {
        3600
    } else {
        total
    } // default 1 hour
}

fn addresses_to_json(addrs: Option<&Vec<serde_json::Value>>) -> String {
    let list: Vec<AddrJson> = addrs
        .unwrap_or(&vec![])
        .iter()
        .map(|a| AddrJson {
            name: a["name"].as_str().map(|s| s.to_string()),
            email: a["email"].as_str().unwrap_or("").to_string(),
        })
        .collect();
    serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string())
}

fn parse_jmap_search_hit(account_id: &str, e: &serde_json::Value) -> SearchHit {
    let id = e["id"].as_str().unwrap_or("").to_string();
    let subject = e["subject"].as_str().unwrap_or("").to_string();

    let (from_name, from_email) = e["from"]
        .as_array()
        .and_then(|a| a.first())
        .map(|f| {
            (
                f["name"].as_str().map(|s| s.to_string()),
                f["email"].as_str().map(|s| s.to_string()),
            )
        })
        .unwrap_or((None, None));

    let date = e["receivedAt"]
        .as_str()
        .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
        .map(|dt| dt.timestamp())
        .unwrap_or(0);

    let snippet = e["preview"].as_str().map(|s| s.to_string());

    let message_id = e["messageId"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .map(|s| format!("<{}>", s));

    // mailboxIds is an object whose keys are mailbox IDs that are true.
    // Pick the first one as the canonical folder for this hit.
    let folder_path = e["mailboxIds"]
        .as_object()
        .and_then(|m| m.iter().find(|(_, v)| v.as_bool().unwrap_or(false)))
        .map(|(k, _)| k.to_string())
        .unwrap_or_default();

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
