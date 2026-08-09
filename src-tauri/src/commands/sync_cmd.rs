use futures::FutureExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, State};
use tokio_util::sync::CancellationToken;

use crate::event::tauri::{emit_folders_changed, emit_messages_changed};
use crate::ops::queue::{MailOp, OpEntry, OpPriority};

/// RAII guard that clears the sync-in-progress flag on drop.
pub(crate) struct SyncGuard(Arc<AtomicBool>);
impl Drop for SyncGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Try to acquire a per-account in-progress flag from the given map. Returns
/// `None` if another caller already holds it; the returned guard releases
/// the flag on drop.
///
/// Shared by mail sync (`AppState.sync_in_progress`) and calendar sync
/// (`AppState.calendar_sync_in_progress`) so both domains use the same
/// serialization pattern without blocking each other.
pub(crate) fn try_acquire_sync_guard(
    flags: &Mutex<HashMap<String, Arc<AtomicBool>>>,
    account_id: &str,
    operation: &str,
) -> Option<SyncGuard> {
    let flag = {
        let mut map = flags.lock().unwrap();
        map.entry(account_id.to_string())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    };

    if flag.swap(true, Ordering::AcqRel) {
        log::debug!(
            "{} already in progress for account {}, skipping",
            operation,
            account_id
        );
        return None;
    }

    Some(SyncGuard(flag))
}

use crate::db;
use crate::error::{Error, Result};
use crate::mail::imap::ImapConfig;
use crate::state::{AppState, IdleControl, IdleHandle, IdlePhase, JmapPushHandle, JmapPushPhase};

const IDLE_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const IDLE_STOP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

fn should_start_imap_idle(_auth_method: &str) -> bool {
    true
}

/// O365 IMAP needs IDLE suspended around any other operation because
/// Microsoft's server only allows one connection per account at a time.
/// Identifying O365 via auth_method is more accurate than the legacy
/// `provider` string since Phase 3 dropped that column.
pub(crate) fn should_suspend_idle_for_imap_operation(auth_method: &str) -> bool {
    auth_method == "oauth-microsoft"
}

pub(crate) struct ImapIdleSuspension {
    _account_guard: tokio::sync::OwnedMutexGuard<()>,
}

fn imap_idle_account_lock(state: &AppState, account_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    state
        .idle_account_locks
        .lock()
        .unwrap()
        .entry(account_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

pub(crate) async fn suspend_imap_idle_for_account(
    state: &State<'_, AppState>,
    account_id: &str,
) -> Result<ImapIdleSuspension> {
    let account_guard = imap_idle_account_lock(state, account_id).lock_owned().await;
    if let Some(generation) = request_imap_idle_stop(state, account_id) {
        wait_for_imap_idle_stop(state, account_id, generation, None).await?;
    }
    Ok(ImapIdleSuspension {
        _account_guard: account_guard,
    })
}

fn request_imap_idle_stop(state: &AppState, account_id: &str) -> Option<u64> {
    let (generation, control) = {
        let mut handles = state.idle_handles.lock().unwrap();
        let handle = handles.get_mut(account_id)?;
        if !matches!(handle.phase, IdlePhase::Joining | IdlePhase::StopFailed) {
            handle.phase = IdlePhase::Stopping;
        }
        (handle.generation, handle.control.clone())
    };
    control.request_stop();
    Some(generation)
}

async fn wait_for_imap_idle_stop(
    state: &AppState,
    account_id: &str,
    generation: u64,
    deadline: Option<tokio::time::Instant>,
) -> Result<()> {
    loop {
        let finished_thread = {
            let mut handles = state.idle_handles.lock().unwrap();
            match handles.get_mut(account_id) {
                None => return Ok(()),
                Some(handle) if handle.generation != generation => {
                    return Err(Error::Sync(format!(
                        "IDLE generation changed while stopping account {}",
                        account_id
                    )));
                }
                Some(handle) if handle.phase == IdlePhase::StopFailed => {
                    return Err(Error::Sync(format!(
                        "IDLE loop for account {} panicked during stop",
                        account_id
                    )));
                }
                Some(handle) => {
                    let finished = handle
                        .thread
                        .as_ref()
                        .is_some_and(std::thread::JoinHandle::is_finished);
                    if finished {
                        handle.phase = IdlePhase::Joining;
                        handle.thread.take()
                    } else {
                        None
                    }
                }
            }
        };

        if let Some(thread) = finished_thread {
            let join_succeeded = thread.join().is_ok();
            let mut handles = state.idle_handles.lock().unwrap();
            if let Some(handle) = handles
                .get_mut(account_id)
                .filter(|handle| handle.generation == generation)
            {
                if join_succeeded {
                    handles.remove(account_id);
                } else {
                    handle.phase = IdlePhase::StopFailed;
                }
            }
            return if join_succeeded {
                Ok(())
            } else {
                Err(Error::Sync(format!(
                    "IDLE loop for account {} panicked during stop",
                    account_id
                )))
            };
        }

        if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            return Err(Error::Sync(format!(
                "IDLE loop for account {} did not stop within {}s; restart is blocked",
                account_id,
                IDLE_STOP_TIMEOUT.as_secs()
            )));
        }
        tokio::time::sleep(IDLE_STOP_POLL_INTERVAL).await;
    }
}

fn finish_jmap_push_join(
    state: &AppState,
    account_id: &str,
    generation: u64,
    task: tokio::task::JoinHandle<()>,
) -> Result<()> {
    let join_result = task
        .now_or_never()
        .expect("a JMAP task taken for joining must already be finished");
    let join_succeeded =
        join_result.is_ok() || join_result.is_err_and(|error| error.is_cancelled());
    let mut handles = state.jmap_push_handles.lock().unwrap();
    if let Some(handle) = handles
        .get_mut(account_id)
        .filter(|handle| handle.generation == generation)
    {
        if join_succeeded {
            handles.remove(account_id);
        } else {
            handle.phase = JmapPushPhase::StopFailed;
        }
    }
    if join_succeeded {
        Ok(())
    } else {
        Err(Error::Sync(format!(
            "JMAP push task for account {} panicked",
            account_id
        )))
    }
}

struct JmapPushReservation<'a> {
    state: &'a AppState,
    account_id: String,
    generation: u64,
    cancellation: CancellationToken,
    event_gate: Arc<Mutex<()>>,
    committed: bool,
}

