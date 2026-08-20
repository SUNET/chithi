use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::backend::mail::{MailBackend, MailOpExecutor, MailSyncCtx};
use crate::db::pool::DbPool;
use crate::error::Result;

use super::coalesce::coalesce;
use super::lifecycle::{SpawnedWorker, WorkerTaskExit};
use super::queue::{MailOp, OpEntry};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerExit {
    ChannelClosed,
    ShutdownRequested,
    InitializationFailed,
}

/// Per-account worker that processes mail operations.
///
/// Each enabled account gets one worker. The worker:
/// - Drains and coalesces pending operations on each iteration
/// - Prioritises user ops (move/copy/delete/flag) over background sync
/// - Executes ops through the account's [`MailOpExecutor`] (the IMAP
///   executor maintains a persistent connection and reconnects when it
///   goes stale; JMAP/Graph executors are stateless HTTP)
/// - Routes sync ops through the account's [`MailBackend`]
pub struct AccountWorker {
    pub account_id: String,
    rx: mpsc::Receiver<OpEntry>,
    db: Arc<DbPool>,
    app: AppHandle,
    /// Resolved once at startup from the account's mail binding.
    backend: Option<&'static dyn MailBackend>,
    executor: Option<Box<dyn MailOpExecutor>>,
    ctx: Option<MailSyncCtx>,
}

impl AccountWorker {
    pub fn new(
        account_id: String,
        rx: mpsc::Receiver<OpEntry>,
        db: Arc<DbPool>,
        app: AppHandle,
    ) -> Self {
        Self {
            account_id,
            rx,
            db,
            app,
            backend: None,
            executor: None,
            ctx: None,
        }
    }

