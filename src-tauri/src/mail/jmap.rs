use crate::error::{Error, Result};
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
    pub start: String,      // ISO 8601
    pub end: String,
    pub all_day: bool,
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

            // Normalize date to UTC for consistent sorting
            let date = e["receivedAt"].as_str()
                .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
                .unwrap_or_default();
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

        // Get the imported email ID for cleanup if submission fails
        let imported_id = resp["methodResponses"][0][1]["created"]["draft"]["id"]
            .as_str()
            .map(|s| s.to_string());

        // Check for submission errors — clean up imported email on failure
        let submission_failed = if resp["methodResponses"].as_array().map(|a| a.len()).unwrap_or(0) > 1 {
            if resp["methodResponses"][1][0].as_str() == Some("error") {
                let desc = resp["methodResponses"][1][1]["description"]
                    .as_str().unwrap_or("Unknown error");
                Some(format!("JMAP submission failed: {}", desc))
            } else if let Some(err) = resp["methodResponses"][1][1]["notCreated"]["sub1"].as_object() {
                let desc = err.get("description").and_then(|d| d.as_str()).unwrap_or("Unknown error");
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
                log::warn!("JMAP cleaning up imported email {} after submission failure", email_id);
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
        let upload_url = self.upload_url_template
            .replace("{accountId}", &self.account_id);

        let resp = self.http.post(&upload_url)
            .basic_auth(&config.username, Some(&config.password))
            .header("Content-Type", "message/rfc822")
            .body(raw_message.to_vec())
            .send().await
            .map_err(|e| Error::Other(format!("JMAP draft upload failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!("JMAP draft upload error {}: {}", status, body)));
        }

        let upload_resp: serde_json::Value = resp.json().await
            .map_err(|e| Error::Other(format!("JMAP draft upload parse error: {}", e)))?;
        let blob_id = upload_resp["blobId"].as_str()
            .ok_or_else(|| Error::Other("No blobId in draft upload response".into()))?
            .to_string();

        // Find the Drafts mailbox
        let drafts_mailbox_id = self.find_mailbox_by_role(config, "drafts").await?
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
            let desc = err.get("description").and_then(|d| d.as_str()).unwrap_or("Unknown error");
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
                                   "recurrenceRules", "uid", "locations",
                                   "participants", "@type"]
                }, "g1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;
        log::debug!("JMAP CalendarEvent response: {}", serde_json::to_string(&resp).unwrap_or_default());

        // Check if the query returned an error
        if resp["methodResponses"][0][0].as_str() == Some("error") {
            let desc = resp["methodResponses"][0][1]["description"].as_str().unwrap_or("Unknown");
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

            // Start datetime — JSCalendar uses local time without timezone.
            // Stalwart stores in UTC, so append Z to mark as UTC for correct display.
            let raw_start = ev["start"].as_str().unwrap_or("").to_string();
            let start = if !raw_start.is_empty() && !raw_start.ends_with('Z') && !raw_start.contains('+') {
                format!("{}Z", raw_start)
            } else {
                raw_start
            };

            let all_day = ev["showWithoutTime"].as_bool().unwrap_or(false);

            let duration_str = ev["duration"].as_str().unwrap_or("PT1H");
            let end = {
                let e = compute_end_from_duration(start.trim_end_matches('Z'), duration_str);
                if start.ends_with('Z') && !e.ends_with('Z') { format!("{}Z", e) } else { e }
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
                    let email = p["calendarAddress"].as_str()
                        .map(|s| s.trim_start_matches("mailto:").to_string())
                        .or_else(|| p["sendTo"].as_object()
                            .and_then(|s| s.get("imip"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim_start_matches("mailto:").to_string()))
                        .or_else(|| p["email"].as_str().map(|s| s.to_string()));
                    let name = p["name"].as_str().map(|s| s.to_string());
                    let mut status = p["participationStatus"].as_str().unwrap_or("needs-action").to_string();
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
            let attendees_json = if attendees.is_empty() { None } else {
                Some(serde_json::to_string(&attendees).unwrap_or_default())
            };

            log::debug!("  event: {} ({}) start={} end={} attendees={}", title, id, start, end, attendees.len());
            events.push(JmapCalendarEvent {
                id,
                calendar_id: cal_id,
                title,
                description,
                location,
                start,
                end,
                all_day,
                recurrence_rule,
                uid,
                organizer_email,
                attendees_json,
            });
        }

        // Client-side filter by calendar if requested
        let filtered = if let Some(cal_id) = calendar_id {
            events.into_iter().filter(|e| e.calendar_id == cal_id).collect()
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
        log::info!("JMAP creating calendar event: '{}' organizer={:?} attendees={:?}",
            event.title, event.organizer_email, event.attendees_json);

        let uid = event.uid.clone().unwrap_or_else(|| {
            format!("{}@chithi", uuid::Uuid::new_v4())
        });

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
                participants.insert("organizer".to_string(), serde_json::json!({
                    "@type": "Participant",
                    "calendarAddress": format!("mailto:{}", org_email),
                    "roles": {"owner": true, "attendee": true},
                    "participationStatus": "accepted",
                    "expectReply": false,
                }));
            }
        }
        if let Some(ref att_json) = event.attendees_json {
            if let Ok(attendees) = serde_json::from_str::<Vec<serde_json::Value>>(att_json) {
                for (i, att) in attendees.iter().enumerate() {
                    let email = att["email"].as_str().unwrap_or_default();
                    if !email.is_empty() {
                        let status = att["status"].as_str().unwrap_or("needs-action");
                        participants.insert(format!("att{}", i), serde_json::json!({
                            "@type": "Participant",
                            "calendarAddress": format!("mailto:{}", email),
                            "roles": {"attendee": true},
                            "participationStatus": status,
                            "expectReply": true,
                        }));
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
            let desc = err.get("description").and_then(|d| d.as_str()).unwrap_or("Unknown error");
            return Err(Error::Other(format!("JMAP create calendar event failed: {}", desc)));
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
            let desc = err.get("description").and_then(|d| d.as_str()).unwrap_or("Unknown error");
            return Err(Error::Other(format!("JMAP update calendar event failed: {}", desc)));
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
        log::info!("JMAP updating participant {} status to {} on event {}", participant_key, status, event_id);

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
            let desc = err.get("description").and_then(|d| d.as_str()).unwrap_or("Unknown error");
            return Err(Error::Other(format!("JMAP update participant failed: {}", desc)));
        }

        log::info!("JMAP updated participant status on event {}", event_id);
        Ok(())
    }

    /// Delete a calendar event on the server via CalendarEvent/set.
    pub async fn delete_calendar_event(
        &self,
        config: &JmapConfig,
        event_id: &str,
    ) -> Result<()> {
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
            let desc = err.get("description").and_then(|d| d.as_str()).unwrap_or("Unknown error");
            return Err(Error::Other(format!("JMAP delete calendar event failed: {}", desc)));
        }

        log::info!("JMAP deleted calendar event id={}", event_id);
        Ok(())
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
                books.push(JmapAddressBook { id, name, is_default });
            }
        }

        log::info!("JMAP found {} address books", books.len());
        Ok(books)
    }

    /// Fetch contacts from a JMAP address book.
    pub async fn fetch_contacts(&self, config: &JmapConfig, address_book_id: Option<&str>) -> Result<Vec<JmapContact>> {
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
        log::debug!("JMAP ContactCard response: {}", serde_json::to_string(&resp).unwrap_or_default());

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
                    else if let Some(components) = name_obj.get("components").and_then(|c| c.as_array()) {
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
                        if parts.is_empty() { "(No name)".to_string() } else { parts.join(" ") }
                    }
                    // Try direct given/surname
                    else {
                        let given = name_obj.get("given").and_then(|g| g.as_str()).unwrap_or("");
                        let surname = name_obj.get("surname").and_then(|s| s.as_str()).unwrap_or("");
                        let name = format!("{} {}", given, surname).trim().to_string();
                        if name.is_empty() { "(No name)".to_string() } else { name }
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
                        let label = em["label"].as_str().map(|s| s.to_string())
                            .or_else(|| em["contexts"].as_object().and_then(|c| c.keys().next().cloned()))
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
                let organization = card["organizations"].as_object()
                    .and_then(|orgs| orgs.values().next())
                    .and_then(|org| org["name"].as_str())
                    .map(|s| s.to_string());

                let title = card["titles"].as_object()
                    .and_then(|titles| titles.values().next())
                    .and_then(|t| t["name"].as_str())
                    .map(|s| s.to_string());

                let notes = card["notes"].as_object()
                    .and_then(|n| n.values().next())
                    .and_then(|note| note["note"].as_str())
                    .map(|s| s.to_string());

                // Determine which address book this belongs to
                let ab_id = card["addressBookIds"].as_object()
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
                    emails_json: serde_json::to_string(&emails).unwrap_or_else(|_| "[]".to_string()),
                    phones_json: serde_json::to_string(&phones).unwrap_or_else(|_| "[]".to_string()),
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
            let middle = name_parts[1..name_parts.len()-1].join(" ");
            components.push(serde_json::json!({"kind": "given2", "value": middle}));
        }
        if name_parts.len() >= 2 {
            components.push(serde_json::json!({"kind": "surname", "value": name_parts.last().unwrap()}));
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
            let desc = err.get("description").and_then(|d| d.as_str()).unwrap_or("Unknown error");
            return Err(Error::Other(format!("JMAP create contact failed: {}", desc)));
        }

        let remote_id = resp["methodResponses"][0][1]["created"]["new1"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        log::info!("JMAP created contact '{}' id={}", display_name, remote_id);
        Ok(remote_id)
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

/// Compute end datetime from a start datetime and an ISO 8601 duration string.
/// Handles simple cases like PT1H, PT30M, P1D, PT1H30M, etc.
/// Falls back to start + 1 hour if parsing fails.
fn compute_end_from_duration(start: &str, duration: &str) -> String {
    use chrono::{NaiveDateTime, NaiveDate, Duration};

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
            'P' => {},
            'T' => { in_time = true; },
            '0'..='9' => { num_buf.push(ch); },
            'D' => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 86400;
                }
                num_buf.clear();
            },
            'H' if in_time => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 3600;
                }
                num_buf.clear();
            },
            'M' if in_time => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 60;
                }
                num_buf.clear();
            },
            'S' if in_time => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n;
                }
                num_buf.clear();
            },
            'W' => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 604800;
                }
                num_buf.clear();
            },
            _ => { num_buf.clear(); },
        }
    }

    if total == 0 { 3600 } else { total } // default 1 hour
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
