//! JMAP mail domain: `Email/*` and `EmailSubmission/*` methods.

use crate::error::{Error, Result};
use crate::mail::search::build_jmap_filter;
use crate::message::{normalize_message_id, SearchHit, SearchQuery};
use serde::Serialize;

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
