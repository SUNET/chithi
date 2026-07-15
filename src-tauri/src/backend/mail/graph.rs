//! Microsoft Graph mail backend (O365 / Exchange Online).

use async_trait::async_trait;
use tauri::Emitter;

use crate::commands::events::{emit_folders_changed, emit_messages_changed};
use crate::db;
use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::ops::queue::MailOp;

use super::{MailBackend, MailOpExecutor, MailSyncCtx};

pub struct GraphMailBackend;

/// Sync an O365 account via Microsoft Graph API.
/// Downloads full MIME bodies during sync and streams them to Maildir,
/// so message reading works offline without live API calls.
/// Two-phase: download without DB lock (UI stays responsive), then fast batch insert.
async fn sync_graph_account(ctx: &MailSyncCtx, account_id: &str) -> Result<()> {
    use crate::mail::graph::{self, GraphClient};
    use crate::mail::sync::{create_maildir_dirs, flags_to_maildir_suffix, sanitize_folder_name};

    let app = &ctx.app;
    let db_arc = &ctx.db;
    let data_dir = ctx.data_dir.clone();

    // Mirror sync_account / sync_jmap_account: emit sync-started so the
    // activity store can mark the operation running and spin the StatusBar
    // icon. Without this, Graph syncs are silent on the frontend.
    let account_name = {
        let conn = db_arc.reader();
        db::accounts::get_account_full(&conn, account_id)
            .map(|a| a.display_name)
            .unwrap_or_else(|_| account_id.to_string())
    };
    app.emit(
        "sync-started",
        serde_json::json!({
            "account_id": account_id,
            "account_name": account_name,
        }),
    )
    .ok();

    let token = graph::get_graph_token(account_id).await?;
    let client = GraphClient::new(&token);

    // Sync mail folders
    let graph_folders = client.list_mail_folders().await?;
    log::info!(
        "Graph sync: {} mail folders for account {}",
        graph_folders.len(),
        account_id
    );

    {
        let conn = db_arc.writer().await;
        for gf in &graph_folders {
            let folder_type = graph::guess_folder_type(&gf.display_name);
            db::folders::upsert_folder(
                &conn,
                account_id,
                &gf.display_name,
                &gf.id,
                folder_type,
                None,
            )?;
            db::folders::update_folder_counts(
                &conn,
                account_id,
                &gf.id,
                gf.unread_count,
                gf.total_count,
            )?;
        }
    }

    // Sync messages for each folder
    let mut grand_total = 0u32;
    for gf in &graph_folders {
        let (messages, _total) = client.list_messages(&gf.id, 200, 0).await?;

        if messages.is_empty() {
            continue;
        }

        let existing_ids = {
            let conn = db_arc.reader();
            let mut stmt = conn
                .prepare("SELECT id FROM messages WHERE account_id = ?1 AND folder_path = ?2")
                .map_err(Error::Database)?;
            let ids: std::collections::HashSet<String> = stmt
                .query_map(rusqlite::params![account_id, gf.id], |row| row.get(0))
                .map_err(Error::Database)?
                .filter_map(|r| r.ok())
                .collect();
            ids
        };

        // Backfill: existing rows synced before threading worked have an
        // empty thread_id. We also have a fresh In-Reply-To from
        // internetMessageHeaders, which lets the frontend render the
        // reply hierarchy for already-stored Graph messages without a
        // re-download.
        {
            let conn = db_arc.writer().await;
            let mut update_thread = conn.prepare(
                "UPDATE messages SET thread_id = ?1
                 WHERE id = ?2 AND (thread_id IS NULL OR thread_id = '')",
            )?;
            let mut update_irt = conn.prepare(
                "UPDATE messages SET in_reply_to = ?1
                 WHERE id = ?2 AND (in_reply_to IS NULL OR in_reply_to = '')",
            )?;
            for msg in &messages {
                let id = format!("{}_{}", account_id, msg.id);
                if !existing_ids.contains(&id) {
                    continue;
                }
                if let Some(cid) = msg.conversation_id.as_deref() {
                    if !cid.is_empty() {
                        update_thread.execute(rusqlite::params![cid, id])?;
                    }
                }
                if let Some(irt) = msg.in_reply_to.as_deref() {
                    if !irt.is_empty() {
                        update_irt.execute(rusqlite::params![irt, id])?;
                    }
                }
            }
        }

        // Collect new messages
        let mut new_messages = Vec::new();
        for msg in &messages {
            let id = format!("{}_{}", account_id, msg.id);
            if existing_ids.contains(&id) {
                continue;
            }
            new_messages.push(msg);
        }

        if new_messages.is_empty() {
            continue;
        }

        // Prepare Maildir directory
        let folder_dir = sanitize_folder_name(&gf.id);
        let maildir_base = data_dir.join(account_id).join(&folder_dir);
        create_maildir_dirs(&maildir_base)?;

        // Phase 1: Stream MIME bodies to disk (no DB lock — UI stays responsive)
        let mut downloaded: Vec<(&graph::GraphMessage, String)> = Vec::new();
        for msg in &new_messages {
            let flags = if msg.is_read {
                vec!["seen".to_string()]
            } else {
                vec![]
            };
            let filename = format!("{}:2,{}", msg.id, flags_to_maildir_suffix(&flags));
            let msg_path = maildir_base.join("cur").join(&filename);

            let maildir_path = match client.download_mime_to_file(&msg.id, &msg_path).await {
                Ok(bytes_written) => {
                    log::debug!(
                        "Graph sync: downloaded {} bytes for {}",
                        bytes_written,
                        msg.id
                    );
                    format!("{}/{}/cur/{}", account_id, folder_dir, filename)
                }
                Err(e) => {
                    log::warn!("Graph sync: failed to download MIME for {}: {}", msg.id, e);
                    // Clean up partial file
                    let _ = std::fs::remove_file(&msg_path);
                    String::new() // Empty = on-demand fetch later
                }
            };
            downloaded.push((msg, maildir_path));
        }

        // Phase 2: Fast batch DB insert (lock held <10ms, not during downloads)
        let conn = db_arc.writer().await;
        conn.execute_batch("BEGIN")?;

        let mut synced = 0u32;
        let mut new_ids: Vec<String> = Vec::new();
        for (msg, maildir_path) in &downloaded {
            let id = format!("{}_{}", account_id, msg.id);
            let flags = if msg.is_read {
                vec!["seen".to_string()]
            } else {
                vec![]
            };
            let thread_id = msg.conversation_id.clone();

            let new_msg = db::messages::NewMessage {
                id: id.clone(),
                account_id: account_id.to_string(),
                folder_path: gf.id.clone(),
                uid: 0,
                message_id: msg.internet_message_id.clone(),
                in_reply_to: msg.in_reply_to.clone(),
                thread_id,
                subject: msg.subject.clone(),
                from_name: msg.from_name.clone(),
                from_email: msg
                    .from_email
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                to_addresses: msg.to_addresses.clone(),
                cc_addresses: msg.cc_addresses.clone(),
                date: msg.date.clone(),
                size: 0,
                has_attachments: msg.has_attachments,
                is_encrypted: false,
                is_signed: false,
                flags: serde_json::to_string(&flags).unwrap_or_default(),
                maildir_path: maildir_path.clone(),
                snippet: msg.preview.clone(),
            };
            db::messages::insert_message(&conn, &new_msg)?;
            new_ids.push(id);
            synced += 1;
        }

        conn.execute_batch("COMMIT")?;
        drop(conn);

        if synced > 0 {
            log::info!(
                "Graph sync: {} new messages in '{}' (bodies streamed to disk)",
                synced,
                gf.display_name
            );
            grand_total += synced;
        }

        // Run filter rules against newly inserted messages. Errors are
        // logged and swallowed so a transient Graph hiccup can't poison
        // the sync — messages already landed in the DB.
        if !new_ids.is_empty() {
            match crate::commands::filters::apply_filters_to_new_messages(
                db_arc, account_id, &gf.id, &new_ids,
            )
            .await
            {
                Ok(filtered) => {
                    if filtered > 0 {
                        log::info!(
                            "Graph filters matched {} of {} new messages in '{}'",
                            filtered,
                            new_ids.len(),
                            gf.display_name
                        );
                    }
                }
                Err(e) => log::warn!("Graph filter pass failed for '{}': {}", gf.display_name, e),
            }
        }
    }

    app.emit(
        "sync-complete",
        serde_json::json!({
            "account_id": account_id,
            "total_synced": grand_total,
        }),
    )
    .ok();
    emit_folders_changed(app, account_id);
    emit_messages_changed(app, account_id);

    log::info!(
        "Graph sync: completed for account {}, {} new messages",
        account_id,
        grand_total
    );
    Ok(())
}

