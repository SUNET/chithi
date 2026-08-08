use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, State};

use crate::event::{emit_folders_changed, emit_messages_changed};

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

use crate::auth::build_jmap_config;
use crate::db;
use crate::error::{Error, Result};
use crate::mail::imap::ImapConfig;
use crate::state::AppState;

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

pub(crate) async fn suspend_imap_idle_for_account(
    state: &State<'_, AppState>,
    account_id: &str,
) -> Result<bool> {
    let handle = {
        let mut handles = state.idle_handles.lock().unwrap();
        handles.remove(account_id)
    };

    let Some(mut idle_handle) = handle else {
        return Ok(false);
    };

    idle_handle.stop_flag.store(true, Ordering::Relaxed);

    if let Some(thread) = idle_handle.thread.take() {
        tokio::task::spawn_blocking(move || {
            let _ = thread.join();
        })
        .await
        .map_err(|e| Error::Sync(format!("Stopping IDLE panicked: {}", e)))?;
    }

    Ok(true)
}

pub(crate) async fn resume_imap_idle_for_account(
    app: &AppHandle,
    state: &State<'_, AppState>,
    account: &db::accounts::AccountFull,
    suspended_idle: bool,
) -> Result<()> {
    if !suspended_idle || !should_start_imap_idle(&account.auth_method) {
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

    start_imap_idle(app, state, &account_summary).await
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
        suspend_imap_idle_for_account(state, account_id).await?
    } else {
        false
    };
    let resume_account = account.clone();

    let ctx = crate::backend::mail::MailSyncCtx {
        app: app.clone(),
        db: state.db.clone(),
        data_dir: state.data_dir.clone(),
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
        suspend_imap_idle_for_account(&state, &account_id).await?
    } else {
        false
    };
    let resume_account = account.clone();

    let ctx = crate::backend::mail::MailSyncCtx {
        app: app.clone(),
        db: state.db.clone(),
        data_dir: state.data_dir.clone(),
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
            suspend_imap_idle_for_account(&state, &account_id).await?
        } else {
            false
        };
        let resume_account = account.clone();

        // For O365: get IMAP-scoped OAuth token
        let ctx = crate::backend::mail::MailSyncCtx {
            app: app.clone(),
            db: state.db.clone(),
            data_dir: state.data_dir.clone(),
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
            start_imap_idle(&app, &state, account).await?;
        } else if account.mail_protocol == "jmap" {
            start_jmap_push(&app, &state, account).await?;
        }
    }

    Ok(())
}

async fn start_imap_idle(
    app: &AppHandle,
    state: &State<'_, AppState>,
    account: &db::accounts::Account,
) -> Result<()> {
    // Check if already running
    {
        let handles = state.idle_handles.lock().unwrap();
        if handles.contains_key(&account.id) {
            log::debug!("IDLE already running for account {}", account.id);
            return Ok(());
        }
    }

    let full_account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account.id)?
    };

    // For O365: get IMAP-scoped OAuth token
    let (password, use_xoauth2) = crate::auth::get_imap_credentials(&full_account).await?;

    let config = ImapConfig {
        host: full_account.imap_host.clone(),
        port: full_account.imap_port,
        username: full_account.username.clone(),
        password,
        use_tls: full_account.use_tls,
        use_xoauth2,
    };

    let account_id = account.id.clone();
    let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = stop_flag.clone();
    let app_clone = app.clone();

    let thread = std::thread::spawn(move || {
        crate::mail::idle::run_idle_loop(
            config,
            account_id.clone(),
            stop_clone,
            Box::new(move |event| match event {
                crate::mail::idle::IdleEvent::NewMail(aid) => {
                    app_clone.emit("idle-new-mail", aid).ok();
                }
                crate::mail::idle::IdleEvent::Disconnected(aid) => {
                    app_clone.emit("idle-disconnected", aid).ok();
                }
                crate::mail::idle::IdleEvent::Reconnected(aid) => {
                    app_clone.emit("idle-reconnected", aid).ok();
                }
            }),
        );
    });

    let handle = crate::state::IdleHandle {
        stop_flag,
        thread: Some(thread),
    };

    state
        .idle_handles
        .lock()
        .unwrap()
        .insert(account.id.clone(), handle);
    log::info!("Started IDLE loop for account {}", account.id);
    Ok(())
}

