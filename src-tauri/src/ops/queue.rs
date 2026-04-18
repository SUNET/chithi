use std::collections::HashMap;

/// A single mail operation to be processed by the per-account worker.
#[derive(Debug)]
pub enum MailOp {
    // --- Sync (lower priority) ---
    /// Full account sync. The worker delegates to the existing sync engine.
    SyncAll { current_folder: Option<String> },
    /// Sync a single folder.
    SyncFolder { folder_path: String },

    // --- User operations (higher priority) ---
    /// Move messages by IMAP UID, grouped by source folder.
    MoveMessages {
        by_folder: HashMap<String, Vec<u32>>,
        target_folder: String,
    },
    /// Delete messages by IMAP UID, grouped by source folder.
    DeleteMessages {
        by_folder: HashMap<String, Vec<u32>>,
    },
    /// Set or remove flags by IMAP UID, grouped by source folder.
    SetFlags {
        by_folder: HashMap<String, Vec<u32>>,
        flags: Vec<String>,
        add: bool,
    },
    /// Copy messages by IMAP UID, grouped by source folder.
    CopyMessages {
        by_folder: HashMap<String, Vec<u32>>,
        target_folder: String,
    },
}

/// Priority level for operations. Lower numeric value = higher priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OpPriority {
    /// User-initiated actions (move, delete, flag) — process first.
    User = 0,
    /// Background sync — yields to user operations.
    Sync = 1,
}

/// An entry in the operation queue.
pub struct OpEntry {
    pub op: MailOp,
    pub priority: OpPriority,
}
