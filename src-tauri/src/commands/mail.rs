use tauri::State;

use crate::commands::sync_cmd::{
    resume_imap_idle_for_account, should_suspend_idle_for_imap_operation,
    suspend_imap_idle_for_account,
};
use crate::db;
use crate::db::messages::{MessageSummary, ThreadedPage};
use crate::error::{Error, Result};
use crate::event::{emit_folders_changed, emit_messages_changed};
use crate::mail::imap::ImapConfig;
use crate::mail::jmap_sync;
use crate::mail::parser;
use crate::mail::search::{SearchHit, SearchQuery};
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
            || v4.octets()[0] == 100 && v4.octets()[1] >= 64 && v4.octets()[1] <= 127
            // 100.64.0.0/10 (CGNAT)
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

/// Split a folder path of the form `parentId/name` into its parent ID and
/// leaf name. For JMAP the parent component is a mailbox ID; for Graph it is a
/// mail folder ID. An empty or missing parent component means a top-level
/// folder, returned as `None`.
fn split_folder_path(folder_path: &str) -> (Option<&str>, &str) {
    match folder_path.rsplit_once('/') {
        Some((parent, name)) if !parent.is_empty() => (Some(parent), name),
        Some((_, name)) => (None, name),
        None => (None, folder_path),
    }
}

