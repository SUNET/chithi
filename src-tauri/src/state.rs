use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use crate::db;
use crate::error::Result;

pub struct SyncHandle {
    pub abort_handle: tokio::task::AbortHandle,
}

pub struct AppState {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub sync_handles: RwLock<HashMap<String, SyncHandle>>,
    pub data_dir: PathBuf,
}

impl AppState {
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir)?;
        let db_path = data_dir.join("emails.db");
        let conn = rusqlite::Connection::open(&db_path)?;
        db::schema::initialize(&conn)?;

        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
            sync_handles: RwLock::new(HashMap::new()),
            data_dir,
        })
    }
}
