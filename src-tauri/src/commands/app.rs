use tauri::AppHandle;

/// Cleanly exit the application from any window. The frontend uses this for
/// the `File > Quit` menu item and the `Ctrl+Q` shortcut. Closing a single
/// window is handled directly in the renderer via `getCurrentWindow().close()`.
#[tauri::command]
pub fn quit_app(app: AppHandle) {
    log::info!("Quit requested via menu/shortcut");
    app.exit(0);
}
