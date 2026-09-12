use imap::types::NameAttribute;
use imap::Session;
use native_tls::TlsStream;
use std::net::TcpStream;

use crate::error::{Error, Result};
use crate::mail::search::build_imap_search;
use crate::message::{normalize_message_id, SearchHit, SearchQuery};
use crate::state::IdleControl;

fn mailbox_is_selectable(attributes: &[NameAttribute<'_>]) -> bool {
    !attributes.contains(&NameAttribute::NoSelect)
}

#[derive(Clone)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub use_tls: bool,
    /// If true, use XOAUTH2 authentication (password field contains the access token).
    pub use_xoauth2: bool,
}

/// XOAUTH2 SASL authenticator for IMAP (used by O365).
/// Format: base64("user={email}\x01auth=Bearer {token}\x01\x01")
struct XOAuth2 {
    user: String,
    token: String,
}

impl imap::Authenticator for XOAuth2 {
    type Response = String;
    fn process(&self, _challenge: &[u8]) -> Self::Response {
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token)
    }
}

/// Lightweight envelope data extracted from IMAP FETCH.
pub struct EnvelopeData {
    pub uid: u32,
    pub subject: Option<String>,
    pub from_name: Option<String>,
    pub from_email: Option<String>,
    pub to_addresses: String,
    pub cc_addresses: String,
    pub date: Option<String>,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    /// Full RFC 5322 References chain, oldest (root) first. Empty when the
    /// header is missing. Used at insert time to thread mailing-list patch
    /// series back to their parent discussion.
    pub references: Vec<String>,
    pub flags: Vec<String>,
    pub size: u64,
    pub has_attachments: bool,
}

/// Outcome of [`ImapConnection::fetch_envelopes_batch`]. Rejected chunks and
/// requested UIDs with missing or unparseable headers land in `failed_uids`,
/// so the caller can leave them outside its sync watermark and retry them later.
#[derive(Default)]
pub struct EnvelopeBatch {
    pub envelopes: Vec<EnvelopeData>,
    pub failed_uids: Vec<u32>,
}

pub struct ImapConnection {
    session: Session<TlsStream<TcpStream>>,
    /// A shutdown handle: imap::Session does not expose its underlying stream.
    socket: TcpStream,
    idle_control: Option<std::sync::Arc<IdleControl>>,
    /// Set once a failure has left unread bytes in the socket. See
    /// [`ImapConnection::is_poisoned`].
    poisoned: bool,
}

/// Keep comma-separated UID FETCH commands below conservative server argument
/// limits. Exchange/M365 rejects large explicit UID sets before we reach the
/// database batch size used by sync.
const IMAP_FETCH_UID_CHUNK_SIZE: usize = 100;

/// What chithi asks for instead of `ENVELOPE`.
///
/// `imap-proto` 0.10.2 parses an ENVELOPE address list as `NIL` or
/// `"(" 1*address ")"` (`opt_addresses`, using `many1!`), which is exactly RFC
/// 3501 §9. Proton Mail Bridge emits `()` for an address field with no
/// addresses — a message whose `To:` header is absent, empty, or a group with
/// no members. That matches neither branch, and the failure surfaces from
/// `imap`'s *reader* (`client.rs`, `ParseError::Invalid`), which abandons the
/// response mid-stream and leaves the rest of it in the socket. The connection
/// is desynchronized from that point on, so every later command on it fails
/// too.
///
/// `imap` 2.4.1 pins `imap-proto ^0.10.0`, so there is no version to upgrade
/// to. Reading the headers directly and parsing them with `mailparse` sidesteps
/// the envelope parser altogether, and folds the References/In-Reply-To fetch
/// that used to be a second round-trip into this one.
const ENVELOPE_FETCH_SPEC: &str = "(UID FLAGS RFC822.SIZE BODY.PEEK[HEADER.FIELDS \
    (SUBJECT FROM TO CC DATE MESSAGE-ID IN-REPLY-TO REFERENCES)])";

/// Format parse diagnostics without exposing correspondence in shareable logs.
/// Headers are private too: retain only command context, error kind and size.
/// Authentication challenges never produce a payload diagnostic.
fn imap_parse_diagnostic(context: &str, e: &imap::Error) -> Option<String> {
    use imap::error::ParseError;

    let imap::Error::Parse(parse_err) = e else {
        return None;
    };
    Some(match parse_err {
        ParseError::Invalid(bytes) => format!(
            "{}: parser rejected {} bytes of server response (payload redacted)",
            context,
            bytes.len(),
        ),
        ParseError::DataNotUtf8(bytes, utf8_err) => format!(
            "{}: server sent {} bytes of non-UTF-8 data ({}) (payload redacted)",
            context,
            bytes.len(),
            utf8_err,
        ),
        ParseError::Unexpected(text) => format!(
            "{}: unexpected response ({} diagnostic bytes, payload redacted)",
            context,
            text.len(),
        ),
        // Authentication challenges can carry credentials — never log them.
        ParseError::Authentication(_, _) => return None,
    })
}

/// Whether a failure left the response stream in an unknown state.
///
/// `imap` 2.4.1 reads a response line by line and, when a line fails to parse
/// outright, gives up with `ParseError::Invalid` without draining the rest of
/// the response (`client.rs`, `read_response_onto`). The unread remainder is
/// still in the socket, so the next command reads *its* predecessor's tail —
/// which is how a single bad FETCH turns into every subsequent SELECT on that
/// connection failing in microseconds.
///
/// `Error::No`/`Error::Bad` are clean: the server's tagged response was read in
/// full, so the connection stays usable.
fn leaves_stream_desynchronized(e: &imap::Error) -> bool {
    matches!(
        e,
        imap::Error::Parse(imap::error::ParseError::Invalid(_))
            | imap::Error::Io(_)
            | imap::Error::ConnectionLost
    )
}

impl ImapConnection {
    /// Connect and authenticate. Must be called from a blocking context.
    pub fn connect(config: &ImapConfig) -> Result<Self> {
        Self::connect_inner(config, None)
    }

    /// True once a failure has desynchronized the stream and shut down its
    /// socket. Callers holding one across several folders must replace it.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Log a failed IMAP command and mark the connection unusable if the
    /// failure desynchronized the stream.
    fn note_error(&mut self, context: &str, e: &imap::Error) {
        if let Some(diagnostic) = imap_parse_diagnostic(context, e) {
            log::error!("{diagnostic}");
        }
        if !self.poisoned && leaves_stream_desynchronized(e) {
            self.poisoned = true;
            // Release the server's connection slot before any caller reconnects,
            // even while the Session or an IDLE socket clone is still alive.
            if let Err(error) = self.socket.shutdown(std::net::Shutdown::Both) {
                if error.kind() != std::io::ErrorKind::NotConnected {
                    log::warn!("Failed to shut down poisoned IMAP socket: {error}");
                }
            }
            if let Some(control) = self.idle_control.take() {
                control.clear_socket();
            }
            log::error!(
                "{}: response stream left desynchronized; connection must not be reused",
                context
            );
        }
    }

    /// Convert an `imap` result into chithi's, logging it and poisoning the
    /// connection where warranted. Bind the command's result to a local first —
    /// `self.checked(ctx, self.session.foo())` borrows `self` twice.
    fn checked<T>(&mut self, context: &str, r: std::result::Result<T, imap::Error>) -> Result<T> {
        r.map_err(|e| {
            self.note_error(context, &e);
            log::error!("{} failed: {}", context, e);
            Error::Imap(e.to_string())
        })
    }

    /// Connect an IDLE session and expose its socket to the lifecycle owner so
    /// shutdown can interrupt blocking network operations.
    pub fn connect_for_idle(
        config: &ImapConfig,
        control: std::sync::Arc<IdleControl>,
    ) -> Result<Self> {
        Self::connect_inner(config, Some(control))
    }

    fn connect_inner(
        config: &ImapConfig,
        idle_control: Option<std::sync::Arc<IdleControl>>,
    ) -> Result<Self> {
        log::info!(
            "IMAP connecting to {}:{} (tls={})",
            config.host,
            config.port,
            config.use_tls
        );

        let tls = native_tls::TlsConnector::builder().build().map_err(|e| {
            log::error!("TLS connector build failed: {}", e);
            Error::Imap(e.to_string())
        })?;

        let stream = TcpStream::connect((&*config.host, config.port)).map_err(|e| {
            log::error!(
                "IMAP connection failed to {}:{}: {}",
                config.host,
                config.port,
                e
            );
            Error::Imap(e.to_string())
        })?;
        let socket = stream
            .try_clone()
            .map_err(|e| Error::Imap(format!("Failed to retain IMAP shutdown handle: {e}")))?;
        if let Some(control) = &idle_control {
            control
                .register_socket(&stream)
                .map_err(|e| Error::Imap(format!("Failed to register IDLE socket: {}", e)))?;
        }

        // Port 993 = implicit TLS (entire connection wrapped in TLS from start).
        // Other ports use STARTTLS, preserving the existing connection policy.
        let client = if config.port == 993 {
            log::debug!("IMAP using implicit TLS");
            let tls_stream = tls.connect(&config.host, stream).map_err(|e| {
                log::error!("IMAP TLS connection failed for {}: {}", config.host, e);
                Error::Imap(e.to_string())
            })?;
            let mut client = imap::Client::new(tls_stream);
            client
                .read_greeting()
                .map_err(|e| Error::Imap(e.to_string()))?;
            client
        } else {
            log::debug!("IMAP using STARTTLS");
            let mut client = imap::Client::new(stream);
            client
                .read_greeting()
                .map_err(|e| Error::Imap(e.to_string()))?;
            client
                .secure(&config.host, &tls)
                .map_err(|e| Error::Imap(e.to_string()))?
        };

        log::debug!("IMAP connected, authenticating as {}", config.username);

        let session = if config.use_xoauth2 {
            log::debug!("IMAP using XOAUTH2 authentication");
            let auth = XOAuth2 {
                user: config.username.clone(),
                token: config.password.clone(),
            };
            client.authenticate("XOAUTH2", &auth).map_err(|e| {
                log::error!("IMAP XOAUTH2 auth failed for {}: {}", config.username, e.0);
                Error::Imap(format!("XOAUTH2 auth failed: {}", e.0))
            })?
        } else {
            client
                .login(&config.username, &config.password)
                .map_err(|e| {
                    log::error!("IMAP login failed for {}: {}", config.username, e.0);
                    Error::Imap(e.0.to_string())
                })?
        };

        log::info!("IMAP authenticated as {}", config.username);
        Ok(Self {
            session,
            socket,
            idle_control,
            poisoned: false,
        })
    }

    pub fn list_folders(&mut self) -> Result<Vec<(String, String)>> {
        log::debug!("IMAP listing folders");
        let listed = self.session.list(None, Some("*"));
        let mailboxes = self.checked("IMAP LIST", listed)?;

        let mut folders = Vec::new();
        for mb in mailboxes.iter() {
            let path = mb.name().to_string();
            let delimiter = mb.delimiter().unwrap_or("/");
            // Decode IMAP Modified UTF-7 (RFC 3501 §5.1.3) to UTF-8 for display.
            // The raw path is kept for IMAP commands (SELECT, etc.).
            let decoded = utf7_imap::decode_utf7_imap(path.clone());
            let display_name = decoded
                .rsplit_once(delimiter)
                .map(|(_, last)| last.to_string())
                .unwrap_or_else(|| decoded.clone());
            folders.push((display_name, path));
        }
        log::info!("IMAP found {} folders", folders.len());
        for (display, path) in &folders {
            log::debug!("  folder: {} ({})", display, path);
        }
        Ok(folders)
    }

