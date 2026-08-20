use tauri::{AppHandle, Manager, State};

use crate::commands::sync_cmd::{resume_imap_idle_for_account, suspend_imap_idle_for_operation};
use crate::db;
use crate::db::messages::{MessageSummary, ThreadedPage};
use crate::error::{Error, Result};
use crate::event::tauri::{emit_folders_changed, emit_messages_changed};
use crate::mail::imap::ImapConfig;
use crate::mail::parser;
use crate::message::{BackendMessageRef, BodyLocation, SearchHit, SearchQuery};
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
/// Provider credentials and wire behavior stay behind [`MailBackend`].
#[tauri::command]
pub async fn search_messages_server(
    app: AppHandle,
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
    let mail_config = account.mail_config();

    let backend = crate::backend::mail::for_account(&mail_config).ok_or_else(|| {
        Error::Other(format!(
            "Account {} has no enabled mail service for server search",
            account_id
        ))
    })?;
    let ctx = crate::backend::mail::MailSyncCtx {
        events: crate::event::tauri::shared_sink(app.clone()),
        db: state.db.clone(),
        data_dir: state.data_dir.clone(),
        providers: state.providers.clone(),
    };

    let suspended_idle = if backend.suspends_idle_for_ops(&mail_config) {
        Some(suspend_imap_idle_for_operation(&app, &state, &account).await?)
    } else {
        None
    };

    let hits = if let Some(suspended_idle) = suspended_idle {
        let search_account = mail_config.clone();
        let resume_account = account.clone();
        let resume_app = app.clone();
        let task = spawn_with_imap_idle_resume(
            account_id.clone(),
            "server search",
            async move { backend.search_messages(&ctx, &search_account, &query).await },
            async move {
                let state = resume_app.state::<AppState>();
                resume_imap_idle_for_account(
                    &resume_app,
                    &state,
                    &resume_account,
                    Some(suspended_idle),
                )
                .await
            },
        );
        task.await
            .map_err(|e| Error::Other(format!("Server search task panicked: {}", e)))??
    } else {
        backend.search_messages(&ctx, &mail_config, &query).await?
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
        let message_ref = BackendMessageRef::graph(&hit.backend_id);
        let id = message_ref.to_db_id(&account_id);
        let maildir = BodyLocation::GraphRemote(hit.backend_id.clone()).to_persisted();
        (id, 0u32, maildir)
    } else if account.mail_protocol_str() == "jmap" {
        // Format matches jmap_sync: `{account_id}_{mailbox_id}_{email_id}`.
        let id = BackendMessageRef::jmap(&hit.folder_path, &hit.backend_id).to_db_id(&account_id);
        (id, 0u32, String::new())
    } else {
        // IMAP: `{account_id}_{folder_path}_{uid}`.
        let uid = hit
            .uid
            .ok_or_else(|| Error::Other("IMAP search hit is missing UID".into()))?;
        let id = BackendMessageRef::imap(&hit.folder_path, uid).to_db_id(&account_id);
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
/// hasn't been downloaded yet, dispatches through the account's mail backend.
async fn ensure_message_body_on_disk(
    app: &tauri::AppHandle,
    state: &State<'_, AppState>,
    account_id: &str,
    message_id: &str,
    maildir_path: &str,
    flags_json: &str,
) -> Result<String> {
    let body_location = BodyLocation::from_persisted(maildir_path);
    if let Some(local_path) = body_location.local_path() {
        return Ok(local_path.to_string());
    }

    let (account, folder_path, uid) = {
        let conn = state.db.reader();
        let account = db::accounts::get_account_full(&conn, account_id)?;
        let (fp, u) = db::messages::get_folder_and_uid(&conn, message_id)?;
        (account, fp, u)
    };
    let mail_config = account.mail_config();

    let flags = serde_json::from_str(flags_json).unwrap_or_default();
    let request = crate::backend::mail::BodyFetchRequest::from_db_row(
        &mail_config,
        message_id,
        &folder_path,
        uid,
        flags,
        body_location,
    )?;
    let backend = crate::backend::mail::for_account(&mail_config).ok_or_else(|| {
        Error::Other(format!(
            "Account {} has no enabled mail service for body fetch",
            account_id
        ))
    })?;
    let ctx = crate::backend::mail::MailSyncCtx {
        events: crate::event::tauri::shared_sink(app.clone()),
        db: state.db.clone(),
        data_dir: state.data_dir.clone(),
        providers: state.providers.clone(),
    };

    let suspended_idle = if backend.suspends_idle_for_ops(&mail_config) {
        Some(suspend_imap_idle_for_operation(app, state, &account).await?)
    } else {
        None
    };

    if let Some(suspended_idle) = suspended_idle {
        let fetch_account = mail_config.clone();
        let resume_account = account;
        let resume_app = app.clone();
        let task = spawn_with_imap_idle_resume(
            account_id.to_string(),
            "body fetch",
            fetch_body_and_record_path(backend, ctx, fetch_account, request),
            async move {
                let state = resume_app.state::<AppState>();
                resume_imap_idle_for_account(
                    &resume_app,
                    &state,
                    &resume_account,
                    Some(suspended_idle),
                )
                .await
            },
        );
        task.await
            .map_err(|e| Error::Other(format!("Body fetch owner task panicked: {}", e)))?
    } else {
        fetch_body_and_record_path(backend, ctx, mail_config, request).await
    }
}

async fn fetch_body_and_record_path(
    backend: &'static dyn crate::backend::mail::MailBackend,
    ctx: crate::backend::mail::MailSyncCtx,
    account: crate::account::MailAccountConfig,
    request: crate::backend::mail::BodyFetchRequest,
) -> Result<String> {
    log::info!(
        "Body not on disk for {}, fetching via {}",
        request.message_id,
        backend.protocol()
    );
    let relative_path = backend.fetch_body_to_disk(&ctx, &account, &request).await?;
    let conn = ctx.db.writer().await;
    db::messages::update_maildir_path(&conn, &request.message_id, &relative_path)?;
    Ok(relative_path)
}

async fn run_with_imap_idle_resume<T, F, R>(
    account_id: &str,
    operation: &'static str,
    operation_future: F,
    resume: R,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
    R: std::future::Future<Output = Result<()>>,
{
    let operation_result = operation_future.await;
    let resume_result = resume.await;
    finish_with_imap_idle_resume(account_id, operation, operation_result, resume_result)
}

fn spawn_with_imap_idle_resume<T, F, R>(
    account_id: String,
    operation: &'static str,
    operation_future: F,
    resume: R,
) -> tokio::task::JoinHandle<Result<T>>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T>> + Send + 'static,
    R: std::future::Future<Output = Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        let operation_result = match tokio::spawn(operation_future).await {
            Ok(result) => result,
            Err(error) => Err(Error::Other(format!(
                "{} task failed: {}",
                operation, error
            ))),
        };
        let resume_result = resume.await;
        finish_with_imap_idle_resume(&account_id, operation, operation_result, resume_result)
    })
}

