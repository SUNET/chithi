use super::{IdleControl, ImapConnection};
use native_tls::{Certificate, Identity, TlsAcceptor, TlsConnector, TlsStream};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// Public test-only identity for loopback sessions, never a production credential.
const CERT: &[u8] = include_bytes!("localhost-test.pem");
const KEY: &[u8] = include_bytes!("localhost-test-key.pem");
const TIMEOUT: Duration = Duration::from_secs(5);
type Peer = BufReader<TlsStream<TcpStream>>;

fn accept_session(listener: &TcpListener) -> Peer {
    listener.set_nonblocking(true).unwrap();
    let deadline = std::time::Instant::now() + TIMEOUT;
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "no replacement connected"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    stream.set_write_timeout(Some(TIMEOUT)).unwrap();
    let identity = Identity::from_pkcs8(CERT, KEY).unwrap();
    let acceptor = TlsAcceptor::new(identity).unwrap();
    let mut peer = BufReader::new(acceptor.accept(stream).unwrap());
    respond(&mut peer, "* OK test server ready\r\n");
    let tag = command(&mut peer, "LOGIN");
    respond(&mut peer, &format!("{tag} OK authenticated\r\n"));
    peer
}

fn connect_session(address: SocketAddr, control: Option<Arc<IdleControl>>) -> ImapConnection {
    let stream = TcpStream::connect_timeout(&address, TIMEOUT).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    stream.set_write_timeout(Some(TIMEOUT)).unwrap();
    let socket = stream.try_clone().unwrap();
    if let Some(control) = &control {
        control.register_socket(&stream).unwrap();
    }
    let connector = TlsConnector::builder()
        .add_root_certificate(Certificate::from_pem(CERT).unwrap())
        .build()
        .unwrap();
    let mut client = imap::Client::new(connector.connect("localhost", stream).unwrap());
    client.read_greeting().unwrap();
    let session = client.login("test", "test").unwrap();
    ImapConnection {
        session,
        socket,
        idle_control: control,
        poisoned: false,
    }
}

fn command(peer: &mut Peer, expected: &str) -> String {
    let mut line = String::new();
    assert!(peer.read_line(&mut line).unwrap() > 0);
    let (tag, command) = line.split_once(' ').unwrap();
    assert!(
        command.starts_with(expected),
        "expected {expected}, got {line:?}"
    );
    tag.to_string()
}

fn respond(peer: &mut Peer, response: &str) {
    peer.get_mut().write_all(response.as_bytes()).unwrap();
    peer.get_mut().flush().unwrap();
}

fn assert_disconnected(peer: &mut Peer) {
    assert!(peer.buffer().is_empty());
    let mut byte = [0];
    match peer.get_mut().get_mut().read(&mut byte) {
        Ok(0) => {}
        // Aborting TCP deliberately omits TLS close_notify as well as LOGOUT.
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        result => panic!("expected disconnect without another command, got {result:?}"),
    }
}

fn finish_logout(peer: &mut Peer) {
    let tag = command(peer, "LOGOUT");
    respond(peer, &format!("* BYE goodbye\r\n{tag} OK logged out\r\n"));
}

#[test]
fn poisoned_fetch_frees_the_slot_before_replacement_and_flagging() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (closed, wait_closed) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut peer = accept_session(&listener);
            let tag = command(&mut peer, "UID FETCH");
            respond(
                &mut peer,
                &format!("not an IMAP response\r\n{tag} OK abandoned tail\r\n"),
            );
            assert_disconnected(&mut peer);
            drop(peer);
            closed.send(()).unwrap();

            // Only admit a replacement after the first connection has closed.
            let mut peer = accept_session(&listener);
            let tag = command(&mut peer, "UID STORE");
            respond(&mut peer, &format!("{tag} OK flags set\r\n"));
            finish_logout(&mut peer);
        });

        let control = Arc::new(IdleControl::new());
        let mut connection = connect_session(address, Some(control.clone()));
        assert_eq!(connection.socket.peer_addr().unwrap(), address);
        assert!(connection.fetch_uids(0).is_err());
        assert!(connection.is_poisoned());
        // The old Session and its IDLE owner are both still alive here.
        wait_closed
            .recv_timeout(TIMEOUT)
            .expect("poisoned socket remained open");
        connection = connect_session(address, None);
        connection.set_flags(&[1], &["\\Seen"], true).unwrap();
        assert!(!connection.is_poisoned());
        connection.logout();
    });
}

