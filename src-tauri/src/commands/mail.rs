use tauri::State;

use crate::db;
use crate::error::{Error, Result};
use crate::mail::imap::ImapConfig;
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

    // If body hasn't been downloaded yet, fetch it from IMAP on-demand
    let actual_maildir_path = if maildir_path.is_empty() {
        log::info!("Body not on disk for {}, fetching from IMAP", message_id);

        // Get account config and message details for IMAP fetch
        let (account, folder_path, uid) = {
            let conn = state.db.lock().await;
            let account = db::accounts::get_account_full(&conn, &account_id)?;
            let (fp, u) = db::messages::get_folder_and_uid(&conn, &message_id)?;
            (account, fp, u)
        };

        let imap_config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username,
            password: account.password,
            use_tls: account.use_tls,
        };

        let flags: Vec<String> = serde_json::from_str(&flags_json).unwrap_or_default();
        let data_dir = state.data_dir.clone();

        // Fetch body in a blocking thread
        let relative_path = tokio::task::spawn_blocking(move || {
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
        .map_err(|e| Error::Other(format!("Body fetch panicked: {}", e)))??;

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
