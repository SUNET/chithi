# ADR 0051: Rust architecture boundary refactor

## Status

Accepted

## Date

2026-08-07

## Context

ADR 0037 introduced the current sync architecture: a read/write SQLite pool,
per-account operation workers, optimistic user operations, offline replay,
and independent mail/calendar/contact sync. ADR 0050 then introduced
per-provider backend traits for mail, calendar and contacts, with the goal
that command handlers resolve a backend and make trait calls instead of
matching provider strings directly.

A follow-up architecture audit of the Rust crate (`src-tauri`) found that the
codebase is part-way through that transition. The provider backend traits are
valuable and should remain the direction, but several older boundaries still
leak through:

- Lower-level mail/backend modules import `commands::*` helpers, especially
  event emission and filtering helpers. This makes the Tauri command layer a
  hidden service layer.
- Command handlers still switch directly on provider/protocol strings for
  server search, body fetch, drafts, mail actions, calendar scheduling, room
  lookup and RSVP paths.
- `MailOpExecutor` exists, but Graph/JMAP actions are often performed by
  ad-hoc spawned command code rather than the worker/offline/coalescing path.
- Provider identity and remote message references are encoded in strings such
  as legacy `graph:` maildir sentinels and underscore-concatenated message ids,
  then parsed in command code.
- `MailSyncCtx` and several sync/provider modules depend directly on Tauri
  runtime objects (`AppHandle`, event names/payloads), making providers hard
  to test outside the desktop app.
- `AppState` is a broad service locator for unrelated domains: DB, IDLE/push
  state, operation workers, attachment tokens, OAuth/SSO
  sessions, PGP keystore/cache and pending prompts.
- Some architecture issues are also correctness risks: manual SQLite
  transactions can leak open transactions on early error, IDLE/push
  suspend/resume is not consistently exception-safe, and worker/push
  lifecycles are not supervised deterministically.

We want to continue the direction established by ADR 0037 and ADR 0050, but
without a large rewrite. The refactor should be incremental, behavior-preserving
where possible, and split into reviewable phases.

## Refinement of prior ADRs

This ADR refines and continues ADR 0037 and ADR 0050; it does not replace them
wholesale.

The explicit refinements are:

1. **ADR 0037 Graph/JMAP operation execution.** ADR 0037 allowed JMAP and
   Graph user operations to remain ad-hoc async HTTP calls because they do not
   benefit from persistent IMAP-style connections. ADR 0051 revises that point:
   the reason to route Graph/JMAP operations through the operation pipeline is
   not connection reuse, but one authoritative contract for coalescing, offline
   replay, retry/failure reporting and operation visibility. Optimistic UI
   semantics from ADR 0037 remain required.
2. **ADR 0050 event-emission non-goal.** ADR 0050 kept frontend event emission
   in `commands/` as cross-provider orchestration. ADR 0051 refines that by
   moving reusable event helper plumbing and future event-sink abstractions out
   of `commands` so lower layers do not import command modules. User-facing
   orchestration decisions and frontend event contracts remain command/service
   concerns.
3. **ADR 0050 `MailOpExecutor` decision.** ADR 0051 completes and enforces ADR
   0050 decision 7: the ops worker consumes the mail backend through
   `MailOpExecutor`, and existing call sites should stop bypassing that path.
4. **ADR 0050 calendar invite non-goal.** Provider-specific remote API calls
   for scheduling, room lookup or RSVP may move behind calendar backend
   capabilities. iTIP parsing/composition, DTO validation, local persistence,
   invite orchestration and frontend event emission remain command/service-owned
   unless a future ADR explicitly supersedes ADR 0050.
5. **ADR 0050 push/IDLE non-goal.** Provider-specific push/IDLE start mechanics
   remain provider-specific. Shared lifecycle supervision may track handles,
   shutdown state and restart safety, but must not erase protocol-specific
   implementation boundaries.
6. **ADR 0050 contact `sync_type` compatibility.** Graph contact books continue
   to persist legacy `contact_books.sync_type = 'o365'`, and push resolution
   remains keyed on the book's `sync_type`. ADR 0051 allows lookup-boundary
   normalization only; no `contact_books.sync_type` migration is part of this
   ADR.