impl JmapPushReservation<'_> {
    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for JmapPushReservation<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut handles = self.state.jmap_push_handles.lock().unwrap();
        if handles
            .get(&self.account_id)
            .is_some_and(|handle| handle.generation == self.generation && handle.task.is_none())
        {
            handles.remove(&self.account_id);
        }
    }
}

fn cancel_jmap_push(cancellation: &CancellationToken, event_gate: &Mutex<()>) {
    let _event_guard = event_gate.lock().unwrap();
    cancellation.cancel();
}

fn if_jmap_push_running(
    cancellation: &CancellationToken,
    event_gate: &Mutex<()>,
    action: impl FnOnce(),
) {
    let _event_guard = event_gate.lock().unwrap();
    if !cancellation.is_cancelled() {
        action();
    }
}

async fn wait_for_jmap_push_stop(
    state: &AppState,
    account_id: &str,
    generation: u64,
    mut deadline: Option<tokio::time::Instant>,
) -> Result<()> {
    loop {
        let finished_task = {
            let mut handles = state.jmap_push_handles.lock().unwrap();
            match handles.get_mut(account_id) {
                None => return Ok(()),
                Some(handle) if handle.generation > generation => return Ok(()),
                Some(handle) if handle.generation != generation => {
                    return Err(Error::Sync(format!(
                        "JMAP push generation moved backwards while stopping account {}",
                        account_id
                    )));
                }
                Some(handle) if handle.phase == JmapPushPhase::StopFailed => {
                    return Err(Error::Sync(format!(
                        "JMAP push task for account {} panicked during stop",
                        account_id
                    )));
                }
                Some(handle)
                    if handle.phase == JmapPushPhase::Stopping && handle.task.is_none() =>
                {
                    handles.remove(account_id);
                    return Ok(());
                }
                Some(handle) => {
                    let finished = handle
                        .task
                        .as_ref()
                        .is_some_and(tokio::task::JoinHandle::is_finished);
                    if finished {
                        handle.phase = JmapPushPhase::Joining;
                        handle.task.take()
                    } else {
                        if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
                        {
                            log::warn!(
                                "JMAP push task for account {} did not stop gracefully; aborting",
                                account_id
                            );
                            if let Some(task) = handle.task.as_ref() {
                                task.abort();
                            }
                            deadline = None;
                        }
                        None
                    }
                }
            }
        };

        if let Some(task) = finished_task {
            return finish_jmap_push_join(state, account_id, generation, task);
        }
        tokio::time::sleep(IDLE_STOP_POLL_INTERVAL).await;
    }
}

async fn reserve_jmap_push_start<'a>(
    state: &'a AppState,
    account_id: &str,
) -> Result<Option<JmapPushReservation<'a>>> {
    loop {
        let stopping_generation = {
            let _lifecycle_guard = state.idle_lifecycle_lock.lock().await;
            if !state.idle_push_enabled.load(Ordering::Acquire) {
                return Ok(None);
            }

            let mut handles = state.jmap_push_handles.lock().unwrap();
            let finished_task = handles
                .get_mut(account_id)
                .filter(|handle| {
                    handle
                        .task
                        .as_ref()
                        .is_some_and(tokio::task::JoinHandle::is_finished)
                })
                .map(|handle| {
                    handle.phase = JmapPushPhase::Joining;
                    (handle.generation, handle.task.take().unwrap())
                });
            if let Some((generation, task)) = finished_task {
                drop(handles);
                finish_jmap_push_join(state, account_id, generation, task)?;
                None
            } else if let Some(handle) = handles.get(account_id) {
                match handle.phase {
                    JmapPushPhase::Starting | JmapPushPhase::Running => return Ok(None),
                    JmapPushPhase::Stopping | JmapPushPhase::Joining => Some(handle.generation),
                    JmapPushPhase::StopFailed => {
                        handles.remove(account_id);
                        None
                    }
                }
            } else {
                let generation = state.jmap_push_generation.fetch_add(1, Ordering::Relaxed);
                let cancellation = CancellationToken::new();
                let event_gate = Arc::new(Mutex::new(()));
                handles.insert(
                    account_id.to_string(),
                    JmapPushHandle {
                        generation,
                        phase: JmapPushPhase::Starting,
                        cancellation: cancellation.clone(),
                        event_gate: event_gate.clone(),
                        task: None,
                    },
                );
                return Ok(Some(JmapPushReservation {
                    state,
                    account_id: account_id.to_string(),
                    generation,
                    cancellation,
                    event_gate,
                    committed: false,
                }));
            }
        };

        if let Some(generation) = stopping_generation {
            wait_for_jmap_push_stop(state, account_id, generation, None).await?;
        }
    }
}

pub(crate) async fn resume_imap_idle_for_account(
    app: &AppHandle,
    state: &State<'_, AppState>,
    account: &db::accounts::AccountFull,
    suspension: Option<ImapIdleSuspension>,
) -> Result<()> {
    let Some(_suspension) = suspension else {
        return Ok(());
    };
    if !should_start_imap_idle(&account.auth_method) {
        return Ok(());
    }

    let account_summary = db::accounts::Account {
        id: account.id.clone(),
        display_name: account.display_name.clone(),
        email: account.email.clone(),
        username: account.username.clone(),
        provider: account.provider.clone(),
        mail_protocol: account.mail_protocol.clone(),
        enabled: account.enabled,
        mail_sync_interval_seconds: account.mail_sync_interval_seconds,
        calendar_sync_interval_seconds: account.calendar_sync_interval_seconds,
        contacts_sync_interval_seconds: account.contacts_sync_interval_seconds,
        has_calendar_binding: account.calendar_binding().is_some(),
        has_contacts_binding: account.contacts_binding().is_some(),
        meet_protocol: account.meet_protocol_str().to_string(),
    };

    start_imap_idle_inner(app, state, &account_summary).await
}

