//! JMAP mail domain: `Email/*` and `EmailSubmission/*` methods.

use crate::error::{Error, Result};
use crate::mail::search::build_jmap_filter;
use crate::message::{normalize_message_id, SearchHit, SearchQuery};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};

use super::{JmapConfig, JmapConnection};

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

/// Explicit RFC 8621 submission envelope built from authoritative send data.
///
/// The fields are private so every value passes through [`Self::new`]: there
/// is always one valid RFC 5321 reverse-path and at least one valid forward-
/// path. Display names are discarded because JMAP submission envelopes carry
/// addr-specs only.
#[derive(Serialize)]
pub struct JmapSubmissionEnvelope {
    #[serde(rename = "mailFrom")]
    mail_from: JmapEnvelopeAddress,
    #[serde(rename = "rcptTo")]
    rcpt_to: Vec<JmapEnvelopeAddress>,
    #[serde(skip)]
    mail_from_mailbox: ParsedMailbox,
}

#[derive(Serialize)]
struct JmapEnvelopeAddress {
    email: String,
    parameters: Option<BTreeMap<String, Option<String>>>,
}

pub(super) struct ParsedMailbox {
    addr_spec: String,
    key: MailboxKey,
    unquoted_wildcard: bool,
}

#[derive(PartialEq, Eq, Hash)]
struct MailboxKey {
    local: String,
    domain: String,
}

impl ParsedMailbox {
    pub(super) fn matches(&self, other: &Self) -> bool {
        self.key == other.key
    }

    pub(super) fn is_wildcard_for(&self, other: &Self) -> bool {
        self.unquoted_wildcard && self.key.domain == other.key.domain
    }
}

impl JmapSubmissionEnvelope {
    /// Build an envelope from one sender and the complete To + Cc + Bcc list.
    ///
    /// RFC 5321 section 2.4 requires local-part comparisons to preserve case,
    /// while domains are case-insensitive. Duplicate recipients therefore use
    /// a semantic quoted local-part and canonical IDNA domain key; the first
    /// parsed addr-spec's spelling is retained in the emitted envelope.
    pub fn new(mail_from: &str, to: &[String], cc: &[String], bcc: &[String]) -> Result<Self> {
        let mail_from = parse_rfc5321_addr_spec(mail_from)
            .ok_or_else(|| Error::Other("Invalid JMAP submission mail-from address".into()))?;

        let mut requires_smtputf8 = !mail_from.addr_spec.is_ascii();
        let mut seen = HashSet::new();
        let mut rcpt_to = Vec::with_capacity(to.len() + cc.len() + bcc.len());
        for (index, value) in to.iter().chain(cc).chain(bcc).enumerate() {
            let address = parse_rfc5321_addr_spec(value).ok_or_else(|| {
                // Do not include the value: errors are logged by callers and
                // this position may belong to a confidential Bcc recipient.
                Error::Other(format!(
                    "Invalid JMAP submission recipient at position {}",
                    index + 1
                ))
            })?;
            if seen.insert(address.key) {
                requires_smtputf8 |= !address.addr_spec.is_ascii();
                rcpt_to.push(JmapEnvelopeAddress {
                    email: address.addr_spec,
                    parameters: None,
                });
            }
        }

        if rcpt_to.is_empty() {
            return Err(Error::Other(
                "JMAP submission envelope has no recipients".into(),
            ));
        }

        let parameters = requires_smtputf8.then(|| {
            let mut parameters = BTreeMap::new();
            parameters.insert("SMTPUTF8".into(), None);
            parameters
        });
        Ok(Self {
            mail_from: JmapEnvelopeAddress {
                email: mail_from.addr_spec.clone(),
                parameters,
            },
            rcpt_to,
            mail_from_mailbox: mail_from,
        })
    }

    fn requires_smtputf8(&self) -> bool {
        self.mail_from.parameters.is_some()
    }

    fn requires_smtputf8_for_message(&self, raw_message: &[u8]) -> bool {
        self.requires_smtputf8() || raw_headers_contain_non_ascii(raw_message)
    }

    fn wire_value(&self, raw_message: &[u8]) -> Result<serde_json::Value> {
        let mut value = serde_json::to_value(self)
            .map_err(|_| Error::Other("Failed to serialize JMAP submission envelope".into()))?;
        if self.requires_smtputf8_for_message(raw_message) {
            value["mailFrom"]["parameters"] = serde_json::json!({ "SMTPUTF8": null });
        }
        Ok(value)
    }

    pub(super) fn mail_from_mailbox(&self) -> &ParsedMailbox {
        &self.mail_from_mailbox
    }
}

fn raw_headers_contain_non_ascii(raw_message: &[u8]) -> bool {
    let header_end = raw_message
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .or_else(|| raw_message.windows(2).position(|window| window == b"\n\n"))
        .unwrap_or(raw_message.len());
    !raw_message[..header_end].is_ascii()
}

/// Parse either a bare addr-spec or one display-name mailbox into the exact
/// addr-spec used for RFC 5321 submission. `mailparse` handles mailbox syntax
/// (including quoted display names and quoted local parts inside angle
/// brackets); lettre performs final addr-spec validation and exposes the
/// parsed local/domain components used for standards-compliant deduplication.
pub(super) fn parse_rfc5321_addr_spec(value: &str) -> Option<ParsedMailbox> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    let addr_spec = match value.parse::<lettre::Address>() {
        Ok(address) => address.to_string(),
        Err(_) => mailparse::addrparse(value)
            .ok()
            .and_then(|addresses| addresses.extract_single_info())
            .map(|mailbox| mailbox.addr.trim().to_string())
            .unwrap_or_else(|| value.to_string()),
    };
    let (local, domain) = validated_addr_spec_parts(&addr_spec)?;
    Some(ParsedMailbox {
        addr_spec,
        key: MailboxKey {
            local: semantic_local_part(&local)?,
            domain,
        },
        unquoted_wildcard: local == "*",
    })
}

fn validated_addr_spec_parts(addr_spec: &str) -> Option<(String, String)> {
    if let Ok(address) = addr_spec.parse::<lettre::Address>() {
        return Some((
            address.user().to_string(),
            canonical_domain(address.domain())?,
        ));
    }

    // lettre accepts IPv6 literals without RFC 5321's required `IPv6:` tag.
    // Validate the tagged form by substituting lettre's accepted spelling,
    // but retain the caller's RFC spelling in the serialized envelope.
    let (local, domain) = addr_spec.rsplit_once('@')?;
    let literal = domain.strip_prefix('[')?.strip_suffix(']')?;
    let (tag, address) = literal.split_once(':')?;
    if !tag.eq_ignore_ascii_case("IPv6") {
        return None;
    }
    let address = address.parse::<std::net::Ipv6Addr>().ok()?;
    lettre::Address::new(local, format!("[{address}]")).ok()?;
    Some((local.to_string(), canonical_domain(domain)?))
}

fn semantic_local_part(local: &str) -> Option<String> {
    let Some(inner) = local.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return Some(local.to_string());
    };
    let mut semantic = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            semantic.push(chars.next()?);
        } else {
            semantic.push(ch);
        }
    }
    Some(semantic)
}

