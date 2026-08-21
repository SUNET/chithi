//! IMAP mail backend. The blocking `ImapConnection` is bridged with
//! `spawn_blocking` — inside `mail::sync::sync_account` for the full
//! sync, and locally here for the folder sync and prefetch pipeline.

use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::account::MailAccountConfig;
use crate::db;
use crate::error::{Error, Result};
use crate::event::{ApplicationEvent, SyncStarted};
use crate::mail::imap::{ImapConfig, ImapConnection};
use crate::mail::sync as mail_sync;
use crate::message::{SearchHit, SearchQuery};
use crate::ops::flags::FlagTarget;
use crate::ops::queue::MailOp;

use super::{
    BodyFetchRequest, DraftSaveRequest, DraftStorageFormat, MailBackend, MailOpExecutor,
    MailSyncCtx,
};

pub struct ImapMailBackend;

const DRAFT_FOLDERS: [&str; 3] = ["Drafts", "INBOX.Drafts", "[Gmail]/Drafts"];

fn body_fetch_target(request: &BodyFetchRequest) -> Result<(&str, u32)> {
    match &request.message_ref {
        crate::message::BackendMessageRef::Imap { folder_path, uid } => Ok((folder_path, *uid)),
        _ => Err(Error::Other(
            "IMAP body fetch received a non-IMAP message reference".into(),
        )),
    }
}

fn validate_search_query(query: &SearchQuery) -> Result<()> {
    if query.has_attachment.is_some() {
        return Err(Error::UnsupportedCapability {
            protocol: "imap",
            capability: "server-side attachment filtering",
        });
    }
    Ok(())
}

/// (message_id, uid, flags_json) tuple for prefetch grouping.
type PrefetchMsg = (String, u32, String);

fn prefetch_connection_limit(auth_method: &str, folder_count: usize) -> usize {
    if auth_method == "oauth-microsoft" {
        1.min(folder_count)
    } else {
        3.min(folder_count)
    }
}

/// Build the connection config, refreshing the O365 IMAP-scoped token
/// when needed.
async fn build_imap_config(ctx: &MailSyncCtx, account: &MailAccountConfig) -> Result<ImapConfig> {
    let credentials = ctx
        .providers
        .credentials()
        .mail_credentials_for(account)
        .await?;
    Ok(ImapConfig {
        host: account.imap_host.clone(),
        port: account.imap_port,
        username: account.username.clone(),
        password: credentials.secret,
        use_tls: account.use_tls,
        use_xoauth2: credentials.use_xoauth2,
    })
}