#[tauri::command]
pub async fn list_folders(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Vec<db::folders::Folder>> {
    log::debug!("Listing folders for account {}", account_id);
    let conn = state.db.reader();
    let flat_folders = db::folders::list_folders(&conn, &account_id)?;
    log::debug!(
        "Found {} folders for account {}",
        flat_folders.len(),
        account_id
    );
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
    let conn = state.db.reader();
    let result = db::messages::get_messages(
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
        "Returned {} messages (total={}) for folder {}",
        result.messages.len(),
        result.total,
        folder_path
    );
    Ok(result)
}

/// Run a server-side search across all folders of an account.
/// Dispatches to the IMAP, JMAP, or Graph backend based on `mail_protocol`.
#[tauri::command]
pub async fn search_messages_server(
    state: State<'_, AppState>,
    account_id: String,
    query: SearchQuery,
) -> Result<Vec<SearchHit>> {
    log::info!(
        "Server search: account={} fields={:?} text_len={}",
        account_id,
        query.fields,
        query.text.len(),
    );

    let account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    let hits = if account.mail_protocol_str() == "graph" {
        let token = crate::mail::graph::get_graph_token(&account_id).await?;
        let client = crate::mail::graph::GraphClient::new(&token);
        client.search_messages(&account_id, &query).await?
    } else if account.mail_protocol_str() == "jmap" {
        let jmap_config = crate::auth::build_jmap_config(&account).await?;
        let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
        conn_jmap
            .search_account(&jmap_config, &account_id, &query)
            .await?
    } else {
        let (password, use_xoauth2) = if account.auth_method == "oauth-microsoft" {
            let tokens = crate::oauth::load_tokens(&account_id)?
                .ok_or_else(|| Error::Other("No O365 tokens".into()))?;
            let refresh = tokens
                .refresh_token
                .ok_or_else(|| Error::Other("No O365 refresh token".into()))?;
            let new = crate::oauth::refresh_with_scopes(
                &crate::oauth::MICROSOFT,
                &refresh,
                crate::oauth::MICROSOFT_IMAP_SCOPES,
            )
            .await?;
            crate::oauth::store_tokens(&account_id, &new)?;
            (new.access_token, true)
        } else {
            (account.password.clone(), false)
        };
        let imap_config = ImapConfig {
            host: account.imap_host.clone(),
            port: account.imap_port,
            username: account.username.clone(),
            password,
            use_tls: account.use_tls,
            use_xoauth2,
        };

        let account_id_for_blocking = account_id.clone();
        let query_for_blocking = query.clone();
        tokio::task::spawn_blocking(move || {
            crate::mail::imap::search_account_blocking(
                &imap_config,
                &account_id_for_blocking,
                &query_for_blocking,
            )
        })
        .await
        .map_err(|e| Error::Other(format!("IMAP search task panicked: {}", e)))??
    };

    log::info!("Server search returned {} hits", hits.len());
    Ok(hits)
}

/// Insert (or upsert) a server-search hit into the local messages table so
/// the existing `get_message_body` flow can fetch and render it. Returns the
/// synthetic database id, which the frontend then passes to `loadMessage`.
///
/// We don't have the full envelope (size, to/cc, encryption flags) from the
/// search response, so we fill in reasonable defaults — the next sync of
/// that folder will reconcile them.
#[tauri::command]
pub async fn import_search_hit(
    state: State<'_, AppState>,
    account_id: String,
    hit: SearchHit,
) -> Result<String> {
    if hit.account_id != account_id {
        return Err(Error::Other(format!(
            "search hit account_id mismatch: command got {:?}, hit was tagged {:?}",
            account_id, hit.account_id
        )));
    }

    let account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    let (id, uid, maildir_path) = if account.mail_protocol_str() == "graph" {
        // Format matches sync_cmd::sync_graph_account: `{account_id}_{graph_id}`,
        // and `graph:{graph_id}` in maildir_path triggers the on-demand stream
        // path in get_message_body.
        let id = format!("{}_{}", account_id, hit.backend_id);
        let maildir = format!("graph:{}", hit.backend_id);
        (id, 0u32, maildir)
    } else if account.mail_protocol_str() == "jmap" {
        // Format matches jmap_sync: `{account_id}_{mailbox_id}_{email_id}`.
        let id = format!("{}_{}_{}", account_id, hit.folder_path, hit.backend_id);
        (id, 0u32, String::new())
    } else {
        // IMAP: `{account_id}_{folder_path}_{uid}`.
        let uid = hit
            .uid
            .ok_or_else(|| Error::Other("IMAP search hit is missing UID".into()))?;
        let id = format!("{}_{}_{}", account_id, hit.folder_path, uid);
        (id, uid, String::new())
    };

    let date_str = if hit.date > 0 {
        chrono::DateTime::<chrono::Utc>::from_timestamp(hit.date, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default()
    } else {
        chrono::Utc::now().to_rfc3339()
    };

    let new_msg = db::messages::NewMessage {
        id: id.clone(),
        account_id: account_id.clone(),
        folder_path: hit.folder_path,
        uid,
        message_id: hit.message_id,
        in_reply_to: None,
        thread_id: None,
        subject: if hit.subject.is_empty() {
            None
        } else {
            Some(hit.subject)
        },
        from_name: hit.from_name,
        from_email: hit.from_email.unwrap_or_else(|| "unknown".to_string()),
        to_addresses: "[]".to_string(),
        cc_addresses: "[]".to_string(),
        date: date_str,
        size: 0,
        has_attachments: false,
        is_encrypted: false,
        is_signed: false,
        flags: "[]".to_string(),
        maildir_path,
        snippet: hit.snippet,
    };

    {
        let conn = state.db.writer().await;
        db::messages::insert_message(&conn, &new_msg)?;
    }

    Ok(id)
}

/// Ensure the raw RFC822 body for `message_id` is on disk and return the
/// relative maildir path that resolves under `state.data_dir`. If the body
/// hasn't been downloaded yet (empty or legacy `graph:` prefix), fetches it
/// on-demand from the appropriate backend (Graph / JMAP / IMAP).
async fn ensure_message_body_on_disk(
    app: &tauri::AppHandle,
    state: &State<'_, AppState>,
    account_id: &str,
    message_id: &str,
    maildir_path: &str,
    flags_json: &str,
) -> Result<String> {
    if !maildir_path.is_empty() && !maildir_path.starts_with("graph:") {
        return Ok(maildir_path.to_string());
    }

    let (account, folder_path, uid) = {
        let conn = state.db.reader();
        let account = db::accounts::get_account_full(&conn, account_id)?;
        let (fp, u) = db::messages::get_folder_and_uid(&conn, message_id)?;
        (account, fp, u)
    };

    let flags: Vec<String> = serde_json::from_str(flags_json).unwrap_or_default();
    let data_dir = state.data_dir.clone();

    let relative_path = if account.mail_protocol_str() == "graph" {
        log::info!("Body not on disk for {}, streaming from Graph", message_id);

        let graph_msg_id = if let Some(gid) = maildir_path.strip_prefix("graph:") {
            gid.to_string()
        } else {
            message_id
                .strip_prefix(&format!("{}_", account_id))
                .unwrap_or(message_id)
                .to_string()
        };

        let token = crate::mail::graph::get_graph_token(account_id).await?;
        let client = crate::mail::graph::GraphClient::new(&token);

        let folder_dir = crate::mail::sync::sanitize_folder_name(&folder_path);
        let maildir_base = data_dir.join(account_id).join(&folder_dir);
        crate::mail::sync::create_maildir_dirs(&maildir_base)?;

        let filename = format!(
            "{}:2,{}",
            graph_msg_id,
            crate::mail::sync::flags_to_maildir_suffix(&flags)
        );
        let msg_path = maildir_base.join("cur").join(&filename);

        let bytes_written = client
            .download_mime_to_file(&graph_msg_id, &msg_path)
            .await?;
        let rp = format!("{}/{}/cur/{}", account_id, folder_dir, filename);
        log::info!("Graph body streamed: {} ({} bytes)", rp, bytes_written);
        rp
    } else if account.mail_protocol_str() == "jmap" {
        log::info!("Body not on disk for {}, fetching from JMAP", message_id);

        let jmap_config = crate::auth::build_jmap_config(&account).await?;

        let jmap_email_id = message_id
            .strip_prefix(&format!("{}_{}_", account_id, folder_path))
            .unwrap_or(message_id);

        jmap_sync::fetch_and_store_jmap_body(
            &jmap_config,
            &data_dir,
            account_id,
            &folder_path,
            jmap_email_id,
            &flags,
        )
        .await?
    } else {
        log::info!("Body not on disk for {}, fetching from IMAP", message_id);

        let suspended_idle = if should_suspend_idle_for_imap_operation(&account.auth_method) {
            suspend_imap_idle_for_account(state, account_id).await?
        } else {
            false
        };
        let resume_account = account.clone();

        let (password, use_xoauth2) = if account.auth_method == "oauth-microsoft" {
            let tokens = crate::oauth::load_tokens(account_id)?
                .ok_or_else(|| Error::Other("No O365 tokens".into()))?;
            let refresh = tokens
                .refresh_token
                .ok_or_else(|| Error::Other("No O365 refresh token".into()))?;
            let new = crate::oauth::refresh_with_scopes(
                &crate::oauth::MICROSOFT,
                &refresh,
                crate::oauth::MICROSOFT_IMAP_SCOPES,
            )
            .await?;
            crate::oauth::store_tokens(account_id, &new)?;
            (new.access_token, true)
        } else {
            (account.password, false)
        };

        let imap_config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username,
            password,
            use_tls: account.use_tls,
            use_xoauth2,
        };

        let account_id_clone = account_id.to_string();
        let relative_path = tokio::task::spawn_blocking(move || {
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
        .map_err(|e| Error::Other(format!("Body fetch panicked: {}", e)))??;

        resume_imap_idle_for_account(app, state, &resume_account, suspended_idle).await?;

        relative_path
    };

    {
        let conn = state.db.writer().await;
        db::messages::update_maildir_path(&conn, message_id, &relative_path)?;
    }

    Ok(relative_path)
}

#[tauri::command]
pub async fn get_message_body(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
) -> Result<db::messages::MessageBody> {
    log::debug!("Loading message body: {}", message_id);

    let (maildir_path, from_email, to_json, cc_json, flags_json, is_encrypted, is_signed) = {
        let conn = state.db.reader();
        db::messages::get_message_metadata(&conn, &account_id, &message_id)?
    };

    let actual_maildir_path = ensure_message_body_on_disk(
        &app,
        &state,
        &account_id,
        &message_id,
        &maildir_path,
        &flags_json,
    )
    .await?;

    // Read and parse the message from disk
    let full_path = crate::path_validation::resolve_under(&state.data_dir, &actual_maildir_path)?;
    log::debug!("Reading message from {}", full_path.display());
    let raw = std::fs::read(&full_path).map_err(|e| {
        log::error!("Failed to read message file {}: {}", full_path.display(), e);
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
        let conn = state.db.reader();
        let (mp, _, _, _, _, _, _) =
            db::messages::get_message_metadata(&conn, &account_id, &message_id)?;
        mp
    };

    if maildir_path.is_empty() || maildir_path.starts_with("graph:") {
        return Err(Error::Other(
            "Remote images not supported for messages without local body".to_string(),
        ));
    }

    let full_path = crate::path_validation::resolve_under(&state.data_dir, &maildir_path)?;
    let raw = std::fs::read(&full_path)
        .map_err(|e| Error::Other(format!("Failed to read message file: {}", e)))?;

    let html = parser::parse_html_with_images(&raw)
        .ok_or_else(|| Error::MailParse("Failed to parse message HTML".to_string()))?;

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
    let mut url_to_data: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let futures: Vec<_> = urls
        .iter()
        .take(20)
        .map(|url| {
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
                        if let Ok(addrs) = tokio::net::lookup_host(format!(
                            "{}:{}",
                            host,
                            parsed.port_or_known_default().unwrap_or(443)
                        ))
                        .await
                        {
                            for addr in addrs {
                                let ip = addr.ip();
                                if ip.is_loopback() || ip.is_unspecified() || is_private_ip(&ip) {
                                    log::debug!(
                                        "Image proxy: blocked private IP {} for {}",
                                        ip,
                                        host
                                    );
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
        })
        .collect();

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
    let conn = state.db.reader();
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
    let conn = state.db.reader();
    let messages = db::messages::get_thread_messages(&conn, &account_id, &folder_path, &thread_id)?;
    log::debug!(
        "Returned {} messages for thread {}",
        messages.len(),
        thread_id
    );
    Ok(messages)
}

#[tauri::command]
pub async fn unthread_message(state: State<'_, AppState>, message_id: String) -> Result<()> {
    log::info!("Unthreading message: {}", message_id);
    let conn = state.db.writer().await;
    db::messages::unthread_message(&conn, &message_id)?;
    Ok(())
}

/// Create a new folder on the mail server and register it locally.
#[tauri::command]
pub async fn create_folder(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    folder_path: String,
) -> Result<()> {
    log::info!(
        "Creating folder '{}' for account {}",
        folder_path,
        account_id
    );

    let account = {
        let conn = state.db.reader();
        db::accounts::get_account_full(&conn, &account_id)?
    };

    if account.mail_protocol_str() == "graph" {
        // Microsoft Graph: POST /me/mailFolders (or .../childFolders).
        // folder_path is "parentFolderId/name" (built by the frontend); the
        // parent component is the parent's Graph folder ID.
        let (parent_id, folder_name) = split_folder_path(&folder_path);
        let token = crate::mail::graph::get_graph_token(&account_id).await?;
        let client = crate::mail::graph::GraphClient::new(&token);
        client
            .create_mail_folder(folder_name, parent_id)
            .await
            .map_err(|e| {
                log::error!(
                    "Failed to create Graph folder '{}' for account {}: {}",
                    folder_path,
                    account_id,
                    e
                );
                e
            })?;
    } else if account.mail_protocol_str() == "jmap" {
        // JMAP: Mailbox/set create
        let jmap_config = crate::auth::build_jmap_config(&account).await?;
        let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
        // For JMAP, folder_path is "parentId/name" (built by the frontend).
        // Split to get the parent mailbox ID and the new folder name.
        let (parent_id, mailbox_name) = split_folder_path(&folder_path);
        conn_jmap
            .create_mailbox(&jmap_config, mailbox_name, parent_id)
            .await?;
    } else {
        // IMAP: CREATE (O365 uses XOAUTH2)
        let (imap_password, imap_xoauth2) = if account.auth_method == "oauth-microsoft" {
            let tokens = crate::oauth::load_tokens(&account_id)?
                .ok_or_else(|| Error::Other("No O365 tokens".into()))?;
            let refresh = tokens
                .refresh_token
                .ok_or_else(|| Error::Other("No O365 refresh token".into()))?;
            let new = crate::oauth::refresh_with_scopes(
                &crate::oauth::MICROSOFT,
                &refresh,
                crate::oauth::MICROSOFT_IMAP_SCOPES,
            )
            .await?;
            crate::oauth::store_tokens(&account_id, &new)?;
            (new.access_token, true)
        } else {
            (account.password, false)
        };
        let imap_config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username,
            password: imap_password,
            use_tls: account.use_tls,
            use_xoauth2: imap_xoauth2,
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

    log::info!(
        "Folder '{}' created on server, will appear after sync",
        folder_path
    );
    emit_folders_changed(&app, &account_id);
    Ok(())
}

/// System folder types that must never be deleted.
const PROTECTED_FOLDER_TYPES: &[&str] = &["inbox", "sent", "drafts", "trash", "junk", "archive"];

/// Delete a folder on the mail server and remove it from local DB.
/// Refuses to delete system folders (inbox, sent, drafts, trash, junk, archive).
#[tauri::command]
pub async fn delete_folder(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    folder_path: String,
) -> Result<()> {
    log::info!(
        "Deleting folder '{}' for account {}",
        folder_path,
        account_id
    );

    // Verify the folder exists in the local DB and is not a system folder
    let account = {
        let conn = state.db.reader();

        // Check that the folder belongs to this account
        let folder_type: Option<String> = conn
            .query_row(
                "SELECT folder_type FROM folders WHERE account_id = ?1 AND path = ?2",
                rusqlite::params![account_id, folder_path],
                |row| row.get(0),
            )
            .map_err(|_| {
                Error::Other(format!(
                    "Folder '{}' not found for account {}",
                    folder_path, account_id
                ))
            })?;

        // Reject deletion of system folders
        if let Some(ref ft) = folder_type {
            if PROTECTED_FOLDER_TYPES.contains(&ft.as_str()) {
                log::warn!(
                    "Refusing to delete system folder '{}' (type={}) for account {}",
                    folder_path,
                    ft,
                    account_id
                );
                return Err(Error::Other(format!(
                    "Cannot delete system folder '{}' ({})",
                    folder_path, ft
                )));
            }
        }

        db::accounts::get_account_full(&conn, &account_id)?
    };

    if account.mail_protocol_str() == "graph" {
        let token = crate::mail::graph::get_graph_token(&account_id).await?;
        let client = crate::mail::graph::GraphClient::new(&token);
        client.delete_mail_folder(&folder_path).await.map_err(|e| {
            log::error!(
                "Failed to delete Graph folder '{}' for account {}: {}",
                folder_path,
                account_id,
                e
            );
            e
        })?;
    } else if account.mail_protocol_str() == "jmap" {
        // JMAP: Mailbox/set destroy — folder_path is the mailbox ID
        let jmap_config = crate::auth::build_jmap_config(&account).await?;
        let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
        conn_jmap
            .destroy_mailbox(&jmap_config, &folder_path, true)
            .await
            .map_err(|e| {
                log::error!(
                    "Failed to delete JMAP folder '{}' for account {}: {}",
                    folder_path,
                    account_id,
                    e
                );
                e
            })?;
    } else {
        // IMAP: DELETE
        let (imap_password, imap_xoauth2) = if account.auth_method == "oauth-microsoft" {
            let tokens = crate::oauth::load_tokens(&account_id)?
                .ok_or_else(|| Error::Other("No O365 tokens".into()))?;
            let refresh = tokens
                .refresh_token
                .ok_or_else(|| Error::Other("No O365 refresh token".into()))?;
            let new = crate::oauth::refresh_with_scopes(
                &crate::oauth::MICROSOFT,
                &refresh,
                crate::oauth::MICROSOFT_IMAP_SCOPES,
            )
            .await?;
            crate::oauth::store_tokens(&account_id, &new)?;
            (new.access_token, true)
        } else {
            (account.password, false)
        };
        let imap_config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username,
            password: imap_password,
            use_tls: account.use_tls,
            use_xoauth2: imap_xoauth2,
        };
        let folder_for_imap = folder_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = crate::mail::imap::ImapConnection::connect(&imap_config)?;
            conn.delete_folder(&folder_for_imap)?;
            conn.logout();
            Ok::<(), crate::error::Error>(())
        })
        .await
        .map_err(|e| Error::Other(format!("Delete folder panicked: {}", e)))??;
    }

    // Remove from local DB
    {
        let conn = state.db.writer().await;
        db::folders::delete_folder(&conn, &account_id, &folder_path)?;
    }

    log::info!(
        "Folder '{}' deleted for account {}",
        folder_path,
        account_id
    );
    emit_folders_changed(&app, &account_id);
    emit_messages_changed(&app, &account_id);
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
        let conn = state.db.reader();
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
    let full_path = crate::path_validation::resolve_under(&state.data_dir, &maildir_path)?;
    let raw = std::fs::read(&full_path)
        .map_err(|e| Error::Other(format!("Failed to read message file: {}", e)))?;

    let parsed = mail_parser::MessageParser::default()
        .parse(&raw)
        .ok_or_else(|| Error::MailParse("Failed to parse message".to_string()))?;

    let attachment = parsed
        .attachments()
        .nth(attachment_index as usize)
        .ok_or_else(|| Error::Other(format!("Attachment index {} not found", attachment_index)))?;

    let contents = attachment.contents().to_vec();

    prompt_save_and_stream(
        &app,
        &suggested_filename,
        "attachment",
        std::io::Cursor::new(contents),
    )
    .await
}

/// Open the native save-as dialog with `suggested_filename`, then atomically
/// stream bytes from `reader` to the user-chosen path (refusing symlinks,
/// temp file + fsync + rename). `std::io::copy` is used so large payloads
/// don't have to be buffered in memory. Returns `Ok(())` if the user
/// cancelled the dialog, since cancellation isn't an error from the
/// caller's perspective.
///
/// The non-blocking callback API + oneshot is intentional: calling
/// `blocking_save_file()` on the tokio worker that invoked the command
/// starves the GTK main thread on Linux, which manifests as a dialog that
/// opens but never renders its Save button.
async fn prompt_save_and_stream<R: std::io::Read>(
    app: &tauri::AppHandle,
    suggested_filename: &str,
    what: &str,
    mut reader: R,
) -> Result<()> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_file_name(suggested_filename)
        .save_file(move |path| {
            let _ = tx.send(path);
        });

    let dest = rx
        .await
        .map_err(|e| Error::Other(format!("Save dialog closed unexpectedly: {}", e)))?;

    let dest = match dest {
        Some(path) => path,
        None => return Ok(()),
    };

    let dest_path = dest
        .as_path()
        .ok_or_else(|| Error::Other("Invalid save path".to_string()))?;

    if let Ok(metadata) = std::fs::symlink_metadata(dest_path) {
        if metadata.file_type().is_symlink() {
            return Err(Error::Other(
                "Refusing to write to a symlink target".to_string(),
            ));
        }
    }

    let dest_dir = dest_path
        .parent()
        .ok_or_else(|| Error::Other("Save path must have a parent directory".to_string()))?;
    let dest_name = dest_path
        .file_name()
        .ok_or_else(|| Error::Other("Save path must include a file name".to_string()))?;
    let unique_suffix = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let temp_path = dest_dir.join(format!(
        ".{}.{}.tmp",
        dest_name.to_string_lossy(),
        unique_suffix
    ));

    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp_path)
            .map_err(|e| Error::Other(format!("Failed to create temp file: {}", e)))?
    };

    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|e| Error::Other(format!("Failed to create temp file: {}", e)))?;

    if let Err(e) = std::io::copy(&mut reader, &mut file) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(Error::Other(format!("Failed to write {}: {}", what, e)));
    }
    if let Err(e) = file.sync_all() {
        let _ = std::fs::remove_file(&temp_path);
        return Err(Error::Other(format!("Failed to flush {}: {}", what, e)));
    }
    drop(file);

    #[cfg(not(unix))]
    {
        // On Windows, rename fails if dest exists. Remove it first.
        if dest_path.exists() {
            let _ = std::fs::remove_file(dest_path);
        }
    }

    if let Err(e) = std::fs::rename(&temp_path, dest_path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(Error::Other(format!("Failed to rename temp file: {}", e)));
    }

    #[cfg(unix)]
    {
        // Fsync parent directory for durability.
        if let Ok(dir) = std::fs::File::open(dest_dir) {
            let _ = dir.sync_all();
        }
    }

    log::info!("{} saved to {}", what, dest_path.display());
    Ok(())
}

