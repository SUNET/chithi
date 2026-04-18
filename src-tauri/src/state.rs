use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::db;
use crate::db::pool::DbPool;
use crate::error::Result;
use crate::ops::queue::OpEntry;
use crate::ops::worker::AccountWorker;

pub struct SyncHandle {
    pub abort_handle: tokio::task::AbortHandle,
}

/// Handle for a running IMAP IDLE loop thread.
pub struct IdleHandle {
    pub stop_flag: Arc<AtomicBool>,
    pub thread: Option<std::thread::JoinHandle<()>>,
}

/// Handle for a running JMAP EventSource push task.
pub struct JmapPushHandle {
    pub stop_flag: Arc<AtomicBool>,
    pub task: tokio::task::JoinHandle<()>,
}

pub struct AppState {
    pub db: Arc<DbPool>,
    pub sync_handles: RwLock<HashMap<String, SyncHandle>>,
    pub idle_handles: std::sync::Mutex<HashMap<String, IdleHandle>>,
    pub jmap_push_handles: std::sync::Mutex<HashMap<String, JmapPushHandle>>,
    /// Per-account sync-in-progress flags. If true, a sync is running and
    /// new sync requests for that account should be skipped.
    pub sync_in_progress: std::sync::Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Per-account operation queue senders. Workers are spawned lazily on
    /// first use and hold persistent connections for their protocol.
    pub op_senders: std::sync::Mutex<HashMap<String, tokio::sync::mpsc::Sender<OpEntry>>>,
    pub data_dir: PathBuf,
    /// Token -> canonical file path for attachments picked via the native
    /// dialog. The renderer only ever sees the token, so a compromised
    /// renderer cannot ask the backend to read arbitrary files.
    pub attachments: std::sync::Mutex<HashMap<String, PathBuf>>,
}

impl AppState {
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir)?;
        let db_path = data_dir.join("chithi.db");

        // Initialize schema on a temporary connection
        let init_conn = rusqlite::Connection::open(&db_path)?;
        db::schema::initialize(&init_conn)?;
        drop(init_conn);

        // Create pool: 1 writer + 4 readers (matches MAX_PARALLEL_CONNECTIONS)
        let pool = DbPool::new(&db_path, 4)?;

        Ok(Self {
            db: Arc::new(pool),
            sync_handles: RwLock::new(HashMap::new()),
            idle_handles: std::sync::Mutex::new(HashMap::new()),
            jmap_push_handles: std::sync::Mutex::new(HashMap::new()),
            sync_in_progress: std::sync::Mutex::new(HashMap::new()),
            op_senders: std::sync::Mutex::new(HashMap::new()),
            data_dir,
            attachments: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Get or create an operation queue sender for the given account.
    /// Spawns a worker task lazily on first use.
    pub fn get_op_sender(
        &self,
        account_id: &str,
        app: &tauri::AppHandle,
    ) -> tokio::sync::mpsc::Sender<OpEntry> {
        let mut senders = self.op_senders.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sender) = senders.get(account_id) {
            if !sender.is_closed() {
                return sender.clone();
            }
            // Channel closed (worker died) — remove and recreate
            senders.remove(account_id);
        }

        let (tx, rx) = tokio::sync::mpsc::channel::<OpEntry>(256);
        let worker = AccountWorker::new(account_id.to_string(), rx, self.db.clone(), app.clone());
        tokio::spawn(worker.run());
        senders.insert(account_id.to_string(), tx.clone());
        tx
    }
}
