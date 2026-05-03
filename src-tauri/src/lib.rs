#![allow(dead_code)]
#![allow(clippy::too_many_arguments)]

mod calendar;
mod commands;
mod db;
mod error;
mod filters;
mod keyring;
mod logging;
mod mail;
mod oauth;
mod ops;
mod path_validation;
mod state;

use state::AppState;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_os::init())
        .setup(|app| {
            let data_dir = resolve_data_dir(app.handle())?;
            std::fs::create_dir_all(&data_dir)?;

            logging::init(&data_dir)?;
            log::info!("Starting Emails application");
            log::info!("Data directory: {}", data_dir.display());

            oauth::init_token_store(&data_dir)?;

            let app_state = AppState::new(data_dir)?;
            app.manage(app_state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::app::quit_app,
            commands::accounts::list_accounts,
            commands::accounts::add_account,
            commands::accounts::get_account_config,
            commands::accounts::update_account,
            commands::accounts::delete_account,
            commands::accounts::probe_dav_endpoints,
            commands::mail::list_folders,
            commands::mail::get_messages,
            commands::mail::search_messages_server,
            commands::mail::import_search_hit,
            commands::mail::get_message_body,
            commands::mail::get_message_html_with_images,
            commands::mail::get_threaded_messages,
            commands::mail::get_thread_messages,
            commands::mail::unthread_message,
            commands::mail::create_folder,
            commands::mail::delete_folder,
            commands::mail::save_attachment,
            commands::sync_cmd::trigger_sync,
            commands::sync_cmd::sync_folder,
            commands::sync_cmd::get_sync_status,
            commands::sync_cmd::prefetch_bodies,
            commands::compose::send_message,
            commands::compose::save_draft,
            commands::attachments::pick_attachments,
            commands::attachments::release_attachment,
            commands::contacts::list_contact_books,
            commands::contacts::list_contacts,
            commands::contacts::get_contact,
            commands::contacts::create_contact,
            commands::contacts::update_contact,
            commands::contacts::delete_contact,
            commands::contacts::search_contacts,
            commands::contacts::search_collected_contacts,
            commands::contacts::sync_contacts,
            commands::sync_cmd::start_idle,
            commands::sync_cmd::stop_idle,
            commands::oauth::oauth_start,
            commands::oauth::oauth_complete,
            commands::oauth::oauth_get_token,
            commands::oauth::oauth_has_tokens,
            commands::oauth::oauth_get_ms_profile,
            commands::oauth::jmap_oidc_start,
            commands::oauth::jmap_oidc_complete,
            commands::oauth::open_oauth_url,
            commands::actions::move_messages,
            commands::actions::move_messages_cross_account,
            commands::actions::delete_messages,
            commands::actions::set_message_flags,
            commands::actions::copy_messages,
            commands::actions::mark_account_read,
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
            commands::calendar::process_invite_reply,
            commands::calendar::process_cancelled_invite,
            commands::calendar::unsubscribe_calendar,
            commands::calendar::sync_calendars,
            commands::calendar::list_timezones,
            commands::calendar::get_default_timezone,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn resolve_data_dir(
    app: &tauri::AppHandle,
) -> std::result::Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    // Desktop: keep the previous `~/.local/share/chithi` layout.
    // Mobile: use the sandboxed app-data dir from Tauri's path resolver — only
    // that location is writable inside the app sandbox on Android/iOS.
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        return Ok(app.path().app_data_dir()?);
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let _ = app;
        let base = dirs::data_local_dir().unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            std::path::PathBuf::from(home).join(".local").join("share")
        });
        Ok(base.join("chithi"))
    }
}
