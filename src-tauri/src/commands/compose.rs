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
    /// OpenPGP sign / encrypt toggles. Both default to off (plain mail).
    #[serde(default)]
    pub pgp_sign: bool,
    #[serde(default)]
    pub pgp_encrypt: bool,
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
    window: tauri::Window,
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

    let plain_raw = smtp::build_raw_message(
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

    // PGP wrap (RFC 3156) if the compose toggles asked for it. Sign or
    // encrypt — or both, in which case we sign-then-encrypt in a single
    // OpenPGP message and wrap in multipart/encrypted (RFC 3156 §6.1).
    let raw_message = if message.pgp_sign || message.pgp_encrypt {
        let origin = window.label().to_string();
        apply_pgp_envelope(
            &app,
            &state,
            &account,
            &plain_raw,
            &message.to,
            &message.cc,
            &message.bcc,
            message.pgp_sign,
            message.pgp_encrypt,
            Some(&origin),
        )
        .await?
    } else {
        plain_raw
    };

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
                // Send the already-built `raw_message` bytes verbatim.
                // PGP/MIME wrapping (when pgp_sign/pgp_encrypt is set) is
                // applied to those bytes upstream; rebuilding the message
                // from structured fields here would discard the wrapping
                // and leak the cleartext on the wire.
                smtp::send_raw(
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
                    &raw_message,
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

/// Wrap the plain `raw_message` bytes in a PGP/MIME envelope. Resolves
/// the signer key from `account.email`, prompts for the passphrase via
/// the global pgp-secret-needed event (caching in
/// `state.pgp_cache`), and produces a multipart/signed and/or
/// multipart/encrypted message per RFC 3156. When both toggles are set
/// we do sign-then-encrypt (signature lives inside the ciphertext) and
/// wrap in multipart/encrypted, which is what every modern MUA
/// recognises and what Thunderbird/GPG round-trip cleanly.
async fn apply_pgp_envelope(
    app: &tauri::AppHandle,
    state: &State<'_, AppState>,
    account: &crate::db::accounts::AccountFull,
    raw_message: &[u8],
    to: &[String],
    cc: &[String],
    bcc: &[String],
    sign: bool,
    encrypt: bool,
    origin_window: Option<&str>,
) -> Result<Vec<u8>> {
    use crate::commands::pgp::{acquire_secret, SecretKind};
    use crate::mail::pgp_mime::canonicalize_for_signing;
    use crate::mail::smtp::{inner_part_of, wrap_pgp_mime_encrypted, wrap_pgp_mime_signed};

    if !sign && !encrypt {
        return Err(Error::Other(
            "apply_pgp_envelope called with no flags set".into(),
        ));
    }

    let store = state
        .pgp_store()
        .map_err(|e| Error::Other(format!("openpgp keystore: {e}")))?;

    // Resolve signer key by From email. `signer_data` is the keystore's
    // stored representation — for software signers it includes the
    // secret packets; for card-resident signers it's public-only and
    // the secret material lives on the card (gated by the PIN).
    let (signer_data, signer_info) = {
        let guard = store.lock().expect("pgp keystore mutex poisoned");
        libtumpa::store::resolve_signer(&guard, &account.email).map_err(|e| {
            Error::Other(format!("openpgp: signer lookup for {}: {e}", account.email))
        })?
    };

    // Decide signer backend. Mirrors what libtumpa's own sign/encrypt
    // paths do: `find_signing_card` reports the card whose signing slot
    // holds the signer key (if any). When that returns Some we MUST go
    // through the card path; passing `signer_data` (public-only) into
    // the software API would parse as an empty secret-key set, which is
    // exactly the "unknown packet header" / "expected SecretKey, got
    // PublicKey" failure we see today.
    let card_match = if sign {
        let signer_pub = signer_data.clone();
        tokio::task::spawn_blocking(move || {
            libtumpa::encrypt::find_signing_card_for_encrypt(&signer_pub)
        })
        .await
        .map_err(|e| Error::Other(format!("pgp card lookup task join failed: {e}")))?
        .unwrap_or(None)
    } else {
        None
    };

    let reason = if encrypt && sign {
        "Sign and encrypt outgoing message"
    } else if encrypt {
        "Encrypt outgoing message"
    } else {
        "Sign outgoing message"
    };

    enum SignerSecret {
        Passphrase(libtumpa::Passphrase),
        CardPin { pin: libtumpa::Pin, ident: String },
        None,
    }

    // Track which cache key the active secret lives under (card ident for
    // PIN, fingerprint for passphrase). If signing/encryption fails the
    // secret is wrong, so we evict it and the next retry re-prompts. With
    // the always-cache policy this is the only path that drops a stale
    // entry mid-session — without it, a single mistyped PIN would loop
    // forever.
    let mut cache_target: Option<String> = None;
    let signer_secret: SignerSecret = if !sign {
        SignerSecret::None
    } else if let Some(ref m) = card_match {
        let pin_str = acquire_secret(
            app,
            state.pgp_pending_secrets.clone(),
            state.pgp_cache.clone(),
            SecretKind::Pin,
            &m.card.ident,
            reason,
            origin_window,
        )
        .await?;
        cache_target = Some(m.card.ident.clone());
        SignerSecret::CardPin {
            pin: libtumpa::Pin::new(pin_str.as_bytes().to_vec()),
            ident: m.card.ident.clone(),
        }
    } else {
        let pass_str = acquire_secret(
            app,
            state.pgp_pending_secrets.clone(),
            state.pgp_cache.clone(),
            SecretKind::Passphrase,
            &signer_info.fingerprint,
            reason,
            origin_window,
        )
        .await?;
        cache_target = Some(signer_info.fingerprint.clone());
        SignerSecret::Passphrase(libtumpa::Passphrase::new(pass_str.to_string()))
    };

    let inner = inner_part_of(raw_message)?;
    let canonical_inner = canonicalize_for_signing(&inner);

    if encrypt {
        // Sign-then-encrypt (or encrypt-only) into one OpenPGP message,
        // then wrap in multipart/encrypted. Recipients = To + Cc + Bcc +
        // the sender's own address ("encrypt-to-self") so the user can
        // decrypt messages from their Sent folder later. If the sender
        // has no usable public key in the local keystore we skip self
        // and log a warning — failing the send would be worse than
        // shipping a message the sender can't read back.
        let mut recipients: Vec<String> = to
            .iter()
            .chain(cc.iter())
            .chain(bcc.iter())
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .collect();
        if has_resolvable_public_key(&store, &account.email) {
            if !recipients_contain(&recipients, &account.email) {
                recipients.push(account.email.clone());
            }
        } else {
            log::warn!(
                "openpgp: skipping encrypt-to-self for {} — no usable public key in keystore; \
                 the sender will NOT be able to decrypt this message from Sent",
                account.email
            );
        }

        let store_for_blocking = store.clone();
        let signer_data_clone = signer_data.clone();
        let canonical_inner_clone = canonical_inner.clone();
        let recipients_owned: Vec<String> = recipients;

        let join_result = tokio::task::spawn_blocking(move || -> libtumpa::Result<Vec<u8>> {
            let guard = store_for_blocking
                .lock()
                .expect("pgp keystore mutex poisoned");
            let recipient_refs: Vec<&str> = recipients_owned.iter().map(|s| s.as_str()).collect();
            match signer_secret {
                SignerSecret::None => libtumpa::encrypt::encrypt_to_recipients(
                    &guard,
                    &recipient_refs,
                    &canonical_inner_clone,
                    true,
                ),
                SignerSecret::Passphrase(pass) => {
                    libtumpa::encrypt::sign_and_encrypt_to_recipients(
                        &guard,
                        &signer_data_clone,
                        &pass,
                        &recipient_refs,
                        &canonical_inner_clone,
                        true,
                    )
                }
                SignerSecret::CardPin { pin, ident } => {
                    libtumpa::encrypt::sign_and_encrypt_on_card_to_recipients(
                        &guard,
                        &signer_data_clone,
                        &pin,
                        Some(&ident),
                        &recipient_refs,
                        &canonical_inner_clone,
                        true,
                    )
                }
            }
        })
        .await
        .map_err(|e| Error::Other(format!("pgp encrypt task join failed: {e}")))?;
        let armored = match join_result {
            Ok(bytes) => bytes,
            Err(e) => {
                if let Some(t) = cache_target.as_deref() {
                    crate::commands::pgp::evict_cached_secret(&state.pgp_cache, t);
                }
                return Err(Error::Other(format!("openpgp encrypt: {e}")));
            }
        };
        let armored_str = String::from_utf8(armored)
            .map_err(|e| Error::Other(format!("openpgp encrypt produced non-utf8: {e}")))?;
        wrap_pgp_mime_encrypted(raw_message, &armored_str)
    } else {
        // Sign-only: detached signature over the canonicalised inner,
        // multipart/signed wrapper. libtumpa's sign_detached_with_hash
        // takes a closure that asks for the right secret kind based on
        // the resolved signer backend, so we match the request to the
        // pre-acquired SignerSecret.
        use libtumpa::sign::{sign_detached_with_hash, Secret, SecretRequest};
        let store_for_blocking = store.clone();
        let signer_data_clone = signer_data.clone();
        let signer_info_clone = signer_info.clone();
        let canonical_inner_clone = canonical_inner.clone();

        let join_result = tokio::task::spawn_blocking(move || {
            let _guard = store_for_blocking
                .lock()
                .expect("pgp keystore mutex poisoned");
            sign_detached_with_hash(
                &signer_data_clone,
                &signer_info_clone,
                &canonical_inner_clone,
                None,
                |req: SecretRequest<'_>| match (&signer_secret, req) {
                    (SignerSecret::Passphrase(p), SecretRequest::KeyPassphrase { .. }) => {
                        Ok(Secret::Passphrase(p.clone()))
                    }
                    (SignerSecret::CardPin { pin, .. }, SecretRequest::CardPin { .. }) => {
                        Ok(Secret::Pin(pin.clone()))
                    }
                    _ => Err(libtumpa::Error::Sign(
                        "signer-secret mismatch between what libtumpa asked for and what \
                         we acquired"
                            .into(),
                    )),
                },
            )
        })
        .await
        .map_err(|e| Error::Other(format!("pgp sign task join failed: {e}")))?;
        let detached = match join_result {
            Ok(d) => d,
            Err(e) => {
                if let Some(t) = cache_target.as_deref() {
                    crate::commands::pgp::evict_cached_secret(&state.pgp_cache, t);
                }
                return Err(Error::Other(format!("openpgp sign: {e}")));
            }
        };

        let micalg = format!("pgp-{}", hash_alg_to_micalg(detached.hash_algorithm));
        wrap_pgp_mime_signed(raw_message, &detached.armored, &micalg)
    }
}

/// Case-insensitive membership check against an email recipient list.
/// We dedup the encrypt-to-self addition so the OpenPGP message doesn't
/// carry two PKESK packets for the same key (correct but wasteful) and
/// so libtumpa's `resolve_recipient_keys` doesn't trip on a duplicate.
fn recipients_contain(list: &[String], email: &str) -> bool {
    let needle = extract_addr_spec(email);
    list.iter()
        .any(|r| extract_addr_spec(r).eq_ignore_ascii_case(&needle))
}

/// Probe the keystore for a public encryption key usable for `id`. Returns
/// true iff `resolve_recipient` returns Ok AND the key passes
/// `ensure_key_usable_for_encryption`. Used as a precheck for the
/// encrypt-to-self addition so we can skip it instead of failing the send.
fn has_resolvable_public_key(
    store: &std::sync::Arc<std::sync::Mutex<libtumpa::KeyStore>>,
    id: &str,
) -> bool {
    let guard = store.lock().expect("pgp keystore mutex poisoned");
    match libtumpa::store::resolve_recipient(&guard, id) {
        Ok((_data, info)) => libtumpa::store::ensure_key_usable_for_encryption(&info).is_ok(),
        Err(_) => false,
    }
}

/// Strip a display name to just the addr-spec ("Alice <a@x>" -> "a@x").
/// Matches what lettre / SMTP envelopes carry on the wire.
fn extract_addr_spec(addr: &str) -> String {
    let trimmed = addr.trim();
    if let (Some(lt), Some(gt)) = (trimmed.find('<'), trimmed.rfind('>')) {
        if lt < gt {
            return trimmed[lt + 1..gt].trim().to_string();
        }
    }
    trimmed.to_string()
}

fn hash_alg_to_micalg(h: libtumpa::HashAlgorithm) -> &'static str {
    use libtumpa::HashAlgorithm as H;
    // Lowercase OpenPGP hash name without the "pgp-" prefix; the wrapper
    // adds the prefix. Mapping per RFC 3156 §5 + RFC 4880 §9.4.
    match h {
        H::Sha1 => "sha1",
        H::Sha224 => "sha224",
        H::Sha256 => "sha256",
        H::Sha384 => "sha384",
        H::Sha512 => "sha512",
        H::Sha3_256 => "sha3-256",
        H::Sha3_512 => "sha3-512",
        _ => "sha256",
    }
}