7. **ADR 0025 Graph body download timing.** ADR 0025 described normal Graph
   sync as downloading full MIME before inserting each message. The current
   delta-sync architecture instead inserts envelope metadata with a
   `graph:{message_id}` remote-body marker, then streams full MIME to Maildir
   during explicit prefetch or the first body request. ADR 0051 accepts that
   timing while preserving ADR 0025's bounded-memory streaming, short database
   lock durations, partial-file cleanup and offline-local-body goals. Once
   fetched, the marker is replaced with the relative Maildir path.

## Compatibility and security invariants

Every phase of this refactor must preserve these prior-ADR invariants.

### Account, credentials and OAuth

- Passwords remain out of SQLite and are loaded through the OS keyring boundary
  from ADR 0011. Focused account/auth config structs must not reintroduce
  password persistence or expose raw secrets more broadly.
- Stored passwords and tokens remain Rust-backend-only. IPC responses that
  currently hide stored credentials, such as account configuration reads,
  continue returning empty password fields for existing credentials (ADR 0028).
- Google OAuth keeps the Desktop-client behavior from ADR 0030: PKCE plus
  `client_secret`, token exchange with both `code_verifier` and
  `client_secret`, and authorization URLs with `access_type=offline` and
  `prompt=consent`.
- Microsoft OAuth remains a true public-client PKCE flow where applicable, and
  Microsoft token handling must preserve distinct Graph and SMTP/Outlook
  resource scopes, refresh-token rotation, and pending-token migration to the
  real account UUID (ADR 0025, ADR 0034).
- Gmail calendar/contacts routing continues to prefer the Google provider even
  when the mail protocol is `imap` and CalDAV/CardDAV fields exist (ADR 0016).
- Fastmail remains provider-specific UX/auth (`provider = 'fastmail'`) over
  JMAP protocol semantics (`mail_protocol = 'jmap'`, bearer JMAP config). New
  provider/message-reference helpers must not assume provider identity maps
  one-to-one with protocol (ADR 0049).

### HTML, remote content and renderer isolation

- Plain text remains the default mail view, and HTML bodies reaching the
  frontend remain Rust-sanitized with remote content blocked by default
  (ADR 0003).
- Untrusted HTML remains isolated in the sandboxed iframe model from ADR 0026:
  `sandbox="allow-scripts"`, no `allow-same-origin`, and validated
  `postMessage` only.
- The frontend continues to make zero direct provider/remote HTTP requests; API
  and provider calls stay in the Rust backend (ADR 0027).
- Remote image loading remains opt-in through the backend proxy from ADR 0032,
  not through renderer network access.
- Any centralized body/image-fetch helper must preserve ADR 0033 SSRF
  protections: hostname blocking, DNS resolution before fetch, rejection of all
  private/reserved resolved addresses, HTTPS-only behavior, timeout, size limit
  and content-type checks.
- Provider message body retrieval and opt-in remote image hydration are distinct
  paths. A generic body-fetch capability must not bypass the image proxy or
  renderer sandbox safeguards.

### Mail identity, sync and message semantics

- Persisted protocol fields and account-type immutability from ADR 0010 remain
  compatibility constraints. ADR 0051 removes protocol string switches from
  command handlers, not persisted protocol configuration.
- Current-folder-first sync ordering remains part of the sync/service boundary:
  requested current folder first, then INBOX, then other folders (ADR 0006).
- Threading semantics from ADR 0004 remain unchanged: sync-time thread
  computation, `messages.thread_id`, empty-string/NULL compatibility, per-folder
  threading, and same-folder child fetch behavior must survive message-reference
  refactors.
- JMAP URL handling keeps ADR 0008's direct `reqwest` implementation,
  per-request authentication and string-preserving reverse-proxy URL rewriting
  for placeholders such as `{accountId}` and `{blobId}`. Refactors must not
  accidentally reintroduce the `jmap-client` HTTP layer for these paths.
- JMAP send remains JMAP Submission where that path is used: upload blob,
  `Email/import`, `EmailSubmission/set`, and dynamic identity lookup (ADR 0009).
- Microsoft 365 mail send remains SMTP+XOAUTH2, not Graph `sendMail`, to avoid
  the DMARC and personal-account issues from ADR 0025. If send ever moves into
  an executor path, the executor must preserve this transport choice.
- OpenPGP-enabled outgoing mail preserves ADR 0047's raw-MIME invariant. The
  PGP-wrapped bytes are persisted to outbox and sent/replayed verbatim unless a
  future ADR explicitly changes the send architecture.
