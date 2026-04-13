use std::sync::Arc;
use std::time::Instant;

use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

use crate::db::pool::DbPool;
use crate::error::{Error, Result};
use crate::mail::imap::{ImapConfig, ImapConnection};

use super::coalesce::coalesce;
use super::queue::{MailOp, OpEntry};

/// Per-account worker that processes mail operations on a persistent connection.
///
/// Each enabled account gets one worker. The worker:
/// - Maintains a persistent IMAP connection (reused across operations)
/// - Drains and coalesces pending operations on each iteration
/// - Prioritises user ops (move/delete/flag) over background sync
/// - Reconnects automatically if the connection goes stale
///
/// For JMAP and Graph accounts the worker delegates to async operations
/// directly (no persistent connection needed — they use HTTP).
/// Wrapper around ImapConnection + selected folder state.
/// Stored separately so it can be moved into `spawn_blocking` without
/// requiring the whole `AccountWorker` to be `Send + Sync`.
struct ImapState {
    conn: ImapConnection,
    selected_folder: Option<String>,
}

// ImapConnection contains a `Receiver<UnsolicitedResponse>` which is not Sync,
// but we only ever access it from one thread at a time (via spawn_blocking).
// SAFETY: The ImapState is always moved into spawn_blocking (single-threaded access).
unsafe impl Send for ImapState {}