fn finish_with_imap_idle_resume<T>(
    account_id: &str,
    operation: &str,
    operation_result: Result<T>,
    resume_result: Result<()>,
) -> Result<T> {
    match (operation_result, resume_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(resume_error)) => Err(resume_error),
        (Err(operation_error), Ok(())) => Err(operation_error),
        (Err(operation_error), Err(resume_error)) => {
            log::error!(
                "Failed to resume IMAP IDLE after {} error for account {}: {}",
                operation,
                account_id,
                resume_error
            );
            Err(operation_error)
        }
    }
}

#[tauri::command]
pub async fn get_message_body(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
) -> Result<crate::message::MessageBody> {
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

    if BodyLocation::from_persisted(&maildir_path).needs_fetch() {
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
        let client = state
            .providers
            .graph_client(&account_id, crate::provider::GraphTokenPurpose::Baseline)
            .await?;
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
        let jmap_config = state.providers.credentials().jmap_config(&account).await?;
        let conn_jmap = crate::mail::jmap::JmapConnection::connect_with_clients(
            &jmap_config,
            state.providers.transports.jmap_discovery_http.clone(),
            state.providers.transports.jmap_api_http.clone(),
        )
        .await?;
        // For JMAP, folder_path is "parentId/name" (built by the frontend).
        // Split to get the parent mailbox ID and the new folder name.
        let (parent_id, mailbox_name) = split_folder_path(&folder_path);
        conn_jmap
            .create_mailbox(&jmap_config, mailbox_name, parent_id)
            .await?;
    } else {
        // IMAP: CREATE (O365 uses XOAUTH2)
        let credentials = state
            .providers
            .credentials()
            .mail_credentials(&account)
            .await?;
        let imap_config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username,
            password: credentials.secret,
            use_tls: account.use_tls,
            use_xoauth2: credentials.use_xoauth2,
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
        let client = state
            .providers
            .graph_client(&account_id, crate::provider::GraphTokenPurpose::Baseline)
            .await?;
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
        let jmap_config = state.providers.credentials().jmap_config(&account).await?;
        let conn_jmap = crate::mail::jmap::JmapConnection::connect_with_clients(
            &jmap_config,
            state.providers.transports.jmap_discovery_http.clone(),
            state.providers.transports.jmap_api_http.clone(),
        )
        .await?;
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
        let credentials = state
            .providers
            .credentials()
            .mail_credentials(&account)
            .await?;
        let imap_config = ImapConfig {
            host: account.imap_host,
            port: account.imap_port,
            username: account.username,
            password: credentials.secret,
            use_tls: account.use_tls,
            use_xoauth2: credentials.use_xoauth2,
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

    if BodyLocation::from_persisted(&maildir_path).needs_fetch() {
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
    use super::{
        finish_with_imap_idle_resume, run_with_imap_idle_resume, spawn_with_imap_idle_resume,
        split_folder_path,
    };
    use crate::error::Error;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn body_fetch_failure_still_runs_resume() {
        let resumed = Arc::new(AtomicBool::new(false));
        let resumed_in_future = resumed.clone();
        let result = run_with_imap_idle_resume::<String, _, _>(
            "account",
            "body fetch",
            async { Err(Error::Other("fetch failed".into())) },
            async move {
                resumed_in_future.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .await;

        assert!(resumed.load(Ordering::Relaxed));
        assert_eq!(result.unwrap_err().to_string(), "fetch failed");
    }

    #[tokio::test]
    async fn dropped_body_fetch_waiter_cannot_skip_resume() {
        let (release_fetch, fetch_released) = tokio::sync::oneshot::channel();
        let (signal_resumed, resumed) = tokio::sync::oneshot::channel();
        let task = spawn_with_imap_idle_resume(
            "account".into(),
            "body fetch",
            async move {
                fetch_released.await.unwrap();
                Ok("path".to_string())
            },
            async move {
                signal_resumed.send(()).unwrap();
                Ok(())
            },
        );

        drop(task);
        release_fetch.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), resumed)
            .await
            .expect("detached body-fetch task did not resume IDLE")
            .expect("resume signal was dropped");
    }

    #[tokio::test]
    async fn panicked_body_fetch_cannot_skip_resume() {
        let resumed = Arc::new(AtomicBool::new(false));
        let resumed_in_future = resumed.clone();
        let task = spawn_with_imap_idle_resume::<String, _, _>(
            "account".into(),
            "body fetch",
            async move { panic!("body-fetch panic") },
            async move {
                resumed_in_future.store(true, Ordering::Relaxed);
                Ok(())
            },
        );

        let result = task.await.expect("body-fetch owner task panicked");
        assert!(resumed.load(Ordering::Relaxed));
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("body fetch task failed"));
    }

    #[test]
    fn body_fetch_result_preserves_fetch_error_when_resume_also_fails() {
        let result = finish_with_imap_idle_resume::<String>(
            "account",
            "body fetch",
            Err(Error::Other("fetch failed".into())),
            Err(Error::Other("resume failed".into())),
        );
        assert_eq!(result.unwrap_err().to_string(), "fetch failed");
    }

    #[test]
    fn body_fetch_result_returns_resume_error_after_successful_fetch() {
        let result = finish_with_imap_idle_resume(
            "account",
            "body fetch",
            Ok("path".to_string()),
            Err(Error::Other("resume failed".into())),
        );
        assert_eq!(result.unwrap_err().to_string(), "resume failed");
    }

    #[test]
    fn body_fetch_result_returns_fetch_error_after_successful_resume() {
        let result = finish_with_imap_idle_resume::<String>(
            "account",
            "body fetch",
            Err(Error::Other("fetch failed".into())),
            Ok(()),
        );
        assert_eq!(result.unwrap_err().to_string(), "fetch failed");
    }

    #[test]
    fn body_fetch_result_returns_value_when_fetch_and_resume_succeed() {
        let result = finish_with_imap_idle_resume("account", "body fetch", Ok("path"), Ok(()));
        assert_eq!(result.unwrap(), "path");
    }

    #[tokio::test]
    async fn server_search_failure_still_runs_resume() {
        let resumed = Arc::new(AtomicBool::new(false));
        let resumed_in_future = resumed.clone();
        let result = run_with_imap_idle_resume::<Vec<String>, _, _>(
            "account",
            "server search",
            async { Err(Error::Other("search failed".into())) },
            async move {
                resumed_in_future.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .await;

        assert!(resumed.load(Ordering::Relaxed));
        assert_eq!(result.unwrap_err().to_string(), "search failed");
    }

    #[tokio::test]
    async fn dropped_search_waiter_cannot_skip_resume() {
        let (release_search, search_released) = tokio::sync::oneshot::channel();
        let (signal_resumed, resumed) = tokio::sync::oneshot::channel();
        let task = spawn_with_imap_idle_resume(
            "account".into(),
            "server search",
            async move {
                search_released.await.unwrap();
                Ok(Vec::<String>::new())
            },
            async move {
                signal_resumed.send(()).unwrap();
                Ok(())
            },
        );

        drop(task);
        release_search.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), resumed)
            .await
            .expect("detached search task did not resume IDLE")
            .expect("resume signal was dropped");
    }

    #[tokio::test]
    async fn panicked_search_cannot_skip_resume() {
        let resumed = Arc::new(AtomicBool::new(false));
        let resumed_in_future = resumed.clone();
        let task = spawn_with_imap_idle_resume::<Vec<String>, _, _>(
            "account".into(),
            "server search",
            async move { panic!("search panic") },
            async move {
                resumed_in_future.store(true, Ordering::Relaxed);
                Ok(())
            },
        );

        let result = task.await.expect("search owner task panicked");
        assert!(resumed.load(Ordering::Relaxed));
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("server search task failed"));
    }

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