- Graph delta sync may persist envelope metadata with a `graph:{message_id}`
  remote-body marker. Explicit prefetch or the first body request must stream
  full MIME to Maildir with bounded memory and without holding the DB lock,
  then replace the marker in a short DB transaction. Failed downloads must not
  replace the marker with a local path, and partial files must be cleaned up.
- IMAP TLS mode remains port-authoritative (993 = implicit TLS; other ports =
  STARTTLS) and plaintext authentication remains unsupported (ADR 0024).
- IMAP sync optimizations from ADR 0041 and ADR 0023 remain regression
  constraints: `UIDNEXT`/`EXISTS` preflight, batch flag sync, no per-message DB
  lookup regression, 1000-envelope batch insert behavior, and short DB lock
  durations.

### Filtering and mail operations

- Filtering remains client-side, SQLite-backed and per-account as in ADR 0002.
  Provider capabilities may execute filter actions, but providers do not own
  filter rule evaluation and this ADR does not introduce server-side filters.
- Graph filter rules remain explicitly unsupported unless a later ADR adds
  support. Unsupported filter/action behavior must be explicit, not a silent
  fallback to another provider path (ADR 0025).
- System-folder deletion guards from ADR 0036 must live in the authoritative
  service path if folder deletion/mutation logic moves: local folder existence
  checks and the `folder_type` denylist for inbox/sent/drafts/trash/junk/archive
  must not remain only in a Tauri wrapper that worker/executor paths can bypass.
- Optimistic UI semantics from ADR 0037 remain required for move/delete/flag
  migrations: local state updates quickly, remote failures surface through
  existing failure/offline events, and sync reconciles later.
- Send migration is not part of the first mail-operation pipeline phase. If a
  later phase moves send into the worker/executor/offline path, it must
  explicitly preserve ADR 0039 compose-close latency, outbox persistence before
  network send, `send-started`/`send-complete`/`send-failed` events and
  operations-panel visibility. It must also explicitly revise ADR 0039's
  consequence that automatic send retry was not implemented yet.

### Calendar and contacts

- Calendar invite delivery ownership from ADR 0038 and ADR 0040 remains
  explicit: Gmail/O365 provider servers send invites where supported, while JMAP
  and generic IMAP/CalDAV self-send when providers do not. The refactor must not
  reintroduce duplicate manual SMTP invites for Gmail/O365.
- Calendar RSVP/invite processing keeps provider UID/remote-id behavior from
  ADR 0040: Google `iCalUID`, Graph `iCalUId`, stored `remote_id`, Graph API
  RSVP side effects, and immediate `calendar-changed` after invite/reply/cancel
  processing.
- `METHOD:CANCEL` processing keeps organizer-email verification before deleting
  local events (ADR 0040).
- Client-side `METHOD:REPLY` processing for Stalwart remains required (ADR
  0015): local `attendees_json` update, JMAP `CalendarEvent/set` participant
  patch, organizer-only behavior and default organizer status of `accepted`.
- Calendar body-fetch refactors are not expected to solve invite email
  auto-processing immediately, but typed body-fetch services should not make
  that future improvement harder (ADR 0040).
- Graph calendar access preserves Graph-specific scopes, refresh-token handling
  and `Prefer: outlook.timezone="UTC"` on `calendarView` requests (ADR 0034).
- Graph room lookup preserves ADR 0046's layered endpoint strategy, `Place.Read.All`
  rationale, normalized `{ name, address }` output, and empty-result/free-text
  fallback semantics.
- Calendar timezone behavior from ADR 0042 remains required: UTC normalization
  on ingest, source timezone preservation, IANA timezone IDs, and round-trip
  fidelity for replies, updates and CalDAV writes.
- Google Calendar/People two-way sync from ADR 0017 remains provider-specific:
  Calendar API v3, People API, incremental `syncToken` with 410 Gone fallback,
  calendar `remote_id` use, RSVP import/patch behavior and contact identity via
  Google `resourceName`.
- Shared contact reconciliation means shared local upsert/delete/matching
  semantics, not merged protocol clients or discovery flows. CardDAV remains
  distinct from CalDAV while sharing WebDAV helpers (ADR 0022).
- The `caldav_url` double-duty compatibility for CalDAV/CardDAV remains unless
  a later migration explicitly changes it (ADR 0022).