pub(crate) async fn restart_imap_idle_after_failed_suspend(
    app: &AppHandle,
    state: &State<'_, AppState>,
    account: &db::accounts::AccountFull,
) -> Result<()> {
    let _account_guard = imap_idle_account_lock(state, &account.id)
        .lock_owned()
        .await;
    let account_summary = db::accounts::Account {
        id: account.id.clone(),
        display_name: account.display_name.clone(),
        email: account.email.clone(),
        username: account.username.clone(),
        provider: account.provider.clone(),
        mail_protocol: account.mail_protocol.clone(),
        enabled: account.enabled,
        mail_sync_interval_seconds: account.mail_sync_interval_seconds,
        calendar_sync_interval_seconds: account.calendar_sync_interval_seconds,
        contacts_sync_interval_seconds: account.contacts_sync_interval_seconds,
        has_calendar_binding: account.calendar_binding().is_some(),
        has_contacts_binding: account.contacts_binding().is_some(),
        meet_protocol: account.meet_protocol_str().to_string(),
    };
    start_imap_idle_inner(app, state, &account_summary).await
}

pub(crate) async fn suspend_imap_idle_for_operation(
    app: &AppHandle,
    state: &State<'_, AppState>,
    account: &db::accounts::AccountFull,
) -> Result<ImapIdleSuspension> {
    suspend_imap_idle_with_recovery(
        &account.id,
        suspend_imap_idle_for_account(state, &account.id),
        restart_imap_idle_after_failed_suspend(app, state, account),
    )
    .await
}

async fn suspend_imap_idle_with_recovery<T, S, R>(
    account_id: &str,
    suspend: S,
    restart: R,
) -> Result<T>
where
    S: std::future::Future<Output = Result<T>>,
    R: std::future::Future<Output = Result<()>>,
{
    match suspend.await {
        Ok(suspension) => Ok(suspension),
        Err(suspend_error) => {
            if let Err(restart_error) = restart.await {
                log::error!(
                    "Failed to restore IMAP IDLE after suspension error for account {}: {}",
                    account_id,
                    restart_error
                );
            }
            Err(suspend_error)
        }
    }
}

fn try_acquire_account_sync_guard(
    state: &State<'_, AppState>,
    account_id: &str,
    operation: &str,
) -> Option<SyncGuard> {
    try_acquire_sync_guard(&state.sync_in_progress, account_id, operation)
}

fn record_deferred_mail_sync(
    pending: &Mutex<HashMap<String, Option<String>>>,
    account_id: &str,
    current_folder: Option<String>,
) {
    let mut pending = pending.lock().unwrap();
    pending
        .entry(account_id.to_string())
        .and_modify(|folder| {
            if current_folder.is_none() {
                *folder = None;
            } else if folder.is_some() {
                *folder = current_folder.clone();
            }
        })
        .or_insert(current_folder);
}

fn defer_mail_sync(state: &State<'_, AppState>, account_id: &str, current_folder: Option<String>) {
    record_deferred_mail_sync(&state.pending_mail_sync, account_id, current_folder);
}

fn take_deferred_mail_sync(
    state: &State<'_, AppState>,
    account_id: &str,
) -> Option<Option<String>> {
    state.pending_mail_sync.lock().unwrap().remove(account_id)
}

async fn run_mail_sync_once(
    app: &AppHandle,
    state: &State<'_, AppState>,
    account_id: &str,
    current_folder: Option<String>,
) -> Result<()> {
    log::info!("Sync requested for account {}", account_id);
    let account_result = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, account_id)
    };
    let account = match account_result {
        Ok(a) => a,
        Err(e) => {
            app.emit(
                "sync-error",
                serde_json::json!({"account_id": account_id, "error": e.to_string()}),
            )
            .ok();
            return Err(e);
        }
    };

    // Calendar-only / contacts-only / mail-disabled accounts have no
    // mail binding, so there is nothing to dispatch here. Return early
    // before the IMAP fallback at the bottom of the protocol chain
    // tries to connect with an empty hostname.
    if account.mail_protocol_str().is_empty() {
        log::debug!(
            "trigger_sync: account {} has no enabled mail binding, skipping",
            account_id
        );
        return Ok(());
    }

    // Non-empty protocol always resolves (unknown falls back to IMAP,
    // matching the pre-trait else-is-IMAP chain).
    let backend = crate::backend::mail::for_account(&account)
        .expect("non-empty mail protocol resolves to a backend");

    let suspended_idle = if backend.suspends_idle_for_ops(&account) {
        log::info!(
            "Suspending IMAP IDLE for account {} before sync",
            account_id
        );
        Some(suspend_imap_idle_for_operation(app, state, &account).await?)
    } else {
        None
    };
    let resume_account = account.clone();

    let ctx = crate::backend::mail::MailSyncCtx {
        events: crate::event::tauri::shared_sink(app.clone()),
        db: state.db.clone(),
        data_dir: state.data_dir.clone(),
        providers: state.providers.clone(),
    };
    // Calendar sync is independent — triggered by its own interval,
    // not chained to mail sync. See CalendarView.vue / calendar.ts.
    let sync_result = backend.sync_account(&ctx, &account, current_folder).await;

    let resume_result =
        resume_imap_idle_for_account(app, state, &resume_account, suspended_idle).await;
    if let Err(e) = &sync_result {
        app.emit(
            "sync-error",
            serde_json::json!({"account_id": account_id, "error": e.to_string()}),
        )
        .ok();
    }
    resume_result?;

    if sync_result.is_ok() {
        let sender = state.get_op_sender(account_id, app).await;
        if let Err(error) = sender
            .send(OpEntry {
                op: MailOp::ReplayOffline,
                priority: OpPriority::Sync,
            })
            .await
        {
            log::warn!(
                "Failed to request offline replay for account {}: {}",
                account_id,
                error
            );
        }
    }

    sync_result
}

async fn run_deferred_mail_syncs(
    app: &AppHandle,
    state: &State<'_, AppState>,
    account_id: &str,
    first_folder: Option<String>,
) -> Result<()> {
    let mut current_folder = first_folder;
    let mut first_error: Option<Error> = None;

    loop {
        let Some(guard) = try_acquire_account_sync_guard(state, account_id, "Deferred sync") else {
            defer_mail_sync(state, account_id, current_folder);
            return match first_error {
                Some(error) => Err(error),
                None => Ok(()),
            };
        };

        log::info!("Running deferred sync for account {}", account_id);
        let sync_result = run_mail_sync_once(app, state, account_id, current_folder).await;
        drop(guard);
        let next = take_deferred_mail_sync(state, account_id);

        if let Err(error) = sync_result {
            first_error.get_or_insert(error);
        }

        let Some(next_folder) = next else {
            return match first_error {
                Some(error) => Err(error),
                None => Ok(()),
            };
        };
        current_folder = next_folder;
    }
}

