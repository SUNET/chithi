//! Client for the `tcli` (tumpa-cli) agent.
//!
//! `tcli` ships a gpg-agent-style daemon (`tcli agent`) that caches OpenPGP
//! passphrases and smartcard PINs for the whole Tumpa tool family on a Unix
//! socket at `~/.tumpa/agent.sock`. Chithi uses it purely as a *shared
//! cache*: it reads cached secrets and writes dialog-collected secrets back,
//! so a key unlocked once in `tcli` / the Tumpa desktop app / chithi is not
//! re-prompted by the others.
//!
//! Chithi never invokes the agent's own pinentry (`GET_OR_PROMPT`): its own
//! `PassphraseDialog` / `PinDialog` stay the only prompt UI (ADR 0048).
//!
//! Protocol — line-based, `\n`-terminated, no connect banner, base64
//! (`STANDARD`, padded) values:
//!
//! ```text
//! GET_PASSPHRASE   <key>          -> PASSPHRASE <b64> | NOT_FOUND
//! PUT_PASSPHRASE   <key> <b64>    -> OK
//! CLEAR_PASSPHRASE <key>          -> OK
//! ```
//!
//! `<key>` is `passphrase:<FP>` for software keys or `pin:<FP>` for card
//! PINs, `<FP>` an uppercase OpenPGP fingerprint — byte-identical to the
//! keys `tcli` itself uses, so the two caches interoperate.
//!
//! The agent is Unix-only. On other platforms every call reports the agent
//! unavailable and callers fall back to chithi's own prompt + in-process
//! credential cache.

use zeroize::Zeroizing;

/// Outcome of an agent cache lookup.
pub enum AgentLookup {
    /// The agent is running and had the secret cached.
    Found(Zeroizing<String>),
    /// The agent is running but the key was not in its cache.
    NotFound,
    /// The agent is not reachable — no socket, connection refused, timed
    /// out, or a protocol error. Callers fall back to chithi's own prompt
    /// and in-process cache.
    Unavailable,
}

/// Agent cache key for a software key's passphrase. Matches the
/// `passphrase:<FP>` form `tcli` uses; the fingerprint is upper-cased so
/// the two tools always agree on the literal key string.
pub fn passphrase_key(fingerprint: &str) -> String {
    format!("passphrase:{}", fingerprint.to_uppercase())
}

/// Agent cache key for a smartcard user PIN — the `pin:<FP>` form `tcli`
/// uses, keyed by the card-resident key's primary fingerprint.
pub fn pin_key(fingerprint: &str) -> String {
    format!("pin:{}", fingerprint.to_uppercase())
}

#[cfg(unix)]
pub use imp::{clear, get, put};

#[cfg(not(unix))]
pub use stub::{clear, get, put};

#[cfg(not(unix))]
mod stub {
    use super::AgentLookup;

    /// No agent on non-Unix platforms — always report it unavailable.
    pub async fn get(_key: &str) -> AgentLookup {
        AgentLookup::Unavailable
    }

    /// No-op: returns `false` (not stored) so callers cache locally.
    pub async fn put(_key: &str, _secret: &str) -> bool {
        false
    }

    /// No-op.
    pub async fn clear(_key: &str) {}
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use zeroize::{Zeroize, Zeroizing};

    use super::AgentLookup;

    /// Connecting to / talking with the agent must be quick — it is a
    /// local Unix socket. A 1 s ceiling means a missing or wedged agent
    /// never stalls a sign/decrypt; the caller just falls back.
    const TIMEOUT: Duration = Duration::from_secs(1);

    /// `$TUMPA_DIR/agent.sock`, else `~/.tumpa/agent.sock` — the same path
    /// `tcli socket` prints.
    fn socket_path() -> PathBuf {
        if let Ok(dir) = std::env::var("TUMPA_DIR") {
            if !dir.is_empty() {
                return PathBuf::from(dir).join("agent.sock");
            }
        }
        dirs::home_dir()
            .unwrap_or_default()
            .join(".tumpa")
            .join("agent.sock")
    }

