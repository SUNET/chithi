# ADR 0050: Per-provider backend traits for mail, calendar and contacts

## Status

Accepted

## Date

2026-07-14

## Current implementation note

As of 2026-08-21, callers construct JMAP connections through
`JmapConnection::connect_with_clients`, injecting the separately
configured discovery and API HTTP clients. The convenience
`JmapConnection::connect` described in the historical decision below
has been removed.

Ordinary event field updates are deliberately not pushed for JMAP or
CalDAV. Both backends retain the calendar trait's successful no-op
policy; event creation, deletion and JMAP attendee responses remain
separate supported paths. The original context and decision text are
retained unchanged as a historical record.

### Calendar mutation safety amendment (2026-09-13, #288)

[ADR 0052](0052-calendar-occurrence-mutation-safety.md) adds persisted,
provider-neutral `recurrence_kind`. Transport/ICS adapters classify source
metadata before lossy RRULE conversion, and backend sync persists that
classification on new and existing rows. The shared event model defines
ordinary mutation eligibility; command orchestration rereads persisted
targets and enforces it before local writes, meeting cleanup or provider
calls. The backend `move_event_to_calendar` command owns source preflight,
cross-account copy and guarded source deletion.

Ordinary notifications use standalone-only `notify_calendar_event`, accepting
only an event ID and deriving account, organizer eligibility and attendees
from persisted state without rewriting attendee metadata;
creation invitations use `send_invites`, which also permits known series.
Delivery revalidates the event snapshot after asynchronous transport
preparation, before each submission and before creation attendee writes. The store
owns exact-ID detail/capability refresh via `get_calendar_event`, independent
of the rendered date range; backend guards remain authoritative.

Provider-owned pure `validate_event_creation` preflight reuses payload
validation for all four calendar backends. The command invokes it inside
the creation transaction before event insertion or pending meeting claim.
Unsupported Google/Graph series creation therefore fails without reporting
a committed local event as success; post-commit runtime pushes remain
best-effort. Provider creation also protects recurrence evidence: JMAP rejects unknown,
occurrence and unrepresentable series payloads before creation. CalDAV's
deferred pass preserves usable retained ICS verbatim, including occurrence
identity, or generates only representable known-local data. Both deferred
passes process eligible rows and then report aggregated creation failures.
Google's supplemental legacy metadata read changes only the kind of matched
unknown rows; normal sync retains exclusive cursor and reconciliation ownership.

RSVP, calendar/account removal and provider reconciliation are outside the
ordinary mutation guards. Existing best-effort command pushes and JMAP/CalDAV
ordinary-update no-ops continue; these safeguards add no atomic remote
operation or ability to undo already-in-flight delivery.

## Context

The spec treats Google, Microsoft Graph, JMAP, CalDAV, CardDAV and IMAP
as peer protocols (ADR 0010, 0014, 0016, 0022, 0025, 0034, 0035, 0037),
but the command layer did not. Provider-specific logic was interleaved
inside single handlers:

- `commands/calendar.rs` (3,885 lines) and `commands/contacts.rs`
  (1,425 lines) inlined four providers' REST/DAV/JMAP push code in
  every create/update/delete command body.
- `mail/jmap.rs` (3,055 lines) was a single `impl JmapConnection` for
  mail, calendars, contacts, mailboxes and identities.
- Google Calendar v3 and People v1 had no client module at all — raw
  `reqwest` calls lived in the command bodies.
- `get_google_token` was duplicated verbatim in two command files, and
  the O365 IMAP-scoped token refresh block was copy-pasted four times
  in `commands/sync_cmd.rs`.
- `mail/jmap_push.rs` and `ops/worker.rs` reached *into*
  `commands::sync_cmd` for `build_jmap_config` — a layering inversion.
- Dispatch keys diverged: most code switches on the per-service
  `service_bindings.protocol` string (`jmap`, `graph`, `google`,
  `caldav`, `carddav`, `imap`), but contact create/update/delete key on
  the legacy `contact_books.sync_type` column, which stores `o365`
  where the binding says `graph`.

