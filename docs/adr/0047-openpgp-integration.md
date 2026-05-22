# ADR 0047: OpenPGP Integration

## Status
Accepted

## Context
Chithi needs end-to-end OpenPGP email: read encrypted/signed mail,
compose signed and/or encrypted mail, manage keys, and use OpenPGP
smartcards. The author also maintains the "tumpa" family of OpenPGP
tools (tumpa-cli, the tumpa desktop app, `wecanencrypt`,
`johnnycanencrypt`), so chithi's OpenPGP layer is deliberately built to
fit that ecosystem rather than reinvent it.

Two early constraints shaped the whole design:

1. **No key generation in chithi.** Generating and managing the master
   secret is the job of the dedicated tumpa tools. Chithi imports,
   lists, and *uses* keys; it never creates them. This keeps the
   attack surface and the UX small.
2. **A shared on-disk keystore.** Re-importing every key into a
   chithi-private store would be hostile to a user who already has the
   tumpa toolchain set up. Chithi reuses the same keystore.

This ADR records the OpenPGP integration as a whole: the dependency
stack, the keystore, key management, PGP/MIME handling, the send and
receive paths, smartcard support, the secret-prompt mechanism, and the
per-account "Advanced settings" policy layer.

## Decision

### Dependency stack
Chithi's OpenPGP is built on **`libtumpa` 0.4.0** (`src-tauri/Cargo.toml`,
`default-features = false`, features `["card", "network"]`). The stack:

```
chithi  ->  libtumpa  ->  wecanencrypt  ->  rpgp
                 |
                 +-- card (PCSC smartcard)   +-- network (WKD over HTTP)
```

`libtumpa` provides the keystore (SQLite-backed), smartcard access,
WKD fetch, and the high-level sign/encrypt/decrypt/verify operations.
Chithi calls `libtumpa` and never `rpgp` directly.

`libtumpa`'s keystore uses `rusqlite`. Because only one crate per cargo
graph may "link" the native `sqlite3`, chithi's own `rusqlite` was
bumped (0.32 -> 0.39) to match `libtumpa`'s `libsqlite3-sys`, resolving
the native-link collision. No `rusqlite` API changes were required.

### The keystore
OpenPGP keys live in **`~/.tumpa/keys.db`** -- a SQLite database, the
**same keystore tumpa-cli and the tumpa desktop app use**. The location
honors `$TUMPA_DIR` and `$TUMPA_KEYSTORE`. A key imported in tumpa-cli
is immediately visible to chithi, and vice versa.

In `AppState` the keystore is `OnceLock<Arc<Mutex<libtumpa::KeyStore>>>`:

- **`OnceLock` (lazy open):** the keystore is opened on first OpenPGP
  command, so a missing or broken keystore never blocks app startup.
- **`std::sync::Mutex`, not `tokio::sync::Mutex`:** `KeyStore` holds a
  `rusqlite::Connection`, which is `Send + !Sync`. `libtumpa`'s API is
  synchronous, so async-aware locking buys nothing. Every CPU-bound
  OpenPGP call runs inside `tokio::task::spawn_blocking`, and the
  `Arc<Mutex<KeyStore>>` (which *is* `Send + Sync`) crosses that
  boundary.

### Key management
Key operations are exposed as Tauri commands in
`src-tauri/src/commands/pgp.rs`:

| Command | Purpose |
|---------|---------|
| `pgp_list_keys` | List key summaries (UIDs, subkeys, card links, status) |
| `pgp_get_key` | Full detail for one key |
| `pgp_import_key` | Import key bytes (armored or binary) |
| `pgp_pick_and_import_key` | Server-side file picker + import |
| `pgp_export_public` | Export an ASCII-armored public key |
| `pgp_delete_key` | Remove a key from the keystore |
| `pgp_wkd_fetch` | Fetch + import a public key via Web Key Directory |
| `pgp_list_cards` / `pgp_card_details` | Enumerate / inspect smartcards |
| `pgp_auto_link_cards` | Match connected cards to keystore keys |
| `pgp_decrypt_message` / `pgp_verify_message` | Receive-path operations |
| `pgp_check_recipients` | Compose-side per-recipient key precheck |
| `pgp_provide_secret` / `pgp_cancel_secret` | Answer a secret prompt |
| `pgp_forget_all` / `pgp_forget_card` | Clear cached secrets |

