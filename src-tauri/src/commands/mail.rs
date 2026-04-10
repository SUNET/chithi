use tauri::State;

use crate::db;
use crate::db::messages::{MessageSummary, ThreadedPage};
use crate::error::{Error, Result};
use crate::mail::imap::ImapConfig;
use crate::mail::jmap::JmapConfig;
use crate::mail::jmap_sync;
use crate::mail::parser;
use crate::mail::sync as mail_sync;
use crate::state::AppState;

/// Check if an IP address is in a private/reserved range (SSRF protection).
fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()            // 127.0.0.0/8
            || v4.is_private()          // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
            || v4.is_link_local()       // 169.254.0.0/16
            || v4.is_broadcast()        // 255.255.255.255
            || v4.is_unspecified()      // 0.0.0.0
            || v4.octets()[0] == 100 && v4.octets()[1] >= 64 && v4.octets()[1] <= 127 // 100.64.0.0/10 (CGNAT)
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()            // ::1
            || v6.is_unspecified()      // ::
            // ULA (fc00::/7) and link-local (fe80::/10)
            || v6.segments()[0] & 0xfe00 == 0xfc00
            || v6.segments()[0] & 0xffc0 == 0xfe80
        }
    }
}

#[tauri::command]
pub async fn list_folders(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Vec<db::folders::Folder>> {
    log::debug!("Listing folders for account {}", account_id);
    let conn = state.db.lock().await;
    let flat_folders = db::folders::list_folders(&conn, &account_id)?;
    log::debug!("Found {} folders for account {}", flat_folders.len(), account_id);
    let tree = db::folders::build_folder_tree(flat_folders);
    Ok(tree)
}

#[tauri::command]
pub async fn get_messages(
    state: State<'_, AppState>,
    account_id: String,
    folder_path: String,
    page: u32,
    per_page: u32,
    sort_column: Option<String>,
    sort_asc: Option<bool>,
    filter: Option<db::messages::QuickFilter>,
) -> Result<db::messages::MessagePage> {
    let col = sort_column.as_deref().unwrap_or("date");
    let asc = sort_asc.unwrap_or(false);
    let qf = filter.unwrap_or_default();
    log::debug!(
        "Getting messages: account={} folder={} page={} per_page={} sort={}:{}",
        account_id,
        folder_path,
        page,
        per_page,
        col,
        if asc { "asc" } else { "desc" }
    );
    let conn = state.db.lock().await;
    let result =
        db::messages::get_messages(&conn, &account_id, &folder_path, page, per_page, col, asc, &qf)?;
    log::debug!(
        "Returned {} messages (total={}) for folder {}",
        result.messages.len(),
        result.total,
        folder_path
    );
    Ok(result)
}

#[tauri::command]
pub async fn get_message_body(
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
) -> Result<db::messages::MessageBody> {
    log::debug!("Loading message body: {}", message_id);

    let (maildir_path, from_email, to_json, cc_json, flags_json, is_encrypted, is_signed) = {
        let conn = state.db.lock().await;
        db::messages::get_message_metadata(&conn, &account_id, &message_id)?
    };

    // Graph API messages: fetch body directly from Graph, not from disk
    if let Some(graph_msg_id) = maildir_path.strip_prefix("graph:") {
        log::debug!("Fetching Graph message body for {}", graph_msg_id);
        let token = crate::mail::graph::get_graph_token(&account_id).await?;
        let client = crate::mail::graph::GraphClient::new(&token);
        let body = client.get_message_body(graph_msg_id).await?;

        let flags: Vec<String> = serde_json::from_str(&flags_json).unwrap_or_default();
        let (body_html, body_text) = if body.content_type == "html" {
            let sanitized = ammonia::clean(&body.content);
            // Simple HTML-to-text: strip tags for plain text view
            let text = body.content
                .replace("<br>", "\n").replace("<br/>", "\n").replace("<br />", "\n")
                .replace("</p>", "\n").replace("</div>", "\n");
            let text = regex::Regex::new(r"<[^>]+>").unwrap().replace_all(&text, "").to_string();
            (Some(sanitized), text)
        } else {
            (None, body.content)
        };

        // Mark as read on the server if not already
        if !flags.contains(&"seen".to_string()) {
            let graph_ids = vec![graph_msg_id.to_string()];
            client.set_read_status(&graph_ids, true).await.ok();
            let conn = state.db.lock().await;
            let mut new_flags = flags.clone();
            new_flags.push("seen".to_string());
            db::messages::update_flags(&conn, &message_id, &serde_json::to_string(&new_flags).unwrap_or_default())?;
        }

        let to: Vec<db::messages::Address> = serde_json::from_str(&to_json).unwrap_or_default();
        let cc: Vec<db::messages::Address> = serde_json::from_str(&cc_json).unwrap_or_default();

        return Ok(db::messages::MessageBody {
            id: message_id,
            subject: None,
            from: db::messages::Address { name: None, email: from_email },
            to,
            cc,
            date: String::new(),
            flags,
            body_html,
            body_text: Some(body_text),
            attachments: vec![],
            is_encrypted,
            is_signed,
            list_id: None,
        });
    }

    // If body hasn't been downloaded yet, fetch it on-demand
    let actual_maildir_path = if maildir_path.is_empty() {
        // Get account config and message details
        let (account, folder_path, uid) = {
            let conn = state.db.lock().await;
            let account = db::accounts::get_account_full(&conn, &account_id)?;
            let (fp, u) = db::messages::get_folder_and_uid(&conn, &message_id)?;
            (account, fp, u)
        };

        let flags: Vec<String> = serde_json::from_str(&flags_json).unwrap_or_default();
        let data_dir = state.data_dir.clone();

        let relative_path = if account.mail_protocol == "jmap" {
            log::info!("Body not on disk for {}, fetching from JMAP", message_id);

            let jmap_config = JmapConfig {
                jmap_url: account.jmap_url.clone(),
                email: account.email.clone(),
                username: account.username.clone(),
                password: account.password.clone(),
            };

            // Extract the JMAP email ID from our composite message ID
            // Format: {account_id}_{folder_path}_{jmap_email_id}
            let jmap_email_id = message_id
                .strip_prefix(&format!("{}_{}_", account_id, folder_path))
                .unwrap_or(&message_id);

            jmap_sync::fetch_and_store_jmap_body(
                &jmap_config,
                &data_dir,
                &account_id,
                &folder_path,
                jmap_email_id,
                &flags,
            )
            .await?
        } else {
            log::info!("Body not on disk for {}, fetching from IMAP", message_id);

            let imap_config = ImapConfig {
                host: account.imap_host,
                port: account.imap_port,
                username: account.username,
                password: account.password,
                use_tls: account.use_tls,
            use_xoauth2: false,
            };

            let account_id_clone = account_id.clone();
            tokio::task::spawn_blocking(move || {
                mail_sync::fetch_and_store_body(
                    &imap_config,
                    &data_dir,
                    &account_id_clone,
                    &folder_path,
                    uid,
                    &flags,
                )
            })
            .await
            .map_err(|e| Error::Other(format!("Body fetch panicked: {}", e)))??
        };

        // Update the maildir_path in the database
        {
            let conn = state.db.lock().await;
            db::messages::update_maildir_path(&conn, &message_id, &relative_path)?;
        }

        relative_path
    } else {
        maildir_path
    };

    // Read and parse the message from disk
    let full_path = state.data_dir.join(&actual_maildir_path);
    log::debug!("Reading message from {}", full_path.display());
    let raw = std::fs::read(&full_path).map_err(|e| {
        log::error!(
            "Failed to read message file {}: {}",
            full_path.display(),
            e
        );
        Error::Other(format!(
            "Failed to read message file {}: {}",
            full_path.display(),
            e
        ))
    })?;

    parser::parse_message_body(
        &message_id,
        &raw,
        &from_email,
        &to_json,
        &cc_json,
        &flags_json,
        is_encrypted,
        is_signed,
    )
    .ok_or_else(|| {
        log::error!("Failed to parse message body for {}", message_id);
        Error::MailParse("Failed to parse message".to_string())
    })
}

/// Re-parse the message body allowing <img> tags, then download each image
/// and embed as base64 data URIs so the sandboxed iframe needs no network access.
/// Returns just the HTML string. Per-message, not persisted.
#[tauri::command]
pub async fn get_message_html_with_images(
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
) -> Result<String> {
    let maildir_path = {
        let conn = state.db.lock().await;
        let (mp, _, _, _, _, _, _) =
            db::messages::get_message_metadata(&conn, &account_id, &message_id)?;
        mp
    };

    if maildir_path.is_empty() || maildir_path.starts_with("graph:") {
        return Err(Error::Other(
            "Remote images not supported for messages without local body".to_string(),
        ));
    }

    let full_path = state.data_dir.join(&maildir_path);
    let raw = std::fs::read(&full_path).map_err(|e| {
        Error::Other(format!("Failed to read message file: {}", e))
    })?;

    let html = parser::parse_html_with_images(&raw).ok_or_else(|| {
        Error::MailParse("Failed to parse message HTML".to_string())
    })?;

    // Find all img src URLs and download them, replacing with data URIs.
    // This keeps the iframe sandbox at allow-scripts only (no allow-same-origin).
    let re = regex::Regex::new(r#"src="(https://[^"]+)""#)
        .map_err(|e| Error::Other(format!("Regex error: {}", e)))?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| Error::Other(format!("HTTP client error: {}", e)))?;

    // Collect all unique URLs
    let urls: Vec<String> = re
        .captures_iter(&html)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    // Download images in parallel (max 20 to avoid abuse)
    use base64::Engine;
    let mut url_to_data: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let futures: Vec<_> = urls.iter().take(20).map(|url| {
        let client = client.clone();
        let url = url.clone();
        async move {
            // SSRF protection: resolve hostname and reject private/internal IPs
            if let Ok(parsed) = reqwest::Url::parse(&url) {
                if let Some(host) = parsed.host_str() {
                    // Block obvious private hostnames
                    let h = host.to_lowercase();
                    if h == "localhost" || h.ends_with(".local") || h.ends_with(".internal") {
                        log::debug!("Image proxy: blocked private host {}", host);
                        return None;
                    }
                    // Resolve DNS and check for private IPs
                    if let Ok(addrs) = tokio::net::lookup_host(format!("{}:{}", host, parsed.port_or_known_default().unwrap_or(443))).await {
                        for addr in addrs {
                            let ip = addr.ip();
                            if ip.is_loopback() || ip.is_unspecified() || is_private_ip(&ip) {
                                log::debug!("Image proxy: blocked private IP {} for {}", ip, host);
                                return None;
                            }
                        }
                    }
                }
            }

            let resp = client.get(&url).send().await.ok()?;
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("image/png")
                .to_string();
            // Only allow image content types, max 5MB
            if !content_type.starts_with("image/") {
                return None;
            }
            let bytes = resp.bytes().await.ok()?;
            if bytes.len() > 5 * 1024 * 1024 {
                return None;
            }
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Some((url, format!("data:{};base64,{}", content_type, b64)))
        }
    }).collect();

    let results = futures::future::join_all(futures).await;
    for result in results.into_iter().flatten() {
        url_to_data.insert(result.0, result.1);
    }

    // Replace URLs with data URIs in the HTML
    let result = re.replace_all(&html, |caps: &regex::Captures| {
        let url = caps.get(1).unwrap().as_str();
        if let Some(data_uri) = url_to_data.get(url) {
            format!("src=\"{}\"", data_uri)
        } else {
            caps[0].to_string()
        }
    });

    Ok(result.into_owned())
}

#[tauri::command]
pub async fn get_threaded_messages(
    state: State<'_, AppState>,
    account_id: String,
    folder_path: String,
    page: u32,
    per_page: u32,
    sort_column: Option<String>,
    sort_asc: Option<bool>,
    filter: Option<db::messages::QuickFilter>,
) -> Result<ThreadedPage> {
    let col = sort_column.as_deref().unwrap_or("date");
    let asc = sort_asc.unwrap_or(false);
    let qf = filter.unwrap_or_default();
    log::debug!(
        "Getting threaded messages: account={} folder={} page={} per_page={} sort={}:{}",
        account_id,
        folder_path,
        page,
        per_page,
        col,
        if asc { "asc" } else { "desc" }
    );
    let conn = state.db.lock().await;
    let result = db::messages::get_threaded_messages(
        &conn,
        &account_id,
        &folder_path,
        page,
        per_page,
        col,
        asc,
        &qf,
    )?;
    log::debug!(
        "Returned {} threads (total_threads={}, total_messages={}) for folder {}",
        result.threads.len(),
        result.total_threads,
        result.total_messages,
        folder_path
    );
    Ok(result)
}

#[tauri::command]
pub async fn get_thread_messages(
    state: State<'_, AppState>,
    account_id: String,
    folder_path: String,
    thread_id: String,
) -> Result<Vec<MessageSummary>> {
    log::debug!(
        "Getting thread messages: account={} folder={} thread={}",
        account_id,
        folder_path,
        thread_id
    );
    let conn = state.db.lock().await;
    let messages = db::messages::get_thread_messages(&conn, &account_id, &folder_path, &thread_id)?;
    log::debug!(
        "Returned {} messages for thread {}",
        messages.len(),
        thread_id
    );
    Ok(messages)
}

#[tauri::command]
pub async fn unthread_message(
    state: State<'_, AppState>,
    message_id: String,
) -> Result<()> {
    log::info!("Unthreading message: {}", message_id);
    let conn = state.db.lock().await;
    db::messages::unthread_message(&conn, &message_id)?;
    Ok(())
}

/// Create a new folder on the mail server and register it locally.
#[tauri::command]
pub async fn create_folder(
    state: State<'_, AppState>,
    account_id: String,
    folder_path: String,
) -> Result<()> {
    log::info!("Creating folder '{}' for account {}", folder_path, account_id);

    let account = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)?
    };

    if account.mail_protocol == "jmap" {
        // JMAP: Mailbox/set create
        let jmap_config = JmapConfig {
            jmap_url: account.jmap_url.clone(),
            email: account.email.clone(),
            username: account.username.clone(),
            password: account.password.clone(),
        };
        let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
        // For JMAP, folder_path is "parentId/name" (built by the frontend).
        // Split to get the parent mailbox ID and the new folder name.
        let (parent_id, mailbox_name) = if let Some((parent, name)) = folder_path.rsplit_once('/') {
            (if parent.is_empty() { None } else { Some(parent) }, name)
        } else {
            (None, folder_path.as_str())
        };
        conn_jmap.create_mailbox(&jmap_config, mailbox_name, parent_id).await?;
    } else {
        // IMAP: CREATE
        let imap_config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username,
            password: account.password,
            use_tls: account.use_tls,
            use_xoauth2: false,
        };
        let folder_for_imap = folder_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = crate::mail::imap::ImapConnection::connect(&imap_config)?;
            conn.create_folder(&folder_for_imap)?;
            conn.logout();
            Ok::<(), crate::error::Error>(())
        })
        .await
        .map_err(|e| Error::Other(format!("Create folder panicked: {}", e)))??;
    }

    // Don't insert into local DB here — the next sync will discover the folder
    // with the correct server-side path/ID and register it properly.

    log::info!("Folder '{}' created on server, will appear after sync", folder_path);
    Ok(())
}

