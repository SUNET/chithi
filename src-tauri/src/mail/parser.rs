use mail_parser::{Address as MailAddress, HeaderValue, MessageParser, MimeHeaders};

use crate::db::messages::NewMessage;
use crate::mail::compat::BackendMessageRef;
use crate::message::{Address, Attachment, MessageBody};

fn mail_address_to_list(addr: &MailAddress<'_>) -> Vec<Address> {
    match addr {
        MailAddress::List(list) => list
            .iter()
            .map(|a| Address {
                name: a.name.as_ref().map(|s| s.to_string()),
                email: a
                    .address
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            })
            .collect(),
        MailAddress::Group(groups) => groups
            .iter()
            .flat_map(|g| {
                g.addresses.iter().map(|a| Address {
                    name: a.name.as_ref().map(|s| s.to_string()),
                    email: a
                        .address
                        .as_ref()
                        .map(|s| s.to_string())
                        .unwrap_or_default(),
                })
            })
            .collect(),
    }
}

/// Parse a raw RFC 5322 message into metadata for indexing.
pub fn parse_envelope(
    account_id: &str,
    folder_path: &str,
    uid: u32,
    raw: &[u8],
    maildir_path: &str,
) -> Option<NewMessage> {
    let parsed = MessageParser::default().parse(raw)?;

    let from_list = parsed
        .from()
        .map(|a| mail_address_to_list(a))
        .unwrap_or_default();
    let from_name = from_list.first().and_then(|a| a.name.clone());
    let from_email = from_list
        .first()
        .map(|a| a.email.clone())
        .unwrap_or_else(|| "unknown".to_string());

    let message_id = parsed.message_id().map(|s| s.to_string());
    let in_reply_to = match parsed.in_reply_to() {
        HeaderValue::Text(t) => Some(t.to_string()),
        HeaderValue::TextList(list) => list.first().map(|s| s.to_string()),
        _ => None,
    };

    let subject = parsed.subject().map(|s| s.to_string());

    let to_list = parsed
        .to()
        .map(|a| mail_address_to_list(a))
        .unwrap_or_default();
    let cc_list = parsed
        .cc()
        .map(|a| mail_address_to_list(a))
        .unwrap_or_default();

    let date = parsed
        .date()
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

    let body_text = parsed.body_text(0).map(|s| s.to_string());
    let snippet = body_text
        .as_ref()
        .map(|t| t.chars().take(200).collect::<String>());

    let has_attachments = parsed.attachment_count() > 0;
    let is_encrypted = parsed
        .content_type()
        .map(|ct| {
            ct.ctype() == "multipart" && ct.subtype().map(|s| s == "encrypted").unwrap_or(false)
        })
        .unwrap_or(false);
    let is_signed = parsed
        .content_type()
        .map(|ct| ct.ctype() == "multipart" && ct.subtype().map(|s| s == "signed").unwrap_or(false))
        .unwrap_or(false);

    let id = BackendMessageRef::imap(folder_path, uid).to_db_id(account_id);

    Some(NewMessage {
        id,
        account_id: account_id.to_string(),
        folder_path: folder_path.to_string(),
        uid,
        message_id,
        in_reply_to,
        thread_id: None, // Thread ID is computed separately during sync
        subject,
        from_name,
        from_email,
        to_addresses: serde_json::to_string(&to_list).unwrap_or_default(),
        cc_addresses: serde_json::to_string(&cc_list).unwrap_or_default(),
        date,
        size: raw.len() as u64,
        has_attachments,
        is_encrypted,
        is_signed,
        flags: "[]".to_string(),
        maildir_path: maildir_path.to_string(),
        snippet,
    })
}

