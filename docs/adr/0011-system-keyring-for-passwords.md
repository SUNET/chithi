# ADR 0011: System Keyring for Password Storage

## Status
Accepted

## Context
Account passwords (IMAP, JMAP, SMTP, CalDAV) were stored as plaintext in the SQLite `accounts` table. This is a security risk — anyone with read access to `~/.local/share/emails/emails.db` could extract all credentials.

## Decision
Store passwords in the operating system's native credential manager instead of SQLite:
- **Linux**: Secret Service D-Bus API (GNOME Keyring, KDE Wallet, KeePassXC)
- **macOS**: Keychain (Security Framework)
- **Windows**: Windows Credential Manager

We use the `keyring` crate (v3) with platform-specific features (`sync-secret-service`, `apple-native`, `windows-native`).

### Design
- **Service name**: `com.emails.desktop` (groups all entries in the credential store)
- **Key**: Account UUID (unique per account)
- A thin `keyring.rs` module wraps `set_password`, `get_password`, `delete_password`
- `insert_account()` and `update_account()` write to the keyring; the DB has no password column
- `get_account_full()` fetches the password from the keyring and populates the `AccountFull` struct
- All downstream consumers (`ImapConfig`, `JmapConfig`, `CalDavConfig`, SMTP) receive the password transparently — zero changes needed

### What did NOT change
- `AccountFull` struct still has `password: String`
- `AccountConfig` struct still has `password: String` (frontend sends it via IPC)
- All protocol handlers and command handlers are untouched
- Frontend account forms are untouched

## Consequences
- Passwords are protected by the OS keyring's encryption and access control
- The `password` column was removed from the `accounts` table entirely
- If the keyring is unavailable (locked, not installed), `get_account_full()` logs a warning and returns an empty password — the user must re-enter it via account settings
- No automatic migration — users re-enter passwords after upgrading