#[test]
fn poisoned_logout_sends_no_protocol_command() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut peer = accept_session(&listener);
            assert_disconnected(&mut peer);
        });
        let mut connection = connect_session(address, None);
        connection.poisoned = true;
        connection.logout();
    });
}

#[test]
fn transport_errors_shutdown_even_while_session_and_idle_owner_are_alive() {
    for error in [
        imap::Error::ConnectionLost,
        imap::Error::Io(std::io::ErrorKind::TimedOut.into()),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (closed, wait_closed) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut peer = accept_session(&listener);
                assert_disconnected(&mut peer);
                closed.send(()).unwrap();
            });
            let control = Arc::new(IdleControl::new());
            let mut connection = connect_session(address, Some(control.clone()));
            connection.note_error("IMAP test transport", &error);
            connection.note_error("IMAP repeated transport error", &error);
            assert!(connection.is_poisoned());
            wait_closed.recv_timeout(TIMEOUT).unwrap();
            assert!(!control.should_stop());
            connection.logout();
        });
    }
}

#[test]
fn dropping_poisoned_session_cannot_clear_a_replacement_idle_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (closed, wait_closed) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..2 {
                let mut peer = accept_session(&listener);
                assert_disconnected(&mut peer);
                closed.send(()).unwrap();
            }
        });
        let control = Arc::new(IdleControl::new());
        let mut old = connect_session(address, Some(control.clone()));
        old.note_error("IMAP test transport", &imap::Error::ConnectionLost);
        wait_closed.recv_timeout(TIMEOUT).unwrap();
        let replacement = connect_session(address, Some(control.clone()));
        drop(old);
        control.request_stop();
        wait_closed.recv_timeout(TIMEOUT).unwrap();
        drop(replacement);
    });
}

#[test]
fn tagged_rejections_keep_the_connection_usable_and_logout_normally() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut peer = accept_session(&listener);
            for status in ["NO", "BAD"] {
                let tag = command(&mut peer, "UID FETCH");
                respond(&mut peer, &format!("{tag} {status} rejected\r\n"));
            }
            let tag = command(&mut peer, "UID FETCH");
            respond(
                &mut peer,
                &format!("* 1 FETCH (UID 7)\r\n{tag} OK fetched\r\n"),
            );
            finish_logout(&mut peer);
        });
        let mut connection = connect_session(address, None);
        for _ in 0..2 {
            assert!(connection.fetch_uids(0).is_err());
            assert!(!connection.is_poisoned());
        }
        assert_eq!(connection.fetch_uids(0).unwrap(), vec![7]);
        connection.logout();
    });
}

