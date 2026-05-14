use crate::error::{Error, Result};
use clearurls::UrlCleaner;
use std::sync::OnceLock;

/// The ClearURLs ruleset is embedded in the crate; building the cleaner
/// compiles the regex providers once. Kept in a OnceLock so the work
/// happens at most once per process rather than per command call.
fn cleaner() -> &'static UrlCleaner {
    static CLEANER: OnceLock<UrlCleaner> = OnceLock::new();
    CLEANER.get_or_init(|| {
        UrlCleaner::from_embedded_rules().expect("embedded ClearURLs ruleset is valid")
    })
}

fn sanitize(url: &str) -> String {
    cleaner()
        .clear_single_url_str(url)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| url.to_string())
}

/// Preview the cleaned form of a URL without opening it. The frontend uses
/// this to show "Open as: <cleaned>" alongside the original in the link
/// popup so the user knows what tracking will be stripped before they
/// commit to opening.
#[tauri::command]
pub fn clean_url(url: String) -> Result<String> {
    Ok(sanitize(&url))
}

/// Schemes the OS handler may be asked to open. http(s) get tracking
/// stripped first; mailto/tel are user-facing handoffs the OS already
/// knows how to route. Everything else (javascript:, file:, data:, ...)
/// is refused so a crafted href smuggled through the iframe postMessage
/// bridge or a hand-edited calendar field cannot escape the renderer.
const ALLOWED_SCHEMES: &[&str] = &["http://", "https://", "mailto:", "tel:"];

fn has_allowed_scheme(url: &str) -> bool {
    // RFC 3986 §3.1 — schemes are case-insensitive. Lowercase just enough
    // of the head to cover the longest allowed scheme prefix.
    let head_len = url.len().min(8);
    let head = url[..head_len].to_ascii_lowercase();
    ALLOWED_SCHEMES.iter().any(|s| head.starts_with(s))
}

/// Open a user-clicked link in the OS default handler, stripping tracking
/// parameters first via the ClearURLs ruleset for http(s) URLs.
/// mailto/tel are passed through unchanged.
#[tauri::command]
pub fn open_link(url: String) -> Result<()> {
    if !has_allowed_scheme(&url) {
        return Err(Error::Other(format!(
            "Refusing to open URL with disallowed scheme: {}",
            url
        )));
    }
    let to_open = if url[..5.min(url.len())].eq_ignore_ascii_case("http:")
        || url[..6.min(url.len())].eq_ignore_ascii_case("https:")
    {
        sanitize(&url)
    } else {
        url
    };
    tauri_plugin_opener::open_url(&to_open, None::<&str>)
        .map_err(|e| Error::Other(format!("open_url failed: {}", e)))
}