#[tauri::command]
pub async fn trigger_sync(
    app: AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    current_folder: Option<String>,
) -> Result<()> {
    let Some(guard) = try_acquire_account_sync_guard(&state, &account_id, "Sync") else {
        defer_mail_sync(&state, &account_id, current_folder);
        return Ok(());
    };

    let sync_result = run_mail_sync_once(&app, &state, &account_id, current_folder).await;
    drop(guard);
    let deferred = take_deferred_mail_sync(&state, &account_id);

    let deferred_result = match deferred {
        Some(deferred_folder) => {
            run_deferred_mail_syncs(&app, &state, &account_id, deferred_folder).await
        }
        None => Ok(()),
    };

    sync_result.and(deferred_result)
}

#[tauri::command]
pub async fn sync_folder(
    app: AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    folder_path: String,
) -> Result<u32> {
    let Some(_guard) = try_acquire_account_sync_guard(&state, &account_id, "Folder sync") else {
        return Ok(0);
    };

    log::info!(
        "Single folder sync: account={} folder={}",
        account_id,
        folder_path
    );
    let account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    // Per-folder sync only makes sense for mail accounts. If there's no
    // enabled mail binding (DAV-only / mail-disabled JMAP) just return
    // a zero new-message count.
    if account.mail_protocol_str().is_empty() {
        log::debug!(
            "sync_folder: account {} has no enabled mail binding, skipping",
            account_id
        );
        return Ok(0);
    }

    // sync-started is emitted by each backend's sync path, so the
    // activity store sees exactly one start per sync.

    let backend = crate::backend::mail::for_account(&account)
        .expect("non-empty mail protocol resolves to a backend");

    let suspended_idle = if backend.suspends_idle_for_ops(&account) {
        log::info!(
            "Suspending IMAP IDLE for account {} before single-folder sync",
            account_id
        );
        Some(suspend_imap_idle_for_operation(&app, &state, &account).await?)
    } else {
        None
    };
    let resume_account = account.clone();

    let ctx = crate::backend::mail::MailSyncCtx {
        events: crate::event::tauri::shared_sink(app.clone()),
        db: state.db.clone(),
        data_dir: state.data_dir.clone(),
        providers: state.providers.clone(),
    };

    // Every backend syncs exactly the requested folder synchronously —
    // a right-click "sync folder" must never escalate to a whole-account
    // sync (Graph used to background one here before delta sync).
    let sync_result: Result<u32> = backend.sync_folder(&ctx, &account, &folder_path).await;

    let resume_result =
        resume_imap_idle_for_account(&app, &state, &resume_account, suspended_idle).await;

    match &sync_result {
        Ok(count) => {
            app.emit(
                "sync-complete",
                serde_json::json!({
                    "account_id": account_id,
                    "total_synced": count,
                }),
            )
            .ok();
            emit_folders_changed(&app, &account_id);
            emit_messages_changed(&app, &account_id);
            log::info!("Single folder sync done: {} new in {}", count, folder_path);
        }
        Err(e) => {
            app.emit(
                "sync-error",
                serde_json::json!({
                    "account_id": account_id,
                    "error": e.to_string(),
                }),
            )
            .ok();
        }
    }

    resume_result?;

    sync_result
}