- Compose autocomplete IPC behavior from ADR 0021 remains stable if contacts
  are refactored: full contacts and collected contacts are searched in parallel,
  deduplicated with full contacts preferred, and the compose window does not
  depend on shared frontend stores.

### Runtime state, push and PGP

- IMAP push keeps ADR 0018's accepted model unless a future ADR changes it: one
  blocking OS thread per IMAP account, stop flags in app state, `idle-new-mail`
  triggering sync, reconnect after errors, and 2-minute polling fallback for
  non-INBOX folders.
- JMAP push keeps ADR 0020's model: one Tokio EventSource task per JMAP account,
  unified `start_idle`/`stop_idle`, `eventSourceUrl` handling, `ping=30`, read
  timeout, reconnect backoff, graceful skip when no `eventSourceUrl` exists and
  the same frontend events as IMAP (`idle-new-mail`, `idle-disconnected`,
  `idle-reconnected`).
- Status-bar recovery semantics from ADR 0019 remain stable: `sync-error` and
  `idle-disconnected` enter disconnected/error state; `sync-complete` and
  `idle-reconnected` are recovery paths; errors do not auto-clear on a timer.
- Worker lifecycle supervision must preserve the ADR 0037 IMAP safety invariant:
  IMAP session state moved into `spawn_blocking` is single-threaded and never
  shared concurrently.
- PGP keystore use preserves ADR 0047's model: shared tumpa keystore, lazy open,
  synchronous `libtumpa` API behind the appropriate blocking boundary, and
  process-lifetime zeroizing credential cache. Where current code violates the
  blocking-boundary intent, the refactor restores/enforces ADR 0047 rather than
  changing the keystore model.
- PGP state extraction must preserve ADR 0048's `acquire_secret` as the sole
  policy owner for tcli-agent vs local prompt selection, shared cache behavior,
  prompt routing and targeted eviction.
- Graph encrypted drafts preserve ADR 0047's explicit plaintext fallback and
  `DraftSaveOutcome.plaintext_fallback` notice semantics.
- Backend-owned dialogs and attachment security from ADR 0029 remain adapter
  boundary responsibilities: renderer supplies suggested filenames, not
  destination paths; attachment bytes do not cross IPC; symlink/path checks stay
  backend-side. Tauri decoupling applies to sync/provider internals, not to
  commands that intentionally require Tauri APIs for secure UX.

## Decision

Refactor the Rust architecture in priority order, starting with correctness
and dependency direction before expanding provider capabilities.

### 1. Stabilize lifecycle and transaction safety first

Before moving large boundaries, fix the architecture issues that can cause
incorrect runtime state:

1. Replace manual `BEGIN`/`COMMIT` transaction blocks on pooled SQLite writer
   connections with rollback-safe transaction helpers or `rusqlite::Transaction`
   while preserving ADR 0023's batch sizes and short lock durations. A fallible
   operation inside a transaction must not return the writer connection to the
   pool while still inside an open transaction.
2. Make push suspension exception-safe. Any path that suspends IMAP IDLE for an
   operation must resume it on both success and failure.
3. Improve IMAP IDLE and JMAP EventSource shutdown/restart semantics so
   `stop_idle` does not report a clean stop while an old thread/task/connection
   can keep running and race with a restarted push loop.
4. Store lifecycle state for background account workers so workers can be
   supervised, shut down and observed instead of only keeping their `mpsc`
   senders. This supervision must not introduce shared concurrent access to
   IMAP session state.
5. Enforce the ADR 0047 blocking boundary for synchronous PGP keystore,
   crypto and smartcard operations where current commands run them directly on
   async runtime workers.

These fixes are not optional cleanup: they protect the refactor from building
new abstractions on top of unsafe lifecycle assumptions.

### 2. Re-establish dependency direction

The command layer is an outer adapter. Lower layers must not depend on it.

Move shared command-layer helpers into non-command modules:

- Move app event helpers such as `emit_messages_changed` and
  `emit_folders_changed` out of `commands::events` into an app/service event
  module. Frontend-visible event names, payloads and recovery semantics remain
  unchanged.
- Move sync-time filtering logic out of `commands::filters` into a filtering
  service module. Tauri commands remain as thin IPC wrappers around that
  service. Rule evaluation remains client-side and per-account.

After this phase, dependencies should point in this direction:

