//! Attachment picking via opaque tokens.
//!
//! Previously the renderer opened the file dialog itself, received
//! absolute paths, and shipped those paths back to the backend on send.
//! A compromised renderer (e.g. via HTML mail sandbox escape) could
//! therefore stage reads of any local file the OS user can see.
//!
//! The backend now owns the dialog: `pick_attachments` opens the native
//! dialog, canonicalises each chosen path, stores the mapping in an
//! in-memory token registry, and returns opaque tokens. Send and draft
//! flows use `peek_tokens` to validate tokens before building attachment
//! data. A send releases its tokens only after the message is persisted
//! to the outbox, preserving retries while rejecting unknown tokens.

use serde::Serialize;
use tauri::State;
use tauri_plugin_dialog::DialogExt;

use crate::error::{Error, Result};
use crate::state::AppState;

/// Opaque handle returned to the renderer after a successful pick.
/// Contains no path information; the name lets the compose UI render a
/// chip without needing the raw path.
#[derive(Debug, Serialize)]
pub struct AttachmentHandle {
    pub token: String,
    pub name: String,
}

/// Open a native file-picker dialog, register each selected file under
/// a random token, and return the handles.
///
/// The token is a v4 UUID. The backend stores `token -> canonical_path`
/// in `AppState::attachments`. Later send/save flows resolve tokens via
/// `peek_tokens` and pass the resulting paths into
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
    let mut resolved = Vec::with_capacity(paths.len());
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

        resolved.push(canonical);
    }

    let mut handles = Vec::with_capacity(resolved.len());
    let mut reg = state.attachments.lock().unwrap_or_else(|e| e.into_inner());
    for canonical in resolved {
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

        handles.push(AttachmentHandle { token, name });
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

/// Release the given tokens, ignoring any that are unknown. This is
/// cleanup only: send flows validate with `peek_tokens`, build the
/// attachment data, and call this only after successful outbox
/// persistence so earlier failures remain retryable.
pub fn release_tokens(state: &AppState, tokens: &[String]) {
    let mut reg = state.attachments.lock().unwrap_or_else(|e| e.into_inner());
    for t in tokens {
        reg.remove(t);
    }
}

/// Validate and resolve the given tokens *without* removing them.
/// Unknown tokens are rejected so the renderer cannot request arbitrary
/// paths. Send and draft flows use the returned paths to build attachment
/// data while retaining the registry entries through any fallible work.
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

#[cfg(test)]
mod tests {
    use super::{peek_tokens, release_tokens};
    use crate::state::AppState;

    fn attachment_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().expect("temporary app directory");
        let state = AppState::new(dir.path().to_path_buf()).expect("app state");
        (dir, state)
    }

    #[test]
    fn peek_rejects_unknown_tokens_without_consuming_valid_ones() {
        let (_dir, state) = attachment_state();
        let token = "known-token".to_string();
        let path = state.data_dir.join("picked-file");
        state
            .attachments
            .lock()
            .unwrap()
            .insert(token.clone(), path.clone());

        let error = peek_tokens(&state, &[token.clone(), "forged-token".to_string()])
            .expect_err("forged token must be rejected");

        assert_eq!(error.to_string(), "Unknown or expired attachment token");
        assert_eq!(peek_tokens(&state, &[token]).unwrap(), vec![path]);
    }

    #[test]
    fn release_cleans_up_peeked_tokens_only() {
        let (_dir, state) = attachment_state();
        let released_token = "released-token".to_string();
        let retained_token = "retained-token".to_string();
        let released_path = state.data_dir.join("released-file");
        let retained_path = state.data_dir.join("retained-file");
        {
            let mut registry = state.attachments.lock().unwrap();
            registry.insert(released_token.clone(), released_path.clone());
            registry.insert(retained_token.clone(), retained_path.clone());
        }

        assert_eq!(
            peek_tokens(&state, std::slice::from_ref(&released_token)).unwrap(),
            vec![released_path]
        );
        release_tokens(
            &state,
            &[released_token.clone(), "unknown-token".to_string()],
        );

        assert!(peek_tokens(&state, &[released_token]).is_err());
        assert_eq!(
            peek_tokens(&state, &[retained_token]).unwrap(),
            vec![retained_path]
        );
    }
}
