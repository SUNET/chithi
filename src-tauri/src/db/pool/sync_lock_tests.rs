use super::DbPool;
use std::sync::{mpsc, Arc, Barrier};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

fn test_pool() -> (tempfile::TempDir, Arc<DbPool>) {
    let temp = tempfile::tempdir().unwrap();
    let pool = Arc::new(DbPool::new(&temp.path().join("sync-locks.db"), 1).unwrap());
    (temp, pool)
}

#[test]
fn concurrent_folder_sync_lookups_share_one_lock() {
    let (_temp, pool) = test_pool();
    let start = Barrier::new(8);
    let locks = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    pool.imap_folder_sync_lock("account", "INBOX")
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(locks.iter().all(|lock| Arc::ptr_eq(&locks[0], lock)));
}

#[test]
fn folder_sync_locks_are_scoped_by_account_raw_path_and_pool() {
    let (_temp, pool) = test_pool();
    let (_other_temp, other_pool) = test_pool();
    let active = pool.imap_folder_sync_lock("account", "INBOX");
    let _guard = active.blocking_lock_owned();
    assert!(pool
        .imap_folder_sync_lock("account", "INBOX")
        .try_lock_owned()
        .is_err());
    for (account, path) in [
        ("account", "Sent"),
        ("another-account", "INBOX"),
        ("account", "INBOX/Child"),
    ] {
        assert!(pool
            .imap_folder_sync_lock(account, path)
            .try_lock_owned()
            .is_ok());
    }
    assert!(other_pool
        .imap_folder_sync_lock("account", "INBOX")
        .try_lock_owned()
        .is_ok());

    let _first = pool
        .imap_folder_sync_lock("account:one", "Folder")
        .blocking_lock_owned();
    assert!(pool
        .imap_folder_sync_lock("account", "one:Folder")
        .try_lock_owned()
        .is_ok());
    // The folder guard is not a long-lived database writer or reader lock.
    assert!(pool.writer.try_lock().is_ok());
    assert_eq!(
        pool.reader()
            .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn waiting_folder_sync_keeps_the_registry_entry_alive() {
    let (_temp, pool) = test_pool();
    let lock = pool.imap_folder_sync_lock("account", "INBOX");
    let identity = Arc::downgrade(&lock);
    let first = lock.clone().lock_owned().await;
    let mut waiting = Box::pin(lock.lock_owned());
    assert!(futures::poll!(&mut waiting).is_pending());
    drop(first);

    // Only the queued acquisition owns the old mutex now. A lookup must not
    // replace it while ownership is being handed off to that waiter.
    let again = pool.imap_folder_sync_lock("account", "INBOX");
    assert!(Arc::ptr_eq(&identity.upgrade().unwrap(), &again));
    let second = tokio::time::timeout(TIMEOUT, waiting).await.unwrap();
    assert!(again.try_lock_owned().is_err());
    drop(second);
    assert!(identity.upgrade().is_none());
}

#[test]
fn folder_sync_registry_prunes_unused_entries_without_removing_live_locks() {
    let (_temp, pool) = test_pool();
    let live = pool.imap_folder_sync_lock("account", "INBOX");
    for index in 0..100 {
        drop(pool.imap_folder_sync_lock("account", &format!("temporary-{index}")));
    }
    // Looking up the live key prunes the final expired temporary entry.
    assert!(Arc::ptr_eq(
        &live,
        &pool.imap_folder_sync_lock("account", "INBOX")
    ));
    assert_eq!(pool.imap_folder_sync_locks.lock().unwrap().len(), 1);
    let identity = Arc::downgrade(&live);
    drop(live);
    assert!(identity.upgrade().is_none());
    let _replacement = pool.imap_folder_sync_lock("account", "Sent");
    let entries = pool.imap_folder_sync_locks.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries.contains_key(&("account".to_string(), "Sent".to_string())));
}

#[test]
fn folder_sync_lock_releases_after_error_and_panic() {
    fn fail(lock: Arc<tokio::sync::Mutex<()>>) -> Result<(), ()> {
        let _guard = lock.blocking_lock_owned();
        Err(())
    }

    let (_temp, pool) = test_pool();
    let lock = pool.imap_folder_sync_lock("account", "INBOX");
    let result = fail(lock.clone());
    assert!(result.is_err());
    assert!(lock.clone().try_lock_owned().is_ok());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = lock.clone().blocking_lock_owned();
        panic!("simulated sync panic");
    }));
    assert!(result.is_err());
    assert!(lock.try_lock_owned().is_ok());
}

#[tokio::test]
async fn cancelled_async_caller_does_not_release_its_blocking_folder_sync() {
    let (_temp, pool) = test_pool();
    let lock = pool.imap_folder_sync_lock("account", "INBOX");
    let (started, wait_started) = tokio::sync::oneshot::channel();
    let (finish, wait_finish) = mpsc::channel();
    let caller = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || {
            let _guard = pool
                .imap_folder_sync_lock("account", "INBOX")
                .blocking_lock_owned();
            started.send(()).unwrap();
            wait_finish.recv_timeout(TIMEOUT).unwrap();
        })
        .await
        .unwrap();
    });
    tokio::time::timeout(TIMEOUT, wait_started)
        .await
        .unwrap()
        .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert!(lock.clone().try_lock_owned().is_err());
    finish.send(()).unwrap();
    let _guard = tokio::time::timeout(TIMEOUT, lock.lock_owned())
        .await
        .unwrap();
}
