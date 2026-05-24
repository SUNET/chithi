use lettre::address::Envelope;
use lettre::message::{header::ContentType, Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

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

/// Build a Message-ID for an outgoing message, with the sender's own
/// domain as the right-hand side.
///
/// lettre's `message_id(None)` fills the domain via `hostname::get()` —
/// the local machine name (e.g. `ubuntu24`). That leaks the host
/// machine's name onto the wire and, being a non-routable token, is a
/// mild spam signal at strict receivers. RFC 5322 §3.6.4 only requires
/// the id be globally unique; the sender's domain is the universal
/// convention and keeps the header innocuous.
fn sender_message_id(from: &Mailbox) -> String {
    format!(
        "<{}@{}>",
        uuid::Uuid::new_v4().simple(),
        from.email.domain()
    )
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

    // Emit a Message-ID explicitly: lettre's `build()` adds none on its
    // own, and the next reply needs one to point In-Reply-To at. The
    // domain is the sender's (see `sender_message_id`), not lettre's
    // local-hostname default.
    let message_id = sender_message_id(&from_mailbox);
    let mut builder = Message::builder()
        .from(from_mailbox)
        .subject(subject)
        .message_id(Some(message_id));

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

    let transport = build_transport(
        smtp_host,
        smtp_port,
        username,
        password,
        use_tls,
        use_xoauth2,
    )?;

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

/// Build an SMTP transport from connection parameters.
///
/// Port 587 forces STARTTLS regardless of `use_tls`; port 465 (or
/// `use_tls=true` on any other port) uses implicit TLS; everything else
/// falls back to STARTTLS on the requested port.
fn build_transport(
    smtp_host: &str,
    smtp_port: u16,
    username: &str,
    password: &str,
    use_tls: bool,
    use_xoauth2: bool,
) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
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
    Ok(transport)
}

/// Parse an address string ("user@host" or "Name <user@host>") into a
/// lettre `Address` (just the addr-spec; display name is dropped because
/// the envelope is only the addr-spec per RFC 5321 §3.6.1).
fn parse_address(addr: &str) -> Result<Address> {
    let trimmed = addr.trim();
    // If it's in `Name <user@host>` form, extract the bracketed addr-spec.
    let addr_spec = if let (Some(lt), Some(gt)) = (trimmed.find('<'), trimmed.rfind('>')) {
        if lt < gt {
            trimmed[lt + 1..gt].trim()
        } else {
            trimmed
        }
    } else {
        trimmed
    };
    addr_spec
        .parse::<Address>()
        .map_err(|e| Error::Other(format!("Invalid SMTP address '{}': {}", addr, e)))
}

/// Send a previously-built RFC 5322 message via SMTP.
///
/// Used for retries: `compose::send_message` already built and persisted
/// the bytes, so on replay we don't reconstruct them. The envelope is
/// stored separately because the bytes alone may not carry the full
/// recipient list (Bcc is sometimes stripped before transmission).
#[allow(clippy::too_many_arguments)]
pub async fn send_raw(
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
    raw_message: &[u8],
) -> Result<()> {
    let from_addr = parse_address(from)?;
    let mut recipients: Vec<Address> = Vec::with_capacity(to.len() + cc.len() + bcc.len());
    for addr in to.iter().chain(cc.iter()).chain(bcc.iter()) {
        recipients.push(parse_address(addr)?);
    }
    if recipients.is_empty() {
        return Err(Error::Other(
            "SMTP send_raw: no recipients in envelope".into(),
        ));
    }
    let envelope = Envelope::new(Some(from_addr), recipients)
        .map_err(|e| Error::Other(format!("SMTP envelope build failed: {}", e)))?;

    log::info!(
        "SMTP send_raw ({} bytes) from {} to {} recipients via {}:{}",
        raw_message.len(),
        from,
        envelope.to().len(),
        smtp_host,
        smtp_port
    );

    let transport = build_transport(
        smtp_host,
        smtp_port,
        username,
        password,
        use_tls,
        use_xoauth2,
    )?;
    let response = transport
        .send_raw(&envelope, raw_message)
        .await
        .map_err(|e| {
            log::error!("SMTP send_raw failed: {}", e);
            Error::Other(format!("SMTP send_raw failed: {}", e))
        })?;

    log::info!(
        "SMTP send_raw success: {} (code {})",
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
    // automatically, so without this our outgoing replies have no
    // Message-ID for the next reply to thread off of. The domain is the
    // sender's own (see `sender_message_id`) per RFC 5322 §3.6.4, rather
    // than lettre's local-hostname default.
    let message_id = sender_message_id(&from_mailbox);
    let mut builder = Message::builder()
        .from(from_mailbox)
        .subject(subject)
        .message_id(Some(message_id));

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

// ---------------------------------------------------------------------------
// PGP/MIME (RFC 3156) wrappers
//
// `wrap_*` takes a complete RFC 822 message (typically the output of
// `build_raw_message`) and re-frames it as a `multipart/signed` or
// `multipart/encrypted` envelope. The envelope headers (From / To / Cc /
// Bcc / Subject / Date / Message-ID / In-Reply-To / References / MIME-
// Version) stay on the outer message; the inner part keeps its Content-*
// headers so the recipient's MUA discovers the original Content-Type
// after unwrapping. Pattern lifted from
// `~/code/openpgp/tumpa_mail_extension/TumpaMailExtension/PGPMimeBuilder.swift`.
// ---------------------------------------------------------------------------

const PGP_OUTER_HEADERS: &[&[u8]] = &[
    b"from",
    b"sender",
    b"reply-to",
    b"to",
    b"cc",
    b"bcc",
    b"subject",
    b"date",
    b"message-id",
    b"in-reply-to",
    b"references",
    b"user-agent",
    b"x-mailer",
    b"thread-topic",
    b"thread-index",
];

/// Inner-part bytes (headers + body) extracted from a complete RFC 822
/// message. The inner part is what gets signed or encrypted; the outer
/// envelope provides the recipient with the original Content-Type after
/// unwrapping.
fn split_inner_part(raw: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    // Find `\r\n\r\n` (or `\n\n` for LF-only messages — lettre uses
    // CRLF but a hand-rolled inner could be LF). The split point goes
    // AFTER the last header's own terminator, so `header_bytes` keeps
    // every header line complete (including its trailing CRLF) and
    // `body_bytes` starts immediately after the blank-line separator.
    //
    // Without the `+ 2` (or `+ 1`) shift the last header's CRLF stays
    // in the separator span and `walk_headers` yields the final header
    // without a terminator. The reconstructor below then adds a single
    // CRLF that closes the header instead of forming a blank line —
    // producing inner bytes with NO blank line between outer headers
    // and the first `--<boundary>` marker. `mail_parser` (and Apple
    // Mail / Outlook) fall back to rendering the whole thing as a
    // single text/plain part, leaking the closing `--<boundary>--`
    // marker and the inner part's headers into the on-screen body.
    // Observed end-to-end against a Chithi-built encrypted message
    // (Try 2, 2026-05-20).
    let (sep_idx, sep_len) = if let Some(i) = find_subslice(raw, b"\r\n\r\n") {
        (i + 2, 2)
    } else if let Some(i) = find_subslice(raw, b"\n\n") {
        (i + 1, 1)
    } else {
        return None;
    };
    let header_bytes = &raw[..sep_idx];
    let body_bytes = &raw[sep_idx + sep_len..];

    // Walk the full message's headers; the inner part keeps Content-*
    // and MIME-Version, the outer keeps everything else. Build "inner"
    // by emitting the original Content-Type / Content-Transfer-Encoding /
    // Content-Disposition / Content-ID headers (folded form preserved)
    // followed by the body.
    let mut inner = Vec::new();
    for (name, value_bytes_with_folding) in walk_headers(header_bytes) {
        let lower = name.to_ascii_lowercase();
        if lower.starts_with(b"content-") || lower == b"mime-version" {
            inner.extend_from_slice(&value_bytes_with_folding);
        }
    }
    // Header / body separator + body.
    inner.extend_from_slice(b"\r\n");
    inner.extend_from_slice(body_bytes);
    Some((header_bytes.to_vec(), inner))
}

/// Iterate `header_section` yielding `(name, full_line_with_folding)` tuples.
/// Each yielded slice covers the entire header line (including continuation
/// lines and trailing CRLF) so callers can emit a faithful copy.
fn walk_headers(headers: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < headers.len() {
        let line_start = i;
        // Find end of this logical header (current physical line + any
        // continuation lines starting with WSP).
        loop {
            // Walk to end of current physical line.
            while i < headers.len() && headers[i] != b'\n' {
                i += 1;
            }
            if i < headers.len() {
                i += 1; // consume LF
            }
            // Check if next line is a continuation (starts with WSP).
            if i < headers.len() && (headers[i] == b' ' || headers[i] == b'\t') {
                continue;
            }
            break;
        }
        let line = &headers[line_start..i];
        // Pull the name (bytes before colon on the first physical line).
        let name_end = line.iter().take_while(|&&b| b != b':').count();
        if name_end < line.len() {
            let name = line[..name_end].to_vec();
            // Re-emit with CRLF (lettre's output is already CRLF but
            // hand-rolled inputs may be LF-only; normalising to CRLF here
            // is on-the-wire compatible).
            let mut emitted = Vec::with_capacity(line.len());
            for &b in line {
                if b == b'\n' && !emitted.ends_with(b"\r") {
                    emitted.push(b'\r');
                }
                emitted.push(b);
            }
            out.push((name, emitted));
        }
    }
    out
}

/// Re-emit the outer header section, dropping any Content-* / MIME-Version
/// header (those moved to the inner part).
///
/// When `subject_override` is `Some`, the `Subject:` field's value is
/// replaced with the override. This is how protected-headers encryption
/// (draft-ietf-lamps-header-protection) keeps the real subject out of
/// the cleartext envelope — the real subject is duplicated INSIDE the
/// ciphertext by `wrap_with_protected_headers`, and the outer envelope
/// carries only a placeholder.
fn outer_headers_only(header_section: &[u8], subject_override: Option<&str>) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, line) in walk_headers(header_section) {
        let lower = name.to_ascii_lowercase();
        if lower.starts_with(b"content-") || lower == b"mime-version" {
            continue;
        }
        if lower == b"subject" {
            if let Some(new_subject) = subject_override {
                out.extend_from_slice(format!("Subject: {new_subject}\r\n").as_bytes());
                continue;
            }
        }
        // Trust the outer-name filter: not every header is "envelope" but
        // we want to keep them unless they clash with the new
        // Content-* we're about to set. Only Content-* / MIME-Version
        // would clash.
        let _ = PGP_OUTER_HEADERS; // suppress unused-const warning if filter logic shrinks
        out.extend_from_slice(&line);
    }
    out
}

/// Wrap an inner MIME body part in a draft-ietf-lamps-header-protection
/// "v1" layer: a `multipart/mixed` entity whose Content-Type carries the
/// `protected-headers="v1"` parameter and whose own header block
/// duplicates the `Subject:` field. The original `inner` part becomes
/// the single child.
///
/// The result is what gets encrypted. A protected-headers-aware receiver
/// (Thunderbird, K-9, chithi's own decrypt path) reads `Subject` off
/// this entity's headers; the cleartext envelope only ever carries a
/// placeholder. `subject` is written as raw UTF-8 — it is inside the
/// ciphertext, and both `mail_parser` and other modern MUAs accept
/// UTF-8 header values there.
pub fn wrap_with_protected_headers(inner: &[u8], subject: &str) -> Vec<u8> {
    let boundary = format!("chithi-protected-{}", uuid::Uuid::new_v4().simple());
    let mut out = Vec::with_capacity(inner.len() + 256);
    out.extend_from_slice(
        format!(
            "Content-Type: multipart/mixed; protected-headers=\"v1\"; \
             boundary=\"{boundary}\"\r\n"
        )
        .as_bytes(),
    );
    // The protected header field(s). Subject only, in v1.
    // Collapse any CR/LF in the subject to a space: a header value can't
    // contain a bare CR/LF, and an embedded `\r\n\r\n` would otherwise
    // terminate this entity's header block early and malform the
    // encrypted MIME structure. (The non-protected path goes through
    // lettre, which RFC 2047-encodes and so can't be injected.)
    let subject = subject.replace(['\r', '\n'], " ");
    out.extend_from_slice(format!("Subject: {subject}\r\n").as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(inner);
    if !inner.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

/// Wrap `raw` (complete RFC 822 message) in `multipart/signed`.
///
/// `armored_signature` is a detached PGP signature over the
/// CRLF-canonicalised inner-part bytes (see
/// `crate::mail::pgp_mime::canonicalize_for_signing`). `micalg` is the
/// OpenPGP hash algorithm name lowercased and prefixed with `pgp-` (e.g.
/// `pgp-sha256`).
pub fn wrap_pgp_mime_signed(raw: &[u8], armored_signature: &str, micalg: &str) -> Result<Vec<u8>> {
    let (header_section, inner_part) = split_inner_part(raw)
        .ok_or_else(|| Error::Other("pgp/mime: source message has no header/body split".into()))?;
    // Signed messages never protect the subject — the whole point of a
    // multipart/signed message is that its content (including the body
    // and any wrapped headers) stays readable.
    let outer_headers = outer_headers_only(&header_section, None);
    let boundary = format!("chithi-pgp-signed-{}", uuid::Uuid::new_v4().simple());

    let mut out: Vec<u8> = Vec::with_capacity(raw.len() + armored_signature.len() + 512);
    out.extend_from_slice(&outer_headers);
    out.extend_from_slice(b"MIME-Version: 1.0\r\n");
    out.extend_from_slice(
        format!(
            "Content-Type: multipart/signed; protocol=\"application/pgp-signature\"; \
             micalg=\"{micalg}\"; boundary=\"{boundary}\"\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(&inner_part);
    if !inner_part.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(
        b"Content-Type: application/pgp-signature; name=\"signature.asc\"\r\n\
          Content-Description: OpenPGP digital signature\r\n\
          Content-Disposition: attachment; filename=\"signature.asc\"\r\n\
          \r\n",
    );
    out.extend_from_slice(armored_signature.as_bytes());
    if !armored_signature.ends_with('\n') {
        out.extend_from_slice(b"\r\n");
    } else if !armored_signature.ends_with("\r\n") {
        // Promote LF to CRLF for the on-the-wire form.
        out.extend_from_slice(b"\r");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok(out)
}

/// Wrap `raw` (complete RFC 822 message) in `multipart/encrypted`.
///
/// `armored_ciphertext` is the ASCII-armored OpenPGP message produced by
/// libtumpa::encrypt::encrypt_to_recipients (or
/// sign_and_encrypt_to_recipients for sign-then-encrypt).
///
/// `subject_override` replaces the outer envelope's `Subject:` value
/// when `Some` — used by protected-headers ("encrypt the subject")
/// encryption, where the caller passes a placeholder such as `"..."`
/// and the real subject has been folded inside the ciphertext via
/// `wrap_with_protected_headers`. Pass `None` to keep the original
/// outer subject.
pub fn wrap_pgp_mime_encrypted(
    raw: &[u8],
    armored_ciphertext: &str,
    subject_override: Option<&str>,
) -> Result<Vec<u8>> {
    let (header_section, _inner_part) = split_inner_part(raw)
        .ok_or_else(|| Error::Other("pgp/mime: source message has no header/body split".into()))?;
    let outer_headers = outer_headers_only(&header_section, subject_override);
    let boundary = format!("chithi-pgp-encrypted-{}", uuid::Uuid::new_v4().simple());

    let mut out: Vec<u8> = Vec::with_capacity(armored_ciphertext.len() + 1024);
    out.extend_from_slice(&outer_headers);
    out.extend_from_slice(b"MIME-Version: 1.0\r\n");
    out.extend_from_slice(
        format!(
            "Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; \
             boundary=\"{boundary}\"\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(b"\r\n");
    // Part 1: PGP/MIME version-control packet (RFC 3156 §4.2).
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(
        b"Content-Type: application/pgp-encrypted\r\n\
          Content-Description: PGP/MIME version identification\r\n\
          \r\n\
          Version: 1\r\n",
    );
    // Part 2: the actual ciphertext.
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(
        b"Content-Type: application/octet-stream; name=\"encrypted.asc\"\r\n\
          Content-Description: OpenPGP encrypted message\r\n\
          Content-Disposition: inline; filename=\"encrypted.asc\"\r\n\
          \r\n",
    );
    out.extend_from_slice(armored_ciphertext.as_bytes());
    // RFC 2046 §5.1.1: the CRLF immediately preceding the closing
    // `--boundary--` delimiter is part of the delimiter. Ensure exactly
    // one CRLF terminates the ciphertext part. rpgp's armor writer ends
    // its output with a bare LF, so the naive "append \r" branch would
    // produce `\n\r` (CR *after* LF) — malformed, and strict parsers
    // such as our own `pgp_mime::extract_encrypted_payload` then fail to
    // locate the closing boundary. Pop the bare LF and re-terminate
    // with a proper CRLF instead.
    if !armored_ciphertext.ends_with('\n') {
        out.extend_from_slice(b"\r\n");
    } else if !armored_ciphertext.ends_with("\r\n") {
        out.pop(); // drop the trailing bare LF
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok(out)
}

/// Return the inner-part bytes from a complete RFC 822 message — the
/// payload that gets signed or encrypted. Exposed publicly so the
/// compose-side caller can canonicalise these bytes for the signature
/// (libtumpa's `canonicalize_for_signing`).
pub fn inner_part_of(raw: &[u8]) -> Result<Vec<u8>> {
    split_inner_part(raw)
        .map(|(_outer, inner)| inner)
        .ok_or_else(|| Error::Other("pgp/mime: source message has no header/body split".into()))
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Format a folded `Autocrypt:` header line for `addr` carrying the
/// Autocrypt-minimised transferable public key `keydata` (raw binary
/// OpenPGP bytes). The returned string is a complete header field
/// WITHOUT a trailing CRLF — `insert_header_before_body` supplies the
/// framing.
///
/// `prefer-encrypt=mutual` is always emitted: a chithi user who has the
/// Autocrypt toggle on is signalling they want encrypted replies. The
/// base64 `keydata` is folded onto continuation lines, each prefixed by
/// a single space (RFC 5322 §2.2.3 FWS). Autocrypt parsers strip ALL
/// whitespace from `keydata` before base64-decoding (Autocrypt Level 1
/// §2.1.1), so the fold width is purely cosmetic; 72 chars keeps every
/// wire line under the 78-column soft limit (1 leading space + 72 = 73).
pub fn format_autocrypt_header(addr: &str, keydata: &[u8]) -> String {
    use base64::Engine;
    // Defense-in-depth: this header is hand-assembled and spliced in by
    // `insert_header_before_body`, bypassing lettre's address validation.
    // An addr-spec cannot contain CR/LF — strip any so a malformed
    // account address can't inject extra headers into the outgoing
    // message.
    let addr = addr.replace(['\r', '\n'], "");
    let b64 = base64::engine::general_purpose::STANDARD.encode(keydata);
    let mut out = format!("Autocrypt: addr={addr}; prefer-encrypt=mutual; keydata=");
    // base64 output is pure ASCII, so byte-index slicing is char-safe.
    const WIDTH: usize = 72;
    let mut idx = 0;
    while idx < b64.len() {
        let end = (idx + WIDTH).min(b64.len());
        out.push_str("\r\n ");
        out.push_str(&b64[idx..end]);
        idx = end;
    }
    out
}

/// Insert `header_line` (a complete header field WITHOUT a trailing
/// CRLF; any continuation lines already folded) into the header block
/// of RFC 5322 message `raw`, as the last header before the blank line
/// that separates headers from body.
///
/// Returns `raw` unchanged if no `\r\n\r\n` header/body separator is
/// found — a malformed message shouldn't be made worse.
pub fn insert_header_before_body(raw: &[u8], header_line: &str) -> Vec<u8> {
    match find_subslice(raw, b"\r\n\r\n") {
        Some(i) => {
            // raw[..i] ends at the last header's content (its trailing
            // CRLF is part of the `\r\n\r\n` at `i`). Splice in
            // "\r\n<header_line>" so the new field becomes the final
            // header, then the existing `\r\n\r\n` closes the block.
            let mut out = Vec::with_capacity(raw.len() + header_line.len() + 2);
            out.extend_from_slice(&raw[..i]);
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(header_line.as_bytes());
            out.extend_from_slice(&raw[i..]);
            out
        }
        None => raw.to_vec(),
    }
}

#[cfg(test)]
mod pgp_wrap_tests {
    use super::*;

    fn build_simple() -> Vec<u8> {
        build_raw_message(
            "alice@example.com",
            &["bob@example.com".into()],
            &[],
            &[],
            "Test subject",
            "Hello, world.",
            None,
            &[],
            None,
            &[],
        )
        .expect("build_raw_message")
    }

    /// Regression: the generated Message-ID's domain must be the
    /// sender's own domain, never the local machine hostname. lettre's
    /// `message_id(None)` default fills it via `hostname::get()` (e.g.
    /// `ubuntu24`), which leaks the host name onto the wire and is a
    /// mild spam signal at strict receivers.
    #[test]
    fn message_id_domain_is_sender_domain_not_hostname() {
        let raw = build_raw_message(
            "alice@example.com",
            &["bob@example.com".into()],
            &[],
            &[],
            "Subject",
            "body",
            None,
            &[],
            None,
            &[],
        )
        .expect("build_raw_message");
        let s = String::from_utf8_lossy(&raw);
        let line = s
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("message-id:"))
            .expect("Message-ID header present");
        assert!(
            line.contains("@example.com>"),
            "Message-ID domain must be the sender's domain: {line:?}"
        );
        assert!(
            !line.contains("@localhost"),
            "Message-ID must not fall back to lettre's hostname default: {line:?}"
        );
    }

    #[test]
    fn split_inner_part_keeps_content_headers_inside() {
        let raw = build_simple();
        let (outer, inner) = split_inner_part(&raw).expect("split");
        let outer_s = String::from_utf8_lossy(&outer);
        let inner_s = String::from_utf8_lossy(&inner);
        // Envelope stays on outer.
        assert!(outer_s.to_ascii_lowercase().contains("from:"));
        assert!(outer_s.to_ascii_lowercase().contains("subject:"));
        // Content-Type moves to inner.
        assert!(inner_s.to_ascii_lowercase().contains("content-type"));
    }

    #[test]
    fn wrap_signed_emits_two_parts_with_correct_protocol() {
        let raw = build_simple();
        let wrapped = wrap_pgp_mime_signed(
            &raw,
            "-----BEGIN PGP SIGNATURE-----\nfake\n-----END PGP SIGNATURE-----\n",
            "pgp-sha512",
        )
        .expect("wrap");
        let s = String::from_utf8_lossy(&wrapped);
        assert!(s.contains("multipart/signed"));
        assert!(s.contains("protocol=\"application/pgp-signature\""));
        assert!(s.contains("micalg=\"pgp-sha512\""));
        assert!(s.contains("application/pgp-signature"));
        assert!(s.contains("BEGIN PGP SIGNATURE"));
        // Outer keeps envelope, dropped Content-* (no Content-Type:
        // multipart/alternative at top — it's the signed wrapper now).
        // We accept one occurrence of "Content-Type:" inside the inner
        // part plus one in the wrapper headers, plus one per body part.
        let occurrences = s.matches("Content-Type:").count();
        assert!(
            occurrences >= 3,
            "expected wrapper + 2 part Content-Type headers, got {occurrences}"
        );
    }

    #[test]
    fn wrap_encrypted_emits_version_part_and_octet_stream() {
        let raw = build_simple();
        let wrapped = wrap_pgp_mime_encrypted(
            &raw,
            "-----BEGIN PGP MESSAGE-----\nfake\n-----END PGP MESSAGE-----\n",
            None,
        )
        .expect("wrap");
        let s = String::from_utf8_lossy(&wrapped);
        assert!(s.contains("multipart/encrypted"));
        assert!(s.contains("protocol=\"application/pgp-encrypted\""));
        assert!(s.contains("application/pgp-encrypted"));
        assert!(s.contains("Version: 1"));
        assert!(s.contains("application/octet-stream"));
        assert!(s.contains("BEGIN PGP MESSAGE"));
    }

    #[test]
    fn inner_part_is_what_we_sign_over() {
        let raw = build_simple();
        let inner = inner_part_of(&raw).expect("inner");
        // The inner part includes the part headers AND the body — that's
        // what the signature is computed over (after CRLF canonicalize).
        let s = String::from_utf8_lossy(&inner);
        assert!(s.to_ascii_lowercase().contains("content-type"));
        assert!(s.contains("Hello, world."));
    }

    /// Regression: when send_message routes a PGP-wrapped raw via SMTP it
    /// must transmit the wrapped bytes verbatim through `send_raw`. The
    /// previous code path rebuilt the message from the structured
    /// ComposeMessage fields, silently dropping the wrapping and leaking
    /// the cleartext body on the wire. This test pins the data flow:
    /// once `wrap_pgp_mime_encrypted` runs, the original plaintext body
    /// must NOT appear anywhere in the wire bytes.
    #[test]
    fn encrypted_wrapped_bytes_do_not_leak_plaintext_body() {
        let secret = "ULTRA_SECRET_PAYLOAD_42";
        let raw = build_raw_message(
            "alice@example.com",
            &["bob@example.com".into()],
            &[],
            &[],
            "Subject is not secret",
            secret,
            None,
            &[],
            None,
            &[],
        )
        .expect("build_raw_message");
        // Sanity: plaintext is in the unwrapped bytes (so the next
        // assertion is meaningful).
        assert!(
            String::from_utf8_lossy(&raw).contains(secret),
            "plain bytes must contain the cleartext for the contrast to hold"
        );

        let wrapped = wrap_pgp_mime_encrypted(
            &raw,
            "-----BEGIN PGP MESSAGE-----\nopaque-ciphertext\n-----END PGP MESSAGE-----\n",
            None,
        )
        .expect("wrap");
        let wrapped_s = String::from_utf8_lossy(&wrapped);
        assert!(
            !wrapped_s.contains(secret),
            "wrapped bytes must not contain the plaintext body — found {secret:?}"
        );
        assert!(wrapped_s.contains("multipart/encrypted"));
    }

    /// Regression: the inner part produced by `inner_part_of` must
    /// preserve the blank line between the outer headers and the first
    /// boundary marker. Without it `mail_parser` (and Apple Mail /
    /// Outlook readers) lump the inner part's Content-Type into the
    /// outer header block, fail to walk the multipart structure, and
    /// render the entire thing as a single text/plain part — leaking
    /// the closing `--<boundary>--` and inner headers into the
    /// on-screen body of every decrypted message. Observed end-to-end
    /// against a Chithi-built encrypted "Try 2" on 2026-05-20.
    ///
    /// This test feeds the lettre-built raw message through the real
    /// `inner_part_of` and then asks `mail_parser` for the text body —
    /// the closing boundary marker must NOT appear in the rendered
    /// body, and the part tree must be walked correctly.
    #[test]
    fn inner_part_preserves_blank_line_so_mail_parser_walks_multipart() {
        let raw = build_raw_message(
            "alice@example.com",
            &["bob@example.com".into()],
            &[],
            &[],
            "Try 2",
            "Hi,\r\n\r\nI hope you can see this email.\r\n\r\nKushal\r\n",
            None,
            &[],
            None,
            &[],
        )
        .expect("build_raw_message");

        let inner = inner_part_of(&raw).expect("inner_part_of");

        // Pull the boundary out of the inner's outer Content-Type so we
        // can check it never appears in the rendered body.
        let inner_str = std::str::from_utf8(&inner).expect("utf8");
        let boundary = inner_str
            .lines()
            .find_map(|l| {
                l.find("boundary=").map(|i| {
                    let v = &l[i + "boundary=".len()..];
                    v.trim_matches(|c: char| c == '"' || c == ';' || c.is_whitespace())
                        .to_string()
                })
            })
            .expect("Content-Type carries a boundary parameter");

        let parsed = mail_parser::MessageParser::default()
            .parse(inner.as_slice())
            .expect("mail_parser must accept inner part");

        // mail_parser must walk the multipart and produce at least the
        // outer container + the inner text/plain. The broken
        // (no-blank-line) inner collapses to a single part.
        assert!(
            parsed.parts.len() >= 2,
            "mail_parser saw {} part(s); expected >=2. Inner bytes are missing the \
             blank line between outer headers and the first --boundary marker.",
            parsed.parts.len()
        );

        let body = parsed.body_text(0).expect("text body present").to_string();
        assert!(body.contains("Hi,"), "expected text body, got: {:?}", body);
        assert!(
            !body.contains(&boundary),
            "boundary {:?} leaked into rendered body: {:?}",
            boundary,
            body
        );
        assert!(
            !body.contains("Content-Type:"),
            "inner Content-Type leaked into body: {:?}",
            body
        );
        assert!(
            !body.contains("Content-Transfer-Encoding:"),
            "inner Content-Transfer-Encoding leaked into body: {:?}",
            body
        );
    }

    /// Regression: the SMTP wire path must accept the wrapped bytes
    /// through `send_raw`'s (envelope, bytes) signature. The envelope
    /// addresses are passed explicitly by the caller — they are NOT
    /// derived from re-parsing the raw bytes. Pin this by constructing
    /// the same Envelope `send_raw` builds.
    #[test]
    fn send_raw_envelope_built_from_explicit_args_not_message_bytes() {
        let from = parse_address("alice@example.com").expect("from");
        let recipients = vec![
            parse_address("bob@example.com").expect("to"),
            parse_address("carol@example.com").expect("cc"),
        ];
        let envelope = Envelope::new(Some(from), recipients).expect("envelope");
        assert_eq!(envelope.to().len(), 2);
        assert!(envelope.from().is_some());
        // The raw bytes are opaque to `send_raw`; whether they're a plain
        // RFC 822 message, a multipart/signed, or a multipart/encrypted
        // envelope doesn't affect the SMTP envelope.
    }

    /// The Autocrypt header must carry `addr`, `prefer-encrypt=mutual`,
    /// and a `keydata=` payload that — once all whitespace is stripped,
    /// per Autocrypt Level 1 §2.1.1 — base64-decodes back to the exact
    /// input bytes. Continuation lines must be folded with CRLF + a
    /// single leading space so the header is RFC 5322-legal.
    #[test]
    fn autocrypt_header_folds_and_round_trips_keydata() {
        use base64::Engine;
        // 600 bytes is enough to force several continuation lines.
        let keydata: Vec<u8> = (0..600u32).map(|i| (i % 256) as u8).collect();
        let header = format_autocrypt_header("alice@example.com", &keydata);

        assert!(header.starts_with("Autocrypt: addr=alice@example.com; "));
        assert!(header.contains("prefer-encrypt=mutual;"));
        assert!(header.contains("keydata="));

        // Every continuation line begins with exactly one space (FWS),
        // and no wire line exceeds the 78-column soft limit.
        for line in header.split("\r\n").skip(1) {
            assert!(
                line.starts_with(' ') && !line.starts_with("  "),
                "continuation line must start with a single space: {line:?}"
            );
            assert!(
                line.len() <= 78,
                "line exceeds 78 cols: {} chars",
                line.len()
            );
        }

        // Strip ALL whitespace from the keydata value and base64-decode.
        let value = header.split("keydata=").nth(1).expect("keydata= present");
        let compact: String = value.chars().filter(|c| !c.is_whitespace()).collect();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(compact.as_bytes())
            .expect("keydata base64-decodes");
        assert_eq!(decoded, keydata, "keydata must round-trip exactly");
    }

    /// `insert_header_before_body` places the new field as the LAST
    /// header (right before the blank line) and leaves the body byte-
    /// identical.
    #[test]
    fn insert_header_before_body_appends_last_header() {
        let raw = b"From: a@x\r\nSubject: hi\r\n\r\nbody text\r\n";
        let out = insert_header_before_body(raw, "Autocrypt: addr=a@x; keydata=AAAA");
        let s = String::from_utf8(out).unwrap();
        assert_eq!(
            s,
            "From: a@x\r\nSubject: hi\r\nAutocrypt: addr=a@x; keydata=AAAA\r\n\r\nbody text\r\n"
        );
    }

    /// No header/body separator → returned unchanged (don't corrupt a
    /// malformed message).
    #[test]
    fn insert_header_before_body_noop_without_separator() {
        let raw = b"this is not a real rfc5322 message";
        let out = insert_header_before_body(raw, "Autocrypt: x");
        assert_eq!(out, raw);
    }

    /// `wrap_with_protected_headers` produces a multipart/mixed entity
    /// tagged `protected-headers="v1"`, carries the real Subject as one
    /// of its own header fields, and keeps the original inner part as
    /// the single child.
    #[test]
    fn protected_headers_wrap_carries_subject_and_child() {
        let inner = b"Content-Type: text/plain\r\n\r\nthe body text\r\n";
        let wrapped = wrap_with_protected_headers(inner, "Secret Subject");
        let s = String::from_utf8(wrapped).unwrap();
        assert!(
            s.starts_with("Content-Type: multipart/mixed; protected-headers=\"v1\";"),
            "must be a protected-headers multipart/mixed entity"
        );
        assert!(
            s.contains("\r\nSubject: Secret Subject\r\n"),
            "the real subject must be a header of the protected entity"
        );
        assert!(
            s.contains("the body text"),
            "the original inner body must survive as the child part"
        );
    }

    /// `wrap_pgp_mime_encrypted` with a `subject_override` replaces the
    /// cleartext outer Subject with the placeholder — the real subject
    /// never appears in the envelope.
    #[test]
    fn wrap_encrypted_subject_override_replaces_outer_subject() {
        // build_simple() sets Subject: "Test subject".
        let raw = build_simple();
        let wrapped = wrap_pgp_mime_encrypted(
            &raw,
            "-----BEGIN PGP MESSAGE-----\nfake\n-----END PGP MESSAGE-----\n",
            Some("..."),
        )
        .expect("wrap");
        let s = String::from_utf8_lossy(&wrapped);
        assert!(
            s.contains("Subject: ...\r\n"),
            "outer Subject must be the placeholder"
        );
        assert!(
            !s.contains("Test subject"),
            "the real subject must NOT appear in the cleartext envelope"
        );
    }

    /// Security regression (review LOW-1): a CR/LF in the Autocrypt
    /// `addr` must not start a new header line — otherwise a malformed
    /// account address could inject extra headers into outgoing mail.
    /// (A `Bcc:` substring left inside the `addr=` *value* is harmless;
    /// only a `CRLF`-then-`Bcc:` line break would be an injection.) The
    /// only CRLFs in the result are the keydata fold continuations.
    #[test]
    fn autocrypt_header_strips_crlf_from_address() {
        let header = format_autocrypt_header(
            "alice@example.com\r\nBcc: victim@example.com",
            b"keydata-bytes",
        );
        assert!(
            !header.contains("\r\nBcc:"),
            "a CRLF-injected header line must not appear: {header:?}"
        );
        // Every CRLF present must be a fold (CRLF + single space) — no
        // CRLF starts a fresh header field.
        for (i, _) in header.match_indices("\r\n") {
            assert_eq!(
                header.as_bytes().get(i + 2),
                Some(&b' '),
                "every CRLF must begin a folded continuation line"
            );
        }
    }

    /// Security regression (review LOW-2): a CR/LF in the protected
    /// subject must not inject extra header fields into — or malform —
    /// the protected-headers entity. The entity's header block stays
    /// exactly two lines (Content-Type + Subject) before its blank line.
    #[test]
    fn protected_headers_wrap_strips_crlf_from_subject() {
        let inner = b"Content-Type: text/plain\r\n\r\nbody\r\n";
        let wrapped =
            wrap_with_protected_headers(inner, "Secret\r\nBcc: victim@example.com\r\n\r\ninjected");
        let s = String::from_utf8(wrapped).unwrap();
        // The protected entity's own header block is Content-Type then
        // Subject, then the blank line — the Subject must be a single
        // line, so the block is exactly two header lines.
        let header_block = s.split("\r\n\r\n").next().unwrap();
        assert_eq!(
            header_block.lines().count(),
            2,
            "protected entity must have exactly 2 header lines, got: {header_block:?}"
        );
        assert!(
            !header_block.contains("\r\nBcc:"),
            "a CRLF-injected header line must not appear: {header_block:?}"
        );
    }
}