    /// Open the socket, send one `\n`-terminated request, read one
    /// `\n`-terminated response. Any connect/IO/timeout failure is an
    /// `Err` — callers translate that to "agent unavailable". The response
    /// is returned in a `Zeroizing` buffer because it may carry the
    /// base64 of a secret.
    async fn round_trip(path: &Path, request: &str) -> io::Result<Zeroizing<String>> {
        let stream = match tokio::time::timeout(TIMEOUT, UnixStream::connect(path)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "agent connect timed out",
                ))
            }
        };
        tokio::time::timeout(TIMEOUT, async move {
            let mut stream = stream;
            stream.write_all(request.as_bytes()).await?;
            let mut reader = BufReader::new(stream);
            let mut line = Zeroizing::new(String::new());
            reader.read_line(&mut line).await?;
            io::Result::Ok(line)
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "agent io timed out"))?
    }

    /// `GET_PASSPHRASE` — look up `key` in the agent's cache.
    async fn get_at(path: &Path, key: &str) -> AgentLookup {
        let request = format!("GET_PASSPHRASE {key}\n");
        let line = match round_trip(path, &request).await {
            Ok(l) => l,
            Err(e) => {
                log::debug!("pgp agent: unavailable ({e})");
                return AgentLookup::Unavailable;
            }
        };
        let resp = line.trim_end();
        if let Some(b64) = resp.strip_prefix("PASSPHRASE ") {
            match B64.decode(b64).map(String::from_utf8) {
                Ok(Ok(secret)) => {
                    log::debug!("pgp agent: cache hit for {key}");
                    AgentLookup::Found(Zeroizing::new(secret))
                }
                _ => {
                    log::warn!("pgp agent: malformed PASSPHRASE payload for {key}");
                    AgentLookup::Unavailable
                }
            }
        } else if resp == "NOT_FOUND" {
            log::debug!("pgp agent: cache miss for {key}");
            AgentLookup::NotFound
        } else {
            // ERR ..., empty, or anything unexpected — be conservative and
            // fall back rather than guessing.
            log::warn!("pgp agent: unexpected response for {key}");
            AgentLookup::Unavailable
        }
    }

    /// `PUT_PASSPHRASE` — store `secret` under `key`. Returns whether the
    /// agent acknowledged with `OK`; a `false` lets the caller fall back to
    /// its in-process cache so the secret is not lost.
    async fn put_at(path: &Path, key: &str, secret: &str) -> bool {
        let mut b64 = B64.encode(secret.as_bytes());
        let mut request = format!("PUT_PASSPHRASE {key} {b64}\n");
        let ok = match round_trip(path, &request).await {
            Ok(line) => line.trim_end() == "OK",
            Err(_) => false,
        };
        // The request line embedded the base64 of the secret — scrub both.
        request.zeroize();
        b64.zeroize();
        if ok {
            log::debug!("pgp agent: stored {key}");
        }
        ok
    }

    /// `CLEAR_PASSPHRASE` — drop the cached entry for `key`. Best-effort:
    /// failures are ignored (the agent may simply not be running).
    async fn clear_at(path: &Path, key: &str) {
        let request = format!("CLEAR_PASSPHRASE {key}\n");
        let _ = round_trip(path, &request).await;
    }

    /// Look up `key` in the running `tcli` agent's cache.
    pub async fn get(key: &str) -> AgentLookup {
        get_at(&socket_path(), key).await
    }

    /// Store `secret` under `key` in the `tcli` agent. Returns `true` if
    /// the agent acknowledged the write.
    pub async fn put(key: &str, secret: &str) -> bool {
        put_at(&socket_path(), key, secret).await
    }

    /// Drop the cached entry for `key` from the `tcli` agent (best-effort).
    pub async fn clear(key: &str) {
        clear_at(&socket_path(), key).await;
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::HashMap;
        use std::sync::Arc;
        use tokio::net::UnixListener;
        use tokio::sync::Mutex;

        /// In-process stand-in for `tcli agent`: a Unix listener that
        /// speaks just enough of the protocol. It stores the base64 token
        /// verbatim (exactly as a real agent would relay it), so a
        /// `put_at` followed by `get_at` exercises our own encode/decode.
        fn spawn_mock(socket: std::path::PathBuf) {
            let listener = UnixListener::bind(&socket).expect("bind mock agent socket");
            let store: Arc<Mutex<HashMap<String, String>>> = Arc::default();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let store = store.clone();
                    tokio::spawn(async move {
                        let (rd, mut wr) = stream.into_split();
                        let mut reader = BufReader::new(rd);
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let mut parts = line.trim_end().split(' ');
                        let resp = match parts.next() {
                            Some("GET_PASSPHRASE") => {
                                let key = parts.next().unwrap_or_default();
                                match store.lock().await.get(key) {
                                    Some(b64) => format!("PASSPHRASE {b64}\n"),
                                    None => "NOT_FOUND\n".to_string(),
                                }
                            }
                            Some("PUT_PASSPHRASE") => {
                                let key = parts.next().unwrap_or_default().to_string();
                                let b64 = parts.next().unwrap_or_default().to_string();
                                store.lock().await.insert(key, b64);
                                "OK\n".to_string()
                            }
                            Some("CLEAR_PASSPHRASE") => {
                                let key = parts.next().unwrap_or_default();
                                store.lock().await.remove(key);
                                "OK\n".to_string()
                            }
                            _ => "ERR Zm9v\n".to_string(),
                        };
                        let _ = wr.write_all(resp.as_bytes()).await;
                    });
                }
            });
        }

        #[tokio::test]
        async fn missing_socket_reports_unavailable() {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("absent.sock");
            assert!(matches!(
                get_at(&path, "passphrase:DEAD").await,
                AgentLookup::Unavailable
            ));
            // put on a dead socket must report "not stored", not panic.
            assert!(!put_at(&path, "passphrase:DEAD", "pw").await);
            // clear on a dead socket must be a silent no-op.
            clear_at(&path, "passphrase:DEAD").await;
        }

        #[tokio::test]
        async fn put_then_get_round_trips_a_unicode_secret() {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("agent.sock");
            spawn_mock(path.clone());

            // Before the put the key is genuinely absent.
            assert!(matches!(
                get_at(&path, "passphrase:BEEF").await,
                AgentLookup::NotFound
            ));

            // Round-trip a non-ASCII passphrase through base64.
            let secret = "córrèct-horse \u{1f510}";
            assert!(put_at(&path, "passphrase:BEEF", secret).await);
            match get_at(&path, "passphrase:BEEF").await {
                AgentLookup::Found(got) => assert_eq!(&*got, secret),
                _ => panic!("expected Found after put"),
            }

            // Clear drops exactly that entry.
            clear_at(&path, "passphrase:BEEF").await;
            assert!(matches!(
                get_at(&path, "passphrase:BEEF").await,
                AgentLookup::NotFound
            ));
        }

        #[tokio::test]
        async fn pin_and_passphrase_entries_are_independent() {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("agent.sock");
            spawn_mock(path.clone());

            assert!(put_at(&path, "pin:CAFE", "123456").await);
            assert!(put_at(&path, "passphrase:CAFE", "letmein").await);
            // Same fingerprint, different namespace — must not collide.
            match get_at(&path, "pin:CAFE").await {
                AgentLookup::Found(p) => assert_eq!(&*p, "123456"),
                _ => panic!("expected pin entry"),
            }
            match get_at(&path, "passphrase:CAFE").await {
                AgentLookup::Found(p) => assert_eq!(&*p, "letmein"),
                _ => panic!("expected passphrase entry"),
            }
        }

        /// Live round-trip against a real `tcli agent`. Ignored by
        /// default so CI and offline runs skip it; run explicitly with:
        ///
        /// ```text
        /// cargo test --lib pgp_agent -- --ignored
        /// ```
        ///
        /// Exercises the real `get` / `put` / `clear` against
        /// `socket_path()`. Uses an obviously-synthetic fingerprint and
        /// removes its own entry afterwards.
        #[tokio::test]
        #[ignore = "requires a running `tcli agent`"]
        async fn live_agent_round_trip() {
            let key = "passphrase:DEADBEEFDEADBEEFDEADBEEFDEADBEEFDEADBEEF";
            let secret = "chithi agent self-test";
            // Clean slate in case an aborted run left the entry behind.
            clear(key).await;
            assert!(
                matches!(get(key).await, AgentLookup::NotFound),
                "agent must be running and the self-test key absent — \
                 start `tcli agent`"
            );
            assert!(put(key, secret).await, "agent did not acknowledge PUT");
            match get(key).await {
                AgentLookup::Found(got) => assert_eq!(&*got, secret),
                _ => panic!("expected Found after PUT"),
            }
            clear(key).await;
            assert!(
                matches!(get(key).await, AgentLookup::NotFound),
                "CLEAR did not remove the entry"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{passphrase_key, pin_key};

    /// The namespaced keys must match `tcli`'s format exactly: a
    /// `passphrase:` / `pin:` prefix and an UPPER-CASE fingerprint. A
    /// lower-case fingerprint from the keystore must still hit the same
    /// cache slot `tcli` wrote.
    #[test]
    fn namespaced_keys_are_prefixed_and_upper_cased() {
        assert_eq!(
            passphrase_key("abcd1234abcd1234"),
            "passphrase:ABCD1234ABCD1234"
        );
        assert_eq!(pin_key("abcd1234abcd1234"), "pin:ABCD1234ABCD1234");
        // Already-upper-case input is left untouched.
        assert_eq!(passphrase_key("DEADBEEF"), "passphrase:DEADBEEF");
    }
}
