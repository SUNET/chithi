//! Microsoft Graph API client for O365 mail, calendar, and contacts.
//!
//! All operations go through `https://graph.microsoft.com/v1.0` with
//! Bearer token authentication. No IMAP/SMTP needed for O365 accounts.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

// ---------------------------------------------------------------------------
// Graph client
// ---------------------------------------------------------------------------

pub struct GraphClient {
    http: reqwest::Client,
    access_token: String,
}

impl GraphClient {
    pub fn new(access_token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            access_token: access_token.to_string(),
        }
    }

    async fn get(&self, path: &str, params: &[(&str, &str)]) -> Result<serde_json::Value> {
        let url = format!("{}{}", GRAPH_BASE, path);
        let resp = self.http
            .get(&url)
            .bearer_auth(&self.access_token)
            .query(params)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Graph GET {} failed: {}", path, e)))?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::Other(format!("Graph GET {} returned {}: {}", path, status, truncate(&body, 500))));
        }

        serde_json::from_str(&body)
            .map_err(|e| Error::Other(format!("Graph JSON parse failed: {}", e)))
    }

    async fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let url = format!("{}{}", GRAPH_BASE, path);
        let resp = self.http
            .post(&url)
            .bearer_auth(&self.access_token)
            .json(body)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Graph POST {} failed: {}", path, e)))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::Other(format!("Graph POST {} returned {}: {}", path, status, truncate(&text, 500))));
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
        let resp = self.http
            .patch(&url)
            .bearer_auth(&self.access_token)
            .json(body)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Graph PATCH {} failed: {}", path, e)))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!("Graph PATCH {} returned {}: {}", path, status, truncate(&text, 500))));
        }
        Ok(())
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let url = format!("{}{}", GRAPH_BASE, path);
        let resp = self.http
            .delete(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Graph DELETE {} failed: {}", path, e)))?;

        let status = resp.status();
        if !status.is_success() && status.as_u16() != 204 {
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!("Graph DELETE {} returned {}: {}", path, status, truncate(&text, 500))));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // User profile
    // -----------------------------------------------------------------------

    /// Get the signed-in user's profile (email, display name).
    pub async fn get_me(&self) -> Result<GraphUser> {
        let resp = self.get("/me", &[("$select", "id,displayName,userPrincipalName,mail")]).await?;

        let display_name = resp["displayName"].as_str().unwrap_or("").to_string();
        let mut email = resp["mail"].as_str()
            .or_else(|| resp["userPrincipalName"].as_str())
            .unwrap_or("")
            .to_string();

        let login_email = email.clone();
        log::info!("Graph /me: displayName={}, login_email={}", display_name, login_email);

        // For personal Microsoft accounts, the login email (e.g., gmail.com) may differ
        // from the actual Outlook mailbox address. Try multiple sources:

        // 1. Check To address of inbox messages (catches user-configured aliases like chithiapp@outlook.com)
        if let Ok(inbox_resp) = self.get(
            "/me/mailFolders('Inbox')/messages",
            &[("$top", "1"), ("$select", "toRecipients")],
        ).await {
            if let Some(to_addr) = inbox_resp["value"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|m| m["toRecipients"].as_array())
                .and_then(|r| r.first())
                .and_then(|r| r["emailAddress"]["address"].as_str())
            {
                if to_addr != email && (to_addr.contains("outlook.") || to_addr.contains("hotmail.") || to_addr.contains("live.")) {
                    log::info!("Graph: mailbox email from Inbox To: {}", to_addr);
                    email = to_addr.to_string();
                }
            }
        }

        // 2. Fallback: check From address of sent messages
        if email == login_email {
            if let Ok(sent_resp) = self.get(
                "/me/mailFolders('SentItems')/messages",
                &[("$top", "1"), ("$select", "from")],
            ).await {
                if let Some(from_addr) = sent_resp["value"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|m| m["from"]["emailAddress"]["address"].as_str())
                {
                    if from_addr != email {
                        log::info!("Graph: mailbox email from Sent: {}", from_addr);
                        email = from_addr.to_string();
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

    /// List all mail folders.
    pub async fn list_mail_folders(&self) -> Result<Vec<GraphMailFolder>> {
        let mut folders = Vec::new();
        let mut url = "/me/mailFolders".to_string();

        loop {
            let resp = self.get(&url, &[
                ("$select", "id,displayName,totalItemCount,unreadItemCount,parentFolderId"),
                ("$top", "100"),
                ("includeHiddenFolders", "true"),
            ]).await?;

            if let Some(values) = resp["value"].as_array() {
                for f in values {
                    folders.push(GraphMailFolder {
                        id: f["id"].as_str().unwrap_or("").to_string(),
                        display_name: f["displayName"].as_str().unwrap_or("").to_string(),
                        total_count: f["totalItemCount"].as_i64().unwrap_or(0),
                        unread_count: f["unreadItemCount"].as_i64().unwrap_or(0),
                        parent_folder_id: f["parentFolderId"].as_str().map(|s| s.to_string()),
                    });
                }
            }

            // Pagination
            if let Some(next) = resp["@odata.nextLink"].as_str() {
                // nextLink is a full URL — strip the base
                url = next.replace(GRAPH_BASE, "");
            } else {
                break;
            }
        }

        // Also fetch child folders (Graph only returns top-level by default)
        let top_ids: Vec<String> = folders.iter().map(|f| f.id.clone()).collect();
        for parent_id in &top_ids {
            let child_resp = self.get(
                &format!("/me/mailFolders/{}/childFolders", parent_id),
                &[("$select", "id,displayName,totalItemCount,unreadItemCount,parentFolderId")],
            ).await;
            if let Ok(resp) = child_resp {
                if let Some(values) = resp["value"].as_array() {
                    for f in values {
                        folders.push(GraphMailFolder {
                            id: f["id"].as_str().unwrap_or("").to_string(),
                            display_name: f["displayName"].as_str().unwrap_or("").to_string(),
                            total_count: f["totalItemCount"].as_i64().unwrap_or(0),
                            unread_count: f["unreadItemCount"].as_i64().unwrap_or(0),
                            parent_folder_id: f["parentFolderId"].as_str().map(|s| s.to_string()),
                        });
                    }
                }
            }
        }

        log::info!("Graph: found {} mail folders", folders.len());
        Ok(folders)
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
                ("$select", "id,subject,from,toRecipients,ccRecipients,receivedDateTime,isRead,hasAttachments,flag,internetMessageId,conversationId,bodyPreview,importance"),
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

    /// Fetch the full body of a message.
    pub async fn get_message_body(&self, message_id: &str) -> Result<GraphMessageBody> {
        let resp = self.get(
            &format!("/me/messages/{}", message_id),
            &[("$select", "body,uniqueBody")],
        ).await?;

        let content_type = resp["body"]["contentType"].as_str().unwrap_or("text");
        let content = resp["body"]["content"].as_str().unwrap_or("").to_string();

        Ok(GraphMessageBody {
            content_type: content_type.to_string(),
            content,
        })
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

    /// Move a message to a different folder.
    pub async fn move_message(&self, message_id: &str, dest_folder_id: &str) -> Result<()> {
        let body = serde_json::json!({ "destinationId": dest_folder_id });
        self.post_json(&format!("/me/messages/{}/move", message_id), &body).await?;
        Ok(())
    }

    /// Delete a message (moves to Deleted Items).
    pub async fn delete_message(&self, message_id: &str) -> Result<()> {
        self.delete(&format!("/me/messages/{}", message_id)).await
    }

    /// Update message properties (isRead, flag, etc).
    pub async fn update_message(&self, message_id: &str, updates: &serde_json::Value) -> Result<()> {
        self.patch_json(&format!("/me/messages/{}", message_id), updates).await
    }

    /// Mark messages as read or unread.
    pub async fn set_read_status(&self, message_ids: &[String], is_read: bool) -> Result<()> {
        let body = serde_json::json!({ "isRead": is_read });
        for id in message_ids {
            self.patch_json(&format!("/me/messages/{}", id), &body).await?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct GraphUser {
    pub display_name: String,
    /// The actual mailbox email (from Sent Items or /me)
    pub email: String,
    /// The Microsoft login identity (from /me — used for XOAUTH2)
    pub login_email: String,
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
    pub has_attachments: bool,
    pub internet_message_id: Option<String>,
    pub conversation_id: Option<String>,
    pub preview: Option<String>,
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_graph_message(m: &serde_json::Value) -> GraphMessage {
    let from = &m["from"]["emailAddress"];
    let from_name = from["name"].as_str().map(|s| s.to_string());
    let from_email = from["address"].as_str().map(|s| s.to_string());

    let to_addresses = parse_recipients(&m["toRecipients"]);
    let cc_addresses = parse_recipients(&m["ccRecipients"]);

    let date = m["receivedDateTime"].as_str()
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
        has_attachments: m["hasAttachments"].as_bool().unwrap_or(false),
        internet_message_id: m["internetMessageId"].as_str().map(|s| s.to_string()),
        conversation_id: m["conversationId"].as_str().map(|s| s.to_string()),
        preview: m["bodyPreview"].as_str().map(|s| s.to_string()),
    }
}

fn parse_recipients(arr: &serde_json::Value) -> String {
    let addrs: Vec<serde_json::Value> = arr.as_array()
        .map(|a| a.iter().map(|r| {
            serde_json::json!({
                "name": r["emailAddress"]["name"].as_str().unwrap_or(""),
                "email": r["emailAddress"]["address"].as_str().unwrap_or(""),
            })
        }).collect())
        .unwrap_or_default();
    serde_json::to_string(&addrs).unwrap_or_else(|_| "[]".to_string())
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max { s } else { &s[..max] }
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

/// Get a valid Graph API access token for an O365 account, refreshing if needed.
pub async fn get_graph_token(account_id: &str) -> Result<String> {
    let tokens = crate::oauth::load_tokens(account_id)?
        .ok_or_else(|| Error::Other("No O365 OAuth tokens. Please sign in with Microsoft.".into()))?;

    if !tokens.is_expired() {
        return Ok(tokens.access_token);
    }

    let refresh_token = tokens.refresh_token
        .ok_or_else(|| Error::Other("No refresh token for O365. Please sign in again.".into()))?;

    let new_tokens = crate::oauth::refresh_access_token(&crate::oauth::MICROSOFT, &refresh_token).await?;
    crate::oauth::store_tokens(account_id, &new_tokens)?;

    Ok(new_tokens.access_token)
}
