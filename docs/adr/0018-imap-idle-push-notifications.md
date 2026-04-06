# ADR 0018: IMAP IDLE for Push Email Notifications

## Status
Accepted

## Context
Email sync was polling every 2 minutes, meaning new emails could take up to 2 minutes to appear. Desktop email clients like Thunderbird use IMAP IDLE (RFC 2177) to get near-instant notification when new mail arrives.

## Decision
Add a persistent IMAP IDLE connection on INBOX for each IMAP account. When the server signals new mail, immediately trigger a sync.

### Architecture

**Thread-per-account model:**
- Each enabled IMAP account gets a dedicated OS thread running the IDLE loop
- The `imap` crate is synchronous/blocking, so IDLE naturally blocks the thread
- Threads are managed via `IdleHandle` in `AppState` with `Arc<AtomicBool>` stop flags
- Tauri events (`idle-new-mail`) bridge from the blocking thread to the async frontend

**IDLE loop lifecycle:**
```
Thread start
  └─ Connect to IMAP server
      └─ SELECT INBOX
          └─ Loop:
              ├─ IDLE (blocks up to 25 minutes)
              ├─ Server notification? → emit "idle-new-mail" event
              ├─ Timeout? → re-enter IDLE
              └─ Error? → logout, wait 30s, reconnect
```

**Timing:**
- IDLE timeout: 25 minutes (under the 29-min RFC 2177 limit)
- Keepalive: every 5 minutes (prevents NAT/firewall timeout)
- Reconnect delay: 30 seconds after error
- Re-enter IDLE immediately after processing notification

**Frontend integration:**
- `startIdle()` called on MailView mount
- `idle-new-mail` event → `triggerSync()` for that account
- `stopIdle()` called on unmount, sets stop flags for graceful exit
- Periodic 2-minute polling kept as fallback for non-INBOX folders

### What IDLE covers vs what it doesn't

| Covered | Not covered |
|---------|-------------|
| New mail in INBOX | New mail in other folders |
| Mail expunged from INBOX | Flag changes from other clients |
| INBOX EXISTS/RECENT notifications | JMAP accounts (use EventSource later) |

Other folders continue to rely on the 2-minute periodic sync.

### Why not async IDLE?
The `imap` crate is synchronous. Using `async-imap` would require migrating the entire IMAP implementation. The blocking thread approach is simple, reliable, and the thread cost is minimal (one thread per IMAP account, mostly sleeping in IDLE).

## Consequences
- New emails appear within seconds of delivery to INBOX
- One OS thread per IMAP account is consumed (acceptable for 1-5 accounts)
- If the IMAP connection drops, auto-reconnect after 30 seconds
- The 2-minute polling interval remains as fallback for non-INBOX folders and as a safety net
- JMAP push notifications (EventSource) can be added separately in the future