/// Prefetch message bodies over IMAP: one pass over the unfetched
/// backlog (up to 1000 messages), grouped by folder to minimize
/// SELECTs, spread over up to 3 parallel connections. Also used by
/// the Graph backend — O365 accounts keep IMAP access and their
/// Graph sync can leave on-demand rows behind.
pub(super) async fn prefetch_pipeline(
    ctx: &MailSyncCtx,
    account: &MailAccountConfig,
) -> Result<u32> {
    // Graph-protocol accounts without an IMAP binding have no host to
    // connect to. Without this guard every sync-complete kicked off a
    // doomed connect to ":993" plus keyring/token reads for credentials.
    if account.imap_host.is_empty() {
        log::debug!(
            "Prefetch: account {} has no IMAP host configured, skipping",
            account.id
        );
        return Ok(0);
    }

    let account_id = account.id.clone();
    let imap_config = build_imap_config(ctx, account).await?;
    let data_dir = ctx.data_dir.clone();

    // Fetch the list of unfetched messages (up to 1000 per cycle)
    let unfetched = {
        let conn = ctx.db.reader();
        db::messages::get_unfetched_messages(&conn, &account_id, 1000)?
    };

    if unfetched.is_empty() {
        log::info!("Prefetch: no unfetched messages for account {}", account_id);
        return Ok(0);
    }

    log::info!(
        "Prefetch: {} unfetched messages to process for account {}",
        unfetched.len(),
        account_id
    );

    // Group messages by folder to minimize IMAP SELECT commands.
    // BTreeMap keeps folders sorted for deterministic ordering.
    let mut by_folder: BTreeMap<String, Vec<PrefetchMsg>> = BTreeMap::new();
    for (message_id, folder_path, uid, flags_json) in unfetched {
        by_folder
            .entry(folder_path)
            .or_default()
            .push((message_id, uid, flags_json));
    }

    let db = ctx.db.clone();
    let folder_count = by_folder.len();
    let max_connections = prefetch_connection_limit(&account.auth_method, folder_count);

    log::info!(
        "Prefetch: {} folders with {} parallel connections",
        folder_count,
        max_connections
    );

    let fetched_count = tokio::task::spawn_blocking(move || -> Result<u32> {
        let rt = tokio::runtime::Handle::current();
        let _guard = rt.enter();

        // Distribute folders across threads
        let folder_list: Vec<(String, Vec<PrefetchMsg>)> = by_folder.into_iter().collect();
        let mut thread_work: Vec<Vec<(String, Vec<PrefetchMsg>)>> =
            (0..max_connections).map(|_| Vec::new()).collect();
        for (i, item) in folder_list.into_iter().enumerate() {
            thread_work[i % max_connections].push(item);
        }

        let rt_handle = tokio::runtime::Handle::current();
        let results: Vec<Result<u32>> = std::thread::scope(|s| {
            let handles: Vec<_> = thread_work
                .into_iter()
                .enumerate()
                .map(|(thread_idx, folders)| {
                    let imap_config = imap_config.clone();
                    let account_id = account_id.clone();
                    let data_dir = data_dir.clone();
                    let db = db.clone();
                    let rt = rt_handle.clone();
                    s.spawn(move || {
                        let _guard = rt.enter();
                        let mut conn = match ImapConnection::connect(&imap_config) {
                            Ok(c) => c,
                            Err(e) => {
                                log::error!(
                                    "Prefetch thread {}: connect failed: {}",
                                    thread_idx,
                                    e
                                );
                                return Err(e);
                            }
                        };
                        let mut count = 0u32;

                        for (folder_path, messages) in &folders {
                            log::info!(
                                "Prefetch[{}]: folder '{}' ({} messages)",
                                thread_idx,
                                folder_path,
                                messages.len()
                            );
                            if let Err(e) = conn.select_folder(folder_path) {
                                log::error!(
                                    "Prefetch[{}]: select '{}' failed: {}",
                                    thread_idx,
                                    folder_path,
                                    e
                                );
                                continue;
                            }

                            let sanitized = mail_sync::sanitize_folder_name(folder_path);
                            let maildir_base = data_dir.join(&account_id).join(&sanitized);
                            if let Err(e) = mail_sync::create_maildir_dirs(&maildir_base) {
                                log::error!("Prefetch[{}]: maildir dirs failed: {}", thread_idx, e);
                                continue;
                            }

                            for chunk in messages.chunks(100) {
                                let batch_uids: Vec<u32> =
                                    chunk.iter().map(|(_, uid, _)| *uid).collect();
                                let bodies = match conn.fetch_bodies_batch(&batch_uids) {
                                    Ok(b) => b,
                                    Err(e) => {
                                        log::error!(
                                            "Prefetch[{}]: batch fetch failed: {}",
                                            thread_idx,
                                            e
                                        );
                                        continue;
                                    }
                                };

                                let mut db_updates: Vec<(String, String)> = Vec::new();
                                for (message_id, uid, flags_json) in chunk {
                                    let body = match bodies.get(uid) {
                                        Some(b) => b,
                                        None => continue,
                                    };
                                    let flags: Vec<String> =
                                        serde_json::from_str(flags_json).unwrap_or_default();
                                    let suffix = mail_sync::flags_to_maildir_suffix(&flags);
                                    let filename = format!("{}:2,{}", uid, suffix);
                                    let msg_path = maildir_base.join("cur").join(&filename);
                                    if std::fs::write(&msg_path, body).is_err() {
                                        continue;
                                    }
                                    let relative_path =
                                        format!("{}/{}/cur/{}", account_id, sanitized, filename);
                                    db_updates.push((message_id.clone(), relative_path));
                                }

                                if !db_updates.is_empty() {
                                    let saved = db_updates.len() as u32;
                                    let conn = rt.block_on(db.writer());
                                    let tx = conn.unchecked_transaction()?;
                                    for (msg_id, path) in &db_updates {
                                        db::messages::update_maildir_path(&tx, msg_id, path)?;
                                    }
                                    tx.commit()?;
                                    count += saved;
                                    log::info!(
                                        "Prefetch[{}]: saved {} bodies in '{}'",
                                        thread_idx,
                                        db_updates.len(),
                                        folder_path
                                    );
                                }
                            }
                        }

                        conn.logout();
                        Ok(count)
                    })
                })
                .collect();

            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or(Err(Error::Sync("Prefetch thread panicked".into())))
                })
                .collect()
        });

        let mut total = 0u32;
        let mut first_error: Option<Error> = None;
        for result in results {
            match result {
                Ok(count) => total += count,
                Err(e) => {
                    log::error!("Prefetch worker failed for account {}: {}", account_id, e);
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            }
        }

        if let Some(e) = first_error {
            return Err(e);
        }

        log::info!(
            "Prefetch: completed for account {}, {} bodies fetched",
            account_id,
            total
        );
        Ok(total)
    })
    .await
    .map_err(|e| Error::Sync(format!("Prefetch task panicked: {}", e)))??;

    Ok(fetched_count)
}

