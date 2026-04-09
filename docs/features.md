# Chithi - Feature Overview

Chithi is a desktop email client built with Tauri v2 (Rust backend) and Vue 3
(TypeScript frontend). It supports multiple email protocols, calendar
integration, contacts, and message filtering with a focus on privacy and
security.

---

## Email

### Protocols
- [x] IMAP with TLS (tested with Gmail, generic IMAP servers)
- [x] JMAP (RFC 8620/8621, tested with Stalwart)
- [x] SMTP for sending (via `lettre`)
- [x] JMAP Submission for sending (no SMTP needed for JMAP accounts)
- [x] Microsoft 365 (IMAP/SMTP with XOAUTH2, Graph API for profile)

### Accounts
- [x] Multi-account support with per-account folders
- [x] Gmail with app password (auto-filled IMAP/SMTP hosts)
- [x] Generic IMAP/SMTP with manual host/port configuration (TLS and STARTTLS)
- [x] JMAP with auto-discovery (`.well-known/jmap`) or manual URL
- [x] Google OAuth2 with PKCE for calendar and contacts access (no client secret)
- [x] Microsoft 365 OAuth2 with PKCE for IMAP/SMTP/Graph (public client)
- [x] Account enable/disable
- [x] Per-account email signatures (plain text, configurable in settings)

### Sync
- [x] Envelope-first sync (headers first, bodies on demand)
- [x] Background body prefetch after sync (parallel connections, batch 100)
- [x] Fast parallel sync with 4 IMAP connections
- [x] Background sync every 2 minutes as fallback
- [x] Sync current folder first for faster UI updates
- [x] Right-click folder to sync only that folder
- [x] IMAP IDLE for near-instant INBOX push notifications (thread-per-account, exponential backoff reconnect)
- [x] JMAP EventSource (SSE) for real-time push notifications (async task-per-account, anti-proxy-buffering headers)
- [x] Sync progress and error events shown in status bar
- [ ] Offline outbox queue (table exists, UI/logic not wired)

### Reading
- [x] Plain text display by default (privacy-first)
- [x] Optional HTML view with toggle button
- [x] HTML rendered in sandboxed iframe (opaque origin, no Tauri IPC access)
- [x] HTML sanitized in Rust (`ammonia`) — defense-in-depth behind iframe sandbox
- [x] No JavaScript execution in HTML emails
- [x] No remote content (images/CSS blocked by default)
- [x] Remote image loading on opt-in per message (backend proxy, base64 data URIs, HTTPS-only)
- [x] Link clicks copy URL to clipboard instead of opening browser
- [x] MIME encoded subject decoding
- [x] Mailing list ID display (`List-Id` header)
- [x] Attachment listing with save to disk (native save dialog owned by backend)
- [ ] Full-text search (Tantivy dependency added, not wired)

### Composing
- [x] Compose in separate native window
- [x] Reply, Reply All, Forward with quoted text
- [x] File attachments via native file picker
- [x] Draft auto-save on window close (Save Draft / Discard / Cancel dialog)
- [x] Attachment mention detection ("attached"/"attachment" warning if no files)
- [x] Per-account signature auto-appended (5-line gap for new messages, 2-line for replies)
- [x] Signature swaps when switching From account
- [x] Ctrl+Z/undo and standard editing shortcuts (WebKitGTK workaround)
- [ ] Rich text compose (HTML editor)
- [ ] Inline images

### Message Actions
- [x] Move to folder (IMAP and JMAP)
- [x] Copy to folder
- [x] Delete messages
- [x] Flag / unflag
- [x] Mark read / unread
- [x] Multi-select with checkboxes (Shift+click for range, Ctrl+click for toggle)
- [x] Delete key shortcut
- [x] Right-click context menu: Reply, Reply All, Forward, Move To, Copy To, Delete
- [x] Not Spam: toolbar button and context menu in Junk folder (moves to Inbox)
- [x] Folder count badges update immediately after move/delete

### Threading
- [x] Conversation threading via `In-Reply-To` and `References` headers
- [x] Subject-based fallback for broken threading
- [x] Expandable/collapsible thread rows
- [x] Thread/unthread individual messages

---

## Calendar