```text
commands
  -> services / filters / backend

backend / mail sync
  -> services / filters / events

backend / mail sync
  -X-> commands
```

This phase must not broaden Tauri window capabilities or require new frontend
permissions.

### 3. Make the mail operation pipeline authoritative

Complete ADR 0050 decision 7 by making the operation worker/offline/coalescing
architecture authoritative for user mail actions across providers, unless a
provider has a documented reason not to participate.

Move Graph and JMAP user operations into the `MailOpExecutor` path instead of
spawning ad-hoc HTTP work from command handlers. Migrate one operation type at
a time, beginning with the smallest/safest operation:

1. flags
2. delete
3. move
4. copy

Send/raw submission is deliberately deferred from this first migration because
ADR 0009, ADR 0025, ADR 0039 and ADR 0047 impose distinct transport, UX,
outbox and OpenPGP invariants. A later send-pipeline change must explicitly
preserve or supersede those ADRs.

Commands may still perform optimistic local DB updates, but remote execution,
failure reporting and offline retry should flow through one operation contract.
Each migrated operation type needs regression coverage for optimistic UI,
offline queueing, `op-failed`/operations-panel visibility and provider-specific
unsupported behavior.

### 4. Centralize provider message references and body locations

Do not keep spreading provider-specific string parsing through command code.
Introduce typed compatibility helpers for existing persisted formats:

- `BackendMessageRef` for IMAP/JMAP/Graph remote message identities.
- `BodyLocation` for local bodies, legacy/not-yet-fetched remote body markers
  and not-yet-fetched bodies.

Existing database strings remain compatible at first. The first goal is to
centralize parsing and formatting so commands no longer call `splitn(3, '_')`,
`strip_prefix("graph:")` or similar provider-specific parsing directly.

This is a compatibility/parser refactor, not a change to normal sync storage
semantics. In particular, Graph delta sync continues to persist
`graph:{message_id}` until explicit prefetch or the first body request streams
full MIME to Maildir and replaces the marker with the relative local path.

### 5. Expand provider traits by capability, not by rewrite

Once dependency direction and operation execution are cleaner, add provider
capabilities in small, reviewable slices.

Mail capabilities to add or split out:

- server-side search
- body fetch / ensure body on disk, subject to the rendering and remote-content
  invariants above
- draft save, preserving ADR 0013 and ADR 0047 semantics
- filter action execution where appropriate, while rule evaluation stays in the
  filtering service

Calendar capabilities to add or split out:

- room suggestions
- room availability
- participant schedules
- provider-specific remote RSVP calls

Provider traits may use optional capability traits or explicit unsupported
outcomes rather than forcing every backend to implement every feature.
Unsupported behavior must be explicit, not an accidental empty vector, silent
no-op or fallback to another provider path.

### 6. Decouple backend contexts from Tauri

Backend and sync modules should not depend directly on `tauri::AppHandle` or
frontend event names. Introduce an application event sink abstraction, with a
Tauri implementation at the adapter boundary and fake implementations for tests.

`MailSyncCtx` and related contexts should move toward narrow dependencies:

- database access
- data directory / storage access
- event sink
- provider/token/client factories where needed

This is a later phase because it touches many signatures, but it is the target
for testability. This decoupling does not apply to adapter-boundary commands
that intentionally require Tauri APIs, such as native dialogs.

### 7. Split domain models and service state gradually

`AccountFull` remains the compatibility/loading aggregate for now, but new
provider/service APIs should prefer focused views:

- mail account config
- calendar account config
- contacts account config
- auth config
- PGP settings

Focused views must preserve the keyring, OAuth, provider-routing and legacy
field semantics listed above.

`AppState` may remain the single Tauri-managed root, but its internals should
be grouped into focused domain state structs over time:

- mail runtime and push lifecycle state
- OAuth/session state
- attachment store
- PGP state
- meet state

Shared domain types should move out of `db` and `mail` where needed so the
persistence layer does not depend on mail parsing/provider implementation
modules.

### 8. Clean up legacy provider compatibility at the boundary

Legacy compatibility stays supported, but it should be isolated:

- Unknown non-empty mail protocols should eventually fail explicitly instead of
  falling back to IMAP, after existing account data has been audited or migrated.
- Legacy aliases such as contacts `sync_type = 'o365'` should be normalized at
  lookup boundaries, not spread through new provider logic. Persisted values and
  book-keyed push resolution remain unchanged unless a later ADR/migration
  supersedes ADR 0050.

