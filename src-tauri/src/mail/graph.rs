//! Microsoft Graph API client for O365 mail, calendar, and contacts.
//!
//! All operations go through `https://graph.microsoft.com/v1.0` with
//! Bearer token authentication. No IMAP/SMTP needed for O365 accounts.

use crate::error::{Error, Result};
use crate::mail::msgid::normalize_message_id;
use crate::mail::search::{build_graph_kql, SearchHit, SearchQuery};
use serde::{Deserialize, Serialize};

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
const GRAPH_BETA_BASE: &str = "https://graph.microsoft.com/beta";

/// Graph JSON batching allows at most 20 sub-requests per `$batch` call.
const BATCH_SIZE: usize = 20;

/// Fold a `$batch` response into per-item results. Sub-responses are keyed
/// by the request "id" we set to the item's global index; they may arrive
/// out of order.
///
/// A batch can return outer HTTP 200 while individual sub-responses are
/// throttled (429) or transient (503/504). Those items are NOT written
/// into `results`; they come back as `(index, retry_after_secs)` so the
/// caller can retry just the affected sub-requests after the per-item
/// delay. Everything else is recorded as a final outcome.
fn apply_batch_responses(
    resp: &serde_json::Value,
    results: &mut [Result<()>],
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
        } else if matches!(status, 429 | 503 | 504) {
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

// ---------------------------------------------------------------------------
// Graph client
// ---------------------------------------------------------------------------

pub struct GraphClient {
    http: reqwest::Client,
    access_token: String,
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

impl GraphClient {
    pub fn new(access_token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            access_token: access_token.to_string(),
        }
    }

    /// Send a request, retrying on throttled (429) and — for idempotent
    /// requests only — transient (503/504) responses, honoring
    /// `Retry-After`. Exchange throttles mailbox access aggressively;
    /// without this every 429 aborted the whole account sync and the
    /// retry storm made the throttling worse.
    ///
    /// `retry_transient` must be `false` for non-idempotent requests
    /// (POSTs like `/sendMail`, resource creation, `$batch` with moves):
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
        let url = format!("{}{}", GRAPH_BASE, path);
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
        let url = format!("{}{}", GRAPH_BETA_BASE, path);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.access_token)
            .query(params)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Graph beta GET {} failed: {}", path, e)))?;

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
        let resp = self
            .send_with_retry(
                || self.http.get(url).bearer_auth(&self.access_token),
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

        let url = format!("{}{}", GRAPH_BASE, path);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Graph GET {} failed: {}", path, e)))?;

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

        let mut file = tokio::fs::File::create(dest).await.map_err(|e| {
            Error::Other(format!("Failed to create file {}: {}", dest.display(), e))
        })?;
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

    async fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let url = format!("{}{}", GRAPH_BASE, path);
        let resp = self
            .send_with_retry(
                || {
                    self.http
                        .post(&url)
                        .bearer_auth(&self.access_token)
                        .json(body)
                },
                &format!("POST {}", path),
                // POST is not idempotent (sendMail, resource creation,
                // $batch moves): 429-only retry.
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
        let url = format!("{}{}", GRAPH_BASE, path);
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
        let url = format!("{}{}", GRAPH_BASE, path);
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
        let mut email = resp["mail"]
            .as_str()
            .or_else(|| resp["userPrincipalName"].as_str())
            .unwrap_or("")
            .to_string();

        let login_email = email.clone();
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

    /// Fetch messages from a mail folder.
    pub async fn list_messages(
        &self,
        folder_id: &str,
        top: u32,
        skip: u32,
    ) -> Result<(Vec<GraphMessage>, i64)> {
        let resp = self.get(
            &format!("/me/mailFolders/{}/messages", folder_id),
            &[
                ("$select", "id,subject,from,toRecipients,ccRecipients,receivedDateTime,isRead,hasAttachments,flag,internetMessageId,conversationId,bodyPreview,importance,internetMessageHeaders"),
                ("$top", &top.to_string()),
                ("$skip", &skip.to_string()),
                ("$orderby", "receivedDateTime desc"),
                ("$count", "true"),
            ],
        ).await?;

        let total = resp["@odata.count"].as_i64().unwrap_or(0);
        let mut messages = Vec::new();

        if let Some(values) = resp["value"].as_array() {
            for m in values {
                messages.push(parse_graph_message(m));
            }
        }

        Ok((messages, total))
    }

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
                                    bodyPreview,importance";

        let what = format!("GET messages/delta for {}", folder_id);
        let resp = self
            .send_with_retry(
                || {
                    let req = match link {
                        Some(url) => self.http.get(url),
                        None => self
                            .http
                            .get(format!(
                                "{}/me/mailFolders/{}/messages/delta",
                                GRAPH_BASE, folder_id
                            ))
                            .query(&[("$select", DELTA_SELECT)]),
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

        let mut messages = Vec::new();
        let mut removed_ids = Vec::new();
        if let Some(values) = resp["value"].as_array() {
            for m in values {
                if m.get("@removed").is_some() {
                    if let Some(id) = m["id"].as_str() {
                        removed_ids.push(id.to_string());
                    }
                } else {
                    messages.push(parse_graph_message(m));
                }
            }
        }

        Ok(GraphDeltaPage {
            messages,
            removed_ids,
            next_link: resp["@odata.nextLink"].as_str().map(String::from),
            delta_link: resp["@odata.deltaLink"].as_str().map(String::from),
        })
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

        let url = format!("{}/me/messages", GRAPH_BASE);
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

    /// Fetch the full body of a message.
    pub async fn get_message_body(&self, message_id: &str) -> Result<GraphMessageBody> {
        let resp = self
            .get(
                &format!("/me/messages/{}", message_id),
                &[("$select", "body,uniqueBody")],
            )
            .await?;

        let content_type = resp["body"]["contentType"].as_str().unwrap_or("text");
        let content = resp["body"]["content"].as_str().unwrap_or("").to_string();

        Ok(GraphMessageBody {
            content_type: content_type.to_string(),
            content,
        })
    }

    pub async fn get_attachments(
        &self,
        message_id: &str,
    ) -> Result<Vec<crate::db::messages::Attachment>> {
        let resp = self
            .get(
                &format!("/me/messages/{}/attachments", message_id),
                &[("$select", "id,name,contentType,size")],
            )
            .await?;

        let mut attachments = Vec::new();
        if let Some(values) = resp["value"].as_array() {
            for (i, att) in values.iter().enumerate() {
                attachments.push(crate::db::messages::Attachment {
                    index: i as u32,
                    filename: att["name"].as_str().map(|s| s.to_string()),
                    content_type: att["contentType"]
                        .as_str()
                        .unwrap_or("application/octet-stream")
                        .to_string(),
                    size: att["size"].as_u64().unwrap_or(0),
                });
            }
        }
        Ok(attachments)
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

    pub async fn save_draft(&self, message: &GraphSendMessage) -> Result<()> {
        let body = serde_json::json!({
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
        });

        self.post_json("/me/messages", &body).await?;
        log::info!("Graph: draft saved successfully");
        Ok(())
    }

    /// Send a mail message via Graph API.
    pub async fn send_mail(&self, message: &GraphSendMessage) -> Result<()> {
        let body = serde_json::json!({
            "message": {
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
            },
            "saveToSentItems": true
        });

        self.post_json("/me/sendMail", &body).await?;
        log::info!("Graph: mail sent successfully");
        Ok(())
    }

    /// Execute pre-built `$batch` sub-requests (each carrying an `id` set
    /// to its global index), chunked at [`BATCH_SIZE`]. Sub-responses that
    /// come back 429/503/504 are retried after their per-item
    /// `Retry-After` delay, up to 3 rounds; a retried operation that
    /// already committed server-side resolves to a 404
    /// `ErrorItemNotFound`, which callers treat as a stale local row —
    /// so retrying moves/deletes converges instead of duplicating work.
    async fn execute_batch_with_retry(
        &self,
        requests: Vec<serde_json::Value>,
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
                retry_indices.extend(apply_batch_responses(&resp, &mut results));
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
        self.execute_batch_with_retry(requests).await
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
        self.execute_batch_with_retry(requests).await
    }

    pub async fn move_message(&self, message_id: &str, dest_folder_id: &str) -> Result<()> {
        let body = serde_json::json!({ "destinationId": dest_folder_id });
        self.post_json(&format!("/me/messages/{}/move", message_id), &body)
            .await?;
        Ok(())
    }

    /// Delete a message (moves to Deleted Items).
    pub async fn delete_message(&self, message_id: &str) -> Result<()> {
        self.delete(&format!("/me/messages/{}", message_id)).await
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

    /// Update message properties (isRead, flag, etc).
    pub async fn update_message(
        &self,
        message_id: &str,
        updates: &serde_json::Value,
    ) -> Result<()> {
        self.patch_json(&format!("/me/messages/{}", message_id), updates)
            .await
    }

    /// Mark messages as read or unread.
    pub async fn set_read_status(&self, message_ids: &[String], is_read: bool) -> Result<()> {
        // Batch up to 20 requests per $batch call (Graph API limit)
        for chunk in message_ids.chunks(20) {
            let requests: Vec<serde_json::Value> = chunk
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

            let batch_body = serde_json::json!({ "requests": requests });
            self.post_json("/$batch", &batch_body).await?;
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

    /// Fetch events in a time range via calendarView.
    /// Uses `Prefer: outlook.timezone="UTC"` so all times come back in UTC.
    /// Fetch events for a specific calendar via `GET /me/calendars/{id}/calendarView`.
    /// Same query semantics as `list_events`, just scoped to one
    /// calendar so multi-calendar accounts (#47) can keep events
    /// separated. Pagination follows `@odata.nextLink` exactly like
    /// `list_events`.
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
                    let url = format!(
                        "{}/me/calendars/{}/calendarView",
                        GRAPH_BASE,
                        urlencoding::encode(calendar_id)
                    );
                    let resp = self.http
                        .get(&url)
                        .bearer_auth(&self.access_token)
                        .header("Prefer", "outlook.timezone=\"UTC\"")
                        .query(&[
                            ("startDateTime", start),
                            ("endDateTime", end),
                            ("$select", "id,subject,bodyPreview,start,end,location,isAllDay,organizer,attendees,iCalUId,recurrence,responseStatus"),
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

    pub async fn list_events(&self, start: &str, end: &str) -> Result<Vec<GraphCalendarEvent>> {
        let mut events = Vec::new();
        let mut next_path: Option<String> = None;
        loop {
            let resp: serde_json::Value = match next_path.take() {
                Some(path) => {
                    // Pagination: next link is a full URL, fetch directly with UTC preference
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
                    let url = format!("{}/me/calendarView", GRAPH_BASE);
                    let resp = self.http
                        .get(&url)
                        .bearer_auth(&self.access_token)
                        .header("Prefer", "outlook.timezone=\"UTC\"")
                        .query(&[
                            ("startDateTime", start),
                            ("endDateTime", end),
                            ("$select", "id,subject,bodyPreview,start,end,location,isAllDay,organizer,attendees,iCalUId,recurrence,responseStatus"),
                            ("$top", "100"),
                            ("$orderby", "start/dateTime"),
                        ])
                        .send()
                        .await
                        .map_err(|e| Error::Other(format!("Graph GET /me/calendarView failed: {}", e)))?;
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    if !status.is_success() {
                        return Err(Error::Other(format!(
                            "Graph GET /me/calendarView returned {}: {}",
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
                Some(next) => {
                    next_path = Some(next);
                }
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
                Some(path) => {
                    let r = self.http
                        .get(&path)
                        .bearer_auth(&self.access_token)
                        .send()
                        .await
                        .map_err(|e| Error::Other(format!("Graph GET failed: {}", e)))?;
                    let status = r.status();
                    let body = r.text().await.unwrap_or_default();
                    if !status.is_success() {
                        return Err(Error::Other(format!("Graph contacts returned {}: {}", status, truncate(&body, 500))));
                    }
                    serde_json::from_str(&body)
                        .map_err(|e| Error::Other(format!("Graph JSON parse failed: {}", e)))?
                }
                None => {
                    self.get(
                        "/me/contacts",
                        &[
                            ("$select", "id,displayName,givenName,surname,middleName,emailAddresses,mobilePhone,businessPhones,homePhones,companyName,jobTitle"),
                            ("$top", "500"),
                            ("$orderby", "displayName"),
                        ],
                    ).await?
                }
            };
            if let Some(items) = resp["value"].as_array() {
                for c in items {
                    contacts.push(parse_graph_contact(c));
                }
            }
            let next_link = resp["@odata.nextLink"]
                .as_str()
                .map(|s: &str| s.to_string());
            match next_link {
                Some(next) => {
                    next_path = Some(next);
                }
                None => break,
            }
        }
        Ok(contacts)
    }

    /// Create a contact.
    pub async fn create_contact(&self, contact: &serde_json::Value) -> Result<String> {
        let resp = self.post_json("/me/contacts", contact).await?;
        Ok(resp["id"].as_str().unwrap_or("").to_string())
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

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct GraphContact {
    pub id: String,
    pub display_name: String,
    pub given_name: Option<String>,
    pub surname: Option<String>,
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
    match s.split_once('@') {
        Some((local, domain)) => !local.is_empty() && domain.contains('.'),
        None => false,
    }
}

/// One page of a Graph messages-delta response.
#[derive(Debug, Clone)]
pub struct GraphDeltaPage {
    /// Created or updated messages (full selected properties).
    pub messages: Vec<GraphMessage>,
    /// Graph ids of messages removed from the folder (deleted or moved out).
    pub removed_ids: Vec<String>,
    /// More pages are available right now (`@odata.nextLink`).
    pub next_link: Option<String>,
    /// Checkpoint to store for the next sync cycle (`@odata.deltaLink`).
    /// Present only on the final page of a round.
    pub delta_link: Option<String>,
}

/// Marker embedded in the error for HTTP 410 on a delta call: the stored
/// delta token expired server-side and the folder needs a full resync.
const DELTA_RESYNC_MARKER: &str = "graph delta resync required";

/// True if the error is a delta-state expiry (HTTP 410). The caller should
/// clear the stored delta link and restart a full enumeration.
pub fn is_delta_resync_required(err: &crate::error::Error) -> bool {
    err.to_string().contains(DELTA_RESYNC_MARKER)
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
    /// Pulled from internetMessageHeaders (In-Reply-To). Wrapped in
    /// angle brackets to match how IMAP/JMAP store it.
    pub in_reply_to: Option<String>,
    /// Pulled from internetMessageHeaders (References), root first,
    /// each id wrapped in angle brackets.
    pub references: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct GraphMessageBody {
    pub content_type: String,
    pub content: String,
}

pub struct GraphSendMessage {
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
    pub is_recurring: bool,
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

    let (in_reply_to, references) = parse_message_headers(&m["internetMessageHeaders"]);

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
        in_reply_to,
        references,
    }
}

/// Walk Graph's `internetMessageHeaders` array (each entry is
/// `{ "name": "...", "value": "..." }`) and pull out In-Reply-To and
/// References as the wrapped Message-IDs the rest of chithi expects.
fn parse_message_headers(arr: &serde_json::Value) -> (Option<String>, Vec<String>) {
    let Some(items) = arr.as_array() else {
        return (None, Vec::new());
    };
    let mut in_reply_to: Option<String> = None;
    let mut references: Vec<String> = Vec::new();
    for item in items {
        let name = item["name"].as_str().unwrap_or("");
        let value = item["value"].as_str().unwrap_or("");
        if value.is_empty() {
            continue;
        }
        if name.eq_ignore_ascii_case("In-Reply-To") && in_reply_to.is_none() {
            in_reply_to = extract_message_ids(value).into_iter().next();
        } else if name.eq_ignore_ascii_case("References") {
            references = extract_message_ids(value);
        }
    }
    (in_reply_to, references)
}

/// Pull every `<message-id>` token from a header value.
fn extract_message_ids(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut inside = false;
    for c in s.chars() {
        match c {
            '<' => {
                inside = true;
                buf.clear();
            }
            '>' if inside => {
                if let Some(id) = normalize_message_id(&buf) {
                    out.push(id);
                }
                inside = false;
                buf.clear();
            }
            _ if inside => buf.push(c),
            _ => {}
        }
    }
    out
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
        is_recurring: e["recurrence"].is_object(),
    }
}

fn parse_graph_contact(c: &serde_json::Value) -> GraphContact {
    let display_name = c["displayName"].as_str().unwrap_or("").to_string();
    let given_name = c["givenName"].as_str().map(|s| s.to_string());
    let surname = c["surname"].as_str().map(|s| s.to_string());
    let organization = c["companyName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let title = c["jobTitle"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    // Parse emails — Graph's "name" field is a display label, not work/home.
    // Use index-based labeling: first = "work", rest = "other".
    let emails: Vec<serde_json::Value> = c["emailAddresses"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .enumerate()
                .filter_map(|(i, e)| {
                    let addr = e["address"].as_str()?;
                    if addr.is_empty() {
                        return None;
                    }
                    let label = if i == 0 { "work" } else { "other" };
                    Some(serde_json::json!({"email": addr, "label": label}))
                })
                .collect()
        })
        .unwrap_or_default();
    let emails_json = serde_json::to_string(&emails).unwrap_or_else(|_| "[]".to_string());

    // Parse phones: Graph has mobilePhone (string), businessPhones (array), homePhones (array)
    let mut phones: Vec<serde_json::Value> = Vec::new();
    if let Some(mobile) = c["mobilePhone"].as_str().filter(|s| !s.is_empty()) {
        phones.push(serde_json::json!({"number": mobile, "label": "mobile"}));
    }
    if let Some(biz) = c["businessPhones"].as_array() {
        for p in biz {
            if let Some(num) = p.as_str().filter(|s| !s.is_empty()) {
                phones.push(serde_json::json!({"number": num, "label": "work"}));
            }
        }
    }
    if let Some(home) = c["homePhones"].as_array() {
        for p in home {
            if let Some(num) = p.as_str().filter(|s| !s.is_empty()) {
                phones.push(serde_json::json!({"number": num, "label": "home"}));
            }
        }
    }
    let phones_json = serde_json::to_string(&phones).unwrap_or_else(|_| "[]".to_string());

    GraphContact {
        id: c["id"].as_str().unwrap_or("").to_string(),
        display_name,
        given_name,
        surname,
        emails_json,
        phones_json,
        organization,
        title,
    }
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
pub fn event_to_graph_json(event: &crate::db::calendar::CalendarEvent) -> serde_json::Value {
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
pub fn event_patch_to_graph_json(event: &crate::db::calendar::CalendarEvent) -> serde_json::Value {
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
/// `mobilePhone` (first mobile-labelled number) and `businessPhones`
/// (work-labelled numbers) because that is how Outlook models them.
pub fn contact_to_graph_json(
    display_name: &str,
    emails_json: &str,
    phones_json: &str,
    organization: Option<&str>,
    title: Option<&str>,
) -> serde_json::Value {
    let mut gc = serde_json::json!({
        "displayName": display_name,
    });
    if let Ok(emails) = serde_json::from_str::<Vec<serde_json::Value>>(emails_json) {
        let ge: Vec<_> = emails
            .iter()
            .filter_map(|e| {
                e["email"]
                    .as_str()
                    .map(|addr| serde_json::json!({"address": addr, "name": ""}))
            })
            .collect();
        if !ge.is_empty() {
            gc["emailAddresses"] = serde_json::json!(ge);
        }
    }
    if let Ok(phones) = serde_json::from_str::<Vec<serde_json::Value>>(phones_json) {
        let mobile = phones
            .iter()
            .find(|p| p["label"].as_str() == Some("mobile"));
        if let Some(m) = mobile.and_then(|p| p["number"].as_str()) {
            gc["mobilePhone"] = serde_json::json!(m);
        }
        let biz: Vec<&str> = phones
            .iter()
            .filter(|p| p["label"].as_str() == Some("work"))
            .filter_map(|p| p["number"].as_str())
            .collect();
        if !biz.is_empty() {
            gc["businessPhones"] = serde_json::json!(biz);
        }
    }
    if let Some(org) = organization {
        gc["companyName"] = serde_json::json!(org);
    }
    if let Some(t) = title {
        gc["jobTitle"] = serde_json::json!(t);
    }
    gc
}

/// Get a valid Graph API access token for an O365 account.
/// Always refreshes with Graph-specific scopes because the stored token
/// may be IMAP-scoped (both share the same keyring entry and refresh token).
pub async fn get_graph_token(account_id: &str) -> Result<String> {
    get_graph_token_with_scopes(account_id, crate::oauth::MICROSOFT_GRAPH_SCOPES, true).await
}

/// Like [`get_graph_token`] but requests the room scopes (`Place.Read.All`).
///
/// Accounts that signed in before room support existed never consented to
/// `Place.Read.All`, so this refresh fails them with consent_required. That
/// is expected: callers (room suggestion/availability commands) treat the
/// error as "rooms unavailable" and never let it disrupt baseline Graph
/// operations, which keep using the consent-safe [`get_graph_token`].
/// Because this whole scope set is optional, failures here never latch the
/// re-auth flag — a room lookup must not be able to take down mail sync.
pub async fn get_graph_token_for_rooms(account_id: &str) -> Result<String> {
    get_graph_token_with_scopes(account_id, crate::oauth::MICROSOFT_GRAPH_ROOM_SCOPES, false).await
}

async fn get_graph_token_with_scopes(
    account_id: &str,
    scopes: &str,
    latch_reauth: bool,
) -> Result<String> {
    // Dead refresh token (invalid_grant)? Fail fast without a network call
    // until the user signs in again — see oauth::auth_required_on_invalid_grant.
    crate::oauth::ensure_not_reauth_required(account_id)?;

    let tokens = crate::oauth::load_tokens(account_id)?.ok_or_else(|| {
        Error::Other("No O365 OAuth tokens. Please sign in with Microsoft.".into())
    })?;

    let refresh_token = tokens
        .refresh_token
        .ok_or_else(|| Error::Other("No refresh token for O365. Please sign in again.".into()))?;

    // Always refresh with Graph scopes — the cached token is likely IMAP-scoped
    let new_tokens =
        crate::oauth::refresh_with_scopes(&crate::oauth::MICROSOFT, &refresh_token, scopes)
            .await
            .map_err(|e| {
                if latch_reauth {
                    crate::oauth::auth_required_on_invalid_grant(account_id, e)
                } else {
                    e
                }
            })?;
    // Don't overwrite the stored tokens — IMAP sync needs the IMAP-scoped token.
    // The refresh_token may rotate, so save that part only.
    if new_tokens.refresh_token.is_some() {
        crate::oauth::store_tokens(
            account_id,
            &crate::oauth::OAuthTokens {
                access_token: tokens.access_token, // Keep the IMAP token as stored
                refresh_token: new_tokens.refresh_token,
                expires_at: tokens.expires_at,
            },
        )?;
    }

    Ok(new_tokens.access_token)
}

#[cfg(test)]
mod batch_tests {
    use super::{apply_batch_responses, is_delta_resync_required, DELTA_RESYNC_MARKER};
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
        apply_batch_responses(&resp, &mut results);
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
        apply_batch_responses(&resp, &mut results);
        assert!(results[0].is_ok());
        // The stale-id detection in commands::filters matches on "404" +
        // "ErrorItemNotFound" appearing in the error text.
        let err = results[1].as_ref().unwrap_err().to_string();
        assert!(err.contains("404"), "missing status in: {err}");
        assert!(err.contains("ErrorItemNotFound"), "missing code in: {err}");
    }

    #[test]
    fn batch_responses_missing_item_stays_err() {
        let resp = serde_json::json!({ "responses": [ { "id": "0", "status": 200 } ] });
        let mut results = fresh_results(2);
        apply_batch_responses(&resp, &mut results);
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
        let retryable = apply_batch_responses(&resp, &mut results);
        assert!(results[0].is_ok());
        // Throttled items keep their placeholder (not a final outcome yet).
        assert!(results[1].is_err());
        assert!(results[2].is_err());
        assert_eq!(retryable, vec![(1, 17), (2, 3)]);
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
        parse_graph_room_availability, parse_graph_rooms, GraphRoom,
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
        // Degenerate inputs are rejected.
        assert!(!looks_like_smtp_address(""));
        assert!(!looks_like_smtp_address("noatsign"));
        assert!(!looks_like_smtp_address("@outlook.com"));
        assert!(!looks_like_smtp_address("user@localhost"));
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
    use super::{contact_to_graph_json, event_patch_to_graph_json, event_to_graph_json};
    use crate::db::calendar::CalendarEvent;

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
        );
        assert_eq!(v["mobilePhone"], "+4670");
        assert_eq!(v["businessPhones"][0], "+4608");
        assert_eq!(v["companyName"], "Analytical Engines");
        assert_eq!(v["emailAddresses"][0]["address"], "ada@x.org");
        assert!(v["jobTitle"].is_null());
    }

    #[test]
    fn contact_handles_malformed_json() {
        let v = contact_to_graph_json("X", "not json", "not json", None, None);
        assert!(v["emailAddresses"].is_null());
        assert!(v["mobilePhone"].is_null());
        assert!(v["businessPhones"].is_null());
    }
}
