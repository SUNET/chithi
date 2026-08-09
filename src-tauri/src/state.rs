use std::collections::HashMap;
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::db;
use crate::db::pool::DbPool;
use crate::error::Result;
use crate::ops::lifecycle::WorkerRegistry;
use crate::ops::queue::OpEntry;
use crate::ops::worker::AccountWorker;

pub struct SyncHandle {
    pub abort_handle: tokio::task::AbortHandle,
}

/// Shared cancellation state for one generation of an IMAP IDLE loop.
pub struct IdleControl {
    stop_flag: AtomicBool,
    event_gate: Mutex<()>,
    socket: Mutex<Option<TcpStream>>,
}

impl IdleControl {
    pub fn new() -> Self {
        Self {
            stop_flag: AtomicBool::new(false),
            event_gate: Mutex::new(()),
            socket: Mutex::new(None),
        }
    }

    pub fn should_stop(&self) -> bool {
        self.stop_flag.load(Ordering::Acquire)
    }

    pub fn register_socket(&self, socket: &TcpStream) -> std::io::Result<()> {
        let clone = socket.try_clone()?;
        let mut active = self.socket.lock().unwrap();
        if self.should_stop() {
            let _ = clone.shutdown(Shutdown::Both);
        } else {
            *active = Some(clone);
        }
        Ok(())
    }

    pub fn clear_socket(&self) {
        self.socket.lock().unwrap().take();
    }

