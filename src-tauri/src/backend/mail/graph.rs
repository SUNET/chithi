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

/// Max delta pages (of up to 200 messages each) applied per folder per
/// sync cycle. The initial enumeration of a huge folder resumes on the
/// next cycle from the persisted `nextLink` instead of monopolizing this
/// one; steady-state delta rounds are one page.
const MAX_DELTA_PAGES_PER_CYCLE: usize = 25;

/// Sync an O365 account via Microsoft Graph delta queries.
///
/// Envelope-only: message bodies are fetched on demand when a message is
/// opened (`commands/mail.rs` handles the `graph:`/empty `maildir_path`
/// cases). Each folder keeps a persisted delta link (`graph_delta_link`),
/// so steady-state sync is ~1 request per folder and the server reports
/// creations, flag changes, and removals (deletes *and* moves out of the
/// folder) explicitly — the previous full-crawl implementation re-listed
/// the newest 200 messages of every folder and downloaded full MIME for
/// anything it didn't recognize, every cycle.
async fn sync_graph_account(
    ctx: &MailSyncCtx,
    account_id: &str,
    current_folder: Option<&str>,
) -> Result<()> {
    use crate::mail::graph::{self, GraphClient};

    let app = &ctx.app;
    let db_arc = &ctx.db;

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

    // Sync order: the folder the user is looking at first, then Inbox,
    // then the rest in walk order — same priority scheme as IMAP/JMAP.
    // Matters most during the initial delta enumeration, which can take
    // a while on big mailboxes.
    let mut graph_folders = graph_folders;
    graph_folders.sort_by_key(|gf| {
        if current_folder == Some(gf.id.as_str()) {
            0u8
        } else if graph::guess_folder_type(&gf.display_name) == Some("inbox") {
            1
        } else {
            2
        }
    });

    // Sync messages for each folder via delta queries. Per-folder errors
    // are logged and skipped so one throttled or broken folder can't
    // starve the folders after it (new folders sort last in the walk).
    let mut grand_total = 0u32;
    for gf in &graph_folders {
        match sync_graph_folder_delta(ctx, &client, account_id, gf).await {
            Ok(synced) => grand_total += synced,
            Err(e) => log::warn!("Graph sync: skipping folder '{}': {}", gf.display_name, e),
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

/// Delta-sync one folder: apply creations, flag updates, and removals
/// reported by Graph since the folder's stored delta link. With no stored
/// link this is the initial full enumeration (paged, resumable). Returns
/// the number of newly inserted messages.
async fn sync_graph_folder_delta(
    ctx: &MailSyncCtx,
    client: &crate::mail::graph::GraphClient,
    account_id: &str,
    gf: &crate::mail::graph::GraphMailFolder,
) -> Result<u32> {
    use crate::mail::graph;

    let db_arc = &ctx.db;

    let mut link = {
        let conn = db_arc.reader();
        db::folders::get_graph_delta_link(&conn, account_id, &gf.id)?
    };

    // Known message ids in this folder, so delta "created or updated"
    // entries split into insert vs flag-update without a per-row query.
    let mut existing_ids: std::collections::HashSet<String> = {
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

    let mut synced = 0u32;
    let mut new_ids: Vec<String> = Vec::new();
    let mut pages = 0usize;

    loop {
        let page = match client.messages_delta_page(&gf.id, link.as_deref()).await {
            Ok(p) => p,
            Err(e) if graph::is_delta_resync_required(&e) => {
                // Stored delta token expired server-side (HTTP 410). Clear
                // it so the next cycle restarts with a full enumeration.
                let conn = db_arc.writer().await;
                db::folders::update_graph_delta_link(&conn, account_id, &gf.id, None)?;
                return Err(Error::Sync(format!(
                    "delta state expired for '{}'; full resync on next cycle",
                    gf.display_name
                )));
            }
            Err(e) => return Err(e),
        };

        {
            let conn = db_arc.writer().await;
            conn.execute_batch("BEGIN")?;

            for removed in &page.removed_ids {
                let id = format!("{}_{}", account_id, removed);
                conn.execute("DELETE FROM messages WHERE id = ?1", rusqlite::params![id])
                    .ok();
                existing_ids.remove(&id);
            }

            for msg in &page.messages {
                let id = format!("{}_{}", account_id, msg.id);
                let flags = if msg.is_read {
                    vec!["seen".to_string()]
                } else {
                    vec![]
                };
                let flags_json = serde_json::to_string(&flags).unwrap_or_default();

                if existing_ids.contains(&id) {
                    // Updated message: mirror the server-side read state
                    // (read/unread toggled on webmail).
                    let _ = db::messages::update_flags(&conn, &id, &flags_json);
                    continue;
                }

                let new_msg = db::messages::NewMessage {
                    id: id.clone(),
                    account_id: account_id.to_string(),
                    folder_path: gf.id.clone(),
                    uid: 0,
                    message_id: msg.internet_message_id.clone(),
                    in_reply_to: msg.in_reply_to.clone(),
                    thread_id: msg.conversation_id.clone(),
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
                    flags: flags_json,
                    // Empty = body fetched on demand when the message is opened.
                    maildir_path: String::new(),
                    snippet: msg.preview.clone(),
                };
                db::messages::insert_message(&conn, &new_msg)?;
                existing_ids.insert(id.clone());
                new_ids.push(id);
                synced += 1;
            }

            // Persist the resume point after every page: an interrupted
            // sync continues from here instead of restarting the folder.
            let resume = page.next_link.as_deref().or(page.delta_link.as_deref());
            if let Some(l) = resume {
                db::folders::update_graph_delta_link(&conn, account_id, &gf.id, Some(l))?;
            }

            conn.execute_batch("COMMIT")?;
        }

        pages += 1;
        match page.next_link {
            Some(next) if pages < MAX_DELTA_PAGES_PER_CYCLE => link = Some(next),
            Some(_) => {
                log::info!(
                    "Graph sync: '{}' has more pages after {} ({} new so far); resuming next cycle",
                    gf.display_name,
                    pages,
                    synced
                );
                break;
            }
            None => break,
        }
    }

    if synced > 0 {
        log::info!(
            "Graph sync: {} new messages in '{}' (envelopes only; bodies on demand)",
            synced,
            gf.display_name
        );
    }

    // Run filter rules against newly inserted messages. Errors are logged
    // and swallowed so a transient Graph hiccup can't poison the sync —
    // messages already landed in the DB.
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

    Ok(synced)
}

#[async_trait]
impl MailBackend for GraphMailBackend {
    fn protocol(&self) -> &'static str {
        "graph"
    }

    async fn sync_account(
        &self,
        ctx: &MailSyncCtx,
        account: &AccountFull,
        current_folder: Option<String>,
    ) -> Result<()> {
        log::info!(
            "Syncing account {} ({}) via Microsoft Graph",
            account.display_name,
            account.email,
        );
        sync_graph_account(ctx, &account.id, current_folder.as_deref()).await
    }

    /// Sync exactly one folder via its delta link. Refreshes the folder's
    /// name/counts from the server first, so this works even for a folder
    /// the local DB hasn't seen yet.
    async fn sync_folder(
        &self,
        ctx: &MailSyncCtx,
        account: &AccountFull,
        folder_path: &str,
    ) -> Result<u32> {
        use crate::mail::graph::{self, GraphClient};

        ctx.app
            .emit(
                "sync-started",
                serde_json::json!({
                    "account_id": account.id,
                    "account_name": account.display_name,
                }),
            )
            .ok();

        let token = graph::get_graph_token(&account.id).await?;
        let client = GraphClient::new(&token);

        let gf = client.get_mail_folder(folder_path).await?;
        {
            let conn = ctx.db.writer().await;
            let folder_type = graph::guess_folder_type(&gf.display_name);
            db::folders::upsert_folder(
                &conn,
                &account.id,
                &gf.display_name,
                &gf.id,
                folder_type,
                None,
            )?;
            db::folders::update_folder_counts(
                &conn,
                &account.id,
                &gf.id,
                gf.unread_count,
                gf.total_count,
            )?;
        }

        sync_graph_folder_delta(ctx, &client, &account.id, &gf).await
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
