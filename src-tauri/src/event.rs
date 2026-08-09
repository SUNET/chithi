use std::sync::Arc;

pub mod tauri;

/// Application events emitted by services and delivered by an adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplicationEvent {
    SyncStarted(SyncStarted),
    SyncProgress(SyncProgress),
    SyncComplete(SyncComplete),
    SyncError(SyncError),
    FoldersChanged(String),
    MessagesChanged(String),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SyncStarted {
    pub account_id: String,
    pub account_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SyncProgress {
    pub account_id: String,
    pub folder: String,
    pub synced: u32,
    pub total_folders: usize,
    pub current_folder: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SyncComplete {
    pub account_id: String,
    pub total_synced: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SyncError {
    pub account_id: String,
    pub error: String,
}

/// Best-effort delivery boundary for application events.
pub trait EventSink: Send + Sync {
    fn publish(&self, event: ApplicationEvent);
}

pub type SharedEventSink = Arc<dyn EventSink>;

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct RecordingEventSink {
        events: Mutex<Vec<ApplicationEvent>>,
    }

    impl EventSink for RecordingEventSink {
        fn publish(&self, event: ApplicationEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[test]
    fn shared_sink_can_publish_across_threads() {
        let recording = Arc::new(RecordingEventSink::default());
        let sink: SharedEventSink = recording.clone();
        let thread_sink = sink.clone();
        std::thread::spawn(move || {
            thread_sink.publish(ApplicationEvent::MessagesChanged("account-1".into()));
        })
        .join()
        .unwrap();

        sink.publish(ApplicationEvent::FoldersChanged("account-1".into()));

        assert_eq!(
            *recording.events.lock().unwrap(),
            vec![
                ApplicationEvent::MessagesChanged("account-1".into()),
                ApplicationEvent::FoldersChanged("account-1".into()),
            ]
        );
    }
}
