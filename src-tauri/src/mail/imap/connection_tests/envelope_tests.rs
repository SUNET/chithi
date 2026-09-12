use super::super::{EnvelopeBatch, EnvelopeData, ImapConfig, ImapConnection};
use super::{accept_session, command, connect_session, finish_logout, respond, Peer};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;

const HEADER_FIELDS: &str = "(SUBJECT FROM TO CC DATE MESSAGE-ID IN-REPLY-TO REFERENCES)";
const HEADERS: &str = "Subject: =?UTF-8?Q?H=C3=A4lsningar?=\r\n\
    From: \"Sender, Test\" <sender@example.test>\r\n\
    To: Recipient <recipient@example.test>\r\n\
    Cc: copy@example.test\r\n\
    Date: Sat, 12 Sep 2026 12:00:00 +0000\r\n\
    Message-ID: <message@example.test>\r\n\
    In-Reply-To: < parent@example.test >\r\n\
    References: <root@example.test>\r\n <parent@example.test>\r\n\r\n";
const SIZE: u64 = 12_512;

fn with_session<T>(
    serve: impl FnOnce(&mut Peer) + Send,
    run: impl FnOnce(&mut ImapConnection, SocketAddr) -> T,
) -> T {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::scope(|scope| {
        let server = scope.spawn(move || {
            let mut peer = accept_session(&listener);
            // Unexpected reconnects must fail rather than queue behind this session.
            drop(listener);
            serve(&mut peer);
            finish_logout(&mut peer);
        });
        let mut connection = connect_session(address, None);
        let result = run(&mut connection, address);
        let poisoned = connection.is_poisoned();
        // Finish bounded TLS I/O before the caller asserts on returned data.
        connection.logout();
        server.join().unwrap();
        assert!(!poisoned);
        result
    })
}

fn fetch_batch(uids: &[u32], serve: impl FnOnce(&mut Peer) + Send) -> EnvelopeBatch {
    with_session(serve, |connection, _| {
        connection.fetch_envelopes_batch(uids)
    })
    .unwrap()
}

fn envelope_command(peer: &mut Peer, uids: &[u32]) -> String {
    let uid_set = uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    command(
        peer,
        &format!(
            "UID FETCH {uid_set} (UID FLAGS RFC822.SIZE \
             BODY.PEEK[HEADER.FIELDS {HEADER_FIELDS}])\r\n"
        ),
    )
}

fn header_fetch(sequence: u32, uid: u32, attributes: &str, headers: &str) -> String {
    let attributes = if attributes.is_empty() {
        String::new()
    } else {
        format!(" {attributes}")
    };
    format!(
        "* {sequence} FETCH (UID {uid}{attributes} \
         BODY[HEADER.FIELDS {HEADER_FIELDS}] {{{}}}\r\n{headers})\r\n\
         * OK Still working...\r\n",
        headers.len()
    )
}

fn complete_fetch(peer: &mut Peer, tag: &str, response: &str) {
    respond(
        peer,
        &format!(
            "* OK Still working...\r\n{response}\
             * OK Still working...\r\n{tag} OK fetched\r\n"
        ),
    );
}

fn assert_full_metadata(envelope: &EnvelopeData) {
    assert_eq!(envelope.subject.as_deref(), Some("Hälsningar"));
    assert_eq!(envelope.from_name.as_deref(), Some("Sender, Test"));
    assert_eq!(envelope.from_email.as_deref(), Some("sender@example.test"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&envelope.to_addresses).unwrap(),
        serde_json::json!([{"name": "Recipient", "email": "recipient@example.test"}])
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&envelope.cc_addresses).unwrap(),
        serde_json::json!([{"name": null, "email": "copy@example.test"}])
    );
    assert_eq!(
        envelope.date.as_deref(),
        Some("Sat, 12 Sep 2026 12:00:00 +0000")
    );
    assert_eq!(
        envelope.message_id.as_deref(),
        Some("<message@example.test>")
    );
    assert_eq!(
        envelope.in_reply_to.as_deref(),
        Some("<parent@example.test>")
    );
    assert_eq!(
        envelope.references,
        ["<root@example.test>", "<parent@example.test>"]
    );
    assert_eq!(envelope.size, SIZE);
    assert!(envelope.has_attachments);
}

