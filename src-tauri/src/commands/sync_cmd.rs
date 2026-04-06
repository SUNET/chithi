use std::collections::BTreeMap;
use tauri::{AppHandle, Emitter, State};

use crate::db;
use crate::error::{Error, Result};
use crate::mail::imap::{ImapConfig, ImapConnection};
use crate::mail::jmap::JmapConfig;
use crate::mail::jmap_sync;
use crate::mail::sync as mail_sync;
use crate::state::AppState;

#[tauri::command]
pub async fn trigger_sync(
    app: AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    current_folder: Option<String>,
) -> Result<()> {
    log::info!("Sync requested for account {}", account_id);
    let account_result = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)
    };
    let account = match account_result {
        Ok(a) => a,
        Err(e) => {
            app.emit("sync-error", serde_json::json!({"account_id": account_id, "error": e.to_string()})).ok();
            return Err(e);
        }
    };

    if account.mail_protocol == "jmap" {
        log::info!(
            "Syncing account {} ({}) via JMAP (url={})",
            account.display_name,
            account.email,
            account.jmap_url
        );

        let jmap_config = JmapConfig {
            jmap_url: account.jmap_url.clone(),
            email: account.email.clone(),
            username: account.username.clone(),
            password: account.password.clone(),
        };

        if let Err(e) = jmap_sync::sync_jmap_account(
            app.clone(),
            state.db.clone(),
            state.data_dir.clone(),
            account_id.clone(),
            account.display_name.clone(),
            jmap_config.clone(),
            current_folder,
        )
        .await {
            app.emit("sync-error", serde_json::json!({"account_id": account_id, "error": e.to_string()})).ok();
            return Err(e);
        }

        // Also sync calendars for JMAP accounts
        log::info!("Syncing calendars for JMAP account {}", account_id);
        if let Err(e) = sync_jmap_calendars(state.db.clone(), &account_id, &jmap_config).await {
            log::error!("Calendar sync failed for account {}: {}", account_id, e);
            // Don't fail the whole sync if calendar sync fails
        }
    } else {
        log::info!(
            "Syncing account {} ({}) via IMAP {}:{}",
            account.display_name,
            account.email,
            account.imap_host,
            account.imap_port
        );

        let imap_config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username,
            password: account.password,
            use_tls: account.use_tls,
        };

        if let Err(e) = mail_sync::sync_account(
            app.clone(),
            state.db.clone(),
            state.data_dir.clone(),
            account_id.clone(),
            account.display_name,
            imap_config,
            current_folder,
        )
        .await {
            app.emit("sync-error", serde_json::json!({"account_id": account_id, "error": e.to_string()})).ok();
            return Err(e);
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn sync_folder(
    app: AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    folder_path: String,
) -> Result<u32> {
    log::info!("Single folder sync: account={} folder={}", account_id, folder_path);
    let account = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)?
    };

    if account.mail_protocol == "jmap" {
        let jmap_config = JmapConfig {
            jmap_url: account.jmap_url.clone(),
            email: account.email.clone(),
            username: account.username.clone(),
            password: account.password.clone(),
        };

        return jmap_sync::sync_jmap_folder_public(
            app,
            state.db.clone(),
            account_id,
            account.display_name,
            folder_path,
            jmap_config,
        )
        .await;
    }

    // IMAP path
    let imap_config = ImapConfig {
        host: account.imap_host,
        port: account.imap_port,
        username: account.username,
        password: account.password,
        use_tls: account.use_tls,
    };

    let db = state.db.clone();
    let account_name = account.display_name.clone();

    app.emit(
        "sync-started",
        serde_json::json!({
            "account_id": account_id,
            "account_name": account_name,
        }),
    ).ok();

    let _app_clone = app.clone();
    let account_id_clone = account_id.clone();
    let folder_clone = folder_path.clone();

    let result = tokio::task::spawn_blocking(move || {
        let mut conn_imap = ImapConnection::connect(&imap_config)?;
        conn_imap.select_folder(&folder_clone)?;
        let count = mail_sync::sync_folder_envelopes_public(
            &db, &account_id_clone, &mut conn_imap, &folder_clone,
        )?;
        conn_imap.logout();
        Ok::<u32, Error>(count)
    })
    .await
    .map_err(|e| Error::Sync(format!("Folder sync panicked: {}", e)))?;

    match &result {
        Ok(count) => {
            app.emit(
                "sync-complete",
                serde_json::json!({
                    "account_id": account_id,
                    "total_synced": count,
                }),
            ).ok();
            log::info!("Single folder sync done: {} new in {}", count, folder_path);
        }
        Err(e) => {
            app.emit(
                "sync-error",
                serde_json::json!({
                    "account_id": account_id,
                    "error": e.to_string(),
                }),
            ).ok();
        }
    }

    result
}

