# ADR 0039: Background Send and Operations Panel

## Status
Accepted

## Context

### Compose window freeze during send

When the user clicks Send in the compose window, the frontend awaits `api.sendMessage()` which performs the full SMTP/JMAP round-trip synchronously. This includes:

- DNS resolution and TCP handshake
- TLS/STARTTLS negotiation
- SMTP authentication (or JMAP session setup)
- Message transfer (proportional to attachment size)
- Server response

Total duration: 500ms to 10+ seconds depending on network conditions and attachment size. During this time the compose window appears frozen — the user cannot interact with it or close it.

Thunderbird solves this by hiding the compose window immediately and showing a "Sending Message..." progress indicator in the main window's status area.

### No unified operation visibility

Background operations (sync, send, move, delete) had limited visibility. The status bar showed a small spinner and connection status, but users couldn't see what was actually happening — which account was syncing, how many operations were queued, or what failed.

## Decision

### Part 1: Background send

Restructure `send_message` in `compose.rs` to split into synchronous and asynchronous phases:

**Synchronous phase** (compose window waits for this):
1. Validate recipients
2. Read attachment files from disk
3. Build RFC5322 message using `lettre`
4. Refresh OAuth token for O365 accounts
5. Persist the complete message and envelope in the outbox with status
   `sending`
6. Emit a correlated `send-started` event
7. Return `Ok(())` to the frontend

**Asynchronous phase** (runs in `tokio::spawn` after command returns):
1. Connect to SMTP server or JMAP
2. Transmit the message
3. On success: emit `send-complete`, auto-collect recipient contacts
4. On definite failure: atomically return the outbox row to `pending`, or mark
   it `dead` immediately when that failure reaches the retry limit, and emit
   `send-failed`
5. When completion is unknown: atomically quarantine the row as `dead` and
   emit `send-unknown`

This means the compose window closes almost instantly (only waits for local I/O), while the actual network send happens in the background.

**Events emitted:**
- `send-started` — `{account_id, subject, outbox_id}` — triggers toast
  "Sending..."
- `send-complete` — `{account_id, subject, outbox_id}` — triggers toast "Sent"
- `send-failed` — `{account_id, subject, outbox_id, error}` — reports a
  definite failure
- `send-unknown` — `{account_id, subject, outbox_id, error}` — warns that
  delivery may already have occurred

The activity store listens for these events and tracks send operations alongside sync operations.

### Part 2: Operations panel

A collapsible panel between the main content area and the status bar, showing all active and recent operations.

**Component**: `src/components/common/OperationsPanel.vue`

- Slide-up animation from the status bar
- Shows operations from `useActivityStore().recentOperations`
- Each row displays: status icon (animated spinner / checkmark / error X), label, detail text, operation type badge
- Operations sorted by start time, newest first
- Max height 40% of viewport, scrollable
- Close button in header

**Toggle mechanism**: The status bar has an operations button (activity/pulse icon) with a badge showing the count of active operations. Clicking it toggles `uiStore.operationsPanelOpen`.

**Operation lifecycle and visibility**:
- Running operations show immediately when started
- Completed operations remain visible for 60 seconds (previously 5 seconds, which was too short to notice)
- Failed operations remain visible for 5 minutes (previously 15 seconds)
- This gives users time to open the panel and see what happened

**Layout in `src/components/shell/DesktopShell.vue`**:
```
<main class="app-content">
  <router-view />
</main>
<OperationsPanel />   <!-- slides up from here -->
<StatusBar />          <!-- always visible at bottom -->
```

## Consequences

### Positive
- Compose window closes in <100ms instead of 1-10+ seconds
- Users see real-time feedback: "Sending..." toast followed by "Sent" or error
- All background operations (sync, send, move, delete) visible in one place
- Operations panel provides transparency into what the app is doing
- Failed sends are clearly surfaced with error details

### Negative
- If the background send fails, the compose window is already closed. The user
  sees an error toast and must use the outbox controls to inspect or manually
  retry an indeterminate delivery.
- The operations panel adds visual complexity to the UI
- Toast notifications from the main window may not be visible if the user has switched to another application

### Remaining limitations