#[test]
fn trailing_flag_only_fetch_preserves_one_complete_envelope() {
    let batch = fetch_batch(&[7], |peer| {
        let tag = envelope_command(peer, &[7]);
        let response = format!(
            "{}* 1 FETCH (UID 7 FLAGS (\\Seen \\Flagged project-tag))\r\n",
            header_fetch(1, 7, "FLAGS (\\Draft) RFC822.SIZE 12512", HEADERS)
        );
        complete_fetch(peer, &tag, &response);
    });

    assert!(batch.failed_uids.is_empty());
    assert_eq!(batch.envelopes.len(), 1);
    let envelope = &batch.envelopes[0];
    assert_eq!(envelope.uid, 7);
    assert_full_metadata(envelope);
    assert_eq!(envelope.flags, ["seen", "flagged", "project-tag"]);
}

#[test]
fn leading_flag_only_fetch_survives_a_header_without_flags() {
    let batch = fetch_batch(&[7], |peer| {
        let tag = envelope_command(peer, &[7]);
        let response = format!(
            "* 1 FETCH (UID 7 FLAGS (\\Seen \\Flagged project-tag))\r\n\
             * OK Still working...\r\n{}",
            header_fetch(1, 7, "RFC822.SIZE 12512", HEADERS)
        );
        complete_fetch(peer, &tag, &response);
    });

    assert!(batch.failed_uids.is_empty());
    assert_eq!(batch.envelopes.len(), 1);
    let envelope = &batch.envelopes[0];
    assert_eq!(envelope.uid, 7);
    assert_full_metadata(envelope);
    assert_eq!(envelope.flags, ["seen", "flagged", "project-tag"]);
}

#[test]
fn split_attributes_merge_in_first_header_completion_order() {
    let batch = fetch_batch(&[7, 8, 9], |peer| {
        let tag = envelope_command(peer, &[7, 8, 9]);
        // UID 7 is observed first, but its header completes last.
        let response = format!(
            "* 1 FETCH (UID 7 FLAGS (\\Seen))\r\n\
             * 3 FETCH (UID 9 RFC822.SIZE 12512)\r\n\
             * OK Still working...\r\n{}{}{}\
             * 1 FETCH (UID 7 RFC822.SIZE 12512)\r\n\
             * 3 FETCH (UID 9 FLAGS (\\Answered))\r\n\
             * 2 FETCH (UID 8)\r\n",
            header_fetch(2, 8, "FLAGS (\\Flagged) RFC822.SIZE 12512", HEADERS),
            header_fetch(3, 9, "", HEADERS),
            header_fetch(1, 7, "", HEADERS)
        );
        complete_fetch(peer, &tag, &response);
    });

    assert!(batch.failed_uids.is_empty());
    assert_eq!(
        batch
            .envelopes
            .iter()
            .map(|env| env.uid)
            .collect::<Vec<_>>(),
        [8, 9, 7]
    );
    for (envelope, flag) in batch.envelopes.iter().zip(["flagged", "answered", "seen"]) {
        assert_full_metadata(envelope);
        assert_eq!(envelope.flags, [flag]);
    }
}

