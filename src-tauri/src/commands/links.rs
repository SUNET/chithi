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

/// Open a user-clicked link in the OS default handler, stripping tracking
/// parameters first via the ClearURLs ruleset. Refuses anything that is
/// not http(s) so the renderer can't shell out to mailto:, file:,
/// javascript:, etc. by smuggling a crafted href through the iframe
/// postMessage bridge or a hand-edited calendar field.
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
