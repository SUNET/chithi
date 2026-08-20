//! Provider-neutral message models and identity rules.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Address {
    pub name: Option<String>,
    pub email: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Attachment {
    pub index: u32,
    pub filename: Option<String>,
    pub content_type: String,
    pub size: u64,
}

/// Detected PGP shape surfaced through the message reader contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PgpKind {
    MimeEncrypted,
    MimeSigned,
    InlineArmor,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageBody {
    pub id: String,
    pub subject: Option<String>,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub date: String,
    pub flags: Vec<String>,
    pub body_html: Option<String>,
    pub body_text: Option<String>,
    pub attachments: Vec<Attachment>,
    pub is_encrypted: bool,
    pub is_signed: bool,
    pub list_id: Option<String>,
    pub has_remote_images: bool,
    /// Detected PGP shape, omitted for plain mail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pgp_kind: Option<PgpKind>,
}

/// Provider-neutral inputs for server-side mail search.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchQuery {
    pub text: String,
    #[serde(default)]
    pub fields: SearchFields,
    #[serde(default)]
    pub has_attachment: Option<bool>,
    #[serde(default)]
    pub since_days: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchFields {
    pub subject: bool,
    pub from: bool,
    pub to: bool,
    pub body: bool,
}

impl Default for SearchFields {
    fn default() -> Self {
        Self {
            subject: true,
            from: true,
            to: true,
            body: true,
        }
    }
}

impl SearchFields {
    pub fn all_enabled(&self) -> bool {
        self.subject && self.from && self.to && self.body
    }

    pub fn any_enabled(&self) -> bool {
        self.subject || self.from || self.to || self.body
    }
}

/// Provider-neutral result returned from server-side mail search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub account_id: String,
    pub folder_path: String,
    pub uid: Option<u32>,
    pub message_id: Option<String>,
    pub backend_id: String,
    pub subject: String,
    pub from_name: Option<String>,
    pub from_email: Option<String>,
    pub date: i64,
    pub snippet: Option<String>,
}

