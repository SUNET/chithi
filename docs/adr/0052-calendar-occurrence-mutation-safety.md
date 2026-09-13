# ADR 0052: Calendar occurrence mutation safety

## Status

Accepted — approved scope of #288, 2026-09-13.

## Context

A displayed calendar event is not necessarily an independently writable
event. Google and Graph expand recurring series into instances that can
have no local RRULE. JMAP and ICS carry additional recurrence and exception
metadata that the local RRULE representation cannot express. Existing
cached rows lack enough evidence to distinguish these cases.

Some UI paths previously converted a synthetic occurrence ID to its master
ID before editing, deleting or moving it. That silently changed the target
from the selected occurrence to the series. Chithi has no occurrence or
series mutation engine to implement those operations safely.

## Decision

### Ordinary actions require confirmed standalone classification

Persist `recurrence_kind` in `calendar_events` and expose it in the shared
event DTO as `unknown`, `standalone`, `series` or `occurrence`. The schema
and missing-field deserialization default to `unknown`; unrecognized stored
or serialized string values also remain unknown. An absent RRULE or an
opaque event ID is not sufficient evidence of standalone status.

Ordinary Edit/Delete/Move requires `standalone` with no nonempty
`recurrence_rule`. Series, occurrences and unknown events are read-only for
these actions. A contradictory standalone row carrying a rule is blocked
too. Updating a standalone event cannot introduce an RRULE.

Desktop and mobile use the same detail component and the shared
`src/lib/calendar-mutation-support.ts` policy/reasons. The store and drag
handlers apply the same policy, including current metadata checks after
refreshes, dialogs and drag start. Unsupported detail controls are disabled
with an accessible explanation. The shared reasons are:

- Recurring: “Editing, deleting, or moving recurring events is not supported
  in Chithi.”
- Unknown: “Recurrence information is unavailable. Refresh this event before
  editing.”

Locally expanded occurrences retain their synthetic IDs and displayed
times, and are marked `occurrence`. Mutation paths never substitute the
master ID. Moves from recurrence are disabled, including sidebar drops and
cross-account moves. Selection snapshots alone do not authorize edits.

After updates and moves, the store refreshes the exact persisted event via
`get_calendar_event`, independently of the rendered date window. Its
store-owned detail/capability cache lets a confirmed standalone event moved
outside that window continue through save, calendar move and notification.
Pending, failed, mismatched or superseded exact reads cannot authorize the
next action from stale range data. Range refreshes invalidate/revalidate
exact data; late reads neither overwrite newer evidence nor restore an old
selection. Out-of-range detail rows are not appended to the rendered range.

### Classify source evidence before lossy conversion

- **Google:** full expanded reads and incremental reads retain unmasked
  `recurrence`, `recurringEventId` and `originalStartTime` metadata. The
  adapter persists a kind independently of the local RRULE. See
  [ADR 0017](0017-google-api-two-way-sync.md).
- **Graph:** `calendarView` selects `type`, `seriesMasterId` and `recurrence`.
  Consistent `singleInstance`, `seriesMaster`, and `occurrence`/`exception`
  metadata maps to the corresponding kind; incomplete or conflicting
  metadata stays unknown. See
  [ADR 0034](0034-microsoft-365-graph-calendar-sync.md).
- **JMAP:** `CalendarEvent/get` omits `properties` to request the provider's
  complete native object, avoiding invalid field names from mixing calendar
  schema versions. Classification precedes DTO/RRULE conversion and accounts
  for native rules, overrides, exclusions, detached/expanded instance
  identity and linked-series metadata. Legitimately omitted optional
  properties can establish standalone status on an otherwise valid complete
  event; malformed or ambiguous evidence cannot.
- **ICS/CalDAV:** classify parsed components and their resource context.
  A valid `RECURRENCE-ID` identifies an occurrence. Recurrence-set properties
  (`RRULE`, `RDATE`, `EXDATE`, and legacy `EXRULE`) establish series evidence.
  Standalone requires a trustworthy single-event resource; malformed,
  incomplete or ambiguous resources are not promoted merely because the
  selected component has no RRULE. `VTIMEZONE` and `VALARM` are metadata,
  not additional events.

