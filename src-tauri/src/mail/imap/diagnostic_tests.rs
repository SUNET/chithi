use super::imap_parse_diagnostic;
use imap::error::ParseError;

const PRIVATE_PAYLOAD: &str = "Subject: Private subject 雪\r\n\
From: sender@private.example\r\n\
To: recipient@private.example\r\n\
Message-ID: <private-message-id@example>\r\n\r\nPrivate body";

const CONTEXTS: &[&str] = &[
    "IMAP UID FETCH 1 BODY[]",
    super::ENVELOPE_FETCH_SPEC,
    "IMAP UID FETCH 1:* (UID FLAGS)",
    "IMAP UID SEARCH",
];

fn assert_redacted(context: &str, error: &imap::Error, byte_count: usize) -> String {
    let diagnostic = imap_parse_diagnostic(context, error).unwrap();
    assert!(diagnostic.starts_with(context));
    assert!(diagnostic.contains(&byte_count.to_string()));
    assert!(diagnostic.contains("payload redacted"));
    // Callers also log Display after note_error; that must not expose payloads.
    for text in [&diagnostic, &error.to_string()] {
        for private in [
            "Private subject",
            "sender@private.example",
            "recipient@private.example",
            "private-message-id",
            "Private body",
            "雪",
        ] {
            assert!(!text.contains(private), "diagnostic exposed {private:?}");
        }
        assert!(!text.contains(['\r', '\n']));
    }
    diagnostic
}

#[test]
fn invalid_response_payloads_are_redacted_for_every_fetch_context() {
    let error = imap::Error::Parse(ParseError::Invalid(PRIVATE_PAYLOAD.as_bytes().to_vec()));
    for context in CONTEXTS {
        let diagnostic = assert_redacted(context, &error, PRIVATE_PAYLOAD.len());
        assert!(diagnostic.contains("parser rejected"));
    }
}

#[test]
fn non_utf8_response_payloads_keep_only_length_and_encoding_error() {
    let mut bytes = PRIVATE_PAYLOAD.as_bytes().to_vec();
    bytes.push(0xff);
    let utf8_error = std::str::from_utf8(&bytes).unwrap_err();
    let byte_count = bytes.len();
    let error = imap::Error::Parse(ParseError::DataNotUtf8(bytes, utf8_error));
    for context in CONTEXTS {
        let diagnostic = assert_redacted(context, &error, byte_count);
        assert!(diagnostic.contains("non-UTF-8"));
        assert!(diagnostic.contains(&utf8_error.to_string()));
    }
}

#[test]
fn unexpected_response_payloads_are_also_redacted() {
    let text = format!("Fetch({PRIVATE_PAYLOAD})");
    let byte_count = text.len();
    let error = imap::Error::Parse(ParseError::Unexpected(text));
    for context in CONTEXTS {
        let diagnostic = assert_redacted(context, &error, byte_count);
        assert!(diagnostic.contains("unexpected response"));
    }
}

#[test]
fn authentication_challenges_have_no_payload_diagnostic() {
    let error = imap::Error::Parse(ParseError::Authentication(
        "private-authentication-challenge".to_string(),
        None,
    ));
    assert!(imap_parse_diagnostic("IMAP AUTHENTICATE", &error).is_none());
    assert!(!error
        .to_string()
        .contains("private-authentication-challenge"));
}

#[test]
fn non_parse_errors_do_not_get_payload_diagnostics() {
    assert!(imap_parse_diagnostic("IMAP UID FETCH", &imap::Error::ConnectionLost).is_none());
}
