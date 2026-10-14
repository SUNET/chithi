# ADR 0052: Upstream-canonical calendar mirror

## Status

Accepted

## Date

2026-10-08

## Context

Calendar reconciliation previously mixed provider resources, local event rows,
RSVP overlays, and action-owned history. Protecting dirty or claimed rows could
preserve events that no longer existed upstream, or leave two competing versions
of a series. Fresh installations and existing databases could display different
calendars after reading the same provider inventory.

## Decision

Remote calendars have two independent paths:

- **Provider read → disposable mirror → display.** `db::calendar_sync` owns
  current wire resources and complete display sets. A complete calendar inventory
  replaces that calendar's projections by provider identity. A complete calendar
  discovery removes absent remote calendars. Local-only calendars are not owned
  by this mirror.
- **User intent → durable journal → provider write → confirmed publication.**
  Calendar action/creation journals retain requests and recovery evidence;
  foreground write receipts record transport outcomes. They never veto provider
  reconciliation and never become a substitute for current remote content.

The mirror does not consult claims, action snapshots, manually managed markers,
pending RSVP status, UID matching, or unpublished rows to decide visible truth.
Absent resources and competing local rows are evicted after a complete read.
Unsupported resources retain their current wire representation and an error,
but not an obsolete display projection. Failed or incomplete inventories cannot
establish absence. Graph series enumeration failures and revision changes are
incomplete reads, not unsupported event content.

Google incremental reads rebuild a complete inventory from the wire mirror.
The mirror and cursor advance in the same SQLite transaction. A missing mirror
requires a full provider read; legacy cursors cannot establish its initial state.
JMAP projects each calendar membership separately and verifies stable inventory
identity before decoding individual resources. Graph reads display sets without
requiring mutation validators, while writes retain strict conditional checks.
CalDAV decodes resources independently rather than freezing a calendar when one
resource cannot be represented.

Display field validation is independent of edit validation: optional titles and
instantaneous timed events remain readable without granting mutation support.
CalDAV display decoding accepts explicit UTC values with redundant TZID,
known Windows-to-IANA timezone mappings, and identical repeated RRULE properties,
without rewriting the original resource. Repeated rules with matching selectors
and differing UNTIL cutoffs are readable only after bounded enumeration proves
there are no original starts between those inclusive cutoffs. The earlier
equivalent cutoff is the display projection; the native rules remain untouched.
Conflicting or unprovable rules, floating times, and
unknown timezones are not guessed. RDATE-only sets use their explicit positions.
Unsupported recurrence projection is reported per event; one event or account
failure cannot discard another's readable dates. Bounded results remain visible
with an explicit incomplete-display warning rather than reverting to old dates.

Remote recurrence display uses the RFC 5545 `rrule` engine, not the editor's
restricted grammar. It supports BYMONTH, ordinal BYDAY and BYSETPOS, retains
explicit UTC UNTIL instants on all-day resources, and interprets date-only
cutoffs inclusively in the event's zone. Expired upstream bounds generate no
future positions; explicit finite overrides remain independently readable.
Historical generation, per-period candidate allocation and result sizes are
bounded; exhaustion is an explicit resource error, never a complete empty
projection. Detailed account/resource errors share one collapsed, height-capped
notice so even many failures cannot consume the calendar grid's entire layout.

Graph display maps all six documented recurrence patterns independently of its
mutation decoder, including relative month/year weekday sets and intervals
outside the editor's subset. A range boundary that is not an occurrence anchors
interval counting at the first matching date, following Graph's documented
semantics. Missing or null subjects remain untitled display events. These
projections never alter native content or relax strict mutation reads, timezone
resolution, complete exception reads, or recurrence-blob verification.

Standalone timed display uses the explicit response boundary zone, not legacy
creation-zone metadata. All-day display restores dates in the original boundary
zone independently of a series' recurrence zone. Unknown boundary zones and
non-midnight all-day bounds remain errors; strict mutation decoding is unchanged.
When a series read cannot be displayed, the mirror retains the fetched master
privately (including legacy recurrence data), rather than only its inventory
summary. Public errors include claimed/computed dates, never native payloads.

