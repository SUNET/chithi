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

/// Outcome of [`ImapConnection::fetch_envelopes_batch`]. UIDs whose chunk the
/// server or the parser rejected land in `failed_uids` instead of being
/// silently dropped, so the caller can leave them outside its sync watermark
/// and retry them later.
#[derive(Default)]
pub struct EnvelopeBatch {
    pub envelopes: Vec<EnvelopeData>,
    pub failed_uids: Vec<u32>,
}

pub struct ImapConnection {
    session: Session<TlsStream<TcpStream>>,
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

/// How much of a rejected server response to put on one log line.
const IMAP_PARSE_LOG_LIMIT: usize = 2048;

/// Log the payload that `imap`'s parser rejected.
///
/// `imap::error::ParseError` carries the offending bytes, but its `Display`
/// impl renders whole variants as one fixed string — every `ParseError::Invalid`
/// prints "Unable to parse status response" regardless of content. Since
/// `Error::Imap` keeps only `e.to_string()`, that payload is otherwise
/// unrecoverable, and the log says nothing about what the server actually sent.
///
/// The payload is a slice of a live server response, so it can carry message
/// subjects and addresses. That is the point — without it a parser failure is
/// unattributable — but it does mean `chithi.log` holds mail data in the clear,
/// so keep it capped and never log an authentication challenge.
fn log_imap_parse_payload(context: &str, e: &imap::Error) {
    use imap::error::ParseError;

    let imap::Error::Parse(parse_err) = e else {
        return;
    };
    match parse_err {
        ParseError::Invalid(bytes) => log::error!(
            "{}: parser rejected {} bytes of server response: \"{}\"",
            context,
            bytes.len(),
            escape_for_log(bytes),
        ),
        ParseError::DataNotUtf8(bytes, utf8_err) => log::error!(
            "{}: server sent {} bytes of non-UTF-8 data ({}): \"{}\"",
            context,
            bytes.len(),
            utf8_err,
            escape_for_log(bytes),
        ),
        ParseError::Unexpected(text) => {
            log::error!(
                "{}: unexpected response: \"{}\"",
                context,
                text.escape_debug()
            )
        }
        // Authentication challenges can carry credentials — never log them.
        ParseError::Authentication(_, _) => {}
    }
}

/// Escape a raw server response so it survives as a single readable log line,
/// capped at [`IMAP_PARSE_LOG_LIMIT`]. Slicing can split a multi-byte
/// character; the lossy conversion renders the fragment as U+FFFD, which is
/// fine for diagnostics.
fn escape_for_log(bytes: &[u8]) -> String {
    let head = &bytes[..bytes.len().min(IMAP_PARSE_LOG_LIMIT)];
    let mut out: String = String::from_utf8_lossy(head)
        .chars()
        .flat_map(|c| c.escape_debug())
        .collect();
    if bytes.len() > head.len() {
        out.push_str("…[truncated]");
    }
    out
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

    /// True once a failure has left unread bytes in the socket. Nothing read
    /// from this connection afterwards can be trusted; callers holding one
    /// across several folders must drop it and reconnect.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Log a failed IMAP command and mark the connection unusable if the
    /// failure desynchronized the stream.
    fn note_error(&mut self, context: &str, e: &imap::Error) {
        log_imap_parse_payload(context, e);
        if !self.poisoned && leaves_stream_desynchronized(e) {
            self.poisoned = true;
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

    /// Fetch UIDs in folder. If since_uid > 0, only fetch UIDs after it.
    pub fn fetch_uids(&mut self, since_uid: u32) -> Result<Vec<u32>> {
        let range = if since_uid > 0 {
            format!("{}:*", since_uid + 1)
        } else {
            "1:*".to_string()
        };
        log::debug!("IMAP UID FETCH {} (since_uid={})", range, since_uid);

        let fetched = self.session.uid_fetch(&range, "UID");
        let messages = self.checked(&format!("IMAP UID FETCH {} UID", range), fetched)?;

        let uids: Vec<u32> = messages
            .iter()
            .filter_map(|f| f.uid)
            .filter(|&uid| uid > since_uid)
            .collect();

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
    pub fn fetch_envelopes_batch(&mut self, uids: &[u32]) -> Result<EnvelopeBatch> {
        let mut batch = EnvelopeBatch::default();
        if uids.is_empty() {
            return Ok(batch);
        }

        log::debug!("IMAP fetching {} envelopes", uids.len());

        for chunk in uids.chunks(IMAP_FETCH_UID_CHUNK_SIZE) {
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

            let fetched = self.session.uid_fetch(&uid_set, ENVELOPE_FETCH_SPEC);
            let fetches = match fetched {
                Ok(f) => f,
                Err(e) => {
                    let context = format!("IMAP UID FETCH {} {}", uid_set, ENVELOPE_FETCH_SPEC);
                    self.note_error(&context, &e);
                    log::warn!(
                        "IMAP FETCH envelopes failed for {} UIDs (skipping chunk): {}",
                        chunk.len(),
                        e
                    );
                    batch.failed_uids.extend_from_slice(chunk);
                    continue;
                }
            };

            for fetch in fetches.iter() {
                let Some(uid) = fetch.uid else { continue };
                let flags: Vec<String> = fetch.flags().iter().map(|f| flag_to_string(f)).collect();
                let size = fetch.size.unwrap_or(0) as u64;
                let header = parse_envelope_headers(fetch.header().unwrap_or_default());

                // Check for attachments from BODYSTRUCTURE
                // Simple heuristic: if the response text mentions "attachment", it likely has one
                // More accurate: check if it's multipart/mixed (indicates attachments)
                let has_attachments = size > 10000; // rough heuristic; will improve later

                batch.envelopes.push(EnvelopeData {
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
                    flags,
                    size,
                    has_attachments,
                });
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

        let fetches = self
            .session
            .uid_fetch(uid.to_string(), "BODY[]")
            .map_err(|e| {
                log::error!("IMAP FETCH body for UID {} failed: {}", uid, e);
                Error::Imap(e.to_string())
            })?;

        if let Some(msg) = fetches.iter().next() {
            if let Some(body) = msg.body() {
                log::debug!("IMAP fetched body for UID {}: {} bytes", uid, body.len());
                return Ok(Some(body.to_vec()));
            }
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

        let fetched = self.session.uid_fetch(&uid_set, "BODY[]");
        let fetches = self.checked(&format!("IMAP UID FETCH {} BODY[]", uid_set), fetched)?;

        let mut results = std::collections::HashMap::new();
        for msg in fetches.iter() {
            if let (Some(uid), Some(body)) = (msg.uid, msg.body()) {
                results.insert(uid, body.to_vec());
            }
        }

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
        let fetched = self.session.uid_fetch("1:*", "(UID FLAGS)");
        let fetches = self.checked("IMAP UID FETCH 1:* (UID FLAGS)", fetched)?;

        let mut results = Vec::new();
        for fetch in fetches.iter() {
            let uid = match fetch.uid {
                Some(u) => u,
                None => continue,
            };
            let flags: Vec<String> = fetch.flags().iter().map(|f| flag_to_string(f)).collect();
            results.push((uid, flags));
        }
        Ok(results)
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
        log::debug!("IMAP logging out");
        self.session.logout().ok();
        if let Some(control) = &self.idle_control {
            control.clear_socket();
        }
    }
}

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
        );

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
        );

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

/// Parse a `BODY.PEEK[HEADER.FIELDS (...)]` block into the fields chithi
/// stores. `mailparse::parse_headers` applies RFC 5322 §2.2.3 unfolding and
/// RFC 2047 decoding, so folded continuation lines don't split a message id in
/// half and encoded-words arrive already decoded.
fn parse_envelope_headers(bytes: &[u8]) -> HeaderEnvelope {
    let Ok((headers, _)) = mailparse::parse_headers(bytes) else {
        return HeaderEnvelope {
            to_addresses: "[]".to_string(),
            cc_addresses: "[]".to_string(),
            ..Default::default()
        };
    };

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
    env
}

#[derive(serde::Serialize)]
struct AddrJson {
    name: Option<String>,
    email: String,
}

/// Split an address header value on the commas that actually separate
/// mailboxes — not those inside a quoted display name (`"Delhage, Lars"`), an
/// angle-bracketed address, an RFC 5322 group (`friends: a@x, b@x;`), or a
/// parenthesized comment (`John Doe (Sales, West) <john@x.se>`). RFC 5322
/// §3.2.2 comments nest and carry their own quoted-pair escapes, distinct from
/// a quoted string's.
fn split_address_list(value: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let (mut start, mut quoted, mut escaped, mut angle, mut group) =
        (0, false, false, false, false);
    let mut comment_depth: u32 = 0;
    for (i, c) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted || comment_depth > 0 => escaped = true,
            '"' if comment_depth == 0 => quoted = !quoted,
            '(' if !quoted => comment_depth += 1,
            ')' if !quoted && comment_depth > 0 => comment_depth -= 1,
            '<' if !quoted && comment_depth == 0 => angle = true,
            '>' if !quoted && comment_depth == 0 => angle = false,
            ':' if !quoted && !angle && comment_depth == 0 => group = true,
            ';' if !quoted && !angle && comment_depth == 0 => group = false,
            ',' if !quoted && !angle && !group && comment_depth == 0 => {
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
    // The whole-header parser applies RFC 2047 decoding and RFC 5322 syntax
    // (quoting, comments, groups) together, correctly -- unlike splitting on
    // `get_value()` first, it can't mistake a comma a decoded encoded-word
    // legally contains (`=?utf-8?q?Doe=2C_John?=` decodes to `Doe, John`) for
    // a separator. It fails outright on one malformed element (missing `@`),
    // which the fallback below handles by parsing one address at a time so a
    // bad element costs only itself -- but a stray empty element between two
    // commas doesn't make it fail, it makes it fold the empty element into
    // its neighbor's address (e.g. ", b@x.se"), so a plain `is_ok()` check
    // isn't enough; the addresses it produced need to look sane too.
    if let Ok(parsed) = mailparse::addrparse_header(header) {
        let addrs = flatten_addrs(&parsed);
        if addrs.iter().all(|a| is_clean_email(&a.email)) {
            return addrs;
        }
    }

    let mut out = Vec::new();
    for item in split_address_list(&header.get_value()) {
        if item.trim().is_empty() {
            continue;
        }
        let Ok(parsed) = mailparse::addrparse(item) else {
            continue;
        };
        out.extend(flatten_addrs(&parsed));
    }
    out
}

/// Whether `email` looks like a real address rather than something
/// `addrparse_header` swallowed a stray list element into.
fn is_clean_email(email: &str) -> bool {
    !email.is_empty() && email == email.trim() && !email.contains(',') && !email.contains(';')
}

fn flatten_addrs(list: &mailparse::MailAddrList) -> Vec<AddrJson> {
    use mailparse::MailAddr;

    let mut out = Vec::new();
    for addr in list.iter() {
        match addr {
            MailAddr::Single(s) => out.push(AddrJson {
                name: s.display_name.clone(),
                email: s.addr.clone(),
            }),
            MailAddr::Group(g) => out.extend(g.addrs.iter().map(|s| AddrJson {
                name: s.display_name.clone(),
                email: s.addr.clone(),
            })),
        }
    }
    out
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
        let env = super::parse_envelope_headers(bytes);
        assert_eq!(env.in_reply_to.as_deref(), Some("<mid@h>"));
        assert_eq!(
            env.references,
            vec!["<root@h>".to_string(), "<mid@h>".to_string()]
        );
    }

    #[test]
    fn parse_threading_headers_unfolds_continuations() {
        let bytes = b"References: <root@h>\r\n <mid@h>\r\n\r\n";
        let env = super::parse_envelope_headers(bytes);
        assert_eq!(
            env.references,
            vec!["<root@h>".to_string(), "<mid@h>".to_string()]
        );
    }

    #[test]
    fn parse_threading_headers_handles_only_references() {
        let bytes = b"References: <root@h>\r\n\r\n";
        let env = super::parse_envelope_headers(bytes);
        assert!(env.in_reply_to.is_none());
        assert_eq!(env.references, vec!["<root@h>".to_string()]);
    }

    #[test]
    fn parse_threading_headers_normalizes_whitespace() {
        // Server emits a leading space inside the bracketed id.
        let bytes = b"In-Reply-To:  < mid@h >\r\n\r\n";
        let env = super::parse_envelope_headers(bytes);
        assert_eq!(env.in_reply_to.as_deref(), Some("<mid@h>"));
    }

    #[test]
    fn parse_threading_headers_empty_block() {
        let env = super::parse_envelope_headers(b"");
        assert!(env.in_reply_to.is_none());
        assert!(env.references.is_empty());
        assert_eq!(env.to_addresses, "[]");
        assert_eq!(env.cc_addresses, "[]");
    }
}

#[cfg(test)]
mod addr_edge_cases {
    use super::parse_envelope_headers;

    #[test]
    fn one_malformed_recipient_does_not_drop_the_rest() {
        let env = parse_envelope_headers(b"To: valid@x.se, bogus\r\n\r\n");
        assert_eq!(env.to_addresses, r#"[{"name":null,"email":"valid@x.se"}]"#);
    }

    #[test]
    fn empty_list_elements_are_skipped() {
        let env = parse_envelope_headers(b"To: \"A\" <a@x.se>, , b@x.se\r\n\r\n");
        assert_eq!(
            env.to_addresses,
            r#"[{"name":"A","email":"a@x.se"},{"name":null,"email":"b@x.se"}]"#
        );
    }

    #[test]
    fn a_comma_inside_a_quoted_display_name_is_not_a_separator() {
        let env = parse_envelope_headers(b"To: \"Delhage, Lars\" <lasse@nohup.se>, b@x.se\r\n\r\n");
        assert_eq!(
            env.to_addresses,
            r#"[{"name":"Delhage, Lars","email":"lasse@nohup.se"},{"name":null,"email":"b@x.se"}]"#
        );
    }

    #[test]
    fn a_group_stays_one_element() {
        let env = parse_envelope_headers(b"Cc: friends: a@x.se, \"B\" <b@x.se>;, c@x.se\r\n\r\n");
        assert_eq!(
            env.cc_addresses,
            r#"[{"name":null,"email":"a@x.se"},{"name":"B","email":"b@x.se"},{"name":null,"email":"c@x.se"}]"#
        );
    }

    #[test]
    fn a_sender_without_a_routable_address_yields_none() {
        let env = parse_envelope_headers(b"From: root\r\n\r\n");
        assert!(env.from_email.is_none());
    }

    #[test]
    fn a_comma_inside_a_parenthesized_comment_is_not_a_separator() {
        let env = parse_envelope_headers(
            b"To: John Doe (Sales, West) <john@example.com>, jane@example.com\r\n\r\n",
        );
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
        // Decoded, "=?UTF-8?Q?Doe=2C_John?=" is "Doe, John" -- an unquoted
        // comma that only exists after RFC 2047 decoding. Splitting on the
        // already-decoded value would mistake it for a separator and
        // truncate the name to "John"; the whole-header parser decodes and
        // parses together, so it isn't fooled.
        let env =
            parse_envelope_headers(b"To: =?UTF-8?Q?Doe=2C_John?= <john@x.se>, jane@x.se\r\n\r\n");
        assert_eq!(
            env.to_addresses,
            r#"[{"name":"Doe, John","email":"john@x.se"},{"name":null,"email":"jane@x.se"}]"#
        );
    }
}