The file picker for import runs **server-side** (`tauri_plugin_dialog`):
the renderer receives the imported result, never the chosen path,
matching the `pick_attachments` security model. The capability set
deliberately grants `dialog:allow-message` only, not
`dialog:allow-open`.

`src/views/OpenPGPView.vue` is the management UI: a searchable key list
with S/C/R (signing/certify/revoked) badges, per-key detail (UIDs,
subkeys, card links, validity), an import modal (paste or pick), a WKD
fetch modal, and a smartcard section. The status line is card-aware --
it shows "in keystore", "on smartcard (ident)", or both, because
`libtumpa` reports `is_secret = false` for card-resident keys (the
keystore holds only their public material). Any logic asking "is there
a usable secret?" must check **both** `is_secret` and the card links.

### PGP/MIME handling
`src-tauri/src/mail/pgp_mime.rs` implements a **byte-exact MIME slicer**
that deliberately does **not** use the general `mail_parser` crate for
boundary walking. Two reasons:

1. RFC 3156 §5.1 requires the *exact bytes* of the signed entity for
   signature verification -- a re-serializing parser would corrupt them.
2. `mail_parser` 0.9 fails to surface the `boundary` attribute when a
   `Content-Type` is folded with a leading-space continuation, parsing
   the whole body as one opaque blob.

The slicer provides `detect_kind` (`MimeEncrypted` / `MimeSigned` /
`InlineArmor`), the three `extract_*_payload` functions, and
`canonicalize_for_signing` (promotes bare CR/LF to CRLF).
`tolerant_signed_variants` recovers from sender-side mangling -- notably
Microsoft Exchange's `\r\r\n` doubling and extra trailing CRLFs -- so an
inbound signature still verifies. Each message's detected shape is
stored on the DB row as `pgp_kind`.

### Send path
`apply_pgp_envelope` in `src-tauri/src/commands/compose.rs` runs when
the compose `pgp_sign` / `pgp_encrypt` toggles are set. Three modes:

- **Sign-only** -- detached signature, wrapped as `multipart/signed`
  (RFC 3156 §5). The signature is computed over
  `canonicalize_for_signing(inner_part_of(raw))`.
- **Encrypt-only** -- ciphertext wrapped as `multipart/encrypted`
  (RFC 3156 §6).
- **Sign-then-encrypt** -- signature sealed inside the ciphertext.

**Recipient routing:** To + Cc are *visible* recipients (normal PKESK
packets carrying the recipient key id); Bcc recipients are *hidden* --
their PKESK packets use the all-zero wildcard key id (RFC 4880
throw-keyid / `--hidden-recipient`) so a To/Cc recipient running
`gpg --list-packets` cannot enumerate the Bcc list. The sender's own
address is appended to the visible set ("encrypt-to-self") so they can
read the message back from Sent; if the sender has no usable public
key, that is skipped with a warning rather than failing the send.

**Signer backend:** before signing, `apply_pgp_envelope` calls
`libtumpa::encrypt::find_signing_card_for_encrypt`. If the signing key
is card-resident it MUST go through the card API (PIN-gated); feeding
card public-only bytes into the software signing API produces opaque
"expected SecretKey, got PublicKey" parse errors. Software keys use the
passphrase API instead.

**Wire transmission:** all outgoing mail -- IMAP, JMAP, and Microsoft
Graph accounts alike -- is transmitted via SMTP (`smtp::send_raw`; Graph
accounts use the IMAP/SMTP binding with XOAUTH2). The PGP-wrapped bytes
are persisted to the outbox and sent verbatim, so the envelope survives
the first-attempt send and any later outbox replay identically.

