use std::cell::Cell;
use std::sync::{Arc, Mutex};

use libtumpa::KeyStore;

use super::{check_pgp_recipient_keys, PgpRecipientCheckError};
use crate::error::Error;
use crate::mail::smtp;

const INVALID_RECIPIENTS: [&str; 4] = [
    "alice@example..com",
    ".alice@example.com",
    "alice..bob@example.com",
    "alice@[not-an-ip]",
];

const SUPPORTED_RECIPIENTS: [&str; 11] = [
    "Alice@EXAMPLE.com",
    r#""quoted local"@Example.COM"#,
    r#""ali\ce"@EXAMPLE.com"#,
    "alice@bücher.example",
    "álïce@example.com",
    r#""Recipient, Bob" <bob@example.com>"#,
    r#"Quoted Local <"quoted local"@Example.COM>"#,
    r#"Angle <"a>b"@example.com>"#,
    "Åsa Österberg <asa@example.com>",
    "literal@[192.0.2.1]",
    "ipv6@[IPv6:2001:db8::1]",
];

fn assert_invalid_before_open(recipients: &[&str], position: usize) {
    let invalid = recipients[position - 1];
    assert!(
        smtp::parse_mailbox(invalid).is_err(),
        "invalid fixture must also be rejected by SMTP: {invalid:?}"
    );
    for recipient in &recipients[..position - 1] {
        if !recipient.bytes().all(|byte| matches!(byte, b' ' | b'\t')) {
            smtp::parse_mailbox(recipient)
                .expect("recipients before the invalid fixture must be valid");
        }
    }

    let called = Cell::new(false);
    let result = check_pgp_recipient_keys(
        recipients
            .iter()
            .map(|recipient| (*recipient).into())
            .collect(),
        || {
            called.set(true);
            Err(Error::Other("keystore should not open".into()))
        },
    );

    let error = result.expect_err("an invalid batch must fail validation");
    assert!(
        matches!(&error, PgpRecipientCheckError::InvalidRecipient { index } if *index == position)
    );
    let message = error.to_string();
    assert_eq!(
        message,
        format!("Invalid recipient address at position {position}")
    );
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        serde_json::json!({"kind": "invalidRecipient", "index": position})
    );
    for recipient in recipients {
        if !recipient.bytes().all(|byte| matches!(byte, b' ' | b'\t')) {
            assert!(
                !message.contains(recipient),
                "validation errors must not expose recipient addresses"
            );
        }
    }
    assert!(
        !called.get(),
        "validation must finish before opening the store"
    );
}

fn assert_empty_without_open(recipients: Vec<String>) {
    let called = Cell::new(false);
    let result = check_pgp_recipient_keys(recipients, || {
        called.set(true);
        Err(Error::Other("keystore should not open".into()))
    });

    assert!(!called.get(), "an empty batch must not open the store");
    assert!(result.expect("empty recipients must succeed").is_empty());
}

fn assert_missing_keys(recipients: Vec<String>, expected: &[&str]) {
    for recipient in &recipients {
        if !recipient.bytes().all(|byte| matches!(byte, b' ' | b'\t')) {
            smtp::parse_mailbox(recipient)
                .expect("valid recipient fixtures must also be accepted by SMTP");
        }
    }

    let calls = Cell::new(0);
    let statuses = check_pgp_recipient_keys(recipients, || {
        calls.set(calls.get() + 1);
        let store = KeyStore::open_in_memory().expect("empty test keystore");
        Ok(Arc::new(Mutex::new(store)))
    })
    .expect("valid recipients without keys must return statuses");

    assert_eq!(calls.get(), 1, "the batch must open the store exactly once");
    assert_eq!(statuses.len(), expected.len());
    for (status, expected_email) in statuses.iter().zip(expected) {
        assert_eq!(status.email.as_str(), *expected_email);
        assert!(
            !status.has_key,
            "an empty store cannot contain a matching key"
        );
        assert_eq!(status.fingerprint, None);
    }
}

#[test]
fn invalid_recipients_are_rejected_before_opening_the_keystore() {
    for invalid in INVALID_RECIPIENTS {
        assert_invalid_before_open(&[invalid], 1);
    }
}

#[test]
fn the_whole_batch_is_validated_before_opening_the_keystore() {
    for invalid in INVALID_RECIPIENTS {
        assert_invalid_before_open(&["valid@example.com", invalid], 2);
    }
}

