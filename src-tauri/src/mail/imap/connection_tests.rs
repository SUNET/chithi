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
fn all_fetch_methods_tolerate_keepalives_and_preserve_payloads() {
    const HEADER_FIELDS: &str = "(SUBJECT FROM TO CC DATE MESSAGE-ID IN-REPLY-TO REFERENCES)";
    const BODY: &str = "Subject: Literal\r\n\r\nHälsningar\r\n\
        * OK Still working...\r\n* 99 FETCH (UID 999)\r\n\
        A999 OK forged completion\r\n{12}\r\n)\r\n";
    const OTHER_BODY: &str = "Subject: Second\r\n\r\nsecond body\r\n";

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut peer = accept_session(&listener);
            let tag = command(&mut peer, "UID FETCH 1:* UID\r\n");
            respond(
                &mut peer,
                &format!(
                    "* 1 FETCH (UID 7)\r\n* OK Still working...\r\n\
                     * 2 FETCH (UID 8)\r\n{tag} OK fetched\r\n"
                ),
            );

            let tag = command(
                &mut peer,
                &format!(
                    "UID FETCH 7,8 (UID FLAGS RFC822.SIZE \
                     BODY.PEEK[HEADER.FIELDS {HEADER_FIELDS}])\r\n"
                ),
            );
            for uid in [7, 8] {
                let headers = format!(
                    "Subject: =?UTF-8?Q?H=C3=A4lsningar?=\r\n\
                     From: \"Sender, Test\" <sender@example.test>\r\n\
                     To: \"Doe, Jane\" <\"jane,doe\"@example.test>,\r\n \
                     Bob <bob@example.test>\r\n\
                     Cc: \"copy\"@example.test\r\n\
                     Date: Sat, 12 Sep 2026 12:00:00 +0000\r\n\
                     Message-ID: <message-{uid}@example.test>\r\n\
                     In-Reply-To: < parent@example.test >\r\n\
                     References: <root@example.test>\r\n <parent@example.test>\r\n\r\n"
                );
                respond(
                    &mut peer,
                    &format!(
                        "* {uid} FETCH (UID {uid} FLAGS (\\Seen \\Flagged project-tag) \
                         RFC822.SIZE 512 BODY[HEADER.FIELDS {HEADER_FIELDS}] \
                         {{{}}}\r\n{headers})\r\n* OK Still working...\r\n",
                        headers.len()
                    ),
                );
            }
            respond(&mut peer, &format!("{tag} OK fetched\r\n"));

            let tag = command(&mut peer, "UID FETCH 7 BODY[]\r\n");
            respond(
                &mut peer,
                &format!(
                    "* 1 FETCH (UID 7 FLAGS (\\Seen))\r\n* OK Still working...\r\n\
                     * 1 FETCH (UID 7 BODY[] {{{}}}\r\n{BODY})\r\n\
                     * OK Still working...\r\n{tag} OK fetched\r\n",
                    BODY.len()
                ),
            );

            let tag = command(&mut peer, "UID FETCH 7,8 BODY[]\r\n");
            for (uid, body) in [(7, BODY), (8, OTHER_BODY)] {
                respond(
                    &mut peer,
                    &format!(
                        "* {uid} FETCH (UID {uid} BODY[] {{{}}}\r\n{body})\r\n\
                         * OK Still working...\r\n",
                        body.len()
                    ),
                );
            }
            respond(&mut peer, &format!("{tag} OK fetched\r\n"));

            let tag = command(&mut peer, "UID FETCH 1:* (UID FLAGS)\r\n");
            respond(
                &mut peer,
                &format!(
                    "* 1 FETCH (UID 7 FLAGS (\\Seen \\Answered \\Flagged \\Deleted \
                     \\Draft \\Recent $Forwarded project-tag))\r\n\
                     * OK Still working...\r\n* 2 FETCH (UID 8 FLAGS ())\r\n\
                     {tag} OK fetched\r\n"
                ),
            );
            finish_logout(&mut peer);
        });

        let mut connection = connect_session(address, None);
        assert_eq!(connection.fetch_uids(0).unwrap(), vec![7, 8]);
        let batch = connection.fetch_envelopes_batch(&[7, 8]).unwrap();
        assert!(batch.failed_uids.is_empty());
        assert_eq!(batch.envelopes.len(), 2);
        for (envelope, uid) in batch.envelopes.iter().zip([7, 8]) {
            assert_eq!(envelope.uid, uid);
            assert_eq!(envelope.subject.as_deref(), Some("Hälsningar"));
            assert_eq!(envelope.from_name.as_deref(), Some("Sender, Test"));
            assert_eq!(envelope.from_email.as_deref(), Some("sender@example.test"));
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&envelope.to_addresses).unwrap(),
                serde_json::json!([
                    {"name": "Doe, Jane", "email": "\"jane,doe\"@example.test"},
                    {"name": "Bob", "email": "bob@example.test"}
                ])
            );
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&envelope.cc_addresses).unwrap(),
                serde_json::json!([{"name": null, "email": "\"copy\"@example.test"}])
            );
            assert_eq!(
                envelope.date.as_deref(),
                Some("Sat, 12 Sep 2026 12:00:00 +0000")
            );
            assert_eq!(
                envelope.message_id,
                Some(format!("<message-{uid}@example.test>"))
            );
            assert_eq!(
                envelope.in_reply_to.as_deref(),
                Some("<parent@example.test>")
            );
            assert_eq!(
                envelope.references,
                ["<root@example.test>", "<parent@example.test>"]
            );
            assert_eq!(envelope.flags, ["seen", "flagged", "project-tag"]);
            assert_eq!(envelope.size, 512);
        }
        assert_eq!(
            connection.fetch_message_body(7).unwrap().as_deref(),
            Some(BODY.as_bytes())
        );
        let bodies = connection.fetch_bodies_batch(&[7, 8]).unwrap();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[&7], BODY.as_bytes());
        assert_eq!(bodies[&8], OTHER_BODY.as_bytes());
        let flags = connection.fetch_all_flags().unwrap();
        assert_eq!(flags.len(), 2);
        assert_eq!(flags[0].0, 7);
        assert_eq!(
            flags[0].1,
            [
                "seen",
                "answered",
                "flagged",
                "deleted",
                "draft",
                "recent",
                "$Forwarded",
                "project-tag"
            ]
        );
        assert_eq!(flags[1], (8, Vec::<String>::new()));
        assert!(!connection.is_poisoned());
        connection.logout();
    });
}