#[async_trait]
impl MailBackend for ImapMailBackend {
    fn protocol(&self) -> &'static str {
        "imap"
    }

    /// O365 IMAP needs IDLE suspended around any other operation
    /// because Microsoft's server only allows one connection per
    /// account at a time. Identifying O365 via auth_method is more
    /// accurate than the legacy `provider` string since Phase 3
    /// dropped that column.
    fn suspends_idle_for_ops(&self, account: &MailAccountConfig) -> bool {
        account.auth_method == "oauth-microsoft"
    }

    async fn sync_account(
        &self,
        ctx: &MailSyncCtx,
        account: &MailAccountConfig,
        current_folder: Option<String>,
    ) -> Result<()> {
        log::info!(
            "Syncing account {} ({}) via IMAP {}:{}",
            account.display_name,
            account.email,
            account.imap_host,
            account.imap_port
        );

        let imap_config = build_imap_config(ctx, account).await?;

        mail_sync::sync_account(
            ctx.events.clone(),
            ctx.db.clone(),
            ctx.data_dir.clone(),
            account.id.clone(),
            account.display_name.clone(),
            imap_config,
            current_folder,
        )
        .await
    }

    async fn sync_folder(
        &self,
        ctx: &MailSyncCtx,
        account: &MailAccountConfig,
        folder_path: &str,
    ) -> Result<u32> {
        // sync_folder_envelopes_public is a low-level helper and
        // doesn't emit sync-started itself, so do it here.
        ctx.events
            .publish(ApplicationEvent::SyncStarted(SyncStarted {
                account_id: account.id.clone(),
                account_name: account.display_name.clone(),
            }));

        let imap_config = build_imap_config(ctx, account).await?;

        let db = ctx.db.clone();
        let account_id = account.id.clone();
        let folder_clone = folder_path.to_string();

        tokio::task::spawn_blocking(move || {
            let mut conn_imap = ImapConnection::connect(&imap_config)?;
            conn_imap.select_folder(&folder_clone)?;
            let count = mail_sync::sync_folder_envelopes_public(
                &db,
                &account_id,
                &mut conn_imap,
                &folder_clone,
            )?;
            conn_imap.logout();
            Ok::<u32, Error>(count)
        })
        .await
        .map_err(|e| Error::Sync(format!("Folder sync panicked: {}", e)))?
    }

    async fn prefetch_bodies(&self, ctx: &MailSyncCtx, account: &MailAccountConfig) -> Result<u32> {
        prefetch_pipeline(ctx, account).await
    }

    async fn fetch_body_to_disk(
        &self,
        ctx: &MailSyncCtx,
        account: &MailAccountConfig,
        request: &BodyFetchRequest,
    ) -> Result<String> {
        let (folder_path, uid) = body_fetch_target(request)?;

        let imap_config = build_imap_config(ctx, account).await?;
        let data_dir = ctx.data_dir.clone();
        let account_id = account.id.clone();
        let folder_path = folder_path.to_string();
        let flags = request.flags.clone();
        tokio::task::spawn_blocking(move || {
            mail_sync::fetch_and_store_body(
                &imap_config,
                &data_dir,
                &account_id,
                &folder_path,
                uid,
                &flags,
            )
        })
        .await
        .map_err(|e| Error::Other(format!("IMAP body fetch task panicked: {}", e)))?
    }

    async fn search_messages(
        &self,
        ctx: &MailSyncCtx,
        account: &MailAccountConfig,
        query: &SearchQuery,
    ) -> Result<Vec<SearchHit>> {
        validate_search_query(query)?;

        let imap_config = build_imap_config(ctx, account).await?;
        let account_id = account.id.clone();
        let query = query.clone();
        tokio::task::spawn_blocking(move || {
            crate::mail::imap::search_account_blocking(&imap_config, &account_id, &query)
        })
        .await
        .map_err(|e| Error::Other(format!("IMAP search task panicked: {}", e)))?
    }

    fn draft_storage_format(&self) -> DraftStorageFormat {
        DraftStorageFormat::RawMime
    }

    async fn save_draft(
        &self,
        ctx: &MailSyncCtx,
        account: &MailAccountConfig,
        request: &DraftSaveRequest,
    ) -> Result<()> {
        let imap_config = build_imap_config(ctx, account).await?;
        let raw_message = request.raw_message.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = ImapConnection::connect(&imap_config)?;
            let mut saved = false;
            for folder in DRAFT_FOLDERS {
                match connection.append_message(folder, &raw_message) {
                    Ok(()) => {
                        saved = true;
                        break;
                    }
                    Err(error) => {
                        log::debug!("Draft folder '{}' failed: {}", folder, error);
                    }
                }
            }
            if !saved {
                return Err(Error::Other("Could not find Drafts folder".into()));
            }
            connection.logout();
            Ok(())
        })
        .await
        .map_err(|e| Error::Other(format!("Draft save task failed: {}", e)))?
    }

    fn op_executor(&self) -> Box<dyn MailOpExecutor> {
        Box::new(ImapOpExecutor::new())
    }
}

