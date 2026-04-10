use serde::Deserialize;
use tauri::State;

use crate::db;
use crate::error::{Error, Result};
use crate::mail::jmap::{JmapConfig, JmapConnection};
use crate::mail::smtp;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ComposeMessage {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: String,
    pub body_html: Option<String>,
    #[serde(default)]
    pub attachments: Vec<FileAttachment>,
}

#[derive(Debug, Deserialize)]
pub struct FileAttachment {
    pub path: String,
    pub name: String,
}

#[tauri::command]
pub async fn send_message(
    state: State<'_, AppState>,
    account_id: String,
    message: ComposeMessage,
) -> Result<()> {
    log::info!(
        "Send message command: account={} to={:?} subject='{}'",
        account_id,
        message.to,
        message.subject
    );

    let account = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)?
    };

    if account.mail_protocol == "graph" {
        log::info!("Sending via Microsoft Graph for account {}", account.email);

        let token = crate::mail::graph::get_graph_token(&account_id).await?;
        let client = crate::mail::graph::GraphClient::new(&token);
        client.send_mail(&crate::mail::graph::GraphSendMessage {
            to: message.to.clone(),
            cc: message.cc.clone(),
            bcc: message.bcc.clone(),
            subject: message.subject.clone(),
            body_text: message.body_text.clone(),
        }).await?;
    } else if account.mail_protocol == "jmap" {
        log::info!("Sending via JMAP for account {}", account.email);

        let jmap_config = JmapConfig {
            jmap_url: account.jmap_url.clone(),
            email: account.email.clone(),
            username: account.username.clone(),
            password: account.password.clone(),
        };

        // Build raw RFC5322 message using lettre's builder, then send via JMAP
        let attachment_data = read_attachments(&message.attachments)?;
        let raw_message = smtp::build_raw_message(
            &account.email,
            &message.to,
            &message.cc,
            &message.bcc,
            &message.subject,
            &message.body_text,
            message.body_html.as_deref(),
            &attachment_data,
        )?;

        let conn_jmap = JmapConnection::connect(&jmap_config).await?;
        conn_jmap.send_email(&jmap_config, &raw_message).await?;
    } else {
        log::debug!(
            "Sending via SMTP {}:{} as {}",
            account.smtp_host,
            account.smtp_port,
            account.email
        );

        // For O365: get SMTP-scoped OAuth token
        let (smtp_username, smtp_password, use_xoauth2) = if account.provider == "o365" {
            let tokens = crate::oauth::load_tokens(&account_id)?
                .ok_or_else(|| crate::error::Error::Other("No O365 tokens for SMTP".into()))?;
            let refresh_token = tokens.refresh_token
                .ok_or_else(|| crate::error::Error::Other("No O365 refresh token for SMTP".into()))?;
            let smtp_tokens = crate::oauth::refresh_with_scopes(
                &crate::oauth::MICROSOFT,
                &refresh_token,
                crate::oauth::MICROSOFT_IMAP_SCOPES, // SMTP.Send is in the same scope set
            ).await?;
            crate::oauth::store_tokens(&account_id, &crate::oauth::OAuthTokens {
                access_token: smtp_tokens.access_token.clone(),
                refresh_token: smtp_tokens.refresh_token,
                expires_at: smtp_tokens.expires_at,
            })?;
            (account.username.clone(), smtp_tokens.access_token, true)
        } else {
            (account.username.clone(), account.password.clone(), false)
        };

        let attachment_data = read_attachments(&message.attachments)?;
        smtp::send_message(
            &account.smtp_host,
            account.smtp_port,
            &smtp_username,
            &smtp_password,
            account.use_tls,
            use_xoauth2,
            &account.email,
            &message.to,
            &message.cc,
            &message.bcc,
            &message.subject,
            &message.body_text,
            message.body_html.as_deref(),
            &attachment_data,
        )
        .await?;
    }

    // Auto-collect recipients to "Collected Contacts"
    {
        let conn = state.db.lock().await;
        for addr in message.to.iter().chain(message.cc.iter()) {
            if let Err(e) = db::contacts::collect_contact(&conn, &account_id, addr, None) {
                log::warn!("Failed to collect contact '{}': {}", addr, e);
            }
        }
    }

    log::info!(
        "Message sent successfully for account {} to {:?}",
        account_id,
        message.to
    );

    Ok(())
}