    /// Return the raw paths of mailboxes that currently accept `SELECT`.
    /// LIST entries marked `\Noselect` remain visible to folder sync but must
    /// not be used by bulk mailbox operations.
    pub fn list_selectable_folder_paths(&mut self) -> Result<std::collections::HashSet<String>> {
        let listed = self.session.list(None, Some("*"));
        let mailboxes = self.checked("IMAP LIST (selectable folders)", listed)?;
        Ok(mailboxes
            .iter()
            .filter(|mailbox| mailbox_is_selectable(mailbox.attributes()))
            .map(|mailbox| mailbox.name().to_string())
            .collect())
    }

    /// SELECT a folder. Returns (exists, uid_validity, uid_next).
    pub fn select_folder(&mut self, folder: &str) -> Result<(u32, u32, u32)> {
        log::debug!("IMAP SELECT {}", folder);
        let selected = self.session.select(folder);
        let mailbox = self.checked(&format!("IMAP SELECT {}", folder), selected)?;
        let exists = mailbox.exists;
        let uid_validity = mailbox.uid_validity.unwrap_or(0);
        let uid_next = mailbox.uid_next.unwrap_or(0);
        log::debug!(
            "IMAP SELECT {}: {} messages, uidvalidity={}, uidnext={}",
            folder,
            exists,
            uid_validity,
            uid_next,
        );
        Ok((exists, uid_validity, uid_next))
    }

    /// Run `UID FETCH <uid_set> <query>` and tolerantly walk the response,
    /// handing each result's UID and attributes to `extract` — see
    /// [`parse_tolerant_fetches`] for why this is used instead of
    /// `Session::uid_fetch` directly.
    ///
    /// Bypassing `uid_fetch` also means skipping its private input
    /// validation (rejecting embedded control characters in `uid_set`/
    /// `query`) — fine here since every call site builds both from
    /// internally-validated `u32` UIDs and hardcoded query literals, never
    /// from unvalidated external input.
    fn tolerant_uid_fetch<T>(
        &mut self,
        uid_set: &str,
        query: &str,
        extract: impl FnMut(u32, &[imap_proto::types::AttributeValue<'_>]) -> Option<T>,
    ) -> Result<Vec<T>> {
        let command = format!("UID FETCH {} {}", uid_set, query);
        let fetched = self.session.run_command_and_read_response(&command);
        let raw = self.checked(&format!("IMAP {command}"), fetched)?;
        parse_tolerant_fetches(&command, &raw, extract)
    }

    /// Fetch UIDs in folder. If since_uid > 0, only fetch UIDs after it.
    pub fn fetch_uids(&mut self, since_uid: u32) -> Result<Vec<u32>> {
        let range = if since_uid > 0 {
            format!("{}:*", since_uid + 1)
        } else {
            "1:*".to_string()
        };
        log::debug!("IMAP UID FETCH {} (since_uid={})", range, since_uid);

        let uids = self.tolerant_uid_fetch(&range, "UID", |uid, _attrs| Some(uid))?;
        let uids: Vec<u32> = uids.into_iter().filter(|&uid| uid > since_uid).collect();

        log::debug!("IMAP fetched {} new UIDs", uids.len());
        Ok(uids)
    }

    /// Fetch lightweight envelopes (no body) for a batch of UIDs.
    /// This is ~100x faster than fetching full bodies.
    ///
    /// A chunk that the server or the response parser rejects is reported in
    /// [`EnvelopeBatch::failed_uids`] rather than aborting the whole batch —
    /// one unreadable message must not cost a folder. Once the connection is
    /// poisoned no further chunk is attempted, since nothing read off a
    /// desynchronized stream can be trusted.
    /// Attributes are merged per requested UID before emitting one envelope;
    /// unsolicited flag-only responses cannot replace its message metadata.
    pub fn fetch_envelopes_batch(&mut self, uids: &[u32]) -> Result<EnvelopeBatch> {
        let mut batch = EnvelopeBatch::default();
        if uids.is_empty() {
            return Ok(batch);
        }

        let mut seen_uids = std::collections::HashSet::new();
        let unique_uids: Vec<u32> = uids
            .iter()
            .copied()
            .filter(|uid| seen_uids.insert(*uid))
            .collect();
        log::debug!("IMAP fetching {} envelopes", unique_uids.len());

        for chunk in unique_uids.chunks(IMAP_FETCH_UID_CHUNK_SIZE) {
            if self.poisoned {
                batch.failed_uids.extend_from_slice(chunk);
                continue;
            }

            let uid_set = uid_set_string(chunk);
            log::debug!(
                "IMAP fetching {} envelopes (UIDs: {}...)",
                chunk.len(),
                &uid_set[..uid_set.len().min(80)]
            );

            let mut pending: std::collections::HashMap<u32, EnvelopeAccumulator> = chunk
                .iter()
                .map(|&uid| (uid, EnvelopeAccumulator::default()))
                .collect();
            let fetched = self.tolerant_uid_fetch(&uid_set, ENVELOPE_FETCH_SPEC, |uid, attrs| {
                let envelope = pending.get_mut(&uid)?;
                let had_header = envelope.header.is_some();
                envelope.merge_attributes(attrs);
                // Preserve first-header response order without emitting duplicates.
                (!had_header && envelope.header.is_some()).then_some(uid)
            });
            match fetched {
                Ok(header_uids) => {
                    for uid in header_uids {
                        match pending
                            .remove(&uid)
                            .and_then(|envelope| envelope.into_envelope(uid))
                        {
                            Some(envelope) => batch.envelopes.push(envelope),
                            None => batch.failed_uids.push(uid),
                        }
                    }
                    // Entries left over never received the requested header,
                    // including missing responses and flag-only updates.
                    batch.failed_uids.extend(
                        chunk
                            .iter()
                            .copied()
                            .filter(|uid| pending.contains_key(uid)),
                    );
                }
                Err(e) => {
                    log::warn!(
                        "IMAP FETCH envelopes failed for {} UIDs (skipping chunk): {}",
                        chunk.len(),
                        e
                    );
                    batch.failed_uids.extend_from_slice(chunk);
                }
            }
        }

        log::info!(
            "IMAP envelope batch: {} envelopes fetched, {} UIDs unread",
            batch.envelopes.len(),
            batch.failed_uids.len()
        );
        Ok(batch)
    }

    /// Fetch the full body (RFC822) for a single message by UID.
    /// Used on-demand when user opens a message.
    pub fn fetch_message_body(&mut self, uid: u32) -> Result<Option<Vec<u8>>> {
        log::debug!("IMAP fetching body for UID {}", uid);

        let bodies = self.tolerant_uid_fetch(&uid.to_string(), "BODY[]", |uid, attrs| {
            let body = attrs.iter().find_map(|a| match a {
                imap_proto::types::AttributeValue::BodySection {
                    section: None,
                    data: Some(body),
                    ..
                }
                | imap_proto::types::AttributeValue::Rfc822(Some(body)) => Some(*body),
                _ => None,
            })?;
            Some((uid, body.to_vec()))
        })?;

        if let Some((_, body)) = bodies.into_iter().next() {
            log::debug!("IMAP fetched body for UID {}: {} bytes", uid, body.len());
            return Ok(Some(body));
        }
        log::warn!("IMAP no body returned for UID {}", uid);
        Ok(None)
    }

    /// Fetch bodies for multiple UIDs in a single IMAP command.
    /// Returns a map of UID → body bytes.
    pub fn fetch_bodies_batch(
        &mut self,
        uids: &[u32],
    ) -> Result<std::collections::HashMap<u32, Vec<u8>>> {
        if uids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let uid_set: String = uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        log::debug!("IMAP batch fetching {} bodies", uids.len());

        let fetched = self.tolerant_uid_fetch(&uid_set, "BODY[]", |uid, attrs| {
            let body = attrs.iter().find_map(|a| match a {
                imap_proto::types::AttributeValue::BodySection {
                    section: None,
                    data: Some(body),
                    ..
                }
                | imap_proto::types::AttributeValue::Rfc822(Some(body)) => Some(*body),
                _ => None,
            })?;
            Some((uid, body.to_vec()))
        })?;

        let results: std::collections::HashMap<u32, Vec<u8>> = fetched.into_iter().collect();

        log::debug!("IMAP batch fetched {} bodies", results.len());
        Ok(results)
    }

    /// Create a new mailbox (folder) on the IMAP server.
    pub fn create_folder(&mut self, folder_path: &str) -> Result<()> {
        // Encode UTF-8 folder name to IMAP Modified UTF-7 (RFC 3501 §5.1.3)
        let encoded = utf7_imap::encode_utf7_imap(folder_path.to_string());
        log::info!(
            "IMAP creating folder: {} (encoded: {})",
            folder_path,
            encoded
        );
        self.session.create(&encoded).map_err(|e| {
            log::error!("IMAP CREATE folder '{}' failed: {}", folder_path, e);
            Error::Imap(e.to_string())
        })?;
        // Subscribe so it shows in LIST
        self.session.subscribe(&encoded).ok();
        Ok(())
    }

    pub fn delete_folder(&mut self, folder_path: &str) -> Result<()> {
        log::info!("IMAP deleting folder: {}", folder_path);
        self.session.unsubscribe(folder_path).ok();
        self.session.delete(folder_path).map_err(|e| {
            log::error!("IMAP DELETE folder '{}' failed: {}", folder_path, e);
            Error::Imap(e.to_string())
        })?;
        Ok(())
    }

    /// Move messages to a destination folder.
    ///
    /// Uses COPY + STORE \Deleted + EXPUNGE, which works on all IMAP servers
    /// (unlike the MOVE extension which isn't universally supported).
    pub fn move_messages(&mut self, uids: &[u32], dest_folder: &str) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }

        let uid_set = uid_set_string(uids);
        log::info!(
            "IMAP moving {} messages (UIDs: {}) to '{}'",
            uids.len(),
            &uid_set[..uid_set.len().min(80)],
            dest_folder
        );

        // 1. Copy messages to destination
        let quoted_dest = quote_mailbox_for_imap(dest_folder)?;
        let copied = self.session.uid_copy(&uid_set, &quoted_dest);
        self.checked(&format!("IMAP UID COPY to '{}'", dest_folder), copied)?;
        log::debug!("IMAP COPY to '{}' succeeded", dest_folder);

        // 2. Mark originals as deleted
        let stored = self.session.uid_store(&uid_set, "+FLAGS (\\Deleted)");
        self.checked("IMAP UID STORE +FLAGS \\Deleted", stored)?;
        log::debug!("IMAP marked {} messages as \\Deleted", uids.len());

        // 3. Expunge to permanently remove
        let expunged = self.session.expunge();
        self.checked("IMAP EXPUNGE", expunged)?;
        log::info!(
            "IMAP move complete: {} messages moved to '{}'",
            uids.len(),
            dest_folder
        );

        Ok(())
    }

