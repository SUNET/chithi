# ADR 0036: System Folder Deletion Guard

## Status
Accepted

## Context

The `delete_folder` Tauri command accepts a `folder_path` from the frontend and forwards it to the mail server (IMAP DELETE, JMAP Mailbox/set destroy, or Graph API). A differential security review (F-01) identified that:

1. No validation prevents deletion of system folders (INBOX, Sent, Drafts, Trash, Junk, Archive)
2. No check verifies the folder exists in the local DB for the given account before issuing a server-side delete
3. A compromised frontend (e.g., XSS in the webview) could invoke `delete_folder` with `folder_path = "INBOX"` and permanently destroy the user's inbox on the server

Server-side folder deletion is typically irreversible — the folder and all its messages are permanently removed.

## Decision

Add two server-side guards in `delete_folder` before forwarding to the mail server:

### 1. Existence check
Query the local `folders` table to verify the folder path exists for the given `account_id`. If not found, reject with an error. This prevents deletion of arbitrary paths that don't correspond to known folders.

### 2. System folder type denylist
If the folder has a `folder_type` matching any of these protected types, reject the deletion:
- `inbox` — primary mailbox
- `sent` — sent mail
- `drafts` — draft messages
- `trash` — deleted items
- `junk` — spam folder
- `archive` — archived mail

These types are assigned during folder sync based on server-reported folder roles (IMAP special-use flags, JMAP role property, Graph well-known folder names).

### Why server-side, not frontend-only

The frontend already has a confirmation dialog, but frontend guards can be bypassed:
- XSS in the webview could call `window.__TAURI_INTERNALS__.invoke("delete_folder", ...)`
- A malicious browser extension or devtools could invoke IPC directly
- The Tauri IPC trust boundary is at the Rust command layer, not the renderer

### What about user-created folders with system-like names?

A user could create a folder named "Inbox" (different from the system INBOX). The guard checks `folder_type`, not `folder_name`. User-created folders have `folder_type = NULL` and are not protected. Only server-declared system folders are protected.

## Consequences

- System folders cannot be deleted through the UI or via IPC, even by a compromised renderer
- The `delete_folder` command returns an error with a clear message ("Cannot delete system folder 'INBOX' (inbox)")
- User-created folders remain deletable regardless of their name
- The check adds one extra DB query (folder_type lookup) before the delete — negligible latency
- If a mail server reports incorrect folder types during sync, the wrong folders could be protected or unprotected. This is an acceptable trade-off since folder types come from server-authoritative data.
