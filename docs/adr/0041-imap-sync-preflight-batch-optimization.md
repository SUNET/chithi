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

## Consequences

### Positive
- Sync CPU usage drops dramatically (39k queries → ~10)
- Dormant folders (80%+) are skipped after a single IMAP SELECT
- Active folders use 1 bulk query instead of per-message lookups
- No behavioral change — preflight only triggers when folder is provably unchanged

### Negative
- If a server doesn't report `UIDNEXT` (returns 0), the preflight is skipped and full sync runs (safe fallback)
- The preflight check relies on `UIDNEXT` + `EXISTS` being sufficient indicators of change. Edge case: if a message is deleted and another added (EXISTS unchanged, UIDNEXT incremented), this is correctly detected because UIDNEXT changes
- The `uid_next` column adds a small schema migration (ALTER TABLE on existing DBs)
- Batch flag sync loads the entire folder's message list into memory. For a 15k-message folder, this is ~1-2MB of HashMap data — acceptable for a desktop app

### Files changed
- `src-tauri/src/mail/imap.rs` — `select_folder` returns `(exists, uid_validity, uid_next)`
- `src-tauri/src/mail/sync.rs` — preflight check in `sync_folder_envelopes`, store `uid_next` after sync
- `src-tauri/src/db/folders.rs` — `get_folder_sync_state`, `update_uid_next` functions
- `src-tauri/src/db/messages.rs` — `sync_flags_by_uid` rewritten to bulk query + HashMap
- `src-tauri/src/db/schema.rs` — `uid_next` column in CREATE TABLE + ALTER TABLE migration