#[derive(serde::Serialize)]
pub struct SyncStatus {
    pub account_id: String,
    pub is_syncing: bool,
    pub last_sync: Option<String>,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn get_sync_status(
    _state: State<'_, AppState>,
    account_id: String,
) -> Result<SyncStatus> {
    Ok(SyncStatus {
        account_id,
        is_syncing: false,
        last_sync: None,
        error: None,
    })
}

/// Prefetch message bodies in the background after sync completes.
/// Opens a single IMAP connection, groups messages by folder to minimize
/// SELECT commands, fetches each body, writes to Maildir, and updates DB.
/// Returns the number of bodies successfully fetched.
#[tauri::command]
pub async fn prefetch_bodies(
    app: AppHandle,
    state: State<'_, AppState>,
    account_id: String,
) -> Result<u32> {
    let Some(guard) = try_acquire_account_sync_guard(&state, &account_id, "Prefetch") else {
        return Ok(0);
    };

    log::info!("Prefetch bodies requested for account {}", account_id);

    let prefetch_result = async {
        let account = {
            let conn = state.db.reader();
            db::accounts::get_account_full(&conn, &account_id)?
        };

        // JMAP inherits the trait's no-op prefetch (bodies are fetched on
        // demand via the JMAP API); accounts without a mail binding have
        // nothing to prefetch.
        let Some(backend) = crate::backend::mail::for_account(&account) else {
            log::debug!(
                "Prefetch: account {} has no enabled mail binding, skipping",
                account_id
            );
            return Ok(0);
        };

        let suspended_idle = if backend.suspends_idle_for_ops(&account) {
            log::info!(
                "Suspending IMAP IDLE for account {} before body prefetch",
                account_id
            );
            Some(suspend_imap_idle_for_operation(&app, &state, &account).await?)
        } else {
            None
        };
        let resume_account = account.clone();

        // For O365: get IMAP-scoped OAuth token
        let ctx = crate::backend::mail::MailSyncCtx {
            events: crate::event::tauri::shared_sink(app.clone()),
            db: state.db.clone(),
            data_dir: state.data_dir.clone(),
            providers: state.providers.clone(),
        };
        let prefetch_result = backend.prefetch_bodies(&ctx, &account).await;
        let resume_result =
            resume_imap_idle_for_account(&app, &state, &resume_account, suspended_idle).await;

        match (prefetch_result, resume_result) {
            (Ok(fetched_count), Ok(())) => Ok(fetched_count),
            (Ok(_), Err(e)) => Err(e),
            (Err(prefetch_error), Ok(())) => Err(prefetch_error),
            (Err(prefetch_error), Err(resume_error)) => {
                log::error!(
                    "Failed to resume IMAP IDLE after prefetch error for account {}: {}",
                    account_id,
                    resume_error
                );
                Err(prefetch_error)
            }
        }
    }
    .await;

    drop(guard);
    let deferred = take_deferred_mail_sync(&state, &account_id);
    let deferred_result = match deferred {
        Some(deferred_folder) => {
            run_deferred_mail_syncs(&app, &state, &account_id, deferred_folder).await
        }
        None => Ok(()),
    };

    match (prefetch_result, deferred_result) {
        (Err(prefetch_error), _) => Err(prefetch_error),
        (Ok(_), Err(deferred_error)) => Err(deferred_error),
        (Ok(fetched_count), Ok(())) => Ok(fetched_count),
    }
}

/// Start IMAP IDLE and JMAP push for all enabled accounts. Call on app startup.
#[tauri::command]
pub async fn start_idle(app: AppHandle, state: State<'_, AppState>) -> Result<()> {
    {
        let _lifecycle_guard = state.idle_lifecycle_lock.lock().await;
        state.idle_push_enabled.store(true, Ordering::Release);
    }
    let accounts = {
        let conn = state.db.reader();
        db::accounts::list_accounts(&conn)?
    };

    for account in &accounts {
        if !account.enabled {
            continue;
        }

        // Account summary still carries mail_protocol directly; dispatch
        // here without going through AccountFull to avoid an extra DB hop.
        if account.mail_protocol == "imap" {
            let account_lock = imap_idle_account_lock(&state, &account.id);
            let Ok(_account_guard) = account_lock.try_lock_owned() else {
                log::debug!(
                    "Deferring IDLE start for account {} until its IMAP operation finishes",
                    account.id
                );
                continue;
            };
            start_imap_idle_inner(&app, &state, account).await?;
        } else if account.mail_protocol == "jmap" {
            start_jmap_push(&app, &state, account).await?;
        }
    }

    Ok(())
}

async fn start_imap_idle_inner(
    app: &AppHandle,
    state: &State<'_, AppState>,
    account: &db::accounts::Account,
) -> Result<()> {
    let reservation = loop {
        let (reservation, stopping_generation) = {
            let _lifecycle_guard = state.idle_lifecycle_lock.lock().await;
            if !state.idle_push_enabled.load(Ordering::Acquire) {
                return Ok(());
            }
            let stopping_generation = state
                .idle_handles
                .lock()
                .unwrap()
                .get(&account.id)
                .filter(|handle| matches!(handle.phase, IdlePhase::Stopping | IdlePhase::Joining))
                .map(|handle| handle.generation);
            if stopping_generation.is_some() {
                (None, stopping_generation)
            } else {
                (reserve_imap_idle_start(state, &account.id)?, None)
            }
        };

        if let Some(generation) = stopping_generation {
            wait_for_imap_idle_stop(state, &account.id, generation, None).await?;
            continue;
        }
        break reservation;
    };
    let Some((generation, control)) = reservation else {
        log::debug!("IDLE already active for account {}", account.id);
        return Ok(());
    };

    let full_account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account.id)
    };
    let full_account = match full_account {
        Ok(full_account) => full_account,
        Err(error) => {
            remove_imap_idle_generation(state, &account.id, generation);
            return Err(error);
        }
    };
    let credentials = match state
        .providers
        .credentials()
        .mail_credentials(&full_account)
        .await
    {
        Ok(credentials) => credentials,
        Err(error) => {
            remove_imap_idle_generation(state, &account.id, generation);
            return Err(error);
        }
    };

    let config = ImapConfig {
        host: full_account.imap_host.clone(),
        port: full_account.imap_port,
        username: full_account.username.clone(),
        password: credentials.secret,
        use_tls: full_account.use_tls,
        use_xoauth2: credentials.use_xoauth2,
    };

    let mut handles = state.idle_handles.lock().unwrap();
    let Some(handle) = handles.get_mut(&account.id) else {
        return Ok(());
    };
    if handle.generation != generation || handle.phase != IdlePhase::Starting {
        if handle.generation == generation && handle.phase == IdlePhase::Stopping {
            handles.remove(&account.id);
        }
        return Ok(());
    }

    let account_id = account.id.clone();
    let thread_control = control.clone();
    let event_control = control.clone();
    let app_clone = app.clone();
    let thread = std::thread::spawn(move || {
        crate::mail::idle::run_idle_loop(
            config,
            account_id.clone(),
            thread_control,
            Box::new(move |event| {
                event_control.if_running(|| match event {
                    crate::mail::idle::IdleEvent::NewMail(aid) => {
                        app_clone.emit("idle-new-mail", aid).ok();
                    }
                    crate::mail::idle::IdleEvent::Disconnected(aid) => {
                        app_clone.emit("idle-disconnected", aid).ok();
                    }
                    crate::mail::idle::IdleEvent::Reconnected(aid) => {
                        app_clone.emit("idle-reconnected", aid).ok();
                    }
                });
            }),
        );
    });

    handle.phase = IdlePhase::Running;
    handle.thread = Some(thread);
    log::info!("Started IDLE loop for account {}", account.id);
    Ok(())
}

fn reserve_imap_idle_start(
    state: &AppState,
    account_id: &str,
) -> Result<Option<(u64, Arc<IdleControl>)>> {
    let mut handles = state.idle_handles.lock().unwrap();
    let finished = handles
        .get(account_id)
        .and_then(|handle| handle.thread.as_ref())
        .is_some_and(std::thread::JoinHandle::is_finished);
    if finished {
        let mut finished_handle = handles.remove(account_id).unwrap();
        if let Some(thread) = finished_handle.thread.take() {
            thread.join().map_err(|_| {
                Error::Sync(format!(
                    "Previous IDLE loop for account {} panicked",
                    account_id
                ))
            })?;
        }
    }

    if let Some(handle) = handles.get(account_id) {
        return match handle.phase {
            IdlePhase::Starting | IdlePhase::Running => Ok(None),
            IdlePhase::Stopping | IdlePhase::Joining => Err(Error::Sync(format!(
                "IDLE loop for account {} is still stopping; restart is blocked",
                account_id
            ))),
            IdlePhase::StopFailed => {
                handles.remove(account_id);
                reserve_new_imap_idle_generation(state, account_id, &mut handles)
            }
        };
    }

    reserve_new_imap_idle_generation(state, account_id, &mut handles)
}

