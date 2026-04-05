use lettre::message::{header::ContentType, Attachment, MultiPart, SinglePart, Mailbox};
use lettre::transport::smtp::authentication::Credentials;
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
        let attachment = Attachment::new(att.name.clone())
            .body(att.data.clone(), ct);
        mixed = mixed.singlepart(attachment);
    }

    Ok(mixed)
}

/// Send an email message via SMTP.
pub async fn send_message(
    smtp_host: &str,
    smtp_port: u16,
    username: &str,
    password: &str,
    use_tls: bool,
    from: &str,
    to: &[String],
    cc: &[String],
    bcc: &[String],
    subject: &str,
    body_text: &str,
    body_html: Option<&str>,
    attachments: &[AttachmentData],
) -> Result<()> {
    log::info!(
        "SMTP sending message from {} to {:?} via {}:{} ({} attachments)",
        from, to, smtp_host, smtp_port, attachments.len()
    );

    let from_mailbox: Mailbox = from
        .parse()
        .map_err(|e| Error::Other(format!("Invalid 'from' address '{}': {}", from, e)))?;

    let mut builder = Message::builder()
        .from(from_mailbox)
        .subject(subject);

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

    let body = build_body(body_text, body_html, attachments)
        .map_err(|e| Error::Other(format!("Failed to build body: {}", e)))?;

    let message = builder
        .multipart(body)
        .map_err(|e| Error::Other(format!("Failed to build message: {}", e)))?;

    let creds = Credentials::new(username.to_string(), password.to_string());

    let transport = if smtp_port == 587 {
        log::debug!("SMTP using STARTTLS on port 587");
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(smtp_host)
            .map_err(|e| Error::Other(format!("SMTP STARTTLS relay setup failed: {}", e)))?
            .port(smtp_port)
            .credentials(creds)
            .build()
    } else if use_tls || smtp_port == 465 {
        log::debug!("SMTP using implicit TLS on port {}", smtp_port);
        AsyncSmtpTransport::<Tokio1Executor>::relay(smtp_host)
            .map_err(|e| Error::Other(format!("SMTP TLS relay setup failed: {}", e)))?
            .port(smtp_port)
            .credentials(creds)
            .build()
    } else {
        log::debug!("SMTP using STARTTLS (default) on port {}", smtp_port);
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(smtp_host)
            .map_err(|e| Error::Other(format!("SMTP relay setup failed: {}", e)))?
            .port(smtp_port)
            .credentials(creds)
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
pub fn build_raw_message(
    from: &str,
    to: &[String],
    cc: &[String],
    bcc: &[String],
    subject: &str,
    body_text: &str,
    body_html: Option<&str>,
    attachments: &[AttachmentData],
) -> Result<Vec<u8>> {
    let from_mailbox: Mailbox = from
        .parse()
        .map_err(|e| Error::Other(format!("Invalid 'from' address '{}': {}", from, e)))?;

    let mut builder = Message::builder().from(from_mailbox).subject(subject);

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

    let body = build_body(body_text, body_html, attachments)
        .map_err(|e| Error::Other(format!("Failed to build body: {}", e)))?;

    let message = builder
        .multipart(body)
        .map_err(|e| Error::Other(format!("Failed to build message: {}", e)))?;

    Ok(message.formatted())
}