pub struct AccountWorker {
    pub account_id: String,
    rx: mpsc::Receiver<OpEntry>,
    db: Arc<DbPool>,
    app: AppHandle,
    /// Persistent IMAP connection state, if this is an IMAP account.
    imap_state: Option<ImapState>,
    imap_config: Option<ImapConfig>,
    last_used: Instant,
    /// Mail protocol for this account ("imap", "jmap", "graph").
    protocol: String,
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
            imap_state: None,
            imap_config: None,
            last_used: Instant::now(),
            protocol: String::new(),
        }
    }

    /// Main loop — runs until the channel is closed.
    pub async fn run(mut self) {
        log::info!("Worker started for account {}", self.account_id);

        // Look up protocol on first run
        if let Err(e) = self.init_protocol().await {
            log::error!(
                "Worker for account {} failed to init: {}",
                self.account_id,
                e
            );
            return;
        }

        while let Some(first) = self.rx.recv().await {
            // Drain all pending ops and coalesce
            let mut batch = vec![first];
            while let Ok(next) = self.rx.try_recv() {
                batch.push(next);
            }
            let ops = coalesce(batch);

            for entry in ops {
                if let Err(e) = self.execute(entry.op).await {
                    log::error!(
                        "Worker op failed for account {}: {}",
                        self.account_id,
                        e
                    );
                    // Don't break the loop — continue processing remaining ops
                }
            }
        }

        // Channel closed — clean up
        if let Some(state) = self.imap_state.take() {
            state.conn.logout();
        }
        log::info!("Worker stopped for account {}", self.account_id);
    }

    async fn init_protocol(&mut self) -> Result<()> {
        let conn = self.db.reader();
        let account = crate::db::accounts::get_account_full(&conn, &self.account_id)?;
        self.protocol = account.mail_protocol.clone();
        Ok(())
    }

    /// Execute a single operation, dispatching by protocol.
    async fn execute(&mut self, op: MailOp) -> Result<()> {
        match &op {
            MailOp::SyncAll { .. } | MailOp::SyncFolder { .. } => {
                self.execute_sync(op).await
            }
            _ => match self.protocol.as_str() {
                "imap" => self.execute_imap(op).await,
                "jmap" => self.execute_jmap(op).await,
                "graph" => self.execute_graph(op).await,
                _ => {
                    log::warn!("Worker: unknown protocol '{}' for account {}", self.protocol, self.account_id);
                    Ok(())
                }
            },
        }
    }

    /// Delegate sync to the existing sync engine.
    /// Sync creates its own connections (including parallel ones for IMAP).
    async fn execute_sync(&mut self, op: MailOp) -> Result<()> {
        let account = {
            let conn = self.db.reader();
            crate::db::accounts::get_account_full(&conn, &self.account_id)?
        };

        match op {
            MailOp::SyncAll { current_folder } => {
                if account.mail_protocol == "graph" {
                    // Graph sync handled by sync_cmd directly
                    return Ok(());
                } else if account.mail_protocol == "jmap" {
                    let jmap_config =
                        crate::commands::sync_cmd::build_jmap_config(&account).await?;
                    crate::mail::jmap_sync::sync_jmap_account(
                        self.app.clone(),
                        self.db.clone(),
                        std::path::PathBuf::new(), // unused for JMAP
                        self.account_id.clone(),
                        account.display_name.clone(),
                        jmap_config,
                        current_folder,
                    )
                    .await?;
                } else {
                    let imap_config = self.build_imap_config(&account).await?;
                    let data_dir = {
                        let state_data_dir = self.app.state::<crate::state::AppState>();
                        state_data_dir.data_dir.clone()
                    };
                    crate::mail::sync::sync_account(
                        self.app.clone(),
                        self.db.clone(),
                        data_dir,
                        self.account_id.clone(),
                        account.display_name.clone(),
                        imap_config,
                        current_folder,
                    )
                    .await?;
                }
            }
            MailOp::SyncFolder { folder_path } => {
                if account.mail_protocol == "jmap" {
                    let jmap_config =
                        crate::commands::sync_cmd::build_jmap_config(&account).await?;
                    crate::mail::jmap_sync::sync_jmap_folder_public(
                        self.app.clone(),
                        self.db.clone(),
                        self.account_id.clone(),
                        account.display_name.clone(),
                        folder_path,
                        jmap_config,
                    )
                    .await?;
                } else if account.mail_protocol == "imap" {
                    let imap_config = self.build_imap_config(&account).await?;
                    let db = self.db.clone();
                    let account_id = self.account_id.clone();
                    let app = self.app.clone();
                    tokio::task::spawn_blocking(move || {
                        let mut conn = ImapConnection::connect(&imap_config)?;
                        crate::mail::sync::sync_folder_envelopes_public(
                            &db,
                            &account_id,
                            &mut conn,
                            &folder_path,
                        )?;
                        conn.logout();
                        crate::commands::events::emit_folders_changed(&app, &account_id);
                        crate::commands::events::emit_messages_changed(&app, &account_id);
                        Ok::<_, Error>(())
                    })
                    .await
                    .map_err(|e| Error::Sync(format!("Sync folder panicked: {}", e)))??;
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    // --- IMAP operations on persistent connection ---

    async fn execute_imap(&mut self, op: MailOp) -> Result<()> {
        // Ensure we have a live connection
        self.ensure_imap_connection().await?;

        // Move the ImapState into spawn_blocking (ImapConnection is !Sync)
        let mut imap_state = self.imap_state.take().unwrap();

        let (result, state_back) = tokio::task::spawn_blocking(move || {
            let result = execute_imap_op(
                &mut imap_state.conn,
                &mut imap_state.selected_folder,
                op,
            );
            (result, imap_state)
        })
        .await
        .map_err(|e| Error::Other(format!("IMAP op task panicked: {}", e)))?;

        if result.is_ok() {
            self.imap_state = Some(state_back);
            self.last_used = Instant::now();
        } else {
            // Connection is likely dead — drop it so next op reconnects
            log::warn!("IMAP op failed, dropping connection for reconnect");
            state_back.conn.logout();
        }

        result
    }

    /// Ensure the persistent IMAP connection is alive.
    /// Reconnects if the connection is stale (>5 min) or missing.
    async fn ensure_imap_connection(&mut self) -> Result<()> {
        let stale = self.last_used.elapsed() > std::time::Duration::from_secs(5 * 60);

        if self.imap_state.is_none() || stale {
            // Drop old connection if stale
            if let Some(state) = self.imap_state.take() {
                let _ = tokio::task::spawn_blocking(move || state.conn.logout()).await;
            }

            let account = {
                let conn = self.db.reader();
                crate::db::accounts::get_account_full(&conn, &self.account_id)?
            };
            let config = self.build_imap_config(&account).await?;
            self.imap_config = Some(config.clone());

            let conn = tokio::task::spawn_blocking(move || ImapConnection::connect(&config))
                .await
                .map_err(|e| Error::Other(format!("IMAP connect task panicked: {}", e)))??;

            self.imap_state = Some(ImapState {
                conn,
                selected_folder: None,
            });
            self.last_used = Instant::now();
            log::info!(
                "Worker: IMAP connection established for account {}",
                self.account_id
            );
        }

        Ok(())
    }

    async fn build_imap_config(
        &mut self,
        account: &crate::db::accounts::AccountFull,
    ) -> Result<ImapConfig> {
        let (password, use_xoauth2) = if account.provider == "o365" {
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
        Ok(ImapConfig {
            host: account.imap_host.clone(),
            port: account.imap_port,
            username: account.username.clone(),
            password,
            use_tls: account.use_tls,
            use_xoauth2,
        })
    }

    // --- JMAP operations (async HTTP, no persistent connection needed) ---

    async fn execute_jmap(&mut self, op: MailOp) -> Result<()> {
        let account = {
            let conn = self.db.reader();
            crate::db::accounts::get_account_full(&conn, &self.account_id)?
        };
        let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;
        let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;

        match op {
            MailOp::MoveMessages {
                by_folder,
                target_folder,
            } => {
                for (source_mailbox, uids) in &by_folder {
                    // UIDs are actually JMAP email IDs stored as u32 — extract from message IDs
                    // For JMAP, `by_folder` won't have actual UIDs, so this path isn't used.
                    // JMAP moves are handled differently (by JMAP email ID, not UID).
                    let _ = (source_mailbox, uids, &target_folder);
                }
                log::debug!("JMAP move handled by optimistic path");
            }
            MailOp::DeleteMessages { by_folder } => {
                let _ = by_folder;
                log::debug!("JMAP delete handled by optimistic path");
            }
            MailOp::SetFlags { flags, add, .. } => {
                let _ = (conn_jmap, jmap_config, flags, add);
                log::debug!("JMAP set_flags handled by optimistic path");
            }
            _ => {}
        }
        Ok(())
    }

    // --- Graph operations (async HTTP) ---

    async fn execute_graph(&mut self, op: MailOp) -> Result<()> {
        match op {
            MailOp::MoveMessages { .. }
            | MailOp::DeleteMessages { .. }
            | MailOp::SetFlags { .. } => {
                log::debug!("Graph op handled by optimistic path");
            }
            _ => {}
        }
        Ok(())
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
            by_folder,
            target_folder,
        } => {
            for (folder_path, uids) in &by_folder {
                select_folder_if_needed(conn, selected, folder_path)?;
                conn.move_messages(uids, &target_folder)?;
            }
        }
        MailOp::DeleteMessages { by_folder } => {
            for (folder_path, uids) in &by_folder {
                select_folder_if_needed(conn, selected, folder_path)?;
                conn.delete_messages(uids)?;
            }
        }
        MailOp::SetFlags {
            by_folder,
            flags,
            add,
        } => {
            let flag_strs: Vec<&str> = flags.iter().map(|s| s.as_str()).collect();
            for (folder_path, uids) in &by_folder {
                select_folder_if_needed(conn, selected, folder_path)?;
                conn.set_flags(uids, &flag_strs, add)?;
            }
        }
        MailOp::CopyMessages {
            by_folder,
            target_folder,
        } => {
            for (folder_path, uids) in &by_folder {
                select_folder_if_needed(conn, selected, folder_path)?;
                conn.copy_messages(uids, &target_folder)?;
            }
        }
        _ => {}
    }
    Ok(())
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
