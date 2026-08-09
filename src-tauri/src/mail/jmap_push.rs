//! JMAP EventSource push notifications (RFC 8620 §7.3).
//!
//! Opens a Server-Sent Events (SSE) stream to the JMAP server's
//! `eventSourceUrl`. When the server signals a state change (new mail,
//! flag changes, mailbox updates), emits a Tauri event so the frontend
//! can trigger a sync.
//!
//! Handles network disconnects with exponential backoff, matching the
//! IMAP IDLE reconnect strategy (ADR 0018 / ADR 0019).

use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::mail::jmap::{JmapConfig, JmapConnection};
use crate::provider::ProviderServices;

/// Initial delay before reconnecting after an error.
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Maximum backoff delay (5 minutes).
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(5 * 60);

/// Server ping interval — ask the server to send a ping every 30 seconds
/// so we detect dead connections quickly.
const PING_INTERVAL_SECS: u32 = 30;

/// If no data (including pings) arrives within this many seconds, treat
/// the connection as dead. Set to 3× the ping interval to tolerate jitter.
const READ_TIMEOUT: Duration = Duration::from_secs(PING_INTERVAL_SECS as u64 * 3);

/// Callback events emitted by the push loop, mirroring `idle::IdleEvent`.
pub enum PushEvent {
    /// State changed on the server — trigger a sync.
    StateChange(String),
    /// Connection lost.
    Disconnected(String),
    /// Reconnected after a disconnect.
    Reconnected(String),
}

/// Run the JMAP EventSource push loop for one account.
/// This function runs indefinitely in an async task until cancelled.
pub async fn run_push_loop(
    mut config: JmapConfig,
    account_id: String,
    cancellation: CancellationToken,
    providers: Arc<ProviderServices>,
    on_event: Arc<dyn Fn(PushEvent) + Send + Sync>,
) {
    log::info!("JMAP push loop starting for account {}", account_id);

    let mut backoff = INITIAL_RECONNECT_DELAY;
    let mut was_disconnected = false;

    while !cancellation.is_cancelled() {
        // For OIDC accounts, refresh the access token before each connect attempt
        // so reconnects after token expiry don't keep using a stale token.
        // Bearer-mode accounts (Fastmail API tokens) also have access_token set
        // but have no refresh endpoint — gate on a non-empty endpoint so we
        // don't fire useless refresh attempts for them.
        if config.access_token.is_some() && !config.oidc_token_endpoint.is_empty() {
            let refresh = tokio::select! {
                _ = cancellation.cancelled() => break,
                result = providers.credentials().jmap_push_access_token(
                    &account_id,
                    &config.oidc_token_endpoint,
                    &config.oidc_client_id,
                ) => result,
            };
            match refresh {
                Ok(Some(new_token)) => config.access_token = Some(new_token),
                Ok(None) => {}
                Err(e) => log::warn!("JMAP push: token refresh failed for {}: {}", account_id, e),
            }
        }

        // Connect and get the EventSource URL
        let connection = tokio::select! {
            _ = cancellation.cancelled() => break,
            result = connect_and_get_url(&config, &providers) => result,
        };
        let (event_source_url, http_auth) = match connection {
            Ok(Some(connection)) => connection,
            Ok(None) => {
                log::info!(
                    "JMAP push: server does not advertise eventSourceUrl for {}; skipping push",
                    account_id
                );
                break;
            }
            Err(e) => {
                if !was_disconnected {
                    log::error!("JMAP push: connection failed for {}: {}", account_id, e);
                    on_event(PushEvent::Disconnected(account_id.clone()));
                    was_disconnected = true;
                }
                if cancellation.is_cancelled() {
                    break;
                }
                log::debug!(
                    "JMAP push: retrying in {}s for {}",
                    backoff.as_secs(),
                    account_id
                );
                if !sleep_interruptible(&cancellation, backoff).await {
                    break;
                }
                backoff = (backoff * 2).min(MAX_RECONNECT_DELAY);
                continue;
            }
        };

        // Reset backoff on successful connection
        backoff = INITIAL_RECONNECT_DELAY;

        if std::mem::replace(&mut was_disconnected, false) {
            log::info!("JMAP push: reconnected for account {}", account_id);
            on_event(PushEvent::Reconnected(account_id.clone()));
            // Trigger sync on reconnect — changes may have occurred while disconnected
            on_event(PushEvent::StateChange(account_id.clone()));
        } else {
            log::info!(
                "JMAP push: connected to EventSource for account {}",
                account_id
            );
        }

        // Stream SSE events
        let result = stream_events(
            &providers.transports.jmap_sse_http,
            &event_source_url,
            &http_auth,
            &account_id,
            &cancellation,
            on_event.clone(),
        )
        .await;

        match result {
            Ok(()) => {
                // Graceful shutdown (stop flag was set)
                break;
            }
            Err(e) => {
                log::warn!(
                    "JMAP push: stream error for {}: {}, reconnecting...",
                    account_id,
                    e
                );
                on_event(PushEvent::Disconnected(account_id.clone()));
                was_disconnected = true;
                if !cancellation.is_cancelled()
                    && !sleep_interruptible(&cancellation, Duration::from_secs(2)).await
                {
                    break;
                }
            }
        }
    }

    log::info!("JMAP push loop stopped for account {}", account_id);
}

