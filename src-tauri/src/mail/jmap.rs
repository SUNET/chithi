use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

pub struct JmapConfig {
    pub jmap_url: String,
    pub email: String,
    pub username: String,
    pub password: String,
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
    pub size: u64,
    pub has_attachments: bool,
    pub flags: Vec<String>,
    pub preview: Option<String>,
}

/// JMAP connection that uses raw HTTP requests through the HTTPS proxy.
/// This avoids issues with jmap-client following internal URLs from the
/// session that aren't accessible externally (e.g., http://host:8080).
pub struct JmapConnection {
    http: reqwest::Client,
    api_url: String,
    download_url_template: String,
    upload_url_template: String,
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
    #[serde(rename = "primaryAccounts")]
    primary_accounts: std::collections::HashMap<String, String>,
}

impl JmapConnection {
    pub async fn connect(config: &JmapConfig) -> Result<Self> {
        let base_url = if !config.jmap_url.is_empty() {
            let url = config.jmap_url.trim_end_matches('/').to_string();
            let url = url.trim_end_matches("/.well-known/jmap").to_string();
            url
        } else {
            // Auto-discover
            let domain = config.email.rsplit_once('@').map(|(_, d)| d)
                .ok_or_else(|| Error::Other(format!("Cannot extract domain from '{}'", config.email)))?;
            let candidates = [
                format!("https://{}", domain),
                format!("https://mail.{}", domain),
                format!("https://jmap.{}", domain),
            ];
            let http = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build().map_err(|e| Error::Other(e.to_string()))?;
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
            found.ok_or_else(|| Error::Other(format!("JMAP auto-discovery failed for {}", domain)))?
        };

        log::info!("JMAP connecting to {} as {}", base_url, config.username);

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build().map_err(|e| Error::Other(e.to_string()))?;

        // Fetch session with authentication
        let well_known = format!("{}/.well-known/jmap", base_url);
        let resp = http.get(&well_known)
            .basic_auth(&config.username, Some(&config.password))
            .send().await
            .map_err(|e| Error::Other(format!("JMAP session fetch failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!("JMAP session: {} {}", status, body)));
        }

        let session: JmapSession = resp.json().await
            .map_err(|e| Error::Other(format!("JMAP session parse failed: {}", e)))?;

        // Get the default account ID
        let account_id = session.primary_accounts
            .values().next()
            .cloned()
            .ok_or_else(|| Error::Other("No primary account in JMAP session".into()))?;

        // Rewrite URLs to go through the HTTPS proxy instead of internal URLs.
        // e.g., "http://mail.example.com:8080/jmap/" → "https://mail.example.com/jmap/"
        let api_url = rewrite_url(&session.api_url, &base_url);
        let download_url = rewrite_url(&session.download_url, &base_url);
        let upload_url = rewrite_url(&session.upload_url, &base_url);

        log::info!("JMAP connected: account={}, api={}", account_id, api_url);

        Ok(Self {
            http,
            api_url,
            download_url_template: download_url,
            upload_url_template: upload_url,
            account_id,
        })
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// Send a JMAP API request and return the response JSON.
    async fn api_request(&self, body: &serde_json::Value, config: &JmapConfig) -> Result<serde_json::Value> {
        let resp = self.http.post(&self.api_url)
            .basic_auth(&config.username, Some(&config.password))
            .json(body)
            .send().await
            .map_err(|e| Error::Other(format!("JMAP request failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!("JMAP API error {}: {}", status, body)));
        }

        resp.json().await.map_err(|e| Error::Other(format!("JMAP response parse error: {}", e)))
    }

    pub async fn list_folders(&self, config: &JmapConfig) -> Result<Vec<(String, String, Option<&'static str>)>> {
        log::debug!("JMAP listing mailboxes");
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Mailbox/get", {
                    "accountId": self.account_id,
                    "properties": ["id", "name", "role", "totalEmails", "unreadEmails"]
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
            log::debug!("  mailbox: {} ({}) role={:?}", name, id, role);
            folders.push((name, id, folder_type));
        }
        log::info!("JMAP found {} mailboxes", folders.len());
        Ok(folders)
    }

    pub async fn fetch_emails(
        &self,
        config: &JmapConfig,
        mailbox_id: &str,
        _since_state: Option<&str>,
    ) -> Result<(Vec<JmapEmail>, String)> {
        log::debug!("JMAP fetching emails from mailbox {}", mailbox_id);

        // Query emails in this mailbox, sorted by receivedAt descending
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Email/query", {
                    "accountId": self.account_id,
                    "filter": { "inMailbox": mailbox_id },
                    "sort": [{ "property": "receivedAt", "isAscending": false }],
                    "limit": 500
                }, "q1"],
                ["Email/get", {
                    "#ids": { "resultOf": "q1", "name": "Email/query", "path": "/ids" },
                    "accountId": self.account_id,
                    "properties": ["id", "subject", "from", "to", "cc", "receivedAt",
                                   "size", "keywords", "messageId", "inReplyTo",
                                   "hasAttachment", "preview"]
                }, "g1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        // Get the state from Email/query
        let state = resp["methodResponses"][0][1]["queryState"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let emails_json = resp["methodResponses"][1][1]["list"]
            .as_array()
            .ok_or_else(|| Error::Other("Invalid Email/get response".into()))?;

        let mut emails = Vec::new();
        for e in emails_json {
            let id = e["id"].as_str().unwrap_or("").to_string();
            let subject = e["subject"].as_str().map(|s| s.to_string());

            let (from_name, from_email) = e["from"].as_array()
                .and_then(|a| a.first())
                .map(|f| (
                    f["name"].as_str().map(|s| s.to_string()),
                    f["email"].as_str().map(|s| s.to_string()),
                ))
                .unwrap_or((None, None));

            let to_addresses = addresses_to_json(e["to"].as_array());
            let cc_addresses = addresses_to_json(e["cc"].as_array());

            let date = e["receivedAt"].as_str().unwrap_or("").to_string();
            let size = e["size"].as_u64().unwrap_or(0);
            let message_id = e["messageId"].as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(|s| format!("<{}>", s));
            let in_reply_to = e["inReplyTo"].as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(|s| format!("<{}>", s));
            let has_attachments = e["hasAttachment"].as_bool().unwrap_or(false);
            let preview = e["preview"].as_str().map(|s| s.to_string());

            // Convert JMAP keywords to flags
            let keywords = e["keywords"].as_object();
            let mut flags = Vec::new();
            if let Some(kw) = keywords {
                if kw.contains_key("$seen") { flags.push("seen".to_string()); }
                if kw.contains_key("$flagged") { flags.push("flagged".to_string()); }
                if kw.contains_key("$answered") { flags.push("answered".to_string()); }
                if kw.contains_key("$draft") { flags.push("draft".to_string()); }
            }

            emails.push(JmapEmail {
                id, subject, from_name, from_email,
                to_addresses, cc_addresses, date, message_id,
                in_reply_to, size, has_attachments, flags, preview,
            });
        }

        log::info!("JMAP fetched {} emails from mailbox {}", emails.len(), mailbox_id);
        Ok((emails, state))
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
        let download_url = self.download_url_template
            .replace("{accountId}", &self.account_id)
            .replace("{blobId}", blob_id)
            .replace("{name}", "message.eml")
            .replace("{type}", "application/octet-stream");

        log::debug!("JMAP downloading blob from {}", download_url);
        let resp = self.http.get(&download_url)
            .basic_auth(&config.username, Some(&config.password))
            .send().await
            .map_err(|e| Error::Other(format!("JMAP download failed: {}", e)))?;

        if !resp.status().is_success() {
            return Err(Error::Other(format!("JMAP download error: {}", resp.status())));
        }

        let bytes = resp.bytes().await
            .map_err(|e| Error::Other(format!("JMAP download read error: {}", e)))?;
        log::debug!("JMAP downloaded {} bytes for email {}", bytes.len(), email_id);
        Ok(Some(bytes.to_vec()))
    }

    pub async fn set_flags(
        &self,
        config: &JmapConfig,
        email_ids: &[String],
        flags: &[&str],
        add: bool,
    ) -> Result<()> {
        log::debug!("JMAP set_flags: {:?} add={} on {} emails", flags, add, email_ids.len());

        let mut update = serde_json::Map::new();
        for id in email_ids {
            let mut patch = serde_json::Map::new();
            for flag in flags {
                let keyword = flag_to_keyword(flag);
                let key = format!("keywords/{}", keyword);
                patch.insert(key, if add { serde_json::json!(true) } else { serde_json::json!(null) });
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

    pub async fn move_emails(
        &self,
        config: &JmapConfig,
        email_ids: &[String],
        from_mailbox: &str,
        to_mailbox: &str,
    ) -> Result<()> {
        log::debug!("JMAP moving {} emails from {} to {}", email_ids.len(), from_mailbox, to_mailbox);
        let mut update = serde_json::Map::new();
        for id in email_ids {
            update.insert(id.clone(), serde_json::json!({
                format!("mailboxIds/{}", from_mailbox): null,
                format!("mailboxIds/{}", to_mailbox): true,
            }));
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
    pub async fn send_email(&self, config: &JmapConfig, raw_message: &[u8]) -> Result<()> {
        log::info!("JMAP sending email ({} bytes)", raw_message.len());

        // Step 1: Upload the raw message as a blob
        let upload_url = self.upload_url_template
            .replace("{accountId}", &self.account_id);
        log::debug!("JMAP uploading blob to {}", upload_url);

        let resp = self.http.post(&upload_url)
            .basic_auth(&config.username, Some(&config.password))
            .header("Content-Type", "message/rfc822")
            .body(raw_message.to_vec())
            .send().await
            .map_err(|e| Error::Other(format!("JMAP upload failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!("JMAP upload error {}: {}", status, body)));
        }

        let upload_resp: serde_json::Value = resp.json().await
            .map_err(|e| Error::Other(format!("JMAP upload response parse error: {}", e)))?;
        let blob_id = upload_resp["blobId"].as_str()
            .ok_or_else(|| Error::Other("No blobId in upload response".into()))?
            .to_string();
        log::debug!("JMAP blob uploaded: {}", blob_id);

        // Step 2: Find the Sent mailbox (or Inbox as fallback) to store the email
        let sent_mailbox_id = self.find_mailbox_by_role(config, "sent").await?
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
        log::debug!("JMAP send response: {}", serde_json::to_string_pretty(&resp).unwrap_or_default());

        // Check for import errors
        if let Some(err) = resp["methodResponses"][0][1]["notCreated"]["draft"].as_object() {
            let desc = err.get("description").and_then(|d| d.as_str()).unwrap_or("Unknown error");
            return Err(Error::Other(format!("JMAP email import failed: {}", desc)));
        }

        // Check for submission errors
        if resp["methodResponses"].as_array().map(|a| a.len()).unwrap_or(0) > 1 {
            if resp["methodResponses"][1][0].as_str() == Some("error") {
                let desc = resp["methodResponses"][1][1]["description"]
                    .as_str().unwrap_or("Unknown error");
                return Err(Error::Other(format!("JMAP submission failed: {}", desc)));
            }
            if let Some(err) = resp["methodResponses"][1][1]["notCreated"]["sub1"].as_object() {
                let desc = err.get("description").and_then(|d| d.as_str()).unwrap_or("Unknown error");
                return Err(Error::Other(format!("JMAP submission failed: {}", desc)));
            }
        }

        log::info!("JMAP email sent successfully");
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

    /// Find a mailbox by its JMAP role (inbox, sent, drafts, trash, junk).
    async fn find_mailbox_by_role(&self, config: &JmapConfig, role: &str) -> Result<Option<String>> {
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
}

/// Rewrite an internal URL to go through the HTTPS proxy.
/// e.g., "http://mail.example.com:8080/jmap/foo" → "https://mail.example.com/jmap/foo"
/// Uses simple string manipulation to preserve template placeholders like {accountId}.
fn rewrite_url(internal_url: &str, base_url: &str) -> String {
    // Extract the path from the internal URL by finding the third slash
    // e.g., "http://mail.example.com:8080/jmap/download/{accountId}..." → "/jmap/download/{accountId}..."
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
