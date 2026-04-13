# ADR 0037: Sync Architecture Overhaul

## Status
Accepted

## Context

The original sync architecture suffered from several performance and UX issues:

1. **Single DB mutex bottleneck**: All database access (sync threads, user operations, UI queries) serialized on a single `Arc<Mutex<rusqlite::Connection>>`. During parallel IMAP sync with 4 threads, all threads would wait for this single lock. UI queries for listing messages or folders would block behind ongoing sync writes.

2. **Blocking user operations**: Move, delete, flag, and copy operations opened a new IMAP connection per action (500-2000ms TCP+TLS+LOGIN overhead) and waited for the full server round-trip before updating the UI. Users perceived the app as frozen during these operations.

3. **Coupled sync subsystems**: Calendar sync ran sequentially after mail sync (JMAP calendars awaited inline in `trigger_sync`). Contact sync was manual-only with no background schedule. A slow mail sync would delay calendar updates.

4. **No operation resilience**: Network failures during move/delete operations were silently lost. The `outbox` table existed in the schema but was never populated.

5. **No operation visibility**: Users had no way to see what background operations were running beyond a small spinner in the status bar.

Thunderbird's architecture (analyzed from `comm/mailnews/` source) provided the reference model: per-server URL command queues, persistent connections, thread-safe sink proxies, operation coalescing, offline operation queuing with deterministic replay, and fully independent mail/calendar/contacts subsystems.

## Decision

Overhaul the sync architecture in 7 incremental phases, each independently mergeable and testable.

### Phase 1: DB Connection Pool (read/write separation)

Replace `Arc<Mutex<rusqlite::Connection>>` with a custom `DbPool` that provides 1 async writer (`tokio::sync::Mutex`) and 4 sync readers (`std::sync::Mutex`). SQLite WAL mode (already enabled) allows concurrent readers with a single writer.

- **Writer**: `db.writer().await` — exclusive async access for INSERT/UPDATE/DELETE
- **Reader**: `db.reader()` — non-blocking round-robin access for SELECT queries (no `.await` needed)
- Each reader opens with `PRAGMA query_only = ON` for safety
- ~130 lock sites migrated across 10 files (55 reads, 75 writes)

**Key file**: `src-tauri/src/db/pool.rs`

### Phase 2: Optimistic UI for move/delete

Move and delete operations update the local DB and emit UI events immediately, then run the server operation in a `tokio::spawn` background task. On server failure, an `op-failed` event is emitted and the next sync reconciles.

The frontend removes messages from local arrays before calling the API, so deletions appear instant. An `op-failed` event listener triggers `fetchMessages()` to reconcile if the background operation fails.

Flag changes (`set_message_flags`) already used this pattern — Phase 2 extended it to move and delete.

### Phase 3: Per-account operation queue with persistent connection

Each account gets a dedicated worker task (`AccountWorker`) with a persistent IMAP connection. All IMAP user operations (move, delete, flag, copy) route through this worker instead of opening a new connection per action.

- **Operation coalescing**: Multiple deletes merge into one, multiple moves to the same target merge, multiple flag changes with the same flags merge (inspired by Thunderbird's `nsImapMoveCoalescer`)
- **Priority ordering**: User operations (move/delete/flag) execute before background sync
- **Folder tracking**: The worker tracks the currently selected folder to skip redundant IMAP SELECT commands
- **Stale connection detection**: Reconnects if connection unused for >5 minutes
- **IDLE independence**: Worker has its own connection, so O365 accounts no longer need to suspend/resume IDLE for every user action

Workers are spawned lazily on first use via `AppState::get_op_sender()`.

JMAP and Graph operations continue to use ad-hoc async HTTP calls since they don't benefit from persistent connections.

**Key files**: `src-tauri/src/ops/queue.rs`, `src-tauri/src/ops/worker.rs`, `src-tauri/src/ops/coalesce.rs`

### Phase 4: Offline operation queue

Failed network operations are persisted to the existing `outbox` DB table and automatically replayed after the next successful sync.

- **Replay order** (matching Thunderbird's `nsImapOfflineSync`): flags -> moves -> copies -> deletes
- **Retry limit**: Operations that fail 5 times are marked `dead` and surfaced via `offline-queue-changed` event
- **Serialization**: `MailOp` round-trips to/from JSON for DB storage

**Key file**: `src-tauri/src/ops/offline.rs`

### Phase 5: Independent calendar sync

Calendar sync decoupled from mail sync. Previously JMAP calendars synced sequentially after mail in `trigger_sync`, and other providers were triggered from the frontend `sync-complete` handler.

Now calendar runs on its own 5-minute interval via `startCalendarSync()` in the calendar store. The backend emits `calendar-changed` events after sync completion.

### Phase 6: Independent contact sync

Contact sync was manual-only. Now runs on a 30-minute interval (matching Thunderbird's CardDAV default) with `contacts-changed` events.

### Phase 7: Frontend event architecture

- New `ops` store for centralized tracking of `op-failed` and `offline-queue-changed` events
- Activity store tracks calendar, contacts, and send events alongside mail sync
- Status bar shows operation failures and has a context-aware sync button

## Consequences

### Positive
- Parallel IMAP sync no longer serializes on a single DB mutex — readers are fully concurrent
- Move/delete/flag operations appear instant (optimistic UI)
- IMAP operations reuse a persistent connection (eliminates per-action connect overhead)
- Calendar and contact sync don't wait for mail sync
- Failed operations are retried automatically
- Users can see all background operations in the operations panel

### Negative
- `unsafe impl Send for ImapState` is required because `imap` crate's `Session` contains a `Receiver<UnsolicitedResponse>` that isn't `Sync`. This is safe because the state is only ever moved into `spawn_blocking` for single-threaded access — never shared. A `# Safety` doc-comment on the struct documents this invariant.
- Optimistic UI means the frontend may briefly show stale state if a server operation fails. On failure, `op-failed` triggers an immediate re-fetch from the DB, and the next sync reconciles with the server. JMAP and Graph failures are also persisted to the offline outbox for retry.
- The `outbox` table doesn't have a dedicated `replay_order` column (uses the existing schema to avoid a migration); replay order is computed at read time with a stable sort (by action type priority, then by insertion order)
- The DB reader pool recovers from mutex poisoning via `unwrap_or_else(|e| e.into_inner())` rather than panicking, which keeps the app running but may use a connection whose previous holder panicked mid-transaction

### Resilience features

- **Worker reconnect backoff**: Consecutive IMAP connection failures trigger exponential backoff (1s, 2s, 4s... max 60s) to avoid burning OAuth token refresh rate limits
- **Worker init failure**: If a worker fails to initialize (e.g., account deleted), it emits an `op-failed` event before exiting so the UI can surface the error
- **SyncAll coalescing**: When multiple sync requests are batched, the LAST `current_folder` value is kept (matching the user's most recent navigation)
- **Background send persistence**: Outgoing emails are saved to the outbox table before the background SMTP/JMAP task runs, surviving app crashes during send

### Module structure

```
src-tauri/src/
  db/pool.rs        — DbPool with read/write separation
  ops/
    mod.rs          — module declarations
    queue.rs        — MailOp, OpPriority, OpEntry
    worker.rs       — AccountWorker with persistent IMAP connection
    coalesce.rs     — operation batching
    offline.rs      — outbox table CRUD and replay
```
