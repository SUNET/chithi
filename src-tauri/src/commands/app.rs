use tauri::{AppHandle, State};

use crate::db;
use crate::error::Result;
use crate::state::AppState;

/// Cleanly exit the application from any window. The frontend uses this for
/// the `File > Quit` menu item and the `Ctrl+Q` shortcut. Closing a single
/// window is handled directly in the renderer via `getCurrentWindow().close()`.
#[tauri::command]
pub fn quit_app(app: AppHandle) {
    log::info!("Quit requested via menu/shortcut");
    app.exit(0);
}

/// Fetch the account/folder the user was last viewing, so the frontend can
/// restore it on startup (#191). Either field is `None` on a fresh install.
#[tauri::command]
pub async fn get_last_view(state: State<'_, AppState>) -> Result<db::settings::LastView> {
    let conn = state.db.reader();
    db::settings::get_last_view(&conn)
}

/// Persist the account/folder the user is currently viewing (#191). The
/// frontend calls this debounced, on every folder/account navigation.
#[tauri::command]
pub async fn save_last_view(
    state: State<'_, AppState>,
    account_id: String,
    folder_path: String,
) -> Result<()> {
    let conn = state.db.writer().await;
    db::settings::save_last_view(&conn, &account_id, &folder_path)
}
