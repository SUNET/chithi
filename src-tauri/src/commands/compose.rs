use serde::Deserialize;
use tauri::{Emitter, State};

use crate::db;
use crate::error::{Error, Result};
use crate::mail::jmap::JmapConnection;
use crate::mail::msgid::normalize_message_id;
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
    /// chithi's internal id of the message we're replying to. The backend
    /// looks up the original's RFC 5322 Message-ID and References chain
    /// and writes proper In-Reply-To / References headers on the outgoing
    /// message. None for new conversations.
    #[serde(default)]
    pub reply_to_message_id: Option<String>,
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

    // Resolve threading headers from the original message we're replying
    // to (if any). This is what makes the next sync render the new
    // message under its parent in the thread tree.
    let (in_reply_to, references) =
        resolve_reply_headers(&state, &account_id, message.reply_to_message_id.as_deref());

    // `raw_message` carries the inlined attachment bytes — wrap in `Arc`
    // so the background spawn can share the buffer with the IMAP APPEND
    // path without deep-cloning megabytes of MIME each send.
    let raw_message: std::sync::Arc<[u8]> = smtp::build_raw_message(
        &account.email,
        &message.to,
        &message.cc,
        &message.bcc,
        &message.subject,
        &message.body_text,
        message.body_html.as_deref(),
        &attachment_data,
        in_reply_to.as_deref(),
        &references,
    )?
    .into();

    // For O365 SMTP: refresh OAuth token now (needs keyring access)
    let smtp_creds =
        if account.mail_protocol_str() != "jmap" && account.auth_method == "oauth-microsoft" {
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
    // The row is inserted with status 'sending' so the worker's replay
    // loop won't pick it up while the first attempt is still in flight.
    // On success the row is deleted; on failure it flips to 'pending'
    // and the next post-sync drain replays it via the worker.
    let raw_message_b64 = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&raw_message)
    };
    let send_payload = serde_json::json!({
        "raw_message_b64": raw_message_b64,
        "subject": subject_display,
        "from": account.email,
        "to": message.to,
        "cc": message.cc,
        "bcc": message.bcc,
    });
    let outbox_id = {
        let conn = state.db.writer().await;
        crate::ops::offline::queue_offline_op_with_status(
            &conn,
            &account_id,
            "send",
            &send_payload,
            "sending",
        )?
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
    // Capture the worker's op-sender now so the spawn can enqueue a
    // Sent-folder sync after a successful APPEND (#189). `state` is
    // not `'static`, so the sender must be cloned out before the move.
    let op_sender_bg = state.get_op_sender(&account_id, &app);
    let recipients: Vec<String> = message
        .to
        .iter()
        .chain(message.cc.iter())
        .cloned()
        .collect();

    tokio::spawn(async move {
        let result: std::result::Result<(), Error> = async {
            if account.mail_protocol_str() == "jmap" {
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
                    in_reply_to.as_deref(),
                    &references,
                )
                .await?;

                // Best-effort: APPEND the sent message to the IMAP Sent
                // folder (#189). SMTP submission alone never writes to
                // Sent for plain IMAP+SMTP or Exchange-via-SMTP-AUTH;
                // JMAP / Graph send APIs are unaffected because they
                // populate Sent server-side. Failures here MUST NOT
                // propagate — the message has been delivered, and a
                // retried send would duplicate it for the recipient.
                // Read-only lookup — use the pool's reader so we don't
                // serialize on the single-writer mutex.
                let sent_folder_path = {
                    let conn = db_bg.reader();
                    crate::db::folders::folder_path_by_type(&conn, &account_id_bg, "sent")
                        .ok()
                        .flatten()
                };
                let imap_config = crate::mail::imap::ImapConfig {
                    host: account.imap_host.clone(),
                    port: account.imap_port,
                    username: smtp_username.clone(),
                    password: smtp_password.clone(),
                    use_tls: account.use_tls,
                    use_xoauth2,
                };
                // `Arc::clone` of `Arc<[u8]>` is a refcount bump — does
                // not duplicate the inlined-attachment bytes (which can
                // be many MB on a send with large attachments).
                let raw_for_append = std::sync::Arc::clone(&raw_message);
                let account_id_append = account_id_bg.clone();
                let append_result = tokio::task::spawn_blocking(move || {
                    crate::mail::imap::append_message_to_sent(
                        &imap_config,
                        sent_folder_path.as_deref(),
                        &raw_for_append,
                    )
                })
                .await;
                match append_result {
                    Ok(Ok(sent_folder)) => {
                        log::info!(
                            "APPENDed sent message to '{}' for account {}",
                            sent_folder,
                            account_id_append
                        );
                        // Nudge a targeted sync so the freshly-APPENDed
                        // message shows up in the UI immediately instead
                        // of waiting for the next scheduled sync.
                        let send_res = op_sender_bg
                            .send(crate::ops::queue::OpEntry {
                                op: crate::ops::queue::MailOp::SyncFolder {
                                    folder_path: sent_folder,
                                },
                                priority: crate::ops::queue::OpPriority::User,
                            })
                            .await;
                        if let Err(e) = send_res {
                            log::warn!(
                                "Failed to queue Sent-folder sync for account {}: {}",
                                account_id_append,
                                e
                            );
                        }
                    }
                    Ok(Err(e)) => log::warn!(
                        "Sent delivered but APPEND to Sent failed for account {}: {}",
                        account_id_append,
                        e
                    ),
                    Err(e) => {
                        let kind = if e.is_panic() {
                            "panicked"
                        } else if e.is_cancelled() {
                            "cancelled"
                        } else {
                            "failed"
                        };
                        log::warn!(
                            "APPEND-to-Sent task {} for account {}: {}",
                            kind,
                            account_id_append,
                            e
                        );
                    }
                }
            }
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                log::info!("Message sent successfully for account {}", account_id_bg);
                // Each writer acquire is scoped: shadowing the previous
                // `conn` would NOT drop the old guard before the new
                // `lock().await`, self-deadlocking on the same mutex.
                {
                    let conn = db_bg.writer().await;
                    if let Err(e) = crate::ops::offline::mark_completed(&conn, outbox_id) {
                        log::warn!("Failed to remove sent message from outbox: {}", e);
                    }
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
                {
                    let conn = db_bg.writer().await;
                    for addr in &recipients {
                        if let Err(e) =
                            db::contacts::collect_contact(&conn, &account_id_bg, addr, None)
                        {
                            log::warn!("Failed to collect contact '{}': {}", addr, e);
                        }
                    }
                }
            }
            Err(e) => {
                log::error!("Send failed for account {}: {}", account_id_bg, e);
                // Flip the row from 'sending' back to 'pending' and record
                // the error so the worker replays it on the next sync.
                let conn = db_bg.writer().await;
                let _ = crate::ops::offline::mark_failed(&conn, outbox_id, &e.to_string());
                let _ = crate::ops::offline::mark_pending(&conn, outbox_id);
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

    // Drafts also carry the threading headers so that, once sent, the
    // outgoing message threads correctly without requiring the user to
    // re-trigger reply context.
    let (in_reply_to, references) =
        resolve_reply_headers(&state, &account_id, message.reply_to_message_id.as_deref());

    let raw_message = smtp::build_raw_message(
        &account.email,
        &draft_to,
        &message.cc,
        &message.bcc,
        &message.subject,
        &message.body_text,
        message.body_html.as_deref(),
        &attachment_data,
        in_reply_to.as_deref(),
        &references,
    )?;

    if account.mail_protocol_str() == "graph" {
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
    } else if account.mail_protocol_str() == "jmap" {
        let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;
        let conn_jmap = JmapConnection::connect(&jmap_config).await?;
        conn_jmap.save_draft(&jmap_config, &raw_message).await?;
    } else {
        // IMAP: append to Drafts folder (O365 uses XOAUTH2)
        let (imap_password, imap_xoauth2) = if account.auth_method == "oauth-microsoft" {
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

/// Resolve the In-Reply-To and References values for a reply.
///
/// `reply_to_id` is chithi's internal id of the message being replied to.
/// We look up its RFC 5322 Message-ID and walk the in_reply_to chain
/// backwards until we either run out or hit a cycle, building the
/// References list root-first. `In-Reply-To` is the immediate parent's
/// Message-ID. Both come back already wrapped in angle brackets when
/// the underlying database row is wrapped that way.
///
/// Returns (None, []) if `reply_to_id` is absent, the row is missing,
/// or the original has no Message-ID we can chain off.
fn resolve_reply_headers(
    state: &State<'_, AppState>,
    account_id: &str,
    reply_to_id: Option<&str>,
) -> (Option<String>, Vec<String>) {
    let Some(reply_to_id) = reply_to_id else {
        return (None, Vec::new());
    };
    if reply_to_id.is_empty() {
        return (None, Vec::new());
    }

    let conn = state.db.reader();
    // Fetch (message_id, in_reply_to) for the original.
    let parent: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT message_id, in_reply_to FROM messages
             WHERE account_id = ?1 AND id = ?2 LIMIT 1",
            rusqlite::params![account_id, reply_to_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    let Some((Some(parent_mid_raw), parent_irt_raw)) = parent else {
        return (None, Vec::new());
    };
    // Older DB rows can carry leading whitespace or be missing brackets.
    // Canonicalize before we hand the value to lettre's header builder so
    // the outgoing In-Reply-To / References match what receiving clients
    // and our own thread-id lookups will expect.
    let Some(parent_mid) = normalize_message_id(&parent_mid_raw) else {
        return (None, Vec::new());
    };

    let mut chain: Vec<String> = Vec::new();
    if let Some(irt) = parent_irt_raw.as_deref().and_then(normalize_message_id) {
        chain.push(irt);
    }

    // Walk backwards via in_reply_to. Cap depth to keep the header
    // bounded; mailing-list threads occasionally chain very long.
    let mut current = chain.first().cloned();
    let mut visited = std::collections::HashSet::new();
    while let Some(mid) = current {
        if !visited.insert(mid.clone()) {
            break;
        }
        if visited.len() > 32 {
            break;
        }
        let next: Option<Option<String>> = conn
            .query_row(
                "SELECT in_reply_to FROM messages
                 WHERE account_id = ?1 AND message_id = ?2 LIMIT 1",
                rusqlite::params![account_id, mid],
                |row| row.get(0),
            )
            .ok();
        match next {
            Some(Some(irt_raw)) => {
                if let Some(irt) = normalize_message_id(&irt_raw) {
                    chain.push(irt.clone());
                    current = Some(irt);
                } else {
                    break;
                }
            }
            _ => break,
        }
    }

    // Chain is collected youngest-first; reverse so References reads
    // root-first per RFC 5322 §3.6.4. The immediate parent's
    // Message-ID always sits at the end of References.
    chain.reverse();
    chain.push(parent_mid.clone());

    (Some(parent_mid), chain)
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