#[test]
fn invalid_position_counts_skipped_horizontal_padding_entries() {
    for invalid in INVALID_RECIPIENTS {
        let padded = format!(" \t{invalid}\t ");
        assert_invalid_before_open(&["", " \t", "valid@example.com", "\t ", &padded], 5);
    }
}

#[test]
fn the_first_invalid_recipient_position_is_reported() {
    assert_invalid_before_open(
        &[
            "",
            "valid@example.com",
            ".alice@example.com",
            "alice@example..com",
        ],
        3,
    );
}

#[test]
fn an_empty_batch_does_not_open_the_keystore() {
    assert_empty_without_open(vec![]);
}

#[test]
fn horizontal_padding_only_recipients_do_not_open_the_keystore() {
    assert_empty_without_open(
        ["", " ", "\t", " \t "]
            .into_iter()
            .map(str::to_string)
            .collect(),
    );
}

#[test]
fn missing_keys_preserve_trimmed_spelling_order_and_duplicates() {
    assert_missing_keys(
        [
            "",
            " \tAlice@EXAMPLE.com \t",
            " \t",
            " bob@example.com ",
            "Alice@EXAMPLE.com",
            "",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
        &["Alice@EXAMPLE.com", "bob@example.com", "Alice@EXAMPLE.com"],
    );
}

#[test]
fn supported_mailbox_forms_keep_their_original_spelling_in_statuses() {
    // Keep bare quoted addr-specs and full mailboxes as supplied, not formatted Mailboxes.
    assert_missing_keys(
        SUPPORTED_RECIPIENTS
            .iter()
            .map(|recipient| format!(" \t{recipient}\t "))
            .collect(),
        &SUPPORTED_RECIPIENTS,
    );
}

#[test]
fn unicode_local_parts_are_not_trimmed_from_key_identifiers_or_statuses() {
    for ch in ['\u{a0}', '\u{2003}', '\u{202f}', '\u{3000}', '\u{feff}'] {
        let leading = format!("{ch}Alice@example.com");
        let trailing = format!("Alice{ch}@example.com");
        let both = format!("{ch}Alice{ch}@example.com");
        assert_missing_keys(
            vec![
                format!(" \t{leading} \t"),
                trailing.clone(),
                both.clone(),
                "Alice@example.com".into(),
            ],
            &[&leading, &trailing, &both, "Alice@example.com"],
        );
    }
}

#[test]
fn non_mailbox_whitespace_and_edge_controls_are_rejected_before_opening() {
    for invalid in [
        "\r",
        "\n",
        "\r\n",
        "\t\r\n",
        "\u{2003}\u{a0}",
        " \u{a0} ",
        "alice@example.com\r",
        "alice@example.com\n",
        "alice@example.com\r\n",
        "\ralice@example.com",
        "\nalice@example.com",
        "\r\nalice@example.com",
        "alice@example.com\u{a0}",
    ] {
        assert_invalid_before_open(&["", " \t", "valid@example.com", invalid], 4);
    }
}

#[test]
fn valid_header_folding_is_preserved_in_key_identifiers_and_statuses() {
    let folded = [
        "\r\n alice@example.com\r\n ",
        "Name\r\n <alice@example.com>",
    ];
    assert_missing_keys(
        folded.iter().map(|value| (*value).into()).collect(),
        &folded,
    );
}

#[test]
fn valid_syntax_reaches_the_loader_and_preserves_its_error() {
    const LOADER_ERROR: &str = "recipient syntax accepted; keystore unavailable";

    for recipient in SUPPORTED_RECIPIENTS {
        smtp::parse_mailbox(recipient)
            .expect("valid recipient fixtures must also be accepted by SMTP");
        let calls = Cell::new(0);
        let error = check_pgp_recipient_keys(vec![recipient.into()], || {
            calls.set(calls.get() + 1);
            Err(Error::Other(LOADER_ERROR.into()))
        })
        .expect_err("the loader failure must be propagated");

        assert_eq!(calls.get(), 1);
        assert!(matches!(
            &error,
            PgpRecipientCheckError::Other(Error::Other(_))
        ));
        assert_eq!(error.to_string(), LOADER_ERROR);
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            serde_json::Value::String(LOADER_ERROR.to_string())
        );
    }
}
