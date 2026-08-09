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
    /// "Compose in Markdown" toggle. When true, `body_text` is treated as
    /// Markdown source: the backend renders it to safe HTML and ships a
    /// `multipart/alternative` carrying the Markdown source as the
    /// plaintext part and the rendered HTML as the html part. Ignored
    /// when `body_html` is already populated (an explicit HTML body wins).
    #[serde(default)]
    pub markdown: bool,
}

/// Resolve the effective HTML body for an outgoing message.
///
/// An explicit `body_html` always wins — the renderer already decided
/// the HTML. Otherwise, when the "Compose in Markdown" toggle is on, the
/// Markdown source in `body_text` is rendered to HTML with the `markdown`
/// crate's GFM flavour. Raw-HTML passthrough and dangerous protocols stay
/// disabled (markdown-rs's default), so the output is safe to send
/// without further sanitisation. On the rare render error we log and fall
/// back to `None` so the message still goes out as plaintext-only rather
/// than failing the send.
///
/// Shared by every send path (and reusable by the mobile composer once it
/// exists) so Markdown rendering lives in one place, not per-platform.
fn resolve_body_html(message: &ComposeMessage) -> Option<String> {
    if let Some(html) = &message.body_html {
        return Some(html.clone());
    }
    if !message.markdown {
        return None;
    }
    match markdown::to_html_with_options(&message.body_text, &markdown::Options::gfm()) {
        Ok(html) => Some(html),
        Err(e) => {
            log::warn!("Markdown render failed, sending plaintext only: {e}");
            None
        }
    }
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
    let mut attachment_data = build_attachment_data(&paths, &names)?;

    // Feature: attach sender's public key when adding an OpenPGP digital
    // signature. Per-account toggle (`pgp_attach_pubkey_on_sign`,
    // default-on); only fires when the message is being signed (whether
    // signed-only or signed-then-encrypted). The pubkey is added as
    // another attachment BEFORE `build_raw_message`, so it lands inside
    // the inner MIME tree, gets canonicalised together with the body,
    // and ends up covered by the signature (sign-only) or sealed inside
    // the encrypted envelope (sign-then-encrypt). Failure to resolve
    // the signer's pubkey is non-fatal — we log and skip, same fail-open
    // pattern as the existing encrypt-to-self fallback in
    // apply_pgp_envelope.
    if message.pgp_sign && account.pgp_attach_pubkey_on_sign {
        if let Some(att) = build_signer_pubkey_attachment(&state, &account.email).await {
            attachment_data.push(att);
        }
    }

    // Resolve threading headers from the original message we're replying
    // to (if any). This is what makes the next sync render the new
    // message under its parent in the thread tree.
    let (in_reply_to, references) =
        resolve_reply_headers(&state, &account_id, message.reply_to_message_id.as_deref());

    // Resolve the HTML alternative: an explicit body_html, or Markdown
    // rendered to safe HTML when "Compose in Markdown" is on. `body_text`
    // (the Markdown source, when in Markdown mode) always rides as the
    // plaintext part, so recipients get both.
    let body_html = resolve_body_html(&message);

    // Build the plain MIME bytes. `plain_raw` stays a `Vec<u8>` so the
    // optional PGP wrap and Autocrypt header inject below can each
    // consume and replace it cheaply. Only the FINAL `raw_message` is
    // wrapped in `Arc<[u8]>` (below), so the background spawn can share
    // the buffer with the IMAP APPEND-to-Sent path (#189) without
    // deep-cloning the inlined attachment bytes each send.
    let plain_raw = smtp::build_raw_message(
        &account.email,
        &message.to,
        &message.cc,
        &message.bcc,
        &message.subject,
        &message.body_text,
        body_html.as_deref(),
        &attachment_data,
        in_reply_to.as_deref(),
        &references,
    )?;

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
            &message.subject,
            message.pgp_sign,
            message.pgp_encrypt,
            Some(&origin),
        )
        .await?
    } else {
        plain_raw
    };

    // Feature: Autocrypt header. Per-account toggle (`pgp_autocrypt_header`,
    // default-on). The `Autocrypt:` header rides on the OUTER envelope of
    // EVERY outgoing message — signed, encrypted, or plain — because
    // Autocrypt key distribution bootstraps from cleartext headers. We
    // inject onto the final `raw_message` (post-PGP-wrap) so the header
    // survives the `wrap_pgp_mime_*` outer-header allowlist. Skipped
    // silently when the sender has no usable key in the keystore — the
    // header is an enhancement, not a guarantee.
    let raw_message = if account.pgp_autocrypt_header {
        match build_autocrypt_header(&state, &account.email).await {
            Some(header_line) => smtp::insert_header_before_body(&raw_message, &header_line),
            None => raw_message,
        }
    } else {
        raw_message
    };

    // Wrap the final bytes in `Arc<[u8]>` so the background spawn can
    // share them between the SMTP `send_raw` call and the IMAP APPEND-
    // to-Sent path (#189) via a refcount bump, rather than deep-cloning
    // the inlined attachment bytes (potentially many MB).
    let raw_message: std::sync::Arc<[u8]> = raw_message.into();

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
    let op_sender_bg = state.get_op_sender(&account_id, &app).await;
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
                let jmap_config = crate::auth::build_jmap_config(&account).await?;
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

