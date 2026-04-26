use imap::Session;
use native_tls::TlsStream;
use std::net::TcpStream;

use crate::error::{Error, Result};
use crate::mail::msgid::normalize_message_id;
use crate::mail::search::{build_imap_search, SearchHit, SearchQuery};

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

impl ImapConfig {
    /// Build an ImapConfig from an account. For O365 accounts, fetches an
    /// IMAP-scoped OAuth token and sets use_xoauth2=true.
    pub fn from_account(account: &crate::db::accounts::AccountFull) -> ImapConfig {
        ImapConfig {
            host: account.imap_host.clone(),
            port: account.imap_port,
            username: account.username.clone(),
            password: account.password.clone(),
            use_tls: account.use_tls,
            use_xoauth2: account.provider == "o365",
        }
    }
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

pub struct ImapConnection {
    session: Session<TlsStream<TcpStream>>,
}

impl ImapConnection {
    /// Connect and authenticate. Must be called from a blocking context.
    pub fn connect(config: &ImapConfig) -> Result<Self> {
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

        // Port 993 = implicit TLS (entire connection wrapped in TLS from start)
        // Port 143 = STARTTLS (connect plain, send STARTTLS command, upgrade to TLS)
        let client = if config.port == 993 {
            log::debug!("IMAP using implicit TLS");
            imap::connect((&*config.host, config.port), &config.host, &tls).map_err(|e| {
                log::error!(
                    "IMAP TLS connection failed to {}:{}: {}",
                    config.host,
                    config.port,
                    e
                );
                Error::Imap(e.to_string())
            })?
        } else {
            log::debug!("IMAP using STARTTLS");
            imap::connect_starttls((&*config.host, config.port), &config.host, &tls).map_err(
                |e| {
                    log::error!(
                        "IMAP STARTTLS failed for {}:{}: {}",
                        config.host,
                        config.port,
                        e
                    );
                    Error::Imap(e.to_string())
                },
            )?
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
        Ok(Self { session })
    }

    pub fn list_folders(&mut self) -> Result<Vec<(String, String)>> {
        log::debug!("IMAP listing folders");
        let mailboxes = self.session.list(None, Some("*")).map_err(|e| {
            log::error!("IMAP LIST failed: {}", e);
            Error::Imap(e.to_string())
        })?;

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

    /// SELECT a folder. Returns (exists, uid_validity, uid_next).
    pub fn select_folder(&mut self, folder: &str) -> Result<(u32, u32, u32)> {
        log::debug!("IMAP SELECT {}", folder);
        let mailbox = self.session.select(folder).map_err(|e| {
            log::error!("IMAP SELECT {} failed: {}", folder, e);
            Error::Imap(e.to_string())
        })?;
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

        let messages = self.session.uid_fetch(&range, "UID").map_err(|e| {
            log::error!("IMAP UID FETCH failed: {}", e);
            Error::Imap(e.to_string())
        })?;

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
    pub fn fetch_envelopes_batch(&mut self, uids: &[u32]) -> Result<Vec<EnvelopeData>> {
        if uids.is_empty() {
            return Ok(vec![]);
        }

        let uid_set: String = uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        log::debug!(
            "IMAP fetching {} envelopes (UIDs: {}...)",
            uids.len(),
            &uid_set[..uid_set.len().min(80)]
        );

        let fetches = self
            .session
            .uid_fetch(&uid_set, "(UID ENVELOPE FLAGS RFC822.SIZE)")
            .map_err(|e| {
                log::error!("IMAP FETCH envelopes failed: {}", e);
                Error::Imap(e.to_string())
            })?;

        let mut results = Vec::new();
        for fetch in fetches.iter() {
            let uid = match fetch.uid {
                Some(u) => u,
                None => continue,
            };
            let flags: Vec<String> = fetch.flags().iter().map(|f| flag_to_string(f)).collect();
            let size = fetch.size.unwrap_or(0) as u64;

            // Parse ENVELOPE
            let envelope = fetch.envelope();
            let (subject, from_name, from_email, to_json, cc_json, date_str, msg_id, in_reply_to) =
                if let Some(env) = envelope {
                    let subject = env.subject.as_ref().map(|s| decode_imap_str(s));

                    let (fname, femail) = env
                        .from
                        .as_ref()
                        .and_then(|addrs| addrs.first())
                        .map(|a| {
                            (
                                a.name.as_ref().map(|n| decode_imap_str(n)),
                                a.mailbox.as_ref().map(|m| {
                                    let mb = decode_imap_str(m);
                                    if let Some(host) = a.host.as_ref() {
                                        format!("{}@{}", mb, decode_imap_str(host))
                                    } else {
                                        mb
                                    }
                                }),
                            )
                        })
                        .unwrap_or((None, None));

                    let to_list = addresses_to_json(env.to.as_deref());
                    let cc_list = addresses_to_json(env.cc.as_deref());

                    let date = env.date.as_ref().map(|d| decode_imap_str(d));
                    // Some servers (notably Microsoft Exchange/M365) emit a
                    // leading space inside the envelope's MessageId/InReplyTo
                    // octet-string. Storing that verbatim breaks the exact-
                    // match `WHERE message_id = ?` lookup in `compute_thread_id`,
                    // so canonicalize at the seam.
                    let mid = env
                        .message_id
                        .as_ref()
                        .and_then(|m| normalize_message_id(&decode_imap_str(m)));
                    let irt = env
                        .in_reply_to
                        .as_ref()
                        .and_then(|r| normalize_message_id(&decode_imap_str(r)));

                    (subject, fname, femail, to_list, cc_list, date, mid, irt)
                } else {
                    (
                        None,
                        None,
                        None,
                        "[]".to_string(),
                        "[]".to_string(),
                        None,
                        None,
                        None,
                    )
                };

            // Check for attachments from BODYSTRUCTURE
            // Simple heuristic: if the response text mentions "attachment", it likely has one
            // More accurate: check if it's multipart/mixed (indicates attachments)
            let has_attachments = size > 10000; // rough heuristic; will improve later

            results.push(EnvelopeData {
                uid,
                subject,
                from_name,
                from_email,
                to_addresses: to_json,
                cc_addresses: cc_json,
                date: date_str,
                message_id: msg_id,
                in_reply_to,
                references: Vec::new(),
                flags,
                size,
                has_attachments,
            });
        }
        log::info!("IMAP envelope batch: {} envelopes fetched", results.len());

        // References travels in a second, header-only fetch. Combining it
        // with ENVELOPE in one FETCH triggers an imap-proto parse error on
        // some servers (the literal-string framing of the body fetch leaks
        // into the next command's response). A second pass is one extra
        // round-trip per batch but keeps the connection state clean.
        if !results.is_empty() {
            self.populate_references(&mut results, &uid_set);
        }

        Ok(results)
    }

    /// Best-effort: fetch the References and In-Reply-To headers for every
    /// envelope in `results` and write them back. References fills
    /// `env.references`. In-Reply-To is used to backfill `env.in_reply_to`
    /// only when the envelope itself didn't carry it (some servers return
    /// NIL even when the header is in the body). Failures here do not abort
    /// the sync.
    fn populate_references(&mut self, results: &mut [EnvelopeData], uid_set: &str) {
        let fetches = match self.session.uid_fetch(
            uid_set,
            "(UID BODY.PEEK[HEADER.FIELDS (REFERENCES IN-REPLY-TO)])",
        ) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("IMAP fetch References/In-Reply-To failed (skipping): {}", e);
                return;
            }
        };
        let mut refs_by_uid: std::collections::HashMap<u32, Vec<String>> =
            std::collections::HashMap::new();
        let mut irt_by_uid: std::collections::HashMap<u32, String> =
            std::collections::HashMap::new();
        for fetch in fetches.iter() {
            if let (Some(uid), Some(bytes)) = (fetch.uid, fetch.header()) {
                let (irt, refs) = parse_threading_headers(bytes);
                if !refs.is_empty() {
                    refs_by_uid.insert(uid, refs);
                }
                if let Some(irt) = irt {
                    irt_by_uid.insert(uid, irt);
                }
            }
        }
        for env in results.iter_mut() {
            if let Some(refs) = refs_by_uid.remove(&env.uid) {
                env.references = refs;
            }
            if env.in_reply_to.is_none() {
                if let Some(irt) = irt_by_uid.remove(&env.uid) {
                    env.in_reply_to = Some(irt);
                }
            }
        }
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

        let fetches = self.session.uid_fetch(&uid_set, "BODY[]").map_err(|e| {
            log::error!("IMAP batch FETCH bodies failed: {}", e);
            Error::Imap(e.to_string())
        })?;

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
        self.session.uid_copy(&uid_set, dest_folder).map_err(|e| {
            log::error!("IMAP UID COPY to '{}' failed: {}", dest_folder, e);
            Error::Imap(format!("COPY to '{}' failed: {}", dest_folder, e))
        })?;
        log::debug!("IMAP COPY to '{}' succeeded", dest_folder);

        // 2. Mark originals as deleted
        self.session
            .uid_store(&uid_set, "+FLAGS (\\Deleted)")
            .map_err(|e| {
                log::error!("IMAP UID STORE +FLAGS \\Deleted failed: {}", e);
                Error::Imap(format!("STORE +FLAGS \\Deleted failed: {}", e))
            })?;
        log::debug!("IMAP marked {} messages as \\Deleted", uids.len());

        // 3. Expunge to permanently remove
        self.session.expunge().map_err(|e| {
            log::error!("IMAP EXPUNGE failed: {}", e);
            Error::Imap(format!("EXPUNGE failed: {}", e))
        })?;
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
        self.session
            .uid_store(&uid_set, "+FLAGS (\\Deleted)")
            .map_err(|e| {
                log::error!("IMAP UID STORE +FLAGS \\Deleted failed: {}", e);
                Error::Imap(format!("STORE +FLAGS \\Deleted failed: {}", e))
            })?;
        log::debug!("IMAP marked {} messages as \\Deleted", uids.len());

        // Expunge
        self.session.expunge().map_err(|e| {
            log::error!("IMAP EXPUNGE failed: {}", e);
            Error::Imap(format!("EXPUNGE failed: {}", e))
        })?;
        log::info!("IMAP delete complete: {} messages expunged", uids.len());

        Ok(())
    }

    /// Set or unset flags on messages.
    ///
    /// If `add` is true, adds the flags (+FLAGS); otherwise removes them (-FLAGS).
    pub fn set_flags(&mut self, uids: &[u32], flags: &[&str], add: bool) -> Result<()> {
        if uids.is_empty() || flags.is_empty() {
            return Ok(());
        }

        let uid_set = uid_set_string(uids);
        let flags_str = flags.join(" ");
        let action = if add { "+FLAGS" } else { "-FLAGS" };
        let store_cmd = format!("{} ({})", action, flags_str);

        log::info!(
            "IMAP {} flags [{}] on {} messages (UIDs: {})",
            if add { "adding" } else { "removing" },
            flags_str,
            uids.len(),
            &uid_set[..uid_set.len().min(80)]
        );

        self.session.uid_store(&uid_set, &store_cmd).map_err(|e| {
            log::error!("IMAP UID STORE {} failed: {}", store_cmd, e);
            Error::Imap(format!("STORE {} failed: {}", store_cmd, e))
        })?;

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
        self.session
            .uid_store("1:*", "+FLAGS.SILENT (\\Seen)")
            .map_err(|e| Error::Imap(format!("STORE +FLAGS.SILENT \\Seen failed: {}", e)))?;
        Ok(())
    }

    /// Fetch current flags for all messages in the selected folder.
    /// Returns a map of UID → flags vec. Uses `1:*` to get everything.
    pub fn fetch_all_flags(&mut self) -> Result<Vec<(u32, Vec<String>)>> {
        let fetches = self.session.uid_fetch("1:*", "(UID FLAGS)").map_err(|e| {
            log::error!("IMAP UID FETCH FLAGS failed: {}", e);
            Error::Imap(format!("FETCH FLAGS failed: {}", e))
        })?;

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

        self.session.uid_copy(&uid_set, dest_folder).map_err(|e| {
            log::error!("IMAP UID COPY to '{}' failed: {}", dest_folder, e);
            Error::Imap(format!("COPY to '{}' failed: {}", dest_folder, e))
        })?;

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
        let result = idle.wait_with_timeout(timeout);
        let had_notification = result.is_ok();
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
        let uids = self.session.uid_search(query).map_err(|e| {
            log::error!("IMAP UID SEARCH failed: {}", e);
            Error::Imap(e.to_string())
        })?;
        Ok(uids.into_iter().collect())
    }

    pub fn logout(mut self) {
        log::debug!("IMAP logging out");
        self.session.logout().ok();
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
        let envelopes = match conn.fetch_envelopes_batch(recent_uids) {
            Ok(e) => e,
            Err(e) => {
                log::warn!("IMAP search: envelope fetch in {} failed: {}", path, e);
                continue;
            }
        };

        for env in envelopes {
            if hits.len() >= SEARCH_TOTAL_LIMIT {
                break;
            }
            hits.push(envelope_to_hit(account_id, &path, env));
        }
    }

    conn.logout();
    Ok(hits)
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

/// Split a `BODY.PEEK[HEADER.FIELDS (REFERENCES IN-REPLY-TO)]` block into
/// `(in_reply_to, references)`, applying RFC 5322 §2.2.3 unfolding so
/// folded continuation lines don't split a single id in half.
fn parse_threading_headers(bytes: &[u8]) -> (Option<String>, Vec<String>) {
    let raw = String::from_utf8_lossy(bytes);
    // Unfold: a CRLF followed by WSP is part of the same header value.
    let unfolded = raw
        .replace("\r\n ", " ")
        .replace("\r\n\t", " ")
        .replace("\n ", " ")
        .replace("\n\t", " ");

    let mut in_reply_to: Option<String> = None;
    let mut references: Vec<String> = Vec::new();
    for line in unfolded.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name_lc = name.trim().to_ascii_lowercase();
        if name_lc == "references" {
            references = extract_msgids(value);
        } else if name_lc == "in-reply-to" && in_reply_to.is_none() {
            in_reply_to = extract_msgids(value).into_iter().next();
        }
    }
    (in_reply_to, references)
}

/// Decode a potentially MIME-encoded IMAP string to a Rust String.
/// Handles =?charset?encoding?text?= encoded words (RFC 2047).
fn decode_imap_str(s: &[u8]) -> String {
    let raw = String::from_utf8_lossy(s);
    if raw.contains("=?") {
        // Use mailparse to decode by wrapping in a fake header
        let fake = format!("Subject: {}\r\n", raw);
        match mailparse::parse_header(fake.as_bytes()) {
            Ok((header, _)) => header.get_value(),
            Err(_) => raw.to_string(),
        }
    } else {
        raw.to_string()
    }
}

#[derive(serde::Serialize)]
struct AddrJson {
    name: Option<String>,
    email: String,
}

/// Convert IMAP address list to JSON string.
fn addresses_to_json(addrs: Option<&[imap_proto::types::Address<'_>]>) -> String {
    let list: Vec<AddrJson> = addrs
        .unwrap_or(&[])
        .iter()
        .map(|a| {
            let email = match (a.mailbox.as_ref(), a.host.as_ref()) {
                (Some(mb), Some(host)) => {
                    format!("{}@{}", decode_imap_str(mb), decode_imap_str(host))
                }
                (Some(mb), None) => decode_imap_str(mb),
                _ => String::new(),
            };
            AddrJson {
                name: a.name.as_ref().map(|n| decode_imap_str(n)),
                email,
            }
        })
        .collect();
    serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
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
        let (irt, refs) = super::parse_threading_headers(bytes);
        assert_eq!(irt.as_deref(), Some("<mid@h>"));
        assert_eq!(refs, vec!["<root@h>".to_string(), "<mid@h>".to_string()]);
    }

    #[test]
    fn parse_threading_headers_unfolds_continuations() {
        let bytes = b"References: <root@h>\r\n <mid@h>\r\n\r\n";
        let (_, refs) = super::parse_threading_headers(bytes);
        assert_eq!(refs, vec!["<root@h>".to_string(), "<mid@h>".to_string()]);
    }

    #[test]
    fn parse_threading_headers_handles_only_references() {
        let bytes = b"References: <root@h>\r\n\r\n";
        let (irt, refs) = super::parse_threading_headers(bytes);
        assert!(irt.is_none());
        assert_eq!(refs, vec!["<root@h>".to_string()]);
    }

    #[test]
    fn parse_threading_headers_normalizes_whitespace() {
        // Server emits a leading space inside the bracketed id.
        let bytes = b"In-Reply-To:  < mid@h >\r\n\r\n";
        let (irt, _) = super::parse_threading_headers(bytes);
        assert_eq!(irt.as_deref(), Some("<mid@h>"));
    }

    #[test]
    fn parse_threading_headers_empty_block() {
        let (irt, refs) = super::parse_threading_headers(b"");
        assert!(irt.is_none());
        assert!(refs.is_empty());
    }
}