/// Frontend pre-check for the compose UI. For each recipient, reports
/// whether a public encryption key is already in the keystore. The UI
/// turns each badge red/green and offers a "fetch via WKD" action for
/// missing ones.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PgpRecipientStatus {
    pub email: String,
    pub has_key: bool,
    pub fingerprint: Option<String>,
}

#[tauri::command]
pub async fn pgp_check_recipients(
    state: State<'_, AppState>,
    recipients: Vec<String>,
) -> Result<Vec<PgpRecipientStatus>> {
    let store = state
        .pgp_store()
        .map_err(|e| Error::Other(format!("openpgp keystore: {e}")))?;
    let guard = store.lock().expect("pgp keystore mutex poisoned");
    let mut out = Vec::with_capacity(recipients.len());
    for email in recipients {
        let trimmed = email.trim().to_string();
        if trimmed.is_empty() {
            continue;
        }
        let status = match libtumpa::store::resolve_recipient(&guard, &trimmed) {
            Ok((_data, info)) => PgpRecipientStatus {
                email: trimmed,
                has_key: true,
                fingerprint: Some(info.fingerprint),
            },
            Err(_) => PgpRecipientStatus {
                email: trimmed,
                has_key: false,
                fingerprint: None,
            },
        };
        out.push(status);
    }
    Ok(out)
}

