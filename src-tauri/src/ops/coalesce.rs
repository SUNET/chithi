use std::collections::HashMap;

use super::queue::{MailOp, OpEntry};

/// Coalesce a batch of pending operations to reduce network round-trips.
///
/// Inspired by Thunderbird's `nsImapMoveCoalescer`:
/// - Multiple `DeleteMessages` are merged into one with combined UIDs.
/// - Multiple `MoveMessages` to the same target are merged.
/// - Adjacent `SetFlags` with the same flags+add value are merged.
/// - Sync operations are deduplicated (only one SyncAll kept).
pub fn coalesce(mut ops: Vec<OpEntry>) -> Vec<OpEntry> {
    if ops.len() <= 1 {
        return ops;
    }

    // Sort by priority so user ops come first
    ops.sort_by_key(|e| e.priority);

    let mut result: Vec<OpEntry> = Vec::new();
    for entry in ops {
        match entry.op {
            MailOp::DeleteMessages { by_folder } => {
                if let Some(OpEntry {
                    op: MailOp::DeleteMessages { by_folder: pending },
                    ..
                }) = result.last_mut()
                {
                    merge_by_folder(pending, by_folder);
                } else {
                    result.push(OpEntry {
                        op: MailOp::DeleteMessages { by_folder },
                        priority: entry.priority,
                    });
                }
            }
            MailOp::MoveMessages {
                by_folder,
                target_folder,
            } => {
                if let Some(OpEntry {
                    op:
                        MailOp::MoveMessages {
                            by_folder: pending,
                            target_folder: pending_target,
                        },
                    ..
                }) = result.last_mut()
                {
                    if *pending_target == target_folder {
                        merge_by_folder(pending, by_folder);
                        continue;
                    }
                }
                result.push(OpEntry {
                    op: MailOp::MoveMessages {
                        by_folder,
                        target_folder,
                    },
                    priority: entry.priority,
                });
            }
            MailOp::SetFlags { mutations } => {
                if let Some(OpEntry {
                    op: MailOp::SetFlags { mutations: pending },
                    ..
                }) = result.last_mut()
                {
                    if let ([pending_mutation], [mutation]) =
                        (pending.as_mut_slice(), mutations.as_slice())
                    {
                        if pending_mutation.flags == mutation.flags
                            && pending_mutation.add == mutation.add
                        {
                            pending_mutation
                                .message_refs
                                .extend(mutation.message_refs.clone());
                            continue;
                        }
                    }
                }
                result.push(OpEntry {
                    op: MailOp::SetFlags { mutations },
                    priority: entry.priority,
                });
            }
            MailOp::SyncAll { current_folder } => {
                result.retain(|pending| !matches!(pending.op, MailOp::SyncAll { .. }));
                result.push(OpEntry {
                    op: MailOp::SyncAll { current_folder },
                    priority: entry.priority,
                });
            }
            MailOp::ReplayOffline => {
                if !result
                    .iter()
                    .any(|pending| matches!(pending.op, MailOp::ReplayOffline))
                {
                    result.push(OpEntry {
                        op: MailOp::ReplayOffline,
                        priority: entry.priority,
                    });
                }
            }
            other => result.push(OpEntry {
                op: other,
                priority: entry.priority,
            }),
        }
    }
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
    use crate::mail::compat::BackendMessageRef;
    use crate::ops::queue::{FlagMutation, OpPriority};

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

    fn flag_entry(uid: u32, add: bool) -> OpEntry {
        OpEntry {
            op: MailOp::SetFlags {
                mutations: vec![FlagMutation {
                    message_refs: vec![BackendMessageRef::imap("INBOX", uid)],
                    flags: vec!["seen".into()],
                    add,
                }],
            },
            priority: OpPriority::User,
        }
    }

    #[test]
    fn adjacent_matching_flags_are_merged() {
        let result = coalesce(vec![flag_entry(1, true), flag_entry(2, true)]);
        assert_eq!(result.len(), 1);
        match &result[0].op {
            MailOp::SetFlags { mutations } => {
                assert_eq!(mutations[0].message_refs.len(), 2)
            }
            _ => panic!("Expected SetFlags"),
        }
    }

    #[test]
    fn conflicting_flags_preserve_order() {
        let result = coalesce(vec![
            flag_entry(1, true),
            flag_entry(1, false),
            flag_entry(1, true),
        ]);
        let adds: Vec<bool> = result
            .iter()
            .filter_map(|entry| match &entry.op {
                MailOp::SetFlags { mutations } => Some(mutations[0].add),
                _ => None,
            })
            .collect();
        assert_eq!(adds, vec![true, false, true]);
    }

    #[test]
    fn flags_remain_before_following_copy() {
        let result = coalesce(vec![
            flag_entry(1, true),
            OpEntry {
                op: MailOp::CopyMessages {
                    by_folder: HashMap::from([("INBOX".into(), vec![1])]),
                    target_folder: "Archive".into(),
                },
                priority: OpPriority::User,
            },
        ]);
        assert!(matches!(result[0].op, MailOp::SetFlags { .. }));
        assert!(matches!(result[1].op, MailOp::CopyMessages { .. }));
    }

    #[test]
    fn replay_signals_are_deduplicated() {
        let result = coalesce(vec![
            OpEntry {
                op: MailOp::ReplayOffline,
                priority: OpPriority::Sync,
            },
            OpEntry {
                op: MailOp::ReplayOffline,
                priority: OpPriority::Sync,
            },
        ]);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0].op, MailOp::ReplayOffline));
    }
}
