use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
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
    /// Per-account mail-sync-in-progress flags. If true, a mail sync is
    /// running and new mail sync requests for that account should be
    /// skipped. Kept separate from calendar so the two domains don't block
    /// each other.
    pub sync_in_progress: std::sync::Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Per-account calendar-sync-in-progress flags. Same shape and intent
    /// as `sync_in_progress` but for `sync_calendars`, so overlapping
    /// calendar-sync triggers (toolbar button + periodic tick + context
    /// menu) don't race on DB writes and emit out-of-order
    /// `calendar-sync-*` events for the same account.
    pub calendar_sync_in_progress: std::sync::Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Per-account operation queue senders. Workers are spawned lazily on
    /// first use and hold persistent connections for their protocol.
    pub op_senders: std::sync::Mutex<HashMap<String, tokio::sync::mpsc::Sender<OpEntry>>>,
    pub data_dir: PathBuf,
    /// Token -> canonical file path for attachments picked via the native
    /// dialog. The renderer only ever sees the token, so a compromised
    /// renderer cannot ask the backend to read arbitrary files.
    pub attachments: std::sync::Mutex<HashMap<String, PathBuf>>,
    /// In-flight Matrix SSO sessions, keyed by the local-port the
    /// frontend will pass back to `meet_matrix_login_complete`.
    /// Each entry carries the bound `TcpListener`, a creation
    /// timestamp (so abandoned flows are evicted on next insert),
    /// and the random state nonce that the legitimate SSO
    /// callback must echo back. (#148)
    pub matrix_sso_listeners: std::sync::Mutex<HashMap<u16, MatrixSsoSession>>,
    /// In-flight Zoom OAuth sessions. Same shape as the Matrix
    /// SSO map but additionally carries the PKCE code verifier
    /// (Zoom is a public OAuth client and uses PKCE rather than
    /// a client_secret). (#148)
    pub zoom_oauth_sessions: std::sync::Mutex<HashMap<u16, ZoomOAuthSession>>,
    /// Shared OpenPGP keystore (~/.tumpa/keys.db by default, overridable
    /// with $TUMPA_DIR / $TUMPA_KEYSTORE). Lazily opened on first use so a
    /// broken or missing keystore directory doesn't block app startup.
    /// Mutex-wrapped because `libtumpa::KeyStore` holds a `rusqlite::
    /// Connection`, which is `Send + !Sync`.
    pgp_store: OnceLock<Arc<std::sync::Mutex<libtumpa::KeyStore>>>,
    /// In-memory passphrase / card-PIN cache. Values are `Zeroizing<String>`
    /// and zero on drop. There is no background sweeper: entries live for
    /// the lifetime of the process unless explicitly evicted via
    /// `pgp::evict_cached_secret` (or the `pgp_forget_all` command).
    pub pgp_cache: Arc<std::sync::Mutex<libtumpa::cache::CredentialCache>>,
    /// Pending secret-prompt one-shots, keyed by the request id emitted to
    /// the frontend on the `pgp-secret-needed` event.
    pub pgp_pending_secrets: Arc<std::sync::Mutex<HashMap<String, PendingSecret>>>,
}

/// A `pgp-secret-needed` request waiting for the frontend to call
/// `pgp_provide_secret` (or `pgp_cancel_secret`). The held value is a
/// `Zeroizing<String>` so any user-supplied secret zeroes on drop even if
/// the consumer panics before consuming it.
pub struct PendingSecret {
    pub tx: tokio::sync::oneshot::Sender<Option<zeroize::Zeroizing<String>>>,
    /// The secret target this prompt was raised for — a key fingerprint
    /// (for passphrases) or a card ident (for PINs). Used by
    /// `pgp_provide_secret` to populate the credential cache when the
    /// "remember" checkbox is set.
    pub target: String,
}

/// One in-flight Matrix SSO login. Lives in
/// `AppState.matrix_sso_listeners` between the `_start` and
/// `_complete` Tauri commands.
pub struct MatrixSsoSession {
    pub created: std::time::Instant,
    pub listener: std::net::TcpListener,
    pub state: String,
}

/// One in-flight Zoom OAuth login. Same shape as the Matrix
/// session but with the PKCE code verifier alongside the state
/// nonce, since Zoom is a public OAuth client.
pub struct ZoomOAuthSession {
    pub created: std::time::Instant,
    pub listener: std::net::TcpListener,
    pub verifier: Option<String>,
    pub state: String,
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

        // Revive any outbox rows left in 'sending' from a previous run.
        // The spawn task that owned them is dead; without this the rows
        // would never be retried and would be invisible to the user.
        {
            let revive_conn = rusqlite::Connection::open(&db_path)?;
            if let Err(e) = crate::ops::offline::revive_stuck_sending(&revive_conn) {
                log::warn!("Failed to revive stuck 'sending' outbox rows: {}", e);
            }
        }

        Ok(Self {
            db: Arc::new(pool),
            sync_handles: RwLock::new(HashMap::new()),
            idle_handles: std::sync::Mutex::new(HashMap::new()),
            jmap_push_handles: std::sync::Mutex::new(HashMap::new()),
            sync_in_progress: std::sync::Mutex::new(HashMap::new()),
            calendar_sync_in_progress: std::sync::Mutex::new(HashMap::new()),
            op_senders: std::sync::Mutex::new(HashMap::new()),
            data_dir,
            attachments: std::sync::Mutex::new(HashMap::new()),
            matrix_sso_listeners: std::sync::Mutex::new(HashMap::new()),
            zoom_oauth_sessions: std::sync::Mutex::new(HashMap::new()),
            pgp_store: OnceLock::new(),
            pgp_cache: Arc::new(std::sync::Mutex::new(
                libtumpa::cache::CredentialCache::new(),
            )),
            pgp_pending_secrets: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    /// Return the shared OpenPGP keystore, opening it on first call. The
    /// keystore lives at `~/.tumpa/keys.db` by default — same database that
    /// tumpa-cli and the tumpa desktop app read and write.
    pub fn pgp_store(
        &self,
    ) -> std::result::Result<Arc<std::sync::Mutex<libtumpa::KeyStore>>, libtumpa::Error> {
        if let Some(store) = self.pgp_store.get() {
            return Ok(store.clone());
        }
        let store = Arc::new(std::sync::Mutex::new(libtumpa::store::open_keystore(None)?));
        // OnceLock::set takes ownership; ignore the Err(_) branch — it only
        // fires when another caller raced us and already installed a store,
        // in which case `get()` below returns the winner.
        let _ = self.pgp_store.set(store);
        Ok(self.pgp_store.get().expect("just initialised").clone())
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