/// Sync JMAP calendars for an account. Extracted as a standalone async function
/// so it can be called from `trigger_sync` without needing `State`.
async fn sync_jmap_calendars(
    db: std::sync::Arc<tokio::sync::Mutex<rusqlite::Connection>>,
    account_id: &str,
    jmap_config: &JmapConfig,
) -> Result<()> {
    use crate::mail::jmap::JmapConnection;

    let jmap_conn = JmapConnection::connect(jmap_config).await?;

    // Fetch calendars
    let jmap_calendars = jmap_conn.list_jmap_calendars(jmap_config).await?;
    log::info!(
        "Calendar sync: fetched {} calendars for account {}",
        jmap_calendars.len(),
        account_id
    );

    let mut remote_to_local: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    {
        let conn = db.lock().await;
        for jcal in &jmap_calendars {
            let color = jcal.color.as_deref().unwrap_or("#4285f4");
            let local_id = crate::db::calendar::upsert_calendar_by_remote_id(
                &conn,
                account_id,
                &jcal.id,
                &jcal.name,
                color,
                jcal.is_default,
            )?;
            remote_to_local.insert(jcal.id.clone(), local_id);
        }
    }

    // Fetch and upsert events for each calendar
    for jcal in &jmap_calendars {
        let events = match jmap_conn
            .fetch_calendar_events(jmap_config, Some(&jcal.id))
            .await
        {
            Ok(evts) => evts,
            Err(e) => {
                log::error!(
                    "Calendar sync: failed to fetch events for '{}': {}",
                    jcal.name,
                    e
                );
                continue;
            }
        };

        log::info!(
            "Calendar sync: fetched {} events for calendar '{}'",
            events.len(),
            jcal.name
        );

        let local_cal_id = remote_to_local
            .get(&jcal.id)
            .cloned()
            .unwrap_or_default();

        let conn = db.lock().await;
        for ev in &events {
            let event_id = uuid::Uuid::new_v4().to_string();
            let cal_event = crate::db::calendar::CalendarEvent {
                id: event_id,
                account_id: account_id.to_string(),
                calendar_id: local_cal_id.clone(),
                uid: ev.uid.clone(),
                title: ev.title.clone(),
                description: ev.description.clone(),
                location: ev.location.clone(),
                start_time: ev.start.clone(),
                end_time: ev.end.clone(),
                all_day: ev.all_day,
                timezone: None,
                recurrence_rule: ev.recurrence_rule.clone(),
                organizer_email: None,
                attendees_json: None,
                my_status: None,
                source_message_id: None,
                ical_data: None,
                remote_id: Some(ev.id.clone()),
                etag: None,
            };

            if let Err(e) = crate::db::calendar::upsert_event_by_remote_id(&conn, &cal_event) {
                log::error!(
                    "Calendar sync: failed to upsert event '{}': {}",
                    ev.title,
                    e
                );
            }
        }
    }

    log::info!("Calendar sync: completed for account {}", account_id);
    Ok(())
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
    state: State<'_, AppState>,
    account_id: String,
) -> Result<u32> {
    log::info!("Prefetch bodies requested for account {}", account_id);

    // Skip prefetch for JMAP accounts — bodies are fetched on-demand via JMAP API
    {
        let conn = state.db.lock().await;
        let account = db::accounts::get_account_full(&conn, &account_id)?;
        if account.mail_protocol == "jmap" {
            log::debug!("Prefetch: skipping JMAP account {}", account_id);
            return Ok(0);
        }
    }

    let (imap_config, data_dir) = {
        let conn = state.db.lock().await;
        let account = db::accounts::get_account_full(&conn, &account_id)?;
        let config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username,
            password: account.password,
            use_tls: account.use_tls,
        };
        (config, state.data_dir.clone())
    };

    // Fetch the list of unfetched messages (up to 100)
    let unfetched = {
        let conn = state.db.lock().await;
        db::messages::get_unfetched_messages(&conn, &account_id, 100)?
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
    let mut by_folder: BTreeMap<String, Vec<(String, u32, String)>> = BTreeMap::new();
    for (message_id, folder_path, uid, flags_json) in unfetched {
        by_folder
            .entry(folder_path)
            .or_default()
            .push((message_id, uid, flags_json));
    }

    let db = state.db.clone();

    let fetched_count = tokio::task::spawn_blocking(move || -> Result<u32> {
        let mut conn_imap = ImapConnection::connect(&imap_config)?;
        let mut count = 0u32;

        for (folder_path, messages) in &by_folder {
            log::info!(
                "Prefetch: selecting folder '{}' ({} messages)",
                folder_path,
                messages.len()
            );
            if let Err(e) = conn_imap.select_folder(folder_path) {
                log::error!("Prefetch: failed to select folder '{}': {}", folder_path, e);
                continue;
            }

            for (message_id, uid, flags_json) in messages {
                log::debug!(
                    "Prefetch: fetching body for message_id={} uid={} folder={}",
                    message_id,
                    uid,
                    folder_path
                );

                let body = match conn_imap.fetch_message_body(*uid) {
                    Ok(Some(b)) => b,
                    Ok(None) => {
                        log::warn!(
                            "Prefetch: no body returned for uid={} in folder '{}'",
                            uid,
                            folder_path
                        );
                        continue;
                    }
                    Err(e) => {
                        log::error!(
                            "Prefetch: failed to fetch body for uid={} in folder '{}': {}",
                            uid,
                            folder_path,
                            e
                        );
                        continue;
                    }
                };

                // Parse flags from JSON to compute Maildir suffix
                let flags: Vec<String> =
                    serde_json::from_str(flags_json).unwrap_or_default();

                // Write body to Maildir
                let sanitized_folder = mail_sync::sanitize_folder_name(folder_path);
                let maildir_base = data_dir.join(&account_id).join(&sanitized_folder);
                if let Err(e) = mail_sync::create_maildir_dirs(&maildir_base) {
                    log::error!(
                        "Prefetch: failed to create maildir dirs for '{}': {}",
                        maildir_base.display(),
                        e
                    );
                    continue;
                }

                let suffix = mail_sync::flags_to_maildir_suffix(&flags);
                let filename = format!("{}:2,{}", uid, suffix);
                let msg_path = maildir_base.join("cur").join(&filename);

                if let Err(e) = std::fs::write(&msg_path, &body) {
                    log::error!(
                        "Prefetch: failed to write body to '{}': {}",
                        msg_path.display(),
                        e
                    );
                    continue;
                }

                let relative_path = format!(
                    "{}/{}/cur/{}",
                    account_id, sanitized_folder, filename
                );

                log::debug!(
                    "Prefetch: body saved {} ({} bytes)",
                    relative_path,
                    body.len()
                );

                // Update DB with the maildir path
                let rt = tokio::runtime::Handle::current();
                let conn = rt.block_on(db.lock());
                if let Err(e) = db::messages::update_maildir_path(&conn, message_id, &relative_path) {
                    log::error!(
                        "Prefetch: failed to update maildir_path for message_id={}: {}",
                        message_id,
                        e
                    );
                    continue;
                }

                count += 1;
            }
        }

        conn_imap.logout();
        log::info!(
            "Prefetch: completed for account {}, {} bodies fetched",
            account_id,
            count
        );
        Ok(count)
    })
    .await
    .map_err(|e| Error::Sync(format!("Prefetch task panicked: {}", e)))??;

    Ok(fetched_count)
}

