use crate::error::{Error, Result};

/// Strip tracking parameters (utm_*, fbclid, gclid, ...) from a single URL.
/// Returns the cleaned URL, or the original if untrack made no changes.
fn sanitize(url: &str) -> String {
    untrack::clone_and_sanitize_text(url).unwrap_or_else(|| url.to_string())
}

/// Preview the cleaned form of a URL without opening it. The frontend uses
/// this to show "Open as: <cleaned>" alongside the original in the link
/// popup so the user knows what tracking will be stripped before they
/// commit to opening.
#[tauri::command]
pub fn clean_url(url: String) -> Result<String> {
    Ok(sanitize(&url))
}

/// Open a user-clicked link in the OS default handler, stripping tracking
/// parameters first via the `untrack` crate. Refuses anything that is not
/// http(s) so the renderer can't shell out to mailto:, file:, javascript:,
/// etc. by smuggling a crafted href through the iframe postMessage bridge.
#[tauri::command]
pub fn open_link(url: String) -> Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(Error::Other(format!(
            "Refusing to open non-http(s) URL: {}",
            url
        )));
    }
    let cleaned = sanitize(&url);
    tauri_plugin_opener::open_url(&cleaned, None::<&str>)
        .map_err(|e| Error::Other(format!("open_url failed: {}", e)))
}
