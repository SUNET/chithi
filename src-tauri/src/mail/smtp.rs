use lettre::message::{header::ContentType, Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::error::{Error, Result};

/// Attachment data ready to embed in a message.
pub struct AttachmentData {
    pub name: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// Build the message body, optionally wrapping in multipart/mixed if there are attachments.
fn build_body(
    body_text: &str,
    body_html: Option<&str>,
    attachments: &[AttachmentData],
) -> std::result::Result<MultiPart, String> {
    // Text body (or text+html alternative)
    let text_part = if let Some(html) = body_html {
        MultiPart::alternative()
            .singlepart(
                SinglePart::builder()
                    .header(ContentType::TEXT_PLAIN)
                    .body(body_text.to_string()),
            )
            .singlepart(
                SinglePart::builder()
                    .header(ContentType::TEXT_HTML)
                    .body(html.to_string()),
            )
    } else {
        MultiPart::alternative().singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_PLAIN)
                .body(body_text.to_string()),
        )
    };

    if attachments.is_empty() {
        return Ok(text_part);
    }

    // Wrap in multipart/mixed with attachments
    let mut mixed = MultiPart::mixed().multipart(text_part);
    for att in attachments {
        let ct = ContentType::parse(&att.content_type).unwrap_or(ContentType::TEXT_PLAIN);
        let attachment = Attachment::new(att.name.clone()).body(att.data.clone(), ct);
        mixed = mixed.singlepart(attachment);
    }

    Ok(mixed)
}

/// Send an email message via SMTP.
///
/// `in_reply_to` and `references` carry RFC 5322 threading headers,
/// already wrapped in angle brackets. Without these the receiving
/// client cannot link the new message to its parent.
#[allow(clippy::too_many_arguments)]
pub async fn send_message(
    smtp_host: &str,
    smtp_port: u16,
    username: &str,
    password: &str,
    use_tls: bool,
    use_xoauth2: bool,
    from: &str,
    to: &[String],
    cc: &[String],
    bcc: &[String],
    subject: &str,
    body_text: &str,
    body_html: Option<&str>,
    attachments: &[AttachmentData],
    in_reply_to: Option<&str>,
    references: &[String],
) -> Result<()> {
    log::info!(
        "SMTP sending message from {} to {:?} via {}:{} ({} attachments, threading={})",
        from,
        to,
        smtp_host,
        smtp_port,
        attachments.len(),
        in_reply_to.is_some(),
    );

    let from_mailbox: Mailbox = from
        .parse()
        .map_err(|e| Error::Other(format!("Invalid 'from' address '{}': {}", from, e)))?;

    // `message_id(None)` makes lettre emit a generated <UUID@host>;
    // without it lettre never adds the header and the next reply
    // has nothing to point In-Reply-To at.
    let mut builder = Message::builder()
        .from(from_mailbox)
        .subject(subject)
        .message_id(None);

    for addr in to {
        let mailbox: Mailbox = addr
            .parse()
            .map_err(|e| Error::Other(format!("Invalid 'to' address '{}': {}", addr, e)))?;
        builder = builder.to(mailbox);
    }
    for addr in cc {
        let mailbox: Mailbox = addr
            .parse()
            .map_err(|e| Error::Other(format!("Invalid 'cc' address '{}': {}", addr, e)))?;
        builder = builder.cc(mailbox);
    }
    for addr in bcc {
        let mailbox: Mailbox = addr
            .parse()
            .map_err(|e| Error::Other(format!("Invalid 'bcc' address '{}': {}", addr, e)))?;
        builder = builder.bcc(mailbox);
    }

    if let Some(irt) = in_reply_to {
        let trimmed = irt.trim();
        if !trimmed.is_empty() {
            builder = builder.in_reply_to(trimmed.to_string());
        }
    }
    if !references.is_empty() {
        let joined = references
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !joined.is_empty() {
            builder = builder.references(joined);
        }
    }

    let body = build_body(body_text, body_html, attachments)
        .map_err(|e| Error::Other(format!("Failed to build body: {}", e)))?;

    let message = builder
        .multipart(body)
        .map_err(|e| Error::Other(format!("Failed to build message: {}", e)))?;

    let creds = Credentials::new(username.to_string(), password.to_string());
    let auth_mechanisms = if use_xoauth2 {
        vec![Mechanism::Xoauth2]
    } else {
        vec![Mechanism::Plain, Mechanism::Login]
    };

    let transport = if smtp_port == 587 {
        log::debug!("SMTP using STARTTLS on port 587 (xoauth2={})", use_xoauth2);
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(smtp_host)
            .map_err(|e| Error::Other(format!("SMTP STARTTLS relay setup failed: {}", e)))?
            .port(smtp_port)
            .credentials(creds)
            .authentication(auth_mechanisms)
            .build()
    } else if use_tls || smtp_port == 465 {
        log::debug!("SMTP using implicit TLS on port {}", smtp_port);
        AsyncSmtpTransport::<Tokio1Executor>::relay(smtp_host)
            .map_err(|e| Error::Other(format!("SMTP TLS relay setup failed: {}", e)))?
            .port(smtp_port)
            .credentials(creds)
            .authentication(auth_mechanisms)
            .build()
    } else {
        log::debug!("SMTP using STARTTLS (default) on port {}", smtp_port);
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(smtp_host)
            .map_err(|e| Error::Other(format!("SMTP relay setup failed: {}", e)))?
            .port(smtp_port)
            .credentials(creds)
            .authentication(auth_mechanisms)
            .build()
    };

    let response = transport.send(message).await.map_err(|e| {
        log::error!("SMTP send failed: {}", e);
        Error::Other(format!("SMTP send failed: {}", e))
    })?;

    log::info!(
        "SMTP message sent successfully: {} (code {})",
        response.message().collect::<Vec<_>>().join(", "),
        response.code()
    );

    Ok(())
}

