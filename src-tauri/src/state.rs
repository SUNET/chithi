use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio_util::sync::CancellationToken;

use crate::db;
use crate::db::pool::DbPool;
use crate::error::{Error, Result};
use crate::ops::lifecycle::WorkerRegistry;
use crate::ops::queue::OpEntry;
use crate::ops::worker::AccountWorker;

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

/// Weak per-lifecycle lock registry. Entries cannot outlive their callers,
/// and one registry mutex makes lookup-or-create atomic.
#[derive(Default)]
pub struct MeetLifecycleCoordinator {
    locks: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

/// Weak per-account lock registry for meeting creation and account mutation.
/// Meeting callers acquire their lifecycle lock first; all callers acquire
/// this before any provider credential lock.
#[derive(Default)]
pub struct AccountLifecycleCoordinator {
    locks: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

impl AccountLifecycleCoordinator {
    pub fn acquire(&self, account_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|error| error.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(account_id).and_then(Weak::upgrade) {
            return lock;
        }

        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(account_id.to_string(), Arc::downgrade(&lock));
        lock
    }
}

impl MeetLifecycleCoordinator {
    pub fn acquire(&self, lifecycle_id: &str) -> Result<Arc<tokio::sync::Mutex<()>>> {
        uuid::Uuid::parse_str(lifecycle_id).map_err(|_| {
            crate::error::Error::Other("meeting lifecycle id must be a UUID".into())
        })?;

        let mut locks = self.locks.lock().unwrap_or_else(|error| error.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(lifecycle_id).and_then(Weak::upgrade) {
            return Ok(lock);
        }

        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(lifecycle_id.to_string(), Arc::downgrade(&lock));
        Ok(lock)
    }
}

pub struct AppState {
    pub db: Arc<DbPool>,
    pub providers: Arc<crate::provider::ProviderServices>,
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
    _data_dir_lock: File,
    /// Token -> canonical file path for attachments picked via the native
    /// dialog. The renderer only ever sees the token, so a compromised
    /// renderer cannot ask the backend to read arbitrary files.
    pub attachments: std::sync::Mutex<HashMap<String, PathBuf>>,
    /// In-flight Nextcloud Talk Login Flow v2 sessions. Poll endpoints and
    /// tokens remain backend-only; the renderer receives only the map key.
    pub talk_login_sessions: std::sync::Mutex<HashMap<String, TalkLoginSession>>,
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
    /// Serializes claims and cleanup for each durable meeting lifecycle.
    pub meet_lifecycle: MeetLifecycleCoordinator,
    /// Serializes account mutation with remote meeting create/cleanup.
    pub account_lifecycle: AccountLifecycleCoordinator,
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
    pub homeserver: String,
    pub listener: std::net::TcpListener,
    pub state: String,
}

pub struct TalkLoginSession {
    pub created: std::time::Instant,
    pub flow: crate::meet::talk::LoginFlowStart,
}

/// One in-flight Zoom OAuth login. Same shape as the Matrix
/// session but with the PKCE code verifier alongside the state
/// nonce, since Zoom is a public OAuth client.
pub struct ZoomOAuthSession {
    pub created: std::time::Instant,
    pub listener: std::net::TcpListener,
    pub verifier: Option<String>,
    pub state: String,
    /// Existing account being reauthenticated, fixed when OAuth starts so a
    /// renderer cannot redirect the completed authorization to another row.
    pub account_id: Option<String>,
}

impl AppState {
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir)?;
        let lock_path = data_dir.join(".chithi.lock");
        let data_dir_lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        match data_dir_lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(Error::Other(format!(
                    "Cannot start Chithi: data directory '{}' is already in use by another Chithi instance (lock file '{}')",
                    data_dir.display(),
                    lock_path.display()
                )));
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let db_path = data_dir.join("chithi.db");

        // Initialize schema on a temporary connection
        let init_conn = rusqlite::Connection::open(&db_path)?;
        db::schema::initialize(&init_conn)?;
        drop(init_conn);

        // Create pool: 1 writer + 4 readers (matches MAX_PARALLEL_CONNECTIONS)
        let pool = DbPool::new(&db_path, 4)?;

        // Quarantine outbox rows left in 'sending' by a previous run. Their
        // delivery outcome is unknowable after the owning task disappears,
        // so only a deliberate manual retry may send them again.
        {
            let quarantine_conn = rusqlite::Connection::open(&db_path)?;
            if let Err(e) = crate::ops::offline::quarantine_stuck_sending(&quarantine_conn) {
                log::warn!("Failed to quarantine stuck 'sending' outbox rows: {}", e);
            }
        }