- [x] Month and week views with event display
- [x] Click to create new events
- [x] Event editing with title, description, location, start/end time, all-day toggle
- [x] Recurring events with recurrence rule editor
- [x] Attendee management with invitation emails
- [x] Multiple calendars per account with color coding
- [x] JMAP calendar sync (JSCalendar-bis format)
- [x] Google Calendar API v3 sync with incremental `syncToken`
- [x] CalDAV calendar discovery and event sync
- [x] Meeting invite detection from email (`text/calendar` MIME parts)
- [x] Accept / Maybe / Decline invite responses (sends iTIP REPLY via SMTP)
- [x] Invite reply auto-processing (updates attendee status on organizer's calendar)
- [x] Multi-day events display in all-day banner
- [x] Local timezone display
- [ ] CalDAV event push (only polling)
- [ ] Recurring event instance editing

---

## Contacts

- [x] Contact books per account
- [x] Contact CRUD: create, edit, delete with first/middle/last name, multiple emails, phones
- [x] Contact search across all accounts
- [x] JMAP contacts sync (JSContact format)
- [x] Google People API sync (list, create, update, delete)
- [x] Auto-collect contacts from sent emails (per account, ranked by use count)
- [x] Right-click email address in message reader to add or edit contact
- [x] Contact lookup scoped to active account (same email can be "Add" on one account, "Edit" on another)
- [x] Three-panel contacts view (books sidebar, contact list, detail panel)
- [ ] Compose autocomplete from contacts
- [ ] CardDAV sync for generic IMAP accounts
- [ ] vCard import/export

---

## Message Filters

- [x] Client-side filter rules engine
- [x] Conditions: from, to, cc, subject, body, has_attachment, size, date, custom headers
- [x] Operators: contains, not_contains, equals, not_equals, matches_regex, greater_than, less_than
- [x] Actions: move, copy, delete, flag, unflag, mark_read, mark_unread
- [x] AND/OR condition groups
- [x] Priority ordering with stop-after-match
- [x] Auto-apply on new message arrival during sync
- [x] Apply to existing messages in a folder on demand
- [x] Filter management UI (create, edit, delete, reorder)
- [ ] Quick filter creation from message context menu
- [ ] Server-side Sieve filters

---

## Security and Privacy

- [x] Plain text email display by default
- [x] HTML email rendered in sandboxed iframe with opaque origin (no Tauri IPC access)
- [x] HTML sanitization via ammonia: no scripts, no event handlers, no remote content
- [x] Content Security Policy enforced for main app window and sandboxed email reader
- [x] Remote images loaded via backend proxy (HTTPS-only, base64 data URIs, per-message opt-in)
- [x] Links copy to clipboard instead of opening browser
- [x] Passwords stored in system keyring (GNOME Keyring / KDE Wallet / macOS Keychain / Windows Credential Manager)
- [x] Passwords never returned to the frontend — edit form uses show/hide toggle for new input only
- [x] OAuth tokens stored in system keyring
- [x] OAuth2 with PKCE for both Google and Microsoft (no client secrets in source code)
- [x] No password column in SQLite database
- [x] Attachment save via backend-owned native dialog (renderer never supplies file paths)
- [x] Per-window Tauri capabilities with least privilege (main, compose, sandboxed reader)
- [x] Symlink check on attachment save paths
- [ ] OpenPGP encryption/signing (via `wecanencrypt`)
- [ ] PGP/MIME (RFC 3156)
- [ ] Key management UI

---

## User Interface

- [x] Three-pane layout (folder tree, message list, message reader)
- [x] Resizable panes with drag handles
- [x] Light theme (default, Figma design tokens)
- [x] Dark theme
- [x] Bundled fonts (Inter, Liberation Mono)
- [x] Infinite scroll for large message lists (100 per page)
- [x] Sortable columns (subject, correspondents, date)
- [x] Status bar with sync button, connection status dot, error messages
- [x] Menu bar (File, View)
- [x] Sidebar navigation (Mail, Calendar, Compose, Contacts, Settings)
- [x] Cross-account folder navigation (click folder on any account)
- [x] Folder right-click context menu (New Folder, Mark Folder Read, Sync)
- [x] Zoom in/out with Ctrl+/Ctrl-/Ctrl+0
- [x] File logging to `~/.local/share/chithi/chithi.log`
- [x] Desktop notifications on new mail (only when window not focused)
- [ ] Keyboard shortcuts beyond Delete key
- [ ] App packaging (deb, AppImage, DMG, MSI)

---

## Storage

- [x] SQLite database with WAL mode (`~/.local/share/chithi/chithi.db`)
- [x] Maildir format for raw email storage (`~/.local/share/chithi/<account_id>/<folder>/`)
- [x] Per-folder sync state tracking (IMAP UIDs, JMAP state strings)
- [x] Folder unread/total counts recalculated from messages table

---

## Architecture Decisions

All design decisions are documented in `docs/adr/`:

| ADR | Decision |
|-----|----------|
| 0001 | Copy link on click (no browser open) |
| 0002 | Client-side message filters |
| 0003 | Plain text default, no remote content in HTML |
| 0004 | Email threading with subject-based fallback |
| 0005 | No JavaScript in HTML emails |
| 0006 | Sync current folder first |
| 0007 | Checkbox selection (no Ctrl+click) |
| 0008 | JMAP proxy URL rewriting for reverse proxies |
| 0009 | JMAP send via Submission API |
| 0010 | Account type selection (Gmail/IMAP/JMAP/CalDAV) |
| 0011 | System keyring for password storage |
| 0012 | Compose in separate window |
| 0013 | Draft save on compose close |
| 0014 | Google OAuth2 for calendar/contacts |
| 0015 | Calendar invite reply processing |
| 0016 | Google Calendar API integration |
| 0017 | Google Calendar two-way sync |
| 0018 | IMAP IDLE for push notifications |
| 0019 | Sync error and network status in status bar |
| 0020 | JMAP EventSource push notifications |
| 0021 | Compose autocomplete from contacts |
| 0022 | CardDAV contact sync |
| 0023 | Fast initial sync (parallel connections, batch inserts) |
| 0024 | IMAP STARTTLS support |
| 0025 | Microsoft 365 Graph API integration |
| 0026 | Sandboxed HTML email rendering (iframe isolation) |
| 0027 | Content Security Policy |
| 0028 | Credential isolation from renderer |
| 0029 | Attachment save with backend-owned dialog |
| 0030 | Google OAuth PKCE public client |
| 0031 | Per-window Tauri capabilities |
| 0032 | Remote image loading via backend proxy |
