use mail_parser::{Address as MailAddress, HeaderValue, MessageParser, MimeHeaders};

use crate::db::messages::{Address, Attachment, MessageBody, NewMessage};

fn mail_address_to_list(addr: &MailAddress<'_>) -> Vec<Address> {
    match addr {
        MailAddress::List(list) => list
            .iter()
            .map(|a| Address {
                name: a.name.as_ref().map(|s| s.to_string()),
                email: a.address.as_ref().map(|s| s.to_string()).unwrap_or_default(),
            })
            .collect(),
        MailAddress::Group(groups) => groups
            .iter()
            .flat_map(|g| {
                g.addresses.iter().map(|a| Address {
                    name: a.name.as_ref().map(|s| s.to_string()),
                    email: a.address.as_ref().map(|s| s.to_string()).unwrap_or_default(),
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
            ct.ctype() == "multipart"
                && ct.subtype().map(|s| s == "encrypted").unwrap_or(false)
        })
        .unwrap_or(false);
    let is_signed = parsed
        .content_type()
        .map(|ct| {
            ct.ctype() == "multipart"
                && ct.subtype().map(|s| s == "signed").unwrap_or(false)
        })
        .unwrap_or(false);

    let id = format!("{}_{}_{}", account_id, folder_path, uid);

    Some(NewMessage {
        id,
        account_id: account_id.to_string(),
        folder_path: folder_path.to_string(),
        uid,
        message_id,
        in_reply_to,
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
    let date = parsed
        .date()
        .map(|d| d.to_rfc3339())
        .unwrap_or_default();

    let body_html = parsed.body_html(0).map(|s| {
        ammonia::Builder::default()
            .add_generic_attributes(&["style"])
            .rm_tags(&["img"])  // Strip all images — no remote content
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
                .map(|ct| {
                    format!(
                        "{}/{}",
                        ct.ctype(),
                        ct.subtype().unwrap_or("octet-stream")
                    )
                })
                .unwrap_or_else(|| "application/octet-stream".to_string());
            Attachment {
                index: i as u32,
                filename,
                content_type,
                size: att.len() as u64,
            }
        })
        .collect();

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
    })
}