        Ok(Self {
            db: Arc::new(pool),
            providers: Arc::new(crate::provider::ProviderServices::production()?),
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
            _data_dir_lock: data_dir_lock,
            attachments: std::sync::Mutex::new(HashMap::new()),
            talk_login_sessions: std::sync::Mutex::new(HashMap::new()),
            matrix_sso_listeners: std::sync::Mutex::new(HashMap::new()),
            zoom_oauth_sessions: std::sync::Mutex::new(HashMap::new()),
            meet_lifecycle: MeetLifecycleCoordinator::default(),
            account_lifecycle: AccountLifecycleCoordinator::default(),
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
    use super::{AccountLifecycleCoordinator, AppState, IdleControl, MeetLifecycleCoordinator};
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn second_app_state_cannot_run_startup_quarantine() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().to_path_buf();
        let first_state = AppState::new(data_dir.clone()).unwrap();
        let db_path = data_dir.join("chithi.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('lock-test', 'Lock Test', 'lock@example.com', 'lock-test')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO outbox (account_id, action_type, payload_json, status)
             VALUES ('lock-test', 'send', '{}', 'sending')",
            [],
        )
        .unwrap();
        let outbox_id = conn.last_insert_rowid();
        drop(conn);

        let error = match AppState::new(data_dir.clone()) {
            Ok(state) => {
                drop(state);
                panic!("a second AppState acquired the same data directory lock");
            }
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(
            message.contains("already in use by another Chithi instance"),
            "unexpected startup error: {message}"
        );
        assert!(message.contains(".chithi.lock"));

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM outbox WHERE id = ?1",
                [outbox_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "sending");
        drop(conn);
        drop(first_state);
    }

    #[test]
    fn dropping_app_state_releases_data_dir_lock() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().to_path_buf();
        let first_state = AppState::new(data_dir.clone()).unwrap();
        drop(first_state);

        let later_state = AppState::new(data_dir.clone()).unwrap();
        assert_eq!(later_state.data_dir, data_dir);
        drop(later_state);
    }

    #[test]
    fn app_states_for_different_data_dirs_can_coexist() {
        let first_temp = tempfile::tempdir().unwrap();
        let second_temp = tempfile::tempdir().unwrap();
        let first_state = AppState::new(first_temp.path().to_path_buf()).unwrap();
        let second_state = AppState::new(second_temp.path().to_path_buf()).unwrap();

        assert_ne!(first_state.data_dir, second_state.data_dir);
        drop(second_state);
        drop(first_state);
    }

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

    #[test]
    fn meet_lifecycle_rejects_malformed_ids_without_inserting() {
        let coordinator = MeetLifecycleCoordinator::default();
        assert!(coordinator.acquire("renderer-controlled").is_err());
        assert!(coordinator.locks.lock().unwrap().is_empty());
    }

    #[test]
    fn meet_lifecycle_prunes_sequential_stale_entries() {
        let coordinator = MeetLifecycleCoordinator::default();
        for _ in 0..32 {
            let id = uuid::Uuid::new_v4().to_string();
            drop(coordinator.acquire(&id).unwrap());
        }

        // Each acquisition prunes the dead entry from the preceding one.
        assert_eq!(coordinator.locks.lock().unwrap().len(), 1);
    }

    #[test]
    fn concurrent_meet_lifecycle_acquisitions_share_one_lock() {
        let coordinator = Arc::new(MeetLifecycleCoordinator::default());
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let id = uuid::Uuid::new_v4().to_string();
        let mut handles = Vec::new();
        for _ in 0..2 {
            let coordinator = coordinator.clone();
            let barrier = barrier.clone();
            let id = id.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let lock = coordinator.acquire(&id).unwrap();
                barrier.wait();
                lock
            }));
        }
        barrier.wait();
        barrier.wait();
        let first = handles.remove(0).join().unwrap();
        let second = handles.remove(0).join().unwrap();

        assert!(Arc::ptr_eq(&first, &second));
        let guard = first.try_lock().unwrap();
        assert!(second.try_lock().is_err());
        drop(guard);
    }

    #[test]
    fn account_lifecycle_prunes_stale_entries() {
        let coordinator = AccountLifecycleCoordinator::default();
        for index in 0..32 {
            drop(coordinator.acquire(&format!("account-{index}")));
        }
        assert_eq!(coordinator.locks.lock().unwrap().len(), 1);
    }

    #[test]
    fn concurrent_account_lifecycle_acquisitions_share_one_lock() {
        let coordinator = Arc::new(AccountLifecycleCoordinator::default());
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let coordinator = coordinator.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let lock = coordinator.acquire("database-account");
                barrier.wait();
                lock
            }));
        }
        barrier.wait();
        barrier.wait();
        let first = handles.remove(0).join().unwrap();
        let second = handles.remove(0).join().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn lifecycle_hierarchy_excludes_account_mutation_during_create() {
        let meet_coordinator = MeetLifecycleCoordinator::default();
        let account_coordinator = AccountLifecycleCoordinator::default();
        let lifecycle_id = uuid::Uuid::new_v4().to_string();
        let lifecycle_lock = meet_coordinator.acquire(&lifecycle_id).unwrap();
        let _lifecycle_guard = lifecycle_lock.lock().await;
        let create_lock = account_coordinator.acquire("account");
        let create_guard = create_lock.lock().await;
        let update_lock = account_coordinator.acquire("account");
        let deletion_lock = account_coordinator.acquire("account");
        assert!(update_lock.try_lock().is_err());
        assert!(deletion_lock.try_lock().is_err());
        drop(create_guard);
        assert!(update_lock.try_lock().is_ok());
        assert!(deletion_lock.try_lock().is_ok());
    }
}