#[tauri::command]
pub async fn save_draft(
    state: State<'_, AppState>,
    account_id: String,
    message: ComposeMessage,
) -> Result<()> {
    log::info!(
        "Save draft command: account={} subject='{}'",
        account_id,
        message.subject
    );

    let account = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)?
    };

    let attachment_data = read_attachments(&message.attachments)?;

    // Drafts may have no recipients — use sender as placeholder To for valid RFC5322
    let draft_to = if message.to.is_empty() && message.cc.is_empty() && message.bcc.is_empty() {
        vec![account.email.clone()]
    } else {
        message.to.clone()
    };

    let raw_message = smtp::build_raw_message(
        &account.email,
        &draft_to,
        &message.cc,
        &message.bcc,
        &message.subject,
        &message.body_text,
        message.body_html.as_deref(),
        &attachment_data,
    )?;

    if account.mail_protocol == "jmap" {
        let jmap_config = JmapConfig {
            jmap_url: account.jmap_url.clone(),
            email: account.email.clone(),
            username: account.username.clone(),
            password: account.password.clone(),
        };
        let conn_jmap = JmapConnection::connect(&jmap_config).await?;
        conn_jmap.save_draft(&jmap_config, &raw_message).await?;
    } else {
        // IMAP: append to Drafts folder (O365 uses XOAUTH2)
        let (imap_password, imap_xoauth2) = if account.provider == "o365" {
            let tokens = crate::oauth::load_tokens(&account.id)?
                .ok_or_else(|| Error::Other("No O365 tokens".into()))?;
            let refresh = tokens.refresh_token
                .ok_or_else(|| Error::Other("No O365 refresh token".into()))?;
            let new = crate::oauth::refresh_with_scopes(
                &crate::oauth::MICROSOFT, &refresh, crate::oauth::MICROSOFT_IMAP_SCOPES,
            ).await?;
            crate::oauth::store_tokens(&account.id, &new)?;
            (new.access_token, true)
        } else {
            (account.password.clone(), false)
        };
        tokio::task::spawn_blocking(move || {
            let imap_config = crate::mail::imap::ImapConfig {
                host: account.imap_host,
                port: account.imap_port,
                username: account.username,
                password: imap_password,
                use_tls: account.use_tls,
                use_xoauth2: imap_xoauth2,
            };
            let mut conn = crate::mail::imap::ImapConnection::connect(&imap_config)?;
            // Try common Drafts folder names
            let draft_folders = ["Drafts", "INBOX.Drafts", "[Gmail]/Drafts"];
            let mut saved = false;
            for folder in &draft_folders {
                match conn.append_message(folder, &raw_message) {
                    Ok(()) => {
                        saved = true;
                        break;
                    }
                    Err(e) => {
                        log::debug!("Draft folder '{}' failed: {}", folder, e);
                    }
                }
            }
            if !saved {
                return Err(crate::error::Error::Other("Could not find Drafts folder".into()));
            }
            conn.logout();
            Ok(())
        })
        .await
        .map_err(|e| crate::error::Error::Other(format!("Draft save task failed: {}", e)))??;
    }

    log::info!("Draft saved successfully for account {}", account_id);
    Ok(())
}

fn read_attachments(attachments: &[FileAttachment]) -> Result<Vec<smtp::AttachmentData>> {
    let mut result = Vec::new();
    for att in attachments {
        let path = std::path::Path::new(&att.path);

        // Validate: path must be absolute
        if !path.is_absolute() {
            return Err(crate::error::Error::Other(format!(
                "Attachment path must be absolute: '{}'", att.path
            )));
        }

        // Validate: path must not contain ".." components
        for component in path.components() {
            if matches!(component, std::path::Component::ParentDir) {
                return Err(crate::error::Error::Other(format!(
                    "Attachment path must not contain '..': '{}'", att.path
                )));
            }
        }

        // Warn about unusual paths (not under typical user directories)
        let path_str = att.path.as_str();
        let is_typical = if cfg!(windows) {
            path_str.starts_with("C:\\Users\\")
                || path_str.starts_with("C:\\Temp\\")
                || path_str.starts_with("C:\\Windows\\Temp\\")
        } else {
            path_str.starts_with("/home/")
                || path_str.starts_with("/tmp/")
                || path_str.starts_with("/Users/")
                || path_str.starts_with("/var/tmp/")
        };
        if !is_typical {
            log::warn!("Attachment from unusual path: '{}'", att.path);
        }

        let data = std::fs::read(path)
            .map_err(|e| crate::error::Error::Other(format!("Failed to read attachment '{}': {}", att.path, e)))?;
        let content_type = mime_guess::from_path(&att.name)
            .first_or_octet_stream()
            .to_string();
        log::info!("Attachment: {} ({}, {} bytes)", att.name, content_type, data.len());
        result.push(smtp::AttachmentData {
            name: att.name.clone(),
            content_type,
            data,
        });
    }
    Ok(result)
}
