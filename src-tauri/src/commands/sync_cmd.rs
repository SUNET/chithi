use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

/// (message_id, uid, flags_json) tuple for prefetch grouping.
type PrefetchMsg = (String, u32, String);

/// RAII guard that clears the sync-in-progress flag on drop.
struct SyncGuard(Arc<AtomicBool>);
impl Drop for SyncGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

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
    // Check if a sync is already running for this account — skip if so
    {
        let flags = state.sync_in_progress.lock().unwrap();
        if let Some(flag) = flags.get(&account_id) {
            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                log::debug!("Sync already in progress for account {}, skipping", account_id);
                return Ok(());
            }
        }
    }

    // Set the sync-in-progress flag
    let sync_flag = {
        let mut flags = state.sync_in_progress.lock().unwrap();
        let flag = flags
            .entry(account_id.clone())
            .or_insert_with(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
        flag.clone()
    };

    // Ensure flag is cleared when sync completes (success or error)
    let _guard = SyncGuard(sync_flag);

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

        // For O365 accounts, get an IMAP-scoped OAuth token
        let (password, use_xoauth2) = if account.provider == "o365" {
            let tokens = crate::oauth::load_tokens(&account_id)?
                .ok_or_else(|| Error::Other("No O365 OAuth tokens. Please sign in with Microsoft.".into()))?;
            let refresh_token = tokens.refresh_token
                .ok_or_else(|| Error::Other("No O365 refresh token.".into()))?;
            let imap_tokens = crate::oauth::refresh_with_scopes(
                &crate::oauth::MICROSOFT,
                &refresh_token,
                crate::oauth::MICROSOFT_IMAP_SCOPES,
            ).await?;
            // Save the potentially rotated refresh token
            crate::oauth::store_tokens(&account_id, &crate::oauth::OAuthTokens {
                access_token: imap_tokens.access_token.clone(),
                refresh_token: imap_tokens.refresh_token,
                expires_at: imap_tokens.expires_at,
            })?;
            (imap_tokens.access_token, true)
        } else {
            (account.password.clone(), false)
        };

        let imap_config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username.clone(),
            password,
            use_tls: account.use_tls,
            use_xoauth2,
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

    // IMAP path — for O365, refresh IMAP-scoped token
    let (password, use_xoauth2) = if account.provider == "o365" {
        let tokens = crate::oauth::load_tokens(&account_id)?
            .ok_or_else(|| Error::Other("No O365 tokens".into()))?;
        let refresh = tokens.refresh_token
            .ok_or_else(|| Error::Other("No O365 refresh token".into()))?;
        let new = crate::oauth::refresh_with_scopes(
            &crate::oauth::MICROSOFT, &refresh, crate::oauth::MICROSOFT_IMAP_SCOPES,
        ).await?;
        crate::oauth::store_tokens(&account_id, &new)?;
        (new.access_token, true)
    } else {
        (account.password, false)
    };

    let imap_config = ImapConfig {
        host: account.imap_host,
        port: account.imap_port,
        username: account.username,
        password,
        use_tls: account.use_tls,
        use_xoauth2,
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
    // Skip if a prefetch is already running for this account
    let prefetch_key = format!("prefetch_{}", account_id);
    {
        let flags = state.sync_in_progress.lock().unwrap();
        if let Some(flag) = flags.get(&prefetch_key) {
            if flag.load(Ordering::Relaxed) {
                log::debug!("Prefetch already in progress for account {}, skipping", account_id);
                return Ok(0);
            }
        }
    }
    let prefetch_flag = {
        let mut flags = state.sync_in_progress.lock().unwrap();
        let flag = flags
            .entry(prefetch_key)
            .or_insert_with(|| Arc::new(AtomicBool::new(false)));
        flag.store(true, Ordering::Relaxed);
        flag.clone()
    };
    let _guard = SyncGuard(prefetch_flag);

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

    let account = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)?
    };

    // For O365: get IMAP-scoped OAuth token
    let (password, use_xoauth2) = if account.provider == "o365" {
        let tokens = crate::oauth::load_tokens(&account_id)?
            .ok_or_else(|| Error::Other("No O365 tokens for prefetch".into()))?;
        let refresh_token = tokens.refresh_token
            .ok_or_else(|| Error::Other("No O365 refresh token for prefetch".into()))?;
        let imap_tokens = crate::oauth::refresh_with_scopes(
            &crate::oauth::MICROSOFT,
            &refresh_token,
            crate::oauth::MICROSOFT_IMAP_SCOPES,
        ).await?;
        crate::oauth::store_tokens(&account_id, &crate::oauth::OAuthTokens {
            access_token: imap_tokens.access_token.clone(),
            refresh_token: imap_tokens.refresh_token,
            expires_at: imap_tokens.expires_at,
        })?;
        (imap_tokens.access_token, true)
    } else {
        (account.password.clone(), false)
    };

    let imap_config = ImapConfig {
        host: account.imap_host,
        port: account.imap_port,
        username: account.username,
        password,
        use_tls: account.use_tls,
        use_xoauth2,
    };
    let data_dir = state.data_dir.clone();

    // Fetch the list of unfetched messages (up to 1000 per cycle)
    let unfetched = {
        let conn = state.db.lock().await;
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

    let db = state.db.clone();
    let folder_count = by_folder.len();
    let max_connections = 3.min(folder_count);

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
                                log::error!("Prefetch thread {}: connect failed: {}", thread_idx, e);
                                return Err(e);
                            }
                        };
                        let mut count = 0u32;

                        for (folder_path, messages) in &folders {
                            log::info!(
                                "Prefetch[{}]: folder '{}' ({} messages)",
                                thread_idx, folder_path, messages.len()
                            );
                            if let Err(e) = conn.select_folder(folder_path) {
                                log::error!("Prefetch[{}]: select '{}' failed: {}", thread_idx, folder_path, e);
                                continue;
                            }

                            let sanitized = mail_sync::sanitize_folder_name(folder_path);
                            let maildir_base = data_dir.join(&account_id).join(&sanitized);
                            if let Err(e) = mail_sync::create_maildir_dirs(&maildir_base) {
                                log::error!("Prefetch[{}]: maildir dirs failed: {}", thread_idx, e);
                                continue;
                            }

                            for chunk in messages.chunks(100) {
                                let batch_uids: Vec<u32> = chunk.iter().map(|(_, uid, _)| *uid).collect();
                                let bodies = match conn.fetch_bodies_batch(&batch_uids) {
                                    Ok(b) => b,
                                    Err(e) => {
                                        log::error!("Prefetch[{}]: batch fetch failed: {}", thread_idx, e);
                                        continue;
                                    }
                                };

                                let mut db_updates: Vec<(String, String)> = Vec::new();
                                for (message_id, uid, flags_json) in chunk {
                                    let body = match bodies.get(uid) {
                                        Some(b) => b,
                                        None => continue,
                                    };
                                    let flags: Vec<String> = serde_json::from_str(flags_json).unwrap_or_default();
                                    let suffix = mail_sync::flags_to_maildir_suffix(&flags);
                                    let filename = format!("{}:2,{}", uid, suffix);
                                    let msg_path = maildir_base.join("cur").join(&filename);
                                    if std::fs::write(&msg_path, body).is_err() { continue; }
                                    let relative_path = format!("{}/{}/cur/{}", account_id, sanitized, filename);
                                    db_updates.push((message_id.clone(), relative_path));
                                    count += 1;
                                }

                                if !db_updates.is_empty() {
                                    let conn = rt.block_on(db.lock());
                                    conn.execute_batch("BEGIN").ok();
                                    for (msg_id, path) in &db_updates {
                                        db::messages::update_maildir_path(&conn, msg_id, path).ok();
                                    }
                                    conn.execute_batch("COMMIT").ok();
                                    log::info!("Prefetch[{}]: saved {} bodies in '{}'", thread_idx, db_updates.len(), folder_path);
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
                .map(|h| h.join().unwrap_or(Err(Error::Sync("Prefetch thread panicked".into()))))
                .collect()
        });

        let total: u32 = results.into_iter().flatten().sum();
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

/// Sync an O365 account via Microsoft Graph API.
async fn sync_graph_account(
    app: AppHandle,
    db_arc: std::sync::Arc<tokio::sync::Mutex<rusqlite::Connection>>,
    account_id: &str,
) -> Result<()> {
    use crate::mail::graph::{self, GraphClient};

    let token = graph::get_graph_token(account_id).await?;
    let client = GraphClient::new(&token);

    // Sync mail folders
    let graph_folders = client.list_mail_folders().await?;
    log::info!("Graph sync: {} mail folders for account {}", graph_folders.len(), account_id);

    {
        let conn = db_arc.lock().await;
        for gf in &graph_folders {
            let folder_type = graph::guess_folder_type(&gf.display_name);
            db::folders::upsert_folder(&conn, account_id, &gf.display_name, &gf.id, folder_type)?;
            db::folders::update_folder_counts(&conn, account_id, &gf.id, gf.unread_count, gf.total_count)?;
        }
    }

    // Sync messages for each folder
    let mut grand_total = 0u32;
    for gf in &graph_folders {
        // Fetch messages (Graph uses skip-based pagination, not UIDs)
        let (messages, _total) = client.list_messages(&gf.id, 200, 0).await?;

        if messages.is_empty() {
            continue;
        }

        let existing_ids = {
            let conn = db_arc.lock().await;
            let mut stmt = conn.prepare(
                "SELECT id FROM messages WHERE account_id = ?1 AND folder_path = ?2"
            ).map_err(Error::Database)?;
            let ids: std::collections::HashSet<String> = stmt
                .query_map(rusqlite::params![account_id, gf.id], |row| row.get(0))
                .map_err(Error::Database)?
                .filter_map(|r| r.ok())
                .collect();
            ids
        };

        let conn = db_arc.lock().await;
        conn.execute_batch("BEGIN")?;

        let mut synced = 0u32;
        for msg in &messages {
            let id = format!("{}_{}", account_id, msg.id);
            if existing_ids.contains(&id) {
                continue;
            }

            let flags = if msg.is_read { vec!["seen".to_string()] } else { vec![] };
            let thread_id = msg.conversation_id.clone();

            let new_msg = db::messages::NewMessage {
                id: id.clone(),
                account_id: account_id.to_string(),
                folder_path: gf.id.clone(),
                uid: 0, // Graph doesn't use UIDs
                message_id: msg.internet_message_id.clone(),
                in_reply_to: None,
                thread_id,
                subject: msg.subject.clone(),
                from_name: msg.from_name.clone(),
                from_email: msg.from_email.clone().unwrap_or_else(|| "unknown".to_string()),
                to_addresses: msg.to_addresses.clone(),
                cc_addresses: msg.cc_addresses.clone(),
                date: msg.date.clone(),
                size: 0,
                has_attachments: msg.has_attachments,
                is_encrypted: false,
                is_signed: false,
                flags: serde_json::to_string(&flags).unwrap_or_default(),
                maildir_path: format!("graph:{}", msg.id), // Special marker for Graph bodies
                snippet: msg.preview.clone(),
            };
            db::messages::insert_message(&conn, &new_msg)?;
            synced += 1;
        }

        conn.execute_batch("COMMIT")?;
        drop(conn);

        if synced > 0 {
            log::info!("Graph sync: {} new messages in '{}'", synced, gf.display_name);
            grand_total += synced;
        }
    }

    app.emit("sync-complete", serde_json::json!({
        "account_id": account_id,
        "total_synced": grand_total,
    })).ok();

    log::info!("Graph sync: completed for account {}, {} new messages", account_id, grand_total);
    Ok(())
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

    // For O365: get IMAP-scoped OAuth token
    let (password, use_xoauth2) = if full_account.provider == "o365" {
        let tokens = crate::oauth::load_tokens(&account.id)?
            .ok_or_else(|| crate::error::Error::Other("No O365 tokens for IDLE".into()))?;
        let refresh_token = tokens.refresh_token
            .ok_or_else(|| crate::error::Error::Other("No O365 refresh token for IDLE".into()))?;
        let imap_tokens = crate::oauth::refresh_with_scopes(
            &crate::oauth::MICROSOFT,
            &refresh_token,
            crate::oauth::MICROSOFT_IMAP_SCOPES,
        ).await?;
        crate::oauth::store_tokens(&account.id, &crate::oauth::OAuthTokens {
            access_token: imap_tokens.access_token.clone(),
            refresh_token: imap_tokens.refresh_token,
            expires_at: imap_tokens.expires_at,
        })?;
        (imap_tokens.access_token, true)
    } else {
        (full_account.password.clone(), false)
    };

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
