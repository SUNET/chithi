# ADR 0031: Per-Window Tauri Capabilities

## Status
Accepted

## Context
The app had a single capability definition (`default.json`) granting the same permissions to both the main window and compose windows. This included `shell:default` (arbitrary shell execution), webview creation, and full window management. A compromised compose window had the same attack surface as the main window. This was item 6 in the pre-public-preview security audit (security0.md).

## Decision
Split capabilities into per-window files with least privilege.

### Main window (`capabilities/default.json`)
```json
{
  "identifier": "default",
  "windows": ["main"],
  "permissions": [
    "core:default",
    "core:webview:allow-create-webview-window",
    "core:window:allow-create",
    "core:window:allow-close",
    "core:window:allow-destroy",
    "core:window:allow-set-focus",
    "notification:default",
    "shell:allow-open",
    "dialog:default"
  ]
}
```

Changes from before:
- **`shell:default` → `shell:allow-open`** — only URL opening (for OAuth redirects in Settings), no arbitrary command execution via `shell:allow-execute` or `shell:allow-spawn`
- **`windows: ["main"]`** — no longer applies to compose windows

### Compose windows (`capabilities/compose.json`)
```json
{
  "identifier": "compose",
  "windows": ["compose-*"],
  "permissions": [
    "core:default",
    "core:window:allow-close",
    "core:window:allow-destroy",
    "core:window:allow-set-focus",
    "dialog:default"
  ]
}
```

Removed vs main window:
- **No `shell`** — compose windows have no reason to open URLs or execute commands
- **No `core:webview:allow-create-webview-window`** — compose doesn't spawn other windows
- **No `core:window:allow-create`** — compose doesn't create windows
- **No `notification`** — compose doesn't send notifications

### Mail reader (sandboxed iframe)
The email reader implemented in ADR 0026 uses a sandboxed `<iframe>` with an opaque origin. It has zero Tauri permissions by design — no IPC, no shell, no dialog, no window creation. No capability file is needed since it's not a separate Tauri webview.

### Privilege comparison

| Permission | Main | Compose |
|---|---|---|
| `core:default` | Yes | Yes |
| `core:webview:allow-create-webview-window` | Yes | No |
| `core:window:allow-create` | Yes | No |
| `core:window:allow-close` | Yes | Yes |
| `core:window:allow-destroy` | Yes | Yes |
| `core:window:allow-set-focus` | Yes | Yes |
| `notification:default` | Yes | No |
| `shell:allow-open` | Yes | No |
| `dialog:default` | Yes | Yes |

## Consequences
- A compromised compose window can no longer execute shell commands, create new windows, or spawn additional webviews
- `shell:allow-open` restricts the main window to URL opening only — no `shell:allow-execute` or `shell:allow-spawn`
- Future permissions for new features should be added to the narrowest capability file that needs them
- Capability files are in `src-tauri/capabilities/` — one per window class