- A panic or forced cancellation inside a detached first-attempt send can
  leave its row in `sending` until restart. Startup then quarantines that row;
  it is never automatically replayed. Exact in-process task supervision is a
  follow-up.
- Calendar invitation and RSVP mail uses the same SMTP/JMAP delivery
  classification, but is not yet represented by this durable Outbox state
  machine. Indeterminate outcomes are returned to the caller and are not
  automatically retried; durable calendar-specific recovery is a follow-up.
- Best-effort IMAP Sent-folder maintenance is not itself durable. A crash after
  recipient delivery can omit the Sent copy rather than risk resubmission.

### Send persistence

The built `raw_message` is base64-encoded and saved to the `outbox` table (action_type = "send") **before** the background task is spawned. This ensures the message survives an app crash during sending:
- On success: the claimed outbox entry is deleted.
- Delivery completion and `send-complete` are persisted/emitted before
  best-effort IMAP Sent-folder append and sync work. A crash may therefore
  leave no Sent copy, but cannot turn accepted delivery into an automatic
  recipient resend.
- On a definite failure: the outbox entry becomes `pending` for automatic
  replay and the `send-failed` event is emitted. The failure that reaches the
  retry limit transitions directly to `dead`; it is never left looking queued
  until another sync.
- When SMTP or JMAP does not provide trustworthy completion evidence: the
  outbox entry becomes `dead`, remains visible, and requires a deliberate
  manual retry because delivery may already have occurred.
- On a crash: an entry stranded in `sending` is quarantined as `dead` on the
  next startup for the same duplicate-prevention reason.
- Startup acquires an exclusive lock for the data directory before opening the
  database or quarantining rows. A second process cannot steal a live send
  claim from the process that owns it.
- Before replay transport begins, the worker atomically claims the exact
  pending row as `sending`. Stale snapshots cannot submit, and retry/discard
  commands cannot mutate an in-flight row.
- Manual retry and discard commands require both the row ID and its active
  account ID. A stale renderer cannot mutate a row belonging to another
  account.
- Unknown rows are labeled "Delivery status unknown". Manual retry requires
  confirmation that it may duplicate delivery; discard explains that removing
  the local record cannot cancel a message already accepted remotely.

### Implementation locations

**Backend:**
- `src-tauri/src/commands/compose.rs` — split `send_message` into sync + async phases, emit send events
- `src-tauri/src/backend/mail/mod.rs` and `backend/mail/imap.rs` — separate
  definitive SMTP delivery from best-effort Sent-folder postprocessing
- `src-tauri/src/commands/outbox.rs` — account-scoped listing, manual retry,
  and discard commands
- `src-tauri/src/error.rs` — typed indeterminate-delivery classification
- `src-tauri/src/mail/smtp.rs` — final-response validation and bounded
  post-acceptance connection cleanup
- `src-tauri/src/mail/jmap/mail.rs` and `src-tauri/src/provider.rs` —
  no-redirect final submission and correlated response validation
- `src-tauri/src/ops/offline.rs` — atomic send claim, completion, retry-limit,
  and quarantine transitions
- `src-tauri/src/ops/worker.rs` — replay routing and correlated send events
- `src-tauri/src/state.rs` — exclusive data-directory ownership before startup
  recovery

**Frontend:**
- `src/stores/activity.ts` — transactionally register
  `send-started`/`send-complete`/`send-failed`/`send-unknown` listeners and
  show correlated toasts
- `src/main.ts` — make bounded activity-listener attempts before mounting,
  always mount, and surface/retry persistent initialization failures
- `src/lib/tauri.ts` and `src/lib/types.ts` — typed, account-scoped Outbox IPC
- `src/components/mail/OutboxList.vue` — generation-safe refreshes, delivery
  status, and guarded manual actions
- `src/components/common/ToastContainer.vue` — accessible send-status
  announcements
- `src/stores/ui.ts` — `operationsPanelOpen` state, `toggleOperationsPanel()`
- `src/stores/ops.ts` — centralized `op-failed` and `offline-queue-changed` tracking
- `src/components/common/OperationsPanel.vue` — new slide-up panel component
- `src/components/common/StatusBar.vue` — operations toggle button with badge
- `src/components/shell/DesktopShell.vue` — mount `OperationsPanel` between
  main content and `StatusBar`
