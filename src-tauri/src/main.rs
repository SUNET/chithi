// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod calendar;
mod commands;
mod db;
mod error;
mod filters;
mod keyring;
mod logging;
mod mail;
mod state;

use state::AppState;

fn main() {
    let data_dir = dirs_data_dir().join("chithi");
    std::fs::create_dir_all(&data_dir).expect("Failed to create data directory");

    logging::init(&data_dir).expect("Failed to initialize logging");

    log::info!("Starting Emails application");
    log::info!("Data directory: {}", data_dir.display());

    let app_state = AppState::new(data_dir).expect("Failed to initialize application state");

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            commands::accounts::list_accounts,
            commands::accounts::add_account,
            commands::accounts::get_account_config,
            commands::accounts::update_account,
            commands::accounts::delete_account,
            commands::mail::list_folders,
            commands::mail::get_messages,
            commands::mail::get_message_body,
            commands::mail::get_threaded_messages,
            commands::mail::get_thread_messages,
            commands::mail::unthread_message,
            commands::sync_cmd::trigger_sync,
            commands::sync_cmd::sync_folder,
            commands::sync_cmd::get_sync_status,
            commands::sync_cmd::prefetch_bodies,
            commands::compose::send_message,
            commands::actions::move_messages,
            commands::actions::delete_messages,
            commands::actions::set_message_flags,
            commands::actions::copy_messages,
            commands::filters::list_filters,
            commands::filters::save_filter,
            commands::filters::delete_filter,
            commands::filters::apply_filters_to_folder,
            commands::calendar::list_calendars,
            commands::calendar::create_calendar,
            commands::calendar::update_calendar,
            commands::calendar::delete_calendar,
            commands::calendar::get_events,
            commands::calendar::create_event,
            commands::calendar::update_event,
            commands::calendar::delete_event,
            commands::calendar::get_email_invites,
            commands::calendar::get_invite_status,
            commands::calendar::respond_to_invite,
            commands::calendar::send_invites,
            commands::calendar::sync_calendars,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn dirs_data_dir() -> std::path::PathBuf {
    if let Some(dir) = dirs::data_local_dir() {
        return dir;
    }
    // Fallback
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(home).join(".local").join("share")
}
