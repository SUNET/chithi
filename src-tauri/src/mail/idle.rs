//! IMAP IDLE loop for push-like email notifications.
//!
//! Maintains a persistent IMAP connection on INBOX and enters IDLE mode.
//! When the server signals new mail, triggers a sync of the inbox folder.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::mail::imap::{ImapConfig, ImapConnection};

/// IDLE timeout — re-enter IDLE every 25 minutes (well under the
/// 29-minute RFC 2177 limit and typical server timeouts).
const IDLE_TIMEOUT: Duration = Duration::from_secs(25 * 60);

/// Delay before reconnecting after an error.
const RECONNECT_DELAY: Duration = Duration::from_secs(30);

/// Run the IDLE loop for one IMAP account's INBOX.
/// This function blocks indefinitely — run it in a dedicated thread.
/// Set `stop` to true to gracefully exit the loop.
pub fn run_idle_loop(
    config: ImapConfig,
    account_id: String,
    stop: Arc<AtomicBool>,
    on_new_mail: Box<dyn Fn(&str) + Send>,
) {
    log::info!("IDLE loop starting for account {}", account_id);

    while !stop.load(Ordering::Relaxed) {
        // Connect
        let mut conn = match ImapConnection::connect(&config) {
            Ok(c) => c,
            Err(e) => {
                log::error!("IDLE: connection failed for {}: {}", account_id, e);
                if stop.load(Ordering::Relaxed) { break; }
                std::thread::sleep(RECONNECT_DELAY);
                continue;
            }
        };

        // Select INBOX
        if let Err(e) = conn.select_folder("INBOX") {
            log::error!("IDLE: failed to select INBOX for {}: {}", account_id, e);
            std::thread::sleep(RECONNECT_DELAY);
            continue;
        }

        log::info!("IDLE: connected and selected INBOX for account {}", account_id);

        // IDLE loop — stay on this connection until it breaks
        loop {
            if stop.load(Ordering::Relaxed) { break; }

            match conn.idle_wait(IDLE_TIMEOUT) {
                Ok(had_notification) => {
                    if had_notification {
                        log::info!("IDLE: new mail for account {}, triggering sync", account_id);
                        on_new_mail(&account_id);
                    }
                    // If timeout (no notification), just re-enter IDLE
                }
                Err(e) => {
                    log::warn!("IDLE: error for {}: {}, reconnecting...", account_id, e);
                    break; // Break inner loop to reconnect
                }
            }
        }

        // Clean up connection
        conn.logout();

        if !stop.load(Ordering::Relaxed) {
            log::info!("IDLE: reconnecting for account {}", account_id);
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    log::info!("IDLE loop stopped for account {}", account_id);
}
