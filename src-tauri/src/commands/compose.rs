use serde::Deserialize;
use tauri::{Emitter, State};

use crate::db;
use crate::error::{Error, Result};
use crate::mail::jmap::JmapConnection;
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

/// An attachment referenced by the renderer. `token` is the opaque handle
/// returned by `commands::attachments::pick_attachments`; the backend
/// resolves it to the real canonical path at send/save time.
///
/// `size` is accepted but ignored — the renderer carries it for UI
/// purposes and Tauri IPC round-trips the ComposeAttachment structure
/// verbatim. Declaring it here (instead of relying on serde's implicit
/// unknown-field tolerance) makes the contract explicit.
#[derive(Debug, Deserialize)]
pub struct FileAttachment {
    pub token: String,
    pub name: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub size: Option<u64>,
}

/// Send an email. Validates and reads attachments synchronously, then spawns
/// the actual network send in the background so the compose window can close
/// immediately. Emits `send-started`, `send-complete`, or `send-failed` events
/// to the main window for status tracking.
#[tauri::command]
pub async fn send_message(
    app: tauri::AppHandle,
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
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    // --- Synchronous part: validate, read attachments, build message ---
    // This is fast (local I/O only) so the compose window waits for it.
    // We *peek* tokens for the build so a failure here (e.g. file
    // removed between pick and send) leaves the registry intact and the
    // user can fix it and retry. Tokens are released only after the
    // message bytes are safely persisted to the outbox; from that point
    // on the outbox owns retry.
    let tokens: Vec<String> = message
        .attachments
        .iter()
        .map(|a| a.token.clone())
        .collect();
    let names: Vec<String> = message.attachments.iter().map(|a| a.name.clone()).collect();
    let paths = crate::commands::attachments::peek_tokens(&state, &tokens)?;
    let attachment_data = build_attachment_data(&paths, &names)?;
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

    // For O365 SMTP: refresh OAuth token now (needs keyring access)
    let smtp_creds = if account.mail_protocol != "jmap" && account.provider == "o365" {
        let tokens = crate::oauth::load_tokens(&account_id)?
            .ok_or_else(|| Error::Other("No O365 tokens for SMTP".into()))?;
        let refresh_token = tokens
            .refresh_token
            .ok_or_else(|| Error::Other("No O365 refresh token for SMTP".into()))?;
        let smtp_tokens = crate::oauth::refresh_with_scopes(
            &crate::oauth::MICROSOFT,
            &refresh_token,
            crate::oauth::MICROSOFT_IMAP_SCOPES,
        )
        .await?;
        crate::oauth::store_tokens(
            &account_id,
            &crate::oauth::OAuthTokens {
                access_token: smtp_tokens.access_token.clone(),
                refresh_token: smtp_tokens.refresh_token,
                expires_at: smtp_tokens.expires_at,
            },
        )?;
        Some((account.username.clone(), smtp_tokens.access_token, true))
    } else {
        None
    };

    // Notify main window that send is starting
    let subject_display = if message.subject.is_empty() {
        "(no subject)".to_string()
    } else {
        message.subject.clone()
    };
    app.emit(
        "send-started",
        serde_json::json!({
            "account_id": account_id,
            "subject": subject_display,
        }),
    )
    .ok();

    // --- Persist to outbox before spawning background send ---
    // This ensures the message survives a crash during sending.
    let raw_message_b64 = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&raw_message)
    };
    let send_payload = serde_json::json!({
        "raw_message_b64": raw_message_b64,
        "subject": subject_display,
    });
    let outbox_id = {
        let conn = state.db.writer().await;
        crate::ops::offline::queue_offline_op(&conn, &account_id, "send", &send_payload)?
    };
    log::info!(
        "Persisted send to outbox (id={}) for account {}",
        outbox_id,
        account_id
    );

    // From here on the outbox owns the payload — the attachment bytes
    // are already inlined in raw_message. Releasing tokens is safe even
    // if the background send retries, and prevents the registry from
    // leaking paths for the lifetime of the process.
    crate::commands::attachments::release_tokens(&state, &tokens);

    // --- Background: actual network send ---
    // The command returns Ok(()) here so the compose window can close.
    let app_bg = app.clone();
    let account_id_bg = account_id.clone();
    let subject_bg = subject_display.clone();
    let db_bg = state.db.clone();
    let recipients: Vec<String> = message
        .to
        .iter()
        .chain(message.cc.iter())
        .cloned()
        .collect();

    tokio::spawn(async move {
        let result: std::result::Result<(), Error> = async {
            if account.mail_protocol == "jmap" {
                log::info!("Sending via JMAP for account {}", account.email);
                let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;
                let conn_jmap = JmapConnection::connect(&jmap_config).await?;
                conn_jmap.send_email(&jmap_config, &raw_message).await?;
            } else {
                let (smtp_username, smtp_password, use_xoauth2) = smtp_creds
                    .unwrap_or_else(|| (account.username.clone(), account.password.clone(), false));

                log::info!(
                    "Sending via SMTP {}:{} as {}",
                    account.smtp_host,
                    account.smtp_port,
                    account.email
                );
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
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                log::info!("Message sent successfully for account {}", account_id_bg);
                // Remove from outbox on success
                let conn = db_bg.writer().await;
                if let Err(e) = crate::ops::offline::mark_completed(&conn, outbox_id) {
                    log::warn!("Failed to remove sent message from outbox: {}", e);
                }
                app_bg
                    .emit(
                        "send-complete",
                        serde_json::json!({
                            "account_id": account_id_bg,
                            "subject": subject_bg,
                        }),
                    )
                    .ok();

                // Auto-collect recipients to "Collected Contacts"
                let conn = db_bg.writer().await;
                for addr in &recipients {
                    if let Err(e) = db::contacts::collect_contact(&conn, &account_id_bg, addr, None)
                    {
                        log::warn!("Failed to collect contact '{}': {}", addr, e);
                    }
                }
            }
            Err(e) => {
                log::error!("Send failed for account {}: {}", account_id_bg, e);
                // Leave the message in the outbox for retry (mark as failed)
                let conn = db_bg.writer().await;
                let _ = crate::ops::offline::mark_failed(&conn, outbox_id, &e.to_string());
                app_bg
                    .emit(
                        "send-failed",
                        serde_json::json!({
                            "account_id": account_id_bg,
                            "subject": subject_bg,
                            "error": e.to_string(),
                        }),
                    )
                    .ok();
            }
        }
    });

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
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    // Drafts peek rather than consume tokens: the user may save a draft
    // and keep editing, so we must keep the token → path mapping alive.
    let tokens: Vec<String> = message
        .attachments
        .iter()
        .map(|a| a.token.clone())
        .collect();
    let names: Vec<String> = message.attachments.iter().map(|a| a.name.clone()).collect();
    let paths = crate::commands::attachments::peek_tokens(&state, &tokens)?;
    let attachment_data = build_attachment_data(&paths, &names)?;

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

    if account.mail_protocol == "graph" {
        // Save draft via Graph API: POST /me/messages creates a draft without sending
        log::info!(
            "Saving draft via Microsoft Graph for account {}",
            account.email
        );
        let token = crate::mail::graph::get_graph_token(&account_id).await?;
        let client = crate::mail::graph::GraphClient::new(&token);
        client
            .save_draft(&crate::mail::graph::GraphSendMessage {
                to: message.to.clone(),
                cc: message.cc.clone(),
                bcc: message.bcc.clone(),
                subject: message.subject.clone(),
                body_text: message.body_text.clone(),
            })
            .await?;
    } else if account.mail_protocol == "jmap" {
        let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;
        let conn_jmap = JmapConnection::connect(&jmap_config).await?;
        conn_jmap.save_draft(&jmap_config, &raw_message).await?;
    } else {
        // IMAP: append to Drafts folder (O365 uses XOAUTH2)
        let (imap_password, imap_xoauth2) = if account.provider == "o365" {
            let tokens = crate::oauth::load_tokens(&account.id)?
                .ok_or_else(|| Error::Other("No O365 tokens".into()))?;
            let refresh = tokens
                .refresh_token
                .ok_or_else(|| Error::Other("No O365 refresh token".into()))?;
            let new = crate::oauth::refresh_with_scopes(
                &crate::oauth::MICROSOFT,
                &refresh,
                crate::oauth::MICROSOFT_IMAP_SCOPES,
            )
            .await?;
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
                return Err(crate::error::Error::Other(
                    "Could not find Drafts folder".into(),
                ));
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

/// Read each resolved attachment path, pair it with its display name and
/// guessed content type, and return the payload structures the SMTP layer
/// wants. Paths are trusted — they come from the backend-owned attachment
/// registry after a user-initiated native file pick.
fn build_attachment_data(
    paths: &[std::path::PathBuf],
    names: &[String],
) -> Result<Vec<smtp::AttachmentData>> {
    if paths.len() != names.len() {
        return Err(crate::error::Error::Other(format!(
            "Attachment path/name length mismatch: {} paths for {} names",
            paths.len(),
            names.len()
        )));
    }
    let mut result = Vec::with_capacity(paths.len());
    for (path, name) in paths.iter().zip(names.iter()) {
        let data = std::fs::read(path).map_err(|e| {
            crate::error::Error::Other(format!(
                "Failed to read attachment '{}': {}",
                path.display(),
                e
            ))
        })?;
        let content_type = mime_guess::from_path(name)
            .first_or_octet_stream()
            .to_string();
        log::info!(
            "Attachment: {} ({}, {} bytes)",
            name,
            content_type,
            data.len()
        );
        result.push(smtp::AttachmentData {
            name: name.clone(),
            content_type,
            data,
        });
    }
    Ok(result)
}
