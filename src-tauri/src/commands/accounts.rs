use tauri::State;

use crate::db;
use crate::error::Result;
use crate::state::AppState;

#[tauri::command]
pub async fn list_accounts(
    state: State<'_, AppState>,
) -> Result<Vec<db::accounts::Account>> {
    log::debug!("Listing accounts");
    let conn = state.db.lock().await;
    let accounts = db::accounts::list_accounts(&conn)?;
    log::debug!("Found {} accounts", accounts.len());
    Ok(accounts)
}

#[tauri::command]
pub async fn add_account(
    state: State<'_, AppState>,
    config: db::accounts::AccountConfig,
) -> Result<String> {
    log::info!(
        "Adding account: {} ({}) provider={} imap={}:{}",
        config.display_name,
        config.email,
        config.provider,
        config.imap_host,
        config.imap_port,
    );
    let id = uuid::Uuid::new_v4().to_string();
    let conn = state.db.lock().await;
    db::accounts::insert_account(&conn, &id, &config)?;
    log::info!("Account created with id={}", id);
    Ok(id)
}

#[tauri::command]
pub async fn delete_account(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<()> {
    log::info!("Deleting account {}", account_id);
    let conn = state.db.lock().await;
    db::accounts::delete_account(&conn, &account_id)?;
    // Also remove Maildir
    let maildir_path = state.data_dir.join(&account_id);
    if maildir_path.exists() {
        log::info!("Removing maildir at {}", maildir_path.display());
        std::fs::remove_dir_all(&maildir_path).ok();
    }
    log::info!("Account {} deleted", account_id);
    Ok(())
}
