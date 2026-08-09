use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerPhase {
    Running,
    Stopping,
    Joining,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerTaskExit {
    Completed,
    Panicked,
    Cancelled,
    SupervisorPanicked,
    SupervisorCancelled,
}

pub struct SpawnedWorker {
    pub task: JoinHandle<WorkerTaskExit>,
    pub ready: oneshot::Receiver<std::result::Result<(), String>>,
}

struct WorkerHandle<T> {
    generation: u64,
    phase: WorkerPhase,
    sender: Option<mpsc::Sender<T>>,
    cancellation: CancellationToken,
    task: Option<JoinHandle<WorkerTaskExit>>,
}

struct StoppingWorker {
    generation: u64,
    task: JoinHandle<WorkerTaskExit>,
}

/// Owns the complete lifecycle of one lazily spawned worker per account.
///
/// Lifecycle operations are serialized per account. A stopped or failed
/// generation is always joined before a replacement can be installed.
pub struct WorkerRegistry<T> {
    handles: Mutex<HashMap<String, WorkerHandle<T>>>,
    account_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    generation: AtomicU64,
    shutting_down: AtomicBool,
    shutdown: CancellationToken,
    shutdown_gate: RwLock<()>,
    channel_capacity: usize,
}

impl<T> WorkerRegistry<T>
where
    T: Send + 'static,
{
    pub fn new(channel_capacity: usize) -> Self {
        Self {
            handles: Mutex::new(HashMap::new()),
            account_locks: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(1),
            shutting_down: AtomicBool::new(false),
            shutdown: CancellationToken::new(),
            shutdown_gate: RwLock::new(()),
            channel_capacity,
        }
    }

    pub async fn get_or_spawn<F>(
        &self,
        account_id: &str,
        spawn: F,
    ) -> std::result::Result<mpsc::Sender<T>, String>
    where
        F: FnOnce(mpsc::Receiver<T>, CancellationToken) -> SpawnedWorker,
    {
        let _shutdown_guard = self.shutdown_gate.read().await;
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("application is shutting down".to_string());
        }

        let account_lock = self.account_lock(account_id);
        let _account_guard = account_lock.lock().await;

        let current_is_healthy = {
            let handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
            handles.get(account_id).is_some_and(|handle| {
                handle.phase == WorkerPhase::Running
                    && handle
                        .sender
                        .as_ref()
                        .is_some_and(|sender| !sender.is_closed())
                    && handle.task.as_ref().is_some_and(|task| !task.is_finished())
            })
        };

        if current_is_healthy {
            let handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
            return Ok(handles[account_id]
                .sender
                .as_ref()
                .expect("healthy worker has a sender")
                .clone());
        }

        if let Some(stopping) = self.begin_stop(account_id) {
            let _ = self.finish_stop(account_id, stopping).await;
        }

        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        let cancellation = self.shutdown.child_token();
        let (sender, receiver) = mpsc::channel(self.channel_capacity);
        let spawned = spawn(receiver, cancellation.clone());

        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                account_id.to_string(),
                WorkerHandle {
                    generation,
                    phase: WorkerPhase::Running,
                    sender: Some(sender.clone()),
                    cancellation,
                    task: Some(spawned.task),
                },
            );

        match spawned.ready.await {
            Ok(Ok(())) => {}
            Ok(Err(message)) => {
                if let Some(stopping) = self.begin_stop(account_id) {
                    let _ = self.finish_stop(account_id, stopping).await;
                }
                return Err(message);
            }
            Err(_) => {
                if let Some(stopping) = self.begin_stop(account_id) {
                    let _ = self.finish_stop(account_id, stopping).await;
                }
                return Err("worker stopped before initialization completed".to_string());
            }
        }

        Ok(sender)
    }

    pub async fn stop_account(&self, account_id: &str) -> Option<WorkerTaskExit> {
        let _shutdown_guard = self.shutdown_gate.read().await;
        let account_lock = self.account_lock(account_id);
        let _account_guard = account_lock.lock().await;

        let stopping = self.begin_stop(account_id)?;
        Some(self.finish_stop(account_id, stopping).await)
    }

    pub async fn with_account_stopped<R, F, Fut>(&self, account_id: &str, action: F) -> R
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = R>,
    {
        let _shutdown_guard = self.shutdown_gate.read().await;
        let account_lock = self.account_lock(account_id);
        let _account_guard = account_lock.lock().await;

        if let Some(stopping) = self.begin_stop(account_id) {
            let _ = self.finish_stop(account_id, stopping).await;
        }
        action().await
    }

    pub async fn stop_all(&self) -> Vec<(String, WorkerTaskExit)> {
        self.shutting_down.store(true, Ordering::Release);
        self.shutdown.cancel();
        let _shutdown_guard = self.shutdown_gate.write().await;

        let account_ids = {
            let handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
            handles.keys().cloned().collect::<Vec<_>>()
        };
        let mut stopping = Vec::with_capacity(account_ids.len());
        for account_id in account_ids {
            if let Some(worker) = self.begin_stop(&account_id) {
                stopping.push((account_id, worker));
            }
        }

        let mut outcomes = Vec::with_capacity(stopping.len());
        for (account_id, worker) in stopping {
            let outcome = self.finish_stop(&account_id, worker).await;
            outcomes.push((account_id, outcome));
        }
        outcomes
    }

    fn account_lock(&self, account_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.account_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(account_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn begin_stop(&self, account_id: &str) -> Option<StoppingWorker> {
        let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        let handle = handles.get_mut(account_id)?;
        handle.phase = WorkerPhase::Stopping;
        handle.cancellation.cancel();
        handle.sender.take();
        handle.phase = WorkerPhase::Joining;
        Some(StoppingWorker {
            generation: handle.generation,
            task: handle.task.take()?,
        })
    }

    async fn finish_stop(&self, account_id: &str, stopping: StoppingWorker) -> WorkerTaskExit {
        let outcome = match stopping.task.await {
            Ok(outcome) => outcome,
            Err(error) if error.is_panic() => WorkerTaskExit::SupervisorPanicked,
            Err(_) => WorkerTaskExit::SupervisorCancelled,
        };

        let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        if handles
            .get(account_id)
            .is_some_and(|handle| handle.generation == stopping.generation)
        {
            handles.remove(account_id);
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::{SpawnedWorker, WorkerRegistry, WorkerTaskExit};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{Barrier, Notify};

    fn ready_worker(task: tokio::task::JoinHandle<WorkerTaskExit>) -> SpawnedWorker {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        ready_tx.send(Ok(())).unwrap();
        SpawnedWorker {
            task,
            ready: ready_rx,
        }
    }

    #[tokio::test]
    async fn concurrent_acquisition_spawns_one_worker() {
        let registry = Arc::new(WorkerRegistry::<u8>::new(8));
        let spawn_count = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(9));
        let mut acquisitions = Vec::new();

        for _ in 0..8 {
            let registry = Arc::clone(&registry);
            let spawn_count = Arc::clone(&spawn_count);
            let barrier = Arc::clone(&barrier);
            acquisitions.push(tokio::spawn(async move {
                barrier.wait().await;
                registry
                    .get_or_spawn("account", move |mut receiver, cancellation| {
                        spawn_count.fetch_add(1, Ordering::SeqCst);
                        ready_worker(tokio::spawn(async move {
                            cancellation.cancelled().await;
                            receiver.close();
                            while receiver.recv().await.is_some() {}
                            WorkerTaskExit::Completed
                        }))
                    })
                    .await
                    .unwrap()
            }));
        }

        barrier.wait().await;
        for acquisition in acquisitions {
            let _sender = acquisition.await.unwrap();
        }

        assert_eq!(spawn_count.load(Ordering::SeqCst), 1);
        registry.stop_all().await;
    }

    #[tokio::test]
    async fn shutdown_drains_operations_accepted_before_receiver_close() {
        let registry = WorkerRegistry::<u8>::new(8);
        let processed = Arc::new(AtomicUsize::new(0));
        let worker_processed = Arc::clone(&processed);
        let sender = registry
            .get_or_spawn("account", move |mut receiver, cancellation| {
                ready_worker(tokio::spawn(async move {
                    cancellation.cancelled().await;
                    receiver.close();
                    while receiver.recv().await.is_some() {
                        worker_processed.fetch_add(1, Ordering::SeqCst);
                    }
                    WorkerTaskExit::Completed
                }))
            })
            .await
            .unwrap();

        sender.send(1).await.unwrap();
        sender.send(2).await.unwrap();
        assert_eq!(
            registry.stop_account("account").await,
            Some(WorkerTaskExit::Completed)
        );
        assert_eq!(processed.load(Ordering::SeqCst), 2);
        assert!(sender.is_closed());
    }

    #[tokio::test]
    async fn replacement_waits_until_previous_generation_is_joined() {
        let registry = Arc::new(WorkerRegistry::<u8>::new(8));
        let stopping = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let active_workers = Arc::new(AtomicUsize::new(0));

        let worker_stopping = Arc::clone(&stopping);
        let worker_release = Arc::clone(&release);
        let worker_active = Arc::clone(&active_workers);
        registry
            .get_or_spawn("account", move |_receiver, cancellation| {
                assert_eq!(worker_active.fetch_add(1, Ordering::SeqCst), 0);
                ready_worker(tokio::spawn(async move {
                    cancellation.cancelled().await;
                    worker_stopping.notify_one();
                    worker_release.notified().await;
                    worker_active.fetch_sub(1, Ordering::SeqCst);
                    WorkerTaskExit::Completed
                }))
            })
            .await
            .unwrap();

        let stop_registry = Arc::clone(&registry);
        let stop = tokio::spawn(async move { stop_registry.stop_account("account").await });
        stopping.notified().await;

        let replacement_registry = Arc::clone(&registry);
        let replacement_active = Arc::clone(&active_workers);
        let mut replacement = tokio::spawn(async move {
            replacement_registry
                .get_or_spawn("account", move |mut receiver, cancellation| {
                    assert_eq!(replacement_active.fetch_add(1, Ordering::SeqCst), 0);
                    ready_worker(tokio::spawn(async move {
                        cancellation.cancelled().await;
                        receiver.close();
                        while receiver.recv().await.is_some() {}
                        replacement_active.fetch_sub(1, Ordering::SeqCst);
                        WorkerTaskExit::Completed
                    }))
                })
                .await
                .unwrap()
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut replacement)
                .await
                .is_err()
        );
        assert_eq!(active_workers.load(Ordering::SeqCst), 1);

        release.notify_one();
        assert_eq!(stop.await.unwrap(), Some(WorkerTaskExit::Completed));
        let _sender = replacement.await.unwrap();
        assert_eq!(active_workers.load(Ordering::SeqCst), 1);
        registry.stop_all().await;
        assert_eq!(active_workers.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn panicked_task_is_observed_when_joined() {
        let registry = WorkerRegistry::<u8>::new(8);
        let sender = registry
            .get_or_spawn("account", |_receiver, _cancellation| {
                ready_worker(tokio::spawn(async move {
                    panic!("worker test panic");
                }))
            })
            .await
            .unwrap();

        sender.closed().await;
        assert_eq!(
            registry.stop_account("account").await,
            Some(WorkerTaskExit::SupervisorPanicked)
        );
    }

    #[tokio::test]
    async fn initialization_failure_does_not_publish_a_sender() {
        let registry = WorkerRegistry::<u8>::new(8);
        let result = registry
            .get_or_spawn("account", |_receiver, _cancellation| {
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                ready_tx
                    .send(Err("initialization failed".to_string()))
                    .unwrap();
                SpawnedWorker {
                    task: tokio::spawn(async { WorkerTaskExit::Completed }),
                    ready: ready_rx,
                }
            })
            .await;

        assert_eq!(result.unwrap_err(), "initialization failed");
        assert!(registry.stop_account("account").await.is_none());
    }

    #[tokio::test]
    async fn account_mutation_excludes_replacement_until_action_finishes() {
        let registry = Arc::new(WorkerRegistry::<u8>::new(8));
        let worker_release = Arc::new(Notify::new());
        let worker_stopping = Arc::new(Notify::new());
        let action_started = Arc::new(Notify::new());
        let action_release = Arc::new(Notify::new());

        let release = Arc::clone(&worker_release);
        let stopping = Arc::clone(&worker_stopping);
        registry
            .get_or_spawn("account", move |_receiver, cancellation| {
                ready_worker(tokio::spawn(async move {
                    cancellation.cancelled().await;
                    stopping.notify_one();
                    release.notified().await;
                    WorkerTaskExit::Completed
                }))
            })
            .await
            .unwrap();

        let mutation_registry = Arc::clone(&registry);
        let started = Arc::clone(&action_started);
        let action_done = Arc::clone(&action_release);
        let mutation = tokio::spawn(async move {
            mutation_registry
                .with_account_stopped("account", || async move {
                    started.notify_one();
                    action_done.notified().await;
                })
                .await;
        });
        worker_stopping.notified().await;
        worker_release.notify_one();
        action_started.notified().await;

        let replacement_registry = Arc::clone(&registry);
        let mut replacement = tokio::spawn(async move {
            replacement_registry
                .get_or_spawn("account", |mut receiver, cancellation| {
                    ready_worker(tokio::spawn(async move {
                        cancellation.cancelled().await;
                        receiver.close();
                        while receiver.recv().await.is_some() {}
                        WorkerTaskExit::Completed
                    }))
                })
                .await
                .unwrap()
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut replacement)
                .await
                .is_err()
        );

        action_release.notify_one();
        mutation.await.unwrap();
        let _sender = replacement.await.unwrap();
        registry.stop_all().await;
    }

    #[tokio::test]
    async fn global_shutdown_rejects_new_workers() {
        let registry = WorkerRegistry::<u8>::new(8);
        assert!(registry.stop_all().await.is_empty());

        let result = registry
            .get_or_spawn("account", |_receiver, _cancellation| {
                ready_worker(tokio::spawn(async { WorkerTaskExit::Completed }))
            })
            .await;
        assert_eq!(result.unwrap_err(), "application is shutting down");
    }

    #[tokio::test]
    async fn global_shutdown_cancels_worker_initialization() {
        let registry = Arc::new(WorkerRegistry::<u8>::new(8));
        let initialization_started = Arc::new(Notify::new());
        let acquisition_registry = Arc::clone(&registry);
        let started = Arc::clone(&initialization_started);
        let acquisition = tokio::spawn(async move {
            acquisition_registry
                .get_or_spawn("account", move |_receiver, cancellation| {
                    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                    SpawnedWorker {
                        task: tokio::spawn(async move {
                            started.notify_one();
                            cancellation.cancelled().await;
                            let _ = ready_tx.send(Err("shutdown".to_string()));
                            WorkerTaskExit::Completed
                        }),
                        ready: ready_rx,
                    }
                })
                .await
        });
        initialization_started.notified().await;

        let outcomes = tokio::time::timeout(Duration::from_secs(1), registry.stop_all())
            .await
            .expect("shutdown should release pending initialization");
        assert!(outcomes.is_empty());
        assert_eq!(acquisition.await.unwrap().unwrap_err(), "shutdown");
    }
}
