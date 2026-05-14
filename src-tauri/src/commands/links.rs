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

// Case-insensitive ASCII prefix check. Works on raw bytes so a URL with
// multi-byte UTF-8 characters (e.g. an IRI like `héllo://...`) cannot
// trigger a panic on a non-char-boundary slice — the allowed scheme
// prefixes are all ASCII, so byte comparison is the right primitive.
fn starts_with_ci(url: &str, prefix: &str) -> bool {
    url.as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

fn has_allowed_scheme(url: &str) -> bool {
    // RFC 3986 §3.1 — schemes are case-insensitive.
    ALLOWED_SCHEMES.iter().any(|s| starts_with_ci(url, s))
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
    let to_open = if starts_with_ci(&url, "http://") || starts_with_ci(&url, "https://") {
        sanitize(&url)
    } else {
        url
    };
    tauri_plugin_opener::open_url(&to_open, None::<&str>)
        .map_err(|e| Error::Other(format!("open_url failed: {}", e)))
}
