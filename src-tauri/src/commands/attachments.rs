//! Attachment picking via opaque tokens.
//!
//! Previously the renderer opened the file dialog itself, received
//! absolute paths, and shipped those paths back to the backend on send.
//! A compromised renderer (e.g. via HTML mail sandbox escape) could
//! therefore stage reads of any local file the OS user can see.
//!
//! The backend now owns the dialog: `pick_attachments` opens the native
//! dialog, canonicalises each chosen path, stores the mapping in an
//! in-memory token registry, and returns opaque tokens. The renderer
//! can only refer to files it actually picked; unknown tokens are
//! rejected at send time.

use serde::Serialize;
use tauri::State;
use tauri_plugin_dialog::DialogExt;

use crate::error::{Error, Result};
use crate::state::AppState;

/// Opaque handle returned to the renderer after a successful pick.
/// Contains no path information; the file metadata is included so the
/// compose UI can render a chip without needing the raw path.
#[derive(Debug, Serialize)]
pub struct AttachmentHandle {
    pub token: String,
    pub name: String,
    pub size: u64,
}

/// Open a native file-picker dialog, register each selected file under
/// a random token, and return the handles.
///
/// The token is a v4 UUID. The backend stores `token -> canonical_path`
/// in `AppState::attachments`. Later send/save flows resolve tokens via
/// `consume_tokens` / `peek_tokens` and pass the resulting paths into
/// `build_attachment_data`; tokens the renderer invents will not match
/// and are rejected.
///
/// Picking the same file twice returns the *existing* token for that
/// canonical path rather than a fresh one, so the frontend's
/// dedup-by-token check catches it.
#[tauri::command]
pub async fn pick_attachments(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<AttachmentHandle>> {
    // Non-blocking dialog + oneshot: see save_attachment for why we avoid
    // blocking_pick_files (GTK main-thread starvation on Linux).
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_files(move |paths| {
        let _ = tx.send(paths);
    });

    let paths = rx
        .await
        .map_err(|e| Error::Other(format!("Attachment picker closed unexpectedly: {}", e)))?;

    let paths = match paths {
        Some(ps) => ps,
        None => return Ok(vec![]), // user cancelled
    };

    // Resolve + stat every picked file before touching the registry so a
    // bad pick doesn't leave a half-populated state.
    let mut resolved: Vec<(std::path::PathBuf, u64)> = Vec::with_capacity(paths.len());
    for file_path in paths {
        let path = file_path
            .as_path()
            .ok_or_else(|| Error::Other("Picked path was not a local filesystem path".into()))?;

        let canonical = std::fs::canonicalize(path).map_err(|e| {
            Error::Other(format!(
                "Failed to resolve picked file {}: {}",
                path.display(),
                e
            ))
        })?;

        let metadata = std::fs::metadata(&canonical).map_err(|e| {
            Error::Other(format!(
                "Failed to stat picked file {}: {}",
                canonical.display(),
                e
            ))
        })?;

        if !metadata.is_file() {
            return Err(Error::Other(format!(
                "Not a regular file: {}",
                canonical.display()
            )));
        }

        resolved.push((canonical, metadata.len()));
    }

    let mut handles = Vec::with_capacity(resolved.len());
    let mut reg = state.attachments.lock().unwrap_or_else(|e| e.into_inner());
    for (canonical, size) in resolved {
        // Dedup by canonical path: if the same file was already registered
        // in this session, hand back the existing token. The renderer then
        // sees the same token and its dedup-by-token check keeps the
        // compose list clean.
        let existing = reg
            .iter()
            .find(|(_, p)| **p == canonical)
            .map(|(t, _)| t.clone());
        let token = match existing {
            Some(t) => t,
            None => {
                let t = uuid::Uuid::new_v4().to_string();
                reg.insert(t.clone(), canonical.clone());
                t
            }
        };

        let name = canonical
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".to_string());

        handles.push(AttachmentHandle { token, name, size });
    }

    Ok(handles)
}

/// Drop a registered attachment token. Called when the user removes an
/// attachment chip or closes the compose window without sending.
///
/// Unknown tokens are silently ignored — the frontend may double-release
/// on rapid remove/close sequences and that is harmless.
#[tauri::command]
pub fn release_attachment(state: State<'_, AppState>, token: String) -> Result<()> {
    let mut reg = state.attachments.lock().unwrap_or_else(|e| e.into_inner());
    reg.remove(&token);
    Ok(())
}

/// Remove the given tokens from the registry and return the canonical
/// paths in the same order. Unknown tokens produce an error so a
/// compromised renderer cannot inject bogus handles into a send.
///
/// Atomic under the registry lock: either every token is validated and
/// removed, or the call errors out and the registry is unchanged. This
/// keeps retries viable when the caller mixes a valid bunch with one
/// stale token.
pub fn consume_tokens(state: &AppState, tokens: &[String]) -> Result<Vec<std::path::PathBuf>> {
    let mut reg = state.attachments.lock().unwrap_or_else(|e| e.into_inner());
    for t in tokens {
        if !reg.contains_key(t) {
            return Err(Error::Other(
                "Unknown or expired attachment token".to_string(),
            ));
        }
    }
    Ok(tokens
        .iter()
        .map(|t| reg.remove(t).expect("contains_key checked above"))
        .collect())
}

/// Release the given tokens, ignoring any that are unknown. Useful for
/// best-effort cleanup (e.g. after a successful send has persisted the
/// message bytes elsewhere). Unlike `consume_tokens`, this does not
/// fail on stale tokens.
pub fn release_tokens(state: &AppState, tokens: &[String]) {
    let mut reg = state.attachments.lock().unwrap_or_else(|e| e.into_inner());
    for t in tokens {
        reg.remove(t);
    }
}

/// Look up the given tokens *without* removing them. Used by drafts:
/// the user may save a draft and keep editing, so we must not consume
/// the registry entry on every save.
pub fn peek_tokens(state: &AppState, tokens: &[String]) -> Result<Vec<std::path::PathBuf>> {
    let reg = state.attachments.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::with_capacity(tokens.len());
    for t in tokens {
        let path = reg
            .get(t)
            .cloned()
            .ok_or_else(|| Error::Other("Unknown or expired attachment token".to_string()))?;
        out.push(path);
    }
    Ok(out)
}