/// A provider-specific message identity.
///
/// Database ids retain their existing underscore-delimited representation.
/// Parsing therefore requires the account and, for JMAP, mailbox context.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

    /// Recover a provider reference from an existing message database row.
    ///
    /// Unknown protocols retain the legacy IMAP fallback used by backend
    /// resolution. JMAP parsing needs the exact mailbox context because all
    /// components of its persisted id may contain underscores.
    pub fn from_db_row(
        protocol: &str,
        account_id: &str,
        db_id: &str,
        folder_path: &str,
        uid: u32,
    ) -> Option<Self> {
        match protocol {
            "graph" => Some(Self::graph_from_db_id(account_id, db_id)),
            "jmap" => Self::jmap_from_db_id(account_id, folder_path, db_id),
            _ => Some(Self::imap(folder_path, uid)),
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

    /// Whether two references identify the same provider object for message
    /// mutations. JMAP flags belong to the email object, which can appear in
    /// multiple mailboxes, so mailbox membership is not part of this check.
    pub fn same_message(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Jmap { email_id: left, .. },
                Self::Jmap {
                    email_id: right, ..
                },
            ) => left == right,
            _ => self == other,
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

/// Canonicalize an RFC 5322 Message-ID-shaped string for storage and lookup.
///
/// Many servers and parser paths provide slightly different shapes for the
/// same id. The stored form is always `<core>` with no whitespace.
pub fn normalize_message_id(value: &str) -> Option<String> {
    let core: String = value
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '<' && *character != '>')
        .collect();
    if core.is_empty() {
        return None;
    }
    Some(format!("<{}>", core))
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_message_id, Address, BackendMessageRef, BodyLocation, MessageBody, PgpKind,
        SearchHit, SearchQuery,
    };

    fn message_body(pgp_kind: Option<PgpKind>) -> MessageBody {
        MessageBody {
            id: "message-1".into(),
            subject: None,
            from: Address {
                name: None,
                email: "sender@example.com".into(),
            },
            to: Vec::new(),
            cc: Vec::new(),
            date: String::new(),
            flags: Vec::new(),
            body_html: None,
            body_text: None,
            attachments: Vec::new(),
            is_encrypted: false,
            is_signed: false,
            list_id: None,
            has_remote_images: false,
            pgp_kind,
        }
    }

    #[test]
    fn wraps_unwrapped_id() {
        assert_eq!(normalize_message_id("abc@host"), Some("<abc@host>".into()));
    }

    #[test]
    fn keeps_already_wrapped_id() {
        assert_eq!(
            normalize_message_id("<abc@host>"),
            Some("<abc@host>".into())
        );
    }

    #[test]
    fn strips_leading_whitespace() {
        assert_eq!(
            normalize_message_id(" <abc@host>"),
            Some("<abc@host>".into())
        );
    }

    #[test]
    fn strips_trailing_whitespace() {
        assert_eq!(
            normalize_message_id("<abc@host> "),
            Some("<abc@host>".into())
        );
    }

    #[test]
    fn strips_internal_whitespace_from_folded_headers() {
        assert_eq!(
            normalize_message_id("<abc@\n host>"),
            Some("<abc@host>".into())
        );
    }

    #[test]
    fn returns_none_for_empty() {
        assert_eq!(normalize_message_id(""), None);
    }

    #[test]
    fn returns_none_for_whitespace_only() {
        assert_eq!(normalize_message_id("   "), None);
    }

    #[test]
    fn returns_none_for_empty_brackets() {
        assert_eq!(normalize_message_id("<>"), None);
    }

    #[test]
    fn pgp_kind_serialization_preserves_reader_contract() {
        let cases = [
            (PgpKind::MimeEncrypted, r#""mimeEncrypted""#),
            (PgpKind::MimeSigned, r#""mimeSigned""#),
            (PgpKind::InlineArmor, r#""inlineArmor""#),
        ];

        for (kind, expected) in cases {
            assert_eq!(serde_json::to_string(&kind).unwrap(), expected);
        }
    }

    #[test]
    fn message_body_pgp_field_preserves_reader_contract() {
        let plain = serde_json::to_value(message_body(None)).unwrap();
        assert!(plain.get("pgp_kind").is_none());

        let encrypted = serde_json::to_value(message_body(Some(PgpKind::MimeEncrypted))).unwrap();
        assert_eq!(encrypted["pgp_kind"], "mimeEncrypted");
    }

    #[test]
    fn search_query_omitted_fields_use_all_enabled_default() {
        let query: SearchQuery = serde_json::from_value(serde_json::json!({
            "text": "invoice",
        }))
        .unwrap();

        assert_eq!(query.text, "invoice");
        assert!(query.fields.all_enabled());
        assert!(query.fields.any_enabled());
        assert_eq!(query.has_attachment, None);
        assert_eq!(query.since_days, None);
    }

    #[test]
    fn search_query_preserves_explicit_false_and_zero() {
        let query: SearchQuery = serde_json::from_value(serde_json::json!({
            "text": "invoice",
            "fields": {
                "from": true,
                "to": false,
                "subject": true,
                "body": false,
            },
            "has_attachment": false,
            "since_days": 0,
        }))
        .unwrap();

        assert!(query.fields.from);
        assert!(!query.fields.to);
        assert!(query.fields.subject);
        assert!(!query.fields.body);
        assert_eq!(query.has_attachment, Some(false));
        assert_eq!(query.since_days, Some(0));
    }

    #[test]
    fn search_hit_json_contract_is_stable() {
        let hit = SearchHit {
            account_id: "acc1".into(),
            folder_path: "INBOX".into(),
            uid: Some(42),
            message_id: None,
            backend_id: "INBOX:42".into(),
            subject: "Invoice".into(),
            from_name: None,
            from_email: Some("sender@example.com".into()),
            date: 1_700_000_000,
            snippet: None,
        };
        let expected = serde_json::json!({
            "account_id": "acc1",
            "folder_path": "INBOX",
            "uid": 42,
            "message_id": null,
            "backend_id": "INBOX:42",
            "subject": "Invoice",
            "from_name": null,
            "from_email": "sender@example.com",
            "date": 1_700_000_000,
            "snippet": null,
        });

        assert_eq!(serde_json::to_value(&hit).unwrap(), expected);

        let decoded: SearchHit = serde_json::from_value(expected).unwrap();
        assert_eq!(decoded.account_id, "acc1");
        assert_eq!(decoded.folder_path, "INBOX");
        assert_eq!(decoded.uid, Some(42));
        assert_eq!(decoded.message_id, None);
        assert_eq!(decoded.backend_id, "INBOX:42");
        assert_eq!(decoded.subject, "Invoice");
        assert_eq!(decoded.from_name, None);
        assert_eq!(decoded.from_email.as_deref(), Some("sender@example.com"));
        assert_eq!(decoded.date, 1_700_000_000);
        assert_eq!(decoded.snippet, None);

        let minimal: SearchHit = serde_json::from_value(serde_json::json!({
            "account_id": "acc1",
            "folder_path": "INBOX",
            "backend_id": "item",
            "subject": "Minimal",
            "date": 0,
        }))
        .unwrap();
        assert_eq!(minimal.uid, None);
        assert_eq!(minimal.message_id, None);
        assert_eq!(minimal.from_name, None);
        assert_eq!(minimal.from_email, None);
        assert_eq!(minimal.snippet, None);

        let all_null = SearchHit {
            account_id: "acc1".into(),
            folder_path: "INBOX".into(),
            uid: None,
            message_id: None,
            backend_id: "item".into(),
            subject: "Minimal".into(),
            from_name: None,
            from_email: None,
            date: 0,
            snippet: None,
        };
        assert_eq!(
            serde_json::to_value(&all_null).unwrap(),
            serde_json::json!({
                "account_id": "acc1",
                "folder_path": "INBOX",
                "uid": null,
                "message_id": null,
                "backend_id": "item",
                "subject": "Minimal",
                "from_name": null,
                "from_email": null,
                "date": 0,
                "snippet": null,
            })
        );
    }

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
    fn resolves_database_rows_by_protocol() {
        assert_eq!(
            BackendMessageRef::from_db_row("imap", "acc", "id", "INBOX", 7),
            Some(BackendMessageRef::imap("INBOX", 7))
        );
        assert_eq!(
            BackendMessageRef::from_db_row("jmap", "acc_1", "acc_1_box_one_mail_id", "box_one", 0,),
            Some(BackendMessageRef::jmap("box_one", "mail_id"))
        );
        assert_eq!(
            BackendMessageRef::from_db_row("graph", "acc_1", "acc_1_AAMk_id", "Inbox", 0),
            Some(BackendMessageRef::graph("AAMk_id"))
        );
    }

    #[test]
    fn unknown_and_empty_protocols_retain_imap_fallback() {
        for protocol in ["pop3", ""] {
            assert_eq!(
                BackendMessageRef::from_db_row(protocol, "acc", "id", "INBOX", 7),
                Some(BackendMessageRef::imap("INBOX", 7))
            );
        }
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
    fn jmap_message_identity_ignores_mailbox_membership() {
        assert!(BackendMessageRef::jmap("inbox", "email_1")
            .same_message(&BackendMessageRef::jmap("archive", "email_1")));
        assert!(!BackendMessageRef::jmap("inbox", "email_1")
            .same_message(&BackendMessageRef::jmap("inbox", "email_2")));
    }

    #[test]
    fn jmap_structural_identity_includes_mailbox_membership() {
        let inbox = BackendMessageRef::jmap("inbox", "email_1");
        let archive = BackendMessageRef::jmap("archive", "email_1");

        assert_ne!(inbox, archive);
        assert!(inbox.same_message(&archive));
        assert_eq!(std::collections::HashSet::from([inbox, archive]).len(), 2);
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

    #[test]
    fn body_location_graph_marker_is_case_sensitive() {
        let uppercase = BodyLocation::from_persisted("GRAPH:AAMk_opaque");

        assert_eq!(uppercase, BodyLocation::Local("GRAPH:AAMk_opaque".into()));
        assert!(!uppercase.needs_fetch());
    }
}