// ---------------------------------------------------------------------------
// Queued-op executor (persistent connection)
// ---------------------------------------------------------------------------

/// Sync one folder's envelopes without the command-path event
/// contract: no `sync-started` (nothing should spin), but
/// folders/messages-changed so the UI picks up the new rows. Used by
/// the ops worker's queued folder syncs and the post-send Sent-folder
/// nudge.
pub(crate) async fn sync_folder_quiet(
    ctx: &MailSyncCtx,
    account: &MailAccountConfig,
    folder_path: &str,
) -> Result<()> {
    let imap_config = build_imap_config(ctx, account).await?;
    let db = ctx.db.clone();
    let account_id = account.id.clone();
    let events = ctx.events.clone();
    let folder_path = folder_path.to_string();
    tokio::task::spawn_blocking(move || {
        let mut conn = ImapConnection::connect(&imap_config)?;
        crate::mail::sync::sync_folder_envelopes_public(&db, &account_id, &mut conn, &folder_path)?;
        conn.logout();
        events.publish(ApplicationEvent::FoldersChanged(account_id.clone()));
        events.publish(ApplicationEvent::MessagesChanged(account_id));
        Ok::<_, Error>(())
    })
    .await
    .map_err(|e| Error::Sync(format!("Sync folder panicked: {}", e)))??;
    Ok(())
}