    /// Delete messages from the currently selected folder.
    ///
    /// Marks messages with \Deleted flag and expunges them.
    pub fn delete_messages(&mut self, uids: &[u32]) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }

        let uid_set = uid_set_string(uids);
        log::info!(
            "IMAP deleting {} messages (UIDs: {})",
            uids.len(),
            &uid_set[..uid_set.len().min(80)]
        );

        // Store \Deleted flag
        let stored = self.session.uid_store(&uid_set, "+FLAGS (\\Deleted)");
        self.checked("IMAP UID STORE +FLAGS \\Deleted", stored)?;
        log::debug!("IMAP marked {} messages as \\Deleted", uids.len());

        // Expunge
        let expunged = self.session.expunge();
        self.checked("IMAP EXPUNGE", expunged)?;
        log::info!("IMAP delete complete: {} messages expunged", uids.len());

        Ok(())
    }

    /// Set or unset flags on messages.
    ///
    /// If `add` is true, adds the flags (+FLAGS); otherwise removes them (-FLAGS).
    /// Well-known system flag names (case-insensitive, with or without a leading
    /// `\`) are translated to their canonical wire form (e.g. `seen` → `\Seen`).
    /// Anything else is passed through verbatim as a user keyword.
    pub fn set_flags(&mut self, uids: &[u32], flags: &[&str], add: bool) -> Result<()> {
        if uids.is_empty() || flags.is_empty() {
            return Ok(());
        }

        let uid_set = uid_set_string(uids);
        let wire_flags: Vec<String> = flags
            .iter()
            .filter(|f| {
                if is_recent_flag(f) {
                    log::warn!("IMAP set_flags: ignoring \\Recent (server-set only)");
                    return false;
                }
                true
            })
            .map(|f| flag_to_wire(f))
            .collect();
        if wire_flags.is_empty() {
            return Ok(());
        }
        let flags_str = wire_flags.join(" ");
        let action = if add { "+FLAGS" } else { "-FLAGS" };
        let store_cmd = format!("{} ({})", action, flags_str);

        log::info!(
            "IMAP {} flags [{}] on {} messages (UIDs: {})",
            if add { "adding" } else { "removing" },
            flags_str,
            uids.len(),
            &uid_set[..uid_set.len().min(80)]
        );

        let stored = self.session.uid_store(&uid_set, &store_cmd);
        self.checked(&format!("IMAP UID STORE {}", store_cmd), stored)?;

        log::info!(
            "IMAP flags updated: {} {} on {} messages",
            if add { "added" } else { "removed" },
            flags_str,
            uids.len()
        );

        Ok(())
    }

    /// Mark all messages in the currently selected folder as \Seen.
    /// Uses .SILENT to suppress per-message FETCH responses, which can be
    /// very large on folders with many messages.
    pub fn mark_all_seen(&mut self) -> Result<()> {
        let stored = self.session.uid_store("1:*", "+FLAGS.SILENT (\\Seen)");
        self.checked("IMAP UID STORE +FLAGS.SILENT \\Seen", stored)?;
        Ok(())
    }

    /// Fetch current flags for all messages in the selected folder.
    /// Returns a map of UID → flags vec. Uses `1:*` to get everything.
    pub fn fetch_all_flags(&mut self) -> Result<Vec<(u32, Vec<String>)>> {
        self.tolerant_uid_fetch("1:*", "(UID FLAGS)", |uid, attrs| {
            let flags: Vec<String> = attrs
                .iter()
                .find_map(|a| match a {
                    imap_proto::types::AttributeValue::Flags(fs) => Some(
                        fs.iter()
                            .map(|s| flag_to_string(&imap::types::Flag::from(*s)))
                            .collect(),
                    ),
                    _ => None,
                })
                .unwrap_or_default();
            Some((uid, flags))
        })
    }

    /// Copy messages to a destination folder without removing originals.
    pub fn copy_messages(&mut self, uids: &[u32], dest_folder: &str) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }

        let uid_set = uid_set_string(uids);
        log::info!(
            "IMAP copying {} messages (UIDs: {}) to '{}'",
            uids.len(),
            &uid_set[..uid_set.len().min(80)],
            dest_folder
        );

        let quoted_dest = quote_mailbox_for_imap(dest_folder)?;
        let copied = self.session.uid_copy(&uid_set, &quoted_dest);
        self.checked(&format!("IMAP UID COPY to '{}'", dest_folder), copied)?;

        log::info!(
            "IMAP copy complete: {} messages copied to '{}'",
            uids.len(),
            dest_folder
        );

        Ok(())
    }

    /// Append a raw RFC5322 message to a folder (used for saving drafts).
    pub fn append_message(&mut self, folder: &str, message: &[u8]) -> Result<()> {
        log::info!(
            "IMAP appending message ({} bytes) to folder '{}'",
            message.len(),
            folder
        );
        self.session
            .append_with_flags(
                folder,
                message,
                &[imap::types::Flag::Seen, imap::types::Flag::Draft],
            )
            .map_err(|e| Error::Imap(format!("IMAP APPEND failed: {}", e)))?;
        log::info!("IMAP message appended to '{}'", folder);
        Ok(())
    }

    /// Append a raw RFC5322 message to a folder, marking it `\Seen`.
    ///
    /// Used by the post-SMTP-send hook (#189) to populate the Sent
    /// folder. Differs from [`Self::append_message`] in that it does
    /// **not** set `\Draft` — these are delivered messages, not drafts.
    pub fn append_sent_message(&mut self, folder: &str, message: &[u8]) -> Result<()> {
        log::info!(
            "IMAP appending sent message ({} bytes) to folder '{}'",
            message.len(),
            folder
        );
        self.session
            .append_with_flags(folder, message, &[imap::types::Flag::Seen])
            .map_err(|e| Error::Imap(format!("IMAP APPEND to '{}' failed: {}", folder, e)))?;
        log::info!("IMAP sent message appended to '{}'", folder);
        Ok(())
    }

    /// Append a raw RFC5322 message to a folder preserving its original state
    /// (no extra flags). Used for cross-account moves where we want to keep
    /// the message as-is.
    pub fn append_message_raw(&mut self, folder: &str, message: &[u8]) -> Result<()> {
        log::info!(
            "IMAP appending raw message ({} bytes) to folder '{}'",
            message.len(),
            folder
        );
        self.session
            .append(folder, message)
            .map_err(|e| Error::Imap(format!("IMAP APPEND failed: {}", e)))?;
        log::info!("IMAP raw message appended to '{}'", folder);
        Ok(())
    }

    /// Enter IMAP IDLE on the currently selected folder.
    /// Blocks until the server sends a notification (new mail, expunge, etc.)
    /// or the timeout expires. Returns true if there was a server notification.
    pub fn idle_wait(&mut self, timeout: std::time::Duration) -> Result<bool> {
        log::debug!("IMAP entering IDLE (timeout={}s)", timeout.as_secs());
        let mut idle = self
            .session
            .idle()
            .map_err(|e| Error::Imap(format!("IDLE setup failed: {}", e)))?;
        idle.set_keepalive(std::time::Duration::from_secs(300)); // 5 min keepalive
        let outcome = idle
            .wait_with_timeout(timeout)
            .map_err(|e| Error::Imap(format!("IMAP IDLE wait failed: {}", e)))?;
        let had_notification = idle_outcome_has_notification(outcome);
        if had_notification {
            log::info!("IMAP IDLE: server notification received");
        } else {
            log::debug!("IMAP IDLE: timeout reached, no notification");
        }
        Ok(had_notification)
    }

    /// Issue a `UID SEARCH` command against the currently selected mailbox
    /// and return matching UIDs. The query string is the raw search key
    /// (e.g., `CHARSET UTF-8 SUBJECT "foo"`).
    pub fn uid_search(&mut self, query: &str) -> Result<Vec<u32>> {
        // The query string carries user-provided search text; log only its
        // shape so debug output is safe to share.
        log::debug!("IMAP UID SEARCH (query_len={})", query.len());
        let searched = self.session.uid_search(query);
        let uids = self.checked("IMAP UID SEARCH", searched)?;
        Ok(uids.into_iter().collect())
    }

    pub fn logout(mut self) {
        if !self.poisoned {
            log::debug!("IMAP logging out");
            self.session.logout().ok();
        }
        if let Some(control) = &self.idle_control {
            control.clear_socket();
        }
    }
}

#[cfg(test)]
mod connection_tests;

#[cfg(test)]
mod diagnostic_tests;

fn idle_outcome_has_notification(outcome: imap::extensions::idle::WaitOutcome) -> bool {
    outcome == imap::extensions::idle::WaitOutcome::MailboxChanged
}

impl Drop for ImapConnection {
    fn drop(&mut self) {
        if let Some(control) = &self.idle_control {
            control.clear_socket();
        }
    }
}

/// Folders that contain duplicate copies of mail (Gmail virtual folders).
/// Skipping them avoids returning the same hit multiple times.
const SEARCH_SKIP_FOLDERS: &[&str] = &["[Gmail]/All Mail", "[Gmail]/Important", "[Gmail]"];

/// Cap on per-folder search hits, to bound work on huge mailboxes.
const SEARCH_PER_FOLDER_LIMIT: usize = 200;
/// Cap on total hits returned across all folders for one query.
const SEARCH_TOTAL_LIMIT: usize = 500;

/// Search across every folder of an IMAP account. Runs synchronously inside
/// a `spawn_blocking` because the `imap` crate uses a blocking session.
pub fn search_account_blocking(
    config: &ImapConfig,
    account_id: &str,
    query: &SearchQuery,
) -> Result<Vec<SearchHit>> {
    let search_arg = match build_imap_search(query) {
        Some(s) => s,
        None => return Ok(vec![]),
    };

    let mut conn = ImapConnection::connect(config)?;
    let folders = conn.list_folders()?;

    let mut hits: Vec<SearchHit> = Vec::new();
    for (_display, path) in folders {
        if hits.len() >= SEARCH_TOTAL_LIMIT {
            break;
        }
        // Nothing read after a desync means anything, so stop rather than
        // append garbage hits from the remaining folders.
        if conn.is_poisoned() {
            log::warn!("IMAP search: aborting, connection desynchronized");
            break;
        }
        if SEARCH_SKIP_FOLDERS
            .iter()
            .any(|skip| path.eq_ignore_ascii_case(skip))
        {
            continue;
        }

        if let Err(e) = conn.select_folder(&path) {
            log::warn!("IMAP search: SELECT {} failed: {}", path, e);
            continue;
        }

        let uids = match conn.uid_search(&search_arg) {
            Ok(u) => u,
            Err(e) => {
                log::warn!("IMAP search: UID SEARCH in {} failed: {}", path, e);
                continue;
            }
        };

        if uids.is_empty() {
            continue;
        }

        // UIDs are server-assigned monotonically per mailbox, so the tail of
        // the SEARCH response is the most recent slice — match the
        // newest-first ordering used by the JMAP and Graph providers.
        let take_n = uids.len().min(SEARCH_PER_FOLDER_LIMIT);
        let recent_uids = &uids[uids.len() - take_n..];
        let batch = match conn.fetch_envelopes_batch(recent_uids) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("IMAP search: envelope fetch in {} failed: {}", path, e);
                continue;
            }
        };
        if !batch.failed_uids.is_empty() {
            log::warn!(
                "IMAP search: {} messages in {} could not be read",
                batch.failed_uids.len(),
                path
            );
        }

        for env in batch.envelopes {
            if hits.len() >= SEARCH_TOTAL_LIMIT {
                break;
            }
            hits.push(envelope_to_hit(account_id, &path, env));
        }
    }

    conn.logout();
    Ok(hits)
}