#[cfg(test)]
mod encrypt_to_self_tests {
    use super::{extract_addr_spec, recipients_contain};

    /// Regression: encrypt-to-self must not double-add the sender if
    /// they're already explicitly in To/Cc/Bcc. Two PKESK packets for the
    /// same key is correct OpenPGP but wasteful, and libtumpa's
    /// `resolve_recipient_keys` will refuse the duplicate.
    #[test]
    fn recipients_contain_matches_addr_spec_only() {
        let list = vec![
            "Alice <alice@example.com>".to_string(),
            "bob@example.com".to_string(),
        ];
        // bare addr-spec match
        assert!(recipients_contain(&list, "alice@example.com"));
        // case-insensitive
        assert!(recipients_contain(&list, "ALICE@example.com"));
        // display-name on the candidate side, addr-spec match still wins
        assert!(recipients_contain(
            &list,
            "Alice In Wonderland <alice@example.com>"
        ));
        // unrelated
        assert!(!recipients_contain(&list, "carol@example.com"));
    }

    /// `extract_addr_spec` is what the SMTP envelope sees. We rely on it
    /// to strip a display name before comparing — if it returned the full
    /// "Alice <alice@x>" string we'd double-add the sender every time.
    #[test]
    fn extract_addr_spec_strips_display_name() {
        assert_eq!(extract_addr_spec("alice@example.com"), "alice@example.com");
        assert_eq!(
            extract_addr_spec("Alice <alice@example.com>"),
            "alice@example.com"
        );
        // Whitespace inside angle brackets gets trimmed too.
        assert_eq!(extract_addr_spec("Alice <  alice@x  >"), "alice@x");
        // Malformed input (lone '<' or '>') falls through to a trim.
        assert_eq!(extract_addr_spec("  bob@x  "), "bob@x");
    }
}