fn canonical_domain(domain: &str) -> Option<String> {
    if domain.ends_with('.') {
        return None;
    }

    if let Some(literal) = domain
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
        if let Ok(address) = literal.parse::<std::net::Ipv4Addr>() {
            return Some(format!("[{address}]"));
        }
        let (tag, address) = literal.split_once(':')?;
        if tag.eq_ignore_ascii_case("IPv6") {
            let address = address.parse::<std::net::Ipv6Addr>().ok()?;
            return Some(format!("[ipv6:{address}]"));
        }
        return None;
    }

    match url::Host::parse(domain).ok()? {
        url::Host::Domain(domain) if valid_ascii_dns_domain(&domain) => {
            Some(domain.to_ascii_lowercase())
        }
        // A dotted-quad without brackets is a domain spelling, not an SMTP
        // address literal, so keep its key distinct from `[192.0.2.1]`.
        url::Host::Ipv4(address) => Some(address.to_string()),
        _ => None,
    }
}

fn valid_ascii_dns_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
}

#[derive(Debug, Clone, Copy)]
enum DeltaSyncFallbackReason {
    CannotCalculateChanges,
    InvalidArguments,
    ExceededPageCap,
    StalledState,
}

impl DeltaSyncFallbackReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::CannotCalculateChanges => "cannotCalculateChanges",
            Self::InvalidArguments => "invalidArguments",
            Self::ExceededPageCap => "Email/changes exceeded page cap",
            Self::StalledState => "Email/changes returned hasMoreChanges without state advance",
        }
    }
}

#[derive(Debug)]
enum FetchEmailChangesError {
    Fallback(DeltaSyncFallbackReason),
    Fatal(Error),
}

impl JmapConnection {
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
                Err(FetchEmailChangesError::Fallback(reason)) => {
                    log::info!(
                        "JMAP Email/changes could not complete ({}); falling back to full sync of {}",
                        reason.as_str(),
                        mailbox_id
                    );
                }
                Err(FetchEmailChangesError::Fatal(e)) => return Err(e),
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
    ) -> std::result::Result<JmapFetchResult, FetchEmailChangesError> {
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

            let resp = self
                .api_request(&request, config)
                .await
                .map_err(FetchEmailChangesError::Fatal)?;

            // Surface server errors (cannotCalculateChanges, invalidArguments, …)
            // so the caller can decide whether to fall back to a full sync.
            if let Some(err_type) = resp["methodResponses"][0][1]["type"].as_str() {
                let fallback_reason = match err_type {
                    "cannotCalculateChanges" => {
                        Some(DeltaSyncFallbackReason::CannotCalculateChanges)
                    }
                    "invalidArguments" => Some(DeltaSyncFallbackReason::InvalidArguments),
                    _ => None,
                };
                if let Some(reason) = fallback_reason {
                    return Err(FetchEmailChangesError::Fallback(reason));
                }
                return Err(FetchEmailChangesError::Fatal(Error::Other(format!(
                    "Email/changes error: {}",
                    err_type
                ))));
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
                return Err(FetchEmailChangesError::Fallback(
                    DeltaSyncFallbackReason::ExceededPageCap,
                ));
            }

            // Guard against a server that returns hasMoreChanges: true
            // without advancing state — would loop forever otherwise.
            if new_state == cursor {
                return Err(FetchEmailChangesError::Fallback(
                    DeltaSyncFallbackReason::StalledState,
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

        let response = self.api_request(&request, config).await?;
        validate_set_flags_response(&response)
    }

    pub async fn delete_emails(&self, config: &JmapConfig, email_ids: &[String]) -> Result<()> {
        log::debug!("JMAP deleting {} emails", email_ids.len());
        for (index, chunk) in delete_chunks(email_ids, self.max_objects_in_set).enumerate() {
            let request = serde_json::json!({
                "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
                "methodCalls": [
                    ["Email/set", {
                        "accountId": self.account_id,
                        "destroy": chunk
                    }, format!("d{}", index + 1)]
                ]
            });
            let response = self.api_request(&request, config).await?;
            validate_delete_response(&response, chunk)?;
        }
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
        for (index, chunk) in delete_chunks(email_ids, self.max_objects_in_set).enumerate() {
            let mut update = serde_json::Map::new();
            for id in chunk {
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
                    }, format!("mv{}", index + 1)]
                ]
            });
            let response = self.api_request(&request, config).await?;
            validate_move_response(&response, chunk)?;
        }
        Ok(())
    }

    /// Add a destination mailbox membership without removing existing ones.
    pub async fn copy_emails(
        &self,
        config: &JmapConfig,
        email_ids: &[String],
        to_mailbox: &str,
    ) -> Result<()> {
        log::debug!("JMAP copying {} emails to {}", email_ids.len(), to_mailbox);
        for (index, chunk) in delete_chunks(email_ids, self.max_objects_in_set).enumerate() {
            let update = copy_updates(chunk, to_mailbox);
            let request = serde_json::json!({
                "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
                "methodCalls": [
                    ["Email/set", {
                        "accountId": self.account_id,
                        "update": update
                    }, format!("cp{}", index + 1)]
                ]
            });
            let response = self.api_request(&request, config).await?;
            validate_copy_response(&response, chunk)?;
        }
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
            return Err(Error::Other(format!("JMAP upload error: {}", status)));
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

    async fn submission_api_request(
        &self,
        request: &serde_json::Value,
        config: &JmapConfig,
    ) -> Result<serde_json::Value> {
        let request = config
            .apply_auth(self.submission_http.post(&self.api_url))
            .json(request)
            .build()
            .map_err(|_| Error::Other("JMAP submission request failed before execution".into()))?;
        let response = self
            .submission_http
            .execute(request)
            .await
            .map_err(|error| {
                if error.is_connect() {
                    Error::Other("JMAP submission request failed before execution".into())
                } else {
                    Error::IndeterminateDelivery
                }
            })?;

        if !response.status().is_success() {
            let status = response.status();
            if status.is_client_error() {
                return Err(Error::Other(format!(
                    "JMAP submission request rejected with HTTP status {status}"
                )));
            }
            // A redirect proves the original endpoint received the POST, and
            // a gateway/server error can follow upstream acceptance. Neither
            // response class proves non-delivery.
            return Err(Error::IndeterminateDelivery);
        }

        response
            .json()
            .await
            .map_err(|_| Error::IndeterminateDelivery)
    }

    pub async fn send_email(
        &self,
        config: &JmapConfig,
        raw_message: &[u8],
        envelope: &JmapSubmissionEnvelope,
    ) -> Result<()> {
        log::info!("JMAP sending email ({} bytes)", raw_message.len());

        let wire_envelope = envelope.wire_value(raw_message)?;
        if envelope.requires_smtputf8_for_message(raw_message)
            && !self.supports_submission_extension("SMTPUTF8")
        {
            return Err(Error::Other(
                "JMAP submission requires unsupported SMTPUTF8 extension".into(),
            ));
        }

        // Resolve all server-side prerequisites before upload so an alias or
        // mailbox configuration error cannot leave an orphaned blob.
        let identity_id = self.find_identity_id(config, envelope).await?;
        log::debug!("JMAP selected an identity for submission");

        let sent_mailbox_id = match self.find_mailbox_by_role(config, "sent").await? {
            Some(mailbox_id) => mailbox_id,
            None => self
                .find_mailbox_by_role(config, "inbox")
                .await?
                .ok_or_else(|| Error::Other("No Sent or Inbox mailbox found".into()))?,
        };
        log::debug!("JMAP selected a mailbox for sent email");

        // Upload the raw message as a blob only after validation and lookup.
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
            return Err(Error::Other(format!("JMAP upload error: {}", status)));
        }

        let upload_resp: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("JMAP upload response parse error: {}", e)))?;
        let blob_id = submission_upload_blob_id(&upload_resp, &self.account_id)?;
        log::debug!("JMAP blob uploaded: {}", blob_id);

        // Import the email into the Sent folder and submit it.
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
                            "identityId": identity_id,
                            "envelope": wire_envelope
                        }
                    }
                }, "s1"]
            ]
        });

        let resp = self.submission_api_request(&request, config).await?;
        if let Err(failure) = validate_submission_response(&resp, &self.account_id) {
            if let Some(email_id) = failure.imported_email_id {
                log::warn!("JMAP cleaning up imported email after explicit submission rejection");
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
            return Err(failure.error);
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
            return Err(Error::Other(format!("JMAP draft upload error: {}", status)));
        }

        let upload_resp: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("JMAP draft upload parse error: {}", e)))?;
        let blob_id = submission_upload_blob_id(&upload_resp, &self.account_id)?;

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
        validate_draft_import_response(&resp, &self.account_id)?;

        log::info!("JMAP draft saved successfully");
        Ok(())
    }
}

