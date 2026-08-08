//! Microsoft Graph mail backend (O365 / Exchange Online).

use async_trait::async_trait;
use tauri::Emitter;

use crate::db;
use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::event::{emit_folders_changed, emit_messages_changed};
use crate::mail::compat::{BackendMessageRef, BodyLocation};
use crate::ops::flags::FlagTarget;
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
                gf.parent_folder_id.as_deref(),
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

    // A full (re-)enumeration — no stored link, e.g. first sync or after
    // HTTP 410 expired the delta token — lists what exists NOW; it does
    // not emit tombstones for messages deleted while no delta state was
    // held. Mark every current row `graph_prune_pending` up front; the
    // enumeration clears the mark on each message it lists, and whatever
    // is still marked when the enumeration COMPLETES (possibly several
    // capped cycles later — the marks are durable, unlike an in-memory
    // seen-set) was deleted server-side and gets pruned.
    if link.is_none() && !existing_ids.is_empty() {
        let conn = db_arc.writer().await;
        conn.execute(
            "UPDATE messages SET graph_prune_pending = 1
             WHERE account_id = ?1 AND folder_path = ?2",
            rusqlite::params![account_id, gf.id],
        )?;
    }

    // Rows whose sync-time filter pass never ran (crash or error after
    // the insert transaction committed): pick them up in this cycle's
    // filter batch. They are already in `existing_ids`, so the page loop
    // below won't re-add them.
    let mut new_ids: Vec<String> = {
        let conn = db_arc.reader();
        let mut stmt = conn
            .prepare(
                "SELECT id FROM messages
                 WHERE account_id = ?1 AND folder_path = ?2 AND graph_filters_pending = 1",
            )
            .map_err(Error::Database)?;
        let ids: Vec<String> = stmt
            .query_map(rusqlite::params![account_id, gf.id], |row| row.get(0))
            .map_err(Error::Database)?
            .filter_map(|r| r.ok())
            .collect();
        ids
    };
    if !new_ids.is_empty() {
        log::info!(
            "Graph sync: {} message(s) in '{}' still awaiting their filter pass",
            new_ids.len(),
            gf.display_name
        );
    }

    let mut completed = false;
    let mut synced = 0u32;
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
            // rusqlite::Transaction rolls back on drop, so any `?` below
            // cannot leave an open transaction on the pooled connection.
            let tx = conn.unchecked_transaction()?;

            // Apply events strictly in server order: the same id can carry
            // an update followed by a removal within one page, and playing
            // removals and upserts as separate passes would resurrect the
            // message. Errors propagate — a failed delete must roll the
            // whole page (checkpoint included) back, or the server would
            // never resend the tombstone.
            for event in &page.events {
                match event {
                    graph::GraphDeltaEvent::Removed(removed) => {
                        let id = BackendMessageRef::graph(removed).to_db_id(account_id);
                        tx.execute("DELETE FROM messages WHERE id = ?1", rusqlite::params![id])?;
                        existing_ids.remove(&id);
                        // A removed row owes no filter pass; drop it from
                        // the batch if it was inserted earlier this cycle.
                        new_ids.retain(|n| n != &id);
                    }
                    graph::GraphDeltaEvent::Upsert(msg) => {
                        let message_ref = BackendMessageRef::graph(&msg.id);
                        let id = message_ref.to_db_id(account_id);
                        // Mirror the full server-side flag state (read +
                        // flagged), not just read: replacing the array with
                        // only `seen` erased an existing `flagged` on every
                        // update.
                        let mut flags: Vec<String> = Vec::new();
                        if msg.is_read {
                            flags.push("seen".to_string());
                        }
                        if msg.is_flagged {
                            flags.push("flagged".to_string());
                        }
                        let flags_json = serde_json::to_string(&flags).unwrap_or_default();

                        if existing_ids.contains(&id) {
                            // Updated message: mirror server-side flag
                            // changes and mark it as confirmed-alive for a
                            // running full enumeration (see
                            // graph_prune_pending above).
                            db::messages::update_flags(&tx, &id, &flags_json)?;
                            tx.execute(
                                "UPDATE messages SET graph_prune_pending = 0 WHERE id = ?1",
                                rusqlite::params![id],
                            )?;
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
                            // `graph:` marks the body as not yet downloaded
                            // AND keeps the row out of the IMAP prefetch
                            // pipeline (which selects `maildir_path = ''`
                            // rows and cannot fetch Graph folders/UID 0).
                            maildir_path: BodyLocation::GraphRemote(msg.id.clone()).to_persisted(),
                            snippet: msg.preview.clone(),
                        };
                        db::messages::insert_message(&tx, &new_msg)?;
                        // Durable "filter pass still owed" marker, committed
                        // with the insert: a crash before the filter run is
                        // retried on the next cycle instead of silently
                        // skipped.
                        tx.execute(
                            "UPDATE messages SET graph_filters_pending = 1 WHERE id = ?1",
                            rusqlite::params![id],
                        )?;
                        existing_ids.insert(id.clone());
                        if !new_ids.contains(&id) {
                            new_ids.push(id);
                        }
                        synced += 1;
                    }
                }
            }

            // Persist the resume point after every page: an interrupted
            // sync continues from here instead of restarting the folder.
            let resume = page.next_link.as_deref().or(page.delta_link.as_deref());
            if let Some(l) = resume {
                db::folders::update_graph_delta_link(&tx, account_id, &gf.id, Some(l))?;
            }

            tx.commit()?;
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
            None => {
                completed = true;
                break;
            }
        }
    }

    // Reconcile on completion (reached the deltaLink): rows still marked
    // graph_prune_pending were never listed by the enumeration that
    // marked them — deleted or moved out server-side while we had no
    // delta state. The marks are durable, so this fires correctly even
    // when the enumeration spanned several page-capped cycles; ordinary
    // delta rounds mark nothing, making this a no-op for them.
    if completed {
        let conn = db_arc.writer().await;
        let pruned = conn.execute(
            "DELETE FROM messages
             WHERE account_id = ?1 AND folder_path = ?2 AND graph_prune_pending = 1",
            rusqlite::params![account_id, gf.id],
        )?;
        if pruned > 0 {
            log::info!(
                "Graph sync: pruned {} stale local row(s) in '{}' after full enumeration",
                pruned,
                gf.display_name
            );
        }
    }

    if synced > 0 {
        log::info!(
            "Graph sync: {} new messages in '{}' (envelopes only; bodies on demand)",
            synced,
            gf.display_name
        );
    }

    // Run filter rules against newly inserted messages (plus any picked
    // up from an interrupted earlier run). Errors are logged and
    // swallowed so a transient Graph hiccup can't poison the sync — the
    // rows keep their graph_filters_pending marker and are retried on
    // the next cycle.
    if !new_ids.is_empty() {
        match crate::filters::service::apply_filters_to_new_messages(
            db_arc, account_id, &gf.id, &new_ids,
        )
        .await
        {
            Ok(outcome) => {
                // Clear the pending marker ONLY for messages whose filter
                // pass completed — messages with failed server-side
                // actions stay marked and are retried next cycle. Rows the
                // filters moved or deleted are already gone from the DB,
                // so their UPDATE simply no-ops.
                let failed: std::collections::HashSet<&String> =
                    outcome.failed_db_ids.iter().collect();
                {
                    let conn = db_arc.writer().await;
                    let tx = conn.unchecked_transaction()?;
                    for id in new_ids.iter().filter(|id| !failed.contains(id)) {
                        tx.execute(
                            "UPDATE messages SET graph_filters_pending = 0 WHERE id = ?1",
                            rusqlite::params![id],
                        )?;
                    }
                    tx.commit()?;
                }
                if !failed.is_empty() {
                    log::warn!(
                        "Graph filters: {} message(s) in '{}' kept pending after failed actions",
                        failed.len(),
                        gf.display_name
                    );
                }
                if outcome.affected > 0 {
                    log::info!(
                        "Graph filters matched {} of {} new messages in '{}'",
                        outcome.affected,
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
                gf.parent_folder_id.as_deref(),
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

    /// Graph-native body prefetch. Graph message rows store the folder's
    /// Graph id in `folder_path` and `uid = 0`, so the IMAP prefetch
    /// pipeline (folder SELECT + fetch-by-UID) can never retrieve them —
    /// delegating there just produced failed selects after every sync.
    /// Instead, stream full MIME for the newest unfetched rows via the
    /// same `download_mime_to_file` path the on-demand fetch uses.
    async fn prefetch_bodies(&self, ctx: &MailSyncCtx, account: &AccountFull) -> Result<u32> {
        use crate::mail::graph::{self, GraphClient};
        use crate::mail::sync::{
            create_maildir_dirs, flags_to_maildir_suffix, sanitize_folder_name,
        };

        /// Bodies fetched per prefetch pass; the pass re-runs after every
        /// sync, so the backlog drains across cycles.
        const MAX_PREFETCH_PER_PASS: u32 = 100;

        let unfetched = {
            let conn = ctx.db.reader();
            db::messages::get_unfetched_graph_messages(&conn, &account.id, MAX_PREFETCH_PER_PASS)?
        };
        if unfetched.is_empty() {
            return Ok(0);
        }
        log::info!(
            "Graph prefetch: {} unfetched bodies for account {}",
            unfetched.len(),
            account.id
        );

        let token = graph::get_graph_token(&account.id).await?;
        let client = GraphClient::new(&token);

        let mut fetched = 0u32;
        for (db_id, folder_path, maildir_path, flags_json) in &unfetched {
            let body_location = BodyLocation::from_persisted(maildir_path);
            let graph_msg_id = body_location
                .graph_item_id()
                .map(str::to_string)
                .unwrap_or_else(|| {
                    BackendMessageRef::graph_from_db_id(&account.id, db_id)
                        .into_graph_item_id()
                        .expect("Graph parser must return a Graph reference")
                });
            let flags: Vec<String> = serde_json::from_str(flags_json).unwrap_or_default();

            let folder_dir = sanitize_folder_name(folder_path);
            let maildir_base = ctx.data_dir.join(&account.id).join(&folder_dir);
            create_maildir_dirs(&maildir_base)?;
            let filename = format!("{}:2,{}", graph_msg_id, flags_to_maildir_suffix(&flags));
            let msg_path = maildir_base.join("cur").join(&filename);

            match client.download_mime_to_file(&graph_msg_id, &msg_path).await {
                Ok(bytes) => {
                    let relative = format!("{}/{}/cur/{}", account.id, folder_dir, filename);
                    let conn = ctx.db.writer().await;
                    db::messages::update_maildir_path(&conn, db_id, &relative)?;
                    log::debug!("Graph prefetch: {} ({} bytes)", relative, bytes);
                    fetched += 1;
                }
                Err(e) => {
                    // Clean up any partial file; the row stays unfetched
                    // and is retried on a later pass or on demand.
                    let _ = std::fs::remove_file(&msg_path);
                    log::warn!("Graph prefetch: failed for {}: {}", graph_msg_id, e);
                }
            }
        }

        log::info!(
            "Graph prefetch: {} of {} bodies fetched for account {}",
            fetched,
            unfetched.len(),
            account.id
        );
        Ok(fetched)
    }

    fn op_executor(&self) -> Box<dyn MailOpExecutor> {
        Box::new(GraphOpExecutor)
    }
}

/// Stateless executor for queued Graph operations.
pub(super) struct GraphOpExecutor;

#[async_trait]
impl MailOpExecutor for GraphOpExecutor {
    async fn execute(&mut self, _ctx: &MailSyncCtx, account_id: &str, op: MailOp) -> Result<()> {
        match op {
            MailOp::CopyMessages {
                message_refs,
                target_folder,
            } => {
                let item_ids = message_refs
                    .into_iter()
                    .map(|message_ref| {
                        message_ref.into_graph_item_id().ok_or_else(|| {
                            Error::Other(
                                "Graph executor received a non-Graph message reference".into(),
                            )
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let token = crate::mail::graph::get_graph_token(account_id).await?;
                let client = crate::mail::graph::GraphClient::new(&token);
                let outcomes = client
                    .copy_messages_batch(&item_ids, &target_folder)
                    .await?;
                let failures: Vec<String> = item_ids
                    .iter()
                    .zip(outcomes)
                    .filter_map(|(item_id, outcome)| {
                        outcome.err().map(|error| format!("{}: {}", item_id, error))
                    })
                    .collect();
                if !failures.is_empty() {
                    return Err(Error::Other(format!(
                        "Graph copy failed for {} message(s): {}",
                        failures.len(),
                        failures.join("; ")
                    )));
                }
            }
            MailOp::MoveMessages {
                message_refs,
                target_folder,
            } => {
                let item_ids = message_refs
                    .into_iter()
                    .map(|message_ref| {
                        message_ref.into_graph_item_id().ok_or_else(|| {
                            Error::Other(
                                "Graph executor received a non-Graph message reference".into(),
                            )
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let token = crate::mail::graph::get_graph_token(account_id).await?;
                let client = crate::mail::graph::GraphClient::new(&token);
                let outcomes = client
                    .move_messages_batch(&item_ids, &target_folder)
                    .await?;
                let failures: Vec<String> = item_ids
                    .iter()
                    .zip(outcomes)
                    .filter_map(|(item_id, outcome)| match outcome {
                        Ok(()) => None,
                        Err(error) if crate::mail::graph::is_item_not_found(&error) => {
                            log::debug!("Graph message {} was already moved", item_id);
                            None
                        }
                        Err(error) => Some(format!("{}: {}", item_id, error)),
                    })
                    .collect();
                if !failures.is_empty() {
                    return Err(Error::Other(format!(
                        "Graph move failed for {} message(s): {}",
                        failures.len(),
                        failures.join("; ")
                    )));
                }
            }
            MailOp::DeleteMessages { message_refs } => {
                let item_ids = message_refs
                    .into_iter()
                    .map(|message_ref| {
                        message_ref.into_graph_item_id().ok_or_else(|| {
                            Error::Other(
                                "Graph executor received a non-Graph message reference".into(),
                            )
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let token = crate::mail::graph::get_graph_token(account_id).await?;
                let client = crate::mail::graph::GraphClient::new(&token);
                let outcomes = client.delete_messages_batch(&item_ids).await?;
                let failures: Vec<String> = item_ids
                    .iter()
                    .zip(outcomes)
                    .filter_map(|(item_id, outcome)| match outcome {
                        Ok(()) => None,
                        Err(error) if crate::mail::graph::is_item_not_found(&error) => {
                            log::debug!("Graph message {} was already deleted", item_id);
                            None
                        }
                        Err(error) => Some(format!("{}: {}", item_id, error)),
                    })
                    .collect();
                if !failures.is_empty() {
                    return Err(Error::Other(format!(
                        "Graph delete failed for {} message(s): {}",
                        failures.len(),
                        failures.join("; ")
                    )));
                }
            }
            MailOp::SetFlags { mutations } => {
                let prepared = mutations
                    .into_iter()
                    .map(|mutation| {
                        let FlagTarget::Messages(message_refs) = mutation.target else {
                            return Err(Error::Other(
                                "Graph executor received an IMAP bulk flag target".into(),
                            ));
                        };
                        let item_ids = message_refs
                            .into_iter()
                            .map(|message_ref| {
                                message_ref.into_graph_item_id().ok_or_else(|| {
                                    Error::Other(
                                        "Graph executor received a non-Graph message reference"
                                            .into(),
                                    )
                                })
                            })
                            .collect::<Result<Vec<_>>>()?;
                        Ok((item_ids, mutation.flags, mutation.add))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let token = crate::mail::graph::get_graph_token(account_id).await?;
                let client = crate::mail::graph::GraphClient::new(&token);
                for (item_ids, flags, add) in prepared {
                    client.set_flags(&item_ids, &flags, add).await?;
                }
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