#[test]
fn explicit_empty_flags_clear_but_absent_attributes_do_not_erase() {
    let batch = fetch_batch(&[7], |peer| {
        let tag = envelope_command(peer, &[7]);
        let response = format!(
            "{}* 1 FETCH (UID 7 FLAGS ())\r\n\
             * OK Still working...\r\n* 1 FETCH (UID 7)\r\n",
            header_fetch(1, 7, "FLAGS (\\Seen \\Flagged) RFC822.SIZE 12512", HEADERS)
        );
        complete_fetch(peer, &tag, &response);
    });

    assert!(batch.failed_uids.is_empty());
    assert_eq!(batch.envelopes.len(), 1);
    assert_full_metadata(&batch.envelopes[0]);
    assert!(batch.envelopes[0].flags.is_empty());
}

#[test]
fn missing_nil_and_omitted_headers_fail_only_their_requested_uids() {
    let batch = fetch_batch(&[7, 8, 9, 10, 11, 12], |peer| {
        let tag = envelope_command(peer, &[7, 8, 9, 10, 11, 12]);
        // UID 10 is omitted entirely; both supported header attributes can be NIL.
        let response = format!(
            "* 1 FETCH (UID 7 FLAGS (\\Seen))\r\n\
             * 2 FETCH (UID 8 RFC822.SIZE 512)\r\n\
             * 3 FETCH (UID 9 BODY[HEADER.FIELDS {HEADER_FIELDS}] NIL)\r\n\
             * OK Still working...\r\n\
             * 6 FETCH (UID 12 RFC822.HEADER NIL)\r\n{}",
            header_fetch(5, 11, "FLAGS (\\Seen) RFC822.SIZE 12512", HEADERS)
        );
        complete_fetch(peer, &tag, &response);
    });

    let mut failed_uids = batch.failed_uids;
    failed_uids.sort_unstable();
    assert_eq!(failed_uids, [7, 8, 9, 10, 12]);
    assert_eq!(batch.envelopes.len(), 1);
    assert_eq!(batch.envelopes[0].uid, 11);
    assert_full_metadata(&batch.envelopes[0]);
    assert_eq!(batch.envelopes[0].flags, ["seen"]);
}

#[test]
fn unrequested_full_header_fetch_is_ignored() {
    let batch = fetch_batch(&[7], |peer| {
        let tag = envelope_command(peer, &[7]);
        let response = format!(
            "{}{}* 2 FETCH (UID 999 FLAGS ())\r\n",
            header_fetch(2, 999, "FLAGS (\\Draft) RFC822.SIZE 12512", HEADERS),
            header_fetch(1, 7, "FLAGS (\\Seen) RFC822.SIZE 12512", HEADERS)
        );
        complete_fetch(peer, &tag, &response);
    });

    assert!(batch.failed_uids.is_empty());
    assert_eq!(batch.envelopes.len(), 1);
    assert_eq!(batch.envelopes[0].uid, 7);
    assert_full_metadata(&batch.envelopes[0]);
    assert_eq!(batch.envelopes[0].flags, ["seen"]);
}

#[test]
fn duplicate_input_uids_are_requested_and_emitted_once() {
    let batch = fetch_batch(&[7, 7, 8, 7, 8], |peer| {
        let tag = envelope_command(peer, &[7, 8]);
        let response = format!(
            "{}{}{}",
            header_fetch(2, 8, "FLAGS (\\Seen) RFC822.SIZE 12512", HEADERS),
            header_fetch(1, 7, "FLAGS (\\Seen) RFC822.SIZE 12512", HEADERS),
            header_fetch(2, 8, "FLAGS (\\Flagged) RFC822.SIZE 12512", HEADERS)
        );
        complete_fetch(peer, &tag, &response);
    });

    assert!(batch.failed_uids.is_empty());
    assert_eq!(
        batch
            .envelopes
            .iter()
            .map(|env| env.uid)
            .collect::<Vec<_>>(),
        [8, 7]
    );
    assert_full_metadata(&batch.envelopes[0]);
    assert_full_metadata(&batch.envelopes[1]);
    assert_eq!(batch.envelopes[0].flags, ["flagged"]);
    assert_eq!(batch.envelopes[1].flags, ["seen"]);
}