/// Parse a raw message into a full MessageBody for the reader view.
pub fn parse_message_body(
    message_id: &str,
    raw: &[u8],
    from_email_hint: &str,
    to_json: &str,
    cc_json: &str,
    flags_json: &str,
    is_encrypted: bool,
    is_signed: bool,
) -> Option<MessageBody> {
    let parsed = MessageParser::default().parse(raw)?;

    let from_list = parsed
        .from()
        .map(|a| mail_address_to_list(a))
        .unwrap_or_default();
    let from_addr = from_list.into_iter().next().unwrap_or(Address {
        name: None,
        email: from_email_hint.to_string(),
    });

    let to: Vec<Address> = serde_json::from_str(to_json).unwrap_or_default();
    let cc: Vec<Address> = serde_json::from_str(cc_json).unwrap_or_default();
    let flags: Vec<String> = serde_json::from_str(flags_json).unwrap_or_default();

    let subject = parsed.subject().map(|s| s.to_string());
    let date = parsed.date().map(|d| d.to_rfc3339()).unwrap_or_default();

    // Grab raw HTML once for both image detection and sanitization
    let raw_html = parsed.body_html(0);

    // Check for remote images before sanitization strips <img> tags.
    // Only match https:// to align with the loading pipeline (parse_html_with_images
    // only allows https URL scheme).
    let has_remote_images = raw_html
        .as_ref()
        .map(|s| {
            use std::sync::LazyLock;
            static RE: LazyLock<regex::Regex> = LazyLock::new(|| {
                regex::Regex::new(r#"(?i)<img\b[^>]*\bsrc\s*=\s*["']https://"#).unwrap()
            });
            RE.is_match(s)
        })
        .unwrap_or(false);

    let body_html = raw_html.map(|s| {
        let url_schemes = std::collections::HashSet::from(["http", "https", "mailto"]);
        ammonia::Builder::default()
            .add_generic_attributes(&["style"])
            .rm_tags(&[
                "img", "object", "embed", "iframe", "form", "input", "button", "textarea",
                "select", "video", "audio", "source", "svg", "math",
            ])
            .url_schemes(url_schemes)
            .clean(&s)
            .to_string()
    });
    let body_text = parsed.body_text(0).map(|s| s.to_string());

    let attachments: Vec<Attachment> = parsed
        .attachments()
        .enumerate()
        .map(|(i, att)| {
            let filename = att.attachment_name().map(|s| s.to_string());
            let content_type = att
                .content_type()
                .map(|ct| format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or("octet-stream")))
                .unwrap_or_else(|| "application/octet-stream".to_string());
            Attachment {
                index: i as u32,
                filename,
                content_type,
                size: att.len() as u64,
            }
        })
        .collect();

    let list_id = match parsed.list_id() {
        HeaderValue::Text(t) => Some(t.to_string()),
        _ => None,
    };

    let pgp_kind = crate::mail::pgp_mime::detect_kind(raw);
    Some(MessageBody {
        id: message_id.to_string(),
        subject,
        from: from_addr,
        to,
        cc,
        date,
        flags,
        body_html,
        body_text,
        attachments,
        is_encrypted,
        is_signed,
        list_id,
        has_remote_images,
        pgp_kind,
    })
}

