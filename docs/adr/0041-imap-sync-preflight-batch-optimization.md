# ADR 0041: IMAP Sync Preflight and Batch Flag Optimization

## Status
Accepted

## Context

The DB connection pool (ADR 0037, Phase 1) replaced a single `Arc<Mutex<Connection>>` with 1 writer + 4 readers. This made the parallel IMAP sync phase truly concurrent for the first time — previously all 4 sync threads serialized on the mutex, spending most of their time waiting.

This surfaced a latent performance problem: `sync_folder_envelopes` performed three expensive operations for **every folder on every sync**, even dormant ones:

1. **Deletion reconciliation** — `UID FETCH 1:*` to get all server UIDs, then per-message comparison against local DB
2. **Flag sync** — `UID FETCH 1:* (UID FLAGS)` from server, then `sync_flags_by_uid` which executed **one SELECT query per message** to compare flags
3. **New envelope fetch** — `UID FETCH {last_uid}:*` for new messages

With 39,000 messages across 34 folders (Trash alone at 15,000), this meant ~39,000 individual SQLite SELECT queries per sync cycle — now running on 4 concurrent threads instead of serialized. The CPU spike was noticeable.

### Key insight

Most folders are dormant on any given sync cycle. Only INBOX and a few active folders receive new messages. The IMAP SELECT command already returns `UIDNEXT` (next UID the server will assign) and `EXISTS` (message count) — if both match what we stored from the last sync, the folder is provably unchanged and all three phases can be skipped.

## Decision

### 1. UIDNEXT/EXISTS preflight check

After `SELECT`ing a folder, compare the server's `uid_next` and `exists` against stored values from the last successful sync. If both match, the folder is unchanged — skip deletion reconciliation, flag sync, and envelope fetch entirely.

```rust
let (exists, _uid_validity, uid_next) = conn_imap.select_folder(folder_path)?;

if last_uid > 0 && stored_uid_next > 0
    && uid_next == stored_uid_next
    && exists as i64 == stored_total
{
    log::debug!("Folder '{}' unchanged, skipping", folder_path);
    return Ok(0);
}
```

This requires:
- `select_folder` returning `uid_next` (already available from the `imap` crate's `Mailbox.uid_next`)
- A `uid_next` column on the `folders` table (added via migration)
- Storing `uid_next` after each successful sync alongside `total_count`

### 2. Batch flag sync

Replace the per-message SELECT loop in `sync_flags_by_uid` with a single bulk query:

**Before** (N queries per folder):
```rust
for (uid, new_flags) in uid_flags {
    let (id, current_flags) = stmt.query_row(
        "SELECT id, flags WHERE uid = ?", [uid]  // 1 query per message
    )?;
    if current_flags != new_flags { UPDATE ... }
}
```

**After** (1 query per folder):
```rust
// Load entire folder into HashMap in one query
let local: HashMap<u32, (String, String)> = stmt.query_map(
    "SELECT uid, id, flags WHERE folder_path = ?", [folder_path]
)?;
// Compare in memory
for (uid, new_flags) in uid_flags {
    if let Some((id, current)) = local.get(uid) {
        if current != new_flags { UPDATE ... }
    }
}
```

### 3. Combined impact

| Metric | Before | After |
|--------|--------|-------|
| Folders processed per sync | 34 (all) | ~4 (active only) |
| Flag sync queries per folder | N (one per message) | 1 (bulk) |
| Total queries per sync cycle | ~39,000 | ~10 |
| Dormant folder cost | Full IMAP fetch + N queries | 1 SELECT (preflight) |

### 4. Serialize sync passes for the same folder

An atomic forward-only watermark update is meaningful only within one
UIDVALIDITY epoch. The database writer mutex serializes transactions, but
does not prevent an old sync pass from writing after another pass resets
the folder's epoch.

Each shared `DbPool` therefore owns a weak lock registry keyed by account
ID and raw folder path. All IMAP envelope-sync paths use a common helper
that acquires the lock before reading the checkpoint or issuing its
SELECT, and holds it through reconciliation, filters, and final counts/UID
metadata. Callers may establish connections before entering this helper.
The command-path backend leaves folder selection to the helper rather than
issuing a redundant SELECT outside the lock.
Database reader and writer guards remain short-lived and are acquired afterward.
Different folders, accounts, and pool instances have independent locks;
unused registry entries are pruned on subsequent lookups.

The guard belongs to the synchronous pass itself. Errors and panics release
it, but cancelling an async caller does not release it while its detached
blocking work is still running. This coordinates envelope-sync passes,
not every user operation or body-prefetch task.

### 5. Merge envelope FETCH attributes by requested UID

A command can receive several FETCH responses for one UID, including
unsolicited flag-only updates. Envelope fetching therefore merges only
attributes actually present, scoped to the UIDs requested in that command.
Later flag updates do not clear headers or size; explicit empty flags do
clear earlier flags.

After successful command completion, emit one envelope per UID in the
order its first header arrived. Duplicate input UIDs are removed before
chunking, and unrequested UIDs are ignored. A requested UID with a missing
header literal or a header parse error is reported as failed for retry,
rather than inserting a blank message. A present zero-byte header literal
remains a valid result.

A header parse error is terminal for that UID within the current command:
duplicate FETCH responses cannot mask it. Other UIDs remain usable, and a
later command starts with fresh parse state. Malformed message headers do
not poison an otherwise synchronized IMAP connection. Header parsing retains
the underlying library's existing tolerance; only actual parse failures
trigger this retry behavior.

## Consequences

### Positive
- Sync CPU usage drops dramatically (39k queries → ~10)
- Dormant folders (80%+) are skipped after a single IMAP SELECT
- Active folders use 1 bulk query instead of per-message lookups
- No behavioral change — preflight only triggers when folder is provably unchanged

### Negative
- A same-folder refresh waits for an active pass to finish, which can delay
  queued work during a long sync. Unrelated folders can still sync in parallel.
- If a server doesn't report `UIDNEXT` (returns 0), the preflight is skipped and full sync runs (safe fallback)
- The preflight check relies on `UIDNEXT` + `EXISTS` being sufficient indicators of change. Edge case: if a message is deleted and another added (EXISTS unchanged, UIDNEXT incremented), this is correctly detected because UIDNEXT changes
- The `uid_next` column adds a small schema migration (ALTER TABLE on existing DBs)
- Batch flag sync loads the entire folder's message list into memory. For a 15k-message folder, this is ~1-2MB of HashMap data — acceptable for a desktop app

### Files changed
- `src-tauri/src/mail/imap.rs` — `select_folder` returns `(exists, uid_validity, uid_next)`
- `src-tauri/src/mail/sync.rs` — preflight check in `sync_folder_envelopes`, store `uid_next` after sync
- `src-tauri/src/db/folders.rs` — `get_folder_sync_state`, `update_uid_next`
  (later superseded by `update_uid_state` to update UIDVALIDITY atomically)
- `src-tauri/src/db/messages.rs` — `sync_flags_by_uid` rewritten to bulk query + HashMap
- `src-tauri/src/db/schema.rs` — `uid_next` column in CREATE TABLE + ALTER TABLE migration
- `src-tauri/src/db/pool.rs` — per-account/folder IMAP sync lock registry