/// Build a raw RFC5322 message (for JMAP submission).
///
/// `in_reply_to` and `references` carry the threading headers. The id
/// strings should arrive WITH their angle brackets — lettre stores them
/// verbatim in the In-Reply-To / References header values. References
/// is rendered as a single space-separated header value.
#[allow(clippy::too_many_arguments)]
pub fn build_raw_message(
    from: &str,
    to: &[String],
    cc: &[String],
    bcc: &[String],
    subject: &str,
    body_text: &str,
    body_html: Option<&str>,
    attachments: &[AttachmentData],
    in_reply_to: Option<&str>,
    references: &[String],
) -> Result<Vec<u8>> {
    let from_mailbox: Mailbox = from
        .parse()
        .map_err(|e| Error::Other(format!("Invalid 'from' address '{}': {}", from, e)))?;

    // Always emit a Message-ID. Lettre's `build()` does NOT add one
    // automatically, so without this, our outgoing replies have no
    // Message-ID for the next reply to thread off of. `message_id(None)`
    // generates `<UUID@hostname>` per RFC 5322 §3.6.4.
    let mut builder = Message::builder()
        .from(from_mailbox)
        .subject(subject)
        .message_id(None);

    for addr in to {
        let mailbox: Mailbox = addr
            .parse()
            .map_err(|e| Error::Other(format!("Invalid 'to' address '{}': {}", addr, e)))?;
        builder = builder.to(mailbox);
    }
    for addr in cc {
        let mailbox: Mailbox = addr
            .parse()
            .map_err(|e| Error::Other(format!("Invalid 'cc' address '{}': {}", addr, e)))?;
        builder = builder.cc(mailbox);
    }
    for addr in bcc {
        let mailbox: Mailbox = addr
            .parse()
            .map_err(|e| Error::Other(format!("Invalid 'bcc' address '{}': {}", addr, e)))?;
        builder = builder.bcc(mailbox);
    }

    if let Some(irt) = in_reply_to {
        let trimmed = irt.trim();
        if !trimmed.is_empty() {
            builder = builder.in_reply_to(trimmed.to_string());
        }
    }
    if !references.is_empty() {
        // RFC 5322 References is a single header whose value is the chain
        // of message-ids separated by whitespace, oldest first.
        let joined = references
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !joined.is_empty() {
            builder = builder.references(joined);
        }
    }

    let body = build_body(body_text, body_html, attachments)
        .map_err(|e| Error::Other(format!("Failed to build body: {}", e)))?;

    let message = builder
        .multipart(body)
        .map_err(|e| Error::Other(format!("Failed to build message: {}", e)))?;

    Ok(message.formatted())
}