The `meet/` module (`MeetProvider`) already demonstrates the intended
shape: an `#[async_trait]` provider trait, unit-struct implementors, a
static `registry()`, and a `provider_for(account)` lookup keyed on the
protocol string, so nothing outside the module name-matches protocols.
(`ops/` is sometimes cited as the exemplar, but it dispatches by
string-matching too — it demonstrates the *queue* architecture of ADR
0037, not provider abstraction.)

## Decision

1. **Split `mail/jmap.rs` by domain** into
   `mail/jmap/{mod,mail,mailboxes,identities,calendar,contacts}.rs`.
   `mod.rs` keeps the shared transport core (`JmapConfig`,
   `JmapConnection::connect`, the private `api_request`); domain
   methods live in child modules as additional `impl JmapConnection`
   blocks. External `crate::mail::jmap::*` paths are preserved via
   re-exports. No behaviour change.

2. **Transport clients own wire payloads.** A new `mail/google.rs`
   `GoogleClient` (Calendar v3 + People v1) absorbs the inline Google
   REST; `GraphClient`, `CalDavClient`, `CardDavClient` and
   `JmapConnection` gain the payload builders that were duplicated in
   command bodies. Payload builders are pure functions with unit
   tests.

3. **Credential resolution moves to `src/auth.rs`**: one function per
   provider/service (`build_jmap_config`, `get_google_token`,
   `get_imap_credentials`), deduplicating the copies and fixing the
   `mail/` and `ops/` → `commands/` inversion. OAuth *flows* (sign-in,
   consent) stay where they were; `auth.rs` only turns a stored
   account into a ready-to-use credential.

4. **Backend traits with static registries**, mirroring `meet/`:
   `backend::calendar::CalendarBackend`,
   `backend::contacts::ContactBackend` and
   `backend::mail::MailBackend`, each with unit-struct implementors
   per provider and a `for_account(account)` lookup keyed on the
   account's service binding. Unlike `MeetProvider`, backend methods
   take per-call context (`&AccountFull`, and `&DbPool` for sync
   methods) because provider syncs interleave remote I/O with
   incremental local upserts and sync-token persistence.

5. **`sync_type` normalization at the lookup**:
   `backend::contacts::for_sync_type` maps the legacy `o365` value to
   the `graph` backend. There is **no data migration** — Graph contact
   sync still writes `sync_type = 'o365'` rows, and push resolution
   stays keyed on the *book's* `sync_type` so a disabled binding does
   not change push behaviour.

6. **Per-provider error semantics move verbatim into the impls.**
   These are deliberate and load-bearing: event-create pushes are
   best-effort everywhere; JMAP and CalDAV do not push event *updates*
   at all; calendar rename propagates errors while color-set swallows
   them for Google/Graph; Google calendar sync falls back to CalDAV
   when a `caldav_url` is configured; contact sync failures are
   swallowed for Google/CardDAV and propagated for JMAP/Graph; JMAP
   contact creation is deferred to the next sync's unpushed-rows pass.

7. **The ops worker consumes the mail backend** via a
   `MailOpExecutor` session object created by
   `MailBackend::op_executor()`. The executor may hold connection
   state (the persistent IMAP connection with staleness tracking and
   reconnect backoff); JMAP and Graph executors are stateless. The
   worker keeps its queue/coalesce/offline-outbox machinery and stops
   string-matching protocols.

## Non-goals

- iTIP/SMTP invite machinery, DTO validation, database writes and
  frontend event emission stay in `commands/` — they are
  cross-provider orchestration, not per-provider capabilities.
- OAuth sign-in flows and the protocol-specific push/idle starters
  (`start_imap_idle`, `start_jmap_push`) stay per-provider, as in
  `meet/`.
- No `contact_books.sync_type` data migration (see 5).

## Consequences

- Adding a provider = a transport client module, one impl file per
  supported service, and one registry line per trait. Command bodies
  are a provider lookup, one trait call and local persistence.
- The command files shrink to their orchestration core; provider code
  is reviewable per protocol.
- Token failures inside a backend surface as logged errors at the
  command layer instead of being silently skipped in a few paths;
  behaviour is otherwise unchanged. The new log lines fire only on
  user-initiated pushes (event/contact create, update, delete), so an
  account with an expired or misconfigured OAuth grant logs once per
  user action, not per sync tick — the recurring sync paths logged
  their token failures before the refactor too.
