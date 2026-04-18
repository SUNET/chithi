use std::collections::HashMap;

use super::queue::{MailOp, OpEntry, OpPriority};

/// Coalesce a batch of pending operations to reduce network round-trips.
///
/// Inspired by Thunderbird's `nsImapMoveCoalescer`:
/// - Multiple `DeleteMessages` are merged into one with combined UIDs.
/// - Multiple `MoveMessages` to the same target are merged.
/// - Multiple `SetFlags` with the same flags+add value are merged.
/// - Sync operations are deduplicated (only one SyncAll kept).
pub fn coalesce(mut ops: Vec<OpEntry>) -> Vec<OpEntry> {
    if ops.len() <= 1 {
        return ops;
    }

    // Sort by priority so user ops come first
    ops.sort_by_key(|e| e.priority);

    let mut result: Vec<OpEntry> = Vec::new();
    let mut pending_deletes: Option<HashMap<String, Vec<u32>>> = None;
    let mut pending_moves: HashMap<String, HashMap<String, Vec<u32>>> = HashMap::new(); // target -> by_folder
    let mut pending_flags: HashMap<(Vec<String>, bool), HashMap<String, Vec<u32>>> = HashMap::new();
    let mut sync_all_folder: Option<Option<String>> = None;

    for entry in ops {
        match entry.op {
            MailOp::DeleteMessages { by_folder } => {
                let deletes = pending_deletes.get_or_insert_with(HashMap::new);
                merge_by_folder(deletes, by_folder);
            }
            MailOp::MoveMessages {
                by_folder,
                target_folder,
            } => {
                let moves = pending_moves.entry(target_folder).or_default();
                merge_by_folder(moves, by_folder);
            }
            MailOp::SetFlags {
                by_folder,
                flags,
                add,
            } => {
                let key = (flags, add);
                let flag_ops = pending_flags.entry(key).or_default();
                merge_by_folder(flag_ops, by_folder);
            }
            MailOp::SyncAll { current_folder } => {
                // Always keep the LAST current_folder value — the user may
                // have navigated between folders while ops were queued.
                sync_all_folder = Some(current_folder);
            }
            // Pass through non-coalescable ops
            other => {
                result.push(OpEntry {
                    op: other,
                    priority: entry.priority,
                });
            }
        }
    }

    // Emit the single coalesced SyncAll with the last folder value
    if let Some(current_folder) = sync_all_folder {
        result.push(OpEntry {
            op: MailOp::SyncAll { current_folder },
            priority: OpPriority::Sync,
        });
    }

    // Emit coalesced flag operations
    for ((flags, add), by_folder) in pending_flags {
        if !by_folder.is_empty() {
            result.push(OpEntry {
                op: MailOp::SetFlags {
                    by_folder,
                    flags,
                    add,
                },
                priority: OpPriority::User,
            });
        }
    }

    // Emit coalesced move operations
    for (target_folder, by_folder) in pending_moves {
        if !by_folder.is_empty() {
            result.push(OpEntry {
                op: MailOp::MoveMessages {
                    by_folder,
                    target_folder,
                },
                priority: OpPriority::User,
            });
        }
    }

    // Emit coalesced delete operation
    if let Some(by_folder) = pending_deletes {
        if !by_folder.is_empty() {
            result.push(OpEntry {
                op: MailOp::DeleteMessages { by_folder },
                priority: OpPriority::User,
            });
        }
    }

    // Re-sort so user ops come first
    result.sort_by_key(|e| e.priority);
    result
}

/// Merge UIDs from `source` into `target`, combining by folder key.
fn merge_by_folder(target: &mut HashMap<String, Vec<u32>>, source: HashMap<String, Vec<u32>>) {
    for (folder, uids) in source {
        target.entry(folder).or_default().extend(uids);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_multiple_deletes() {
        let ops = vec![
            OpEntry {
                op: MailOp::DeleteMessages {
                    by_folder: HashMap::from([("INBOX".into(), vec![1, 2])]),
                },
                priority: OpPriority::User,
            },
            OpEntry {
                op: MailOp::DeleteMessages {
                    by_folder: HashMap::from([("INBOX".into(), vec![3])]),
                },
                priority: OpPriority::User,
            },
        ];

        let result = coalesce(ops);
        assert_eq!(result.len(), 1);
        match &result[0].op {
            MailOp::DeleteMessages { by_folder } => {
                assert_eq!(by_folder["INBOX"].len(), 3);
            }
            _ => panic!("Expected DeleteMessages"),
        }
    }

    #[test]
    fn coalesce_moves_to_same_target() {
        let ops = vec![
            OpEntry {
                op: MailOp::MoveMessages {
                    by_folder: HashMap::from([("INBOX".into(), vec![1])]),
                    target_folder: "Trash".into(),
                },
                priority: OpPriority::User,
            },
            OpEntry {
                op: MailOp::MoveMessages {
                    by_folder: HashMap::from([("INBOX".into(), vec![2, 3])]),
                    target_folder: "Trash".into(),
                },
                priority: OpPriority::User,
            },
        ];

        let result = coalesce(ops);
        assert_eq!(result.len(), 1);
        match &result[0].op {
            MailOp::MoveMessages {
                by_folder,
                target_folder,
            } => {
                assert_eq!(target_folder, "Trash");
                assert_eq!(by_folder["INBOX"].len(), 3);
            }
            _ => panic!("Expected MoveMessages"),
        }
    }

    #[test]
    fn coalesce_dedup_sync_all() {
        let ops = vec![
            OpEntry {
                op: MailOp::SyncAll {
                    current_folder: Some("INBOX".into()),
                },
                priority: OpPriority::Sync,
            },
            OpEntry {
                op: MailOp::SyncAll {
                    current_folder: Some("Sent".into()),
                },
                priority: OpPriority::Sync,
            },
        ];

        let result = coalesce(ops);
        let syncs: Vec<_> = result
            .iter()
            .filter(|e| matches!(e.op, MailOp::SyncAll { .. }))
            .collect();
        assert_eq!(syncs.len(), 1);
        // Should keep the LAST current_folder value
        match &syncs[0].op {
            MailOp::SyncAll { current_folder } => {
                assert_eq!(current_folder.as_deref(), Some("Sent"));
            }
            _ => panic!("Expected SyncAll"),
        }
    }

    #[test]
    fn user_ops_before_sync() {
        let ops = vec![
            OpEntry {
                op: MailOp::SyncAll {
                    current_folder: None,
                },
                priority: OpPriority::Sync,
            },
            OpEntry {
                op: MailOp::DeleteMessages {
                    by_folder: HashMap::from([("INBOX".into(), vec![1])]),
                },
                priority: OpPriority::User,
            },
        ];

        let result = coalesce(ops);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].priority, OpPriority::User);
        assert_eq!(result[1].priority, OpPriority::Sync);
    }
}
