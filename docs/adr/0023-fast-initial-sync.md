# ADR 0023: Fast Initial Sync for Large Accounts

## Status
Accepted

## Context
Fresh sync of a Gmail account with ~6,300 unique emails across 9 folders took 10+ minutes, and body prefetch took 6+ hours. This was unacceptable for first-time account setup.

Root causes identified:
- Single IMAP connection syncing all folders sequentially
- Gmail virtual folders (`[Gmail]/All Mail`, `[Gmail]/Important`) duplicating 20,000+ envelope fetches
- Per-message `SELECT COUNT(*)` for existence check (6,000 individual queries)
- Full UID list fetch for deletion reconciliation even on first sync (empty DB)
- `BODYSTRUCTURE` in FETCH command wasting bandwidth (unused — attachment detection uses `size > 10000`)
- Body prefetch: 1 body per IMAP command, 100 per cycle, single connection
- 2-minute sync timer restarting sync while previous one was still running

## Decision

### Envelope sync optimizations
1. **Parallel folder sync** — up to 4 IMAP connections, priority folders (current + INBOX) sequential first, remaining folders distributed round-robin across threads via `std::thread::scope`
2. **Skip Gmail virtual folders** — `[Gmail]/All Mail`, `[Gmail]/Important`, and `[Gmail]` are registered in DB (for move/delete) but envelopes are not synced
3. **Skip deletion reconciliation on first sync** — if `last_seen_uid == 0`, local DB is empty, nothing to reconcile
4. **Batch existence check** — load all local UIDs into `HashSet<u32>` with one query, check in-memory instead of per-message `SELECT`
5. **Remove BODYSTRUCTURE from FETCH** — `(UID ENVELOPE FLAGS RFC822.SIZE)` instead of `(UID ENVELOPE FLAGS RFC822.SIZE BODYSTRUCTURE)`
6. **Batch DB inserts** — wrap each 1000-envelope batch in `BEGIN`/`COMMIT` transaction
7. **Sync-in-progress guard** — `AtomicBool` per account prevents the 2-minute timer from starting a duplicate sync

### Body prefetch optimizations
1. **Batch body fetch** — `fetch_bodies_batch()` fetches up to 100 UIDs in a single `UID FETCH uid1,uid2,...,uid100 BODY[]` command
2. **Parallel prefetch connections** — up to 3 IMAP connections, each handling different folders concurrently
3. **1000 bodies per cycle** (up from 100)
4. **Prefetch-in-progress guard** — prevents duplicate prefetch cycles

### Tokio runtime context
Parallel threads spawned by `std::thread::scope` need access to the Tokio runtime for `block_on(db.lock())`. The runtime handle is captured before spawning and entered via `rt.enter()` in each thread.

## Consequences

**Envelope sync:**
- Gmail account (6,300 emails, 6 folders after skip): ~96 seconds (was 10+ minutes)
- Virtual folder skip eliminates ~20,000 duplicate fetches
- Parallel threads reuse the same DB via `Arc<Mutex<Connection>>`

**Body prefetch:**
- 13,455 bodies in ~2 minutes (was 6+ hours)
- ~1000 bodies every 5-10 seconds with parallel connections
- No duplicate fetches (in-progress guard)

**Incremental sync:**
- Deletion reconciliation still runs (needed for server-side deletes)
- But skipped on first sync where it was pure waste