/// Open an IMAP session and APPEND `raw_message` to the account's Sent
/// folder, marking it `\Seen`. Returns the folder path that succeeded
/// so the caller can nudge a targeted sync on it.
///
/// Tries `sent_folder_path` first when supplied (from the local folder
/// cache), then walks a list of common Sent-folder names — covering
/// vanilla IMAP servers (`Sent`), Courier-style hierarchies
/// (`INBOX.Sent`), Exchange / O365 (`Sent Items`), Cyrus
/// (`Sent Messages`) and Gmail (`[Gmail]/Sent Mail`).
///
/// This is the post-SMTP-send hook from #189: SMTP submission alone
/// never writes to Sent for plain IMAP or O365 SMTP+XOAUTH2 accounts.
/// JMAP submission handles Sent server-side and does not use this hook.
/// Callers should treat failures as best-effort — the message has already
/// been delivered, so a failed APPEND must NOT bubble up and trigger an
/// outbox retry (that would cause duplicate delivery).
///
/// Blocking. Wrap in `tokio::task::spawn_blocking` from async code.
pub fn append_message_to_sent(
    config: &ImapConfig,
    sent_folder_path: Option<&str>,
    raw_message: &[u8],
) -> Result<String> {
    let mut conn = ImapConnection::connect(config)?;
    // Cached path first (preferred — picked up by sync from SPECIAL-USE
    // or name heuristics), then the common fallbacks. The fallback walk
    // covers the case where the cached path is stale or wrong: an
    // account that was renamed server-side, or first-sync edge cases
    // where the cache lists an outdated path.
    let mut candidates: Vec<String> = Vec::new();
    if let Some(p) = sent_folder_path {
        candidates.push(p.to_string());
    }
    for fallback in [
        "Sent",
        "INBOX.Sent",
        "Sent Items",
        "Sent Messages",
        "[Gmail]/Sent Mail",
    ] {
        if !candidates.iter().any(|c| c == fallback) {
            candidates.push(fallback.to_string());
        }
    }
    let mut last_err: Option<Error> = None;
    for folder in candidates {
        match conn.append_sent_message(&folder, raw_message) {
            Ok(()) => {
                conn.logout();
                return Ok(folder);
            }
            Err(e) => {
                log::debug!("APPEND to Sent candidate '{}' failed: {}", folder, e);
                last_err = Some(e);
            }
        }
    }
    conn.logout();
    Err(last_err
        .unwrap_or_else(|| Error::Imap("APPEND to Sent failed: no candidate folders".into())))
}

fn envelope_to_hit(account_id: &str, folder_path: &str, env: EnvelopeData) -> SearchHit {
    let date_secs = env
        .date
        .as_deref()
        .and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok())
        .map(|dt| dt.timestamp())
        .unwrap_or(0);

    SearchHit {
        account_id: account_id.to_string(),
        folder_path: folder_path.to_string(),
        uid: Some(env.uid),
        message_id: env.message_id,
        backend_id: format!("{}:{}", folder_path, env.uid),
        subject: env.subject.unwrap_or_default(),
        from_name: env.from_name,
        from_email: env.from_email,
        date: date_secs,
        snippet: None,
    }
}

