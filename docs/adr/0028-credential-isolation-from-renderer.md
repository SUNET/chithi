# ADR 0028: Credential Isolation from Renderer

## Status
Accepted

## Context
When editing an account in Settings, `get_account_config()` fetched the actual password from the system keyring and returned it to the frontend over Tauri IPC. A compromised renderer could call this command to exfiltrate all stored passwords. This was item 3 in the pre-public-preview security audit (security0.md).

## Decision
Never return stored passwords or tokens to the frontend. The password field in the edit form uses a show/hide toggle for new input only.

### Backend changes

**`get_account_config()`** (`src-tauri/src/commands/accounts.rs`):
- Returns `password: String::new()` instead of the keyring value
- The actual password stays in the Rust backend — it never crosses the IPC boundary to the renderer

**`update_account()`** (`src-tauri/src/db/accounts.rs`):
- If `config.password` is empty, the keyring update is skipped (existing password preserved)
- If `config.password` is non-empty, the new password is written to the keyring
- This enables the "leave empty to keep current" UX pattern

### Frontend changes

**`PasswordInput.vue`** (`src/components/common/PasswordInput.vue`):
- New reusable component adapted from tugpgp's PinEntry
- Password/text type toggle via eye icon button
- Eye SVGs (`eye-visible.svg`, `eye-hidden.svg`) use `stroke="currentColor"` — automatically adapts to light/dark themes
- Visibility resets on blur (auto-hides when focus leaves the field)
- Input styling matches the Settings form exactly (Inter font, 16px, same border/radius/background)

**`SettingsView.vue`**:
- Uses `PasswordInput` instead of plain `<input type="password">`
- When editing: placeholder says "Leave empty to keep current password"
- When creating: placeholder shows the account-type-specific hint
- OAuth accounts (O365) continue to hide the password field entirely

### What this prevents
- A compromised renderer cannot read stored passwords via `get_account_config()`
- XSS or IPC abuse yields `password: ""` — no credential exposure
- The keyring is only read by the Rust backend when establishing IMAP/SMTP/JMAP connections

### What users can still do
- See what they're typing in the password field (eye toggle)
- Change their password by typing a new one
- Keep the existing password by leaving the field empty
- View stored passwords in their OS keyring manager (GNOME Keyring, KDE Wallet, macOS Keychain) if needed

## Consequences
- Passwords never cross the IPC boundary from backend to frontend
- Users can verify new input via show/hide toggle but cannot retrieve previously saved passwords from the app
- Empty password on save is a no-op — safe default that prevents accidental credential deletion
- The PasswordInput component is reusable for future password fields (e.g., PGP passphrase)