/// Outcome of `save_draft`, returned to the renderer so it can tell the
/// user when the "Store drafts encrypted" toggle could not be honored.
#[derive(Debug, serde::Serialize)]
pub struct DraftSaveOutcome {
    /// True when `pgp_encrypt_drafts` was enabled for the account but the
    /// draft was nonetheless stored in plaintext — either a Microsoft
    /// Graph account (no raw-MIME draft endpoint wired up) or no usable
    /// public key in the keystore. The renderer surfaces a non-blocking
    /// notice so the toggle's label isn't silently misleading.
    pub plaintext_fallback: bool,
}

#[tauri::command]
pub async fn save_draft(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message: ComposeMessage,
) -> Result<DraftSaveOutcome> {
    log::info!(
        "Save draft command: account={} subject='{}'",
        account_id,
        message.subject
    );

    let account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };
    let backend = crate::backend::mail::for_account(&account)
        .ok_or_else(|| Error::Other("Account has no enabled mail binding".into()))?;

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

    // Render the HTML alternative for Markdown drafts too, so the draft is
    // stored as multipart/alternative(Markdown source, rendered HTML). On
    // resume the composer detects that HTML alternative and re-arms
    // Markdown mode, so a resumed-then-sent Markdown draft keeps its HTML.
    let body_html = resolve_body_html(&message);

    let raw_message = smtp::build_raw_message(
        &account.email,
        &draft_to,
        &message.cc,
        &message.bcc,
        &message.subject,
        &message.body_text,
        body_html.as_deref(),
        &attachment_data,
        in_reply_to.as_deref(),
        &references,
    )?;

    // Feature: store draft messages in encrypted format. Per-account
    // toggle (`pgp_encrypt_drafts`, default-on). The draft body is
    // encrypted-to-self with the sender's PUBLIC key — no signing, no
    // passphrase / card PIN prompt, so a card that isn't plugged in at
    // draft time is irrelevant (the user re-attaches it to decrypt on
    // resume). Structured-text backends short-circuit because they have no
    // raw-MIME endpoint wired
    // up, so an encrypted MIME blob can't round-trip — we log and save
    // plaintext, same fail-open posture as a missing key. JMAP and IMAP
    // both upload `raw_message` verbatim, so the encrypted form flows
    // through unchanged.
    // `plaintext_fallback` records that encryption was requested but
    // could not be applied — reported back to the renderer for LOW-3.
    let mut plaintext_fallback = false;
    let raw_message = if account.pgp_encrypt_drafts {
        if backend.draft_storage_format()
            == crate::backend::mail::DraftStorageFormat::StructuredText
        {
            log::warn!(
                "Encrypted drafts not yet supported on Graph accounts ({}); \
                 saving plaintext draft",
                account.email
            );
            plaintext_fallback = true;
            raw_message
        } else {
            match encrypt_draft_to_self(&state, &account.email, &raw_message).await {
                Some(encrypted) => encrypted,
                None => {
                    // encrypt_draft_to_self already logged the cause
                    // (no usable public key, or an encrypt failure).
                    plaintext_fallback = true;
                    raw_message
                }
            }
        }
    } else {
        raw_message
    };

    // Graph currently persists only these structured fields, while IMAP and
    // JMAP consume `raw_message` verbatim. The backend owns that distinction.
    let ctx = crate::backend::mail::MailSyncCtx {
        events: crate::event::tauri::shared_sink(app),
        db: state.db.clone(),
        data_dir: state.data_dir.clone(),
        providers: state.providers.clone(),
    };
    backend
        .save_draft(
            &ctx,
            &account,
            &crate::backend::mail::DraftSaveRequest {
                raw_message,
                to: message.to,
                cc: message.cc,
                bcc: message.bcc,
                subject: message.subject,
                body_text: message.body_text,
            },
        )
        .await?;

    log::info!("Draft saved successfully for account {}", account_id);
    Ok(DraftSaveOutcome { plaintext_fallback })
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
#[allow(clippy::too_many_arguments)]
async fn apply_pgp_envelope(
    app: &tauri::AppHandle,
    state: &State<'_, AppState>,
    account: &crate::db::accounts::AccountFull,
    raw_message: &[u8],
    to: &[String],
    cc: &[String],
    bcc: &[String],
    subject: &str,
    sign: bool,
    encrypt: bool,
    origin_window: Option<&str>,
) -> Result<Vec<u8>> {
    use crate::commands::pgp::{acquire_secret, SecretKind};
    use crate::mail::pgp_mime::canonicalize_for_signing;
    use crate::mail::smtp::{
        inner_part_of, wrap_pgp_mime_encrypted, wrap_pgp_mime_signed, wrap_with_protected_headers,
    };

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

    // Track which cache keys the active secret lives under: `cache_target`
    // is the in-process `CredentialCache` key (card ident for a PIN,
    // fingerprint for a passphrase); `agent_cache_key` is the namespaced
    // `tcli` agent key (`pin:<FP>` / `passphrase:<FP>`), `None` when the
    // agent cannot be addressed for this secret. If signing/encryption
    // fails the secret is wrong, so `evict_cached_secret` drops it from
    // both caches and the next retry re-prompts — without that a single
    // mistyped PIN would loop forever.
    let mut cache_target: Option<String> = None;
    let mut agent_cache_key: Option<String> = None;
    let signer_secret: SignerSecret = if !sign {
        SignerSecret::None
    } else if let Some(ref m) = card_match {
        // `tpass` / `tcli` key the card PIN by the signer key's primary
        // fingerprint (`KeyInfo.fingerprint`). Use the value chithi
        // already resolved for the signer so the agent cache matches,
        // with no dependency on the `card_keys` link table.
        let agent_key = crate::mail::pgp_agent::pin_key(&signer_info.fingerprint);
        let pin_str = acquire_secret(
            app,
            state.pgp_pending_secrets.clone(),
            state.pgp_cache.clone(),
            SecretKind::Pin,
            &m.card.ident,
            Some(&agent_key),
            reason,
            origin_window,
        )
        .await?;
        cache_target = Some(m.card.ident.clone());
        agent_cache_key = Some(agent_key);
        SignerSecret::CardPin {
            pin: libtumpa::Pin::new(pin_str.as_bytes().to_vec()),
            ident: m.card.ident.clone(),
        }
    } else {
        let agent_key = crate::mail::pgp_agent::passphrase_key(&signer_info.fingerprint);
        let pass_str = acquire_secret(
            app,
            state.pgp_pending_secrets.clone(),
            state.pgp_cache.clone(),
            SecretKind::Passphrase,
            &signer_info.fingerprint,
            Some(&agent_key),
            reason,
            origin_window,
        )
        .await?;
        cache_target = Some(signer_info.fingerprint.clone());
        agent_cache_key = Some(agent_key);
        SignerSecret::Passphrase(libtumpa::Passphrase::new(pass_str.to_string()))
    };

    let inner = inner_part_of(raw_message)?;
    let canonical_inner = canonicalize_for_signing(&inner);

    if encrypt {
        // Sign-then-encrypt (or encrypt-only) into one OpenPGP message,
        // then wrap in multipart/encrypted. Visible recipients = To + Cc +
        // the sender's own address ("encrypt-to-self") so the user can
        // decrypt messages from their Sent folder later. Hidden recipients
        // = Bcc: their PKESK packets get the all-zero wildcard key id
        // (RFC 4880 throw-keyid / --hidden-recipient) so the To/Cc
        // recipients can't read off the Bcc list from the OpenPGP packet
        // stream. If the sender has no usable public key in the local
        // keystore we skip self and log a warning — failing the send would
        // be worse than shipping a message the sender can't read back.
        let mut visible_recipients: Vec<String> = to
            .iter()
            .chain(cc.iter())
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .collect();
        let hidden_recipients: Vec<String> = bcc
            .iter()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .collect();
        if has_resolvable_public_key(&store, &account.email).await {
            if !recipients_contain(&visible_recipients, &account.email)
                && !recipients_contain(&hidden_recipients, &account.email)
            {
                visible_recipients.push(account.email.clone());
            }
        } else {
            log::warn!(
                "openpgp: skipping encrypt-to-self for {} — no usable public key in keystore; \
                 the sender will NOT be able to decrypt this message from Sent",
                account.email
            );
        }

        // Protected headers ("encrypt the subject"): when the per-account
        // toggle is on, the real subject is folded into a
        // multipart/mixed; protected-headers="v1" wrapper that becomes
        // the encrypted payload, and the cleartext outer envelope gets a
        // "..." placeholder. Off → encrypt the body part directly and
        // leave the outer subject untouched.
        let encrypt_subject = account.pgp_encrypt_subject;
        let payload = if encrypt_subject {
            canonicalize_for_signing(&wrap_with_protected_headers(&inner, subject))
        } else {
            canonical_inner.clone()
        };

        let store_for_blocking = store.clone();
        let signer_data_clone = signer_data.clone();
        let canonical_inner_clone = payload;
        let visible_owned = visible_recipients;
        let hidden_owned = hidden_recipients;

        let join_result = tokio::task::spawn_blocking(move || -> libtumpa::Result<Vec<u8>> {
            let guard = store_for_blocking
                .lock()
                .expect("pgp keystore mutex poisoned");
            let visible_refs: Vec<&str> = visible_owned.iter().map(|s| s.as_str()).collect();
            let hidden_refs: Vec<&str> = hidden_owned.iter().map(|s| s.as_str()).collect();
            match signer_secret {
                SignerSecret::None => libtumpa::encrypt::encrypt_to_recipients_with_hidden(
                    &guard,
                    &visible_refs,
                    &hidden_refs,
                    &canonical_inner_clone,
                    true,
                ),
                SignerSecret::Passphrase(pass) => {
                    libtumpa::encrypt::sign_and_encrypt_to_recipients_with_hidden(
                        &guard,
                        &signer_data_clone,
                        &pass,
                        &visible_refs,
                        &hidden_refs,
                        &canonical_inner_clone,
                        true,
                    )
                }
                SignerSecret::CardPin { pin, ident } => {
                    libtumpa::encrypt::sign_and_encrypt_on_card_to_recipients_with_hidden(
                        &guard,
                        &signer_data_clone,
                        &pin,
                        Some(&ident),
                        &visible_refs,
                        &hidden_refs,
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
                    crate::commands::pgp::evict_cached_secret(
                        &state.pgp_cache,
                        t,
                        agent_cache_key.as_deref(),
                    );
                }
                return Err(Error::Other(format!("openpgp encrypt: {e}")));
            }
        };
        let armored_str = String::from_utf8(armored)
            .map_err(|e| Error::Other(format!("openpgp encrypt produced non-utf8: {e}")))?;
        // When the subject was protected, replace the cleartext outer
        // Subject with a placeholder. Receivers that understand
        // protected headers (incl. chithi's own decrypt path) recover
        // the real subject from inside the ciphertext.
        let subject_override = if encrypt_subject { Some("...") } else { None };
        wrap_pgp_mime_encrypted(raw_message, &armored_str, subject_override)
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
                    crate::commands::pgp::evict_cached_secret(
                        &state.pgp_cache,
                        t,
                        agent_cache_key.as_deref(),
                    );
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

/// Resolve the sender's signer key by email, export the ASCII-armored
/// PUBLIC key bytes, and wrap them as an `AttachmentData` ready to be
/// appended to the outgoing message's attachment list.
///
/// Returns `None` (after logging a warning) if the signer cannot be
/// resolved or the pubkey export fails — the caller treats absence as
/// "skip the pubkey attachment, send the message anyway". The filename
/// convention matches Thunderbird and Enigmail: `OpenPGP_0x<long-keyid>.asc`
/// where `<long-keyid>` is the last 16 hex chars of the fingerprint, so
/// receiving MUAs that file attached keys by name don't collide.
async fn build_signer_pubkey_attachment(
    state: &State<'_, AppState>,
    sender_email: &str,
) -> Option<smtp::AttachmentData> {
    let store = state.pgp_store().ok()?;
    let sender_email = sender_email.to_string();
    tokio::task::spawn_blocking(move || {
        let guard = store.lock().expect("pgp keystore mutex poisoned");
        signer_pubkey_attachment_from_store(&guard, &sender_email)
    })
    .await
    .ok()
    .flatten()
}

/// Pure helper extracted out of `build_signer_pubkey_attachment` so the
/// MIME-shape logic (filename pattern, content-type, full-armor body) is
/// unit-testable against a hand-built `KeyStore` without needing a live
/// `AppState`. The `&KeyStore` here is the locked guard the caller
/// already holds.
fn signer_pubkey_attachment_from_store(
    store: &libtumpa::KeyStore,
    sender_email: &str,
) -> Option<smtp::AttachmentData> {
    let (_signer_data, signer_info) = libtumpa::store::resolve_signer(store, sender_email).ok()?;
    match libtumpa::key::export_public_armored(store, &signer_info.fingerprint) {
        Ok(armored) => {
            // Long key id = last 16 hex chars of the fingerprint (uppercase
            // hex per OpenPGP convention; the fingerprint itself is
            // already uppercase out of libtumpa).
            let fp = &signer_info.fingerprint;
            let long_keyid = &fp[fp.len().saturating_sub(16)..];
            Some(smtp::AttachmentData {
                name: format!("OpenPGP_0x{long_keyid}.asc"),
                content_type: "application/pgp-keys".to_string(),
                data: armored.into_bytes(),
            })
        }
        Err(e) => {
            log::warn!(
                "openpgp: skipping pubkey attachment for signed message from {} \
                 — export_public_armored failed: {e}",
                sender_email
            );
            None
        }
    }
}

/// Build the folded `Autocrypt:` header line for `sender_email`, or
/// `None` (logged at debug) when the sender has no key in the keystore
/// — Autocrypt is an opportunistic enhancement, so a missing key is not
/// an error. Resolves the signer key by email, exports an
/// Autocrypt-minimised transferable public key
/// (`libtumpa::key::export_public_for_autocrypt`), and folds it via
/// `smtp::format_autocrypt_header`. The returned string is the header
/// line WITHOUT a trailing CRLF — `insert_header_before_body` adds the
/// framing.
async fn build_autocrypt_header(state: &State<'_, AppState>, sender_email: &str) -> Option<String> {
    let store = state.pgp_store().ok()?;
    let sender_email = sender_email.to_string();
    tokio::task::spawn_blocking(move || {
        let guard = store.lock().expect("pgp keystore mutex poisoned");
        let (_signer_data, signer_info) =
            libtumpa::store::resolve_signer(&guard, &sender_email).ok()?;
        let addr = extract_addr_spec(&sender_email);
        match libtumpa::key::export_public_for_autocrypt(&guard, &signer_info.fingerprint, &addr) {
            Ok(keydata) => Some(smtp::format_autocrypt_header(&addr, &keydata)),
            Err(e) => {
                log::debug!(
                    "openpgp: no Autocrypt header for {} — export_public_for_autocrypt failed: {e}",
                    sender_email
                );
                None
            }
        }
    })
    .await
    .ok()
    .flatten()
}

/// Encrypt a draft `raw_message` to the sender's own public key, in the
/// standard PGP/MIME `multipart/encrypted` shape: the inner body part is
/// encrypted, the outer envelope headers (From / To / Subject / ...)
/// stay in cleartext, exactly like a normal encrypted mail. Returns
/// `None` when there's no usable public key for `sender_email` or any
/// step fails — the caller then stores the plaintext draft (fail-open).
///
/// Encryption only, never signing: `encrypt_to_recipients` uses the
/// recipient's PUBLIC key, which needs no unlock, so Save Draft never
/// triggers a passphrase or card-PIN prompt. The CPU-bound encrypt runs
/// on a blocking thread.
async fn encrypt_draft_to_self(
    state: &State<'_, AppState>,
    sender_email: &str,
    raw_message: &[u8],
) -> Option<Vec<u8>> {
    let store = state.pgp_store().ok()?;
    if !has_resolvable_public_key(&store, sender_email).await {
        log::warn!(
            "openpgp: encrypt-drafts is on but no usable public key for {} — \
             saving plaintext draft",
            sender_email
        );
        return None;
    }
    let raw = raw_message.to_vec();
    let email = sender_email.to_string();
    tokio::task::spawn_blocking(move || {
        let guard = store.lock().expect("pgp keystore mutex poisoned");
        encrypt_draft_core(&guard, &email, &raw)
    })
    .await
    .ok()
    .flatten()
}

/// Pure core of `encrypt_draft_to_self`, split out so the PGP/MIME
/// shaping is unit-testable against a hand-built `KeyStore` without a
/// live `AppState` or a Tokio runtime. Returns `None` on any failure;
/// the caller treats that as "save plaintext".
fn encrypt_draft_core(
    store: &libtumpa::KeyStore,
    sender_email: &str,
    raw_message: &[u8],
) -> Option<Vec<u8>> {
    use crate::mail::pgp_mime::canonicalize_for_signing;
    // Encrypt the inner body part only — same split a normal outgoing
    // PGP/MIME message uses. Canonicalise line endings to CRLF so the
    // body reparses cleanly after the decrypt round-trip.
    let inner = smtp::inner_part_of(raw_message).ok()?;
    let canonical = canonicalize_for_signing(&inner);
    let armored =
        libtumpa::encrypt::encrypt_to_recipients(store, &[sender_email], &canonical, true)
            .map_err(|e| log::warn!("openpgp: draft encrypt-to-self failed: {e}"))
            .ok()?;
    let armored_str = String::from_utf8(armored).ok()?;
    // Drafts keep their subject in the cleartext outer envelope — the
    // resume path reads it back from there, and protected-headers
    // ("encrypt the subject") is a separate per-account feature scoped
    // to messages actually sent, not to drafts.
    smtp::wrap_pgp_mime_encrypted(raw_message, &armored_str, None).ok()
}

/// Probe the keystore for a public encryption key usable for `id`. Returns
/// true iff `resolve_recipient` returns Ok AND the key passes
/// `ensure_key_usable_for_encryption`. Used as a precheck for the
/// encrypt-to-self addition so we can skip it instead of failing the send.
async fn has_resolvable_public_key(
    store: &std::sync::Arc<std::sync::Mutex<libtumpa::KeyStore>>,
    id: &str,
) -> bool {
    let store = store.clone();
    let id = id.to_string();
    tokio::task::spawn_blocking(move || {
        let guard = store.lock().expect("pgp keystore mutex poisoned");
        match libtumpa::store::resolve_recipient(&guard, &id) {
            Ok((_data, info)) => libtumpa::store::ensure_key_usable_for_encryption(&info).is_ok(),
            Err(_) => false,
        }
    })
    .await
    .unwrap_or(false)
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
mod markdown_body_tests {
    use super::{resolve_body_html, ComposeMessage};

    fn msg(body_text: &str, body_html: Option<&str>, markdown: bool) -> ComposeMessage {
        ComposeMessage {
            to: vec![],
            cc: vec![],
            bcc: vec![],
            subject: String::new(),
            body_text: body_text.to_string(),
            body_html: body_html.map(|s| s.to_string()),
            attachments: vec![],
            reply_to_message_id: None,
            pgp_sign: false,
            pgp_encrypt: false,
            markdown,
        }
    }

    /// Markdown off and no explicit HTML → plaintext-only (None), so the
    /// message keeps shipping as a single text/plain part.
    #[test]
    fn no_markdown_no_html_is_none() {
        assert!(resolve_body_html(&msg("# Hello", None, false)).is_none());
    }

    /// Markdown on → body_text is rendered to HTML. GFM constructs work
    /// (heading, bold), and the Markdown source is preserved as-is for the
    /// plaintext part by the caller.
    #[test]
    fn markdown_on_renders_html() {
        let html = resolve_body_html(&msg("# Title\n\nsome **bold** text", None, true))
            .expect("markdown should render html");
        assert!(html.contains("<h1>"), "heading should render: {html}");
        assert!(
            html.contains("<strong>bold</strong>"),
            "bold should render: {html}"
        );
    }

    /// Safety: raw HTML in the Markdown source must NOT pass through as
    /// live markup — markdown-rs escapes it (dangerous-HTML off by
    /// default), so a `<script>` becomes inert text, never an element.
    #[test]
    fn markdown_escapes_raw_html() {
        let html = resolve_body_html(&msg("hi <script>alert('xss')</script> there", None, true))
            .expect("render");
        assert!(
            !html.contains("<script>"),
            "raw <script> must be escaped, not emitted live: {html}"
        );
        assert!(
            html.contains("&lt;script&gt;"),
            "the script tag should survive as escaped text: {html}"
        );
    }

    /// An explicit body_html always wins, even with the Markdown toggle
    /// on — the renderer already decided the HTML.
    #[test]
    fn explicit_html_wins_over_markdown() {
        let html = resolve_body_html(&msg("# ignored", Some("<p>explicit</p>"), true))
            .expect("explicit html");
        assert_eq!(html, "<p>explicit</p>");
    }
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

#[cfg(test)]
mod signer_pubkey_attachment_tests {
    use super::signer_pubkey_attachment_from_store;
    use libtumpa::{key, CipherSuite, KeyStore, Passphrase, SubkeyFlags};

    fn make_keystore_with_key(uid: &str) -> (KeyStore, String) {
        let store = KeyStore::open_in_memory().expect("keystore");
        let pw = Passphrase::new("pw".into());
        let params = key::GenerateKeyParams {
            uids: vec![uid.into()],
            cipher_suite: CipherSuite::Cv25519,
            expiry: None,
            subkey_flags: SubkeyFlags::all(),
            can_primary_sign: true,
        };
        let info = key::generate_and_import(&store, params, &pw).expect("generate");
        (store, info.fingerprint)
    }

    /// Round-trip: create an in-memory keystore, generate a key for
    /// alice@example.com, and assert that the pubkey-attachment helper
    /// returns a well-formed `AttachmentData` with:
    ///   * content_type "application/pgp-keys" (RFC 3156 §7 / Autocrypt)
    ///   * filename "OpenPGP_0x<long-keyid>.asc" matching the last 16
    ///     hex chars of the fingerprint (Thunderbird convention)
    ///   * data is the ASCII-armored PUBLIC key, no secret material
    #[test]
    fn pubkey_attachment_shape_matches_thunderbird_convention() {
        let (store, fp) = make_keystore_with_key("Alice <alice@example.com>");

        let att =
            signer_pubkey_attachment_from_store(&store, "alice@example.com").expect("attachment");

        assert_eq!(att.content_type, "application/pgp-keys");
        let expected_keyid = &fp[fp.len() - 16..];
        assert_eq!(att.name, format!("OpenPGP_0x{expected_keyid}.asc"));
        let armored = std::str::from_utf8(&att.data).expect("utf8 armor");
        assert!(
            armored.starts_with("-----BEGIN PGP PUBLIC KEY BLOCK-----"),
            "expected armored public-key block, got: {}",
            armored.lines().next().unwrap_or("")
        );
        assert!(
            !armored.contains("BEGIN PGP PRIVATE KEY"),
            "pubkey attachment must NEVER include secret material"
        );
    }

    /// No usable signer for the address → helper returns None (caller
    /// then skips the attachment without failing the send). This mirrors
    /// the encrypt-to-self fail-open behaviour for accounts that have no
    /// PGP key in the keystore yet.
    #[test]
    fn pubkey_attachment_returns_none_when_no_signer_resolved() {
        let store = KeyStore::open_in_memory().expect("keystore");
        assert!(signer_pubkey_attachment_from_store(&store, "nobody@example.com").is_none());
    }
}

#[cfg(test)]
mod encrypt_draft_tests {
    use super::encrypt_draft_core;
    use crate::mail::{pgp_mime, smtp};
    use libtumpa::{key, CipherSuite, KeyStore, Passphrase, SubkeyFlags};

    /// Round-trip: encrypt a draft to self, then decrypt the ciphertext
    /// with the secret key and assert the recovered body equals the
    /// canonicalised original inner part. Also pins the PGP/MIME shape:
    /// the output is `multipart/encrypted` and the outer Subject stays
    /// in cleartext (body-only encryption — protected subjects are a
    /// separate feature/toggle).
    #[test]
    fn encrypt_draft_round_trips_through_decrypt() {
        let store = KeyStore::open_in_memory().expect("keystore");
        let pw = Passphrase::new("pw".into());
        let params = key::GenerateKeyParams {
            uids: vec!["Alice <alice@example.com>".into()],
            cipher_suite: CipherSuite::Cv25519,
            expiry: None,
            subkey_flags: SubkeyFlags::all(),
            can_primary_sign: true,
        };
        let generated = key::generate(params, &pw).expect("generate");
        store.import_key(&generated.secret_key).expect("import");

        let raw = smtp::build_raw_message(
            "alice@example.com",
            &["alice@example.com".into()],
            &[],
            &[],
            "Draft subject",
            "Draft body in progress.",
            None,
            &[],
            None,
            &[],
        )
        .expect("build raw");

        let encrypted =
            encrypt_draft_core(&store, "alice@example.com", &raw).expect("encrypt draft");

        let s = String::from_utf8_lossy(&encrypted);
        assert!(
            s.contains("multipart/encrypted"),
            "draft must be wrapped as multipart/encrypted"
        );
        assert!(
            s.contains("Subject: Draft subject"),
            "outer Subject stays in cleartext for body-only encryption"
        );

        // The ciphertext decrypts back to the canonicalised inner body.
        let ciphertext =
            pgp_mime::extract_encrypted_payload(&encrypted).expect("extract ciphertext");
        let recovered =
            libtumpa::decrypt::decrypt_with_key(&generated.secret_key, &ciphertext, &pw)
                .expect("decrypt draft");
        let original_inner = smtp::inner_part_of(&raw).expect("inner part");
        let canonical = pgp_mime::canonicalize_for_signing(&original_inner);
        assert_eq!(
            &recovered[..],
            &canonical[..],
            "decrypted draft body must match the canonicalised original inner"
        );
        assert!(
            String::from_utf8_lossy(&recovered).contains("Draft body in progress."),
            "the in-progress body text must survive the encrypt/decrypt round-trip"
        );
    }

    /// No key in the store for the sender → `encrypt_draft_core` returns
    /// None and the caller stores the plaintext draft (fail-open).
    #[test]
    fn encrypt_draft_returns_none_without_a_key() {
        let store = KeyStore::open_in_memory().expect("keystore");
        let raw = smtp::build_raw_message(
            "nobody@example.com",
            &["nobody@example.com".into()],
            &[],
            &[],
            "x",
            "y",
            None,
            &[],
            None,
            &[],
        )
        .expect("build raw");
        assert!(encrypt_draft_core(&store, "nobody@example.com", &raw).is_none());
    }
}

#[cfg(test)]
mod protected_headers_tests {
    use crate::mail::{pgp_mime, smtp};
    use libtumpa::{key, CipherSuite, KeyStore, Passphrase, SubkeyFlags};

    /// Full protected-headers round-trip mirroring what `apply_pgp_envelope`
    /// does when the "encrypt the subject" toggle is on: wrap the inner
    /// body in a protected-headers entity carrying the real subject,
    /// encrypt that, then wrap in multipart/encrypted with a "..." outer
    /// subject. Then decrypt and assert the real subject is recovered
    /// from inside the ciphertext while the cleartext envelope only
    /// shows "...".
    #[test]
    fn protected_headers_round_trip_hides_then_recovers_subject() {
        let store = KeyStore::open_in_memory().expect("keystore");
        let pw = Passphrase::new("pw".into());
        let params = key::GenerateKeyParams {
            uids: vec!["Alice <alice@example.com>".into()],
            cipher_suite: CipherSuite::Cv25519,
            expiry: None,
            subkey_flags: SubkeyFlags::all(),
            can_primary_sign: true,
        };
        let generated = key::generate(params, &pw).expect("generate");
        store.import_key(&generated.secret_key).expect("import");

        let real_subject = "Quarterly numbers are confidential";
        let raw = smtp::build_raw_message(
            "alice@example.com",
            &["bob@example.com".into()],
            &[],
            &[],
            real_subject,
            "Body that should also stay secret.",
            None,
            &[],
            None,
            &[],
        )
        .expect("build raw");

        // Step 1+2: protected-headers wrap, then encrypt.
        let inner = smtp::inner_part_of(&raw).expect("inner part");
        let protected = smtp::wrap_with_protected_headers(&inner, real_subject);
        let canonical = pgp_mime::canonicalize_for_signing(&protected);
        let armored = libtumpa::encrypt::encrypt_to_recipients(
            &store,
            &["alice@example.com"],
            &canonical,
            true,
        )
        .expect("encrypt");
        let armored_str = String::from_utf8(armored).expect("utf8 armor");

        // Step 3: multipart/encrypted with the subject placeholder.
        let wrapped = smtp::wrap_pgp_mime_encrypted(&raw, &armored_str, Some("...")).expect("wrap");

        // The cleartext envelope hides the real subject.
        let wrapped_s = String::from_utf8_lossy(&wrapped);
        assert!(
            wrapped_s.contains("Subject: ...\r\n"),
            "outer envelope must carry the placeholder subject"
        );
        assert!(
            !wrapped_s.contains(real_subject),
            "the real subject must NOT be anywhere in the cleartext bytes"
        );

        // Decrypt and confirm the protected entity carries the real subject.
        let ciphertext = pgp_mime::extract_encrypted_payload(&wrapped).expect("extract ciphertext");
        let recovered =
            libtumpa::decrypt::decrypt_with_key(&generated.secret_key, &ciphertext, &pw)
                .expect("decrypt");
        let recovered_s = String::from_utf8_lossy(&recovered);
        assert!(
            recovered_s.contains("protected-headers=\"v1\""),
            "decrypted payload must be the protected-headers entity"
        );
        assert!(
            recovered_s.contains(&format!("Subject: {real_subject}")),
            "the real subject must be recoverable from inside the ciphertext"
        );
    }
}
