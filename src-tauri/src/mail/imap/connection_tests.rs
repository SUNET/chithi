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
