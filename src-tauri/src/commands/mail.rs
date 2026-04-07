use tauri::State;

use crate::db;
use crate::db::messages::{MessageSummary, ThreadedPage};
use crate::error::{Error, Result};
use crate::mail::imap::ImapConfig;
use crate::mail::jmap::JmapConfig;
use crate::mail::jmap_sync;
use crate::mail::parser;
use crate::mail::sync as mail_sync;
use crate::state::AppState;

#[tauri::command]
pub async fn list_folders(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Vec<db::folders::Folder>> {
    log::debug!("Listing folders for account {}", account_id);
    let conn = state.db.lock().await;
    let folders = db::folders::list_folders(&conn, &account_id)?;
    log::debug!("Found {} folders for account {}", folders.len(), account_id);
    Ok(folders)
}

#[tauri::command]
pub async fn get_messages(
    state: State<'_, AppState>,
    account_id: String,
    folder_path: String,
    page: u32,
    per_page: u32,
    sort_column: Option<String>,
    sort_asc: Option<bool>,
) -> Result<db::messages::MessagePage> {
    let col = sort_column.as_deref().unwrap_or("date");
    let asc = sort_asc.unwrap_or(false);
    log::debug!(
        "Getting messages: account={} folder={} page={} per_page={} sort={}:{}",
        account_id,
        folder_path,
        page,
        per_page,
        col,
        if asc { "asc" } else { "desc" }
    );
    let conn = state.db.lock().await;
    let result =
        db::messages::get_messages(&conn, &account_id, &folder_path, page, per_page, col, asc)?;
    log::debug!(
        "Returned {} messages (total={}) for folder {}",
        result.messages.len(),
        result.total,
        folder_path
    );
    Ok(result)
}

#[tauri::command]
pub async fn get_message_body(
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
) -> Result<db::messages::MessageBody> {
    log::debug!("Loading message body: {}", message_id);

    let (maildir_path, from_email, to_json, cc_json, flags_json, is_encrypted, is_signed) = {
        let conn = state.db.lock().await;
        db::messages::get_message_metadata(&conn, &account_id, &message_id)?
    };

    // If body hasn't been downloaded yet, fetch it on-demand
    let actual_maildir_path = if maildir_path.is_empty() {
        // Get account config and message details
        let (account, folder_path, uid) = {
            let conn = state.db.lock().await;
            let account = db::accounts::get_account_full(&conn, &account_id)?;
            let (fp, u) = db::messages::get_folder_and_uid(&conn, &message_id)?;
            (account, fp, u)
        };

        let flags: Vec<String> = serde_json::from_str(&flags_json).unwrap_or_default();
        let data_dir = state.data_dir.clone();

        let relative_path = if account.mail_protocol == "jmap" {
            log::info!("Body not on disk for {}, fetching from JMAP", message_id);

            let jmap_config = JmapConfig {
                jmap_url: account.jmap_url.clone(),
                email: account.email.clone(),
                username: account.username.clone(),
                password: account.password.clone(),
            };

            // Extract the JMAP email ID from our composite message ID
            // Format: {account_id}_{folder_path}_{jmap_email_id}
            let jmap_email_id = message_id
                .strip_prefix(&format!("{}_{}_", account_id, folder_path))
                .unwrap_or(&message_id);

            jmap_sync::fetch_and_store_jmap_body(
                &jmap_config,
                &data_dir,
                &account_id,
                &folder_path,
                jmap_email_id,
                &flags,
            )
            .await?
        } else {
            log::info!("Body not on disk for {}, fetching from IMAP", message_id);

            let imap_config = ImapConfig {
                host: account.imap_host,
                port: account.imap_port,
                username: account.username,
                password: account.password,
                use_tls: account.use_tls,
            };

            let account_id_clone = account_id.clone();
            tokio::task::spawn_blocking(move || {
                mail_sync::fetch_and_store_body(
                    &imap_config,
                    &data_dir,
                    &account_id_clone,
                    &folder_path,
                    uid,
                    &flags,
                )
            })
            .await
            .map_err(|e| Error::Other(format!("Body fetch panicked: {}", e)))??
        };

        // Update the maildir_path in the database
        {
            let conn = state.db.lock().await;
            db::messages::update_maildir_path(&conn, &message_id, &relative_path)?;
        }

        relative_path
    } else {
        maildir_path
    };

    // Read and parse the message from disk
    let full_path = state.data_dir.join(&actual_maildir_path);
    log::debug!("Reading message from {}", full_path.display());
    let raw = std::fs::read(&full_path).map_err(|e| {
        log::error!(
            "Failed to read message file {}: {}",
            full_path.display(),
            e
        );
        Error::Other(format!(
            "Failed to read message file {}: {}",
            full_path.display(),
            e
        ))
    })?;

    parser::parse_message_body(
        &message_id,
        &raw,
        &from_email,
        &to_json,
        &cc_json,
        &flags_json,
        is_encrypted,
        is_signed,
    )
    .ok_or_else(|| {
        log::error!("Failed to parse message body for {}", message_id);
        Error::MailParse("Failed to parse message".to_string())
    })
}

#[tauri::command]
pub async fn get_threaded_messages(
    state: State<'_, AppState>,
    account_id: String,
    folder_path: String,
    page: u32,
    per_page: u32,
    sort_column: Option<String>,
    sort_asc: Option<bool>,
) -> Result<ThreadedPage> {
    let col = sort_column.as_deref().unwrap_or("date");
    let asc = sort_asc.unwrap_or(false);
    log::debug!(
        "Getting threaded messages: account={} folder={} page={} per_page={} sort={}:{}",
        account_id,
        folder_path,
        page,
        per_page,
        col,
        if asc { "asc" } else { "desc" }
    );
    let conn = state.db.lock().await;
    let result = db::messages::get_threaded_messages(
        &conn,
        &account_id,
        &folder_path,
        page,
        per_page,
        col,
        asc,
    )?;
    log::debug!(
        "Returned {} threads (total_threads={}, total_messages={}) for folder {}",
        result.threads.len(),
        result.total_threads,
        result.total_messages,
        folder_path
    );
    Ok(result)
}

#[tauri::command]
pub async fn get_thread_messages(
    state: State<'_, AppState>,
    account_id: String,
    folder_path: String,
    thread_id: String,
) -> Result<Vec<MessageSummary>> {
    log::debug!(
        "Getting thread messages: account={} folder={} thread={}",
        account_id,
        folder_path,
        thread_id
    );
    let conn = state.db.lock().await;
    let messages = db::messages::get_thread_messages(&conn, &account_id, &folder_path, &thread_id)?;
    log::debug!(
        "Returned {} messages for thread {}",
        messages.len(),
        thread_id
    );
    Ok(messages)
}

#[tauri::command]
pub async fn unthread_message(
    state: State<'_, AppState>,
    message_id: String,
) -> Result<()> {
    log::info!("Unthreading message: {}", message_id);
    let conn = state.db.lock().await;
    db::messages::unthread_message(&conn, &message_id)?;
    Ok(())
}

/// Create a new folder on the mail server and register it locally.
#[tauri::command]
pub async fn create_folder(
    state: State<'_, AppState>,
    account_id: String,
    folder_path: String,
) -> Result<()> {
    log::info!("Creating folder '{}' for account {}", folder_path, account_id);

    let account = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)?
    };

    if account.mail_protocol == "jmap" {
        // JMAP: Mailbox/set create
        let jmap_config = JmapConfig {
            jmap_url: account.jmap_url.clone(),
            email: account.email.clone(),
            username: account.username.clone(),
            password: account.password.clone(),
        };
        let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
        conn_jmap.create_mailbox(&jmap_config, &folder_path).await?;
    } else {
        // IMAP: CREATE
        let imap_config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username,
            password: account.password,
            use_tls: account.use_tls,
        };
        let folder_for_imap = folder_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = crate::mail::imap::ImapConnection::connect(&imap_config)?;
            conn.create_folder(&folder_for_imap)?;
            conn.logout();
            Ok::<(), crate::error::Error>(())
        })
        .await
        .map_err(|e| Error::Other(format!("Create folder panicked: {}", e)))??;
    }

    // Don't insert into local DB here — the next sync will discover the folder
    // with the correct server-side path/ID and register it properly.

    log::info!("Folder '{}' created on server, will appear after sync", folder_path);
    Ok(())
}