fn submission_upload_blob_id(
    response: &serde_json::Value,
    expected_account_id: &str,
) -> Result<String> {
    if response
        .get("accountId")
        .and_then(serde_json::Value::as_str)
        != Some(expected_account_id)
    {
        return Err(Error::Other(
            "JMAP upload response did not match the requested account".into(),
        ));
    }
    response
        .get("blobId")
        .and_then(serde_json::Value::as_str)
        .filter(|blob_id| !blob_id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::Other("No blobId in upload response".into()))
}

fn validate_draft_import_response(
    response: &serde_json::Value,
    expected_account_id: &str,
) -> Result<()> {
    match classify_import_response(response, expected_account_id) {
        CreationOutcome::Created(_) => Ok(()),
        CreationOutcome::Rejected(error) => Err(error),
        CreationOutcome::Indeterminate => {
            Err(Error::Other("Malformed JMAP draft import response".into()))
        }
    }
}

struct SubmissionResponseFailure {
    imported_email_id: Option<String>,
    error: Error,
}

enum CreationOutcome<T> {
    Created(T),
    Rejected(Error),
    Indeterminate,
}

fn validate_submission_response(
    response: &serde_json::Value,
    expected_account_id: &str,
) -> std::result::Result<(), SubmissionResponseFailure> {
    let imported = classify_import_response(response, expected_account_id);
    let submitted = classify_email_submission_response(response, expected_account_id);

    match (imported, submitted) {
        (CreationOutcome::Created(_), CreationOutcome::Created(())) => Ok(()),
        (CreationOutcome::Created(email_id), CreationOutcome::Rejected(error)) => {
            Err(SubmissionResponseFailure {
                imported_email_id: Some(email_id),
                error,
            })
        }
        (CreationOutcome::Rejected(error), CreationOutcome::Rejected(_))
        | (CreationOutcome::Rejected(error), CreationOutcome::Indeterminate) => {
            // A submission using `#draft` cannot be created when the import
            // creation itself was explicitly rejected.
            Err(SubmissionResponseFailure {
                imported_email_id: None,
                error,
            })
        }
        (CreationOutcome::Indeterminate, CreationOutcome::Rejected(error)) => {
            // The submission was explicitly rejected, so delivery did not
            // occur even though the import response cannot be trusted.
            Err(SubmissionResponseFailure {
                imported_email_id: None,
                error,
            })
        }
        _ => Err(SubmissionResponseFailure {
            // Never destroy the imported Email unless submission rejection is
            // positively known. Deleting it cannot cancel accepted delivery.
            imported_email_id: None,
            error: Error::IndeterminateDelivery,
        }),
    }
}