fn reserve_new_imap_idle_generation(
    state: &AppState,
    account_id: &str,
    handles: &mut HashMap<String, IdleHandle>,
) -> Result<Option<(u64, Arc<IdleControl>)>> {
    let generation = state.idle_generation.fetch_add(1, Ordering::Relaxed);
    let control = Arc::new(IdleControl::new());
    handles.insert(
        account_id.to_string(),
        IdleHandle {
            generation,
            phase: IdlePhase::Starting,
            control: control.clone(),
            thread: None,
        },
    );
    Ok(Some((generation, control)))
}

fn remove_imap_idle_generation(state: &AppState, account_id: &str, generation: u64) {
    let mut handles = state.idle_handles.lock().unwrap();
    if handles
        .get(account_id)
        .is_some_and(|handle| handle.generation == generation)
    {
        handles.remove(account_id);
    }
}

async fn start_jmap_push(
    app: &AppHandle,
    state: &State<'_, AppState>,
    account: &db::accounts::Account,
) -> Result<()> {
    let Some(reservation) = reserve_jmap_push_start(state, &account.id).await? else {
        log::debug!(
            "JMAP push already active or disabled for account {}",
            account.id
        );
        return Ok(());
    };
    let generation = reservation.generation;
    let cancellation = reservation.cancellation.clone();
    let event_gate = reservation.event_gate.clone();

    let full_account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account.id)
    };
    let full_account = full_account?;

    let jmap_config = state
        .providers
        .credentials()
        .jmap_config(&full_account)
        .await?;

    let account_id = account.id.clone();
    let task_cancellation = cancellation.clone();
    let event_cancellation = cancellation.clone();
    let callback_event_gate = event_gate.clone();
    let app_clone = app.clone();
    let providers = state.providers.clone();

    let _lifecycle_guard = state.idle_lifecycle_lock.lock().await;
    if !state.idle_push_enabled.load(Ordering::Acquire) || cancellation.is_cancelled() {
        return Ok(());
    }
    let mut handles = state.jmap_push_handles.lock().unwrap();
    let Some(handle) = handles.get_mut(&account.id) else {
        return Ok(());
    };
    if handle.generation != generation || handle.phase != JmapPushPhase::Starting {
        return Ok(());
    }

    let task = tokio::spawn(async move {
        crate::mail::jmap_push::run_push_loop(
            jmap_config,
            account_id.clone(),
            task_cancellation,
            providers,
            std::sync::Arc::new(move |event| {
                if_jmap_push_running(&event_cancellation, &callback_event_gate, || match event {
                    crate::mail::jmap_push::PushEvent::StateChange(aid) => {
                        app_clone.emit("idle-new-mail", &aid).ok();
                    }
                    crate::mail::jmap_push::PushEvent::Disconnected(aid) => {
                        app_clone.emit("idle-disconnected", &aid).ok();
                    }
                    crate::mail::jmap_push::PushEvent::Reconnected(aid) => {
                        app_clone.emit("idle-reconnected", &aid).ok();
                    }
                });
            }),
        )
        .await;
    });

    handle.phase = JmapPushPhase::Running;
    handle.task = Some(task);
    drop(handles);
    reservation.commit();
    log::info!("Started JMAP push for account {}", account.id);
    Ok(())
}