### Receive path
`pgp_decrypt_message` extracts the ciphertext, then tries a **software**
decryption key first (`find_software_decryption_key`); on a miss it
falls through to a **smartcard** (`find_decryption_card`) so a user
whose secret lives on a card can still decrypt -- including their own
Sent items. Decryption and inner-signature verification happen together
(`decrypt_and_verify_with_key` / `_on_card`); the plaintext is re-parsed
through the normal `parse_message_body` so HTML sanitization
(`ammonia`) still applies.

`pgp_verify_message` handles `multipart/signed`, retrying through
`tolerant_signed_variants` on a `Bad` result before giving up.
Verification surfaces as one of five outcomes -- `Unsigned`, `Good`,
`Bad`, `UnknownKey`, `Error` -- carried to the reader UI.

In the reader (`MessageReader.vue`): encrypted messages show a banner
with a **manual Decrypt button** (no auto-decrypt); signed messages
**auto-verify** on open and render a badge -- green (Good), red (Bad),
amber (UnknownKey / Error).

### Secret prompting (passphrases and card PINs)
Prompting is the **only** way a secret enters the app -- there is no
inline-argument or environment-variable path. `acquire_secret`:

1. Checks an in-memory cache first.
2. On a miss, registers a one-shot keyed by a request id and emits a
   `pgp-secret-needed` event (`kind: passphrase | pin`, target, reason).
3. The frontend `pgp-prompts` store (a FIFO queue) shows
   `PassphraseDialog` / `PinDialog`; the user's answer returns via
   `pgp_provide_secret`.

The event is routed with `app.emit_to(window_label, ...)` to the window
that initiated the operation, so the dialog appears where the user is
(compose vs. main window) and only once.

**Cache policy:** every successfully entered secret is cached for the
process lifetime as a `Zeroizing<String>` (`libtumpa::cache::CredentialCache`).
There is no TTL sweeper and no "remember" opt-in. Eviction happens on:
app exit (the `Zeroizing` destructors overwrite the buffers), the
explicit `pgp_forget_all` / `pgp_forget_card` commands, and -- crucially
-- a targeted `evict_cached_secret` whenever a cached secret produced a
sign/encrypt/decrypt failure (without it, a single mistyped PIN would
loop forever, since the bad value keeps coming back from the cache).

### Smartcard support
OpenPGP smartcards (Nitrokey, YubiKey, and other OpenPGP-Card-spec
devices) are reached through `libtumpa`'s PCSC backend.
`pgp_auto_link_cards` matches on-card key fingerprints against keystore
keys and persists the associations in a `card_keys` table. Both the
send path (sign-on-card) and the receive path (decrypt-on-card) detect
and use a linked card automatically, PIN-gated through the same
`acquire_secret` mechanism.

### Per-account Advanced settings
On top of the core, four per-account OpenPGP policy toggles -- modeled
on Thunderbird's per-identity options, **all enabled by default** --
were added:

| Toggle (`accounts` column) | Effect |
|----------------------------|--------|
| `pgp_attach_pubkey_on_sign` | Attach the sender's ASCII-armored public key (`application/pgp-keys`, filename `OpenPGP_0x<keyid>.asc`) to every signed message. |
| `pgp_autocrypt_header` | Add an `Autocrypt: addr=...; prefer-encrypt=mutual; keydata=...` header to every outgoing message (key-distribution bootstrap). |
| `pgp_encrypt_subject` | Encrypt the subject: the real subject is folded into a `multipart/mixed; protected-headers="v1"` entity inside the ciphertext (draft-ietf-lamps-header-protection); the cleartext outer Subject becomes `...`. |
| `pgp_encrypt_drafts` | Encrypt drafts to the sender's own public key before storing them on the server. |

