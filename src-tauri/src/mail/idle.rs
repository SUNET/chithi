//! IMAP IDLE loop for push-like email notifications.
//!
//! Maintains a persistent IMAP connection on INBOX and enters IDLE mode.
//! When the server signals new mail, triggers a sync of the inbox folder.
//! Handles network disconnects with exponential backoff and auto-reconnect.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::mail::imap::{ImapConfig, ImapConnection};

/// IDLE timeout — re-enter IDLE every 25 minutes (well under the
/// 29-minute RFC 2177 limit and typical server timeouts).
const IDLE_TIMEOUT: Duration = Duration::from_secs(25 * 60);

/// Initial delay before reconnecting after an error.
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Maximum backoff delay (5 minutes).
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(5 * 60);

/// Callback events emitted by the IDLE loop.
pub enum IdleEvent<'a> {
    /// New mail arrived — trigger a sync.
    NewMail(&'a str),
    /// Connection lost — network may be down.
    Disconnected(&'a str),
    /// Successfully reconnected after a disconnect.
    Reconnected(&'a str),
}

/// Run the IDLE loop for one IMAP account's INBOX.
/// This function blocks indefinitely — run it in a dedicated thread.
/// Set `stop` to true to gracefully exit the loop.
pub fn run_idle_loop(
    config: ImapConfig,
    account_id: String,
    stop: Arc<AtomicBool>,
    on_event: Box<dyn Fn(IdleEvent<'_>) + Send>,
) {
    log::info!("IDLE loop starting for account {}", account_id);

    let mut backoff = INITIAL_RECONNECT_DELAY;
    let mut was_disconnected = false;

    while !stop.load(Ordering::Relaxed) {
        // Connect
        let mut conn = match ImapConnection::connect(&config) {
            Ok(c) => c,
            Err(e) => {
                if !was_disconnected {
                    log::error!("IDLE: connection failed for {}: {}", account_id, e);
                    on_event(IdleEvent::Disconnected(&account_id));
                    was_disconnected = true;
                }
                if stop.load(Ordering::Relaxed) { break; }
                // Exponential backoff with jitter
                log::debug!("IDLE: retrying in {}s for {}", backoff.as_secs(), account_id);
                sleep_interruptible(&stop, backoff);
                backoff = (backoff * 2).min(MAX_RECONNECT_DELAY);
                continue;
            }
        };

        // Select INBOX
        if let Err(e) = conn.select_folder("INBOX") {
            log::error!("IDLE: failed to select INBOX for {}: {}", account_id, e);
            sleep_interruptible(&stop, backoff);
            backoff = (backoff * 2).min(MAX_RECONNECT_DELAY);
            continue;
        }

        // Reset backoff on successful connection
        backoff = INITIAL_RECONNECT_DELAY;

        if was_disconnected {
            log::info!("IDLE: reconnected for account {}", account_id);
            on_event(IdleEvent::Reconnected(&account_id));
            // Trigger sync on reconnect — emails may have arrived while disconnected
            on_event(IdleEvent::NewMail(&account_id));
            was_disconnected = false;
        } else {
            log::info!("IDLE: connected and selected INBOX for account {}", account_id);
        }

        // IDLE loop — stay on this connection until it breaks
        loop {
            if stop.load(Ordering::Relaxed) { break; }

            match conn.idle_wait(IDLE_TIMEOUT) {
                Ok(had_notification) => {
                    if had_notification {
                        log::info!("IDLE: new mail for account {}, triggering sync", account_id);
                        on_event(IdleEvent::NewMail(&account_id));
                    }
                    // If timeout (no notification), just re-enter IDLE
                }
                Err(e) => {
                    log::warn!("IDLE: error for {}: {}, reconnecting...", account_id, e);
                    on_event(IdleEvent::Disconnected(&account_id));
                    was_disconnected = true;
                    break; // Break inner loop to reconnect
                }
            }
        }

        // Clean up connection
        conn.logout();

        if !stop.load(Ordering::Relaxed) {
            sleep_interruptible(&stop, Duration::from_secs(2));
        }
    }

    log::info!("IDLE loop stopped for account {}", account_id);
}

/// Sleep for `duration` but check `stop` flag every second so we can
/// exit quickly when the app is shutting down.
fn sleep_interruptible(stop: &AtomicBool, duration: Duration) {
    let steps = duration.as_secs();
    for _ in 0..steps {
        if stop.load(Ordering::Relaxed) { return; }
        std::thread::sleep(Duration::from_secs(1));
    }
}