#[test]
fn duplicate_inputs_are_deduplicated_before_chunking_and_chunks_are_isolated() {
    let mut uids: Vec<u32> = (1..=100).collect();
    uids.extend([1, 101, 101]);
    let batch = fetch_batch(&uids, |peer| {
        let first_chunk: Vec<u32> = (1..=100).collect();
        let tag = envelope_command(peer, &first_chunk);
        // UID 101 belongs to the overall call, but not to this command's chunk.
        let mut response = header_fetch(
            101,
            101,
            "FLAGS (\\Draft) RFC822.SIZE 1",
            "Subject: Outside current chunk\r\n\r\n",
        );
        for uid in first_chunk {
            response.push_str(&header_fetch(
                uid,
                uid,
                "FLAGS (\\Seen) RFC822.SIZE 12512",
                HEADERS,
            ));
        }
        complete_fetch(peer, &tag, &response);

        let tag = envelope_command(peer, &[101]);
        let response = format!(
            "* 1 FETCH (UID 1 FLAGS ())\r\n{}",
            header_fetch(101, 101, "FLAGS (\\Flagged) RFC822.SIZE 12512", HEADERS)
        );
        complete_fetch(peer, &tag, &response);
    });

    assert!(batch.failed_uids.is_empty());
    assert_eq!(
        batch
            .envelopes
            .iter()
            .map(|env| env.uid)
            .collect::<Vec<_>>(),
        (1..=101).collect::<Vec<_>>()
    );
    for envelope in &batch.envelopes {
        assert_full_metadata(envelope);
        let flag = if envelope.uid == 101 {
            "flagged"
        } else {
            "seen"
        };
        assert_eq!(envelope.flags, [flag]);
    }
}

#[test]
fn empty_present_header_literals_are_successful_envelopes() {
    let batch = fetch_batch(&[7, 8], |peer| {
        let tag = envelope_command(peer, &[7, 8]);
        let response = format!(
            "{}* 2 FETCH (UID 8 FLAGS (\\Seen) RFC822.SIZE 23 \
             RFC822.HEADER {{0}}\r\n)\r\n",
            header_fetch(1, 7, "FLAGS () RFC822.SIZE 0", "")
        );
        complete_fetch(peer, &tag, &response);
    });

    assert!(batch.failed_uids.is_empty());
    assert_eq!(
        batch
            .envelopes
            .iter()
            .map(|env| env.uid)
            .collect::<Vec<_>>(),
        [7, 8]
    );
    for envelope in &batch.envelopes {
        assert!(envelope.subject.is_none());
        assert!(envelope.from_name.is_none());
        assert!(envelope.from_email.is_none());
        assert!(envelope.date.is_none());
        assert!(envelope.message_id.is_none());
        assert!(envelope.in_reply_to.is_none());
        assert!(envelope.references.is_empty());
        assert_eq!(envelope.to_addresses, "[]");
        assert_eq!(envelope.cc_addresses, "[]");
        assert!(!envelope.has_attachments);
    }
    assert_eq!(batch.envelopes[0].size, 0);
    assert!(batch.envelopes[0].flags.is_empty());
    assert_eq!(batch.envelopes[1].size, 23);
    assert_eq!(batch.envelopes[1].flags, ["seen"]);
}