/// Holds auth credentials for the SSE connection.
struct HttpAuth {
    username: String,
    password: String,
    access_token: Option<String>,
}

impl HttpAuth {
    /// Apply authentication to a reqwest RequestBuilder.
    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(ref token) = self.access_token {
            req.bearer_auth(token)
        } else {
            req.basic_auth(&self.username, Some(&self.password))
        }
    }
}

/// Connect to the JMAP server, fetch session, and return the EventSource URL.
async fn connect_and_get_url(
    config: &JmapConfig,
    providers: &ProviderServices,
) -> Result<Option<(String, HttpAuth)>, String> {
    let conn = JmapConnection::connect_with_clients(
        config,
        providers.transports.jmap_discovery_http.clone(),
        providers.transports.jmap_api_http.clone(),
    )
    .await
    .map_err(|e| format!("JMAP connect failed: {}", e))?;

    let Some(url) = conn.event_source_url("*", PING_INTERVAL_SECS) else {
        return Ok(None);
    };

    Ok(Some((
        url,
        HttpAuth {
            username: config.username.clone(),
            password: config.password.clone(),
            access_token: config.access_token.clone(),
        },
    )))
}

/// Stream SSE events from the JMAP EventSource endpoint.
/// Returns Ok(()) if the stop flag was set, Err on connection/parse errors.
async fn stream_events(
    client: &reqwest::Client,
    url: &str,
    auth: &HttpAuth,
    account_id: &str,
    cancellation: &CancellationToken,
    on_event: Arc<dyn Fn(PushEvent) + Send + Sync>,
) -> Result<(), String> {
    use futures::StreamExt;

    log::debug!("JMAP push: opening SSE stream at {}", url);

    let request = auth
        .apply_auth(client.get(url))
        .header("Accept", "text/event-stream")
        // Prevent reverse proxies (nginx) from buffering SSE responses.
        .header("Cache-Control", "no-cache")
        .header("X-Accel-Buffering", "no")
        .send();
    let response = tokio::select! {
        _ = cancellation.cancelled() => return Ok(()),
        result = request => result.map_err(|e| format!("SSE request failed: {}", e))?,
    };

    let status = response.status();
    if !status.is_success() {
        return Err(format!("SSE endpoint returned {}", status));
    }

    log::info!(
        "JMAP push: SSE stream connected for account {} (status {})",
        account_id,
        status
    );

    // Process the SSE stream line by line, with a per-chunk read timeout
    // to detect dead/buffered connections.
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut event_type = String::new();
    let mut data_lines: Vec<String> = Vec::new();

    loop {
        // Wait for next chunk with a timeout.
        // If no data (including pings) in READ_TIMEOUT, connection is dead.
        let next_chunk = tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            result = tokio::time::timeout(READ_TIMEOUT, stream.next()) => result,
        };
        let chunk = match next_chunk {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(Some(Err(e))) => {
                return Err(format!("SSE stream error: {}", e));
            }
            Ok(None) => {
                // Stream ended (server closed connection)
                return Err("SSE stream closed by server".to_string());
            }
            Err(_) => {
                return Err(format!(
                    "SSE read timeout (no data in {}s, pings expected every {}s)",
                    READ_TIMEOUT.as_secs(),
                    PING_INTERVAL_SECS
                ));
            }
        };

        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Process complete lines from the buffer
        while let Some(newline_pos) = buffer.find('\n') {
            let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
            buffer = buffer[newline_pos + 1..].to_string();

            if line.is_empty() {
                // Empty line = end of event, dispatch it
                if !data_lines.is_empty() {
                    let data = data_lines.join("\n");
                    handle_sse_event(account_id, &event_type, &data, &*on_event);
                }
                event_type.clear();
                data_lines.clear();
            } else if let Some(value) = line.strip_prefix("event:") {
                event_type = value.trim().to_string();
            } else if let Some(value) = line.strip_prefix("data:") {
                data_lines.push(value.trim().to_string());
            }
            // Ignore "id:", "retry:", and comment lines (starting with ':')
        }
    }
}