Display cancellation proofs support Microsoft's MonthNth weekday-mask/index
encoding for relative monthly/yearly patterns. Blob selectors and interval phase
must match the projected Graph pattern. One bounded pass through the display
engine verifies all deleted/modified original positions and the complete bounded
range's count and final date. Corrupt or unprovable blobs remain resource errors;
the strict editor cancellation decoder and mutation grammar are unchanged.
Observed imported date-bounded blobs may store Graph's inclusive range boundary
rather than the final matching occurrence date. Display accepts that exact date
only when it matches the source Graph endDate and projected event-local cutoff,
and complete enumeration proves the same count and all required original slots.
Count-bounded blobs still require the exact final occurrence date; unrelated
later dates, selector/phase mismatches and unprovable ranges remain errors.
Graph can return an explicitly cancelled exception even when the master's
cancelledOccurrences list is empty. Display verifies that exception's immutable
identity, master link and original position, then records an exclusion while
retaining live exceptions and native data. All cancelled exception originals
are checked in one bounded recurrence pass. The final master recheck remains
mandatory, and strict mutation reads still reject cancelled exception content.

The engine enumerates civil-time candidates; the adapter applies the event's
zone explicitly. Nonexistent DST times are skipped without consuming COUNT;
ambiguous times use the first instant. UTC cutoffs are checked after timezone
resolution. This avoids the library's default forward shift of nonexistent
times and preserves the provider's original resource unchanged.

Ordinary creation reuses a caller-supplied operation UUID after an uncertain
response. Updates, deletions, and imported copies propagate provider failures
before publishing content or requesting meeting cleanup. Foreground receipts
distinguish an accepted remote write from completed local publication; uncertain
writes are recorded and are not automatically replayed. Imported-copy receipts
retain provider addressing before the canonical read. Resumable action and
creation publication rechecks upstream state rather than replaying saved display
content. A missing local cache can be rebuilt from a fresh, identity-checked
provider read; an absent or independently changed provider event is not restored
from history.

Mirror eviction is not a request to delete a video room. Only explicit user
deletion or transfer orchestration may request meeting cleanup. Pending rooms
reserved by unfinished creations cannot be discarded independently.

This supersedes preservation-based calendar reconciliation in ADRs 0017, 0034,
and 0037, and the calendar portion of ADR 0051's optimistic-update requirement.
Mail and contact operation semantics are unchanged. Whole-series editing and
moves retain the plan/execute confirmation boundary; recurring user deletion is
not added by this decision.

Visible-window reads filter standalone resources using the canonical mirror's
dates before decoding master DTOs. They do not deserialize unrelated provider
history. Recurrence expansion streams candidates and stops at the original
COUNT, cutoff, or established result limit, retaining DST and work-budget checks.
Only sets with visible positions need editing snapshots; navigation does not
publish new content or use a stale projection cache to hide expensive reads.

## Consequences

- Existing and fresh databases converge to the same upstream display state after
  a successful complete sync, regardless of local history.
- Historical remote-backed rows are not visible before provider confirmation.
- A partial failure can refresh successfully read resources and still report a
  sync error; failed calendars keep their last confirmed mirror. The UI refreshes
  committed changes and shows the account's error rather than claiming success.
- User writes may require another provider read before the new state is visible.
  A transport acknowledgement alone does not complete publication.
- The mirror can be rebuilt without discarding operation journals. Journals are
  durable recovery evidence, not a second calendar database.

## Verification

Tests cover convergence with conflicting local history and claims, eviction
without meeting deletion, unsupported-resource isolation, incomplete inventory
rollback, incremental cursor rollback, multiple JMAP memberships, external
moves, foreground provider failures, creation retry identity, and recovery after
cache eviction without resurrecting absent upstream content.
Display regressions cover month/ordinal/set-position rules through the mirror,
inclusive date and UTC cutoffs, expired rules with explicit additions, historical
COUNT, long overlaps, half-open windows, DST gaps/folds, fractional DTSTART,
exclusions and moved occurrences, and explicit expansion-budget failures. UI
tests retain visible events and one collapsed notice with many resource errors
on desktop and mobile.
