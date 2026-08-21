use super::flags::FlagMutation;
use crate::message::BackendMessageRef;

/// A single mail operation to be processed by the per-account worker.
#[derive(Debug, PartialEq, Eq)]
pub enum MailOp {
    // --- Sync (lower priority) ---
    /// Sync a single folder.
    SyncFolder { folder_path: String },
    /// Replay queued offline operations after a successful direct sync.
    ReplayOffline,
    // --- User operations (higher priority) ---
    /// Move provider-specific message objects to a target folder.
    MoveMessages {
        message_refs: Vec<BackendMessageRef>,
        target_folder: String,
    },
    /// Delete provider-specific message objects.
    DeleteMessages {
        message_refs: Vec<BackendMessageRef>,
    },
    /// Set or remove flags on provider-specific message references.
    SetFlags { mutations: Vec<FlagMutation> },
    /// Copy provider-specific message objects to a target folder.
    CopyMessages {
        message_refs: Vec<BackendMessageRef>,
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
        matches!(self, MailOp::SyncFolder { .. })
    }

    /// Whether replaying the complete operation after an execution failure is
    /// safe. JMAP copy adds mailbox membership idempotently; IMAP and Graph
    /// copy can create duplicates when a prior attempt committed remotely.
    pub fn can_retry_after_execution_failure(&self) -> bool {
        match self {
            MailOp::CopyMessages { message_refs, .. } => {
                !message_refs.is_empty()
                    && message_refs
                        .iter()
                        .all(|message_ref| matches!(message_ref, BackendMessageRef::Jmap { .. }))
            }
            _ => true,
        }
    }
}

/// Priority level for operations. Lower numeric value = higher priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OpPriority {
    /// User-initiated actions (move, copy, delete, flag) — process first.
    User = 0,
    /// Background sync — yields to user operations.
    Sync = 1,
}

/// An entry in the operation queue.
pub struct OpEntry {
    pub op: MailOp,
    pub priority: OpPriority,
}

#[cfg(test)]
mod tests {
    use super::MailOp;
    use crate::message::BackendMessageRef;

    #[test]
    fn only_jmap_copy_is_safe_to_retry_after_execution_failure() {
        for (message_refs, expected) in [
            (vec![BackendMessageRef::imap("INBOX", 1)], false),
            (vec![BackendMessageRef::graph("item")], false),
            (vec![BackendMessageRef::jmap("inbox", "email")], true),
            (Vec::new(), false),
        ] {
            let op = MailOp::CopyMessages {
                message_refs,
                target_folder: "archive".into(),
            };
            assert_eq!(op.can_retry_after_execution_failure(), expected);
        }
    }

    #[test]
    fn convergent_operations_remain_retryable() {
        let op = MailOp::DeleteMessages {
            message_refs: vec![BackendMessageRef::graph("item")],
        };
        assert!(op.can_retry_after_execution_failure());
    }
}
