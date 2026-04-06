use tauri::State;

use crate::commands::calendar::random_calendar_color;
use crate::db;
use crate::db::calendar::NewCalendar;
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

    // Create a default calendar for the new account
    let cal_id = uuid::Uuid::new_v4().to_string();
    let default_calendar = NewCalendar {
        account_id: id.clone(),
        name: "Calendar".to_string(),
        color: random_calendar_color(),
        is_default: true,
    };
    db::calendar::insert_calendar(&conn, &cal_id, &default_calendar)?;
    log::info!("Default calendar created with id={} for account={}", cal_id, id);

    Ok(id)
}

#[tauri::command]
pub async fn get_account_config(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<db::accounts::AccountConfig> {
    log::debug!("Getting config for account {}", account_id);
    let conn = state.db.lock().await;
    let full = db::accounts::get_account_full(&conn, &account_id)?;
    Ok(db::accounts::AccountConfig {
        display_name: full.display_name,
        email: full.email,
        provider: full.provider,
        mail_protocol: full.mail_protocol,
        imap_host: full.imap_host,
        imap_port: full.imap_port,
        smtp_host: full.smtp_host,
        smtp_port: full.smtp_port,
        jmap_url: full.jmap_url,
        caldav_url: full.caldav_url,
        username: full.username,
        password: full.password,
        use_tls: full.use_tls,
        signature: full.signature,
    })
}

#[tauri::command]
pub async fn update_account(
    state: State<'_, AppState>,
    account_id: String,
    config: db::accounts::AccountConfig,
) -> Result<()> {
    log::info!("Updating account {} ({})", account_id, config.email);
    let conn = state.db.lock().await;
    db::accounts::update_account(&conn, &account_id, &config)?;
    Ok(())
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
