//! JMAP protocol client, split by domain.
//!
//! This module owns the shared transport core: [`JmapConfig`] (credential
//! resolution and per-request auth) and [`JmapConnection`] (session URLs and
//! the private `api_request` primitive every domain call goes through).
//! Domain methods live in child modules as additional `impl JmapConnection`
//! blocks:
//!
//! - [`mail`]: messages — fetch/search/flags/move/delete/import, send, drafts
//! - [`mailboxes`]: `Mailbox/*` — folder listing, create/destroy, role lookup
//! - [`identities`]: `Identity/*` — sending-identity lookup
//! - [`calendar`]: `Calendar/*` + `CalendarEvent/*` (RFC 8984 JSCalendar)
//! - [`contacts`]: `AddressBook/*` + `ContactCard/*` (RFC 9553 JSContact)
//!
//! `api_request` and `JmapConfig::apply_auth` are private to this tree;
//! callers outside `mail::jmap` always go through the typed domain methods.

use crate::error::{Error, Result};
use serde::Deserialize;

mod calendar;
mod contacts;
mod identities;
mod mail;
mod mailboxes;

pub use calendar::JmapCalendarEvent;
pub use mail::{JmapEmail, JmapFetchResult};

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
        // Fail fast if bearer/OIDC was selected but no access token
        // resolved. Without this guard, apply_auth() would silently fall
        // through to HTTP Basic with an empty password, the server would
        // 401, and the user would see a generic auth failure instead of
        // "your credential is missing". bearer_auth("") would otherwise
        // emit an "Authorization: Bearer " header with no value, which
        // is equally useless.
        //
        // The error message uses "access token" — accurate for both
        // bearer mode (Fastmail API token used as Bearer) and OIDC
        // (refreshed OIDC access token). The credential-source hint
        // differs by mode so the user knows where to look.
        if (config.auth_method == "bearer" || config.auth_method == "oidc")
            && config.access_token.as_deref().unwrap_or("").is_empty()
        {
            let source_hint = if config.auth_method == "bearer" {
                "the API token in the keyring is missing or empty"
            } else {
                "the OIDC token refresh did not return an access token"
            };
            return Err(Error::Other(format!(
                "JMAP {} mode is selected but no access token is available — {}",
                config.auth_method, source_hint,
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

        // Diagnostic: enough to tell which auth mode is in play and to
        // spot truncated/empty credentials without leaking any part of
        // them. The auth mode comes from `auth_method` directly (not
        // inferred from `access_token.is_some()`, which would misreport
        // OIDC as "bearer"). Length is the post-trim credential the
        // request will actually send, and is logged uniformly as
        // `credential_len` for all auth methods.
        let credential_len = if config.auth_method == "basic" {
            config.password.len()
        } else {
            config.access_token.as_deref().map(str::len).unwrap_or(0)
        };
        log::info!(
            "JMAP connecting to {} as {} [auth={} credential_len={}]",
            base_url,
            config.username,
            config.auth_method,
            credential_len,
        );

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