    /// Spawn the worker behind a monitor that observes panics and cancellation.
    pub fn spawn_supervised(self, cancellation: CancellationToken) -> SpawnedWorker {
        let account_id = self.account_id.clone();
        let app = self.app.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            match tokio::spawn(self.run(cancellation, ready_tx)).await {
                Ok(exit) => {
                    log::info!("Worker for account {} exited: {:?}", account_id, exit);
                    WorkerTaskExit::Completed
                }
                Err(error) if error.is_panic() => {
                    log::error!("Worker for account {} panicked: {}", account_id, error);
                    emit_op_failed(
                        &app,
                        &account_id,
                        "worker_runtime",
                        "Worker stopped unexpectedly",
                    );
                    WorkerTaskExit::Panicked
                }
                Err(error) => {
                    log::error!("Worker for account {} was cancelled: {}", account_id, error);
                    emit_op_failed(
                        &app,
                        &account_id,
                        "worker_runtime",
                        "Worker was cancelled unexpectedly",
                    );
                    WorkerTaskExit::Cancelled
                }
            }
        });
        SpawnedWorker {
            task,
            ready: ready_rx,
        }
    }

    /// Main loop — runs until shutdown is requested or the channel is closed.
    pub async fn run(
        mut self,
        cancellation: CancellationToken,
        ready: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
    ) -> WorkerExit {
        log::info!("Worker started for account {}", self.account_id);

        if cancellation.is_cancelled() {
            let _ = ready.send(Err("worker shutdown was requested".to_string()));
            return WorkerExit::ShutdownRequested;
        }

        // Resolve the backend on first run. Shutdown must be able to release a
        // sender acquisition that is waiting for initialization to complete.
        let init_result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                let _ = ready.send(Err("worker shutdown was requested".to_string()));
                return WorkerExit::ShutdownRequested;
            }
            result = self.init_backend() => result,
        };
        if let Err(e) = init_result {
            let message = e.to_string();
            log::error!(
                "Worker for account {} failed to init: {}",
                self.account_id,
                message
            );
            emit_op_failed(
                &self.app,
                &self.account_id,
                "worker_init",
                &format!("Worker failed to initialize: {}", message),
            );
            let _ = ready.send(Err(message));
            return WorkerExit::InitializationFailed;
        }
        let _ = ready.send(Ok(()));

        let mut shutdown_requested = false;
        loop {
            let first = if shutdown_requested {
                self.rx.recv().await
            } else {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        self.rx.close();
                        shutdown_requested = true;
                        self.rx.recv().await
                    }
                    entry = self.rx.recv() => entry,
                }
            };
            let Some(first) = first else {
                break;
            };

            // Drain all pending ops and coalesce
            let mut batch = vec![first];
            while let Ok(next) = self.rx.try_recv() {
                batch.push(next);
            }
            let ops = coalesce(batch);

            let mut sync_succeeded = false;
            let mut replay_requested = false;
            for entry in ops {
                if matches!(entry.op, MailOp::ReplayOffline) {
                    replay_requested = true;
                    continue;
                }
                let is_sync = entry.op.is_sync();
                match self.execute(entry.op).await {
                    Ok(()) => {
                        if is_sync {
                            sync_succeeded = true;
                        }
                    }
                    Err(e) => {
                        log::error!("Worker op failed for account {}: {}", self.account_id, e);
                        // Don't break the loop — continue processing remaining ops
                    }
                }
            }

            // After a successful sync, replay any pending offline operations
            if sync_succeeded || replay_requested {
                self.replay_offline_ops().await;
            }
        }

        // Channel closed — clean up
        if let Some(mut executor) = self.executor.take() {
            executor.shutdown().await;
        }
        log::info!("Worker stopped for account {}", self.account_id);
        if shutdown_requested {
            WorkerExit::ShutdownRequested
        } else {
            WorkerExit::ChannelClosed
        }
    }

    async fn init_backend(&mut self) -> Result<()> {
        let account = {
            let conn = self.db.reader();
            crate::db::accounts::get_account_full(&conn, &self.account_id)?
        };
        self.backend = crate::backend::mail::for_account(&account.mail_config());
        self.executor = self.backend.map(|b| b.op_executor());
        let data_dir = self.app.state::<crate::state::AppState>().data_dir.clone();
        let providers = self.app.state::<crate::state::AppState>().providers.clone();
        self.ctx = Some(MailSyncCtx {
            events: crate::event::tauri::shared_sink(self.app.clone()),
            db: self.db.clone(),
            data_dir,
            providers,
        });
        Ok(())
    }

    /// Run `op` through the account's executor. `Ok(())` when the
    /// account has no mail backend (nothing to execute against).
    async fn execute_op(&mut self, op: MailOp) -> Result<()> {
        let ctx = self.ctx.as_ref().expect("worker initialized");
        match self.executor.as_mut() {
            Some(executor) => executor.execute(ctx, &self.account_id, op).await,
            None => {
                log::warn!(
                    "Worker: no mail backend for account {}, dropping op",
                    self.account_id
                );
                Ok(())
            }
        }
    }

    /// Replay pending offline operations after a successful sync.
    async fn replay_offline_ops(&mut self) {
        let pending = {
            let conn = self.db.reader();
            match super::offline::get_pending_ops(&conn, &self.account_id) {
                Ok(ops) => ops,
                Err(e) => {
                    log::error!("Failed to read offline ops for {}: {}", self.account_id, e);
                    return;
                }
            }
        };

        if pending.is_empty() {
            return;
        }

        log::info!(
            "Replaying {} offline operations for account {}",
            pending.len(),
            self.account_id
        );

        for entry in &pending {
            if super::offline::is_dead(entry) {
                let conn = self.db.writer().await;
                let _ = super::offline::mark_dead(&conn, entry.id);
                log::warn!(
                    "Offline op {} ({}) exceeded max retries, marking dead",
                    entry.id,
                    entry.action_type
                );
                self.app
                    .emit(
                        "offline-queue-changed",
                        serde_json::json!({
                            "account_id": self.account_id,
                            "dead_op_id": entry.id,
                            "action_type": entry.action_type,
                        }),
                    )
                    .ok();
                continue;
            }

            let Some(op) = super::offline::outbox_to_mail_op(entry) else {
                let error = format!(
                    "Invalid or unsupported offline {} payload",
                    entry.action_type
                );
                log::warn!(
                    "Failed to deserialize offline op {} ({}), marking dead",
                    entry.id,
                    entry.action_type
                );
                let conn = self.db.writer().await;
                if let Err(mark_error) = super::offline::mark_invalid(&conn, entry.id, &error) {
                    log::error!(
                        "Failed to mark invalid offline op {} dead: {}",
                        entry.id,
                        mark_error
                    );
                    break;
                }
                drop(conn);
                self.app
                    .emit(
                        "offline-queue-changed",
                        serde_json::json!({
                            "account_id": self.account_id,
                            "dead_op_id": entry.id,
                            "action_type": entry.action_type,
                        }),
                    )
                    .ok();
                continue;
            };
            let retry_safe = op.can_retry_after_execution_failure();

            // Execute the replayed op directly (not through execute() to avoid
            // re-queuing to outbox on failure — we handle retries here)
            let result = self.execute_op(op).await;

            match result {
                Ok(()) => {
                    let conn = self.db.writer().await;
                    let _ = super::offline::mark_completed(&conn, entry.id);
                    log::info!(
                        "Replayed offline op {} ({}) successfully",
                        entry.id,
                        entry.action_type
                    );
                    drop(conn);
                    if entry.action_type == "send" {
                        let subject =
                            serde_json::from_str::<serde_json::Value>(&entry.payload_json)
                                .ok()
                                .and_then(|v| {
                                    v.get("subject").and_then(|s| s.as_str()).map(String::from)
                                })
                                .unwrap_or_default();
                        self.app
                            .emit(
                                "send-complete",
                                serde_json::json!({
                                    "account_id": self.account_id,
                                    "subject": subject,
                                    "via": "outbox-replay",
                                }),
                            )
                            .ok();
                    }
                }
                Err(e) => {
                    if !retry_safe {
                        let error = format!(
                            "Copy outcome may be ambiguous; automatic retry disabled to avoid duplicates: {}",
                            e
                        );
                        let conn = self.db.writer().await;
                        if let Err(mark_error) =
                            super::offline::mark_invalid(&conn, entry.id, &error)
                        {
                            log::error!(
                                "Failed to mark ambiguous copy op {} dead: {}",
                                entry.id,
                                mark_error
                            );
                            break;
                        }
                        drop(conn);
                        log::warn!(
                            "Replay of copy op {} failed; marked dead without automatic retry: {}",
                            entry.id,
                            e
                        );
                        self.app
                            .emit(
                                "offline-queue-changed",
                                serde_json::json!({
                                    "account_id": self.account_id,
                                    "dead_op_id": entry.id,
                                    "action_type": entry.action_type,
                                }),
                            )
                            .ok();
                        continue;
                    }
                    let conn = self.db.writer().await;
                    let _ = super::offline::mark_failed(&conn, entry.id, &e.to_string());
                    log::warn!(
                        "Replay of offline op {} ({}) failed (attempt {}): {}",
                        entry.id,
                        entry.action_type,
                        entry.retry_count + 1,
                        e
                    );
                    // For send ops we keep going: a retryable transient
                    // (timeout, transient SMTP 4xx, etc.) on one message
                    // shouldn't stall the other replays in this drain.
                    // For other ops a failure usually indicates the
                    // connection is broken, so break as before.
                    if entry.action_type != "send" {
                        break;
                    }
                }
            }
        }
    }

    /// Execute a single operation, dispatching sync ops to the backend
    /// and everything else to the executor. The match is deliberately
    /// exhaustive (no wildcard): adding a `MailOp` variant forces a
    /// routing decision here at compile time.
    async fn execute(&mut self, op: MailOp) -> Result<()> {
        match op {
            MailOp::SyncAll { current_folder } => self.sync_all(current_folder).await,
            MailOp::SyncFolder { folder_path } => self.sync_folder(folder_path).await,
            MailOp::ReplayOffline => Ok(()),
            op @ (MailOp::MoveMessages { .. }
            | MailOp::DeleteMessages { .. }
            | MailOp::SetFlags { .. }
            | MailOp::CopyMessages { .. }
            | MailOp::SendRaw { .. }) => self.execute_user_op(op).await,
        }
    }

    /// Run a user op through the executor; on failure, queue it to the
    /// offline outbox for replay after the next successful sync.
    async fn execute_user_op(&mut self, op: MailOp) -> Result<()> {
        // Serialize the op for outbox before executing (we move op into the executor)
        let outbox_data = super::offline::mail_op_to_outbox(&op).map(|(t, p)| (t.to_string(), p));
        let retry_safe = op.can_retry_after_execution_failure();

        let preflight = match &op {
            MailOp::SetFlags { mutations } => {
                let conn = self.db.writer().await;
                super::offline::supersede_pending_flag_ops(&conn, &self.account_id, mutations)
            }
            MailOp::DeleteMessages { message_refs } => {
                let conn = self.db.writer().await;
                super::offline::supersede_pending_flags_for_delete(
                    &conn,
                    &self.account_id,
                    message_refs,
                )
            }
            _ => Ok(()),
        };
        let result = match preflight {
            Ok(()) => self.execute_op(op).await,
            Err(error) => Err(error),
        };

        // On failure of user operations, queue to outbox for later replay
        if let Err(ref e) = result {
            if let Some((action_type, payload)) = outbox_data {
                let conn = self.db.writer().await;
                let error = if retry_safe {
                    format!("{} (will retry)", e)
                } else {
                    format!(
                        "Copy outcome may be ambiguous; automatic retry disabled to avoid duplicates: {}",
                        e
                    )
                };
                let queued = if retry_safe {
                    super::offline::queue_offline_op(
                        &conn,
                        &self.account_id,
                        &action_type,
                        &payload,
                    )
                } else {
                    super::offline::queue_dead_op(
                        &conn,
                        &self.account_id,
                        &action_type,
                        &payload,
                        &error,
                    )
                };
                match queued {
                    Ok(id) => {
                        if retry_safe {
                            log::info!(
                                "Queued failed {} op to outbox (id={}) for account {}: {}",
                                action_type,
                                id,
                                self.account_id,
                                e
                            );
                        } else {
                            log::warn!(
                                "Stored ambiguous {} op as dead (id={}) for account {}: {}",
                                action_type,
                                id,
                                self.account_id,
                                e
                            );
                            self.app
                                .emit(
                                    "offline-queue-changed",
                                    serde_json::json!({
                                        "account_id": self.account_id,
                                        "dead_op_id": id,
                                        "action_type": action_type,
                                    }),
                                )
                                .ok();
                        }
                        emit_op_failed(&self.app, &self.account_id, &action_type, &error);
                    }
                    Err(db_err) => {
                        log::error!(
                            "Failed to queue offline op for account {}: {}",
                            self.account_id,
                            db_err
                        );
                        emit_op_failed(&self.app, &self.account_id, &action_type, &e.to_string());
                    }
                }
            }
        }

        result
    }

    /// The account row and resolved backend for a sync op — `None`
    /// when the account has no mail backend. Sync creates its own
    /// connections (including parallel ones for IMAP); it never uses
    /// the executor's.
    fn sync_target(
        &self,
    ) -> Result<Option<(&'static dyn MailBackend, crate::account::MailAccountConfig)>> {
        let account = {
            let conn = self.db.reader();
            crate::db::accounts::get_account_full(&conn, &self.account_id)?
        };
        Ok(self.backend.map(|b| (b, account.mail_config())))
    }

    /// Delegate a full account sync to the backend. Deliberate
    /// carve-out from the trait's command-path semantics: Graph account
    /// syncs are owned by `sync_cmd`, so a queued SyncAll is a no-op.
    async fn sync_all(&mut self, current_folder: Option<String>) -> Result<()> {
        let Some((backend, account)) = self.sync_target()? else {
            return Ok(());
        };
        if backend.protocol() == "graph" {
            // Graph sync handled by sync_cmd directly
            return Ok(());
        }
        let ctx = self.ctx.as_ref().expect("worker initialized");
        backend.sync_account(ctx, &account, current_folder).await
    }

    /// Sync a single folder. Deliberate carve-outs from the trait's
    /// command-path semantics: queued IMAP folder syncs are "quiet" —
    /// no `sync-started` spinner, but folders/messages-changed — unlike
    /// the command's `sync_folder`, which drives the UI spinner; and
    /// Graph has no per-folder fetch at all.
    async fn sync_folder(&mut self, folder_path: String) -> Result<()> {
        let Some((backend, account)) = self.sync_target()? else {
            return Ok(());
        };
        let ctx = self.ctx.as_ref().expect("worker initialized");
        match backend.protocol() {
            "jmap" => {
                backend.sync_folder(ctx, &account, &folder_path).await?;
            }
            "imap" => {
                crate::backend::mail::imap::sync_folder_quiet(ctx, &account, &folder_path).await?;
            }
            _ => {}
        }
        Ok(())
    }
}

/// Emit an `op-failed` event to the frontend.
pub(crate) fn emit_op_failed(app: &AppHandle, account_id: &str, op_type: &str, error: &str) {
    app.emit(
        "op-failed",
        serde_json::json!({
            "account_id": account_id,
            "op_type": op_type,
            "error": error,
        }),
    )
    .ok();
}
