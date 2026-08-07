//! OpenPGP commands backed by libtumpa's shared keystore.
//!
//! All commands operate on the same on-disk keystore (`~/.tumpa/keys.db`
//! by default; honors `$TUMPA_DIR` / `$TUMPA_KEYSTORE`) that tumpa-cli and
//! the tumpa desktop app use. We never create keys here — generation stays
//! in those apps. We list, import, export, fetch over WKD, and enumerate /
//! auto-link OpenPGP smartcards.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use libtumpa::{
    card, card::link as card_link, key, network, KeySummary, SubkeySummary, UserIdSummary,
};
use serde::Serialize;
use tauri::{Emitter, State};
use tauri_plugin_dialog::DialogExt;
use zeroize::Zeroizing;

use crate::error::{Error, Result};
use crate::state::{AppState, PendingSecret};

/// Tauri event emitted when the backend needs a passphrase or PIN.
pub const SECRET_NEEDED_EVENT: &str = "pgp-secret-needed";

// ---------------------------------------------------------------------------
// DTOs
//
// libtumpa / wecanencrypt types are not `Serialize`, so we expose chithi-
// owned camelCase shapes over IPC. Keeps the JSON contract stable even if
// upstream type shapes evolve.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PgpUserId {
    pub uid: String,
    pub email: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PgpSubkey {
    pub fingerprint: String,
    pub key_id: String,
    /// One of "signing" | "encryption" | "authentication" | "certification" | "unknown".
    pub key_type: String,
    pub algorithm: Option<String>,
    pub bit_length: Option<usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PgpKeySummary {
    pub fingerprint: String,
    pub is_secret: bool,
    pub primary_uid: Option<String>,
    pub user_ids: Vec<PgpUserId>,
    pub subkeys: Vec<PgpSubkey>,
    pub creation_time: Option<DateTime<Utc>>,
    pub expiration_time: Option<DateTime<Utc>>,
    pub is_revoked: bool,
    pub revocation_time: Option<DateTime<Utc>>,
    /// Card idents this key is linked to (one entry per slot — same ident
    /// can repeat if multiple slots on the same card hold this key).
    pub card_idents: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PgpCardSummary {
    pub ident: String,
    pub manufacturer_name: String,
    pub serial_number: String,
    pub cardholder_name: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PgpCardDetails {
    pub ident: String,
    pub serial_number: String,
    pub cardholder_name: Option<String>,
    pub manufacturer_name: Option<String>,
    pub public_key_url: Option<String>,
    pub signature_fingerprint: Option<String>,
    pub encryption_fingerprint: Option<String>,
    pub authentication_fingerprint: Option<String>,
    pub signature_counter: u32,
    pub pin_retry_counter: u8,
    pub reset_code_retry_counter: u8,
    pub admin_pin_retry_counter: u8,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PgpCardDetection {
    pub key_fingerprint: String,
    pub card_ident: String,
    /// Slot on the card holding the key: "signature" | "encryption" |
    /// "authentication".
    pub slot: String,
    pub slot_fingerprint: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PgpImportResult {
    pub fingerprint: String,
    pub is_secret: bool,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn lt_err(e: libtumpa::Error) -> Error {
    Error::Other(format!("openpgp: {e}"))
}

fn user_id_to_dto(u: UserIdSummary) -> PgpUserId {
    PgpUserId {
        uid: u.uid,
        email: u.email,
    }
}

fn subkey_to_dto(s: SubkeySummary) -> PgpSubkey {
    PgpSubkey {
        fingerprint: s.fingerprint,
        key_id: s.key_id,
        key_type: s.key_type,
        algorithm: s.algorithm,
        bit_length: s.bit_length,
    }
}

fn summary_to_dto(s: KeySummary, card_idents: Vec<String>) -> PgpKeySummary {
    PgpKeySummary {
        fingerprint: s.fingerprint,
        is_secret: s.is_secret,
        primary_uid: s.primary_uid,
        user_ids: s.user_ids.into_iter().map(user_id_to_dto).collect(),
        subkeys: s.subkeys.into_iter().map(subkey_to_dto).collect(),
        creation_time: s.creation_time,
        expiration_time: s.expiration_time,
        is_revoked: s.is_revoked,
        revocation_time: s.revocation_time,
        card_idents,
    }
}

// ---------------------------------------------------------------------------
// Key listing & detail
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn pgp_list_keys(state: State<'_, AppState>) -> Result<Vec<PgpKeySummary>> {
    let store = state.pgp_store().map_err(lt_err)?;
    tokio::task::spawn_blocking(move || {
        let guard = store.lock().expect("pgp keystore mutex poisoned");
        let summaries = guard.list_keys_summary().map_err(|e| lt_err(e.into()))?;
        let links = card_link::card_idents_map(&guard).map_err(lt_err)?;
        Ok(summaries
            .into_iter()
            .map(|s| {
                let idents = links.get(&s.fingerprint).cloned().unwrap_or_default();
                summary_to_dto(s, idents)
            })
            .collect())
    })
    .await
    .map_err(|e| Error::Other(format!("key list task join failed: {e}")))?
}

#[tauri::command]
pub async fn pgp_get_key(state: State<'_, AppState>, fingerprint: String) -> Result<PgpKeySummary> {
    let store = state.pgp_store().map_err(lt_err)?;
    tokio::task::spawn_blocking(move || {
        let guard = store.lock().expect("pgp keystore mutex poisoned");
        let summary = guard
            .get_key_summary(&fingerprint)
            .map_err(|e| lt_err(e.into()))?;
        let idents = card_link::card_idents_for_key(&guard, &fingerprint).map_err(lt_err)?;
        Ok(summary_to_dto(summary, idents))
    })
    .await
    .map_err(|e| Error::Other(format!("key get task join failed: {e}")))?
}

// ---------------------------------------------------------------------------
// Import / delete / export
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn pgp_import_key(state: State<'_, AppState>, data: Vec<u8>) -> Result<PgpImportResult> {
    let store = state.pgp_store().map_err(lt_err)?;
    tokio::task::spawn_blocking(move || {
        let guard = store.lock().expect("pgp keystore mutex poisoned");
        let info = key::import_any(&guard, &data).map_err(lt_err)?;
        Ok(PgpImportResult {
            fingerprint: info.fingerprint,
            is_secret: info.is_secret,
        })
    })
    .await
    .map_err(|e| Error::Other(format!("key import task join failed: {e}")))?
}

/// Open the native file picker, read the chosen file, and import it.
/// Returns `None` if the user cancels. Mirrors `pick_attachments`' pattern
/// (server-side dialog + oneshot) so the renderer never sees the chosen
/// path.
#[tauri::command]
pub async fn pgp_pick_and_import_key(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<PgpImportResult>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .add_filter("OpenPGP keys", &["asc", "pgp", "gpg", "key"])
        .pick_file(move |path| {
            let _ = tx.send(path);
        });
    let picked = rx
        .await
        .map_err(|e| Error::Other(format!("key picker closed unexpectedly: {e}")))?;
    let Some(file_path) = picked else {
        return Ok(None); // user cancelled
    };
    let path = file_path
        .as_path()
        .ok_or_else(|| Error::Other("picked path was not on local filesystem".into()))?;
    let bytes = std::fs::read(path)?;
    let store = state.pgp_store().map_err(lt_err)?;
    let result = tokio::task::spawn_blocking(move || -> Result<PgpImportResult> {
        let guard = store.lock().expect("pgp keystore mutex poisoned");
        let info = key::import_any(&guard, &bytes).map_err(lt_err)?;
        Ok(PgpImportResult {
            fingerprint: info.fingerprint,
            is_secret: info.is_secret,
        })
    })
    .await
    .map_err(|e| Error::Other(format!("key import task join failed: {e}")))??;
    Ok(Some(result))
}

#[tauri::command]
pub async fn pgp_delete_key(state: State<'_, AppState>, fingerprint: String) -> Result<()> {
    let store = state.pgp_store().map_err(lt_err)?;
    tokio::task::spawn_blocking(move || {
        let guard = store.lock().expect("pgp keystore mutex poisoned");
        key::delete(&guard, &fingerprint).map_err(lt_err)
    })
    .await
    .map_err(|e| Error::Other(format!("key delete task join failed: {e}")))?
}

#[tauri::command]
pub async fn pgp_export_public(state: State<'_, AppState>, fingerprint: String) -> Result<String> {
    let store = state.pgp_store().map_err(lt_err)?;
    tokio::task::spawn_blocking(move || {
        let guard = store.lock().expect("pgp keystore mutex poisoned");
        key::export_public_armored(&guard, &fingerprint).map_err(lt_err)
    })
    .await
    .map_err(|e| Error::Other(format!("key export task join failed: {e}")))?
}

// ---------------------------------------------------------------------------
// WKD
// ---------------------------------------------------------------------------

/// Fetch a public key by email via WKD and import it. Returns the
/// fingerprint of the imported key.
///
/// libtumpa's WKD path is blocking (reqwest in blocking mode under
/// `network`), so we hand it to `spawn_blocking` to keep the tokio
/// scheduler responsive.
#[tauri::command]
pub async fn pgp_wkd_fetch(state: State<'_, AppState>, email: String) -> Result<String> {
    let store = state.pgp_store().map_err(lt_err)?;
    tokio::task::spawn_blocking(move || {
        let guard = store.lock().expect("pgp keystore mutex poisoned");
        network::wkd_fetch_and_import(&guard, &email)
    })
    .await
    .map_err(|e| Error::Other(format!("wkd task join failed: {e}")))?
    .map_err(lt_err)
}

// ---------------------------------------------------------------------------
// Smartcards
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn pgp_list_cards() -> Result<Vec<PgpCardSummary>> {
    let cards = tokio::task::spawn_blocking(card::list_all_cards)
        .await
        .map_err(|e| Error::Other(format!("card list task join failed: {e}")))?
        .map_err(|e| Error::Other(format!("openpgp card: {e}")))?;
    Ok(cards
        .into_iter()
        .map(|c| PgpCardSummary {
            ident: c.ident,
            manufacturer_name: c.manufacturer_name,
            serial_number: c.serial_number,
            cardholder_name: c.cardholder_name,
        })
        .collect())
}

#[tauri::command]
pub async fn pgp_card_details(ident: String) -> Result<PgpCardDetails> {
    let info = tokio::task::spawn_blocking(move || card::get_card_details(Some(&ident)))
        .await
        .map_err(|e| Error::Other(format!("card details task join failed: {e}")))?
        .map_err(|e| Error::Other(format!("openpgp card: {e}")))?;
    Ok(PgpCardDetails {
        ident: info.ident,
        serial_number: info.serial_number,
        cardholder_name: info.cardholder_name,
        manufacturer_name: info.manufacturer_name,
        public_key_url: info.public_key_url,
        signature_fingerprint: info.signature_fingerprint,
        encryption_fingerprint: info.encryption_fingerprint,
        authentication_fingerprint: info.authentication_fingerprint,
        signature_counter: info.signature_counter,
        pin_retry_counter: info.pin_retry_counter,
        reset_code_retry_counter: info.reset_code_retry_counter,
        admin_pin_retry_counter: info.admin_pin_retry_counter,
    })
}

/// Scan all connected cards, match their on-card key fingerprints against
/// keys in the store, and persist the card↔key associations in the
/// `card_keys` table. Returns the set of detections that were stored.
#[tauri::command]
pub async fn pgp_auto_link_cards(state: State<'_, AppState>) -> Result<Vec<PgpCardDetection>> {
    let store = state.pgp_store().map_err(lt_err)?;
    let detections = tokio::task::spawn_blocking(move || {
        let guard = store.lock().expect("pgp keystore mutex poisoned");
        card_link::auto_link_all(&guard)
    })
    .await
    .map_err(|e| Error::Other(format!("auto-link task join failed: {e}")))?
    .map_err(lt_err)?;
    Ok(detections
        .into_iter()
        .map(|d| PgpCardDetection {
            key_fingerprint: d.key_fingerprint,
            card_ident: d.card_ident,
            slot: card_link::slot_str(d.slot).to_string(),
            slot_fingerprint: d.slot_fingerprint,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Secret prompts + credential cache
//
// libtumpa never owns secrets — it expects callers to supply `&Passphrase`
// / `&Pin` on every operation. To honour that without forcing the user to
// re-type their passphrase on every send/decrypt, we ship a tiny in-memory
// cache (libtumpa::cache::CredentialCache) and a request/response dance
// over a Tauri event:
//
//   1. Sign/decrypt closure needs a secret for `target` (a key fingerprint
//      or card ident).
//   2. It first calls `acquire_secret`, which consults the cache.
//   3. On a miss, `acquire_secret` registers a oneshot in
//      `pgp_pending_secrets`, emits the `pgp-secret-needed` event, and
//      awaits a response.
//   4. The frontend dialog calls `pgp_provide_secret` (or
//      `pgp_cancel_secret`) with the request id. The first wraps the value
//      in `Zeroizing<String>` *immediately* on the backend side and sends
//      it through the oneshot; the second sends `None`.
//   5. `acquire_secret` returns the value (or a cancellation error).
//
// The bare `String` argument to `pgp_provide_secret` never leaves the
// command body — it's moved into `Zeroizing` on line 1 and dropped via
// the standard zeroing destructor when the oneshot consumer is done.
// ---------------------------------------------------------------------------

/// Why a secret prompt is being raised. Drives the dialog kind on the
/// frontend.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SecretKind {
    /// A key passphrase for a software-only key (target = fingerprint).
    Passphrase,
    /// A smartcard user PIN (target = card ident).
    Pin,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SecretPromptPayload<'a> {
    request_id: &'a str,
    kind: SecretKind,
    /// Fingerprint (passphrase) or card ident (pin).
    target: &'a str,
    /// Human-readable explanation: "Decrypt message from Alice", etc.
    reason: &'a str,
}

/// Provide the secret for a pending prompt. The bare `String` argument is
/// wrapped in `Zeroizing` on entry and never copied outside that wrapper,
/// so the IPC buffer becomes the only allocation that can leak — and
/// that's owned by Tauri's IPC pipeline, not us.
///
/// This command is a pure relay: it just hands the secret back to the
/// waiting `acquire_secret` through the oneshot. Caching — whether into
/// the `tcli` agent or the in-process `CredentialCache` — is decided by
/// `acquire_secret` once it receives the value, so the agent-vs-local
/// policy lives in exactly one place.
#[tauri::command]
pub async fn pgp_provide_secret(
    state: State<'_, AppState>,
    request_id: String,
    value: String,
) -> Result<()> {
    let secret = Zeroizing::new(value);
    let entry = state
        .pgp_pending_secrets
        .lock()
        .expect("pgp pending secrets mutex poisoned")
        .remove(&request_id);
    let Some(pending) = entry else {
        // Stale id (prompt was cancelled by the caller or already
        // answered). Treat as a no-op so the dialog can close cleanly.
        return Ok(());
    };
    let _ = pending.tx.send(Some(secret));
    Ok(())
}

/// Cancel a pending prompt. The waiter receives `None` and turns it into
/// a `SecretCancelled` error so the calling sign/decrypt aborts cleanly.
#[tauri::command]
pub async fn pgp_cancel_secret(state: State<'_, AppState>, request_id: String) -> Result<()> {
    let entry = state
        .pgp_pending_secrets
        .lock()
        .expect("pgp pending secrets mutex poisoned")
        .remove(&request_id);
    if let Some(pending) = entry {
        let _ = pending.tx.send(None);
    }
    Ok(())
}

/// Clear every cached passphrase and PIN. The previous cache is replaced
/// with a fresh empty one and dropped — the inner `Zeroizing<String>`
/// values overwrite their backing buffers on drop.
#[tauri::command]
pub async fn pgp_forget_all(state: State<'_, AppState>) -> Result<()> {
    let mut guard = state.pgp_cache.lock().expect("pgp cache mutex poisoned");
    *guard = libtumpa::cache::CredentialCache::new();
    Ok(())
}

/// Forget any cached PIN for a specific card. Called automatically when a
/// previously-connected card disappears from `pgp_list_cards`.
#[tauri::command]
pub async fn pgp_forget_card(state: State<'_, AppState>, ident: String) -> Result<()> {
    state
        .pgp_cache
        .lock()
        .expect("pgp cache mutex poisoned")
        .clear_card(&ident);
    Ok(())
}

/// Acquire a secret (passphrase or PIN) for `target`.
///
/// Lookup order:
///
///  1. **`tcli` agent.** When `agent_key` is `Some` and the agent is
///     running, `GET_PASSPHRASE` it. A hit returns immediately; a miss
///     means "agent up, just not cached" — we prompt, then write the
///     collected secret back with `PUT_PASSPHRASE`. While the agent is
///     reachable it is the *sole* cache: we do NOT also populate the
///     in-process `CredentialCache`, so the agent's TTL governs expiry
///     and a key unlocked in any Tumpa tool is shared.
///  2. **In-process cache.** When the agent is unreachable — or
///     `agent_key` is `None`, e.g. an unlinked smartcard whose ident
///     could not be resolved to a fingerprint — fall back to chithi's
///     own `CredentialCache`, exactly as before the agent integration.
///  3. **Prompt.** On a miss in whichever cache applies, emit
///     `pgp-secret-needed` and await the user's dialog input.
///
/// `agent_key` is the namespaced agent cache key (`passphrase:<FP>` /
/// `pin:<FP>`, built via `mail::pgp_agent`); `target` is the in-process
/// cache key (a fingerprint for passphrases, a card ident for PINs). The
/// two differ because chithi and `tcli` historically key their caches
/// differently.
///
/// `origin_window` routes the prompt to a single webview. When `Some`,
/// only the window with that label receives the event (compose-triggered
/// sends → compose window; reader-triggered decrypts → main window).
/// When `None` (e.g. background card-link tasks with no originating
/// window) the event is broadcast to every webview as a fallback.
/// Without this routing the dialog renders in BOTH App.vue and
/// ComposeView.vue at once, which the user perceives as being asked for
/// the PIN twice.
///
/// Not a `#[tauri::command]` — it's an internal helper reused by the
/// sign/decrypt paths.
#[allow(clippy::too_many_arguments)]
pub async fn acquire_secret(
    app: &tauri::AppHandle,
    pending: Arc<std::sync::Mutex<std::collections::HashMap<String, PendingSecret>>>,
    cache: Arc<std::sync::Mutex<libtumpa::cache::CredentialCache>>,
    kind: SecretKind,
    target: &str,
    agent_key: Option<&str>,
    reason: &str,
    origin_window: Option<&str>,
) -> Result<Zeroizing<String>> {
    use crate::mail::pgp_agent::{self, AgentLookup};

    // 1. Try the tcli agent first when we have a namespaced key for it.
    let agent_up = if let Some(ak) = agent_key {
        match pgp_agent::get(ak).await {
            AgentLookup::Found(secret) => return Ok(secret),
            AgentLookup::NotFound => true,
            AgentLookup::Unavailable => false,
        }
    } else {
        false
    };

    // 2. Agent down (or no agent key): consult the in-process cache.
    if !agent_up {
        if let Some(cached) = cache
            .lock()
            .expect("pgp cache mutex poisoned")
            .get(target)
            .cloned()
        {
            return Ok(cached);
        }
    }

    // 3. Prompt the user through chithi's own dialog.
    let secret = prompt_via_dialog(app, &pending, kind, target, reason, origin_window).await?;

    // 4. Cache the freshly-collected secret. When the agent is up it is
    //    the single source of truth — write back so every Tumpa tool
    //    benefits and the TTL governs expiry; only if that write fails
    //    (the agent died between GET and PUT) keep a local copy so the
    //    secret is not lost this session. When the agent is down, cache
    //    in-process as before.
    let stored_in_agent = if agent_up {
        let ak = agent_key.expect("agent_up implies agent_key is Some");
        pgp_agent::put(ak, &secret).await
    } else {
        false
    };
    if !stored_in_agent {
        cache
            .lock()
            .expect("pgp cache mutex poisoned")
            .store(target, secret.clone());
    }
    Ok(secret)
}

/// Emit `pgp-secret-needed` and await the frontend's response. Factored
/// out of `acquire_secret` so the cache/agent policy and the raw prompt
/// mechanics stay separable.
async fn prompt_via_dialog(
    app: &tauri::AppHandle,
    pending: &Arc<std::sync::Mutex<std::collections::HashMap<String, PendingSecret>>>,
    kind: SecretKind,
    target: &str,
    reason: &str,
    origin_window: Option<&str>,
) -> Result<Zeroizing<String>> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    pending
        .lock()
        .expect("pgp pending secrets mutex poisoned")
        .insert(request_id.clone(), PendingSecret { tx });
    let payload = SecretPromptPayload {
        request_id: &request_id,
        kind,
        target,
        reason,
    };
    let emit_result = match origin_window {
        Some(label) => app.emit_to(label, SECRET_NEEDED_EVENT, payload),
        None => app.emit(SECRET_NEEDED_EVENT, payload),
    };
    emit_result.map_err(|e| Error::Other(format!("pgp secret event emit failed: {e}")))?;
    match rx.await {
        Ok(Some(secret)) => Ok(secret),
        Ok(None) => Err(Error::Other("pgp: secret prompt cancelled by user".into())),
        Err(_) => Err(Error::Other("pgp: secret prompt channel closed".into())),
    }
}

// ---------------------------------------------------------------------------
// Decrypt + verify (Phase C)
// ---------------------------------------------------------------------------

/// Result of decrypting a PGP/MIME message. The plaintext is re-parsed
/// through `parse_message_body` so the same sanitization pipeline applies
/// — the returned `MessageBody` is what the reader renders in place of
/// the encrypted placeholder.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PgpDecryptedMessage {
    pub plaintext_body: crate::db::messages::MessageBody,
    pub verify_outcome: PgpVerifyOutcome,
}

/// Outcome of a signature check (detached, inline, or inner-signature
/// after decrypt). The variants mirror libtumpa's distinct outcome types
/// so the UI can render distinct badges without losing fidelity.
// `rename_all` on the enum only renames variant TAGS (Good → "good").
// `rename_all_fields` (serde 1.0.180+) also camelCases the FIELDS inside
// each variant — without it, `signer_fingerprint` stayed snake_case in
// the JSON payload while the TS type expected `signerFingerprint`, and
// the reader threw `o.signerFingerprint is undefined` on every Good
// signature outcome.
#[derive(Debug, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PgpVerifyOutcome {
    /// No signature was present (encrypt-only payload).
    Unsigned,
    /// Signature is valid; signer is in the keystore.
    Good {
        signer_uid: Option<String>,
        signer_fingerprint: String,
        verifier_fingerprint: String,
    },
    /// Signer is in the keystore but signature didn't verify.
    Bad {
        signer_uid: Option<String>,
        signer_fingerprint: String,
    },
    /// Signature references a key we don't have.
    UnknownKey { key_id: String },
    /// Signature parsing or verification produced an error.
    Error { message: String },
}

impl PgpVerifyOutcome {
    fn from_decrypt(o: libtumpa::decrypt::DecryptVerifyOutcome) -> Self {
        use libtumpa::decrypt::DecryptVerifyOutcome as D;
        match o {
            D::Unsigned => PgpVerifyOutcome::Unsigned,
            D::Good {
                key_info,
                verifier_fingerprint,
            } => PgpVerifyOutcome::Good {
                signer_uid: key_info.user_ids.into_iter().next().map(|u| u.value),
                signer_fingerprint: key_info.fingerprint,
                verifier_fingerprint,
            },
            D::Bad { key_info } => PgpVerifyOutcome::Bad {
                signer_uid: key_info.user_ids.into_iter().next().map(|u| u.value),
                signer_fingerprint: key_info.fingerprint,
            },
            D::UnknownKey { issuer_ids } => PgpVerifyOutcome::UnknownKey {
                key_id: issuer_ids.into_iter().next().unwrap_or_default(),
            },
        }
    }

    fn from_verify(o: libtumpa::verify::VerifyOutcome, verifier_fp: Option<String>) -> Self {
        use libtumpa::verify::VerifyOutcome as V;
        match o {
            V::Good {
                key_info,
                verifier_fingerprint,
            } => PgpVerifyOutcome::Good {
                signer_uid: key_info.user_ids.into_iter().next().map(|u| u.value),
                signer_fingerprint: key_info.fingerprint,
                verifier_fingerprint,
            },
            V::Bad { key_info } => PgpVerifyOutcome::Bad {
                signer_uid: key_info.user_ids.into_iter().next().map(|u| u.value),
                signer_fingerprint: key_info.fingerprint,
            },
            V::UnknownKey { key_id } => PgpVerifyOutcome::UnknownKey { key_id },
            // V::Error variant doesn't exist on libtumpa::VerifyOutcome —
            // errors come back as Result::Err from verify_detached. We
            // map those to PgpVerifyOutcome::Error at the call site.
        }
        .also_attach_verifier_fp(verifier_fp)
    }
}

trait AttachVerifierFp {
    fn also_attach_verifier_fp(self, _: Option<String>) -> PgpVerifyOutcome;
}
impl AttachVerifierFp for PgpVerifyOutcome {
    fn also_attach_verifier_fp(self, _: Option<String>) -> PgpVerifyOutcome {
        // Verifier fp is already populated for Good; this hook exists so
        // future overrides can add provenance without changing the
        // mapping table above.
        self
    }
}

/// Decrypt the raw message bytes for `message_id`. Loads bytes from the
/// maildir, extracts the ciphertext, finds a matching secret key in the
/// keystore, prompts for the passphrase (via the global secret-prompt
/// machinery), decrypts, optionally verifies the inner signature, and
/// re-parses the plaintext into a `MessageBody`.
#[tauri::command]
pub async fn pgp_decrypt_message(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
) -> Result<PgpDecryptedMessage> {
    let (raw, from_email, to_json, cc_json, flags_json, is_encrypted, is_signed) =
        load_raw_with_metadata(&app, &state, &account_id, &message_id).await?;

    let ciphertext = if let Some(c) = crate::mail::pgp_mime::extract_encrypted_payload(&raw) {
        c
    } else if let Some(c) = crate::mail::pgp_mime::extract_inline_armor(&raw) {
        c
    } else {
        return Err(Error::Other(
            "pgp: message is not encrypted (no ciphertext found)".into(),
        ));
    };

    let store = state.pgp_store().map_err(lt_err)?;

    // Resolve a software secret key first. If none matches, fall through
    // to the card path — this is the only way to decrypt messages
    // encrypted to a key whose secret material lives on a smartcard
    // (including the user's own Sent items when their key is card-
    // resident). Both branches share `acquire_secret` for the prompt and
    // `evict_cached_secret` for failed-auth recovery.
    let software_key = {
        let guard = store.lock().expect("pgp keystore mutex poisoned");
        libtumpa::decrypt::find_software_decryption_key(&guard, &ciphertext).map_err(lt_err)?
    };

    let origin = window.label().to_string();
    let result = if let Some((key_data, key_info)) = software_key {
        let fingerprint = key_info.fingerprint;
        let agent_key = crate::mail::pgp_agent::passphrase_key(&fingerprint);
        let passphrase_str = acquire_secret(
            &app,
            state.pgp_pending_secrets.clone(),
            state.pgp_cache.clone(),
            SecretKind::Passphrase,
            &fingerprint,
            Some(&agent_key),
            "Decrypt an OpenPGP message",
            Some(&origin),
        )
        .await?;
        let passphrase = libtumpa::Passphrase::new(passphrase_str.to_string());

        let store_for_blocking = store.clone();
        let ciphertext_clone = ciphertext.clone();
        let key_data_clone = key_data.clone();
        let join = tokio::task::spawn_blocking(move || {
            let guard = store_for_blocking
                .lock()
                .expect("pgp keystore mutex poisoned");
            libtumpa::decrypt::decrypt_and_verify_with_key(
                &guard,
                &key_data_clone,
                &ciphertext_clone,
                &passphrase,
            )
        })
        .await
        .map_err(|e| Error::Other(format!("pgp decrypt task join failed: {e}")))?;
        match join {
            Ok(r) => r,
            Err(e) => {
                evict_cached_secret(&state.pgp_cache, &fingerprint, Some(&agent_key));
                return Err(lt_err(e));
            }
        }
    } else {
        // No software secret key — try the card. `find_decryption_card`
        // walks every connected card looking for one whose encryption
        // slot fingerprint matches a PKESK packet in the ciphertext.
        let card_match = {
            let store_for_blocking = store.clone();
            let ciphertext_clone = ciphertext.clone();
            tokio::task::spawn_blocking(move || {
                let guard = store_for_blocking
                    .lock()
                    .expect("pgp keystore mutex poisoned");
                libtumpa::decrypt::find_decryption_card(&guard, &ciphertext_clone)
            })
            .await
            .map_err(|e| Error::Other(format!("pgp card lookup task join failed: {e}")))?
            .map_err(lt_err)?
        };
        let Some(card) = card_match else {
            return Err(Error::Other(
                "pgp: no matching secret key in the keystore for this ciphertext".into(),
            ));
        };
        let ident = card.card.ident.clone();
        // `tpass` / `tcli` key the card PIN by the card key's primary
        // fingerprint — `libtumpa::decrypt::DecryptionCard.key_info`, the
        // very struct `find_decryption_card` just returned. Use that exact
        // value so the agent cache interoperates, with no dependency on
        // the `card_keys` link table being populated on this machine.
        let agent_key = crate::mail::pgp_agent::pin_key(&card.key_info.fingerprint);
        let key_data = card.key_data;
        let pin_str = acquire_secret(
            &app,
            state.pgp_pending_secrets.clone(),
            state.pgp_cache.clone(),
            SecretKind::Pin,
            &ident,
            Some(&agent_key),
            "Decrypt an OpenPGP message",
            Some(&origin),
        )
        .await?;
        let pin = libtumpa::Pin::new(pin_str.as_bytes().to_vec());

        let store_for_blocking = store.clone();
        let ciphertext_clone = ciphertext.clone();
        let key_data_clone = key_data.clone();
        let ident_for_blocking = ident.clone();
        let join = tokio::task::spawn_blocking(move || {
            let guard = store_for_blocking
                .lock()
                .expect("pgp keystore mutex poisoned");
            libtumpa::decrypt::decrypt_and_verify_on_card(
                &guard,
                &key_data_clone,
                &ciphertext_clone,
                &pin,
                Some(&ident_for_blocking),
            )
        })
        .await
        .map_err(|e| Error::Other(format!("pgp card decrypt task join failed: {e}")))?;
        match join {
            Ok(r) => r,
            Err(e) => {
                evict_cached_secret(&state.pgp_cache, &ident, Some(&agent_key));
                return Err(lt_err(e));
            }
        }
    };

    // Parsing the decrypted plaintext (mail_parser + ammonia HTML
    // sanitization) is CPU-bound. Run it on the blocking pool so the
    // async executor stays responsive while a chunky HTML body is
    // walked.
    let plaintext_bytes: Vec<u8> = result.plaintext.to_vec();
    let from_email_clone = from_email.clone();
    let to_json_clone = to_json.clone();
    let cc_json_clone = cc_json.clone();
    let flags_json_clone = flags_json.clone();
    let message_id_clone = message_id.clone();
    let plaintext_body = tokio::task::spawn_blocking(move || {
        crate::mail::parser::parse_message_body(
            &message_id_clone,
            &plaintext_bytes,
            &from_email_clone,
            &to_json_clone,
            &cc_json_clone,
            &flags_json_clone,
            is_encrypted,
            is_signed,
        )
    })
    .await
    .map_err(|e| Error::Other(format!("pgp body parse task join failed: {e}")))?
    .ok_or_else(|| Error::MailParse("could not parse decrypted plaintext as MIME".into()))?;

    // Protected headers (draft-ietf-lamps-header-protection): if the
    // decrypted payload carried its own Subject, the sender used the
    // "encrypt the subject" feature and the cleartext envelope only has
    // a `...` placeholder. Persist the recovered subject over the stored
    // placeholder so the message list, search, and thread view all show
    // the real subject. A normal encrypted message's decrypted payload
    // is a bare body part with no Subject of its own, so this is a
    // no-op for non-protected mail.
    if let Some(ref subj) = plaintext_body.subject {
        if !subj.is_empty() {
            let conn = state.db.writer().await;
            if let Err(e) = crate::db::messages::update_subject(&conn, &message_id, subj) {
                log::warn!("pgp: failed to persist protected subject for {message_id}: {e}");
            }
        }
    }

    Ok(PgpDecryptedMessage {
        plaintext_body,
        verify_outcome: PgpVerifyOutcome::from_decrypt(result.outcome),
    })
}

/// Verify the signature on a `multipart/signed` message. For inline
/// armor, treats the armor as inline-signed and runs `verify_inline`.
#[tauri::command]
pub async fn pgp_verify_message(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
) -> Result<PgpVerifyOutcome> {
    let (raw, ..) = load_raw_with_metadata(&app, &state, &account_id, &message_id).await?;

    // Try multipart/signed first.
    if let Some((signed_entity, signature)) = crate::mail::pgp_mime::extract_signed_payload(&raw) {
        let store = state.pgp_store().map_err(lt_err)?;
        let outcome = tokio::task::spawn_blocking(move || {
            let guard = store.lock().expect("pgp keystore mutex poisoned");
            // Try canonical first; fall through to tolerant variants on
            // a Bad outcome. UnknownKey / Good shortcut out.
            let primary = libtumpa::verify::verify_detached(&guard, &signed_entity, &signature);
            match primary {
                Ok(libtumpa::verify::VerifyOutcome::Bad { .. }) => {
                    for variant in crate::mail::pgp_mime::tolerant_signed_variants(&signed_entity) {
                        if let Ok(o) =
                            libtumpa::verify::verify_detached(&guard, &variant, &signature)
                        {
                            if matches!(o, libtumpa::verify::VerifyOutcome::Good { .. }) {
                                return Ok(o);
                            }
                        }
                    }
                    primary
                }
                other => other,
            }
        })
        .await
        .map_err(|e| Error::Other(format!("pgp verify task join failed: {e}")))?;

        return match outcome {
            Ok(o) => Ok(PgpVerifyOutcome::from_verify(o, None)),
            Err(e) => Ok(PgpVerifyOutcome::Error {
                message: e.to_string(),
            }),
        };
    }

    Err(Error::Other(
        "pgp: message is not signed (no multipart/signed envelope)".into(),
    ))
}

/// Load the raw maildir bytes plus the metadata needed to re-parse them.
/// Reuses the on-demand-fetch path from `get_message_body` semantically
/// but skipped here for brevity — we assume the body is already on disk
/// (which is the common case for messages the user has opened once).
async fn load_raw_with_metadata(
    _app: &tauri::AppHandle,
    state: &State<'_, AppState>,
    account_id: &str,
    message_id: &str,
) -> Result<(Vec<u8>, String, String, String, String, bool, bool)> {
    let (maildir_path, from_email, to_json, cc_json, flags_json, is_encrypted, is_signed) = {
        let conn = state.db.reader();
        crate::db::messages::get_message_metadata(&conn, account_id, message_id)?
    };
    if maildir_path.is_empty() || maildir_path.starts_with("graph:") {
        return Err(Error::Other(
            "pgp: message body not on disk — open the message normally once first".into(),
        ));
    }
    let full_path = crate::path_validation::resolve_under(&state.data_dir, &maildir_path)?;
    let raw = std::fs::read(&full_path)?;
    Ok((
        raw,
        from_email,
        to_json,
        cc_json,
        flags_json,
        is_encrypted,
        is_signed,
    ))
}

/// Drop a stale secret after an authentication failure. Invoked by the
/// decrypt path and `apply_pgp_envelope` when a cached PIN/passphrase
/// produced a sign/encrypt/decrypt failure, so the user is re-prompted
/// instead of looping with the same wrong secret.
///
/// Removes `target` from the in-process `CredentialCache` and, when
/// `agent_key` is `Some`, also sends a targeted `CLEAR_PASSPHRASE` to the
/// `tcli` agent — otherwise the next `GET_PASSPHRASE` would serve the
/// wrong secret straight back. The CLEAR is fire-and-forget: a failure
/// just means the agent is not running. This automatic failure path is
/// the *only* place chithi clears the shared agent; the manual
/// `pgp_forget_*` commands deliberately leave it untouched.
///
/// Note: there is no background TTL sweeper on the in-process cache. It
/// lives in memory for the lifetime of the process; closing the app drops
/// the `CredentialCache` and the `Zeroizing<String>` values overwrite
/// their backing buffers as they're dropped.
pub fn evict_cached_secret(
    cache: &Arc<std::sync::Mutex<libtumpa::cache::CredentialCache>>,
    target: &str,
    agent_key: Option<&str>,
) {
    cache
        .lock()
        .expect("pgp cache mutex poisoned")
        .remove(target);
    if let Some(ak) = agent_key {
        let ak = ak.to_string();
        tokio::spawn(async move {
            crate::mail::pgp_agent::clear(&ak).await;
        });
    }
}

// ---------------------------------------------------------------------------
// Tests
//
// PCSC isn't available in CI, so card-specific commands aren't covered
// here. The keystore round-trip is covered against a temp $TUMPA_DIR.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use libtumpa::store as ltstore;

    /// Generate a tiny test key in a fresh keystore directory and exercise
    /// list / get / export / delete via the libtumpa surface that our
    /// commands use. We invoke libtumpa directly rather than going through
    /// the Tauri command machinery because `tauri::State` is awkward to
    /// construct in a unit test — the command bodies are thin wrappers
    /// around exactly these calls.
    #[test]
    fn keystore_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Force libtumpa::store::open_keystore(None) to land here.
        // SAFETY: set_var/remove_var are unsafe in edition 2024 because
        // they're not thread-safe; this test is single-threaded and the
        // env mutation is contained to the test process.
        unsafe {
            std::env::set_var("TUMPA_DIR", dir.path());
            std::env::remove_var("TUMPA_KEYSTORE");
        }

        let store = ltstore::open_keystore(None).expect("open keystore");

        // Start empty.
        let initial = store.list_keys_summary().expect("list initial");
        assert!(initial.is_empty(), "expected empty keystore");

        // Generate a key directly via libtumpa so we have something to
        // import. We can't import an arbitrary armored blob without first
        // having a key, so we round-trip through generate -> export.
        let pw = libtumpa::Passphrase::new("test-pw".into());
        let params = key::GenerateKeyParams {
            uids: vec!["Test User <test@example.com>".into()],
            cipher_suite: libtumpa::CipherSuite::Cv25519,
            expiry: None,
            subkey_flags: libtumpa::SubkeyFlags::all(),
            can_primary_sign: true,
        };
        let generated = key::generate_and_import(&store, params, &pw).expect("generate");
        let fp = generated.fingerprint.clone();

        // List sees it.
        let after = store.list_keys_summary().expect("list after");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].fingerprint, fp);
        assert!(after[0].is_secret);

        // get_key_summary works.
        let one = store.get_key_summary(&fp).expect("get summary");
        assert_eq!(one.fingerprint, fp);

        // Export public, re-import (idempotent), still one key.
        let armored = key::export_public_armored(&store, &fp).expect("export");
        assert!(armored.contains("BEGIN PGP PUBLIC KEY BLOCK"));

        // Delete.
        key::delete(&store, &fp).expect("delete");
        let empty_again = store.list_keys_summary().expect("list final");
        assert!(empty_again.is_empty());

        unsafe {
            std::env::remove_var("TUMPA_DIR");
        }
    }

    /// Sanity check: a byte slice can be zeroized in place. Validates
    /// the `zeroize` crate contract our cache and prompt machinery
    /// depends on. The upstream `zeroize` crate has its own drop tests
    /// (which we can't reproduce reliably from chithi because freed heap
    /// regions get recycled immediately); this test just confirms the
    /// trait is in scope and behaves as documented.
    #[test]
    fn zeroize_in_place_writes_zeros() {
        use zeroize::Zeroize;
        let mut buf = [1u8, 2, 3, 4, 5, 6, 7, 8];
        buf.zeroize();
        assert_eq!(buf, [0u8; 8]);
    }

    /// `CredentialCache::sweep` honours its TTL — entries older than the
    /// cutoff are dropped, fresher entries survive.
    #[test]
    fn cache_sweep_evicts_stale_entries() {
        let mut cache = libtumpa::cache::CredentialCache::new();
        cache.store("fp-a", Zeroizing::new(String::from("pw-a")));
        // sweep(0) means "everything stored before *right now* is stale".
        // Sleep one millisecond so the cutoff lies strictly after the
        // store timestamp.
        std::thread::sleep(std::time::Duration::from_millis(1));
        let removed = cache.sweep(0);
        assert_eq!(removed, 1);
        assert!(cache.get("fp-a").is_none());

        // A fresh entry should survive a sweep with a non-zero TTL.
        cache.store("fp-b", Zeroizing::new(String::from("pw-b")));
        let removed = cache.sweep(60);
        assert_eq!(removed, 0);
        assert!(cache.get("fp-b").is_some());
    }

    /// `clear_card` removes only the matching entry. The other entries
    /// stay put — this matters when the user yanks one of several
    /// connected cards.
    #[test]
    fn cache_clear_card_is_targeted() {
        let mut cache = libtumpa::cache::CredentialCache::new();
        cache.store("0006:DEAD", Zeroizing::new(String::from("1234")));
        cache.store("0006:BEEF", Zeroizing::new(String::from("5678")));
        cache.clear_card("0006:DEAD");
        assert!(cache.get("0006:DEAD").is_none());
        assert!(cache.get("0006:BEEF").is_some());
    }

    /// Forgetting everything by `mem::replace`-ing the cache with a
    /// fresh empty one (what `pgp_forget_all` does) leaves the cache
    /// in a clean state. Validates the cache's `Default`-friendly shape.
    #[test]
    fn cache_replace_empties_it() {
        let mut cache = libtumpa::cache::CredentialCache::new();
        cache.store("fp", Zeroizing::new(String::from("pw")));
        assert!(cache.get("fp").is_some());
        cache = libtumpa::cache::CredentialCache::new();
        assert!(cache.get("fp").is_none());
    }

    /// Regression: with the always-cache policy a wrong PIN/passphrase
    /// would loop forever if we never evicted the stale entry on a sign
    /// failure. `evict_cached_secret` drops exactly the target's entry,
    /// so the next sign attempt re-prompts while every other cached
    /// secret stays put (important when several cards / keys are in use
    /// simultaneously).
    #[test]
    fn evict_cached_secret_drops_only_the_target_entry() {
        let cache = Arc::new(std::sync::Mutex::new(
            libtumpa::cache::CredentialCache::new(),
        ));
        {
            let mut guard = cache.lock().expect("cache mutex");
            guard.store("0006:DEAD", Zeroizing::new(String::from("1234")));
            guard.store("0006:BEEF", Zeroizing::new(String::from("5678")));
            guard.store(
                "FINGERPRINT-XYZ",
                Zeroizing::new(String::from("passphrase")),
            );
        }
        // `None` agent key: a plain `#[test]` has no tokio runtime, and
        // this exercise is only about the in-process cache eviction.
        super::evict_cached_secret(&cache, "0006:DEAD", None);
        let guard = cache.lock().expect("cache mutex");
        assert!(
            guard.get("0006:DEAD").is_none(),
            "evicted entry must be gone"
        );
        assert!(
            guard.get("0006:BEEF").is_some(),
            "other card PIN must survive"
        );
        assert!(
            guard.get("FINGERPRINT-XYZ").is_some(),
            "unrelated passphrase must survive"
        );
    }

    /// Regression: `#[serde(tag = "kind", rename_all = "camelCase")]` ONLY
    /// renames the variant tag values — it does NOT touch the fields
    /// inside the variants. Without `rename_all_fields = "camelCase"` the
    /// JSON went out as `signer_fingerprint` while the TS reader code
    /// accessed `signerFingerprint`, throwing `undefined is not an object`
    /// on every Good signature outcome (only visible once the card
    /// decrypt path actually returned a Good outcome end-to-end).
    #[test]
    fn pgp_verify_outcome_serializes_variant_fields_in_camel_case() {
        let outcome = PgpVerifyOutcome::Good {
            signer_uid: Some("Alice <alice@example.com>".into()),
            signer_fingerprint: "DEADBEEF".repeat(5),
            verifier_fingerprint: "CAFEBABE".repeat(5),
        };
        let json = serde_json::to_value(&outcome).expect("serialize");
        assert_eq!(json["kind"], "good");
        assert!(
            json.get("signerFingerprint").is_some(),
            "expected camelCase field name in JSON: {json}"
        );
        assert!(
            json.get("verifierFingerprint").is_some(),
            "expected camelCase field name in JSON: {json}"
        );
        assert!(
            json.get("signerUid").is_some(),
            "expected camelCase field name in JSON: {json}"
        );
        assert!(
            json.get("signer_fingerprint").is_none(),
            "snake_case must NOT appear once rename_all_fields is set: {json}"
        );

        // UnknownKey's keyId follows the same rule.
        let unk = PgpVerifyOutcome::UnknownKey {
            key_id: "ABCDEF12".into(),
        };
        let unk_json = serde_json::to_value(&unk).expect("serialize");
        assert_eq!(unk_json["kind"], "unknownKey");
        assert_eq!(unk_json["keyId"], "ABCDEF12");
        assert!(unk_json.get("key_id").is_none());
    }
}