Design decisions for this layer:

- **Per-account, not global.** The settings are only meaningful for an
  account that owns a key; different accounts may have different keys
  or none. They are stored as four `INTEGER NOT NULL DEFAULT 1` columns
  on the `accounts` table (idempotent migration), edited in an
  "Advanced settings" group in the account-edit form
  (`SettingsView.vue`).
- **Policy is read from `AccountFull`,** which `send_message` /
  `save_draft` already load -- the database is the single source of
  truth; no settings are threaded through the compose IPC.
- **Protected subjects are recovered on decrypt:** when an incoming
  message's decrypted payload carries its own `Subject`, it is
  persisted over the `...` placeholder (`db::messages::update_subject`)
  so the message list, search, and notifications show the real subject.
- **Encrypted drafts** are encrypt-to-self with the *public* key only --
  no signing, no passphrase/PIN prompt at Save Draft time, so a card
  that is not plugged in is irrelevant. Resuming a draft decrypts it
  back into the composer; a failed decrypt shows an inline placeholder
  + Retry. Microsoft Graph accounts have no raw-MIME draft endpoint, so
  encrypted drafts fall back to plaintext there; `save_draft` reports
  this (`DraftSaveOutcome.plaintext_fallback`) and the composer shows a
  non-blocking notice.

The attach-pubkey, Autocrypt, and protected-subject features only alter
outgoing MIME bytes, so they work uniformly across IMAP/JMAP/Graph (all
send via SMTP). Only encrypted drafts diverge by backend, as above.

## Security considerations
- **Secrets:** passphrases and PINs enter only through the prompt
  mechanism; they are held as `Zeroizing<String>` and their buffers are
  overwritten on drop. Bad secrets are evicted so they cannot be reused.
- **Throw-keyid for Bcc** keeps the Bcc recipient list out of the
  OpenPGP packet stream that To/Cc recipients can inspect.
- **Server-side file picker** keeps filesystem paths out of the
  renderer, consistent with attachment handling.
- **CRLF hardening:** the Autocrypt header and the protected-headers
  subject are hand-assembled (bypassing `lettre`'s address encoding),
  so `format_autocrypt_header` strips CR/LF from the address and
  `wrap_with_protected_headers` collapses CR/LF in the subject -- a
  malformed account address or crafted subject cannot inject header
  lines into outgoing mail.
- **Fail-open is deliberate:** OpenPGP enhancements skip silently (with
  a log line, and for drafts a UI notice) when no usable key is present,
  rather than blocking a user mid-setup.
- The integration and the Advanced-settings work were both put through
  differential security review
  (`CHITHI_DIFFERENTIAL_REVIEW_2026-05-22.md`).

## Consequences
- Chithi participates in the tumpa ecosystem: one shared keystore, no
  duplicated key material, no key generation in the mail client.
- All OpenPGP work is `spawn_blocking` because `libtumpa`'s API is
  synchronous; the `Arc<Mutex<KeyStore>>` is the synchronization point.
- chithi carries its own PGP/MIME slicer rather than depending on a
  general MIME parser for boundary walking -- more code to maintain, but
  byte-exact signature verification and folded-header correctness
  require it.
- Verification tolerance (`tolerant_signed_variants`) exists solely to
  cope with Exchange/Outlook sender-side mangling; it is a pragmatic
  concession, not a spec requirement.
- New installs and existing accounts get the four Advanced settings on
  by default; users opt out per account.
- Known limitations carried forward: a `pgp-changed` event is not yet
  emitted, so cross-process keystore changes (e.g. a tumpa-cli import
  while chithi runs) need a manual refresh; encrypted drafts are not yet
  supported on Microsoft Graph accounts; resuming a draft does not carry
  forward Bcc recipients or attachments and saves a new draft rather
  than replacing the original.
- Depends on `libtumpa` 0.4.0 (`card` + `network` features) and the
  `rusqlite` 0.39 alignment described above.
