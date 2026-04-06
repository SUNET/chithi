# ADR 0020: JMAP EventSource Push Notifications

## Status
Accepted

## Context
IMAP accounts use IDLE (RFC 2177) for near-instant new mail detection (ADR 0018). JMAP accounts relied solely on the 2-minute periodic sync, causing noticeable delay before new emails appeared.

RFC 8620 §7.3 defines an EventSource (Server-Sent Events) mechanism for JMAP push notifications. The JMAP session resource includes an `eventSourceUrl` template that clients can connect to for real-time state change notifications.

## Decision
Add a JMAP EventSource push loop that mirrors the IMAP IDLE architecture.

### Architecture

**Async task per JMAP account:**
- Each enabled JMAP account gets a dedicated Tokio task running the EventSource loop
- Uses `reqwest` to open a long-lived HTTP connection to the `eventSourceUrl`
- Parses the SSE stream manually (no external SSE crate — the protocol is simple)
- Tasks are managed via `JmapPushHandle` in `AppState` with `Arc<AtomicBool>` stop flags

**EventSource URL construction (RFC 8620 §7.3):**
```
{eventSourceUrl} template placeholders:
  {types}      → "*" (all state change types: Email, Mailbox, CalendarEvent, etc.)
  {closeafter} → "no" (keep connection open indefinitely)
  {ping}       → "30" (server pings every 30 seconds for keep-alive)
```

**Push loop lifecycle:**
```
Task start
  └─ Connect to JMAP server (fetch session)
      └─ Build EventSource URL from session template
          └─ Open SSE stream with Basic Auth
              └─ Loop:
                  ├─ "state" event → emit "idle-new-mail" (triggers sync)
                  ├─ "ping" event  → no-op (keep-alive)
                  ├─ Stream error  → emit "idle-disconnected", reconnect
                  └─ Stop flag     → exit
```

**Reconnect strategy (same as IMAP IDLE):**
- Initial delay: 5 seconds
- Exponential backoff: 5s → 10s → 20s → ... → 5 minutes max
- On reconnect: emit `idle-reconnected` + trigger sync (catch up on missed changes)
- Interruptible sleep: checks stop flag every second for graceful shutdown

### Frontend integration
JMAP push emits the same Tauri events as IMAP IDLE:
- `idle-new-mail` → triggers sync for that account
- `idle-disconnected` → StatusBar shows red dot + error (ADR 0019)
- `idle-reconnected` → StatusBar restores green dot

No frontend changes needed — the existing `MailView.vue` listeners and `StatusBar.vue` handle both protocols identically.

### Unified start/stop
The `start_idle` command now starts both IMAP IDLE threads and JMAP push tasks for all enabled accounts. `stop_idle` stops both. The frontend calls the same two commands regardless of account type.

### SSE parsing
The SSE protocol is parsed inline rather than using an external crate:
- Lines starting with `event:` set the event type
- Lines starting with `data:` accumulate the data payload
- Empty lines dispatch the accumulated event
- Comment lines (`:`) and `id:`/`retry:` are ignored

This is ~30 lines of parsing code, avoiding an additional dependency for a simple line-oriented protocol.

### Reverse proxy buffering (critical)

SSE behind a reverse proxy (nginx, HAProxy, etc.) is prone to **response buffering**: the proxy holds small SSE events in its buffer and never forwards them to the client. This caused the initial implementation to silently hang — no pings, no state change events.

**Client-side mitigations (applied):**
- `Cache-Control: no-cache` — standard HTTP header telling intermediaries not to buffer
- `X-Accel-Buffering: no` — nginx-specific header to disable `proxy_buffering`
- **Read timeout (90s)** — if no data arrives within 3× the ping interval (30s × 3), the connection is treated as dead. This triggers reconnect with backoff instead of hanging forever.

**Server-side fix (recommended for deployment):**
```nginx
location /jmap/eventsource/ {
    proxy_buffering off;
    proxy_cache off;
}
```

The read timeout is essential even with proper proxy config — it catches network-level dead connections that neither side detects (half-open TCP sockets, NAT timeout, etc.).

### Stalwart-specific notes

Stalwart's JMAP EventSource implementation follows RFC 8620 §7.3 with minor differences:
- Sends an additional `calendarAlert` event type (not in RFC 8620 core) — treated as a state change by our implementation
- Ping data `interval` field is in milliseconds, not seconds as the RFC specifies — we ignore the ping data payload, so this is harmless
- Minimum allowed ping interval: 30 seconds

## Consequences
- JMAP accounts now get near-instant notifications (same as IMAP IDLE)
- One Tokio task per JMAP account (lightweight compared to IMAP's OS thread)
- The 2-minute periodic sync remains as a fallback
- Servers without `eventSourceUrl` in their session gracefully skip push (logged at startup)
- Network disconnect/reconnect is handled with the same backoff strategy as IMAP
- StatusBar correctly reflects JMAP connection state (ADR 0019)
- Deployments behind reverse proxies need `proxy_buffering off` for the eventsource path