Classification records evidence of mutability, not support for expanding
every recurrence pattern or synchronizing exceptions.

Publication must not discard that evidence and turn a recurring local row
into a provider standalone event on the next refresh. Google and Graph's
current creation payloads cannot represent recurrence, so they reject
recurring or unclassified creation before credential or HTTP access.
Every calendar backend implements the pure `validate_event_creation` hook
using its existing payload builder/validation. The creation command resolves
the account and destination calendar and runs this preflight in the write
transaction before inserting the event or claiming a pending meeting.
Deterministic unsupported creation returns an error with no local event or
meeting ownership change; the form stays open with its pending binding.
Supported local/JMAP/CalDAV series retain their normal creation workflow.
Runtime provider failures after a successful preflight retain the existing
post-commit best-effort semantics. This does not implement recurring creation
for Google or Graph, or add a manual mail fallback for either provider.

### Conservative legacy recovery

Remote-backed legacy rows need a provider read to establish their kind.
When a Google calendar has a saved token and unknown remote-backed rows,
a separate bounded read updates only `recurrence_kind` on matching existing
unknown rows for which it establishes a known kind. Recovery skips cancelled
and inconclusive resources, changes no event content, inserts/prunes no rows
and never changes the delta cursor. Normal incremental sync follows using
the original saved token even after a recovery error; it owns content,
deletions and cursor advancement/expiry. Without a token, the normal initial
full read supplies classification. Local-only unknown rows do not trigger
the supplemental read. Graph, CalDAV and JMAP persist refreshed classification
on existing rows; CalDAV reparses even when the ETag is unchanged.

At startup, local-only unknown rows can be classified from retained ICS
when it yields one conclusively classified event matching the stored UID,
start and `all_day`. A stored nonempty RRULE prevents recovery as standalone
from contradictory ICS. Recovery is transactional and retryable across
restarts, and changes classification rather than event content.

Unknown rows without trustworthy proof stay read-only. A provider read may
not return an event or may return inconclusive metadata; local-only rows
may have no retained ICS. The refresh hint is therefore a recovery attempt,
not a promise that every unknown event can be made editable.

### Publishing local events must preserve recurrence

JMAP guards both immediate provider creation and the deferred local-event
pass before `CalendarEvent/set`. Unknown events, occurrences, contradictory
standalone rows and unrepresentable series are rejected. Series creation
requires a known local event whose complete RRULE can be represented
faithfully. A lossy provider DTO or serialized round trip cannot establish
that completeness or turn an unsupported event into a standalone creation.

CalDAV's deferred pass preserves retained ICS verbatim when parsing confirms
a matching known kind, including `RECURRENCE-ID` and recurrence-set data.
Thus an occurrence with usable retained ICS can be published without losing
its identity. Without retained ICS, generation accepts confirmed standalone
events or known-local series with a representable RRULE, which is included
in the generated resource. Unknown rows and incomplete/unrepresentable
recurrence evidence are rejected rather than flattened into standalone ICS.

Both deferred passes continue processing eligible rows and then report
aggregated creation failures, including unsupported rows. Unsupported rows
remain local; this policy does not enable ordinary recurrence editing.

### Backend enforcement and move ownership

`CalendarEvent::ensure_mutable` defines the backend eligibility check.
`update_event` and `delete_event` load fresh persisted targets before
writes, meeting ownership/cleanup changes or network side effects. The
check runs in the local write transaction; update also preflights before
waiting for a meeting lifecycle lock and rechecks after acquiring it.
These are persisted-state guards, not an additional provider read or remote
ETag compare-and-swap protocol.

The renderer invokes `move_event_to_calendar` with the source event ID and
destination calendar/account IDs. The command:

1. Loads and validates the authoritative source and destination ownership
   before any copy. A same-account move uses the guarded update path.
2. For a cross-account move, builds the copy from persisted source content,
   then rechecks source eligibility, the expected source snapshot and target
   ownership inside the destination-insert transaction.
