# ADR 0013: Draft Save on Window Close

## Status
Accepted

## Context
When a user closes the compose window with unsaved content, the email should not be silently lost. Desktop email clients typically prompt to save as a draft.

## Decision
When the compose window is closed with unsaved changes, a native 3-button dialog appears:

- **Save Draft**: Saves the email to the server's Drafts folder, triggers a sync, then closes
- **Discard**: Closes without saving
- **Cancel**: Returns to the compose window

### Dirty tracking
Changes are detected by comparing current field values against the initial values (from URL query params for reply/forward, or empty for new compose). Any change to To, Cc, Bcc, Subject, Body, or adding attachments marks the compose as "dirty". Sending successfully sets a flag to skip the prompt.

### Draft storage
- **JMAP**: Upload raw message as blob, `Email/import` to the Drafts mailbox with `$draft` keyword
- **IMAP**: `APPEND` to the Drafts folder with `\Seen` and `\Draft` flags. Tries `Drafts`, `INBOX.Drafts`, `[Gmail]/Drafts` folder names.
- **Empty recipients**: Drafts may have no To/Cc/Bcc. The sender's own email is used as a placeholder To to produce valid RFC5322.
- **After save**: `triggerSync` is called so the draft appears in the local mailbox immediately.

### Save button
The toolbar Save button also calls `saveDraft()` for manual saves without closing.

## Consequences
- Drafts are stored on the server, synced across devices
- Users never lose compose content accidentally
- The dialog uses native OS buttons via `tauri-plugin-dialog` (`message()` with `YesNoCancel`)
- Custom button labels return the label string as the result (not "Yes"/"No"), so matching handles both