/// Process a single SSE event. JMAP EventSource sends `state` events
/// with a JSON payload containing changed type→state mappings.
fn handle_sse_event(account_id: &str, event_type: &str, data: &str, on_event: &dyn Fn(PushEvent)) {
    match event_type {
        "state" => {
            // RFC 8620 §7.3: The data is a StateChange object with
            // "changed" mapping accountId → { "Email": "newstate", ... }
            log::info!(
                "JMAP push: state change for account {}: {}",
                account_id,
                truncate(data, 200)
            );
            on_event(PushEvent::StateChange(account_id.to_string()));
        }
        "ping" => {
            log::debug!("JMAP push: ping for account {}", account_id);
            // No action needed — just keep-alive
        }
        _ => {
            log::debug!(
                "JMAP push: unknown event type '{}' for account {}: {}",
                event_type,
                account_id,
                truncate(data, 100)
            );
            // Treat any unknown event as a potential state change
            if !data.is_empty() {
                on_event(PushEvent::StateChange(account_id.to_string()));
            }
        }
    }
}

/// Truncate a string for log output.
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}

/// Sleep for a reconnect delay, returning early when cancelled.
async fn sleep_interruptible(cancellation: &CancellationToken, duration: Duration) -> bool {
    tokio::select! {
        _ = cancellation.cancelled() => false,
        _ = tokio::time::sleep(duration) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello");
    }

    #[test]
    fn test_handle_sse_ping_does_not_trigger_event() {
        let triggered = std::sync::Arc::new(std::sync::Mutex::new(false));
        let triggered_clone = triggered.clone();
        let on_event = move |event: PushEvent| {
            if matches!(event, PushEvent::StateChange(_)) {
                *triggered_clone.lock().unwrap() = true;
            }
        };
        handle_sse_event("acc1", "ping", "", &on_event);
        assert!(!*triggered.lock().unwrap());
    }

    #[test]
    fn test_handle_sse_state_triggers_event() {
        let triggered = std::sync::Arc::new(std::sync::Mutex::new(false));
        let triggered_clone = triggered.clone();
        let on_event = move |event: PushEvent| {
            if matches!(event, PushEvent::StateChange(_)) {
                *triggered_clone.lock().unwrap() = true;
            }
        };
        handle_sse_event("acc1", "state", r#"{"changed":{}}"#, &on_event);
        assert!(*triggered.lock().unwrap());
    }

    #[tokio::test]
    async fn cancellation_interrupts_quiet_sse_stream() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (response_sent, response_received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      Content-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\n\
                      Connection: keep-alive\r\n\r\n",
                )
                .await
                .unwrap();
            response_sent.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let url = format!("http://{}/events", address);
        let client = reqwest::Client::new();
        let push = tokio::spawn(async move {
            stream_events(
                &client,
                &url,
                &HttpAuth {
                    username: "user".into(),
                    password: "password".into(),
                    access_token: None,
                },
                "account",
                &task_cancellation,
                Arc::new(|_| {}),
            )
            .await
        });

        response_received.await.unwrap();
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), push)
            .await
            .unwrap()
            .unwrap();

        assert!(result.is_ok());
        server.abort();
    }

    #[tokio::test]
    async fn reconnect_sleep_is_cancelled_promptly() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let completed = sleep_interruptible(&cancellation, Duration::from_secs(60)).await;

        assert!(!completed);
    }
}