#[async_trait]
impl MailBackend for GraphMailBackend {
    fn protocol(&self) -> &'static str {
        "graph"
    }

    /// Microsoft Graph has no cheap per-folder fetch — every sync runs
    /// against the whole account, so the command backgrounds it.
    fn folder_sync_backgrounds(&self) -> bool {
        true
    }

    async fn sync_account(
        &self,
        ctx: &MailSyncCtx,
        account: &AccountFull,
        _current_folder: Option<String>,
    ) -> Result<()> {
        log::info!(
            "Syncing account {} ({}) via Microsoft Graph",
            account.display_name,
            account.email,
        );
        sync_graph_account(ctx, &account.id).await
    }

    /// Never called: `folder_sync_backgrounds` routes the command to a
    /// background [`Self::sync_account`] instead.
    async fn sync_folder(
        &self,
        _ctx: &MailSyncCtx,
        _account: &AccountFull,
        _folder_path: &str,
    ) -> Result<u32> {
        Err(Error::Sync(
            "Graph per-folder sync runs as a background account sync".into(),
        ))
    }

    /// O365 accounts keep IMAP access alongside Graph, and their Graph
    /// sync can leave rows behind for on-demand fetch — reuse the IMAP
    /// prefetch pipeline exactly as the pre-trait command did.
    async fn prefetch_bodies(&self, ctx: &MailSyncCtx, account: &AccountFull) -> Result<u32> {
        super::imap::prefetch_pipeline(ctx, account).await
    }

    fn op_executor(&self) -> Box<dyn MailOpExecutor> {
        Box::new(GraphOpExecutor)
    }
}

/// Stateless executor. Move/delete/flag ops are already applied by the
/// optimistic command path; queued sends cannot be replayed (see
/// `execute`).
pub(super) struct GraphOpExecutor;

#[async_trait]
impl MailOpExecutor for GraphOpExecutor {
    async fn execute(&mut self, _ctx: &MailSyncCtx, _account_id: &str, op: MailOp) -> Result<()> {
        match op {
            MailOp::MoveMessages { .. }
            | MailOp::DeleteMessages { .. }
            | MailOp::SetFlags { .. } => {
                log::debug!("Graph op handled by optimistic path");
            }
            MailOp::SendRaw { .. } => {
                // Graph's server-side `/me/mailFolders/outbox` already holds
                // messages that were accepted by `sendMail`. Client-side
                // replay would require re-parsing the raw MIME back into
                // Graph's structured payload, which is error-prone. Fail
                // here so the row eventually marks dead and the user can
                // surface it via the Outbox view.
                return Err(Error::Other(
                    "Graph send replay is not implemented; the server-side Outbox folder owns delivery from this point. Discard or recompose this message.".into(),
                ));
            }
            _ => {}
        }
        Ok(())
    }
}
