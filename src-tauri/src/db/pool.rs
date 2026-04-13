use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::Connection;

use crate::error::{Error, Result};

/// A guard that holds a read-only database connection from the pool.
/// The connection is returned to the pool when the guard is dropped.
pub struct PooledReader<'a> {
    guard: std::sync::MutexGuard<'a, Connection>,
}

impl std::ops::Deref for PooledReader<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.guard
    }
}

/// SQLite connection pool with read/write separation.
///
/// SQLite in WAL mode allows exactly one writer and unlimited concurrent
/// readers, but only when using separate connections.  This pool enforces
/// that model: one writer behind a `tokio::sync::Mutex` (async-aware) and
/// N readers behind `std::sync::Mutex` (fast, no async overhead for <1ms
/// reads).
pub struct DbPool {
    writer: tokio::sync::Mutex<Connection>,
    readers: Vec<std::sync::Mutex<Connection>>,
    next_reader: AtomicUsize,
    #[allow(dead_code)]
    db_path: PathBuf,
}

impl DbPool {
    /// Create a new pool with one writer and `reader_count` reader connections.
    pub fn new(db_path: &Path, reader_count: usize) -> Result<Self> {
        let writer = open_writer(db_path)?;
        let mut readers = Vec::with_capacity(reader_count);
        for _ in 0..reader_count {
            readers.push(std::sync::Mutex::new(open_reader(db_path)?));
        }
        Ok(Self {
            writer: tokio::sync::Mutex::new(writer),
            readers,
            next_reader: AtomicUsize::new(0),
            db_path: db_path.to_path_buf(),
        })
    }

    /// Acquire the exclusive writer connection (async).
    ///
    /// Only one task can hold this at a time.  Prefer short transactions.
    pub async fn writer(&self) -> tokio::sync::MutexGuard<'_, Connection> {
        self.writer.lock().await
    }

    /// Acquire a read-only connection (non-blocking round-robin).
    ///
    /// Safe to call from synchronous / `spawn_blocking` contexts because
    /// `std::sync::Mutex` never yields to the async executor.
    pub fn reader(&self) -> PooledReader<'_> {
        let idx = self.next_reader.fetch_add(1, Ordering::Relaxed) % self.readers.len();
        let guard = self.readers[idx]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        PooledReader { guard }
    }
}

fn open_writer(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path).map_err(Error::Database)?;
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        PRAGMA foreign_keys=ON;
        PRAGMA busy_timeout=5000;
        ",
    )
    .map_err(Error::Database)?;
    Ok(conn)
}

fn open_reader(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(Error::Database)?;
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        PRAGMA query_only=ON;
        PRAGMA busy_timeout=5000;
        ",
    )
    .map_err(Error::Database)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn temp_db_path() -> PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        // Create schema so readers can open
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "
            PRAGMA journal_mode=WAL;
            CREATE TABLE test (id INTEGER PRIMARY KEY, value TEXT);
            INSERT INTO test VALUES (1, 'hello');
            ",
        )
        .unwrap();
        // Keep tempdir alive by leaking it (tests only)
        std::mem::forget(dir);
        path
    }

    #[tokio::test]
    async fn concurrent_readers_dont_block() {
        let pool = Arc::new(DbPool::new(&temp_db_path(), 4).unwrap());

        let mut handles = vec![];
        for _ in 0..8 {
            let pool = pool.clone();
            handles.push(tokio::task::spawn_blocking(move || {
                let reader = pool.reader();
                let val: String = reader
                    .query_row("SELECT value FROM test WHERE id = 1", [], |row| row.get(0))
                    .unwrap();
                assert_eq!(val, "hello");
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }

    #[tokio::test]
    async fn writer_doesnt_block_readers() {
        let pool = Arc::new(DbPool::new(&temp_db_path(), 2).unwrap());

        // Hold writer lock
        let writer = pool.writer().await;

        // Reader should still work
        let pool2 = pool.clone();
        let reader_result = tokio::task::spawn_blocking(move || {
            let reader = pool2.reader();
            reader
                .query_row("SELECT value FROM test WHERE id = 1", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap()
        })
        .await
        .unwrap();

        assert_eq!(reader_result, "hello");
        drop(writer);
    }

    #[tokio::test]
    async fn writer_can_write() {
        let pool = Arc::new(DbPool::new(&temp_db_path(), 2).unwrap());

        {
            let writer = pool.writer().await;
            writer
                .execute("INSERT INTO test VALUES (2, 'world')", [])
                .unwrap();
        }

        let reader = pool.reader();
        let val: String = reader
            .query_row("SELECT value FROM test WHERE id = 2", [], |row| row.get(0))
            .unwrap();
        assert_eq!(val, "world");
    }
}
