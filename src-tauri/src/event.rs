use tauri::Emitter;

/// Emit a `folders-changed` event so the frontend refreshes folder lists/counts.
pub fn emit_folders_changed(app: &tauri::AppHandle, account_id: &str) {
    app.emit("folders-changed", account_id.to_string()).ok();
}

/// Emit a `messages-changed` event so the frontend refreshes the message list.
pub fn emit_messages_changed(app: &tauri::AppHandle, account_id: &str) {
    app.emit("messages-changed", account_id.to_string()).ok();
}