#[test]
fn partial_envelope_sync_retries_before_unchanged_folder_preflight_skips() {
    use crate::db::{self, pool::DbPool};
    use crate::mail::sync::sync_folder_envelopes_public;

    const ACCOUNT: &str = "partial-fetch-test";
    const LAST_UID: u32 = 102;
    const HEADER_FIELDS: &str = "(SUBJECT FROM TO CC DATE MESSAGE-ID IN-REPLY-TO REFERENCES)";

    fn select_unchanged_folder(peer: &mut Peer) {
        let tag = command(peer, "SELECT \"INBOX\"\r\n");
        respond(
            peer,
            &format!(
                "* FLAGS (\\Seen)\r\n* {LAST_UID} EXISTS\r\n* 0 RECENT\r\n\
                 * OK [UIDVALIDITY 7] valid\r\n* OK [UIDNEXT 103] next\r\n\
                 {tag} OK [READ-WRITE] selected\r\n"
            ),
        );
    }

    fn fetch_envelope_chunk(peer: &mut Peer, uids: &[u32], reject: bool) {
        let uid_set = uids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let tag = command(
            peer,
            &format!(
                "UID FETCH {uid_set} (UID FLAGS RFC822.SIZE \
                 BODY.PEEK[HEADER.FIELDS {HEADER_FIELDS}])\r\n"
            ),
        );
        if reject {
            respond(peer, &format!("{tag} NO temporarily unavailable\r\n"));
            return;
        }
        let mut response = String::new();
        for uid in uids {
            let headers = format!(
                "Subject: Regression {uid}\r\n\
                 From: Sender <sender@example.test>\r\n\
                 To: Recipient <recipient@example.test>\r\n\
                 Date: Sat, 12 Sep 2026 12:00:00 +0000\r\n\
                 Message-ID: <regression-{uid}@example.test>\r\n\r\n"
            );
            response.push_str(&format!(
                "* {uid} FETCH (UID {uid} FLAGS () RFC822.SIZE 512 \
                 BODY[HEADER.FIELDS {HEADER_FIELDS}] {{{}}}\r\n{headers})\r\n",
                headers.len()
            ));
        }
        response.push_str(&format!("{tag} OK fetched\r\n"));
        respond(peer, &response);
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Sync enters Handle::block_on for DB writes, so call it outside block_on.
    let _entered = runtime.enter();
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DbPool::new(&temp.path().join("partial-fetch.db"), 1).unwrap());
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
             VALUES ('existing-uid-1', ?1, 'INBOX', 1, 'Regression 1',
                     'sender@example.test', '2026-09-12T12:00:00+00:00')",
            [ACCOUNT],
        )
        .unwrap();
    }

    let assert_state = |expected_uids: &[u32], last_seen_uid: u32, uid_next: u32| {
        let conn = db.reader();
        assert_eq!(
            db::folders::get_last_seen_uid(&conn, ACCOUNT, "INBOX").unwrap(),
            last_seen_uid
        );
        assert_eq!(
            db::folders::get_folder_sync_state(&conn, ACCOUNT, "INBOX").unwrap(),
            (7, uid_next, expected_uids.len() as i64)
        );
        let mut stmt = conn
            .prepare(
                "SELECT uid, subject, from_email FROM messages
                 WHERE account_id = ?1 AND folder_path = 'INBOX' ORDER BY uid",
            )
            .unwrap();
        let messages: Vec<(u32, String, String)> = stmt
            .query_map([ACCOUNT], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let expected: Vec<_> = expected_uids
            .iter()
            .map(|&uid| {
                (
                    uid,
                    format!("Regression {uid}"),
                    "sender@example.test".to_string(),
                )
            })
            .collect();
        assert_eq!(messages, expected);
    };
    assert_state(&[1], 1, 2);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let config = super::ImapConfig {
        host: address.ip().to_string(),
        port: address.port(),
        username: "test".to_string(),
        password: "test".to_string(),
        use_tls: true,
        use_xoauth2: false,
    };
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let mut peer = accept_session(&listener);
            // Unexpected reconnects must fail promptly rather than queue here.
            drop(listener);
            for first_pass in [true, false] {
                select_unchanged_folder(&mut peer);
                for (spec, first_uid, flags) in [
                    ("1:* UID", 1, ""),
                    ("1:* (UID FLAGS)", 1, " FLAGS ()"),
                    ("2:* UID", 2, ""),
                ] {
                    let tag = command(&mut peer, &format!("UID FETCH {spec}\r\n"));
                    let mut response = String::new();
                    for uid in first_uid..=LAST_UID {
                        response.push_str(&format!("* {uid} FETCH (UID {uid}{flags})\r\n"));
                    }
                    response.push_str(&format!("{tag} OK fetched\r\n"));
                    respond(&mut peer, &response);
                }

                // The first 100-UID chunk succeeds; UID 2 fails without poisoning.
                let successful_chunk: Vec<u32> = (3..=LAST_UID).rev().collect();
                fetch_envelope_chunk(&mut peer, &successful_chunk, false);
                fetch_envelope_chunk(&mut peer, &[2], first_pass);
            }
            select_unchanged_folder(&mut peer);
            // A completed folder must preflight-skip all subsequent FETCH commands.
            finish_logout(&mut peer);
        });

        let mut connection = connect_session(address, None);
        let partial_uids: Vec<u32> = std::iter::once(1).chain(3..=LAST_UID).collect();
        let all_uids: Vec<u32> = (1..=LAST_UID).collect();
        for (expected_inserted, expected_uids, last_seen_uid) in [
            (100, partial_uids.as_slice(), 1),
            (1, all_uids.as_slice(), LAST_UID),
            (0, all_uids.as_slice(), LAST_UID),
        ] {
            assert_eq!(
                sync_folder_envelopes_public(&db, ACCOUNT, &mut connection, "INBOX", &config)
                    .unwrap(),
                expected_inserted
            );
            assert!(!connection.is_poisoned());
            assert_state(expected_uids, last_seen_uid, 103);
        }
        connection.logout();
    });
}