/// Start IMAP IDLE and JMAP push for all enabled accounts. Call on app startup.
#[tauri::command]
pub async fn start_idle(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<()> {
    let accounts = {
        let conn = state.db.lock().await;
        db::accounts::list_accounts(&conn)?
    };

    for account in &accounts {
        if !account.enabled { continue; }

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
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account.id)?
    };

    let config = ImapConfig {
        host: full_account.imap_host.clone(),
        port: full_account.imap_port,
        username: full_account.username.clone(),
        password: full_account.password.clone(),
        use_tls: full_account.use_tls,
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
            Box::new(move |event| {
                match event {
                    crate::mail::idle::IdleEvent::NewMail(aid) => {
                        app_clone.emit("idle-new-mail", aid).ok();
                    }
                    crate::mail::idle::IdleEvent::Disconnected(aid) => {
                        app_clone.emit("idle-disconnected", aid).ok();
                    }
                    crate::mail::idle::IdleEvent::Reconnected(aid) => {
                        app_clone.emit("idle-reconnected", aid).ok();
                    }
                }
            }),
        );
    });

    let handle = crate::state::IdleHandle {
        stop_flag,
        thread: Some(thread),
    };

    state.idle_handles.lock().unwrap().insert(account.id.clone(), handle);
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
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account.id)?
    };

    let jmap_config = JmapConfig {
        jmap_url: full_account.jmap_url.clone(),
        email: full_account.email.clone(),
        username: full_account.username.clone(),
        password: full_account.password.clone(),
    };

    let account_id = account.id.clone();
    let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = stop_flag.clone();
    let app_clone = app.clone();

    let task = tokio::spawn(async move {
        crate::mail::jmap_push::run_push_loop(
            jmap_config,
            account_id.clone(),
            stop_clone,
            std::sync::Arc::new(move |event| {
                match event {
                    crate::mail::jmap_push::PushEvent::StateChange(aid) => {
                        app_clone.emit("idle-new-mail", &aid).ok();
                    }
                    crate::mail::jmap_push::PushEvent::Disconnected(aid) => {
                        app_clone.emit("idle-disconnected", &aid).ok();
                    }
                    crate::mail::jmap_push::PushEvent::Reconnected(aid) => {
                        app_clone.emit("idle-reconnected", &aid).ok();
                    }
                }
            }),
        )
        .await;
    });

    let handle = crate::state::JmapPushHandle {
        stop_flag,
        task,
    };

    state.jmap_push_handles.lock().unwrap().insert(account.id.clone(), handle);
    log::info!("Started JMAP push for account {}", account.id);
    Ok(())
}

/// Stop all IMAP IDLE loops and JMAP push tasks.
#[tauri::command]
pub async fn stop_idle(
    state: State<'_, AppState>,
) -> Result<()> {
    // Stop IMAP IDLE threads
    let mut handles = state.idle_handles.lock().unwrap();
    for (account_id, handle) in handles.drain() {
        log::info!("Stopping IDLE loop for account {}", account_id);
        handle.stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = handle.thread {
            drop(thread);
        }
    }
    drop(handles);

    // Stop JMAP push tasks
    let mut jmap_handles = state.jmap_push_handles.lock().unwrap();
    for (account_id, handle) in jmap_handles.drain() {
        log::info!("Stopping JMAP push for account {}", account_id);
        handle.stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        handle.task.abort();
    }

    Ok(())
}