/// Wrapper around ImapConnection + selected folder state.
/// Stored separately so it can be moved into `spawn_blocking` without
/// requiring the whole executor to be `Send + Sync`.
///
/// # Safety
///
/// `ImapState` is manually marked `Send` because `ImapConnection` contains
/// a `Receiver<UnsolicitedResponse>` which is `!Sync`. However, we guarantee
/// exclusive single-threaded access: the value is always moved (not shared)
/// into a `tokio::task::spawn_blocking` closure, used within that closure,
/// and then moved back. It is never accessed concurrently from multiple
/// threads.
struct ImapState {
    conn: ImapConnection,
    selected_folder: Option<String>,
}

// SAFETY: see doc-comment on `ImapState` above. The value is only ever
// moved into `spawn_blocking` for single-threaded access — never shared.
unsafe impl Send for ImapState {}

/// Executes queued ops on a persistent IMAP connection (reused across
/// operations, reconnected when stale) — one per ops worker.
pub(super) struct ImapOpExecutor {
    imap_state: Option<ImapState>,
    last_used: std::time::Instant,
    /// Consecutive connection failures — used for exponential backoff to
    /// avoid burning OAuth token refreshes in a tight reconnect loop.
    consecutive_failures: u32,
}

impl ImapOpExecutor {
    fn new() -> Self {
        Self {
            imap_state: None,
            last_used: std::time::Instant::now(),
            consecutive_failures: 0,
        }
    }

    /// Ensure the persistent IMAP connection is alive.
    /// Reconnects if the connection is stale (>5 min) or missing.
    /// Uses exponential backoff on consecutive failures to avoid burning
    /// OAuth token refreshes in a tight reconnect loop.
    async fn ensure_imap_connection(&mut self, ctx: &MailSyncCtx, account_id: &str) -> Result<()> {
        let stale = self.last_used.elapsed() > std::time::Duration::from_secs(5 * 60);

        if self.imap_state.is_none() || stale {
            // Exponential backoff: 1s, 2s, 4s, 8s, ... max 60s
            if self.consecutive_failures > 0 {
                let delay_secs = std::cmp::min(1u64 << (self.consecutive_failures - 1), 60);
                log::info!(
                    "Worker: backoff {}s before reconnect (failures={}) for account {}",
                    delay_secs,
                    self.consecutive_failures,
                    account_id
                );
                tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
            }

            // Drop old connection if stale
            if let Some(state) = self.imap_state.take() {
                let _ = tokio::task::spawn_blocking(move || state.conn.logout()).await;
            }

            // Re-read the account so a rotated password / refreshed
            // token is picked up on reconnect.
            let account = {
                let conn = ctx.db.reader();
                crate::db::accounts::get_account_full(&conn, account_id)?
            }
            .mail_config();
            let config = build_imap_config(ctx, &account).await?;

            let conn = tokio::task::spawn_blocking(move || ImapConnection::connect(&config))
                .await
                .map_err(|e| Error::Other(format!("IMAP connect task panicked: {}", e)))??;

            self.imap_state = Some(ImapState {
                conn,
                selected_folder: None,
            });
            self.last_used = std::time::Instant::now();
            self.consecutive_failures = 0;
            log::info!(
                "Worker: IMAP connection established for account {}",
                account_id
            );
        }

        Ok(())
    }

