use lettre::message::{header::ContentType, MultiPart, SinglePart, Mailbox};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::error::{Error, Result};

/// Send an email message via SMTP.
///
/// Connects to the given SMTP server, authenticates, and sends a message
/// constructed from the provided headers and body parts.
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
) -> Result<()> {
    log::info!(
        "SMTP sending message from {} to {:?} via {}:{}",
        from,
        to,
        smtp_host,
        smtp_port
    );

    // Parse the "from" address
    let from_mailbox: Mailbox = from
        .parse()
        .map_err(|e| Error::Other(format!("Invalid 'from' address '{}': {}", from, e)))?;

    // Start building the message
    let mut builder = Message::builder()
        .from(from_mailbox)
        .subject(subject);

    // Add To recipients
    for addr in to {
        let mailbox: Mailbox = addr
            .parse()
            .map_err(|e| Error::Other(format!("Invalid 'to' address '{}': {}", addr, e)))?;
        builder = builder.to(mailbox);
    }

    // Add CC recipients
    for addr in cc {
        let mailbox: Mailbox = addr
            .parse()
            .map_err(|e| Error::Other(format!("Invalid 'cc' address '{}': {}", addr, e)))?;
        builder = builder.cc(mailbox);
    }

    // Add BCC recipients
    for addr in bcc {
        let mailbox: Mailbox = addr
            .parse()
            .map_err(|e| Error::Other(format!("Invalid 'bcc' address '{}': {}", addr, e)))?;
        builder = builder.bcc(mailbox);
    }

    // Build the message body
    let message = if let Some(html) = body_html {
        log::debug!("SMTP building multipart/alternative message (text + html)");
        builder
            .multipart(
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
                    ),
            )
            .map_err(|e| Error::Other(format!("Failed to build multipart message: {}", e)))?
    } else {
        log::debug!("SMTP building plain text message");
        builder
            .body(body_text.to_string())
            .map_err(|e| Error::Other(format!("Failed to build message: {}", e)))?
    };

    // Build the SMTP transport
    let creds = Credentials::new(username.to_string(), password.to_string());

    // Use STARTTLS for port 587, implicit TLS for port 465 or when use_tls is set
    // with a non-587 port, and relay (opportunistic) otherwise.
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

    // Send the message
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