These changes come late because existing user data may rely on compatibility
behavior.

## Non-goals

- No large rewrite of the Rust crate.
- No immediate database migration for message ids, body locations or provider
  aliases.
- No immediate `contact_books.sync_type` migration; `o365` persists for Graph
  contact books as accepted in ADR 0050.
- No immediate removal of `AccountFull`; it remains the compatibility aggregate
  until focused account views have replaced it at service boundaries.
- No change to frontend behavior solely for architecture purity.
- No broadening of Tauri capabilities, renderer access to secrets, or renderer
  direct network/provider access.
- No replacement of ADR 0037 or ADR 0050. This ADR explicitly refines and
  continues those decisions as described above.
- No send-pipeline migration in the first operation-pipeline phase; send needs a
  separate focused decision because it crosses transport, outbox, UX and
  OpenPGP invariants.

## Refactor sequence

Use small PRs in approximately this order:

1. Rollback-safe SQLite transaction handling that preserves batch performance.
2. Exception-safe IMAP IDLE resume after prefetch/sync failures.
3. Deterministic-enough IMAP IDLE and JMAP EventSource shutdown/restart
   semantics.
4. PGP blocking-boundary enforcement for synchronous keystore/crypto/card work.
5. App event helpers moved out of `commands` without changing event contracts.
6. Sync-time filtering moved out of `commands::filters` while preserving
   client-side per-account rule evaluation.
7. Provider message/body-location compatibility helpers introduced.
8. One mail action type moved into the worker/executor path.
9. Remaining non-send mail action types moved into the worker/executor path.
10. Worker lifecycle handles stored and supervised.
11. Mail search capability moved behind provider backend.
12. Mail body-fetch capability moved behind provider backend.
13. Draft-save capability moved behind provider backend.
14. Calendar scheduling/room/provider-RSVP capabilities moved behind calendar
    backend while iTIP orchestration remains service/command-owned.
15. Event sink abstraction introduced for backend/sync modules.
16. Injectable HTTP/token clients introduced with provider-specific OAuth and
    token-scope behavior preserved.
17. Focused account config structs introduced for new service/provider APIs.
18. Shared domain types moved out of `db`/`mail` where dependency direction is
    currently inverted.
19. Contact reconciliation shared across providers without merging protocol
    clients/discovery.
    The Graph remote-ID and JMAP remote-ID-then-UID checkpoints use the shared
    reconciler as of 2026-08-22. Google and CardDAV still require complete,
    fail-closed snapshot adapters before this sequence item is complete.
20. Unknown-provider IMAP fallback removed after compatibility audit.
21. Legacy provider aliases normalized at lookup boundaries only.

## Consequences

### Positive

- The command layer becomes a thin adapter/orchestration boundary instead of a
  hidden service layer.
- Provider behavior becomes easier to extend because new providers implement
  capabilities instead of requiring edits across many command string switches.
- Mail operation retry/coalescing/offline semantics become consistent across
  IMAP, JMAP and Graph for migrated non-send actions.
- Existing string-encoded message/body formats remain compatible while parsing
  becomes centralized and testable.
- Sync/provider modules become easier to test without a live Tauri runtime.
- Lifecycle and transaction behavior become safer before deeper refactors.
- Prior security invariants around credentials, renderer isolation, remote
  content and backend-owned file operations stay explicit during refactoring.

### Negative

- The transition will temporarily add wrapper/helper modules while old call
  sites are migrated.
- Some provider traits will grow or split into capability traits, requiring
  careful naming to avoid another partial abstraction.
- Moving operations into the worker path may change timing and failure
  reporting behavior, so each operation type needs focused regression testing.
- Removing the unknown-provider IMAP fallback must wait until existing account
  data has been audited or migrated.
- Calendar provider-capability extraction will leave some command/service-owned
  iTIP orchestration in place, so the architecture remains intentionally mixed
  until a future ADR chooses a deeper calendar workflow refactor.

### Compatibility

Existing accounts, service bindings, message ids, body-location strings,
keyring entries, OAuth tokens, contact book `sync_type` values and frontend
event contracts must continue to work throughout the refactor. Compatibility
parsing should move to centralized helpers first; persistence changes can be
considered only after the new boundaries are stable and a later ADR/migration
accepts the data-model change.
