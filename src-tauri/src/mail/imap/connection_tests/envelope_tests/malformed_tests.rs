use super::super::{mpsc, TIMEOUT};
use super::{
    accept_session, assert_full_metadata, command, complete_fetch, connect_session,
    envelope_command, fetch_batch, finish_logout, header_fetch, respond, with_session, Arc,
    EnvelopeBatch, ImapConfig, Peer, TcpListener, HEADERS, SIZE,
};

const ORPHANED_CONTINUATION: &str = " orphaned continuation\r\nSubject: private subject\r\n\r\n";
const LONE_CR: &str = "\r";
const VALID_PREFIX_THEN_LONE_CR: &str = "Subject: valid prefix\r\n\rbroken";
const MALFORMED_HEADERS: [&str; 3] = [ORPHANED_CONTINUATION, LONE_CR, VALID_PREFIX_THEN_LONE_CR];

fn rfc822_header_fetch(sequence: u32, uid: u32, headers: &str) -> String {
    format!(
        "* {sequence} FETCH (UID {uid} FLAGS (\\Seen) RFC822.SIZE {SIZE} \
         RFC822.HEADER {{{}}}\r\n{headers})\r\n",
        headers.len()
    )
}

fn assert_uid_7_failed_once_with_healthy_uid_8(batch: &EnvelopeBatch) {
    assert_eq!(batch.failed_uids, [7]);
    assert_eq!(batch.envelopes.len(), 1);
    assert_eq!(batch.envelopes[0].uid, 8);
    assert_full_metadata(&batch.envelopes[0]);
    assert_eq!(batch.envelopes[0].flags, ["seen"]);
}

fn assert_malformed_literals_are_isolated(rfc822: bool) {
    for headers in MALFORMED_HEADERS {
        assert!(mailparse::parse_headers(headers.as_bytes()).is_err());
        let batch = fetch_batch(&[7, 8], |peer| {
            let tag = envelope_command(peer, &[7, 8]);
            let malformed = if rfc822 {
                rfc822_header_fetch(1, 7, headers)
            } else {
                header_fetch(1, 7, "FLAGS (\\Seen) RFC822.SIZE 12512", headers)
            };
            let response = format!(
                "{malformed}{}",
                header_fetch(2, 8, "FLAGS (\\Seen) RFC822.SIZE 12512", HEADERS)
            );
            complete_fetch(peer, &tag, &response);
        });

        assert_uid_7_failed_once_with_healthy_uid_8(&batch);
    }
}

#[test]
fn malformed_body_header_literals_fail_only_their_uid() {
    assert_malformed_literals_are_isolated(false);
}

#[test]
fn malformed_rfc822_header_literals_fail_only_their_uid() {
    assert_malformed_literals_are_isolated(true);
}

#[test]
fn valid_malformed_valid_fetches_keep_the_uid_failed_once() {
    assert!(mailparse::parse_headers(ORPHANED_CONTINUATION.as_bytes()).is_err());
    let batch = fetch_batch(&[7, 8, 7], |peer| {
        let tag = envelope_command(peer, &[7, 8]);
        let response = format!(
            "{}{}{}{}",
            header_fetch(1, 7, "FLAGS (\\Seen) RFC822.SIZE 12512", HEADERS),
            rfc822_header_fetch(1, 7, ORPHANED_CONTINUATION),
            header_fetch(2, 8, "FLAGS (\\Seen) RFC822.SIZE 12512", HEADERS),
            header_fetch(1, 7, "FLAGS (\\Flagged) RFC822.SIZE 12512", HEADERS)
        );
        complete_fetch(peer, &tag, &response);
    });

    assert_uid_7_failed_once_with_healthy_uid_8(&batch);
}

#[test]
fn malformed_then_valid_fetch_keeps_the_uid_failed() {
    assert!(mailparse::parse_headers(VALID_PREFIX_THEN_LONE_CR.as_bytes()).is_err());
    let batch = fetch_batch(&[7, 8], |peer| {
        let tag = envelope_command(peer, &[7, 8]);
        let response = format!(
            "{}{}{}",
            header_fetch(1, 7, "RFC822.SIZE 12512", VALID_PREFIX_THEN_LONE_CR),
            rfc822_header_fetch(1, 7, HEADERS),
            header_fetch(2, 8, "FLAGS (\\Seen) RFC822.SIZE 12512", HEADERS)
        );
        complete_fetch(peer, &tag, &response);
    });

    assert_uid_7_failed_once_with_healthy_uid_8(&batch);
}

#[test]
fn repeated_malformed_headers_and_flag_only_updates_cannot_restore_a_uid() {
    assert!(mailparse::parse_headers(LONE_CR.as_bytes()).is_err());
    let batch = fetch_batch(&[7, 7, 8, 7], |peer| {
        let tag = envelope_command(peer, &[7, 8]);
        let response = format!(
            "* 1 FETCH (UID 7 FLAGS (\\Draft))\r\n{}{}{}\
             * 1 FETCH (UID 7 FLAGS (\\Seen \\Flagged) RFC822.SIZE 12512)\r\n\
             * 1 FETCH (UID 7 FLAGS ())\r\n",
            header_fetch(1, 7, "", LONE_CR),
            rfc822_header_fetch(1, 7, LONE_CR),
            header_fetch(2, 8, "FLAGS (\\Seen) RFC822.SIZE 12512", HEADERS)
        );
        complete_fetch(peer, &tag, &response);
    });

    assert_uid_7_failed_once_with_healthy_uid_8(&batch);
}