    /// Send a queued `MailOp::SendRaw` over the account's SMTP host —
    /// not the persistent IMAP connection, so a stalled mail server
    /// doesn't gate retries of a queued send. SMTP delivery is shared
    /// with Graph; only IMAP runs the post-delivery Sent hook below.
    async fn deliver_smtp_send(
        &mut self,
        ctx: &MailSyncCtx,
        account_id: &str,
        op: MailOp,
    ) -> Result<super::SendPostprocess> {
        let delivery = super::replay_send_raw_via_smtp(ctx, account_id, op).await?;
        Ok(super::SendPostprocess::ImapSent(Box::new(delivery)))
    }
}

pub(super) async fn postprocess_smtp_delivery(
    ctx: &MailSyncCtx,
    account_id: &str,
    delivery: super::RawSmtpDelivery,
) {
    let super::RawSmtpDelivery {
        account,
        credentials,
        raw_message,
    } = delivery;

    // Delivery is already complete in the outbox. APPEND and the resulting
    // sync are best-effort only and must never cause recipient resubmission.
    let sent_folder_path = {
        let conn = ctx.db.reader();
        crate::db::folders::folder_path_by_type(&conn, account_id, "sent")
            .ok()
            .flatten()
    };
    let imap_config = ImapConfig {
        host: account.imap_host.clone(),
        port: account.imap_port,
        username: account.username.clone(),
        password: credentials.secret,
        use_tls: account.use_tls,
        use_xoauth2: credentials.use_xoauth2,
    };
    let account_id_append = account_id.to_string();
    let append_result = tokio::task::spawn_blocking(move || {
        crate::mail::imap::append_message_to_sent(
            &imap_config,
            sent_folder_path.as_deref(),
            &raw_message,
        )
    })
    .await;
    match append_result {
        Ok(Ok(sent_folder)) => {
            log::info!(
                "Outbox replay: APPENDed sent message to '{}' for account {}",
                sent_folder,
                account_id_append
            );
            if let Err(e) = sync_folder_quiet(ctx, &account, &sent_folder).await {
                log::warn!(
                    "Outbox replay: Sent-folder sync nudge failed for account {}: {}",
                    account_id_append,
                    e
                );
            }
        }
        Ok(Err(e)) => log::warn!(
            "Outbox replay: delivered but APPEND to Sent failed for account {}: {}",
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
                "Outbox replay: APPEND-to-Sent task {} for account {}: {}",
                kind,
                account_id_append,
                e
            );
        }
    }
}

#[async_trait]
impl MailOpExecutor for ImapOpExecutor {
    async fn execute(&mut self, ctx: &MailSyncCtx, account_id: &str, op: MailOp) -> Result<()> {
        // SendRaw on an IMAP account goes out over SMTP, not the persistent
        // IMAP connection. Branch before we touch IMAP state so a stalled
        // mail server doesn't gate retries of a queued send.
        if let MailOp::SendRaw { .. } = op {
            let postprocess = self.deliver_smtp_send(ctx, account_id, op).await?;
            postprocess.run_best_effort(ctx, account_id).await;
            return Ok(());
        }

        // Ensure we have a live connection
        self.ensure_imap_connection(ctx, account_id).await?;

        // Move the ImapState into spawn_blocking (ImapConnection is !Sync)
        let mut imap_state = self.imap_state.take().unwrap();

        let (result, state_back) = tokio::task::spawn_blocking(move || {
            let result = execute_imap_op(&mut imap_state.conn, &mut imap_state.selected_folder, op);
            (result, imap_state)
        })
        .await
        .map_err(|e| Error::Other(format!("IMAP op task panicked: {}", e)))?;

        if result.is_ok() {
            self.imap_state = Some(state_back);
            self.last_used = std::time::Instant::now();
            self.consecutive_failures = 0;
        } else {
            // Connection is likely dead — drop it so next op reconnects
            log::warn!("IMAP op failed, dropping connection for reconnect");
            self.consecutive_failures += 1;
            state_back.conn.logout();
        }

        result
    }

