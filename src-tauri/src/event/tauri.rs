use tauri::Emitter;

use super::{ApplicationEvent, EventSink, SharedEventSink};

/// Delivers application events through the Tauri frontend adapter.
pub struct TauriEventSink {
    app: tauri::AppHandle,
}

impl TauriEventSink {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self { app }
    }
}

impl EventSink for TauriEventSink {
    fn publish(&self, event: ApplicationEvent) {
        let (name, payload) = frontend_contract(event);
        self.app.emit(name, payload).ok();
    }
}

pub fn shared_sink(app: tauri::AppHandle) -> SharedEventSink {
    std::sync::Arc::new(TauriEventSink::new(app))
}

/// Emit a `folders-changed` event from a Tauri adapter boundary.
pub fn emit_folders_changed(app: &tauri::AppHandle, account_id: &str) {
    TauriEventSink::new(app.clone())
        .publish(ApplicationEvent::FoldersChanged(account_id.to_string()));
}

/// Emit a `messages-changed` event from a Tauri adapter boundary.
pub fn emit_messages_changed(app: &tauri::AppHandle, account_id: &str) {
    TauriEventSink::new(app.clone())
        .publish(ApplicationEvent::MessagesChanged(account_id.to_string()));
}

fn frontend_contract(event: ApplicationEvent) -> (&'static str, serde_json::Value) {
    match event {
        ApplicationEvent::SyncStarted(payload) => (
            "sync-started",
            serde_json::json!({
                "account_id": payload.account_id,
                "account_name": payload.account_name,
            }),
        ),
        ApplicationEvent::SyncProgress(payload) => (
            "sync-progress",
            serde_json::json!({
                "account_id": payload.account_id,
                "folder": payload.folder,
                "synced": payload.synced,
                "total_folders": payload.total_folders,
                "current_folder": payload.current_folder,
            }),
        ),
        ApplicationEvent::SyncComplete(payload) => (
            "sync-complete",
            serde_json::json!({
                "account_id": payload.account_id,
                "total_synced": payload.total_synced,
            }),
        ),
        ApplicationEvent::SyncError(payload) => (
            "sync-error",
            serde_json::json!({
                "account_id": payload.account_id,
                "error": payload.error,
            }),
        ),
        ApplicationEvent::FoldersChanged(account_id) => {
            ("folders-changed", serde_json::Value::String(account_id))
        }
        ApplicationEvent::MessagesChanged(account_id) => {
            ("messages-changed", serde_json::Value::String(account_id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{SyncComplete, SyncError, SyncProgress, SyncStarted};

    #[test]
    fn maps_events_to_existing_frontend_contracts() {
        let cases = [
            (
                ApplicationEvent::SyncStarted(SyncStarted {
                    account_id: "a1".into(),
                    account_name: "Primary".into(),
                }),
                "sync-started",
                serde_json::json!({"account_id": "a1", "account_name": "Primary"}),
            ),
            (
                ApplicationEvent::SyncProgress(SyncProgress {
                    account_id: "a1".into(),
                    folder: "Inbox".into(),
                    synced: 2,
                    total_folders: 4,
                    current_folder: 1,
                }),
                "sync-progress",
                serde_json::json!({
                    "account_id": "a1",
                    "folder": "Inbox",
                    "synced": 2,
                    "total_folders": 4,
                    "current_folder": 1,
                }),
            ),
            (
                ApplicationEvent::SyncComplete(SyncComplete {
                    account_id: "a1".into(),
                    total_synced: 3,
                }),
                "sync-complete",
                serde_json::json!({"account_id": "a1", "total_synced": 3}),
            ),
            (
                ApplicationEvent::SyncError(SyncError {
                    account_id: "a1".into(),
                    error: "offline".into(),
                }),
                "sync-error",
                serde_json::json!({"account_id": "a1", "error": "offline"}),
            ),
            (
                ApplicationEvent::FoldersChanged("a1".into()),
                "folders-changed",
                serde_json::json!("a1"),
            ),
            (
                ApplicationEvent::MessagesChanged("a1".into()),
                "messages-changed",
                serde_json::json!("a1"),
            ),
        ];

        for (event, expected_name, expected_payload) in cases {
            let (name, payload) = frontend_contract(event);
            assert_eq!(name, expected_name);
            assert_eq!(payload, expected_payload);
        }
    }
}
