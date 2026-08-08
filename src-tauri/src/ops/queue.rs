use std::collections::HashMap;

use super::flags::FlagMutation;

/// A single mail operation to be processed by the per-account worker.
#[derive(Debug, PartialEq, Eq)]
pub enum MailOp {
    // --- Sync (lower priority) ---
    /// Full account sync. The worker delegates to the existing sync engine.
    SyncAll { current_folder: Option<String> },
    /// Sync a single folder.
    SyncFolder { folder_path: String },
    /// Replay queued offline operations after a successful direct sync.
    ReplayOffline,
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
    /// Set or remove flags on provider-specific message references.
    SetFlags { mutations: Vec<FlagMutation> },
    /// Copy messages by IMAP UID, grouped by source folder.
    CopyMessages {
        by_folder: HashMap<String, Vec<u32>>,
        target_folder: String,
    },
    /// Replay a previously-built RFC 5322 message.
    ///
    /// First-attempt sends originate in `commands::compose::send_message`
    /// which both persists the outbox row and spawns its own send task;
    /// this variant is only constructed by `outbox_to_mail_op` when the
    /// worker replays a failed send during the post-sync drain.
    SendRaw {
        raw_message: Vec<u8>,
        from: String,
        to: Vec<String>,
        cc: Vec<String>,
        bcc: Vec<String>,
        subject: String,
    },
}

impl MailOp {
    /// Background-sync ops are routed to the account's `MailBackend`;
    /// everything else is a user op that goes through the
    /// `MailOpExecutor` and, on failure, the offline outbox. The
    /// worker's `execute` match enforces the same split exhaustively —
    /// this is the shared classification for callers that only need
    /// the boolean.
    pub fn is_sync(&self) -> bool {
        matches!(self, MailOp::SyncAll { .. } | MailOp::SyncFolder { .. })
    }
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
