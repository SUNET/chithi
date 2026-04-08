# ADR 0029: Attachment Save with Backend-Owned Dialog

## Status
Accepted

## Context
Emails can have file attachments. The app needed a way to save attachments from received messages to disk. The security audit (security0.md item 4) required that attachment handling not give the renderer an arbitrary file-write primitive.

An initial implementation opened the save dialog in the frontend and passed the chosen path to the backend over IPC. This was insecure: a compromised renderer could skip the dialog and invoke the command with any absolute path, writing attacker-controlled content to arbitrary locations.

## Decision
The save dialog is opened by the **Rust backend** using `tauri_plugin_dialog::DialogExt`. The renderer never supplies a file path — only a suggested filename.

### Command signature
```rust
pub async fn save_attachment(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
    attachment_index: u32,
    suggested_filename: String,  // NOT a path
) -> Result<()>
```

### Flow
1. Renderer calls `save_attachment(accountId, messageId, index, "report.pdf")`
2. Backend reads the raw email from Maildir and extracts the attachment by index
3. Backend opens the native OS save dialog via `app.dialog().file().set_file_name(&suggested_filename).blocking_save_file()`
4. User picks a save location (or cancels)
5. Backend checks the chosen path is not a symlink
6. Backend writes the attachment bytes to the user-approved path

### Security properties
- **No renderer-supplied paths** — the renderer sends a suggested filename, not a path. The actual destination is chosen by the user through a backend-owned native dialog.
- **No raw bytes cross IPC** — attachment content stays in the Rust process. The renderer only sees metadata (filename, size, content-type).
- **Symlink check** — refuses to write if the destination is a symlink, preventing symlink-following attacks that could clobber sensitive files.
- **Compromised renderer impact** — can only trigger the native save dialog with a suggested filename. The user must actively choose a destination. The attacker cannot write to an arbitrary path.

### Attachment display
Attachments are shown in `MessageReader.vue` between the headers and the message body:
- Paperclip icon with count header
- Clickable chips per attachment showing filename, size, and a download icon
- Clicking triggers the backend `save_attachment` command
- Loading state on the chip while saving, toast on success/failure

### What remains open
Compose attachments still accept file paths from the renderer (via the frontend dialog plugin). A compromised renderer could craft a fake IPC call to read arbitrary files as attachments. This should be hardened with backend path validation in a future change.

## Consequences
- Attachment save is secure against renderer compromise — no arbitrary file-write primitive
- Users see attachments listed with metadata and can save them via native OS dialog
- The backend handles the full lifecycle: read email → extract attachment → open dialog → write file
- Compose attachment path validation is deferred (separate concern, noted in security0.md)