    async fn execute_replayed_send(
        &mut self,
        ctx: &MailSyncCtx,
        account_id: &str,
        op: MailOp,
    ) -> Result<super::SendPostprocess> {
        self.deliver_smtp_send(ctx, account_id, op).await
    }

    async fn shutdown(&mut self) {
        if let Some(state) = self.imap_state.take() {
            state.conn.logout();
        }
    }
}

/// Execute a single IMAP operation on a connection (runs in spawn_blocking).
fn execute_imap_op(
    conn: &mut ImapConnection,
    selected: &mut Option<String>,
    op: MailOp,
) -> Result<()> {
    match op {
        MailOp::MoveMessages {
            message_refs,
            target_folder,
        } => {
            let mut by_folder = std::collections::HashMap::<String, Vec<u32>>::new();
            for message_ref in message_refs {
                let crate::message::BackendMessageRef::Imap { folder_path, uid } = message_ref
                else {
                    return Err(Error::Other(
                        "IMAP executor received a non-IMAP message reference".into(),
                    ));
                };
                by_folder.entry(folder_path).or_default().push(uid);
            }
            for (folder_path, uids) in &by_folder {
                select_folder_if_needed(conn, selected, folder_path)?;
                conn.move_messages(uids, &target_folder)?;
            }
        }
        MailOp::DeleteMessages { message_refs } => {
            let mut by_folder = std::collections::HashMap::<String, Vec<u32>>::new();
            for message_ref in message_refs {
                let crate::message::BackendMessageRef::Imap { folder_path, uid } = message_ref
                else {
                    return Err(Error::Other(
                        "IMAP executor received a non-IMAP message reference".into(),
                    ));
                };
                by_folder.entry(folder_path).or_default().push(uid);
            }
            for (folder_path, uids) in &by_folder {
                select_folder_if_needed(conn, selected, folder_path)?;
                conn.delete_messages(uids)?;
            }
        }
        MailOp::SetFlags { mutations } => {
            for mutation in mutations {
                match mutation.target {
                    FlagTarget::Messages(message_refs) => {
                        let mut by_folder = std::collections::HashMap::<String, Vec<u32>>::new();
                        for message_ref in message_refs {
                            let crate::message::BackendMessageRef::Imap { folder_path, uid } =
                                message_ref
                            else {
                                return Err(Error::Other(
                                    "IMAP executor received a non-IMAP message reference".into(),
                                ));
                            };
                            by_folder.entry(folder_path).or_default().push(uid);
                        }
                        let flag_strs: Vec<&str> =
                            mutation.flags.iter().map(String::as_str).collect();
                        for (folder_path, uids) in &by_folder {
                            select_folder_if_needed(conn, selected, folder_path)?;
                            conn.set_flags(uids, &flag_strs, mutation.add)?;
                        }
                    }
                    FlagTarget::AllMessagesInFolders {
                        folder_paths,
                        excluded_refs,
                    } => {
                        if !mutation.add
                            || mutation.flags.len() != 1
                            || !mutation.flags[0].eq_ignore_ascii_case("seen")
                        {
                            return Err(Error::Other(
                                "IMAP bulk flag target only supports adding seen".into(),
                            ));
                        }
                        let mut excluded_by_folder =
                            std::collections::HashMap::<String, Vec<u32>>::new();
                        for message_ref in excluded_refs {
                            let crate::message::BackendMessageRef::Imap { folder_path, uid } =
                                message_ref
                            else {
                                return Err(Error::Other(
                                    "IMAP bulk target received a non-IMAP exclusion".into(),
                                ));
                            };
                            if !folder_paths.contains(&folder_path) {
                                return Err(Error::Other(
                                    "IMAP bulk target exclusion is outside its folders".into(),
                                ));
                            }
                            excluded_by_folder.entry(folder_path).or_default().push(uid);
                        }
                        let selectable_folders = conn.list_selectable_folder_paths()?;
                        for folder_path in &folder_paths {
                            if !selectable_folders.contains(folder_path) {
                                log::debug!(
                                    "Skipping non-selectable or stale IMAP folder '{}' for bulk read",
                                    folder_path
                                );
                                continue;
                            }
                            select_folder_if_needed(conn, selected, folder_path)?;
                            conn.mark_all_seen()?;
                            if let Some(uids) = excluded_by_folder.get(folder_path) {
                                conn.set_flags(uids, &["seen"], false)?;
                            }
                        }
                    }
                }
            }
        }
        MailOp::CopyMessages {
            message_refs,
            target_folder,
        } => {
            let by_folder = group_imap_message_refs(message_refs)?;
            for (folder_path, uids) in &by_folder {
                select_folder_if_needed(conn, selected, folder_path)?;
                conn.copy_messages(uids, &target_folder)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn group_imap_message_refs(
    message_refs: Vec<crate::message::BackendMessageRef>,
) -> Result<std::collections::HashMap<String, Vec<u32>>> {
    let mut by_folder = std::collections::HashMap::<String, Vec<u32>>::new();
    for message_ref in message_refs {
        let crate::message::BackendMessageRef::Imap { folder_path, uid } = message_ref else {
            return Err(Error::Other(
                "IMAP executor received a non-IMAP message reference".into(),
            ));
        };
        by_folder.entry(folder_path).or_default().push(uid);
    }
    Ok(by_folder)
}

/// SELECT a folder on the IMAP connection, skipping if already selected.
fn select_folder_if_needed(
    conn: &mut ImapConnection,
    selected: &mut Option<String>,
    folder: &str,
) -> Result<()> {
    if selected.as_deref() != Some(folder) {
        conn.select_folder(folder)?;
        *selected = Some(folder.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        body_fetch_target, group_imap_message_refs, prefetch_connection_limit,
        validate_search_query,
    };
    use crate::backend::mail::BodyFetchRequest;
    use crate::message::{BackendMessageRef, BodyLocation, SearchFields, SearchQuery};

    #[test]
    fn copy_references_are_grouped_by_source_folder() {
        let grouped = group_imap_message_refs(vec![
            BackendMessageRef::imap("INBOX", 1),
            BackendMessageRef::imap("Archive", 2),
            BackendMessageRef::imap("INBOX", 3),
        ])
        .unwrap();
        assert_eq!(grouped["INBOX"], vec![1, 3]);
        assert_eq!(grouped["Archive"], vec![2]);
    }

    #[test]
    fn copy_rejects_non_imap_references() {
        let result = group_imap_message_refs(vec![BackendMessageRef::graph("item")]);
        assert!(result.is_err());
    }

    #[test]
    fn body_fetch_rejects_non_imap_reference() {
        let request = BodyFetchRequest {
            message_id: "db-id".into(),
            message_ref: BackendMessageRef::graph("item"),
            folder_path: "INBOX".into(),
            flags: Vec::new(),
            body_location: BodyLocation::NotFetched,
        };
        assert!(body_fetch_target(&request).is_err());
    }

    #[test]
    fn o365_prefetch_uses_one_connection() {
        assert_eq!(prefetch_connection_limit("oauth-microsoft", 3), 1);
        assert_eq!(prefetch_connection_limit("password", 3), 3);
    }

    #[test]
    fn attachment_filtering_is_explicitly_unsupported() {
        for has_attachment in [true, false] {
            let query = SearchQuery {
                text: "report".into(),
                fields: SearchFields::default(),
                has_attachment: Some(has_attachment),
                since_days: None,
            };
            let error = validate_search_query(&query).unwrap_err();
            assert_eq!(
                error.to_string(),
                "imap does not support server-side attachment filtering"
            );
        }
    }
}
