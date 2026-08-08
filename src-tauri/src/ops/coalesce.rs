use super::flags::FlagTarget;
use super::queue::{MailOp, OpEntry};

/// Coalesce a batch of pending operations to reduce network round-trips.
///
/// Inspired by Thunderbird's `nsImapMoveCoalescer`:
/// - Multiple `DeleteMessages` are merged and deduplicated.
/// - Multiple `MoveMessages` to the same target are merged.
/// - `CopyMessages` remain separate because copying is not idempotent.
/// - Adjacent `SetFlags` with the same flags+add value are merged.
/// - Sync operations are deduplicated (only one SyncAll kept).
pub fn coalesce(mut ops: Vec<OpEntry>) -> Vec<OpEntry> {
    if ops.is_empty() {
        return ops;
    }

    // Sort by priority so user ops come first
    ops.sort_by_key(|e| e.priority);

    let mut result: Vec<OpEntry> = Vec::new();
    for entry in ops {
        match entry.op {
            MailOp::DeleteMessages { message_refs } => {
                if let Some(OpEntry {
                    op:
                        MailOp::DeleteMessages {
                            message_refs: pending,
                        },
                    ..
                }) = result.last_mut()
                {
                    for message_ref in message_refs {
                        push_unique_ref(pending, message_ref);
                    }
                } else {
                    let mut unique_refs = Vec::new();
                    for message_ref in message_refs {
                        push_unique_ref(&mut unique_refs, message_ref);
                    }
                    result.push(OpEntry {
                        op: MailOp::DeleteMessages {
                            message_refs: unique_refs,
                        },
                        priority: entry.priority,
                    });
                }
            }
            MailOp::MoveMessages {
                message_refs,
                target_folder,
            } => {
                if let Some(OpEntry {
                    op:
                        MailOp::MoveMessages {
                            message_refs: pending,
                            target_folder: pending_target,
                        },
                    ..
                }) = result.last_mut()
                {
                    if *pending_target == target_folder {
                        for message_ref in message_refs {
                            if !pending.contains(&message_ref) {
                                pending.push(message_ref);
                            }
                        }
                        continue;
                    }
                }
                result.push(OpEntry {
                    op: MailOp::MoveMessages {
                        message_refs,
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
                            if let (
                                FlagTarget::Messages(pending_refs),
                                FlagTarget::Messages(message_refs),
                            ) = (&mut pending_mutation.target, &mutation.target)
                            {
                                pending_refs.extend(message_refs.clone());
                                continue;
                            }
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

fn push_unique_ref(
    message_refs: &mut Vec<crate::mail::compat::BackendMessageRef>,
    candidate: crate::mail::compat::BackendMessageRef,
) {
    if !message_refs
        .iter()
        .any(|existing| existing.same_message(&candidate))
    {
        message_refs.push(candidate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::compat::BackendMessageRef;
    use crate::ops::flags::FlagMutation;
    use crate::ops::queue::OpPriority;

    #[test]
    fn coalesce_multiple_deletes() {
        let ops = vec![
            OpEntry {
                op: MailOp::DeleteMessages {
                    message_refs: vec![
                        BackendMessageRef::imap("INBOX", 1),
                        BackendMessageRef::jmap("inbox", "email_1"),
                    ],
                },
                priority: OpPriority::User,
            },
            OpEntry {
                op: MailOp::DeleteMessages {
                    message_refs: vec![
                        BackendMessageRef::imap("INBOX", 2),
                        BackendMessageRef::jmap("archive", "email_1"),
                        BackendMessageRef::graph("item_1"),
                    ],
                },
                priority: OpPriority::User,
            },
        ];

        let result = coalesce(ops);
        assert_eq!(result.len(), 1);
        match &result[0].op {
            MailOp::DeleteMessages { message_refs } => {
                assert_eq!(message_refs.len(), 4);
                assert_eq!(
                    message_refs
                        .iter()
                        .filter(|message_ref| {
                            message_ref.same_message(&BackendMessageRef::jmap("other", "email_1"))
                        })
                        .count(),
                    1
                );
            }
            _ => panic!("Expected DeleteMessages"),
        }
    }

    #[test]
    fn singleton_delete_is_deduplicated() {
        let result = coalesce(vec![OpEntry {
            op: MailOp::DeleteMessages {
                message_refs: vec![
                    BackendMessageRef::jmap("inbox", "email_1"),
                    BackendMessageRef::jmap("archive", "email_1"),
                ],
            },
            priority: OpPriority::User,
        }]);

        match &result[0].op {
            MailOp::DeleteMessages { message_refs } => assert_eq!(message_refs.len(), 1),
            _ => panic!("Expected DeleteMessages"),
        }
    }

    #[test]
    fn coalesce_moves_to_same_target() {
        let ops = vec![
            OpEntry {
                op: MailOp::MoveMessages {
                    message_refs: vec![BackendMessageRef::imap("INBOX", 1)],
                    target_folder: "Trash".into(),
                },
                priority: OpPriority::User,
            },
            OpEntry {
                op: MailOp::MoveMessages {
                    message_refs: vec![
                        BackendMessageRef::imap("INBOX", 1),
                        BackendMessageRef::imap("INBOX", 2),
                        BackendMessageRef::jmap("inbox", "email_1"),
                        BackendMessageRef::jmap("archive", "email_1"),
                    ],
                    target_folder: "Trash".into(),
                },
                priority: OpPriority::User,
            },
        ];

        let result = coalesce(ops);
        assert_eq!(result.len(), 1);
        match &result[0].op {
            MailOp::MoveMessages {
                message_refs,
                target_folder,
            } => {
                assert_eq!(target_folder, "Trash");
                assert_eq!(message_refs.len(), 4);
                assert!(message_refs.contains(&BackendMessageRef::jmap("inbox", "email_1")));
                assert!(message_refs.contains(&BackendMessageRef::jmap("archive", "email_1")));
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
                    message_refs: vec![BackendMessageRef::imap("INBOX", 1)],
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
                    target: FlagTarget::messages(vec![BackendMessageRef::imap("INBOX", uid)]),
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
                assert_eq!(mutations[0].target.message_refs().unwrap().len(), 2)
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
    fn bulk_flags_are_not_coalesced_with_message_flags() {
        let bulk = OpEntry {
            op: MailOp::SetFlags {
                mutations: vec![FlagMutation {
                    target: FlagTarget::AllMessagesInFolders {
                        folder_paths: vec!["INBOX".into()],
                        excluded_refs: Vec::new(),
                    },
                    flags: vec!["seen".into()],
                    add: true,
                }],
            },
            priority: OpPriority::User,
        };

        let result = coalesce(vec![bulk, flag_entry(1, true)]);
        assert_eq!(result.len(), 2);
        assert!(matches!(result[0].op, MailOp::SetFlags { .. }));
        assert!(matches!(result[1].op, MailOp::SetFlags { .. }));
    }

    #[test]
    fn flags_remain_before_following_copy() {
        let result = coalesce(vec![
            flag_entry(1, true),
            OpEntry {
                op: MailOp::CopyMessages {
                    message_refs: vec![BackendMessageRef::imap("INBOX", 1)],
                    target_folder: "Archive".into(),
                },
                priority: OpPriority::User,
            },
        ]);
        assert!(matches!(result[0].op, MailOp::SetFlags { .. }));
        assert!(matches!(result[1].op, MailOp::CopyMessages { .. }));
    }

    #[test]
    fn copy_operations_are_not_coalesced_or_deduplicated() {
        let copy = || OpEntry {
            op: MailOp::CopyMessages {
                message_refs: vec![BackendMessageRef::imap("INBOX", 1)],
                target_folder: "Archive".into(),
            },
            priority: OpPriority::User,
        };

        let result = coalesce(vec![copy(), copy()]);
        assert_eq!(result.len(), 2);
        assert!(matches!(result[0].op, MailOp::CopyMessages { .. }));
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
