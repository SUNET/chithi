# ADR 0012: Compose as Separate Window

## Status
Accepted

## Context
Email composition was originally a route within the main window, replacing the mail view while composing. This prevented users from referencing other emails while writing, and only allowed one compose at a time.

## Decision
Compose opens in a separate native Tauri window (1024x700) via `WebviewWindow`. Each compose action (Compose button, Reply, Reply All, Forward) creates an independent window. Multiple compose windows can be open simultaneously.

### Key design choices:

- **Separate Vue instance**: Each compose window runs its own Vue app. Pinia stores are empty in the new window, so ComposeView fetches accounts directly via `api.listAccounts()` on mount.
- **Account context via URL**: The active account ID is passed as a query parameter (`?accountId=...`) so the From dropdown pre-selects the correct account.
- **Window title**: Shows "Write {subject} - Chithi" or "Write (no subject) - Chithi".
- **Standalone layout**: `App.vue` detects the compose route and renders without sidebar/menubar/statusbar.
- **Close = close window**: Send and Discard call `currentWindow.close()` instead of `router.push()`.

### Capabilities required:
- `core:webview:allow-create-webview-window`
- `core:window:allow-create`
- `core:window:allow-close`
- `core:window:allow-destroy`

## Consequences
- Users can compose while reading other emails
- Multiple compose windows can be open at once
- Reply/Forward data passes through URL query params (limited by URL length for very long quoted bodies)
- The compose window's Pinia stores are independent from the main window