/// Extract an attachment from a message and save it.
/// The save dialog is opened by the backend — the renderer never supplies a path.
#[tauri::command]
pub async fn save_attachment(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
    attachment_index: u32,
    suggested_filename: String,
) -> Result<()> {
    log::info!(
        "Saving attachment {} from message {}",
        attachment_index,
        message_id,
    );

    let maildir_path = {
        let conn = state.db.lock().await;
        let (mp, _, _, _, _, _, _) =
            db::messages::get_message_metadata(&conn, &account_id, &message_id)?;
        mp
    };

    if maildir_path.is_empty() || maildir_path.starts_with("graph:") {
        return Err(Error::Other(
            "Attachment save not supported for messages without local body".to_string(),
        ));
    }

    // Extract attachment bytes first, before showing dialog
    let full_path = state.data_dir.join(&maildir_path);
    let raw = std::fs::read(&full_path).map_err(|e| {
        Error::Other(format!("Failed to read message file: {}", e))
    })?;

    let parsed = mail_parser::MessageParser::default()
        .parse(&raw)
        .ok_or_else(|| Error::MailParse("Failed to parse message".to_string()))?;

    let attachment = parsed
        .attachments()
        .nth(attachment_index as usize)
        .ok_or_else(|| Error::Other(format!("Attachment index {} not found", attachment_index)))?;

    let contents = attachment.contents().to_vec();

    // Open the native save dialog from the backend — renderer cannot bypass this
    use tauri_plugin_dialog::DialogExt;
    let dest = app
        .dialog()
        .file()
        .set_file_name(&suggested_filename)
        .blocking_save_file();

    let dest = match dest {
        Some(path) => path,
        None => return Ok(()), // user cancelled
    };

    let dest_path = dest.as_path().ok_or_else(|| {
        Error::Other("Invalid save path".to_string())
    })?;

    // Refuse to follow symlinks — prevents clobbering arbitrary files
    if dest_path.is_symlink() {
        return Err(Error::Other(
            "Refusing to write to a symlink target".to_string(),
        ));
    }

    std::fs::write(dest_path, &contents).map_err(|e| {
        Error::Other(format!("Failed to write attachment: {}", e))
    })?;

    log::info!("Attachment saved to {}", dest_path.display());
    Ok(())
}