    pub fn request_stop(&self) {
        {
            let _event_gate = self.event_gate.lock().unwrap();
            self.stop_flag.store(true, Ordering::Release);
        }
        if let Some(socket) = self.socket.lock().unwrap().as_ref() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    pub fn if_running(&self, action: impl FnOnce()) {
        let _event_gate = self.event_gate.lock().unwrap();
        if !self.should_stop() {
            action();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdlePhase {
    Starting,
    Running,
    Stopping,
    Joining,
    StopFailed,
}

/// Lifecycle record for one generation of an IMAP IDLE loop.
pub struct IdleHandle {
    pub generation: u64,
    pub phase: IdlePhase,
    pub control: Arc<IdleControl>,
    pub thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JmapPushPhase {
    Starting,
    Running,
    Stopping,
    Joining,
    StopFailed,
}

/// Lifecycle record for one generation of a JMAP EventSource push task.
pub struct JmapPushHandle {
    pub generation: u64,
    pub phase: JmapPushPhase,
    pub cancellation: CancellationToken,
    pub event_gate: Arc<Mutex<()>>,
    pub task: Option<tokio::task::JoinHandle<()>>,
}

pub struct AppState {
    pub db: Arc<DbPool>,
    pub providers: Arc<crate::provider::ProviderServices>,
    pub sync_handles: RwLock<HashMap<String, SyncHandle>>,
    pub idle_handles: std::sync::Mutex<HashMap<String, IdleHandle>>,
    pub idle_generation: AtomicU64,
    pub idle_lifecycle_lock: Arc<tokio::sync::Mutex<()>>,
    pub idle_push_enabled: AtomicBool,
    pub idle_account_locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pub jmap_push_generation: AtomicU64,
    pub jmap_push_handles: std::sync::Mutex<HashMap<String, JmapPushHandle>>,
    /// Per-account mail-sync-in-progress flags. If true, a mail sync or
    /// prefetch is running and new mail sync requests for that account are
    /// deferred until the current guarded operation releases the account.
    /// Kept separate from calendar so the two domains don't block each other.
    pub sync_in_progress: std::sync::Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Per-account mail sync requests that arrived while `sync_in_progress`
    /// was held. This keeps IMAP IDLE notifications from being dropped during
    /// long O365 body-prefetch passes.
    pub pending_mail_sync: std::sync::Mutex<HashMap<String, Option<String>>>,
    /// Per-account calendar-sync-in-progress flags. Same shape and intent
    /// as `sync_in_progress` but for `sync_calendars`, so overlapping
    /// calendar-sync triggers (toolbar button + periodic tick + context
    /// menu) don't race on DB writes and emit out-of-order
    /// `calendar-sync-*` events for the same account.
    pub calendar_sync_in_progress: std::sync::Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Per-account operation workers, including their shutdown controls and
    /// join handles. Workers are spawned lazily on first use.
    op_workers: WorkerRegistry<OpEntry>,
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
///
/// Caching the collected secret (in the `tcli` agent or the in-process
/// `CredentialCache`) is owned entirely by `acquire_secret` — this struct
/// is now just the oneshot back-channel for the prompt.
pub struct PendingSecret {
    pub tx: tokio::sync::oneshot::Sender<Option<zeroize::Zeroizing<String>>>,
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
            providers: Arc::new(crate::provider::ProviderServices::production()?),
            sync_handles: RwLock::new(HashMap::new()),
            idle_handles: std::sync::Mutex::new(HashMap::new()),
            idle_generation: AtomicU64::new(1),
            idle_lifecycle_lock: Arc::new(tokio::sync::Mutex::new(())),
            idle_push_enabled: AtomicBool::new(false),
            idle_account_locks: std::sync::Mutex::new(HashMap::new()),
            jmap_push_generation: AtomicU64::new(1),
            jmap_push_handles: std::sync::Mutex::new(HashMap::new()),
            sync_in_progress: std::sync::Mutex::new(HashMap::new()),
            pending_mail_sync: std::sync::Mutex::new(HashMap::new()),
            calendar_sync_in_progress: std::sync::Mutex::new(HashMap::new()),
            op_workers: WorkerRegistry::new(256),
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
    pub async fn get_op_sender(
        &self,
        account_id: &str,
        app: &tauri::AppHandle,
    ) -> tokio::sync::mpsc::Sender<OpEntry> {
        let worker_account_id = account_id.to_string();
        let db = self.db.clone();
        let app = app.clone();
        match self
            .op_workers
            .get_or_spawn(account_id, move |receiver, cancellation| {
                AccountWorker::new(worker_account_id, receiver, db, app)
                    .spawn_supervised(cancellation)
            })
            .await
        {
            Ok(sender) => sender,
            Err(message) => {
                log::warn!(
                    "Operation worker unavailable for account {}: {}",
                    account_id,
                    message
                );
                let (sender, receiver) = tokio::sync::mpsc::channel(1);
                drop(receiver);
                sender
            }
        }
    }

    /// Gracefully stop and join an account's operation worker, if present.
    pub async fn stop_op_worker(&self, account_id: &str) {
        if let Some(outcome) = self.op_workers.stop_account(account_id).await {
            log::info!(
                "Stopped operation worker for account {}: {:?}",
                account_id,
                outcome
            );
        }
    }

    /// Keep replacement workers excluded while mutating account state.
    pub async fn with_op_worker_stopped<R, F, Fut>(&self, account_id: &str, action: F) -> R
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = R>,
    {
        self.op_workers
            .with_account_stopped(account_id, action)
            .await
    }

    /// Stop accepting operation work and join every account worker.
    pub async fn stop_all_op_workers(&self) {
        for (account_id, outcome) in self.op_workers.stop_all().await {
            log::info!(
                "Stopped operation worker for account {} during shutdown: {:?}",
                account_id,
                outcome
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::IdleControl;
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn idle_control_interrupts_registered_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_server, _) = listener.accept().unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();

        let control = IdleControl::new();
        control.register_socket(&client).unwrap();
        control.request_stop();

        let mut client = client;
        let mut byte = [0_u8; 1];
        assert_eq!(client.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn idle_control_closes_socket_registered_after_stop() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_server, _) = listener.accept().unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();

        let control = IdleControl::new();
        control.request_stop();
        control.register_socket(&client).unwrap();

        let mut client = client;
        let mut byte = [0_u8; 1];
        assert_eq!(client.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn idle_control_suppresses_events_after_stop() {
        let emitted = AtomicBool::new(false);
        let control = IdleControl::new();
        control.request_stop();

        control.if_running(|| emitted.store(true, Ordering::Relaxed));

        assert!(!emitted.load(Ordering::Relaxed));
    }
}
