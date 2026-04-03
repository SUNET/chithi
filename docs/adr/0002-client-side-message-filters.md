# ADR 0002: Client-side per-account message filtering

## Status

Accepted

## Date

2026-04-03

## Context

Email users need to automatically organize incoming messages — moving newsletters to folders, flagging messages from specific senders, deleting spam patterns, etc. Server-side filtering (Sieve/ManageSieve) is not universally supported across IMAP providers, and Gmail uses its own proprietary filter system that isn't accessible via IMAP.

We needed a filtering approach that works identically across all supported providers (standard IMAP, Gmail, and future JMAP/O365).

## Decision

Filters are implemented entirely client-side in the Rust backend, per-account:

- **Rules are scoped to a single account.** Each filter rule has an `account_id` foreign key. There are no global filters — each account manages its own rules independently. This avoids confusion when accounts have different folder structures.

- **Rules are stored in SQLite** (`filter_rules` table) with conditions and actions serialized as JSON. This keeps the schema simple and allows flexible condition/action types without schema migrations.

- **The matching engine** (`filters/engine.rs`) evaluates conditions against message envelope data (from, to, cc, subject, size, has_attachments). It supports AND/OR condition groups, 7 operators (contains, equals, regex, greater/less than, etc.), and 8 action types (move, copy, delete, flag, mark read/unread, stop processing). Rules run in priority order with optional stop-after-match.

- **Filters run at two points:**
  1. **During sync** — automatically applied to newly synced envelopes, using the already-open IMAP connection. Errors are logged but do not fail the sync.
  2. **On demand** — user can apply filters to an existing folder via the UI ("Apply Filters to Folder").

- **IMAP actions are executed server-side** — move, copy, delete, and flag changes are sent to the IMAP server so they persist across clients. The local SQLite index is updated accordingly.

## Consequences

- Filters work identically on all IMAP providers without requiring server-side Sieve support.
- Filter rules are local to this client — they don't sync to other email clients or webmail. Users who want server-side rules should configure those separately.
- Filtering during sync adds minimal overhead since it operates on envelope metadata already in memory and reuses the open IMAP connection.
- Regex support uses the `regex` crate with graceful error handling — invalid patterns are logged and treated as non-matching rather than crashing.
- Future work: Sieve export (generate Sieve scripts from local rules for servers that support ManageSieve).