#[test]
fn sync_inserts_one_full_message_with_latest_flags_and_reference_thread() {
    use crate::db::{self, pool::DbPool};
    use crate::mail::sync::sync_folder_envelopes_public;

    const ACCOUNT: &str = "aggregated-envelope-test";

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Sync blocks on DB writes internally; enter the runtime outside block_on.
    let _entered = runtime.enter();
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DbPool::new(&temp.path().join("aggregated-envelope.db"), 1).unwrap());
    {
        let conn = runtime.block_on(db.writer());
        db::schema::initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES (?1, 'Test', 'recipient@example.test', 'test')",
            [ACCOUNT],
        )
        .unwrap();
        db::folders::upsert_folder(&conn, ACCOUNT, "INBOX", "INBOX", Some("inbox"), None).unwrap();
        db::folders::update_uid_state(&conn, ACCOUNT, "INBOX", 7, 1).unwrap();
    }

    let inserted = with_session(
        |peer| {
            let tag = command(peer, "SELECT \"INBOX\"\r\n");
            respond(
                peer,
                &format!(
                    "* FLAGS (\\Seen \\Flagged \\Draft)\r\n* 1 EXISTS\r\n* 0 RECENT\r\n\
                     * OK [UIDVALIDITY 7] valid\r\n* OK [UIDNEXT 8] next\r\n\
                     {tag} OK [READ-WRITE] selected\r\n"
                ),
            );
            let tag = command(peer, "UID FETCH 1:* UID\r\n");
            complete_fetch(peer, &tag, "* 1 FETCH (UID 7)\r\n");
            let tag = envelope_command(peer, &[7]);
            let response = format!(
                "{}* 1 FETCH (UID 7 FLAGS (\\Seen \\Flagged project-tag))\r\n",
                header_fetch(1, 7, "FLAGS (\\Draft) RFC822.SIZE 12512", HEADERS)
            );
            complete_fetch(peer, &tag, &response);
        },
        |connection, address| {
            let config = ImapConfig {
                host: address.ip().to_string(),
                port: address.port(),
                username: "test".to_string(),
                password: "test".to_string(),
                use_tls: true,
                use_xoauth2: false,
            };
            sync_folder_envelopes_public(&db, ACCOUNT, connection, "INBOX", &config)
        },
    )
    .unwrap();

    let conn = db.reader();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row
            .get::<_, u32>(0))
            .unwrap(),
        1
    );
    conn.query_row(
        "SELECT uid, subject, from_name, from_email, to_addresses, cc_addresses,
                date, message_id, in_reply_to, thread_id, size, has_attachments,
                flags, snippet, maildir_path
         FROM messages WHERE account_id = ?1 AND folder_path = 'INBOX'",
        [ACCOUNT],
        |row| {
            assert_eq!(row.get::<_, u32>("uid")?, 7);
            assert_eq!(row.get::<_, i64>("size")?, SIZE as i64);
            assert!(row.get::<_, bool>("has_attachments")?);
            // References are consumed at insert time, not stored as a column.
            for (column, expected) in [
                ("subject", "Hälsningar"),
                ("from_name", "Sender, Test"),
                ("from_email", "sender@example.test"),
                ("date", "2026-09-12T12:00:00+00:00"),
                ("message_id", "<message@example.test>"),
                ("in_reply_to", "<parent@example.test>"),
                ("thread_id", "<root@example.test>"),
                ("snippet", "Hälsningar"),
                ("maildir_path", ""),
            ] {
                assert_eq!(
                    row.get::<_, Option<String>>(column)?.as_deref(),
                    Some(expected),
                    "persisted {column}"
                );
            }
            for (column, expected) in [
                (
                    "to_addresses",
                    serde_json::json!([
                        {"name": "Recipient", "email": "recipient@example.test"}
                    ]),
                ),
                (
                    "cc_addresses",
                    serde_json::json!([{"name": null, "email": "copy@example.test"}]),
                ),
                (
                    "flags",
                    serde_json::json!(["seen", "flagged", "project-tag"]),
                ),
            ] {
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&row.get::<_, String>(column)?)
                        .unwrap(),
                    expected,
                    "persisted {column}"
                );
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        inserted, 1,
        "duplicate FETCH rows must not inflate insert count"
    );
    assert_eq!(
        db::folders::get_last_seen_uid(&conn, ACCOUNT, "INBOX").unwrap(),
        7
    );
    assert_eq!(
        db::folders::get_folder_sync_state(&conn, ACCOUNT, "INBOX").unwrap(),
        (7, 8, 1)
    );
}