/// Save a message's raw RFC822 (.eml) bytes to a user-chosen path. Fetches
/// the body on-demand if it isn't on disk yet, then opens the native save
/// dialog and streams the bytes through to the destination without
/// buffering the whole message in memory. `suggested_filename` is provided
/// by the caller (typically derived from the subject).
#[tauri::command]
pub async fn save_message_as_eml(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
    suggested_filename: String,
) -> Result<()> {
    log::info!("Saving message {} as .eml", message_id);

    let (maildir_path, _, _, _, flags_json, _, _) = {
        let conn = state.db.reader();
        db::messages::get_message_metadata(&conn, &account_id, &message_id)?
    };

    let actual_maildir_path = ensure_message_body_on_disk(
        &app,
        &state,
        &account_id,
        &message_id,
        &maildir_path,
        &flags_json,
    )
    .await?;

    let full_path = crate::path_validation::resolve_under(&state.data_dir, &actual_maildir_path)?;
    let source = std::fs::File::open(&full_path)
        .map_err(|e| Error::Other(format!("Failed to open message file: {}", e)))?;

    prompt_save_and_stream(
        &app,
        &suggested_filename,
        "message",
        std::io::BufReader::new(source),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::split_folder_path;

    // A bare name with no slash is a top-level folder.
    #[test]
    fn split_folder_path_top_level() {
        assert_eq!(split_folder_path("Projects"), (None, "Projects"));
    }

    // "parentId/name" yields the parent ID and the leaf name. The parent is a
    // server-side ID (JMAP mailbox ID / Graph folder ID), so it must not be
    // confused with the leaf — a regression here once routed Graph/JMAP folder
    // creation down the IMAP path.
    #[test]
    fn split_folder_path_nested() {
        assert_eq!(
            split_folder_path("AAMkAGI2parentid/Reports"),
            (Some("AAMkAGI2parentid"), "Reports")
        );
    }

    // A leading slash means an empty parent component — treat as top-level.
    #[test]
    fn split_folder_path_empty_parent() {
        assert_eq!(split_folder_path("/Reports"), (None, "Reports"));
    }

    // Only the last slash separates parent from leaf; earlier slashes (e.g. in
    // a Graph base64 ID) stay with the parent component.
    #[test]
    fn split_folder_path_splits_on_last_slash() {
        assert_eq!(
            split_folder_path("grand/parent/Child"),
            (Some("grand/parent"), "Child")
        );
    }
}
