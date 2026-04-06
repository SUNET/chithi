# ADR 0019: Sync Error and Network Status in Status Bar

## Status
Accepted

## Context
When the network goes down, the status bar continued showing "2 accounts connected" with a green dot. Two problems existed:

1. The `sync-error` Tauri event (emitted when IMAP/JMAP sync fails) updated the error message text but did not change the connection status dot to red.
2. The error message auto-cleared after 10 seconds via `setTimeout`, reverting the UI to "connected" even though the network was still down.

The IMAP IDLE loop (ADR 0018) emits `idle-disconnected` when its persistent connection drops, but periodic sync and manual sync failures went through a separate `sync-error` path that had no effect on the status dot.

## Decision
Unify all failure signals into a single "disconnected" UI state:

1. **`sync-error`** sets `connectionStatus` to `"disconnected"` (red dot) and displays the error message. No auto-clear timer.
2. **`idle-disconnected`** sets `connectionStatus` to `"disconnected"` (same as before).
3. **`sync-complete`** restores `connectionStatus` to `"connected"` (green dot) and clears the error message. This is the only path (along with `idle-reconnected`) that clears the error state.
4. **`idle-reconnected`** restores `connectionStatus` to `"connected"` and clears the error (same as before).

### Event flow

```
Network down:
  IDLE thread detects drop   ──► idle-disconnected  ──► red dot, "Offline"
  Periodic/manual sync fails ──► sync-error         ──► red dot, error message

Network restored:
  IDLE thread reconnects     ──► idle-reconnected   ──► green dot, error cleared
  Next sync succeeds         ──► sync-complete       ──► green dot, error cleared
```

### Why no auto-clear timer
A 10-second timer created a false sense of connectivity. If the network is still down, the next sync attempt will fail again anyway, producing a flash of "connected" followed by another error. Persistent error display until actual recovery is more honest and less confusing.

## Consequences
- The status bar accurately reflects network/sync state at all times
- Error messages persist until the problem is actually resolved (successful sync or IDLE reconnect)
- Both IDLE disconnects and sync failures produce the same visual signal (red dot)
- No flickering between error and connected states during extended outages