/// Re-parse a raw message body allowing `<img>` tags.
/// All other dangerous tags remain stripped. Used for the "Load remote images" feature.
/// The sandboxed iframe's CSP (`img-src https: data:`) is the enforcement layer
/// that blocks non-HTTPS image loads at the browser level.
pub fn parse_html_with_images(raw: &[u8]) -> Option<String> {
    let parsed = MessageParser::default().parse(raw)?;
    parsed.body_html(0).map(|s| {
        let url_schemes = std::collections::HashSet::from(["https", "mailto"]);
        ammonia::Builder::default()
            .add_generic_attributes(&["style"])
            .add_tags(&["img"])
            .add_tag_attributes("img", &["src", "alt", "width", "height", "style"])
            .rm_tags(&[
                "object", "embed", "iframe", "form", "input", "button", "textarea", "select",
                "video", "audio", "source", "svg", "math",
            ])
            .url_schemes(url_schemes)
            .clean(&s)
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_envelope, parse_html_with_images, parse_message_body};
    use crate::db::messages::NewMessage;
    use crate::message::{Address, MessageBody};

    const ADDRESSES: &[u8] = include_bytes!("../../../tests/fixtures/eai-test-messages/addresses");
    const ATTACHMENT: &[u8] =
        include_bytes!("../../../tests/fixtures/eai-test-messages/attachment");
    const FROM: &[u8] = include_bytes!("../../../tests/fixtures/eai-test-messages/from");
    const MIMEFIELD: &[u8] = include_bytes!("../../../tests/fixtures/eai-test-messages/mimefield");
    const NOT_EMOJI: &[u8] = include_bytes!("../../../tests/fixtures/eai-test-messages/not-emoji");
    const PUNYCODE: &[u8] = include_bytes!("../../../tests/fixtures/eai-test-messages/punycode");

    fn envelope(raw: &[u8]) -> NewMessage {
        parse_envelope("account", "INBOX", 1, raw, "message.eml").expect("EAI message should parse")
    }

    fn body(raw: &[u8]) -> MessageBody {
        let envelope = envelope(raw);
        parse_message_body(
            &envelope.id,
            raw,
            &envelope.from_email,
            &envelope.to_addresses,
            &envelope.cc_addresses,
            &envelope.flags,
            envelope.is_encrypted,
            envelope.is_signed,
        )
        .expect("EAI message body should parse")
    }

    fn addresses(json: &str) -> Vec<Address> {
        serde_json::from_str(json).expect("parser should emit valid address JSON")
    }

    fn html_message(html: &str) -> Vec<u8> {
        format!(
            "From: sender@example.org\r\n\
             To: recipient@example.org\r\n\
             Subject: Sanitizer test\r\n\
             Message-ID: <sanitizer@example.org>\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: text/html; charset=utf-8\r\n\r\n\
             {html}"
        )
        .into_bytes()
    }

    fn assert_html_is_removed(raw: &[u8], forbidden: &[&str]) {
        let normal = body(raw).body_html.expect("HTML body should parse");
        let with_images = parse_html_with_images(raw).expect("HTML body should parse");
        for value in forbidden {
            assert!(!normal.contains(value), "normal HTML retained {value}");
            assert!(
                !with_images.contains(value),
                "image-enabled HTML retained {value}"
            );
        }
    }

    #[test]
    fn parses_unicode_addresses_in_structured_headers() {
        let message = envelope(ADDRESSES);

        assert_eq!(message.from_name.as_deref(), Some("Jøran Øygårdvær"));
        assert_eq!(message.from_email, "jøran@example.com");
        let cc = addresses(&message.cc_addresses);
        assert_eq!(cc.len(), 1);
        assert_eq!(cc[0].name.as_deref(), Some("Jøran Øygårdvær"));
        assert_eq!(cc[0].email, "jøran@example.com");
    }

    #[test]
    fn parses_unicode_from_header() {
        let message = body(FROM);

        assert_eq!(message.from.name.as_deref(), Some("Jøran Øygårdvær"));
        assert_eq!(message.from.email, "jøran@example.com");
        assert_eq!(
            message.body_text.as_deref().map(str::trim_end),
            Some("asdf")
        );
    }

    #[test]
    fn preserves_punycode_domains_and_unicode_local_parts() {
        let message = envelope(PUNYCODE);
        let to = addresses(&message.to_addresses);

        assert_eq!(message.from_email, "info@xn--dmi-0na.fo");
        assert_eq!(to[0].email, "dømi@xn--dmi-0na.fo");
    }

    #[test]
    fn does_not_decode_punycode_like_local_part() {
        let message = envelope(NOT_EMOJI);

        assert_eq!(message.from_email, "xn--ls8ha@outlook.com");
    }

    #[test]
    fn parses_unicode_filename_on_single_part_attachment() {
        let message = body(MIMEFIELD);

        assert_eq!(message.attachments.len(), 1);
        assert_eq!(
            message.attachments[0].filename.as_deref(),
            Some("blåbærsyltetøy")
        );
        assert_eq!(message.attachments[0].content_type, "text/plain");
    }

    #[test]
    fn parses_unicode_filename_on_multipart_attachment() {
        let message = body(ATTACHMENT);

        assert_eq!(message.attachments.len(), 1);
        assert_eq!(
            message.attachments[0].filename.as_deref(),
            Some("blåbærsyltetøy")
        );
        assert_eq!(message.attachments[0].content_type, "image/jpeg");
        assert!(message.attachments[0].size > 1_000);
    }

    #[test]
    fn strips_svg_animation_xss_from_rustsec_2026_0213() {
        let raw = html_message(
            r#"<svg xmlns="http://www.w3.org/2000/svg">
                <a><set attributeName="href" to="javascript:alert('xss')"></set>
                <text>Click</text></a>
            </svg>"#,
        );

        assert_html_is_removed(&raw, &["<svg", "<set", "javascript:"]);
    }

    #[test]
    fn strips_mathml_mxss_from_rustsec_2026_0193() {
        let raw = html_message(
            r#"<math><annotation-xml encoding="text/html">
                <style><!--</style><img src=x onerror=alert('xss')>
            </annotation-xml></math>"#,
        );

        assert_html_is_removed(&raw, &["<math", "<annotation-xml", "onerror"]);
    }
}
