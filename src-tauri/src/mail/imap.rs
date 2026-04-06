use imap::Session;
use native_tls::TlsStream;
use std::net::TcpStream;

use crate::error::{Error, Result};

pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub use_tls: bool,
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
        log::info!("IMAP connecting to {}:{}", config.host, config.port);

        let tls = native_tls::TlsConnector::builder()
            .build()
            .map_err(|e| {
                log::error!("TLS connector build failed: {}", e);
                Error::Imap(e.to_string())
            })?;

        let client = imap::connect((&*config.host, config.port), &config.host, &tls)
            .map_err(|e| {
                log::error!(
                    "IMAP connection failed to {}:{}: {}",
                    config.host,
                    config.port,
                    e
                );
                Error::Imap(e.to_string())
            })?;

        log::debug!("IMAP connected, authenticating as {}", config.username);

        let session = client
            .login(&config.username, &config.password)
            .map_err(|e| {
                log::error!("IMAP login failed for {}: {}", config.username, e.0);
                Error::Imap(e.0.to_string())
            })?;

        log::info!("IMAP authenticated as {}", config.username);
        Ok(Self { session })
    }

    pub fn list_folders(&mut self) -> Result<Vec<(String, String)>> {
        log::debug!("IMAP listing folders");
        let mailboxes = self
            .session
            .list(None, Some("*"))
            .map_err(|e| {
                log::error!("IMAP LIST failed: {}", e);
                Error::Imap(e.to_string())
            })?;

        let mut folders = Vec::new();
        for mb in mailboxes.iter() {
            let name = mb.name().to_string();
            let delimiter = mb.delimiter().unwrap_or("/");
            let display_name = name
                .rsplit_once(delimiter)
                .map(|(_, last)| last.to_string())
                .unwrap_or_else(|| name.clone());
            folders.push((display_name, name));
        }
        log::info!("IMAP found {} folders", folders.len());
        for (display, path) in &folders {
            log::debug!("  folder: {} ({})", display, path);
        }
        Ok(folders)
    }

    pub fn select_folder(&mut self, folder: &str) -> Result<(u32, u32)> {
        log::debug!("IMAP SELECT {}", folder);
        let mailbox = self
            .session
            .select(folder)
            .map_err(|e| {
                log::error!("IMAP SELECT {} failed: {}", folder, e);
                Error::Imap(e.to_string())
            })?;
        let exists = mailbox.exists;
        let uid_validity = mailbox.uid_validity.unwrap_or(0);
        log::debug!(
            "IMAP SELECT {}: {} messages, uidvalidity={}",
            folder,
            exists,
            uid_validity
        );
        Ok((exists, uid_validity))
    }

    /// Fetch UIDs in folder. If since_uid > 0, only fetch UIDs after it.
    pub fn fetch_uids(&mut self, since_uid: u32) -> Result<Vec<u32>> {
        let range = if since_uid > 0 {
            format!("{}:*", since_uid + 1)
        } else {
            "1:*".to_string()
        };
        log::debug!("IMAP UID FETCH {} (since_uid={})", range, since_uid);

        let messages = self
            .session
            .uid_fetch(&range, "UID")
            .map_err(|e| {
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
            .uid_fetch(&uid_set, "(UID ENVELOPE FLAGS RFC822.SIZE BODYSTRUCTURE)")
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
                    let subject = env
                        .subject
                        .as_ref()
                        .map(|s| decode_imap_str(s));

                    let (fname, femail) = env
                        .from
                        .as_ref()
                        .and_then(|addrs| addrs.first())
                        .map(|a| {
                            (
                                a.name.as_ref().map(|n| decode_imap_str(n)),
                                a.mailbox
                                    .as_ref()
                                    .map(|m| {
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
                    let mid = env.message_id.as_ref().map(|m| decode_imap_str(m));
                    let irt = env.in_reply_to.as_ref().map(|r| decode_imap_str(r));

                    (subject, fname, femail, to_list, cc_list, date, mid, irt)
                } else {
                    (None, None, None, "[]".to_string(), "[]".to_string(), None, None, None)
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
                flags,
                size,
                has_attachments,
            });
        }
        log::info!(
            "IMAP envelope batch: {} envelopes fetched",
            results.len()
        );
        Ok(results)
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
        self.session
            .uid_copy(&uid_set, dest_folder)
            .map_err(|e| {
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
        log::info!("IMAP move complete: {} messages moved to '{}'", uids.len(), dest_folder);

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

        self.session
            .uid_store(&uid_set, &store_cmd)
            .map_err(|e| {
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

        self.session
            .uid_copy(&uid_set, dest_folder)
            .map_err(|e| {
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
        log::info!("IMAP appending message ({} bytes) to folder '{}'", message.len(), folder);
        self.session
            .append_with_flags(folder, message, &[imap::types::Flag::Seen, imap::types::Flag::Draft])
            .map_err(|e| Error::Imap(format!("IMAP APPEND failed: {}", e)))?;
        log::info!("IMAP message appended to '{}'", folder);
        Ok(())
    }

    /// Enter IMAP IDLE on the currently selected folder.
    /// Blocks until the server sends a notification (new mail, expunge, etc.)
    /// or the timeout expires. Returns true if there was a server notification.
    pub fn idle_wait(&mut self, timeout: std::time::Duration) -> Result<bool> {
        log::debug!("IMAP entering IDLE (timeout={}s)", timeout.as_secs());
        let mut idle = self.session.idle().map_err(|e| Error::Imap(format!("IDLE setup failed: {}", e)))?;
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

    pub fn logout(mut self) {
        log::debug!("IMAP logging out");
        self.session.logout().ok();
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
