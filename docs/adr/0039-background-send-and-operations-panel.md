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
5. Emit `send-started` event
6. Return `Ok(())` to the frontend

**Asynchronous phase** (runs in `tokio::spawn` after command returns):
1. Connect to SMTP server or JMAP
2. Transmit the message
3. On success: emit `send-complete`, auto-collect recipient contacts
4. On failure: emit `send-failed` with error details

This means the compose window closes almost instantly (only waits for local I/O), while the actual network send happens in the background.

**Events emitted:**
- `send-started` — `{account_id, subject}` — triggers toast "Sending..."
- `send-complete` — `{account_id, subject}` — triggers toast "Sent"
- `send-failed` — `{account_id, subject, error}` — triggers error toast (10s)

The activity store listens for these events and tracks send operations alongside sync operations.

### Part 2: Operations panel

A collapsible panel between the main content area and the status bar, showing all active and recent operations.

**Component**: `src/components/common/OperationsPanel.vue`

- Slide-up animation from the status bar
- Shows operations from `useActivityStore().recentOperations`
- Each row displays: status icon (animated spinner / checkmark / error X), label, detail text, operation type badge
- Running operations sorted to the top
- Max height 40% of viewport, scrollable
- Close button in header

**Toggle mechanism**: The status bar has an operations button (activity/pulse icon) with a badge showing the count of active operations. Clicking it toggles `uiStore.operationsPanelOpen`.

**Operation lifecycle and visibility**:
- Running operations show immediately when started
- Completed operations remain visible for 60 seconds (previously 5 seconds, which was too short to notice)
- Failed operations remain visible for 5 minutes (previously 15 seconds)
- This gives users time to open the panel and see what happened

**Layout in App.vue**:
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
- If the background send fails, the compose window is already closed. The user sees an error toast but cannot retry from the compose window. They would need to compose a new message. (Future: store failed sends in outbox for retry)
- The operations panel adds visual complexity to the UI
- Toast notifications from the main window may not be visible if the user has switched to another application

### Files modified

**Backend:**
- `src-tauri/src/commands/compose.rs` — split `send_message` into sync + async phases, emit send events

**Frontend:**
- `src/stores/activity.ts` — listen for `send-started`/`send-complete`/`send-failed`, show toasts
- `src/stores/ui.ts` — `operationsPanelOpen` state, `toggleOperationsPanel()`
- `src/stores/ops.ts` — centralized `op-failed` and `offline-queue-changed` tracking
- `src/components/common/OperationsPanel.vue` — new slide-up panel component
- `src/components/common/StatusBar.vue` — operations toggle button with badge
- `src/App.vue` — mount OperationsPanel between content and status bar
