//! JMAP EventSource push notifications (RFC 8620 §7.3).
//!
//! Opens a Server-Sent Events (SSE) stream to the JMAP server's
//! `eventSourceUrl`. When the server signals a state change (new mail,
//! flag changes, mailbox updates), emits a Tauri event so the frontend
//! can trigger a sync.
//!
//! Handles network disconnects with exponential backoff, matching the
//! IMAP IDLE reconnect strategy (ADR 0018 / ADR 0019).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::mail::jmap::{JmapConfig, JmapConnection};

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
/// This function runs indefinitely in an async task — cancel it via
/// the `stop` flag or by aborting the task.
pub async fn run_push_loop(
    config: JmapConfig,
    account_id: String,
    stop: Arc<AtomicBool>,
    on_event: Arc<dyn Fn(PushEvent) + Send + Sync>,
) {
    log::info!("JMAP push loop starting for account {}", account_id);

    let mut backoff = INITIAL_RECONNECT_DELAY;
    let mut was_disconnected = false;

    while !stop.load(Ordering::Relaxed) {
        // Connect and get the EventSource URL
        let (event_source_url, http_auth) = match connect_and_get_url(&config).await {
            Ok(v) => v,
            Err(e) => {
                if !was_disconnected {
                    log::error!(
                        "JMAP push: connection failed for {}: {}",
                        account_id, e
                    );
                    on_event(PushEvent::Disconnected(account_id.clone()));
                    was_disconnected = true;
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                log::debug!(
                    "JMAP push: retrying in {}s for {}",
                    backoff.as_secs(),
                    account_id
                );
                sleep_interruptible(&stop, backoff).await;
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
            &event_source_url,
            &http_auth,
            &account_id,
            &stop,
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
                    account_id, e
                );
                on_event(PushEvent::Disconnected(account_id.clone()));
                was_disconnected = true;
                if !stop.load(Ordering::Relaxed) {
                    sleep_interruptible(&stop, Duration::from_secs(2)).await;
                }
            }
        }
    }

    log::info!("JMAP push loop stopped for account {}", account_id);
}

/// Holds Basic Auth credentials for the SSE connection.
struct HttpAuth {
    username: String,
    password: String,
}

/// Connect to the JMAP server, fetch session, and return the EventSource URL.
async fn connect_and_get_url(config: &JmapConfig) -> Result<(String, HttpAuth), String> {
    let conn = JmapConnection::connect(config)
        .await
        .map_err(|e| format!("JMAP connect failed: {}", e))?;

    // Request all state change types: *, which covers Email, Mailbox, etc.
    let url = conn
        .event_source_url("*", PING_INTERVAL_SECS)
        .ok_or_else(|| "Server does not advertise eventSourceUrl".to_string())?;

    Ok((
        url,
        HttpAuth {
            username: config.username.clone(),
            password: config.password.clone(),
        },
    ))
}

/// Stream SSE events from the JMAP EventSource endpoint.
/// Returns Ok(()) if the stop flag was set, Err on connection/parse errors.
async fn stream_events(
    url: &str,
    auth: &HttpAuth,
    account_id: &str,
    stop: &Arc<AtomicBool>,
    on_event: Arc<dyn Fn(PushEvent) + Send + Sync>,
) -> Result<(), String> {
    use futures::StreamExt;

    log::debug!("JMAP push: opening SSE stream at {}", url);

    // No overall timeout — SSE is a long-lived connection.
    // We use READ_TIMEOUT per-chunk to detect dead connections.
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| format!("HTTP client build error: {}", e))?;

    let response = client
        .get(url)
        .basic_auth(&auth.username, Some(&auth.password))
        .header("Accept", "text/event-stream")
        // Prevent reverse proxies (nginx) from buffering SSE responses.
        .header("Cache-Control", "no-cache")
        .header("X-Accel-Buffering", "no")
        .send()
        .await
        .map_err(|e| format!("SSE request failed: {}", e))?;

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
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Wait for next chunk with a timeout.
        // If no data (including pings) in READ_TIMEOUT, connection is dead.
        let chunk = match tokio::time::timeout(READ_TIMEOUT, stream.next()).await {
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
fn handle_sse_event(
    account_id: &str,
    event_type: &str,
    data: &str,
    on_event: &dyn Fn(PushEvent),
) {
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

/// Async interruptible sleep — checks `stop` flag every second.
async fn sleep_interruptible(stop: &AtomicBool, duration: Duration) {
    let steps = duration.as_secs();
    for _ in 0..steps {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
