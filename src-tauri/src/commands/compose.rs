use serde::Deserialize;
use tauri::State;

use crate::db;
use crate::error::Result;
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

    if account.mail_protocol == "jmap" {
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

        let attachment_data = read_attachments(&message.attachments)?;
        smtp::send_message(
            &account.smtp_host,
            account.smtp_port,
            &account.username,
            &account.password,
            account.use_tls,
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

    log::info!(
        "Message sent successfully for account {} to {:?}",
        account_id,
        message.to
    );

    Ok(())
}

fn read_attachments(attachments: &[FileAttachment]) -> Result<Vec<smtp::AttachmentData>> {
    let mut result = Vec::new();
    for att in attachments {
        let data = std::fs::read(&att.path)
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