/// Build a comma-separated UID set string from a slice of UIDs.
fn uid_set_string(uids: &[u32]) -> String {
    uids.iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Walk a raw IMAP response byte stream, keeping only `Response::Fetch`
/// lines and handing each one's UID and attributes to `extract`; everything
/// else — status updates, a `* OK Still working ...` keep-alive, EXISTS/
/// RECENT notices, the tagged completion line — is skipped rather than
/// treated as a fatal parse error.
///
/// RFC 3501 §7 permits a server to send an untagged status response at any
/// point in a command's output, to keep the connection alive during a slow
/// scan. `imap` 2.4.1's own response parser only tolerates a fixed handful
/// of unsolicited response kinds (`Status`, `Recent`, `Flags`, `Exists`,
/// `Expunge`) and aborts the *whole command* on anything else — including
/// this keep-alive — discarding every FETCH result the server already sent,
/// even though the server went on to complete the command successfully.
/// This is a lower-level, more tolerant replacement for `Session::uid_fetch`
/// used together with [`ImapConnection::tolerant_uid_fetch`], which supplies
/// `raw` via `Session::run_command_and_read_response`. On success that reader
/// has consumed and validated the tagged completion, so this second parsing
/// pass cannot leave unread response data. Reader failures still pass through
/// the connection's poison detection and redacted diagnostics.
fn parse_tolerant_fetches<T>(
    command: &str,
    raw: &[u8],
    mut extract: impl FnMut(u32, &[imap_proto::types::AttributeValue<'_>]) -> Option<T>,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    let mut rest = raw;
    while !rest.is_empty() {
        match imap_proto::parse_response(rest) {
            Ok((remaining, imap_proto::types::Response::Fetch(_, attrs))) => {
                rest = remaining;
                let uid = attrs.iter().find_map(|a| match a {
                    imap_proto::types::AttributeValue::Uid(uid) => Some(*uid),
                    _ => None,
                });
                if let Some(uid) = uid {
                    if let Some(item) = extract(uid, &attrs) {
                        out.push(item);
                    }
                }
            }
            // Anything else the server sent (a keep-alive, a flag/exists
            // notification, the tagged completion, ...) carries nothing
            // `extract` needs — skip it and keep walking.
            Ok((remaining, _other)) => rest = remaining,
            Err(_) => {
                return Err(Error::Imap(format!(
                    "{}: unparseable response ({} bytes remaining)",
                    command,
                    rest.len()
                )));
            }
        }
    }
    Ok(out)
}

/// Quote a mailbox name as an IMAP RFC 3501 quoted-string.
///
/// `imap` 2.4.1's `Session::uid_copy` / `Session::copy` interpolate the
/// destination mailbox name into `UID COPY <set> <name>` without any
/// quoting, so a name containing a space (e.g. `Infra/SUNET Drive`) is
/// parsed by the server as two atoms and the COPY fails with
/// `Mailbox doesn't exist: Infra/SUNET`. Quote it ourselves: wrap in
/// `"` and backslash-escape `"` and `\` per the `quoted` grammar.
///
/// The `quoted` grammar (RFC 3501 §4.3) excludes CR, LF, and NUL —
/// these would break command framing on the wire. Since `dest_folder`
/// reaches us from a Tauri command argument we fail loudly on any
/// control character rather than silently stripping it (silent
/// stripping would change the destination folder, which is worse than
/// a clear error).
fn quote_mailbox_for_imap(name: &str) -> Result<String> {
    if let Some(c) = name.chars().find(|c| c.is_control()) {
        return Err(Error::Imap(format!(
            "invalid mailbox name: contains control character U+{:04X}",
            c as u32
        )));
    }
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for c in name.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Ok(out)
}

fn flag_to_string(flag: &imap::types::Flag<'_>) -> String {
    match flag {
        imap::types::Flag::Seen => "seen".to_string(),
        imap::types::Flag::Answered => "answered".to_string(),
        imap::types::Flag::Flagged => "flagged".to_string(),
        imap::types::Flag::Deleted => "deleted".to_string(),
        imap::types::Flag::Draft => "draft".to_string(),
        imap::types::Flag::Recent => "recent".to_string(),
        imap::types::Flag::MayCreate => "maycreate".to_string(),
        imap::types::Flag::Custom(s) => s.to_string(),
    }
}

/// Convert a flag name to its IMAP wire form.
///
/// Callers (and our own local DB) store system flags in their lowercase,
/// unprefixed form (`seen`, `answered`, ...). RFC 3501 STORE needs them
/// as the system-flag tokens `\Seen`, `\Answered`, ... — sending the
/// bare lowercase form creates a user keyword instead and trips the
/// parser in `imap-proto` against some servers' responses.
fn flag_to_wire(flag: &str) -> String {
    let trimmed = flag.trim_start_matches('\\');
    match trimmed.to_ascii_lowercase().as_str() {
        "seen" => "\\Seen".to_string(),
        "answered" => "\\Answered".to_string(),
        "flagged" => "\\Flagged".to_string(),
        "deleted" => "\\Deleted".to_string(),
        "draft" => "\\Draft".to_string(),
        // `\Recent` is server-set only per RFC 3501 §2.3.2 and cannot be
        // modified via STORE. Don't map `recent` here so callers can still
        // use it as a user keyword if they really want to.
        _ => flag.to_string(),
    }
}

/// True if `flag` refers to the server-managed `\Recent` system flag,
/// which RFC 3501 §2.3.2 forbids modifying via STORE.
fn is_recent_flag(flag: &str) -> bool {
    flag.trim_start_matches('\\').eq_ignore_ascii_case("recent")
}

#[cfg(test)]
mod quote_mailbox_tests {
    use super::quote_mailbox_for_imap;

    #[test]
    fn simple_name_is_quoted() {
        assert_eq!(quote_mailbox_for_imap("INBOX").unwrap(), "\"INBOX\"");
    }

    #[test]
    fn name_with_space_is_quoted_unchanged() {
        // The regression from #185: a space in the name caused the server
        // to parse `Infra/SUNET Drive` as two atoms.
        assert_eq!(
            quote_mailbox_for_imap("Infra/SUNET Drive").unwrap(),
            "\"Infra/SUNET Drive\""
        );
    }

    #[test]
    fn embedded_double_quote_is_backslash_escaped() {
        assert_eq!(
            quote_mailbox_for_imap("weird\"name").unwrap(),
            "\"weird\\\"name\""
        );
    }

    #[test]
    fn embedded_backslash_is_backslash_escaped() {
        assert_eq!(quote_mailbox_for_imap("a\\b").unwrap(), "\"a\\\\b\"");
    }

    #[test]
    fn empty_name_round_trips_to_empty_quoted_string() {
        assert_eq!(quote_mailbox_for_imap("").unwrap(), "\"\"");
    }

    #[test]
    fn control_characters_are_rejected() {
        // RFC 3501 §4.3 excludes CR/LF/NUL from the quoted grammar. We
        // reject loudly rather than silently strip — silent stripping
        // would change the destination folder, which is worse than a
        // clear error and could mask injection attempts via the Tauri
        // command argument.
        for bad in ["a\rb", "a\nb", "a\0b", "\x07bell"] {
            assert!(
                quote_mailbox_for_imap(bad).is_err(),
                "expected rejection for {:?}",
                bad
            );
        }
    }
}

#[cfg(test)]
mod mailbox_selectability_tests {
    use super::{mailbox_is_selectable, NameAttribute};

    #[test]
    fn noselect_attribute_is_not_executable() {
        assert!(!mailbox_is_selectable(&[NameAttribute::NoSelect]));
        assert!(mailbox_is_selectable(&[NameAttribute::NoInferiors]));
        assert!(mailbox_is_selectable(&[]));
    }
}

#[cfg(test)]
mod bridge_envelope_regression {
    use super::parse_envelope_headers;

    /// The response shape that broke Proton Bridge sync: `()` where RFC 3501 §9
    /// requires `NIL` for an address field with no addresses. Pinning it here
    /// documents *why* [`super::ENVELOPE_FETCH_SPEC`] avoids `ENVELOPE` — if a
    /// future `imap-proto` accepts this, the workaround can go.
    #[test]
    fn imap_proto_still_rejects_an_empty_address_list() {
        const WITH_NIL: &[u8] = b"* 1 FETCH (UID 1 ENVELOPE (\"Mon, 17 Nov 2008 17:29:20 +0100\" \
\"s\" ((\"A\" NIL \"a\" \"x.se\")) ((\"A\" NIL \"a\" \"x.se\")) ((\"A\" NIL \"a\" \"x.se\")) \
NIL NIL NIL NIL \"<m@x.se>\") FLAGS (\\Seen) RFC822.SIZE 9236)\r\n";
        const WITH_EMPTY_LIST: &[u8] =
            b"* 1 FETCH (UID 1 ENVELOPE (\"Mon, 17 Nov 2008 17:29:20 +0100\" \
\"s\" ((\"A\" NIL \"a\" \"x.se\")) ((\"A\" NIL \"a\" \"x.se\")) ((\"A\" NIL \"a\" \"x.se\")) \
() NIL NIL NIL \"<m@x.se>\") FLAGS (\\Seen) RFC822.SIZE 9236)\r\n";

        assert!(imap_proto::parse_response(WITH_NIL).is_ok());
        assert!(
            imap_proto::parse_response(WITH_EMPTY_LIST).is_err(),
            "imap-proto now accepts `()`; ENVELOPE_FETCH_SPEC may be able to use ENVELOPE again"
        );
    }

    /// The same message, read the way chithi reads it now.
    #[test]
    fn headers_survive_what_the_envelope_parser_could_not() {
        let env = parse_envelope_headers(
            b"Subject: =?utf-8?q?L=C3=A4rartr=C3=A4ff_hos_Informator?=\r\n\
From: \"Ola Skoog\" <Ola.Skoog@informator.se>\r\n\
To: undisclosed-recipients:;\r\n\
Date: Mon, 17 Nov 2008 17:29:20 +0100\r\n\
Message-ID: <AD0E2B98@se-exh01.informator.ad>\r\n\r\n",
        )
        .unwrap();

        assert_eq!(env.subject.as_deref(), Some("Lärarträff hos Informator"));
        assert_eq!(env.from_name.as_deref(), Some("Ola Skoog"));
        assert_eq!(env.from_email.as_deref(), Some("Ola.Skoog@informator.se"));
        assert_eq!(env.date.as_deref(), Some("Mon, 17 Nov 2008 17:29:20 +0100"));
        assert_eq!(
            env.message_id.as_deref(),
            Some("<AD0E2B98@se-exh01.informator.ad>")
        );
        // A group with no members contributes no recipients, which is exactly
        // the case Bridge renders as `()`.
        assert_eq!(env.to_addresses, "[]");
        assert_eq!(env.cc_addresses, "[]");
    }

    #[test]
    fn recipient_lists_keep_display_names_and_group_members() {
        let env = parse_envelope_headers(
            b"To: \"Lars Delhage\" <lasse@nohup.se>, bare@example.org\r\n\
Cc: friends: a@x.se, \"B\" <b@x.se>;\r\n\r\n",
        )
        .unwrap();

        assert_eq!(
            env.to_addresses,
            r#"[{"name":"Lars Delhage","email":"lasse@nohup.se"},{"name":null,"email":"bare@example.org"}]"#
        );
        assert_eq!(
            env.cc_addresses,
            r#"[{"name":null,"email":"a@x.se"},{"name":"B","email":"b@x.se"}]"#
        );
    }
}

#[cfg(test)]
mod flag_to_wire_tests {
    use super::flag_to_wire;

    #[test]
    fn lowercase_system_flags_become_backslashed() {
        assert_eq!(flag_to_wire("seen"), "\\Seen");
        assert_eq!(flag_to_wire("answered"), "\\Answered");
        assert_eq!(flag_to_wire("flagged"), "\\Flagged");
        assert_eq!(flag_to_wire("deleted"), "\\Deleted");
        assert_eq!(flag_to_wire("draft"), "\\Draft");
    }

    #[test]
    fn recent_is_not_mapped() {
        // RFC 3501 §2.3.2: `\Recent` is server-managed and cannot be set via
        // STORE. Leave bare `recent` alone so callers can keep it as a user
        // keyword if they explicitly want to.
        assert_eq!(flag_to_wire("recent"), "recent");
        assert_eq!(flag_to_wire("Recent"), "Recent");
    }

    #[test]
    fn canonical_form_is_idempotent() {
        assert_eq!(flag_to_wire("\\Seen"), "\\Seen");
        assert_eq!(flag_to_wire("\\Flagged"), "\\Flagged");
    }

    #[test]
    fn mixed_case_system_flag_is_normalized() {
        assert_eq!(flag_to_wire("SEEN"), "\\Seen");
        assert_eq!(flag_to_wire("Flagged"), "\\Flagged");
    }

    #[test]
    fn user_keywords_pass_through_verbatim() {
        assert_eq!(flag_to_wire("$Important"), "$Important");
        assert_eq!(flag_to_wire("Junk"), "Junk");
    }

    #[test]
    fn recent_detector_matches_canonical_and_bare_forms() {
        use super::is_recent_flag;
        assert!(is_recent_flag("\\Recent"));
        assert!(is_recent_flag("Recent"));
        assert!(is_recent_flag("recent"));
        assert!(is_recent_flag("RECENT"));
        assert!(!is_recent_flag("seen"));
        assert!(!is_recent_flag("$Recent"));
    }
}

/// Extract `<message-id>` tokens from a single header value (the part
/// after `Field-Name:`). Returned ids are canonical form.
fn extract_msgids(value: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut inside = false;
    for c in value.chars() {
        match c {
            '<' => {
                inside = true;
                buf.clear();
            }
            '>' if inside => {
                if let Some(id) = normalize_message_id(&buf) {
                    out.push(id);
                }
                inside = false;
                buf.clear();
            }
            _ if inside => buf.push(c),
            _ => {}
        }
    }
    out
}

/// The envelope fields chithi stores, recovered from a message's headers
/// rather than from the server's `ENVELOPE` structure. See
/// [`ENVELOPE_FETCH_SPEC`] for why.
#[derive(Default)]
struct HeaderEnvelope {
    subject: Option<String>,
    from_name: Option<String>,
    from_email: Option<String>,
    to_addresses: String,
    cc_addresses: String,
    date: Option<String>,
    message_id: Option<String>,
    in_reply_to: Option<String>,
    references: Vec<String>,
}

/// Attributes may arrive in separate FETCH responses for the same UID. Missing
/// attributes leave previous values intact; explicit FLAGS () still clears flags.
#[derive(Default)]
struct EnvelopeAccumulator {
    /// None means no literal arrived. An error is terminal for this command,
    /// preventing duplicate FETCHes from masking a header parse failure.
    header: Option<std::result::Result<HeaderEnvelope, ()>>,
    flags: Vec<String>,
    size: u64,
}

impl EnvelopeAccumulator {
    fn merge_attributes(&mut self, attributes: &[imap_proto::types::AttributeValue<'_>]) {
        use imap_proto::types::{AttributeValue, MessageSection, SectionPath};

        for attribute in attributes {
            match attribute {
                AttributeValue::Flags(flags) => {
                    self.flags = flags
                        .iter()
                        .map(|flag| flag_to_string(&imap::types::Flag::from(*flag)))
                        .collect();
                }
                AttributeValue::Rfc822Size(size) => self.size = u64::from(*size),
                AttributeValue::BodySection {
                    section: Some(SectionPath::Full(MessageSection::Header)),
                    data: Some(header),
                    ..
                }
                | AttributeValue::Rfc822Header(Some(header))
                    if !matches!(&self.header, Some(Err(()))) =>
                {
                    self.header = Some(parse_envelope_headers(header).ok_or(()));
                }
                _ => {}
            }
        }
    }

    fn into_envelope(self, uid: u32) -> Option<EnvelopeData> {
        let header = self.header?.ok()?;
        Some(EnvelopeData {
            uid,
            subject: header.subject,
            from_name: header.from_name,
            from_email: header.from_email,
            to_addresses: header.to_addresses,
            cc_addresses: header.cc_addresses,
            date: header.date,
            message_id: header.message_id,
            in_reply_to: header.in_reply_to,
            references: header.references,
            flags: self.flags,
            size: self.size,
            // Size-based attachment heuristic, not MIME-derived metadata.
            has_attachments: self.size > 10000,
        })
    }
}

/// Parse a `BODY.PEEK[HEADER.FIELDS (...)]` block into the fields chithi
/// stores. `mailparse::parse_headers` applies RFC 5322 §2.2.3 unfolding and
/// RFC 2047 decoding, so folded continuation lines don't split a message id in
/// half and encoded-words arrive already decoded.
/// A structural parse error returns None; a valid empty block remains successful.
fn parse_envelope_headers(bytes: &[u8]) -> Option<HeaderEnvelope> {
    let (headers, _) = mailparse::parse_headers(bytes).ok()?;

    let mut env = HeaderEnvelope::default();
    let mut from: Option<&mailparse::MailHeader<'_>> = None;
    let mut to: Option<&mailparse::MailHeader<'_>> = None;
    let mut cc: Option<&mailparse::MailHeader<'_>> = None;

    for header in &headers {
        // Only the first occurrence of each field counts (RFC 5322 §3.6
        // allows at most one, but malformed mail does repeat them).
        match header.get_key_ref().to_ascii_lowercase().as_str() {
            "subject" if env.subject.is_none() => env.subject = Some(header.get_value()),
            "date" if env.date.is_none() => env.date = Some(header.get_value()),
            "message-id" if env.message_id.is_none() => {
                env.message_id = extract_msgids(&header.get_value()).into_iter().next();
            }
            "in-reply-to" if env.in_reply_to.is_none() => {
                env.in_reply_to = extract_msgids(&header.get_value()).into_iter().next();
            }
            "references" if env.references.is_empty() => {
                env.references = extract_msgids(&header.get_value());
            }
            "from" if from.is_none() => from = Some(header),
            "to" if to.is_none() => to = Some(header),
            "cc" if cc.is_none() => cc = Some(header),
            _ => {}
        }
    }

    if let Some(first) = from.and_then(|h| header_addresses(h).into_iter().next()) {
        env.from_name = first.name;
        env.from_email = Some(first.email);
    }
    env.to_addresses = addresses_to_json(to);
    env.cc_addresses = addresses_to_json(cc);
    Some(env)
}

#[derive(serde::Serialize)]
struct AddrJson {
    name: Option<String>,
    email: String,
}

/// Track the regions where address punctuation is literal, before RFC 2047
/// decoding. Comments nest; quotes, comments and domain literals admit escapes.
#[derive(Default)]
struct AddressSyntax {
    quoted: bool,
    escaped: bool,
    comment_depth: usize,
    literal: bool,
    invalid: bool,
}

impl AddressSyntax {
    fn is_structural(&mut self, c: char) -> bool {
        if c.is_control() && c != '\t' {
            self.invalid = true;
        }
        if self.escaped {
            self.escaped = false;
        } else if self.comment_depth > 0 {
            match c {
                '\\' => self.escaped = true,
                '(' => self.comment_depth += 1,
                ')' => self.comment_depth -= 1,
                _ => {}
            }
        } else if self.quoted {
            match c {
                '\\' => self.escaped = true,
                '"' => self.quoted = false,
                _ => {}
            }
        } else if self.literal {
            match c {
                '\\' => self.escaped = true,
                ']' => self.literal = false,
                '[' => self.invalid = true,
                _ => {}
            }
        } else {
            match c {
                '"' => self.quoted = true,
                '(' => self.comment_depth = 1,
                '[' => self.literal = true,
                ')' | ']' => self.invalid = true,
                _ => return true,
            }
        }
        false
    }

    fn is_balanced(&self) -> bool {
        !self.invalid && !self.quoted && !self.escaped && !self.literal && self.comment_depth == 0
    }
}

/// Encoded words are opaque to address delimiters, including the nonconforming
/// raw Q punctuation that mailparse tolerates. Decoding still happens per name.
fn address_syntax_chars(value: &str) -> impl Iterator<Item = (usize, char)> + '_ {
    let mut encoded_until = 0;
    let mut syntax = AddressSyntax::default();
    let mut angle = false;
    value.char_indices().filter(move |&(index, c)| {
        if index < encoded_until {
            return false;
        }
        if !angle
            && !syntax.quoted
            && !syntax.literal
            && !syntax.escaped
            && value[..index]
                .chars()
                .next_back()
                .is_none_or(is_encoded_address_boundary)
        {
            if let Some(length) = encoded_address_word_len(&value[index..]) {
                if value[index + length..]
                    .chars()
                    .next()
                    .is_none_or(is_encoded_address_boundary)
                {
                    encoded_until = index + length;
                    return false;
                }
            }
        }
        if syntax.is_structural(c) {
            match c {
                '<' => angle = true,
                '>' => angle = false,
                _ => {}
            }
        }
        true
    })
}

fn is_encoded_address_boundary(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '(' | ')' | '<' | '>' | ',' | ':' | ';')
}

fn encoded_address_word_len(value: &str) -> Option<usize> {
    let (charset, rest) = value.strip_prefix("=?")?.split_once('?')?;
    if charset.is_empty() || charset.chars().any(char::is_whitespace) {
        return None;
    }
    let payload = rest
        .strip_prefix("Q?")
        .or_else(|| rest.strip_prefix("q?"))
        .or_else(|| rest.strip_prefix("B?"))
        .or_else(|| rest.strip_prefix("b?"))?;
    let end = payload.find("?=")?;
    if payload[..end].is_empty()
        || payload[..end]
            .chars()
            .any(|c| c.is_whitespace() || c == '?')
    {
        return None;
    }
    Some(value.len() - payload.len() + end + 2)
}

/// Split an address header value on the commas that actually separate
/// mailboxes — not those inside a quoted display name (`"Delhage, Lars"`), an
/// angle-bracketed address, an RFC 5322 group (`friends: a@x, b@x;`), or a
/// parenthesized comment (`John Doe (Sales, West) <john@x.se>`). RFC 5322
/// §3.2.2 comments nest and carry their own quoted-pair escapes, distinct from
/// a quoted string's.
fn split_address_list(value: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let (mut start, mut angle, mut group_depth) = (0, false, 0usize);
    let mut syntax = AddressSyntax::default();
    for (i, c) in address_syntax_chars(value) {
        if !syntax.is_structural(c) {
            continue;
        }
        match c {
            '<' => angle = true,
            '>' => angle = false,
            ':' if !angle => group_depth += 1,
            ';' if !angle => group_depth = group_depth.saturating_sub(1),
            ',' if !angle && group_depth == 0 => {
                items.push(&value[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    items.push(&value[start..]);
    items
}

/// Flatten one address header into the `{name, email}` shape chithi stores.
///
/// Parsed one mailbox at a time on purpose: `mailparse::addrparse` rejects the
/// *entire* list if any element lacks an `@`, which would drop every valid
/// recipient alongside the bad one. The server-side `ENVELOPE` parse this
/// replaced was per-address, so match that — a malformed element costs only
/// itself.
///
/// RFC 5322 group syntax (`To: undisclosed-recipients:;`) contributes its
/// members and nothing for the group name itself.
fn header_addresses(header: &mailparse::MailHeader<'_>) -> Vec<AddrJson> {
    // Always split raw syntax first: mailparse can successfully swallow a
    // bare quoted local part into the following mailbox's display name.
    // Encoded commas must remain encoded until each item has been isolated.
    let raw = unfold_header_value(header.get_value_raw());
    let mut out = Vec::new();
    for item in split_address_list(&raw) {
        if item.trim().is_empty() {
            continue;
        }
        out.extend(parse_address_item(item));
    }
    out
}

/// Unfold a raw header value per RFC 5322 §2.2.3: a line break immediately
/// followed by whitespace collapses (the whitespace itself remains and
/// supplies the word boundary); a bare line break gets a space in its place
/// so words on either side don't glue together. Runs before RFC 2047
/// decoding, so it only ever sees plain structural bytes -- an encoded word
/// is pure ASCII and contains no line breaks of its own.
fn unfold_header_value(bytes: &[u8]) -> String {
    // Match mailparse's UTF-8/Latin-1 handling without decoding encoded words.
    let raw = match std::str::from_utf8(bytes) {
        Ok(raw) => std::borrow::Cow::Borrowed(raw),
        Err(_) => {
            std::borrow::Cow::Owned(bytes.iter().copied().map(char::from).collect::<String>())
        }
    };
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {}
            '\n' => {
                if !matches!(chars.peek(), Some(' ') | Some('\t')) {
                    out.push(' ');
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Parse a raw mailbox or a single, terminated group. Group members take the
/// same mailbox path as top-level recipients, including empty/bad members.
fn parse_address_item(item: &str) -> Vec<AddrJson> {
    let mut syntax = AddressSyntax::default();
    let (mut angle, mut colon, mut terminator) = (false, None, None);
    for (i, c) in address_syntax_chars(item) {
        if !syntax.is_structural(c) {
            continue;
        }
        match c {
            '<' => angle = true,
            '>' => angle = false,
            ':' if !angle => {
                if colon.replace(i).is_some() {
                    return Vec::new();
                }
            }
            ';' if !angle => {
                if colon.is_none() || terminator.replace(i).is_some() {
                    return Vec::new();
                }
                // A member's stray closing delimiter must not reject siblings.
                // The label and each member are validated independently below.
                syntax.invalid = false;
            }
            _ => {}
        }
    }
    if angle || !syntax.is_balanced() {
        return Vec::new();
    }
    if let Some(colon) = colon {
        let Some(end) = terminator else {
            return Vec::new();
        };
        if !is_address_cfws(&item[end + 1..]) {
            return Vec::new();
        }
        // Validate the raw display-name independently of malformed members.
        let named = format!("{}<group@invalid>", &item[..colon]);
        if is_address_cfws(&item[..colon]) || parse_address_mailbox(&named).is_none() {
            return Vec::new();
        }
        return split_address_list(&item[colon + 1..end])
            .into_iter()
            .filter_map(parse_address_mailbox)
            .collect();
    }
    parse_address_mailbox(item).into_iter().collect()
}

/// Preserve addr-spec spelling, especially quoted local parts. A placeholder
/// angle address lets mailparse decode the original name without treating a
/// quoted `>` in the real local part as the end of the address.
fn parse_address_mailbox(item: &str) -> Option<AddrJson> {
    let item = item.trim();
    if let Some(bare) = without_address_comments(item) {
        if is_clean_email(bare.trim()) {
            return Some(AddrJson {
                name: None,
                email: bare.trim().to_string(),
            });
        }
    }

    let mut syntax = AddressSyntax::default();
    let (mut left, mut right, mut bare_at) = (None, None, false);
    for (i, c) in address_syntax_chars(item) {
        if !syntax.is_structural(c) {
            continue;
        }
        match c {
            '<' if left.is_none() && !bare_at => left = Some(i),
            '>' if left.is_some() && right.is_none() => right = Some(i),
            '<' | '>' => return None,
            '@' if left.is_none() => bare_at = true,
            ',' | ':' | ';' if left.is_none() || right.is_some() => return None,
            _ => {}
        }
    }
    if !syntax.is_balanced() {
        return None;
    }
    let (left, right) = (left?, right?);
    let email = without_address_comments(&item[left + 1..right])?;
    let email = email.trim();
    if !is_clean_email(email) || !is_address_cfws(&item[right + 1..]) {
        return None;
    }
    let named = format!("{}<mailbox@invalid>", &item[..left]);
    let mailbox = addrparse_raw_item(&named).ok()?.extract_single_info()?;
    Some(AddrJson {
        name: mailbox.display_name,
        email: email.to_string(),
    })
}

/// Decode and parse one raw (undecoded) address-list element as a mailbox.
/// Wraps `item` as the value of a throwaway header so [`mailparse`]'s own
/// `addrparse_header` — which decodes RFC 2047 encoded words and parses
/// RFC 5322 address syntax in the same pass — does the work, instead of
/// decoding and parsing as two separate steps.
fn addrparse_raw_item(
    item: &str,
) -> std::result::Result<mailparse::MailAddrList, mailparse::MailParseError> {
    let synthetic = format!("X-Item:{}\n", item);
    let (header, _) = mailparse::parse_header(synthetic.as_bytes())?;
    mailparse::addrparse_header(&header)
}

/// Only comments and folding whitespace may follow an angle address/group.
fn is_address_cfws(value: &str) -> bool {
    let mut syntax = AddressSyntax::default();
    for c in value.chars() {
        let comment = syntax.comment_depth > 0 || c == '(';
        let structural = syntax.is_structural(c);
        if !comment && !(structural && matches!(c, ' ' | '\t')) {
            return false;
        }
    }
    syntax.is_balanced()
}

/// Comments are CFWS, not part of the addr-spec. Leave quoted parentheses
/// alone and retain a word boundary so comments cannot join invalid tokens.
fn without_address_comments(value: &str) -> Option<std::borrow::Cow<'_, str>> {
    if !value.contains('(') {
        return Some(std::borrow::Cow::Borrowed(value));
    }
    let mut syntax = AddressSyntax::default();
    let mut out = String::with_capacity(value.len());
    let mut comment_boundary = false;
    for c in value.chars() {
        let comment = syntax.comment_depth > 0 || (c == '(' && !syntax.quoted && !syntax.literal);
        syntax.is_structural(c);
        if comment {
            comment_boundary = true;
            continue;
        }
        if comment_boundary && !out.ends_with(char::is_whitespace) && !c.is_whitespace() {
            out.push(' ');
        }
        comment_boundary = false;
        out.push(c);
    }
    syntax.is_balanced().then_some(std::borrow::Cow::Owned(out))
}

/// Check header addr-spec syntax, without SMTP's ASCII/length/routability
/// restrictions. Quoted punctuation, quoted-pairs and EAI stay byte-for-byte.
fn is_clean_email(email: &str) -> bool {
    if email.is_empty() || email != email.trim() {
        return false;
    }
    let mut syntax = AddressSyntax::default();
    let mut at = None;
    for (i, c) in email.char_indices() {
        if syntax.is_structural(c) && c == '@' && at.replace(i).is_some() {
            return false;
        }
    }
    let Some(at) = at else {
        return false;
    };
    let local = email[..at].trim();
    let domain = email[at + 1..].trim();
    syntax.is_balanced()
        && is_address_local_part(local)
        && (is_address_dot_atom(domain)
            || (domain.len() > 2 && is_address_quoted(domain, '[', ']')))
}

/// RFC 5322 also admits dot-separated quoted words via obs-local-part.
fn is_address_local_part(value: &str) -> bool {
    let is_word = |word: &str| {
        let word = word.trim();
        is_address_dot_atom(word) || is_address_quoted(word, '"', '"')
    };
    let mut syntax = AddressSyntax::default();
    let mut start = 0;
    for (i, c) in value.char_indices() {
        if syntax.is_structural(c) && c == '.' {
            if !is_word(&value[start..i]) {
                return false;
            }
            start = i + 1;
        }
    }
    syntax.is_balanced() && is_word(&value[start..])
}

fn is_address_dot_atom(value: &str) -> bool {
    value.split('.').all(|atom| {
        let atom = atom.trim();
        !atom.is_empty()
            && atom.chars().all(|c| {
                c.is_ascii_alphanumeric()
                    || "!#$%&'*+-/=?^_`{|}~".contains(c)
                    || (!c.is_ascii() && !c.is_whitespace() && !c.is_control())
            })
    })
}

fn is_address_quoted(value: &str, open: char, close: char) -> bool {
    let Some(inner) = value.strip_prefix(open).and_then(|v| v.strip_suffix(close)) else {
        return false;
    };
    let mut escaped = false;
    for c in inner.chars() {
        if c.is_control() && c != '\t' {
            return false;
        }
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == open || c == close {
            return false;
        }
    }
    !escaped
}

/// Serialize one address header to the JSON array stored in
/// `messages.to_addresses` / `messages.cc_addresses`.
fn addresses_to_json(header: Option<&mailparse::MailHeader<'_>>) -> String {
    let list = header.map(header_addresses).unwrap_or_default();
    serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use imap::extensions::idle::WaitOutcome;

    #[test]
    fn tolerant_fetch_survives_a_still_working_keepalive() {
        // Proton Bridge sends an untagged `* OK Still working ...` line mid-FETCH
        // to keep the connection alive during a slow scan (RFC 3501 §7 permits
        // this at any time). `imap` 2.4.1's own parser treats it as a fatal
        // "unexpected response" and discards the whole command's results;
        // `parse_tolerant_fetches` must skip it and keep the FETCH data either
        // side of it.
        let raw = b"* 1 FETCH (UID 100 FLAGS (\\Seen))\r\n\
* OK Still working...\r\n\
* 2 FETCH (UID 101 FLAGS (\\Answered))\r\n\
a1 OK UID FETCH completed\r\n";
        let uids =
            super::parse_tolerant_fetches("UID FETCH 1:* (UID FLAGS)", raw, |uid, _attrs| {
                Some(uid)
            })
            .unwrap();
        assert_eq!(uids, vec![100, 101]);
    }

    #[test]
    fn tolerant_fetch_errors_on_truly_unparseable_bytes() {
        let raw = b"this is not an IMAP response\r\n";
        let result =
            super::parse_tolerant_fetches("UID FETCH 1:* UID", raw, |uid, _attrs| Some(uid));
        assert!(result.is_err());
    }

    #[test]
    fn idle_timeout_is_not_a_mailbox_notification() {
        assert!(!super::idle_outcome_has_notification(WaitOutcome::TimedOut));
        assert!(super::idle_outcome_has_notification(
            WaitOutcome::MailboxChanged
        ));
    }

    #[test]
    fn test_utf7_imap_decode() {
        let decoded = utf7_imap::decode_utf7_imap("Komih&AOU-g".to_string());
        assert_eq!(decoded, "Komihåg");
    }

    #[test]
    fn test_utf7_imap_roundtrip() {
        let original = "Komihåg";
        let encoded = utf7_imap::encode_utf7_imap(original.to_string());
        let decoded = utf7_imap::decode_utf7_imap(encoded);
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_utf7_imap_ascii_passthrough() {
        let decoded = utf7_imap::decode_utf7_imap("INBOX".to_string());
        assert_eq!(decoded, "INBOX");
    }

    #[test]
    fn parse_threading_headers_extracts_both() {
        let bytes = b"References: <root@h> <mid@h>\r\nIn-Reply-To: <mid@h>\r\n\r\n";
        let env = super::parse_envelope_headers(bytes).unwrap();
        assert_eq!(env.in_reply_to.as_deref(), Some("<mid@h>"));
        assert_eq!(
            env.references,
            vec!["<root@h>".to_string(), "<mid@h>".to_string()]
        );
    }

    #[test]
    fn parse_threading_headers_unfolds_continuations() {
        let bytes = b"References: <root@h>\r\n <mid@h>\r\n\r\n";
        let env = super::parse_envelope_headers(bytes).unwrap();
        assert_eq!(
            env.references,
            vec!["<root@h>".to_string(), "<mid@h>".to_string()]
        );
    }

    #[test]
    fn parse_threading_headers_handles_only_references() {
        let bytes = b"References: <root@h>\r\n\r\n";
        let env = super::parse_envelope_headers(bytes).unwrap();
        assert!(env.in_reply_to.is_none());
        assert_eq!(env.references, vec!["<root@h>".to_string()]);
    }

    #[test]
    fn parse_threading_headers_normalizes_whitespace() {
        // Server emits a leading space inside the bracketed id.
        let bytes = b"In-Reply-To:  < mid@h >\r\n\r\n";
        let env = super::parse_envelope_headers(bytes).unwrap();
        assert_eq!(env.in_reply_to.as_deref(), Some("<mid@h>"));
    }

    #[test]
    fn parse_threading_headers_empty_block() {
        let env = super::parse_envelope_headers(b"").unwrap();
        assert!(env.in_reply_to.is_none());
        assert!(env.references.is_empty());
        assert_eq!(env.to_addresses, "[]");
        assert_eq!(env.cc_addresses, "[]");
    }

    #[test]
    fn envelope_header_parse_errors_are_distinct_from_empty_blocks() {
        for bytes in [
            b" orphaned continuation\r\n".as_slice(),
            b"\r",
            b"Subject: valid prefix\r\n\rbroken",
        ] {
            assert!(mailparse::parse_headers(bytes).is_err());
            assert!(super::parse_envelope_headers(bytes).is_none());
        }
    }

    #[test]
    fn envelope_header_parsing_retains_mailparse_tolerance() {
        let bytes = b"Colonless line\r\nSubject: Retained\r\n\r\n";
        assert!(mailparse::parse_headers(bytes).is_ok());
        let env = super::parse_envelope_headers(bytes).unwrap();
        assert_eq!(env.subject.as_deref(), Some("Retained"));
    }
}

#[cfg(test)]
mod addr_edge_cases {
    use super::parse_envelope_headers;

    fn assert_address_headers(value: &str, expected: serde_json::Value) {
        let raw = format!("From: {value}\r\nTo: {value}\r\nCc: {value}\r\n\r\n");
        let env = parse_envelope_headers(raw.as_bytes()).unwrap();
        let expected_from = expected
            .as_array()
            .expect("expected address array")
            .first()
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"name": null, "email": null}));
        assert_eq!(
            serde_json::json!({"name": env.from_name, "email": env.from_email}),
            expected_from,
            "From: {value}"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&env.to_addresses).unwrap(),
            expected,
            "To: {value}"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&env.cc_addresses).unwrap(),
            expected,
            "Cc: {value}"
        );
    }

    #[test]
    fn a_bare_quoted_local_part_is_preserved() {
        assert_address_headers(
            r#""alice"@example.org"#,
            serde_json::json!([{"name": null, "email": "\"alice\"@example.org"}]),
        );
    }

    #[test]
    fn a_bare_quoted_local_part_is_not_swallowed_by_a_named_sibling() {
        assert_address_headers(
            r#""alice"@example.org, Bob <bob@example.org>"#,
            serde_json::json!([
                {"name": null, "email": "\"alice\"@example.org"},
                {"name": "Bob", "email": "bob@example.org"}
            ]),
        );
    }

    #[test]
    fn a_bare_quoted_local_part_survives_malformed_siblings() {
        assert_address_headers(
            r#"bogus, "alice"@example.org, missing@, bob@example.org"#,
            serde_json::json!([
                {"name": null, "email": "\"alice\"@example.org"},
                {"name": null, "email": "bob@example.org"}
            ]),
        );
    }

    #[test]
    fn group_members_use_the_same_quoted_mailbox_parser() {
        assert_address_headers(
            r#"friends: bogus, "alice"@example.org, Bob <bob@example.org>;, c@x.se"#,
            serde_json::json!([
                {"name": null, "email": "\"alice\"@example.org"},
                {"name": "Bob", "email": "bob@example.org"},
                {"name": null, "email": "c@x.se"}
            ]),
        );
    }

    #[test]
    fn empty_group_members_do_not_become_part_of_an_address() {
        assert_address_headers(
            "friends: a@x.se, , b@x.se;",
            serde_json::json!([
                {"name": null, "email": "a@x.se"},
                {"name": null, "email": "b@x.se"}
            ]),
        );
    }

    #[test]
    fn malformed_group_members_do_not_invalidate_healthy_members() {
        for value in [
            "friends: a@x.se, bogus], b@x.se;, c@x.se",
            "friends: a@x.se, b@x.se, bogus];, c@x.se",
        ] {
            assert_address_headers(
                value,
                serde_json::json!([
                    {"name": null, "email": "a@x.se"},
                    {"name": null, "email": "b@x.se"},
                    {"name": null, "email": "c@x.se"}
                ]),
            );
        }
    }

    #[test]
    fn quoted_empty_group_names_still_have_members() {
        for name in [r#""""#, r#"" ""#] {
            assert_address_headers(
                &format!("{name}: a@x.se, b@x.se;"),
                serde_json::json!([
                    {"name": null, "email": "a@x.se"},
                    {"name": null, "email": "b@x.se"}
                ]),
            );
        }
    }

    #[test]
    fn tolerated_encoded_word_punctuation_cannot_hide_sibling_recipients() {
        // These raw Q payloads are nonconforming in a phrase, but mailparse
        // accepts them. Their punctuation must not consume another recipient.
        for name in ["A[B", "A,B", "A:B", "A;B", "A<B", "A\"B", "A(B"] {
            for suffix in ["", " (note)"] {
                assert_address_headers(
                    &format!("=?UTF-8?Q?{name}?= <a@x.se>{suffix}, b@x.se"),
                    serde_json::json!([
                        {"name": name, "email": "a@x.se"},
                        {"name": null, "email": "b@x.se"}
                    ]),
                );
            }
        }
    }

    #[test]
    fn encoded_word_shapes_inside_addresses_remain_literal() {
        assert_address_headers(
            r#"A <"\=?UTF-8?Q?x?="@x.se>, b@x.se"#,
            serde_json::json!([
                {"name": "A", "email": "\"\\=?UTF-8?Q?x?=\"@x.se"},
                {"name": null, "email": "b@x.se"}
            ]),
        );
        assert_address_headers(
            "=?UTF-8?Q?a@x.se,b?=@x.se",
            serde_json::json!([
                {"name": null, "email": "=?UTF-8?Q?a@x.se"},
                {"name": null, "email": "b?=@x.se"}
            ]),
        );
    }

    #[test]
    fn empty_group_members_preserve_encoded_display_names() {
        assert_address_headers(
            "=?UTF-8?Q?Friends=3A_West?=: , a@x.se, , \
             =?UTF-8?Q?Doe=2C_John?= <john@x.se>, ;, b@x.se",
            serde_json::json!([
                {"name": null, "email": "a@x.se"},
                {"name": "Doe, John", "email": "john@x.se"},
                {"name": null, "email": "b@x.se"}
            ]),
        );
    }

    #[test]
    fn quoted_local_punctuation_and_escapes_keep_their_original_spelling() {
        for address in [
            r#""a,b;c"@example.org"#,
            r#""a>b"@example.org"#,
            r#""a@b"@example.org"#,
            r#""a\"b"@example.org"#,
            r#""a\\b"@example.org"#,
            r#""a\\"@example.org"#,
            r#""a\ b"@example.org"#,
            r#""a\";b\\c,>d@e"@example.org"#,
        ] {
            let expected = serde_json::json!([
                {"name": null, "email": address},
                {"name": "Bob", "email": "bob@example.org"}
            ]);
            assert_address_headers(
                &format!("{address}, Bob <bob@example.org>"),
                expected.clone(),
            );
            assert_address_headers(
                &format!("friends: {address}, Bob <bob@example.org>;"),
                expected,
            );
        }
    }

    #[test]
    fn quoted_angle_addresses_preserve_the_encoded_name_and_local_part() {
        assert_address_headers(
            r#"=?UTF-8?Q?Doe=2C_John?= <"a>b"@example.org>, "B, C" <"a\";b\\c,>d@e"@example.org>"#,
            serde_json::json!([
                {"name": "Doe, John", "email": "\"a>b\"@example.org"},
                {"name": "B, C", "email": "\"a\\\";b\\\\c,>d@e\"@example.org"}
            ]),
        );
    }

    #[test]
    fn ipv6_domain_literals_do_not_start_groups_or_hide_siblings() {
        assert_address_headers(
            r#"alice@[IPv6:2001:db8::1], "bob"@[IPv6:2001:db8::2], c@x.se"#,
            serde_json::json!([
                {"name": null, "email": "alice@[IPv6:2001:db8::1]"},
                {"name": null, "email": "\"bob\"@[IPv6:2001:db8::2]"},
                {"name": null, "email": "c@x.se"}
            ]),
        );
        assert_address_headers(
            "friends: alice@[IPv6:2001:db8::1], , b@x.se;, c@x.se",
            serde_json::json!([
                {"name": null, "email": "alice@[IPv6:2001:db8::1]"},
                {"name": null, "email": "b@x.se"},
                {"name": null, "email": "c@x.se"}
            ]),
        );
    }

    #[test]
    fn internationalized_addresses_and_display_names_are_preserved() {
        assert_address_headers(
            r#"用户@例子.公司, Jörg <jörg@bücher.example>, "雪\ 花"@例子.公司"#,
            serde_json::json!([
                {"name": null, "email": "用户@例子.公司"},
                {"name": "Jörg", "email": "jörg@bücher.example"},
                {"name": null, "email": "\"雪\\ 花\"@例子.公司"}
            ]),
        );
    }

    #[test]
    fn received_header_local_parts_allow_quoted_words_and_cfws() {
        assert_address_headers(
            r#"alice."b,c"@example.org, "snow(雪)"@例子.公司 (note), Bob <"bob"@example.org (note)>"#,
            serde_json::json!([
                {"name": null, "email": "alice.\"b,c\"@example.org"},
                {"name": null, "email": "\"snow(雪)\"@例子.公司"},
                {"name": "Bob", "email": "\"bob\"@example.org"}
            ]),
        );
    }

    #[test]
    fn raw_legacy_display_names_keep_mailparses_latin1_fallback() {
        let env = parse_envelope_headers(
            b"From: J\xf6rg <jorg@example.org>\r\n\
              To: J\xf6rg <jorg@example.org>\r\n\
              Cc: J\xf6rg <jorg@example.org>\r\n\r\n",
        )
        .unwrap();
        assert_eq!(env.from_name.as_deref(), Some("Jörg"));
        assert_eq!(env.from_email.as_deref(), Some("jorg@example.org"));
        let expected = r#"[{"name":"Jörg","email":"jorg@example.org"}]"#;
        assert_eq!(env.to_addresses, expected);
        assert_eq!(env.cc_addresses, expected);
    }

    #[test]
    fn folded_encoded_names_are_decoded_after_splitting() {
        assert_address_headers(
            "=?UTF-8?Q?Doe=2C?=\r\n =?UTF-8?Q?_John?=\r\n \
             <\"alice\"@example.org>,\r\n bob@example.org",
            serde_json::json!([
                {"name": "Doe, John", "email": "\"alice\"@example.org"},
                {"name": null, "email": "bob@example.org"}
            ]),
        );
    }

    #[test]
    fn comments_can_follow_angle_addresses_and_group_terminators() {
        assert_address_headers(
            "friends: Alice (Sales, West) <alice@example.org> \
             (outer (inner) still outer); (note: \\) still open), \
             bob@example.org (B, C)",
            serde_json::json!([
                {"name": "Alice", "email": "alice@example.org"},
                {"name": null, "email": "bob@example.org"}
            ]),
        );
        assert_address_headers("undisclosed-recipients:; (empty)", serde_json::json!([]));
    }

    #[test]
    fn malformed_address_items_are_not_salvaged_as_mailbox_substrings() {
        for value in [
            "@example.org",
            "alice@",
            "alice@@example.org",
            "alice bob@example.org",
            "alice(note)bob@example.org",
            "alice@example.org suffix",
            "Alice <alice@example.org> suffix",
            "alice@example.org <bob@example.org>",
            "Alice <<alice@example.org>>",
            "Alice <alice@example.org",
            "Alice <alice@example.org> (unterminated",
            r#""alice"junk@example.org"#,
            r#""alice@example.org"#,
            r#""alice"@example.org junk"#,
            "alice@[IPv6:2001:db8::1",
            "friends: alice@example.org, bob@example.org",
            "friends: alice@example.org; suffix",
            "friends: alice@example.org;;",
            "outer: alice@example.org, inner: bob@example.org;;",
            ": alice@example.org;",
            "mailto:alice@example.org",
        ] {
            assert_address_headers(value, serde_json::json!([]));
        }
    }

    #[test]
    fn invalid_terminated_groups_cost_only_their_own_list_item() {
        for value in [
            "friends: alice@example.org; suffix, bob@example.org",
            "outer: alice@example.org, inner: c@x.se;;, bob@example.org",
            "Alice <alice@example.org> suffix, bob@example.org",
        ] {
            assert_address_headers(
                value,
                serde_json::json!([{"name": null, "email": "bob@example.org"}]),
            );
        }
    }

    #[test]
    fn one_malformed_recipient_does_not_drop_the_rest() {
        let env = parse_envelope_headers(b"To: valid@x.se, bogus\r\n\r\n").unwrap();
        assert_eq!(env.to_addresses, r#"[{"name":null,"email":"valid@x.se"}]"#);
    }

    #[test]
    fn empty_list_elements_are_skipped() {
        let env = parse_envelope_headers(b"To: \"A\" <a@x.se>, , b@x.se\r\n\r\n").unwrap();
        assert_eq!(
            env.to_addresses,
            r#"[{"name":"A","email":"a@x.se"},{"name":null,"email":"b@x.se"}]"#
        );
    }

    #[test]
    fn a_comma_inside_a_quoted_display_name_is_not_a_separator() {
        let env = parse_envelope_headers(b"To: \"Delhage, Lars\" <lasse@nohup.se>, b@x.se\r\n\r\n")
            .unwrap();
        assert_eq!(
            env.to_addresses,
            r#"[{"name":"Delhage, Lars","email":"lasse@nohup.se"},{"name":null,"email":"b@x.se"}]"#
        );
    }

    #[test]
    fn a_group_stays_one_element() {
        let env = parse_envelope_headers(b"Cc: friends: a@x.se, \"B\" <b@x.se>;, c@x.se\r\n\r\n")
            .unwrap();
        assert_eq!(
            env.cc_addresses,
            r#"[{"name":null,"email":"a@x.se"},{"name":"B","email":"b@x.se"},{"name":null,"email":"c@x.se"}]"#
        );
    }

    #[test]
    fn a_malformed_member_costs_only_itself_inside_a_group() {
        let env = parse_envelope_headers(b"Cc: friends: a@x.se, bogus;, c@x.se\r\n\r\n").unwrap();
        assert_eq!(
            env.cc_addresses,
            r#"[{"name":null,"email":"a@x.se"},{"name":null,"email":"c@x.se"}]"#
        );
    }

    #[test]
    fn a_malformed_sibling_does_not_corrupt_an_encoded_display_name() {
        // The comma appears only after decoding; it is part of the name.
        let env = parse_envelope_headers(
            b"To: =?UTF-8?Q?Doe=2C_John?= <john@x.se>, bogus, jane@x.se\r\n\r\n",
        )
        .unwrap();
        assert_eq!(
            env.to_addresses,
            r#"[{"name":"Doe, John","email":"john@x.se"},{"name":null,"email":"jane@x.se"}]"#
        );
    }

    #[test]
    fn a_sender_without_a_routable_address_yields_none() {
        let env = parse_envelope_headers(b"From: root\r\n\r\n").unwrap();
        assert!(env.from_email.is_none());
    }

    #[test]
    fn a_comma_inside_a_parenthesized_comment_is_not_a_separator() {
        let env = parse_envelope_headers(
            b"To: John Doe (Sales, West) <john@example.com>, jane@example.com\r\n\r\n",
        )
        .unwrap();
        assert_eq!(
            env.to_addresses,
            r#"[{"name":"John Doe","email":"john@example.com"},{"name":null,"email":"jane@example.com"}]"#
        );
    }

    // The next two exercise `split_address_list` directly rather than through
    // `parse_envelope_headers`: `mailparse::addrparse` doesn't itself nest
    // comments or honor a quoted-pair escape inside one, so asserting a clean
    // display name past that point would pin mailparse's behavior, not
    // chithi's. What chithi controls is not splitting at the wrong comma.

    #[test]
    fn nested_comments_keep_the_address_as_one_item() {
        let items = super::split_address_list("A (outer (inner) still outer) <a@x.se>, b@x.se");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].trim(), "A (outer (inner) still outer) <a@x.se>");
        assert_eq!(items[1].trim(), "b@x.se");
    }

    #[test]
    fn an_escaped_paren_inside_a_comment_does_not_close_it_early() {
        // "\)" is a quoted-pair, a literal ")" character, not the comment's
        // real close -- which is the *next* ")".
        let items = super::split_address_list("A (note: \\) still open) <a@x.se>, b@x.se");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].trim(), "A (note: \\) still open) <a@x.se>");
        assert_eq!(items[1].trim(), "b@x.se");
    }

    #[test]
    fn an_encoded_word_that_decodes_to_a_comma_keeps_its_whole_display_name() {
        // Each isolated raw item is decoded and parsed together, keeping the
        // encoded comma inside the display-name token.
        let env =
            parse_envelope_headers(b"To: =?UTF-8?Q?Doe=2C_John?= <john@x.se>, jane@x.se\r\n\r\n")
                .unwrap();
        assert_eq!(
            env.to_addresses,
            r#"[{"name":"Doe, John","email":"john@x.se"},{"name":null,"email":"jane@x.se"}]"#
        );
    }
}