#[test]
fn unrequested_malformed_headers_are_ignored() {
    for headers in MALFORMED_HEADERS {
        assert!(mailparse::parse_headers(headers.as_bytes()).is_err());
        let batch = fetch_batch(&[7], |peer| {
            let tag = envelope_command(peer, &[7]);
            let response = format!(
                "{}{}{}",
                header_fetch(2, 999, "FLAGS (\\Draft) RFC822.SIZE 12512", headers),
                header_fetch(1, 7, "FLAGS (\\Seen) RFC822.SIZE 12512", HEADERS),
                rfc822_header_fetch(2, 999, headers)
            );
            complete_fetch(peer, &tag, &response);
        });

        assert!(batch.failed_uids.is_empty());
        assert_eq!(batch.envelopes.len(), 1);
        assert_eq!(batch.envelopes[0].uid, 7);
        assert_full_metadata(&batch.envelopes[0]);
        assert_eq!(batch.envelopes[0].flags, ["seen"]);
    }
}

#[test]
fn malformed_header_uid_recovers_on_the_next_command_on_the_same_connection() {
    assert!(mailparse::parse_headers(VALID_PREFIX_THEN_LONE_CR.as_bytes()).is_err());
    let (failed, poisoned_after_failure, recovered) = with_session(
        |peer| {
            let tag = envelope_command(peer, &[7, 8]);
            let response = format!(
                "{}{}",
                header_fetch(1, 7, "RFC822.SIZE 12512", VALID_PREFIX_THEN_LONE_CR),
                header_fetch(2, 8, "FLAGS (\\Seen) RFC822.SIZE 12512", HEADERS)
            );
            complete_fetch(peer, &tag, &response);

            let tag = envelope_command(peer, &[7]);
            complete_fetch(peer, &tag, &rfc822_header_fetch(1, 7, HEADERS));
        },
        |connection, _| {
            let failed = connection.fetch_envelopes_batch(&[7, 8]);
            let poisoned = connection.is_poisoned();
            let recovered = connection.fetch_envelopes_batch(&[7]);
            (failed, poisoned, recovered)
        },
    );

    assert!(!poisoned_after_failure);
    assert_uid_7_failed_once_with_healthy_uid_8(&failed.unwrap());
    let recovered = recovered.unwrap();
    assert!(recovered.failed_uids.is_empty());
    assert_eq!(recovered.envelopes.len(), 1);
    assert_eq!(recovered.envelopes[0].uid, 7);
    assert_full_metadata(&recovered.envelopes[0]);
    assert_eq!(recovered.envelopes[0].flags, ["seen"]);
}

