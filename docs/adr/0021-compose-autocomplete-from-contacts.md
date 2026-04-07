# ADR 0021: Compose Autocomplete from Contacts

## Status
Accepted

## Context
Composing emails required manually typing full email addresses. With contacts stored locally (synced from JMAP, Google People API, and auto-collected from sent emails), the client has address data that should be surfaced during composition.

## Decision
Add inline autocomplete to the To, Cc, and Bcc fields in the compose window.

### Behavior
- Triggers after typing 2+ characters, with 150ms debounce to avoid excessive queries
- Searches two sources in parallel: `searchContacts` (full contact books) and `searchCollectedContacts` (auto-collected from sent emails)
- Results deduplicated by email address; full contacts take priority over collected
- Up to 8 results shown in a dropdown below the active input
- Each result shows: display name, email address, and a source badge ("Contacts" or "Recent")

### Keyboard navigation
- Arrow Up/Down to move selection
- Enter or Tab to insert the selected contact
- Escape to dismiss the dropdown

### Comma-separated handling
Recipient fields accept multiple addresses separated by commas or semicolons. The autocomplete searches only the **last term** (text after the final comma/semicolon). Selecting a result replaces that last term with `Display Name <email>, `, preserving earlier addresses.

### Why not a separate component?
The autocomplete is implemented directly in ComposeView rather than as a reusable component. The compose window runs as a separate WebviewWindow with its own Vue instance, so shared components add complexity. The logic is ~80 lines of TypeScript + 60 lines of CSS, small enough to inline.

### Why both contact sources?
- **Full contacts** have structured data (name, multiple emails, labels) from synced address books
- **Collected contacts** capture addresses the user has emailed before but may not have formally added as contacts, ranked by `use_count` so frequently-used addresses appear first

## Consequences
- Recipient entry is faster — no need to remember or type full email addresses
- Auto-collected contacts surface without requiring manual contact management
- The 150ms debounce and 8-result limit keep the UI responsive
- Works in the separate compose WebviewWindow (fetches via IPC, no shared store dependency)