3. Compares the expected source snapshot again in the source-deletion
   transaction, before deleting or claiming meeting cleanup. Concurrent
   changes to source content or represented metadata leave that source
   intact. If the copy already exists, the error identifies the copy and
   reports that the source was not removed.

The cross-account move remains copy-then-delete with existing best-effort
provider CRUD. It is not an atomic remote move or a new durable retry
protocol. A permitted local mutation can still outlive a failed provider
push under [ADR 0050](0050-provider-backend-traits.md), including the existing
JMAP/CalDAV ordinary-update no-ops.

## Scope

Creation derives `standalone` or `series` from known creation input. New
series creation and invitation delivery for known series remain separate
workflows, subject to existing provider capabilities. `send_invites` is the
creation-invitation command and permits known series. Ordinary edit/delete/
move notification callers use the distinct `notify_calendar_event` command,
which takes only the event ID, requires confirmed standalone status and has
no series exemption. It derives the account, organizer eligibility and full
attendee records from a checked persisted snapshot. A missing or different
organizer cannot authorize notifications. Ordinary notifications never write
attendees, names, response status or self markers. Only explicit creation
invitations retain their existing attendee-write behavior.

Detail and calendar-view callers fetch the exact current event for prompt
eligibility and recheck after dialogs; captured range-list attendees and
account IDs are not notification inputs. Ordinary detail edits omit the
attendee patch because that form does not edit attendees.

Delivery compares the current persisted event with its expected snapshot
after asynchronous credentials/session preparation, before each transport
submission and before creation attendee writes, including the Google/Graph branch
that delegates mail delivery to the provider. Changes abort subsequent work.
These checks do not undo delivery already in flight or make a multi-recipient
send, notification plus mutation, or provider operation atomic.

RSVP, calendar/account removal and provider reconciliation are outside the
ordinary mutation guards. This decision adds no occurrence editor, series
editor or exception-sync engine, and does not redesign provider CRUD delivery.

## Consequences

- Selecting an instance cannot silently mutate its master, even when the
  instance has a plain provider ID and no local RRULE.
- Conservative classification can keep events read-only until sufficient
  source evidence exists; some legacy rows will remain unknown.
- Cross-account races can leave a reported partial copy for the user to
  resolve, preserving a changed source rather than deleting a newer version.
- Recurrence classification and ordinary mutation policy have explicit
  shared ownership, while provider-specific evidence remains in adapters.

## Tests

Automated test sources cover:

- `src/__tests__/calendar-mutation-safety.test.ts`: responsive UI branches
  at mobile/desktop widths, occurrence identity and displayed times, shared
  reasons, forced handlers, stale selection/editor state, refresh races,
  drag/drop guards, exact-ID stale-read rejection and out-of-range standalone
  save/move/notification flows.
- `src/__tests__/calendar-move-tauri.test.ts` and
  `src/__tests__/event-detail-calendar-move.test.ts`: exact-read, move and
  notification IPC arguments, and detail-panel move behavior.
- Rust integration-style tests in
  `src-tauri/src/commands/calendar/occurrence_safety_tests.rs`: real database
  state, rejected writes and meeting cleanup, ownership, standalone paths,
  creation/invitation scope, lock/transaction races, partial-copy reporting
  and restart behavior.
- Rust model, persistence, migration, ICS and provider tests: classification
  round trips, malformed/ambiguous metadata, conservative startup recovery,
  provider request shapes, and refresh of existing rows, including Google's
  kind-only recovery, cursor isolation and continued delta catch-up.
- JMAP backend/transport and CalDAV payload tests: rejected lossy creation,
  continued processing of eligible deferred JMAP rows, representable local
  series creation and verbatim CalDAV recurrence/occurrence ICS preservation.

## References

- [RFC 5545 §3.8.4.4 — RECURRENCE-ID](https://www.rfc-editor.org/rfc/rfc5545.html#section-3.8.4.4):
  identifies a particular instance using its original recurrence date/time,
  including when the instance has been rescheduled.
- [RFC 8984 — JSCalendar](https://www.rfc-editor.org/rfc/rfc8984.html):
  JSON calendar data model, including recurrence rules and overrides;
  distinct from the version-specific JMAP Calendars method schema.