#[test]
fn malformed_envelope_sync_holds_watermark_and_retries_before_preflight_skips() {
    use crate::db::{self, pool::DbPool};
    use crate::mail::sync::sync_folder_envelopes_public;

    const ACCOUNT: &str = "malformed-envelope-test";
    const BODY_LINK: &str = "cached/healthy-uid-3-must-survive.eml";

    fn select_unchanged_folder(peer: &mut Peer) {
        let tag = command(peer, "SELECT \"INBOX\"\r\n");
        respond(
            peer,
            &format!(
                "* FLAGS (\\Seen)\r\n* 3 EXISTS\r\n* 0 RECENT\r\n\
                 * OK [UIDVALIDITY 7] valid\r\n* OK [UIDNEXT 4] next\r\n\
                 {tag} OK [READ-WRITE] selected\r\n"
            ),
        );
    }

    fn healthy_header_fetch(uid: u32) -> String {
        let headers = HEADERS.replace(
            "<message@example.test>",
            &format!("<malformed-regression-{uid}@example.test>"),
        );
        header_fetch(uid, uid, "FLAGS () RFC822.SIZE 12512", &headers)
    }

    assert!(mailparse::parse_headers(ORPHANED_CONTINUATION.as_bytes()).is_err());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Sync blocks on DB writes internally; enter the runtime outside block_on.
    let _entered = runtime.enter();
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DbPool::new(&temp.path().join("malformed-envelope.db"), 1).unwrap());
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
        db::folders::update_uid_state(&conn, ACCOUNT, "INBOX", 7, 2).unwrap();
        db::folders::update_last_seen_uid(&conn, ACCOUNT, "INBOX", 1).unwrap();
        db::folders::update_folder_counts(&conn, ACCOUNT, "INBOX", 1, 1).unwrap();
        conn.execute(
            "INSERT INTO messages
             (id, account_id, folder_path, uid, subject, from_email, date)
             VALUES ('existing-uid-1', ?1, 'INBOX', 1, 'Hälsningar',
                     'sender@example.test', '2026-09-12T12:00:00+00:00')",
            [ACCOUNT],
        )
        .unwrap();
    }

    let assert_state = |expected_uids: &[u32], last_seen_uid: u32, uid_next: u32| {
        let conn = db.reader();
        let mut stmt = conn
            .prepare(
                "SELECT uid, subject, from_email FROM messages
                 WHERE account_id = ?1 AND folder_path = 'INBOX' ORDER BY uid",
            )
            .unwrap();
        let messages: Vec<(u32, Option<String>, String)> = stmt
            .query_map([ACCOUNT], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let expected: Vec<_> = expected_uids
            .iter()
            .map(|&uid| {
                (
                    uid,
                    Some("Hälsningar".to_string()),
                    "sender@example.test".to_string(),
                )
            })
            .collect();
        assert_eq!(messages, expected, "failed UIDs must not create blank rows");
        assert_eq!(
            db::folders::get_last_seen_uid(&conn, ACCOUNT, "INBOX").unwrap(),
            last_seen_uid
        );
        assert_eq!(
            db::folders::get_folder_sync_state(&conn, ACCOUNT, "INBOX").unwrap(),
            (7, uid_next, expected_uids.len() as i64)
        );
        assert_eq!(
            conn.query_row(
                "SELECT unread_count FROM folders WHERE account_id = ?1 AND path = 'INBOX'",
                [ACCOUNT],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            expected_uids.len() as i64
        );
    };
    let cached_row = || {
        let conn = db.reader();
        conn.query_row(
            "SELECT rowid, id, maildir_path FROM messages
             WHERE account_id = ?1 AND folder_path = 'INBOX' AND uid = 3",
            [ACCOUNT],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .unwrap()
    };
    assert_state(&[1], 1, 2);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let config = ImapConfig {
        host: address.ip().to_string(),
        port: address.port(),
        username: "test".to_string(),
        password: "test".to_string(),
        use_tls: true,
        use_xoauth2: false,
    };
    let (next_pass, wait_next_pass) = mpsc::channel();
    std::thread::scope(|scope| {
        let server = scope.spawn(move || {
            let mut peer = accept_session(&listener);
            drop(listener);
            for pass in 0..3 {
                match wait_next_pass.recv_timeout(TIMEOUT) {
                    Ok(()) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    Err(error) => panic!("sync pass {pass} was not started: {error}"),
                }
                select_unchanged_folder(&mut peer);
                if pass == 2 {
                    break;
                }
                for (spec, first_uid, flags) in [
                    ("1:* UID", 1, ""),
                    ("1:* (UID FLAGS)", 1, " FLAGS ()"),
                    ("2:* UID", 2, ""),
                ] {
                    let tag = command(&mut peer, &format!("UID FETCH {spec}\r\n"));
                    let mut response = String::new();
                    for uid in first_uid..=3 {
                        response.push_str(&format!("* {uid} FETCH (UID {uid}{flags})\r\n"));
                    }
                    complete_fetch(&mut peer, &tag, &response);
                }

                let tag = envelope_command(&mut peer, &[3, 2]);
                let uid_2 = if pass == 0 {
                    header_fetch(2, 2, "FLAGS () RFC822.SIZE 12512", ORPHANED_CONTINUATION)
                } else {
                    healthy_header_fetch(2)
                };
                complete_fetch(
                    &mut peer,
                    &tag,
                    &format!("{uid_2}{}", healthy_header_fetch(3)),
                );
            }
            // After recovery the unchanged third pass must issue SELECT only.
            finish_logout(&mut peer);
        });

        // Drop this sender before joining the server if a DB assertion fails.
        let next_pass = next_pass;
        let mut connection = connect_session(address, None);
        next_pass.send(()).unwrap();
        let inserted =
            sync_folder_envelopes_public(&db, ACCOUNT, &mut connection, "INBOX", &config).unwrap();
        assert!(!connection.is_poisoned());
        assert_state(&[1, 3], 1, 4);
        assert_eq!(inserted, 1);

        {
            let conn = runtime.block_on(db.writer());
            assert_eq!(
                conn.execute(
                    "UPDATE messages SET maildir_path = ?2
                     WHERE account_id = ?1 AND folder_path = 'INBOX' AND uid = 3",
                    rusqlite::params![ACCOUNT, BODY_LINK],
                )
                .unwrap(),
                1
            );
        }
        let before = cached_row();
        assert_eq!(before.2, BODY_LINK);

        for expected_inserted in [1, 0] {
            next_pass.send(()).unwrap();
            let inserted =
                sync_folder_envelopes_public(&db, ACCOUNT, &mut connection, "INBOX", &config)
                    .unwrap();
            assert!(!connection.is_poisoned());
            assert_state(&[1, 2, 3], 3, 4);
            assert_eq!(inserted, expected_inserted);
            assert_eq!(cached_row(), before, "healthy cached row was rebuilt");
        }
        connection.logout();
        server.join().unwrap();
    });
}
