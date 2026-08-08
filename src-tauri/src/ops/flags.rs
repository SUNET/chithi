use crate::mail::compat::BackendMessageRef;

/// The provider objects affected by one ordered flag mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagTarget {
    /// Explicit provider message identities.
    Messages(Vec<BackendMessageRef>),
    /// Every IMAP message in the listed folders, including uncached messages.
    ///
    /// Exclusions preserve newer per-message unread intent when an older bulk
    /// read operation is replayed from the offline outbox.
    AllMessagesInFolders {
        folder_paths: Vec<String>,
        excluded_refs: Vec<BackendMessageRef>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountReadScope {
    KnownUnreadMessages,
    AllKnownFolders,
}

impl FlagTarget {
    pub fn messages(message_refs: Vec<BackendMessageRef>) -> Self {
        Self::Messages(message_refs)
    }

    /// Build the provider-specific target for "mark account read".
    ///
    /// Unknown protocols retain the application's legacy IMAP fallback.
    pub fn for_mark_account_read(
        protocol: &str,
        folder_paths: Vec<String>,
        unread_refs: Vec<BackendMessageRef>,
    ) -> Self {
        match protocol {
            "graph" | "jmap" => Self::Messages(unread_refs),
            _ => Self::AllMessagesInFolders {
                folder_paths,
                excluded_refs: Vec::new(),
            },
        }
    }

    pub fn account_read_scope(protocol: &str) -> AccountReadScope {
        match protocol {
            "graph" | "jmap" => AccountReadScope::KnownUnreadMessages,
            _ => AccountReadScope::AllKnownFolders,
        }
    }

    pub fn message_refs(&self) -> Option<&[BackendMessageRef]> {
        match self {
            Self::Messages(message_refs) => Some(message_refs),
            Self::AllMessagesInFolders { .. } => None,
        }
    }

    pub fn message_refs_mut(&mut self) -> Option<&mut Vec<BackendMessageRef>> {
        match self {
            Self::Messages(message_refs) => Some(message_refs),
            Self::AllMessagesInFolders { .. } => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Messages(message_refs) => message_refs.is_empty(),
            Self::AllMessagesInFolders { folder_paths, .. } => folder_paths.is_empty(),
        }
    }
}

/// One ordered flag mutation within a queued flag operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagMutation {
    pub target: FlagTarget,
    pub flags: Vec<String>,
    pub add: bool,
}

/// Remove explicitly targeted messages that a newer delete makes obsolete.
/// Server-wide IMAP targets are retained because they remain valid operations.
pub fn remove_deleted_refs(
    mut mutation: FlagMutation,
    deleted_refs: &[BackendMessageRef],
) -> Option<FlagMutation> {
    if let FlagTarget::Messages(message_refs) = &mut mutation.target {
        message_refs.retain(|message_ref| !contains_ref(deleted_refs, message_ref));
    }
    (!mutation.target.is_empty()).then_some(mutation)
}

/// Remove portions of `older` overwritten by `newer` while preserving the
/// original operation's position in the outbox.
pub fn subtract_flag_mutation(older: FlagMutation, newer: &FlagMutation) -> Vec<FlagMutation> {
    let overlapping_flags: Vec<String> = older
        .flags
        .iter()
        .filter(|old_flag| {
            newer
                .flags
                .iter()
                .any(|new_flag| normalize_flag(new_flag) == normalize_flag(old_flag))
        })
        .cloned()
        .collect();
    if overlapping_flags.is_empty() {
        return vec![older];
    }
    let remaining_flags: Vec<String> = older
        .flags
        .iter()
        .filter(|old_flag| {
            !newer
                .flags
                .iter()
                .any(|new_flag| normalize_flag(new_flag) == normalize_flag(old_flag))
        })
        .cloned()
        .collect();

    let old_target = older.target.clone();
    let new_target = newer.target.clone();
    match (&old_target, &new_target) {
        (FlagTarget::Messages(old_refs), FlagTarget::Messages(new_refs)) => {
            split_message_target(older, old_refs, new_refs, remaining_flags)
        }
        (
            FlagTarget::Messages(old_refs),
            FlagTarget::AllMessagesInFolders {
                folder_paths,
                excluded_refs,
            },
        ) => {
            let covered: Vec<_> = old_refs
                .iter()
                .filter(|message_ref| {
                    ref_in_folders(message_ref, folder_paths)
                        && !contains_ref(excluded_refs, message_ref)
                })
                .cloned()
                .collect();
            split_message_target(older, old_refs, &covered, remaining_flags)
        }
        (
            FlagTarget::AllMessagesInFolders {
                folder_paths,
                excluded_refs,
            },
            FlagTarget::Messages(new_refs),
        ) => {
            if older.add == newer.add {
                let exclusions_left: Vec<_> = excluded_refs
                    .iter()
                    .filter(|excluded_ref| !contains_ref(new_refs, excluded_ref))
                    .cloned()
                    .collect();
                if exclusions_left.len() == excluded_refs.len() {
                    return vec![older];
                }

                let restored_target = FlagTarget::AllMessagesInFolders {
                    folder_paths: folder_paths.clone(),
                    excluded_refs: exclusions_left,
                };
                if remaining_flags.is_empty() {
                    return vec![FlagMutation {
                        target: restored_target,
                        flags: older.flags,
                        add: older.add,
                    }];
                }
                return vec![
                    FlagMutation {
                        target: older.target,
                        flags: remaining_flags,
                        add: older.add,
                    },
                    FlagMutation {
                        target: restored_target,
                        flags: overlapping_flags,
                        add: older.add,
                    },
                ];
            }
            let additions: Vec<_> = new_refs
                .iter()
                .filter(|message_ref| {
                    ref_in_folders(message_ref, folder_paths)
                        && !contains_ref(excluded_refs, message_ref)
                })
                .cloned()
                .collect();
            if additions.is_empty() {
                return vec![older];
            }

            let mut result = Vec::new();
            if !remaining_flags.is_empty() {
                result.push(FlagMutation {
                    target: older.target.clone(),
                    flags: remaining_flags,
                    add: older.add,
                });
            }
            let mut exclusions = excluded_refs.clone();
            for message_ref in additions {
                if !contains_ref(&exclusions, &message_ref) {
                    exclusions.push(message_ref);
                }
            }
            result.push(FlagMutation {
                target: FlagTarget::AllMessagesInFolders {
                    folder_paths: folder_paths.clone(),
                    excluded_refs: exclusions,
                },
                flags: overlapping_flags,
                add: older.add,
            });
            result
        }
        (
            FlagTarget::AllMessagesInFolders {
                folder_paths: old_folders,
                excluded_refs,
            },
            FlagTarget::AllMessagesInFolders {
                folder_paths: new_folders,
                ..
            },
        ) => {
            let folders_left: Vec<_> = old_folders
                .iter()
                .filter(|folder| !new_folders.contains(folder))
                .cloned()
                .collect();
            if folders_left.len() == old_folders.len() {
                return vec![older];
            }

            let mut result = Vec::new();
            if !remaining_flags.is_empty() {
                result.push(FlagMutation {
                    target: older.target.clone(),
                    flags: remaining_flags,
                    add: older.add,
                });
            }
            if !folders_left.is_empty() {
                result.push(FlagMutation {
                    target: FlagTarget::AllMessagesInFolders {
                        excluded_refs: excluded_refs
                            .iter()
                            .filter(|message_ref| ref_in_folders(message_ref, &folders_left))
                            .cloned()
                            .collect(),
                        folder_paths: folders_left,
                    },
                    flags: overlapping_flags,
                    add: older.add,
                });
            }
            result
        }
    }
}