fn classify_import_response(
    response: &serde_json::Value,
    expected_account_id: &str,
) -> CreationOutcome<String> {
    let Some((method, body)) = matching_method_response(response, "i1") else {
        return CreationOutcome::Indeterminate;
    };
    if method == "error" {
        return classify_method_error("JMAP Email/import failed", body);
    }
    if method != "Email/import" {
        return CreationOutcome::Indeterminate;
    }
    if !has_expected_account_id(body, expected_account_id) {
        return CreationOutcome::Indeterminate;
    }
    let created = body
        .pointer("/created/draft/id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let rejected = body.pointer("/notCreated/draft");
    match (created, rejected) {
        (Some(email_id), None) => CreationOutcome::Created(email_id),
        (None, Some(error)) => bounded_rejection("JMAP Email/import rejected draft", error)
            .map(CreationOutcome::Rejected)
            .unwrap_or(CreationOutcome::Indeterminate),
        _ => CreationOutcome::Indeterminate,
    }
}

fn classify_email_submission_response(
    response: &serde_json::Value,
    expected_account_id: &str,
) -> CreationOutcome<()> {
    let Some((method, body)) = matching_method_response(response, "s1") else {
        return CreationOutcome::Indeterminate;
    };
    if method == "error" {
        return classify_method_error("JMAP EmailSubmission/set failed", body);
    }
    if method != "EmailSubmission/set" {
        return CreationOutcome::Indeterminate;
    }
    if !has_expected_account_id(body, expected_account_id) {
        return CreationOutcome::Indeterminate;
    }
    let created = body
        .pointer("/created/sub1/id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .is_some();
    let rejected = body.pointer("/notCreated/sub1");
    match (created, rejected) {
        (true, None) => CreationOutcome::Created(()),
        (false, Some(error)) => {
            bounded_rejection("JMAP EmailSubmission/set rejected submission", error)
                .map(CreationOutcome::Rejected)
                .unwrap_or(CreationOutcome::Indeterminate)
        }
        _ => CreationOutcome::Indeterminate,
    }
}

fn has_expected_account_id(body: &serde_json::Value, expected_account_id: &str) -> bool {
    body.get("accountId").and_then(serde_json::Value::as_str) == Some(expected_account_id)
}

fn bounded_rejection(context: &str, error: &serde_json::Value) -> Option<Error> {
    let error_type = super::bounded_jmap_error_type(error)?;
    Some(Error::Other(format!("{context} (type={error_type})")))
}

fn classify_method_error<T>(context: &str, error: &serde_json::Value) -> CreationOutcome<T> {
    let Some(error_type) = super::bounded_jmap_error_type(error) else {
        return CreationOutcome::Indeterminate;
    };
    // RFC 8620 section 3.6.2 permits a serverPartialFail response after
    // some requested changes have occurred, so it never proves rejection.
    if error_type == "serverPartialFail" {
        return CreationOutcome::Indeterminate;
    }
    CreationOutcome::Rejected(Error::Other(format!("{context} (type={error_type})")))
}

fn matching_method_response<'a>(
    response: &'a serde_json::Value,
    call_id: &str,
) -> Option<(&'a str, &'a serde_json::Value)> {
    let responses = response
        .get("methodResponses")
        .and_then(serde_json::Value::as_array)?;
    let mut matched = None;
    for response in responses {
        let Some(tuple) = response.as_array() else {
            continue;
        };
        if tuple.get(2).and_then(serde_json::Value::as_str) != Some(call_id) {
            continue;
        }
        if tuple.len() != 3 || matched.is_some() {
            return None;
        }
        let method = tuple.first().and_then(serde_json::Value::as_str)?;
        matched = Some((method, &tuple[1]));
    }
    matched
}

fn copy_updates(
    email_ids: &[String],
    to_mailbox: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mailbox_token = to_mailbox.replace('~', "~0").replace('/', "~1");
    email_ids
        .iter()
        .map(|id| {
            (
                id.clone(),
                serde_json::json!({
                    format!("mailboxIds/{}", mailbox_token): true,
                }),
            )
        })
        .collect()
}

fn delete_chunks(
    email_ids: &[String],
    max_objects_in_set: usize,
) -> std::slice::Chunks<'_, String> {
    email_ids.chunks(max_objects_in_set.max(1))
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

fn validate_set_flags_response(response: &serde_json::Value) -> Result<()> {
    let method_response = response
        .get("methodResponses")
        .and_then(serde_json::Value::as_array)
        .and_then(|responses| responses.first())
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::Other("JMAP set_flags returned no method response".into()))?;
    let method_name = method_response
        .first()
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let body = method_response.get(1).cloned().unwrap_or_default();
    if method_name == "error" {
        let description = body
            .get("description")
            .or_else(|| body.get("type"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown method error");
        return Err(Error::Other(format!(
            "JMAP set_flags failed: {}",
            description
        )));
    }
    if method_name != "Email/set" {
        return Err(Error::Other(format!(
            "JMAP set_flags returned unexpected method response '{}'",
            method_name
        )));
    }
    if let Some(not_updated) = body
        .get("notUpdated")
        .and_then(serde_json::Value::as_object)
        .filter(|items| !items.is_empty())
    {
        return Err(Error::Other(format!(
            "JMAP set_flags rejected {} message(s): {}",
            not_updated.len(),
            serde_json::Value::Object(not_updated.clone())
        )));
    }
    Ok(())
}

fn validate_delete_response(response: &serde_json::Value, email_ids: &[String]) -> Result<()> {
    let method_response = response
        .get("methodResponses")
        .and_then(serde_json::Value::as_array)
        .and_then(|responses| responses.first())
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::Other("JMAP delete returned no method response".into()))?;
    let method_name = method_response
        .first()
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let body = method_response.get(1).cloned().unwrap_or_default();
    if method_name == "error" {
        let description = body
            .get("description")
            .or_else(|| body.get("type"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown method error");
        return Err(Error::Other(format!("JMAP delete failed: {}", description)));
    }
    if method_name != "Email/set" {
        return Err(Error::Other(format!(
            "JMAP delete returned unexpected method response '{}'",
            method_name
        )));
    }
    let destroyed = body
        .get("destroyed")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let not_destroyed = body
        .get("notDestroyed")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    {
        let failures: serde_json::Map<String, serde_json::Value> = not_destroyed
            .iter()
            .filter(|(_, error)| {
                error.get("type").and_then(serde_json::Value::as_str) != Some("notFound")
            })
            .map(|(id, error)| (id.clone(), error.clone()))
            .collect();
        if !failures.is_empty() {
            return Err(Error::Other(format!(
                "JMAP delete rejected {} message(s): {}",
                failures.len(),
                serde_json::Value::Object(failures)
            )));
        }
    }
    let unreported: Vec<_> = email_ids
        .iter()
        .filter(|email_id| {
            !destroyed
                .iter()
                .any(|destroyed_id| destroyed_id.as_str() == Some(email_id))
                && !not_destroyed.contains_key(*email_id)
        })
        .collect();
    if !unreported.is_empty() {
        return Err(Error::Other(format!(
            "JMAP delete omitted {} message(s) from its response",
            unreported.len()
        )));
    }
    Ok(())
}

fn validate_move_response(response: &serde_json::Value, email_ids: &[String]) -> Result<()> {
    validate_email_update_response(response, email_ids, "move")
}

fn validate_copy_response(response: &serde_json::Value, email_ids: &[String]) -> Result<()> {
    validate_email_update_response(response, email_ids, "copy")
}

fn validate_email_update_response(
    response: &serde_json::Value,
    email_ids: &[String],
    operation: &str,
) -> Result<()> {
    let method_response = response
        .get("methodResponses")
        .and_then(serde_json::Value::as_array)
        .and_then(|responses| responses.first())
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::Other(format!("JMAP {} returned no method response", operation)))?;
    let method_name = method_response
        .first()
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let body = method_response.get(1).cloned().unwrap_or_default();
    if method_name == "error" {
        let description = body
            .get("description")
            .or_else(|| body.get("type"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown method error");
        return Err(Error::Other(format!(
            "JMAP {} failed: {}",
            operation, description
        )));
    }
    if method_name != "Email/set" {
        return Err(Error::Other(format!(
            "JMAP {} returned unexpected method response '{}'",
            operation, method_name
        )));
    }
    let updated = body
        .get("updated")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    let not_updated = body
        .get("notUpdated")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    let failures: serde_json::Map<String, serde_json::Value> = not_updated
        .iter()
        .filter(|(_, error)| {
            error.get("type").and_then(serde_json::Value::as_str) != Some("notFound")
        })
        .map(|(id, error)| (id.clone(), error.clone()))
        .collect();
    if !failures.is_empty() {
        return Err(Error::Other(format!(
            "JMAP {} rejected {} message(s): {}",
            operation,
            failures.len(),
            serde_json::Value::Object(failures)
        )));
    }
    let unreported: Vec<_> = email_ids
        .iter()
        .filter(|email_id| !updated.contains_key(*email_id) && !not_updated.contains_key(*email_id))
        .collect();
    if !unreported.is_empty() {
        return Err(Error::Other(format!(
            "JMAP {} omitted {} message(s) from its response",
            operation,
            unreported.len()
        )));
    }
    Ok(())
}

#[derive(Serialize)]
struct AddrJson {
    name: Option<String>,
    email: String,
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

#[cfg(test)]
mod submission_envelope_tests {
    use super::{JmapConnection, JmapSubmissionEnvelope};
    use crate::mail::jmap::JmapConfig;

    fn envelope_json(
        mail_from: &str,
        to: &[String],
        cc: &[String],
        bcc: &[String],
    ) -> serde_json::Value {
        serde_json::to_value(JmapSubmissionEnvelope::new(mail_from, to, cc, bcc).unwrap()).unwrap()
    }

    #[test]
    fn display_names_and_quoted_local_parts_become_addr_specs() {
        let to = vec![r#""Recipient, Bob" <bob@example.com>"#.into()];
        let cc = vec![r#"Quoted Local <"quoted local"@Example.COM>"#.into()];
        let envelope = envelope_json(r#""Sender, Alice" <Sender@EXAMPLE.com>"#, &to, &cc, &[]);

        assert_eq!(
            envelope,
            serde_json::json!({
                "mailFrom": {
                    "email": "Sender@EXAMPLE.com",
                    "parameters": null
                },
                "rcptTo": [
                    { "email": "bob@example.com", "parameters": null },
                    {
                        "email": "\"quoted local\"@Example.COM",
                        "parameters": null
                    }
                ]
            })
        );
    }

    #[test]
    fn duplicate_domains_ignore_case_and_keep_first_spelling() {
        let to = vec!["Alice <alice@EXAMPLE.com>".into()];
        let cc = vec!["alice@example.com".into()];
        let bcc = vec!["alice@Example.Com".into()];
        let envelope = envelope_json("sender@example.com", &to, &cc, &bcc);

        assert_eq!(
            envelope["rcptTo"],
            serde_json::json!([{
                "email": "alice@EXAMPLE.com",
                "parameters": null
            }])
        );
    }

    #[test]
    fn quoted_and_escaped_equivalents_dedupe_without_rewriting_first() {
        let to = vec![r#""ali\ce"@EXAMPLE.com"#.into()];
        let cc = vec!["alice@example.com".into(), r#""alice"@example.com"#.into()];
        let envelope = envelope_json("sender@example.com", &to, &cc, &[]);

        assert_eq!(
            envelope["rcptTo"],
            serde_json::json!([{
                "email": "\"ali\\ce\"@EXAMPLE.com",
                "parameters": null
            }])
        );
    }

    #[test]
    fn idna_equivalent_domains_dedupe_and_smtputf8_is_mail_from_only() {
        let to = vec!["alice@bücher.example".into()];
        let cc = vec!["alice@xn--bcher-kva.example".into()];
        let envelope = envelope_json("sender@example.com", &to, &cc, &[]);

        assert_eq!(
            envelope,
            serde_json::json!({
                "mailFrom": {
                    "email": "sender@example.com",
                    "parameters": { "SMTPUTF8": null }
                },
                "rcptTo": [{
                    "email": "alice@bücher.example",
                    "parameters": null
                }]
            })
        );
    }

    #[test]
    fn discarded_unicode_duplicate_does_not_require_smtputf8() {
        let to = vec!["alice@xn--bcher-kva.example".into()];
        let bcc = vec!["alice@bücher.example".into()];
        let envelope = envelope_json("sender@example.com", &to, &[], &bcc);

        assert_eq!(envelope["mailFrom"]["parameters"], serde_json::Value::Null);
        assert_eq!(
            envelope["rcptTo"],
            serde_json::json!([{
                "email": "alice@xn--bcher-kva.example",
                "parameters": null
            }])
        );
    }

    #[test]
    fn utf8_headers_require_smtputf8_but_utf8_body_does_not() {
        let envelope = JmapSubmissionEnvelope::new(
            "sender@example.com",
            &["recipient@example.com".into()],
            &[],
            &[],
        )
        .unwrap();

        let utf8_header = envelope
            .wire_value("To: álïce@example.com\r\n\r\nbody".as_bytes())
            .unwrap();
        assert_eq!(
            utf8_header["mailFrom"]["parameters"],
            serde_json::json!({ "SMTPUTF8": null })
        );

        let utf8_body = envelope
            .wire_value("To: alice@example.com\r\n\r\nálïce".as_bytes())
            .unwrap();
        assert_eq!(utf8_body["mailFrom"]["parameters"], serde_json::Value::Null);
    }

    #[test]
    fn rfc_address_literals_are_valid_and_untagged_ipv6_is_rejected() {
        let to = vec!["ipv4@[192.0.2.1]".into(), "ipv6@[IPv6:2001:db8::1]".into()];
        let envelope = envelope_json("sender@example.com", &to, &[], &[]);

        assert_eq!(
            envelope["rcptTo"],
            serde_json::json!([
                { "email": "ipv4@[192.0.2.1]", "parameters": null },
                {
                    "email": "ipv6@[IPv6:2001:db8::1]",
                    "parameters": null
                }
            ])
        );

        assert!(JmapSubmissionEnvelope::new(
            "sender@example.com",
            &["ipv6@[2001:db8::1]".into()],
            &[],
            &[],
        )
        .is_err());
    }

    #[test]
    fn local_part_case_variants_remain_distinct() {
        let to = vec!["Alice@example.com".into(), "alice@EXAMPLE.com".into()];
        let envelope = envelope_json("sender@example.com", &to, &[], &[]);

        assert_eq!(
            envelope["rcptTo"],
            serde_json::json!([
                { "email": "Alice@example.com", "parameters": null },
                { "email": "alice@EXAMPLE.com", "parameters": null }
            ])
        );
    }

    #[test]
    fn invalid_or_empty_envelopes_fail_before_upload_without_exposing_recipient() {
        // `send_email` requires this private-field type, so a constructor
        // failure occurs before its first operation (the blob upload).
        assert!(JmapSubmissionEnvelope::new(
            "invalid sender",
            &["recipient@example.com".into()],
            &[],
            &[]
        )
        .is_err());

        let confidential = "confidential bcc is invalid";
        let error = match JmapSubmissionEnvelope::new(
            "sender@example.com",
            &[],
            &[],
            &[confidential.into()],
        ) {
            Ok(_) => panic!("invalid recipient must be rejected"),
            Err(error) => error.to_string(),
        };
        assert!(!error.contains(confidential));

        assert!(JmapSubmissionEnvelope::new("sender@example.com", &[], &[], &[]).is_err());
    }

    #[tokio::test]
    async fn unsupported_smtputf8_fails_before_any_upload() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(1))
            .build()
            .unwrap();
        let submission_http = http.clone();
        let connection = JmapConnection {
            http,
            submission_http,
            api_url: format!("{base_url}/api"),
            download_url_template: format!("{base_url}/download/{{blobId}}"),
            upload_url_template: format!("{base_url}/upload/{{accountId}}"),
            event_source_url_template: None,
            account_id: "account-1".into(),
            max_objects_in_set: 500,
            submission_extensions: std::collections::HashMap::new(),
        };
        let config = JmapConfig {
            jmap_url: base_url,
            email: "sender@example.com".into(),
            username: "sender".into(),
            password: "password".into(),
            access_token: None,
            auth_method: "basic".into(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
        };
        let envelope = JmapSubmissionEnvelope::new(
            "sender@example.com",
            &["recipient@bücher.example".into()],
            &[],
            &[],
        )
        .unwrap();

        let error = connection
            .send_email(&config, b"opaque MIME bytes", &envelope)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("SMTPUTF8"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "SMTPUTF8 rejection must occur before any network request"
        );
    }
}

#[cfg(test)]
mod submission_response_tests {
    use super::validate_submission_response;

    const ACCOUNT_ID: &str = "account-1";

    fn successful_response() -> serde_json::Value {
        serde_json::json!({
            "methodResponses": [
                ["Email/import", {
                    "accountId": ACCOUNT_ID,
                    "created": { "draft": { "id": "email-1" } }
                }, "i1"],
                ["EmailSubmission/set", {
                    "accountId": ACCOUNT_ID,
                    "created": { "sub1": { "id": "submission-1" } }
                }, "s1"]
            ]
        })
    }

    #[test]
    fn requires_positive_import_and_submission_creation() {
        assert!(validate_submission_response(&successful_response(), ACCOUNT_ID).is_ok());

        for response in [
            serde_json::json!({
                "methodResponses": [["Email/import", {
                    "accountId": ACCOUNT_ID,
                    "created": { "draft": { "id": "email-1" } }
                }, "i1"]]
            }),
            serde_json::json!({
                "methodResponses": [
                    ["Email/import", {
                        "accountId": ACCOUNT_ID,
                        "created": { "draft": { "id": "email-1" } }
                    }, "i1"],
                    ["EmailSubmission/set", {
                        "accountId": ACCOUNT_ID,
                        "created": { "sub1": {} }
                    }, "s1"]
                ]
            }),
            serde_json::json!({
                "methodResponses": [
                    ["Email/import", {
                        "accountId": ACCOUNT_ID,
                        "created": { "draft": { "id": "email-1" } }
                    }, "i1"],
                    ["Email/set", {
                        "accountId": ACCOUNT_ID,
                        "created": { "sub1": { "id": "submission-1" } }
                    }, "s1"]
                ]
            }),
            serde_json::json!({
                "methodResponses": [
                    ["Email/import", {
                        "accountId": ACCOUNT_ID,
                        "created": { "draft": { "id": "email-1" } }
                    }, "i1"],
                    ["EmailSubmission/set", {
                        "accountId": ACCOUNT_ID,
                        "created": { "sub1": { "id": "submission-1" } }
                    }, "wrong-call-id"]
                ]
            }),
        ] {
            let failure = validate_submission_response(&response, ACCOUNT_ID).unwrap_err();
            assert!(failure.imported_email_id.is_none());
            assert!(failure.error.is_indeterminate_delivery());
        }
    }

    #[test]
    fn unrelated_extra_response_does_not_erase_proven_success() {
        let mut response = successful_response();
        response["methodResponses"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!(["Core/echo", {}, "extra"]));

        assert!(validate_submission_response(&response, ACCOUNT_ID).is_ok());
    }

    #[test]
    fn successful_responses_require_the_expected_account_id() {
        for method_index in [0, 1] {
            for account_id in [
                None,
                Some(serde_json::Value::Null),
                Some(serde_json::json!(7)),
                Some(serde_json::json!("other-account")),
            ] {
                let mut response = successful_response();
                let body = response["methodResponses"][method_index][1]
                    .as_object_mut()
                    .unwrap();
                if let Some(account_id) = account_id {
                    body.insert("accountId".into(), account_id);
                } else {
                    body.remove("accountId");
                }

                let failure = validate_submission_response(&response, ACCOUNT_ID).unwrap_err();
                assert!(failure.imported_email_id.is_none());
                assert!(failure.error.is_indeterminate_delivery());
            }
        }
    }

    #[test]
    fn explicit_submission_rejection_allows_import_cleanup() {
        let response = serde_json::json!({
            "methodResponses": [
                ["Email/import", {
                    "accountId": ACCOUNT_ID,
                    "created": { "draft": { "id": "email-1" } }
                }, "i1"],
                ["EmailSubmission/set", {
                    "accountId": ACCOUNT_ID,
                    "notCreated": {
                        "sub1": { "type": "forbiddenToSend" }
                    }
                }, "s1"]
            ]
        });

        let failure = validate_submission_response(&response, ACCOUNT_ID).unwrap_err();
        assert_eq!(failure.imported_email_id.as_deref(), Some("email-1"));
        assert!(!failure.error.is_indeterminate_delivery());
    }

    #[test]
    fn mismatched_rejection_account_is_indeterminate_without_cleanup() {
        let response = serde_json::json!({
            "methodResponses": [
                ["Email/import", {
                    "accountId": ACCOUNT_ID,
                    "created": { "draft": { "id": "email-1" } }
                }, "i1"],
                ["EmailSubmission/set", {
                    "accountId": "other-account",
                    "notCreated": {
                        "sub1": { "type": "forbiddenToSend" }
                    }
                }, "s1"]
            ]
        });

        let failure = validate_submission_response(&response, ACCOUNT_ID).unwrap_err();
        assert!(failure.imported_email_id.is_none());
        assert!(failure.error.is_indeterminate_delivery());
    }

    #[test]
    fn contradictory_or_malformed_rejection_is_indeterminate() {
        for rejected in [
            serde_json::json!({ "type": "forbiddenToSend" }),
            serde_json::json!({ "description": "missing type" }),
        ] {
            let mut response = successful_response();
            response["methodResponses"][1][1]["notCreated"]["sub1"] = rejected;

            let failure = validate_submission_response(&response, ACCOUNT_ID).unwrap_err();
            assert!(failure.imported_email_id.is_none());
            assert!(failure.error.is_indeterminate_delivery());
        }
    }

    #[test]
    fn server_partial_fail_never_permits_cleanup_or_retry() {
        let response = serde_json::json!({
            "methodResponses": [
                ["Email/import", {
                    "accountId": ACCOUNT_ID,
                    "created": { "draft": { "id": "email-1" } }
                }, "i1"],
                ["error", { "type": "serverPartialFail" }, "s1"]
            ]
        });

        let failure = validate_submission_response(&response, ACCOUNT_ID).unwrap_err();
        assert!(failure.imported_email_id.is_none());
        assert!(failure.error.is_indeterminate_delivery());
    }

    #[test]
    fn import_failure_does_not_claim_an_email_for_cleanup() {
        let response = serde_json::json!({
            "methodResponses": [
                ["Email/import", {
                    "accountId": ACCOUNT_ID,
                    "notCreated": {
                        "draft": {
                            "type": "invalidEmail",
                            "description": "contains private message data"
                        }
                    }
                }, "i1"],
                ["EmailSubmission/set", { "accountId": ACCOUNT_ID }, "s1"]
            ]
        });

        let failure = validate_submission_response(&response, ACCOUNT_ID).unwrap_err();
        assert!(failure.imported_email_id.is_none());
        let error = failure.error.to_string();
        assert!(error.contains("invalidEmail"));
        assert!(!error.contains("private message data"));
    }

    #[test]
    fn invalid_recipients_error_is_bounded_and_redacted() {
        let secret = "hidden-recipient@example.test";
        let response = serde_json::json!({
            "methodResponses": [
                ["Email/import", {
                    "accountId": ACCOUNT_ID,
                    "created": { "draft": { "id": "email-1" } }
                }, "i1"],
                ["EmailSubmission/set", {
                    "accountId": ACCOUNT_ID,
                    "notCreated": {
                        "sub1": {
                            "type": "invalidRecipients",
                            "description": format!("recipient rejected: {secret}"),
                            "invalidRecipients": [secret]
                        }
                    }
                }, "s1"]
            ]
        });

        let failure = validate_submission_response(&response, ACCOUNT_ID).unwrap_err();
        assert_eq!(failure.imported_email_id.as_deref(), Some("email-1"));
        let error = failure.error.to_string();
        assert!(error.contains("type=invalidRecipients"));
        assert!(!error.contains(secret));
        assert!(error.len() < 160);
    }
}

#[cfg(test)]
mod upload_and_draft_response_tests {
    use super::{submission_upload_blob_id, validate_draft_import_response};

    const ACCOUNT_ID: &str = "account-1";

    #[test]
    fn submission_upload_requires_the_expected_account_id() {
        let valid = serde_json::json!({
            "accountId": ACCOUNT_ID,
            "blobId": "blob-1"
        });
        assert_eq!(
            submission_upload_blob_id(&valid, ACCOUNT_ID).unwrap(),
            "blob-1"
        );

        for response in [
            serde_json::json!({ "blobId": "private-blob" }),
            serde_json::json!({ "accountId": null, "blobId": "private-blob" }),
            serde_json::json!({ "accountId": 7, "blobId": "private-blob" }),
            serde_json::json!({
                "accountId": "other-account",
                "blobId": "private-blob"
            }),
        ] {
            let error = submission_upload_blob_id(&response, ACCOUNT_ID)
                .unwrap_err()
                .to_string();
            assert!(!error.contains("private-blob"));
            assert!(error.len() < 160);
        }
    }

    #[test]
    fn submission_upload_requires_a_nonempty_blob_id() {
        for response in [
            serde_json::json!({ "accountId": ACCOUNT_ID }),
            serde_json::json!({ "accountId": ACCOUNT_ID, "blobId": null }),
            serde_json::json!({ "accountId": ACCOUNT_ID, "blobId": "" }),
        ] {
            assert!(submission_upload_blob_id(&response, ACCOUNT_ID).is_err());
        }
    }

    #[test]
    fn draft_import_requires_correlated_creation_for_the_expected_account() {
        let valid = serde_json::json!({
            "methodResponses": [["Email/import", {
                "accountId": ACCOUNT_ID,
                "created": { "draft": { "id": "email-1" } }
            }, "i1"]]
        });
        assert!(validate_draft_import_response(&valid, ACCOUNT_ID).is_ok());

        let secret = "private draft response";
        for response in [
            serde_json::json!({}),
            serde_json::json!({
                "methodResponses": [["Email/import", {
                    "accountId": ACCOUNT_ID,
                    "created": { "draft": { "id": "email-1" } }
                }, "wrong"]]
            }),
            serde_json::json!({
                "methodResponses": [["Email/get", {
                    "accountId": ACCOUNT_ID,
                    "created": { "draft": { "id": "email-1" } }
                }, "i1"]]
            }),
            serde_json::json!({
                "methodResponses": [["Email/import", {
                    "created": { "draft": { "id": "email-1" } }
                }, "i1"]]
            }),
            serde_json::json!({
                "methodResponses": [["Email/import", {
                    "accountId": null,
                    "created": { "draft": { "id": "email-1" } }
                }, "i1"]]
            }),
            serde_json::json!({
                "methodResponses": [["Email/import", {
                    "accountId": "other-account",
                    "created": { "draft": { "id": "email-1" } }
                }, "i1"]]
            }),
            serde_json::json!({
                "methodResponses": [["Email/import", {
                    "accountId": ACCOUNT_ID,
                    "created": { "draft": {} }
                }, "i1"]]
            }),
            serde_json::json!({
                "methodResponses": [["Email/import", {
                    "accountId": ACCOUNT_ID,
                    "notImported": {
                        "draft": {
                            "type": "invalidEmail",
                            "description": secret
                        }
                    }
                }, "i1"]]
            }),
        ] {
            let error = validate_draft_import_response(&response, ACCOUNT_ID)
                .unwrap_err()
                .to_string();
            assert!(!error.contains(secret));
            assert!(error.len() < 160);
        }
    }

    #[test]
    fn draft_import_rejection_and_method_error_are_bounded_and_redacted() {
        let secret = "private draft response";
        for response in [
            serde_json::json!({
                "methodResponses": [["Email/import", {
                    "accountId": ACCOUNT_ID,
                    "notCreated": {
                        "draft": {
                            "type": "invalidEmail",
                            "description": secret
                        }
                    }
                }, "i1"]]
            }),
            serde_json::json!({
                "methodResponses": [["error", {
                    "type": "serverFail",
                    "description": secret
                }, "i1"]]
            }),
        ] {
            let error = validate_draft_import_response(&response, ACCOUNT_ID)
                .unwrap_err()
                .to_string();
            assert!(!error.contains(secret));
            assert!(error.len() < 160);
        }
    }
}

#[cfg(test)]
mod submission_request_tests {
    use super::{JmapConfig, JmapConnection};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn connection(api_url: String) -> JmapConnection {
        let submission_http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(1))
            .build()
            .unwrap();
        connection_with_submission_http(api_url, submission_http)
    }

    fn connection_with_submission_http(
        api_url: String,
        submission_http: reqwest::Client,
    ) -> JmapConnection {
        JmapConnection {
            http: reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(1))
                .build()
                .unwrap(),
            submission_http,
            api_url,
            download_url_template: String::new(),
            upload_url_template: String::new(),
            event_source_url_template: None,
            account_id: "account-1".into(),
            max_objects_in_set: 500,
            submission_extensions: std::collections::HashMap::new(),
        }
    }

    fn config() -> JmapConfig {
        JmapConfig {
            jmap_url: String::new(),
            email: "sender@example.test".into(),
            username: "sender".into(),
            password: "password".into(),
            access_token: None,
            auth_method: "basic".into(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
        }
    }

    async fn serve_once(status: &str, body: &str) -> String {
        serve_once_with_headers(status, "", body).await
    }

    async fn serve_once_with_headers(status: &str, headers: &str, body: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api", listener.local_addr().unwrap());
        let status = status.to_string();
        let headers = headers.to_string();
        let body = body.to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0, "request ended before its body was complete");
                request.extend_from_slice(&chunk[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 {status}\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        url
    }

    async fn serve_redirect_once(status: u16, location: &str) -> String {
        serve_once_with_headers(
            &format!("{status} Redirect"),
            &format!("Location: {location}\r\n"),
            "",
        )
        .await
    }

    #[tokio::test]
    async fn http_rejection_is_definite_but_invalid_success_body_is_indeterminate() {
        let request = serde_json::json!({ "methodCalls": [] });

        let rejected_url = serve_once("401 Unauthorized", "private response").await;
        let rejected = connection(rejected_url)
            .submission_api_request(&request, &config())
            .await
            .unwrap_err();
        assert!(!rejected.is_indeterminate_delivery());
        assert_eq!(
            rejected.to_string(),
            "JMAP submission request rejected with HTTP status 401 Unauthorized"
        );
        assert!(!rejected.to_string().contains("private response"));

        let malformed_url = serve_once("200 OK", "private non-json response").await;
        let malformed = connection(malformed_url)
            .submission_api_request(&request, &config())
            .await
            .unwrap_err();
        assert!(malformed.is_indeterminate_delivery());
        assert!(!malformed.to_string().contains("private non-json response"));

        let gateway_url = serve_once("502 Bad Gateway", "private gateway response").await;
        let gateway = connection(gateway_url)
            .submission_api_request(&request, &config())
            .await
            .unwrap_err();
        assert!(gateway.is_indeterminate_delivery());
        assert!(!gateway.to_string().contains("private gateway response"));
    }

    #[tokio::test]
    async fn request_builder_failure_is_definite() {
        let error = connection("://invalid submission URL".into())
            .submission_api_request(&serde_json::json!({ "methodCalls": [] }), &config())
            .await
            .unwrap_err();
        assert!(!error.is_indeterminate_delivery());
    }

    #[tokio::test]
    async fn connection_failure_is_definite() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api", listener.local_addr().unwrap());
        drop(listener);

        let error = connection(url)
            .submission_api_request(&serde_json::json!({ "methodCalls": [] }), &config())
            .await
            .unwrap_err();
        assert!(!error.is_indeterminate_delivery());
    }

    #[tokio::test]
    async fn submission_redirects_are_indeterminate_and_not_followed() {
        let request = serde_json::json!({ "methodCalls": [] });
        for status in [301, 302, 303, 307, 308] {
            let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target_url = format!("http://{}/redirected", target.local_addr().unwrap());
            let origin_url = serve_redirect_once(status, &target_url).await;

            let error = connection(origin_url)
                .submission_api_request(&request, &config())
                .await
                .unwrap_err();
            assert!(error.is_indeterminate_delivery(), "HTTP {status}");
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), target.accept())
                    .await
                    .is_err(),
                "HTTP {status} submission redirect was followed"
            );
        }
    }

    #[tokio::test]
    async fn every_http_3xx_submission_response_is_indeterminate() {
        let request = serde_json::json!({ "methodCalls": [] });
        for status in 300..=399 {
            let url = serve_once(&format!("{status} Redirect"), "").await;
            let error = connection(url)
                .submission_api_request(&request, &config())
                .await
                .unwrap_err();
            assert!(error.is_indeterminate_delivery(), "HTTP {status}");
        }
    }

    #[tokio::test]
    async fn redirect_error_after_dispatch_is_indeterminate() {
        let submission_http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::limited(0))
            .timeout(std::time::Duration::from_secs(1))
            .build()
            .unwrap();
        let url = serve_redirect_once(307, "http://127.0.0.1:9/redirected").await;

        let error = connection_with_submission_http(url, submission_http)
            .submission_api_request(&serde_json::json!({ "methodCalls": [] }), &config())
            .await
            .unwrap_err();
        assert!(error.is_indeterminate_delivery());
    }
}

#[cfg(test)]
mod set_flags_tests {
    use super::{
        copy_updates, delete_chunks, validate_copy_response, validate_delete_response,
        validate_move_response, validate_set_flags_response,
    };

    #[test]
    fn accepts_successful_email_set() {
        let response = serde_json::json!({
            "methodResponses": [["Email/set", { "updated": { "id": null } }, "s1"]]
        });
        assert!(validate_set_flags_response(&response).is_ok());
    }

    #[test]
    fn rejects_not_updated_and_method_errors() {
        let not_updated = serde_json::json!({
            "methodResponses": [["Email/set", {
                "notUpdated": { "id": { "type": "notFound" } }
            }, "s1"]]
        });
        assert!(validate_set_flags_response(&not_updated).is_err());

        let method_error = serde_json::json!({
            "methodResponses": [["error", {
                "type": "serverFail",
                "description": "temporary failure"
            }, "s1"]]
        });
        assert!(validate_set_flags_response(&method_error).is_err());
    }

    #[test]
    fn delete_accepts_destroyed_and_not_found_messages() {
        let ids = vec!["id".to_string()];
        let destroyed = serde_json::json!({
            "methodResponses": [["Email/set", { "destroyed": ["id"] }, "d1"]]
        });
        assert!(validate_delete_response(&destroyed, &ids).is_ok());

        let not_found = serde_json::json!({
            "methodResponses": [["Email/set", {
                "notDestroyed": { "id": { "type": "notFound" } }
            }, "d1"]]
        });
        assert!(validate_delete_response(&not_found, &ids).is_ok());
    }

    #[test]
    fn delete_chunks_respect_the_session_limit() {
        let ids: Vec<String> = (0..5).map(|id| id.to_string()).collect();
        let sizes: Vec<usize> = delete_chunks(&ids, 2).map(<[_]>::len).collect();
        assert_eq!(sizes, vec![2, 2, 1]);
    }

    #[test]
    fn delete_rejects_item_and_method_errors() {
        let ids = vec!["id".to_string()];
        let forbidden = serde_json::json!({
            "methodResponses": [["Email/set", {
                "notDestroyed": { "id": { "type": "forbidden" } }
            }, "d1"]]
        });
        assert!(validate_delete_response(&forbidden, &ids).is_err());

        let method_error = serde_json::json!({
            "methodResponses": [["error", { "type": "serverFail" }, "d1"]]
        });
        assert!(validate_delete_response(&method_error, &ids).is_err());

        let omitted = serde_json::json!({
            "methodResponses": [["Email/set", {}, "d1"]]
        });
        assert!(validate_delete_response(&omitted, &ids).is_err());
    }

    #[test]
    fn move_accepts_updated_and_not_found_messages() {
        let ids = vec!["id".to_string()];
        let updated = serde_json::json!({
            "methodResponses": [["Email/set", { "updated": { "id": null } }, "mv1"]]
        });
        assert!(validate_move_response(&updated, &ids).is_ok());

        let not_found = serde_json::json!({
            "methodResponses": [["Email/set", {
                "notUpdated": { "id": { "type": "notFound" } }
            }, "mv1"]]
        });
        assert!(validate_move_response(&not_found, &ids).is_ok());
    }

    #[test]
    fn move_rejects_item_method_and_omission_errors() {
        let ids = vec!["id".to_string()];
        let forbidden = serde_json::json!({
            "methodResponses": [["Email/set", {
                "notUpdated": { "id": { "type": "forbidden" } }
            }, "mv1"]]
        });
        assert!(validate_move_response(&forbidden, &ids).is_err());

        let method_error = serde_json::json!({
            "methodResponses": [["error", { "type": "serverFail" }, "mv1"]]
        });
        assert!(validate_move_response(&method_error, &ids).is_err());

        let omitted = serde_json::json!({
            "methodResponses": [["Email/set", {}, "mv1"]]
        });
        assert!(validate_move_response(&omitted, &ids).is_err());
    }

    #[test]
    fn copy_adds_only_the_destination_mailbox_membership() {
        let updates = copy_updates(&["email_1".into()], "archive/one~two");
        assert_eq!(
            updates["email_1"],
            serde_json::json!({ "mailboxIds/archive~1one~0two": true })
        );
    }

    #[test]
    fn copy_accepts_updated_and_not_found_messages() {
        let ids = vec!["id".to_string()];
        let updated = serde_json::json!({
            "methodResponses": [["Email/set", { "updated": { "id": null } }, "cp1"]]
        });
        assert!(validate_copy_response(&updated, &ids).is_ok());

        let not_found = serde_json::json!({
            "methodResponses": [["Email/set", {
                "notUpdated": { "id": { "type": "notFound" } }
            }, "cp1"]]
        });
        assert!(validate_copy_response(&not_found, &ids).is_ok());
    }

    #[test]
    fn copy_rejects_item_method_and_omission_errors() {
        let ids = vec!["id".to_string()];
        let forbidden = serde_json::json!({
            "methodResponses": [["Email/set", {
                "notUpdated": { "id": { "type": "forbidden" } }
            }, "cp1"]]
        });
        assert!(validate_copy_response(&forbidden, &ids).is_err());

        let method_error = serde_json::json!({
            "methodResponses": [["error", { "type": "serverFail" }, "cp1"]]
        });
        assert!(validate_copy_response(&method_error, &ids).is_err());

        let omitted = serde_json::json!({
            "methodResponses": [["Email/set", {}, "cp1"]]
        });
        assert!(validate_copy_response(&omitted, &ids).is_err());
    }
}