#[test]
fn overlapping_folder_sync_rereads_checkpoint_after_waiting() {
    assert_serialized_folder_sync(false);
}

#[test]
fn uidvalidity_reset_waits_for_inflight_folder_sync() {
    assert_serialized_folder_sync(true);
}

fn assert_serialized_folder_sync(epoch_change: bool) {
    use crate::db::{self, pool::DbPool};
    use crate::mail::sync::sync_folder_envelopes_public;
    use std::time::Instant;

    const ACCOUNT: &str = "serialized-sync-test";
    const HEADER_FIELDS: &str = "(SUBJECT FROM TO CC DATE MESSAGE-ID IN-REPLY-TO REFERENCES)";
    const BODY_LINK: &str = "cached/body-must-survive.eml";

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        AFetchingEnvelope,
        BEnteredSync,
        BSelected,
    }

    // Release A before scoped threads are joined, including on assertion failure.
    struct ReleaseA(mpsc::Sender<()>);

    impl Drop for ReleaseA {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    fn selected(peer: &mut Peer, tag: &str, epoch: u32, uid_next: u32) {
        respond(
            peer,
            &format!(
                "* FLAGS (\\Seen)\r\n* 2 EXISTS\r\n* 0 RECENT\r\n\
                 * OK [UIDVALIDITY {epoch}] valid\r\n* OK [UIDNEXT {uid_next}] next\r\n\
                 {tag} OK [READ-WRITE] selected\r\n"
            ),
        );
    }

    fn envelope_command(peer: &mut Peer, uid_set: &str) -> String {
        command(
            peer,
            &format!(
                "UID FETCH {uid_set} (UID FLAGS RFC822.SIZE \
                 BODY.PEEK[HEADER.FIELDS {HEADER_FIELDS}])\r\n"
            ),
        )
    }

    fn envelopes(peer: &mut Peer, tag: &str, entries: &[(u32, u32)]) {
        for &(sequence, uid) in entries {
            let headers = format!(
                "Subject: Serialized {uid}\r\n\
                 From: Sender <sender@example.test>\r\n\
                 To: Recipient <recipient@example.test>\r\n\
                 Date: Sat, 12 Sep 2026 12:00:00 +0000\r\n\
                 Message-ID: <serialized-{uid}@example.test>\r\n\r\n"
            );
            respond(
                peer,
                &format!(
                    "* {sequence} FETCH (UID {uid} FLAGS () RFC822.SIZE 512 \
                     BODY[HEADER.FIELDS {HEADER_FIELDS}] {{{}}}\r\n{headers})\r\n",
                    headers.len()
                ),
            );
        }
        respond(peer, &format!("{tag} OK fetched\r\n"));
    }

    fn config(address: SocketAddr) -> super::ImapConfig {
        super::ImapConfig {
            host: address.ip().to_string(),
            port: address.port(),
            username: "test".to_string(),
            password: "test".to_string(),
            use_tls: true,
            use_xoauth2: false,
        }
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DbPool::new(&temp.path().join("serialized-sync.db"), 2).unwrap());
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
        db::folders::update_uid_state(&conn, ACCOUNT, "INBOX", 1, 9001).unwrap();
        db::folders::update_last_seen_uid(&conn, ACCOUNT, "INBOX", 9000).unwrap();
        db::folders::update_folder_counts(&conn, ACCOUNT, "INBOX", 1, 1).unwrap();
        conn.execute(
            "INSERT INTO messages
             (id, account_id, folder_path, uid, subject, from_email, date, maildir_path)
             VALUES ('existing-uid-9000', ?1, 'INBOX', 9000, 'Serialized 9000',
                     'sender@example.test', '2026-09-12T12:00:00+00:00', '')",
            [ACCOUNT],
        )
        .unwrap();
    }

    let assert_state = |expected_uids: &[u32], epoch: u32, uid_next: u32| {
        let conn = db.reader();
        assert_eq!(
            db::folders::get_last_seen_uid(&conn, ACCOUNT, "INBOX").unwrap(),
            *expected_uids.last().unwrap()
        );
        assert_eq!(
            db::folders::get_folder_sync_state(&conn, ACCOUNT, "INBOX").unwrap(),
            (epoch, uid_next, expected_uids.len() as i64)
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
                    format!("Serialized {uid}"),
                    "sender@example.test".to_string(),
                )
            })
            .collect();
        assert_eq!(messages, expected);
    };
    assert_state(&[9000], 1, 9001);

    let (epoch, uid_next, expected_uids): (u32, u32, &[u32]) = if epoch_change {
        (2, 3, &[1, 2])
    } else {
        (1, 10001, &[9000, 10000])
    };
    let listener_a = TcpListener::bind("127.0.0.1:0").unwrap();
    let address_a = listener_a.local_addr().unwrap();
    let listener_b = TcpListener::bind("127.0.0.1:0").unwrap();
    let address_b = listener_b.local_addr().unwrap();
    let (events, wait_event) = mpsc::channel();
    let (release_a, wait_release_a) = mpsc::channel();
    // The registry is weak: only this probe, A's owned guard and B's waiter
    // contribute strong references while A is stopped before its final writes.
    let lock = db.imap_folder_sync_lock(ACCOUNT, "INBOX");

    std::thread::scope(|scope| {
        let release_a = ReleaseA(release_a);
        let events = &events;
        let db = &db;
        let runtime = &runtime;
        let assert_state = &assert_state;
        let server_a = scope.spawn(move || {
            let mut peer = accept_session(&listener_a);
            drop(listener_a);
            let tag = command(&mut peer, "SELECT \"INBOX\"\r\n");
            selected(&mut peer, &tag, 1, 10001);
            for (spec, response) in [
                (
                    "1:* UID",
                    "* 1 FETCH (UID 9000)\r\n* 2 FETCH (UID 10000)\r\n",
                ),
                (
                    "1:* (UID FLAGS)",
                    "* 1 FETCH (UID 9000 FLAGS ())\r\n* 2 FETCH (UID 10000 FLAGS ())\r\n",
                ),
                ("9001:* UID", "* 2 FETCH (UID 10000)\r\n"),
            ] {
                let tag = command(&mut peer, &format!("UID FETCH {spec}\r\n"));
                respond(&mut peer, &format!("{response}{tag} OK fetched\r\n"));
            }
            let tag = envelope_command(&mut peer, "10000");
            events.send(Event::AFetchingEnvelope).unwrap();
            wait_release_a
                .recv_timeout(TIMEOUT)
                .expect("A was not released at its last envelope command");
            envelopes(&mut peer, &tag, &[(2, 10000)]);
            finish_logout(&mut peer);
        });
        let server_b = scope.spawn(move || {
            let mut peer = accept_session(&listener_b);
            drop(listener_b);
            let tag = command(&mut peer, "SELECT \"INBOX\"\r\n");
            events.send(Event::BSelected).unwrap();
            // A's watermark, rows and final preflight metadata must all be
            // committed before B can SELECT, let alone reset the UID epoch.
            assert_state(&[9000, 10000], 1, 10001);
            selected(&mut peer, &tag, epoch, uid_next);
            if epoch_change {
                let tag = command(&mut peer, "UID FETCH 1:* UID\r\n");
                respond(
                    &mut peer,
                    &format!("* 1 FETCH (UID 1)\r\n* 2 FETCH (UID 2)\r\n{tag} OK fetched\r\n"),
                );
                let tag = envelope_command(&mut peer, "2,1");
                envelopes(&mut peer, &tag, &[(2, 2), (1, 1)]);
            }
            // In the same epoch, B must use A's fresh checkpoint and issue
            // no FETCH at all. The third pass is SELECT-only in both cases.
            let tag = command(&mut peer, "SELECT \"INBOX\"\r\n");
            selected(&mut peer, &tag, epoch, uid_next);
            finish_logout(&mut peer);
        });
        let client_a = scope.spawn(move || {
            // Enter a runtime without block_on: sync itself blocks on DB writes.
            let _entered = runtime.enter();
            let mut connection = connect_session(address_a, None);
            let result = sync_folder_envelopes_public(
                db,
                ACCOUNT,
                &mut connection,
                "INBOX",
                &config(address_a),
            );
            connection.logout();
            result.unwrap()
        });

        assert_eq!(
            wait_event.recv_timeout(TIMEOUT).unwrap(),
            Event::AFetchingEnvelope
        );
        assert_state(&[9000], 1, 9001);
        let client_b = scope.spawn(move || {
            let _entered = runtime.enter();
            let mut connection = connect_session(address_b, None);
            let config = config(address_b);
            events.send(Event::BEnteredSync).unwrap();
            assert_eq!(
                sync_folder_envelopes_public(db, ACCOUNT, &mut connection, "INBOX", &config)
                    .unwrap(),
                if epoch_change { 2 } else { 0 }
            );
            assert_state(expected_uids, epoch, uid_next);

            let cached_uid = *expected_uids.last().unwrap();
            {
                let conn = runtime.block_on(db.writer());
                assert_eq!(
                    conn.execute(
                        "UPDATE messages SET maildir_path = ?3
                         WHERE account_id = ?1 AND folder_path = 'INBOX' AND uid = ?2",
                        rusqlite::params![ACCOUNT, cached_uid, BODY_LINK],
                    )
                    .unwrap(),
                    1
                );
            }
            let cached_row = || {
                let conn = db.reader();
                conn.query_row(
                    "SELECT rowid, id, maildir_path FROM messages
                     WHERE account_id = ?1 AND folder_path = 'INBOX' AND uid = ?2",
                    rusqlite::params![ACCOUNT, cached_uid],
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
            let before = cached_row();
            assert_eq!(before.2, BODY_LINK);
            assert_eq!(
                sync_folder_envelopes_public(db, ACCOUNT, &mut connection, "INBOX", &config)
                    .unwrap(),
                0
            );
            assert_state(expected_uids, epoch, uid_next);
            assert_eq!(
                cached_row(),
                before,
                "unchanged sync rebuilt the cached row"
            );
            assert!(!connection.is_poisoned());
            connection.logout();
        });

        assert_eq!(
            wait_event.recv_timeout(TIMEOUT).unwrap(),
            Event::BEnteredSync
        );
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match wait_event.try_recv() {
                Ok(event) => panic!("B reached SELECT while A was paused: {event:?}"),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => panic!("sync event channel closed"),
            }
            if Arc::strong_count(&lock) == 3 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "B neither waited on A's folder lock nor issued SELECT"
            );
            std::thread::yield_now();
        }
        assert!(lock.try_lock().is_err(), "A must still own the folder lock");
        assert_state(&[9000], 1, 9001);
        drop(release_a);
        assert_eq!(wait_event.recv_timeout(TIMEOUT).unwrap(), Event::BSelected);

        assert_eq!(client_a.join().unwrap(), 1);
        client_b.join().unwrap();
        server_a.join().unwrap();
        server_b.join().unwrap();
    });
    assert_state(expected_uids, epoch, uid_next);
}