async fn start_jmap_push(
    app: &AppHandle,
    state: &State<'_, AppState>,
    account: &db::accounts::Account,
) -> Result<()> {
    // Check if already running
    {
        let handles = state.jmap_push_handles.lock().unwrap();
        if handles.contains_key(&account.id) {
            log::debug!("JMAP push already running for account {}", account.id);
            return Ok(());
        }
    }

    let full_account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account.id)?
    };

    let jmap_config = build_jmap_config(&full_account).await?;

    let account_id = account.id.clone();
    let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = stop_flag.clone();
    let app_clone = app.clone();

    let task = tokio::spawn(async move {
        crate::mail::jmap_push::run_push_loop(
            jmap_config,
            account_id.clone(),
            stop_clone,
            std::sync::Arc::new(move |event| match event {
                crate::mail::jmap_push::PushEvent::StateChange(aid) => {
                    app_clone.emit("idle-new-mail", &aid).ok();
                }
                crate::mail::jmap_push::PushEvent::Disconnected(aid) => {
                    app_clone.emit("idle-disconnected", &aid).ok();
                }
                crate::mail::jmap_push::PushEvent::Reconnected(aid) => {
                    app_clone.emit("idle-reconnected", &aid).ok();
                }
            }),
        )
        .await;
    });

    let handle = crate::state::JmapPushHandle { stop_flag, task };

    state
        .jmap_push_handles
        .lock()
        .unwrap()
        .insert(account.id.clone(), handle);
    log::info!("Started JMAP push for account {}", account.id);
    Ok(())
}

/// Stop all IMAP IDLE loops and JMAP push tasks.
#[tauri::command]
pub async fn stop_idle(state: State<'_, AppState>) -> Result<()> {
    let idle_threads = {
        let mut handles = state.idle_handles.lock().unwrap();
        let mut threads = Vec::new();
        for (account_id, mut handle) in handles.drain() {
            log::info!("Stopping IDLE loop for account {}", account_id);
            handle.stop_flag.store(true, Ordering::Relaxed);
            if let Some(thread) = handle.thread.take() {
                threads.push((account_id, thread));
            }
        }
        threads
    };

    let jmap_tasks = {
        let mut jmap_handles = state.jmap_push_handles.lock().unwrap();
        let mut tasks = Vec::new();
        for (account_id, handle) in jmap_handles.drain() {
            log::info!("Stopping JMAP push for account {}", account_id);
            handle.stop_flag.store(true, Ordering::Relaxed);
            tasks.push((account_id, handle.task));
        }
        tasks
    };

    let mut stop_error: Option<Error> = None;

    for (account_id, mut task) in jmap_tasks {
        match tokio::time::timeout(std::time::Duration::from_secs(5), &mut task).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) if e.is_cancelled() => {
                log::debug!(
                    "JMAP push task for account {} was already cancelled",
                    account_id
                );
            }
            Ok(Err(e)) => {
                let msg = format!(
                    "JMAP push task for account {} failed during stop: {}",
                    account_id, e
                );
                log::error!("{}", msg);
                stop_error.get_or_insert_with(|| Error::Sync(msg));
            }
            Err(_) => {
                log::warn!(
                    "JMAP push task for account {} did not stop gracefully; aborting",
                    account_id
                );
                task.abort();
                if let Err(e) = task.await {
                    if !e.is_cancelled() {
                        let msg = format!(
                            "JMAP push task for account {} failed after abort: {}",
                            account_id, e
                        );
                        log::error!("{}", msg);
                        stop_error.get_or_insert_with(|| Error::Sync(msg));
                    }
                }
            }
        }
    }

    for (account_id, thread) in idle_threads {
        let join_task = tokio::task::spawn_blocking(move || thread.join());
        match tokio::time::timeout(std::time::Duration::from_secs(5), join_task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(_))) => {
                let msg = format!("IDLE loop for account {} panicked during stop", account_id);
                log::error!("{}", msg);
                stop_error.get_or_insert_with(|| Error::Sync(msg));
            }
            Ok(Err(e)) => {
                let msg = format!(
                    "Stopping IDLE loop for account {} panicked: {}",
                    account_id, e
                );
                log::error!("{}", msg);
                stop_error.get_or_insert_with(|| Error::Sync(msg));
            }
            Err(_) => {
                log::warn!(
                    "IDLE loop for account {} did not stop within 5s; it will exit after IDLE returns",
                    account_id
                );
            }
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
