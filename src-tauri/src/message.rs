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
    use super::{normalize_message_id, Address, MessageBody, PgpKind};

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
}