fn split_message_target(
    older: FlagMutation,
    old_refs: &[BackendMessageRef],
    covered_refs: &[BackendMessageRef],
    remaining_flags: Vec<String>,
) -> Vec<FlagMutation> {
    if covered_refs.is_empty() {
        return vec![older];
    }
    let refs_left: Vec<_> = old_refs
        .iter()
        .filter(|message_ref| !contains_ref(covered_refs, message_ref))
        .cloned()
        .collect();
    let mut result = Vec::new();
    if !refs_left.is_empty() {
        result.push(FlagMutation {
            target: FlagTarget::Messages(refs_left),
            flags: older.flags.clone(),
            add: older.add,
        });
    }
    if !remaining_flags.is_empty() {
        result.push(FlagMutation {
            target: FlagTarget::Messages(covered_refs.to_vec()),
            flags: remaining_flags,
            add: older.add,
        });
    }
    result
}

fn contains_ref(refs: &[BackendMessageRef], candidate: &BackendMessageRef) -> bool {
    refs.iter()
        .any(|message_ref| message_ref.same_message(candidate))
}

fn ref_in_folders(message_ref: &BackendMessageRef, folder_paths: &[String]) -> bool {
    matches!(
        message_ref,
        BackendMessageRef::Imap { folder_path, .. }
            if folder_paths.contains(folder_path)
    )
}

fn normalize_flag(flag: &str) -> String {
    flag.trim_start_matches('\\').to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_read_uses_server_wide_imap_target() {
        let target = FlagTarget::for_mark_account_read(
            "imap",
            vec!["INBOX".into(), "Archive".into()],
            vec![BackendMessageRef::imap("INBOX", 7)],
        );

        assert_eq!(
            target,
            FlagTarget::AllMessagesInFolders {
                folder_paths: vec!["INBOX".into(), "Archive".into()],
                excluded_refs: Vec::new(),
            }
        );
    }

    #[test]
    fn account_read_uses_known_messages_for_http_providers() {
        for protocol in ["graph", "jmap"] {
            let refs = vec![BackendMessageRef::graph("opaque_with_under")];
            assert_eq!(
                FlagTarget::for_mark_account_read(protocol, vec!["INBOX".into()], refs.clone()),
                FlagTarget::Messages(refs)
            );
        }
    }

    #[test]
    fn account_read_scope_avoids_unneeded_provider_queries() {
        assert_eq!(
            FlagTarget::account_read_scope("imap"),
            AccountReadScope::AllKnownFolders
        );
        assert_eq!(
            FlagTarget::account_read_scope("graph"),
            AccountReadScope::KnownUnreadMessages
        );
    }

    #[test]
    fn account_read_keeps_unknown_protocol_imap_fallback() {
        assert!(matches!(
            FlagTarget::for_mark_account_read("legacy", vec!["INBOX".into()], Vec::new()),
            FlagTarget::AllMessagesInFolders { .. }
        ));
    }

    #[test]
    fn deletion_removes_only_matching_explicit_refs() {
        let mutation = FlagMutation {
            target: FlagTarget::messages(vec![
                BackendMessageRef::jmap("inbox", "email_1"),
                BackendMessageRef::jmap("inbox", "email_2"),
            ]),
            flags: vec!["seen".into()],
            add: true,
        };

        let remaining =
            remove_deleted_refs(mutation, &[BackendMessageRef::jmap("archive", "email_1")])
                .unwrap();
        assert_eq!(
            remaining.target.message_refs().unwrap(),
            &[BackendMessageRef::jmap("inbox", "email_2")]
        );
    }
}