#[test]
fn interrupted_body_literals_fail_and_close_the_connection() {
    for batch in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (closed, wait_closed) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut peer = accept_session(&listener);
                let uid_set = if batch { "7,8" } else { "7" };
                let tag = command(&mut peer, &format!("UID FETCH {uid_set} BODY[]\r\n"));
                if batch {
                    respond(&mut peer, "* 1 FETCH (UID 7 BODY[] {2}\r\nok)\r\n");
                }
                let uid = if batch { 8 } else { 7 };
                respond(
                    &mut peer,
                    &format!(
                        "* OK Still working...\r\n\
                         * 2 FETCH (UID {uid} BODY[] {{512}}\r\n\
                         Subject: Interrupted\r\n\r\npartial body\r\n\
                         * OK Still working...\r\n{tag} OK not a completion\r\n"
                    ),
                );
                // Half-close mid-literal, retaining the read side to observe cleanup.
                peer.get_mut()
                    .get_mut()
                    .shutdown(std::net::Shutdown::Write)
                    .unwrap();
                assert_disconnected(&mut peer);
                closed.send(()).unwrap();
            });
            let control = Arc::new(IdleControl::new());
            let mut connection = connect_session(address, Some(control.clone()));
            if batch {
                assert!(connection.fetch_bodies_batch(&[7, 8]).is_err());
            } else {
                assert!(connection.fetch_message_body(7).is_err());
            }
            assert!(connection.is_poisoned());
            wait_closed.recv_timeout(TIMEOUT).unwrap();
            connection.logout();
        });
    }
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
                respond(
                    &mut peer,
                    &format!(
                        "* 1 FETCH (UID 41)\r\n* OK Still working...\r\n\
                         * 2 FETCH (UID 42)\r\n{tag} {status} rejected\r\n"
                    ),
                );
                let tag = command(&mut peer, "UID FETCH");
                respond(
                    &mut peer,
                    &format!("* 1 FETCH (UID 7)\r\n{tag} OK fetched\r\n"),
                );
            }
            finish_logout(&mut peer);
        });
        let mut connection = connect_session(address, None);
        for _ in 0..2 {
            assert!(connection.fetch_uids(0).is_err());
            assert!(!connection.is_poisoned());
            assert_eq!(connection.fetch_uids(0).unwrap(), vec![7]);
        }
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
            respond(
                peer,
                &format!("* OK Still working...\r\n{tag} NO temporarily unavailable\r\n"),
            );
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
                 BODY[HEADER.FIELDS {HEADER_FIELDS}] {{{}}}\r\n{headers})\r\n\
                 * OK Still working...\r\n",
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
                        response.push_str(&format!(
                            "* {uid} FETCH (UID {uid}{flags})\r\n* OK Still working...\r\n"
                        ));
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
