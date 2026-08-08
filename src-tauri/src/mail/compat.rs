//! Compatibility types for persisted provider message references and bodies.

/// A provider-specific message identity.
///
/// Database ids retain their existing underscore-delimited representation.
/// Parsing therefore requires the account and, for JMAP, mailbox context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendMessageRef {
    Imap {
        folder_path: String,
        uid: u32,
    },
    Jmap {
        mailbox_id: String,
        email_id: String,
    },
    Graph {
        item_id: String,
    },
}

impl BackendMessageRef {
    pub fn imap(folder_path: impl Into<String>, uid: u32) -> Self {
        Self::Imap {
            folder_path: folder_path.into(),
            uid,
        }
    }

    pub fn jmap(mailbox_id: impl Into<String>, email_id: impl Into<String>) -> Self {
        Self::Jmap {
            mailbox_id: mailbox_id.into(),
            email_id: email_id.into(),
        }
    }

    pub fn graph(item_id: impl Into<String>) -> Self {
        Self::Graph {
            item_id: item_id.into(),
        }
    }

    /// Recover a Graph item id from the existing `{account_id}_{item_id}` form.
    ///
    /// Raw ids remain accepted for compatibility with older call sites.
    pub fn graph_from_db_id(account_id: &str, db_id: &str) -> Self {
        let prefix = format!("{}_", account_id);
        Self::graph(db_id.strip_prefix(&prefix).unwrap_or(db_id))
    }

    /// Recover a JMAP email id by stripping the exact known prefix.
    ///
    /// JMAP ids are opaque and may contain underscores, so delimiter splitting
    /// cannot recover this identity safely.
    pub fn jmap_from_db_id(account_id: &str, mailbox_id: &str, db_id: &str) -> Option<Self> {
        let prefix = format!("{}_{}_", account_id, mailbox_id);
        db_id
            .strip_prefix(&prefix)
            .map(|email_id| Self::jmap(mailbox_id, email_id))
    }

    /// Format the existing persisted `messages.id` representation.
    pub fn to_db_id(&self, account_id: &str) -> String {
        match self {
            Self::Imap { folder_path, uid } => {
                format!("{}_{}_{}", account_id, folder_path, uid)
            }
            Self::Jmap {
                mailbox_id,
                email_id,
            } => format!("{}_{}_{}", account_id, mailbox_id, email_id),
            Self::Graph { item_id } => format!("{}_{}", account_id, item_id),
        }
    }

    pub fn graph_item_id(&self) -> Option<&str> {
        match self {
            Self::Graph { item_id } => Some(item_id),
            _ => None,
        }
    }

    pub fn into_graph_item_id(self) -> Option<String> {
        match self {
            Self::Graph { item_id } => Some(item_id),
            _ => None,
        }
    }

    pub fn jmap_email_id(&self) -> Option<&str> {
        match self {
            Self::Jmap { email_id, .. } => Some(email_id),
            _ => None,
        }
    }

    pub fn into_jmap_email_id(self) -> Option<String> {
        match self {
            Self::Jmap { email_id, .. } => Some(email_id),
            _ => None,
        }
    }
}

/// The meaning of a persisted `messages.maildir_path` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyLocation {
    NotFetched,
    Local(String),
    GraphRemote(String),
}

impl BodyLocation {
    pub fn from_persisted(value: &str) -> Self {
        if value.is_empty() {
            Self::NotFetched
        } else if let Some(item_id) = value.strip_prefix("graph:") {
            Self::GraphRemote(item_id.to_string())
        } else {
            Self::Local(value.to_string())
        }
    }

    pub fn to_persisted(&self) -> String {
        match self {
            Self::NotFetched => String::new(),
            Self::Local(path) => path.clone(),
            Self::GraphRemote(item_id) => format!("graph:{}", item_id),
        }
    }

    pub fn local_path(&self) -> Option<&str> {
        match self {
            Self::Local(path) => Some(path),
            _ => None,
        }
    }

    pub fn graph_item_id(&self) -> Option<&str> {
        match self {
            Self::GraphRemote(item_id) => Some(item_id),
            _ => None,
        }
    }

    pub fn needs_fetch(&self) -> bool {
        !matches!(self, Self::Local(_))
    }
}

#[cfg(test)]
mod tests {
    use super::{BackendMessageRef, BodyLocation};

    #[test]
    fn formats_existing_database_ids() {
        assert_eq!(
            BackendMessageRef::imap("Archive_2026", 42).to_db_id("acc_1"),
            "acc_1_Archive_2026_42"
        );
        assert_eq!(
            BackendMessageRef::jmap("parent_child", "email_with_underscores").to_db_id("acc_1"),
            "acc_1_parent_child_email_with_underscores"
        );
        assert_eq!(
            BackendMessageRef::graph("AAMk_opaque").to_db_id("acc_1"),
            "acc_1_AAMk_opaque"
        );
    }

    #[test]
    fn parses_graph_database_ids_with_legacy_raw_fallback() {
        let prefixed = BackendMessageRef::graph_from_db_id("acc_1", "acc_1_AAMk_opaque");
        assert_eq!(prefixed.graph_item_id(), Some("AAMk_opaque"));

        let raw = BackendMessageRef::graph_from_db_id("acc_1", "AAMk_opaque");
        assert_eq!(raw.graph_item_id(), Some("AAMk_opaque"));
    }

    #[test]
    fn parses_jmap_ids_using_the_known_prefix() {
        let parsed = BackendMessageRef::jmap_from_db_id(
            "acc_1",
            "parent_child",
            "acc_1_parent_child_email_with_underscores",
        )
        .unwrap();
        assert_eq!(parsed.jmap_email_id(), Some("email_with_underscores"));
    }

    #[test]
    fn rejects_jmap_ids_with_the_wrong_context() {
        assert!(
            BackendMessageRef::jmap_from_db_id("acc_1", "inbox", "different_inbox_email").is_none()
        );
        assert!(
            BackendMessageRef::jmap_from_db_id("acc_1", "inbox", "acc_1_archive_email").is_none()
        );
    }

    #[test]
    fn body_locations_round_trip_existing_values() {
        for persisted in ["", "graph:AAMk_opaque", "graph:", "acc/inbox/cur/42:2,S"] {
            assert_eq!(
                BodyLocation::from_persisted(persisted).to_persisted(),
                persisted
            );
        }
    }

    #[test]
    fn classifies_body_locations_without_validating_paths() {
        let not_fetched = BodyLocation::from_persisted("");
        assert!(not_fetched.needs_fetch());
        assert_eq!(not_fetched.local_path(), None);

        let graph = BodyLocation::from_persisted("graph:AAMk_opaque");
        assert!(graph.needs_fetch());
        assert_eq!(graph.graph_item_id(), Some("AAMk_opaque"));

        let local = BodyLocation::from_persisted("future:value");
        assert!(!local.needs_fetch());
        assert_eq!(local.local_path(), Some("future:value"));
    }
}