/// Stop all IMAP IDLE loops and JMAP push tasks.
#[tauri::command]
pub async fn stop_idle(state: State<'_, AppState>) -> Result<()> {
    let (idle_generations, jmap_generations) = {
        let _lifecycle_guard = state.idle_lifecycle_lock.lock().await;
        state.idle_push_enabled.store(false, Ordering::Release);
        let idle_generations = {
            let mut handles = state.idle_handles.lock().unwrap();
            let mut generations = Vec::new();
            for (account_id, handle) in handles.iter_mut() {
                log::info!("Stopping IDLE loop for account {}", account_id);
                if !matches!(handle.phase, IdlePhase::Joining | IdlePhase::StopFailed) {
                    handle.phase = IdlePhase::Stopping;
                }
                generations.push((
                    account_id.clone(),
                    handle.generation,
                    handle.control.clone(),
                ));
            }
            generations
        };
        let jmap_generations = {
            let mut jmap_handles = state.jmap_push_handles.lock().unwrap();
            let mut generations = Vec::new();
            for (account_id, handle) in jmap_handles.iter_mut() {
                log::info!("Stopping JMAP push for account {}", account_id);
                if !matches!(
                    handle.phase,
                    JmapPushPhase::Joining | JmapPushPhase::StopFailed
                ) {
                    handle.phase = JmapPushPhase::Stopping;
                }
                cancel_jmap_push(&handle.cancellation, &handle.event_gate);
                generations.push((account_id.clone(), handle.generation));
            }
            generations
        };
        (idle_generations, jmap_generations)
    };
    for (_, _, control) in &idle_generations {
        control.request_stop();
    }

    let mut stop_error: Option<Error> = None;

    let jmap_deadline = tokio::time::Instant::now() + IDLE_STOP_TIMEOUT;
    for (account_id, generation) in jmap_generations {
        if let Err(error) =
            wait_for_jmap_push_stop(&state, &account_id, generation, Some(jmap_deadline)).await
        {
            log::error!("{}", error);
            stop_error.get_or_insert(error);
        }
    }

    let idle_deadline = tokio::time::Instant::now() + IDLE_STOP_TIMEOUT;
    for (account_id, generation, _) in idle_generations {
        if let Err(error) =
            wait_for_imap_idle_stop(&state, &account_id, generation, Some(idle_deadline)).await
        {
            log::error!("{}", error);
            stop_error.get_or_insert(error);
        }
    }

    if let Some(error) = stop_error {
        return Err(error);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn o365_sync_suspends_idle_but_still_allows_idle_startup() {
        // Phase 3: these helpers now key off auth_method, not provider.
        assert!(super::should_start_imap_idle("oauth-microsoft"));
        assert!(super::should_suspend_idle_for_imap_operation(
            "oauth-microsoft"
        ));
        assert!(!super::should_suspend_idle_for_imap_operation(
            "oauth-google"
        ));
        assert!(!super::should_suspend_idle_for_imap_operation("password"));
    }

    #[test]
    fn idle_start_reservation_allows_only_one_generation() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::new(temp.path().to_path_buf()).unwrap();

        let first = reserve_imap_idle_start(&state, "account").unwrap();
        let second = reserve_imap_idle_start(&state, "account").unwrap();

        assert!(first.is_some());
        assert!(second.is_none());
        assert_eq!(
            state.idle_handles.lock().unwrap()["account"].phase,
            IdlePhase::Starting
        );
    }

    #[test]
    fn idle_start_is_blocked_while_previous_generation_stops() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::new(temp.path().to_path_buf()).unwrap();
        reserve_imap_idle_start(&state, "account").unwrap();

        request_imap_idle_stop(&state, "account").unwrap();
        let error = match reserve_imap_idle_start(&state, "account") {
            Err(error) => error,
            Ok(_) => panic!("stopping generation accepted a replacement"),
        };

        assert!(error.to_string().contains("restart is blocked"));
        assert_eq!(
            state.idle_handles.lock().unwrap()["account"].phase,
            IdlePhase::Stopping
        );
    }

    #[tokio::test]
    async fn jmap_start_reservation_allows_only_one_generation() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::new(temp.path().to_path_buf()).unwrap();
        state.idle_push_enabled.store(true, Ordering::Release);

        let first = reserve_jmap_push_start(&state, "account").await.unwrap();
        let second = reserve_jmap_push_start(&state, "account").await.unwrap();

        assert!(first.is_some());
        assert!(second.is_none());
        assert_eq!(
            state.jmap_push_handles.lock().unwrap()["account"].phase,
            JmapPushPhase::Starting
        );
    }

    #[tokio::test]
    async fn jmap_restart_waits_for_previous_generation_to_finish() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::new(temp.path().to_path_buf()).unwrap();
        state.idle_push_enabled.store(true, Ordering::Release);
        let reservation = reserve_jmap_push_start(&state, "account")
            .await
            .unwrap()
            .unwrap();
        let generation = reservation.generation;
        let cancellation = reservation.cancellation.clone();
        let release = Arc::new(tokio::sync::Notify::new());
        let task_release = release.clone();
        {
            let mut handles = state.jmap_push_handles.lock().unwrap();
            let handle = handles.get_mut("account").unwrap();
            handle.phase = JmapPushPhase::Running;
            handle.task = Some(tokio::spawn(async move {
                task_release.notified().await;
            }));
            handle.phase = JmapPushPhase::Stopping;
            cancellation.cancel();
        }
        reservation.commit();

        let restart = reserve_jmap_push_start(&state, "account");
        tokio::pin!(restart);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(40), restart.as_mut())
                .await
                .is_err()
        );

        release.notify_one();
        let replacement = tokio::time::timeout(std::time::Duration::from_secs(1), restart.as_mut())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(replacement.generation > generation);
    }

    #[tokio::test]
    async fn jmap_stop_aborts_and_joins_unresponsive_task() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::new(temp.path().to_path_buf()).unwrap();
        state.idle_push_enabled.store(true, Ordering::Release);
        let reservation = reserve_jmap_push_start(&state, "account")
            .await
            .unwrap()
            .unwrap();
        let generation = reservation.generation;
        {
            let mut handles = state.jmap_push_handles.lock().unwrap();
            let handle = handles.get_mut("account").unwrap();
            handle.phase = JmapPushPhase::Stopping;
            handle.task = Some(tokio::spawn(std::future::pending()));
            handle.cancellation.cancel();
        }
        reservation.commit();

        wait_for_jmap_push_stop(
            &state,
            "account",
            generation,
            Some(tokio::time::Instant::now()),
        )
        .await
        .unwrap();

        assert!(!state
            .jmap_push_handles
            .lock()
            .unwrap()
            .contains_key("account"));
    }

    #[tokio::test]
    async fn dropping_jmap_start_rolls_back_uncommitted_reservation() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::new(temp.path().to_path_buf()).unwrap();
        state.idle_push_enabled.store(true, Ordering::Release);
        let reservation = reserve_jmap_push_start(&state, "account")
            .await
            .unwrap()
            .unwrap();
        drop(reservation);

        assert!(!state
            .jmap_push_handles
            .lock()
            .unwrap()
            .contains_key("account"));
    }

    #[tokio::test]
    async fn jmap_stop_accepts_a_newer_generation_after_old_joined() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::new(temp.path().to_path_buf()).unwrap();
        state.idle_push_enabled.store(true, Ordering::Release);
        let old = reserve_jmap_push_start(&state, "account")
            .await
            .unwrap()
            .unwrap();
        let old_generation = old.generation;
        {
            let mut handles = state.jmap_push_handles.lock().unwrap();
            let handle = handles.get_mut("account").unwrap();
            handle.phase = JmapPushPhase::Stopping;
            handle.task = Some(tokio::spawn(async {}));
        }
        old.commit();
        while !state.jmap_push_handles.lock().unwrap()["account"]
            .task
            .as_ref()
            .unwrap()
            .is_finished()
        {
            tokio::task::yield_now().await;
        }
        let finished_task = {
            let mut handles = state.jmap_push_handles.lock().unwrap();
            let handle = handles.get_mut("account").unwrap();
            handle.phase = JmapPushPhase::Joining;
            handle.task.take().unwrap()
        };
        finish_jmap_push_join(&state, "account", old_generation, finished_task).unwrap();

        let replacement = reserve_jmap_push_start(&state, "account")
            .await
            .unwrap()
            .unwrap();
        assert!(replacement.generation > old_generation);
        wait_for_jmap_push_stop(&state, "account", old_generation, None)
            .await
            .unwrap();
    }

    #[test]
    fn jmap_cancellation_is_linearized_with_event_delivery() {
        let cancellation = CancellationToken::new();
        let event_gate = Arc::new(Mutex::new(()));
        let (event_started_tx, event_started_rx) = std::sync::mpsc::channel();
        let (release_event_tx, release_event_rx) = std::sync::mpsc::channel();
        let event_cancellation = cancellation.clone();
        let callback_gate = event_gate.clone();
        let event = std::thread::spawn(move || {
            if_jmap_push_running(&event_cancellation, &callback_gate, || {
                event_started_tx.send(()).unwrap();
                release_event_rx.recv().unwrap();
            });
        });
        event_started_rx.recv().unwrap();

        let stop_cancellation = cancellation.clone();
        let stop_gate = event_gate.clone();
        let (stop_started_tx, stop_started_rx) = std::sync::mpsc::channel();
        let stop = std::thread::spawn(move || {
            stop_started_tx.send(()).unwrap();
            cancel_jmap_push(&stop_cancellation, &stop_gate);
        });
        stop_started_rx.recv().unwrap();
        assert!(!cancellation.is_cancelled());

        release_event_tx.send(()).unwrap();
        event.join().unwrap();
        stop.join().unwrap();
        let emitted_after_stop = AtomicBool::new(false);
        if_jmap_push_running(&cancellation, &event_gate, || {
            emitted_after_stop.store(true, Ordering::Relaxed);
        });

        assert!(cancellation.is_cancelled());
        assert!(!emitted_after_stop.load(Ordering::Relaxed));
    }

    #[test]
    fn imap_operation_lock_is_exclusive_per_account() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::new(temp.path().to_path_buf()).unwrap();
        let first = imap_idle_account_lock(&state, "account");
        let second = imap_idle_account_lock(&state, "account");
        let _guard = first.try_lock_owned().unwrap();

        assert!(second.try_lock_owned().is_err());
    }

    #[tokio::test]
    async fn imap_suspend_failure_attempts_recovery() {
        let recovered = Arc::new(AtomicBool::new(false));
        let recovered_in_future = recovered.clone();
        let result = suspend_imap_idle_with_recovery::<String, _, _>(
            "account",
            async { Err(Error::Other("suspend failed".into())) },
            async move {
                recovered_in_future.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .await;

        assert!(recovered.load(Ordering::Relaxed));
        assert_eq!(result.unwrap_err().to_string(), "suspend failed");
    }

    #[tokio::test]
    async fn idle_stop_timeout_retains_stopping_generation() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::new(temp.path().to_path_buf()).unwrap();
        let (generation, _) = reserve_imap_idle_start(&state, "account").unwrap().unwrap();
        {
            let mut handles = state.idle_handles.lock().unwrap();
            let handle = handles.get_mut("account").unwrap();
            handle.phase = IdlePhase::Running;
            handle.thread = Some(std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }));
        }

        request_imap_idle_stop(&state, "account").unwrap();
        let result = wait_for_imap_idle_stop(
            &state,
            "account",
            generation,
            Some(tokio::time::Instant::now()),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(
            state.idle_handles.lock().unwrap()["account"].phase,
            IdlePhase::Stopping
        );

        wait_for_imap_idle_stop(
            &state,
            "account",
            generation,
            Some(tokio::time::Instant::now() + std::time::Duration::from_secs(1)),
        )
        .await
        .unwrap();
        assert!(!state.idle_handles.lock().unwrap().contains_key("account"));
    }

    #[tokio::test]
    async fn concurrent_idle_stop_waiters_both_observe_thread_panic() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::new(temp.path().to_path_buf()).unwrap();
        let (generation, _) = reserve_imap_idle_start(&state, "account").unwrap().unwrap();
        {
            let mut handles = state.idle_handles.lock().unwrap();
            let handle = handles.get_mut("account").unwrap();
            handle.phase = IdlePhase::Running;
            handle.thread = Some(std::thread::spawn(|| panic!("IDLE test panic")));
        }
        while !state.idle_handles.lock().unwrap()["account"]
            .thread
            .as_ref()
            .unwrap()
            .is_finished()
        {
            tokio::task::yield_now().await;
        }
        request_imap_idle_stop(&state, "account").unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);

        let (first, second) = tokio::join!(
            wait_for_imap_idle_stop(&state, "account", generation, Some(deadline)),
            wait_for_imap_idle_stop(&state, "account", generation, Some(deadline))
        );

        assert!(first.is_err());
        assert!(second.is_err());
        assert_eq!(
            state.idle_handles.lock().unwrap()["account"].phase,
            IdlePhase::StopFailed
        );
    }

    /// A second acquire for the same account is rejected while the first
    /// guard is alive — the serialization contract that both
    /// `trigger_sync` and `sync_calendars` rely on to keep their emitted
    /// "*-sync-*" events coherent.
    #[test]
    fn try_acquire_sync_guard_rejects_concurrent_same_account() {
        let flags: Mutex<HashMap<String, Arc<AtomicBool>>> = Mutex::new(HashMap::new());

        let first = try_acquire_sync_guard(&flags, "acc-1", "test");
        assert!(first.is_some());

        let second = try_acquire_sync_guard(&flags, "acc-1", "test");
        assert!(
            second.is_none(),
            "concurrent acquire for the same account must be rejected",
        );

        // A different account is unaffected.
        let other = try_acquire_sync_guard(&flags, "acc-2", "test");
        assert!(other.is_some());

        // Releasing the first guard lets the next caller proceed.
        drop(first);
        let retry = try_acquire_sync_guard(&flags, "acc-1", "test");
        assert!(retry.is_some());
    }

    #[test]
    fn deferred_mail_sync_coalesces_to_broadest_request() {
        let pending: Mutex<HashMap<String, Option<String>>> = Mutex::new(HashMap::new());

        record_deferred_mail_sync(&pending, "acc-1", Some("INBOX".into()));
        record_deferred_mail_sync(&pending, "acc-1", Some("Important".into()));
        assert_eq!(
            pending.lock().unwrap().get("acc-1").cloned(),
            Some(Some("Important".into()))
        );

        record_deferred_mail_sync(&pending, "acc-1", None);
        assert_eq!(pending.lock().unwrap().get("acc-1").cloned(), Some(None));

        record_deferred_mail_sync(&pending, "acc-1", Some("INBOX".into()));
        assert_eq!(
            pending.lock().unwrap().get("acc-1").cloned(),
            Some(None),
            "a full-account pending sync must not be narrowed by a later folder hint",
        );
    }
}
