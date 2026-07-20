# ADR 0050: Microsoft Teams Online Meetings for O365 Calendar Events

## Status
Accepted

## Context
Chithi already attaches video meetings to calendar events through the `meet/`
provider abstraction (Nextcloud Talk, Matrix, Zoom — see ADR on #148). Those
are **external** services: each lives on its own `meet` service binding with a
bespoke auth flow, mints a join URL eagerly via a pre-flight `meet_create_url`
call, and needs explicit `delete_meeting` / `reschedule_meeting` lifecycle
calls.

Microsoft Teams on an O365 account is fundamentally different. Teams is
intrinsic to the account itself, and Microsoft Graph can attach a Teams meeting
**natively** to a calendar event by setting `isOnlineMeeting: true` on the
event create/update payload. Graph then mints the join URL server-side, injects
the Teams join block into the event body, and ties the meeting's lifecycle to
the event's lifecycle.

Two account flavours must be supported. Chithi's O365 support targets personal
Microsoft (MSA) accounts as the primary tested path and work/school (Azure AD)
accounts as a secondary path (see ADR 0025). The two require different Graph
`onlineMeetingProvider` values: `teamsForBusiness` for work/school,
`teamsForConsumer` for personal.

## Decision

### 1. Native Graph, not the `meet/` abstraction
Add the Teams meeting by injecting `isOnlineMeeting: true` and
`onlineMeetingProvider` directly into the `/me/events` create (`POST`) and
update (`PATCH`) payloads in `commands/calendar.rs`. Graph returns the minted
join URL under `onlineMeeting.joinUrl`, which Chithi stores and displays.

Teams deliberately does **not** go through `meet/`: that module's
`validate_meet_binding` calls `provider_for(account)`, which a native O365
account has no entry for, and Teams has none of the external-provider
properties (separate binding, eager URL, explicit delete/reschedule). Keeping
it native avoids polluting the provider registry with a non-provider.

### 2. Work/school vs personal detection from the token
Choose the provider value from the account's Graph access token via the `tid`
(tenant id) claim:

- `tid == 9188040d-6c67-4c5b-b112-36a304b66dad` (the well-known MSA tenant) →
  `teamsForConsumer`.
- any other `tid` → `teamsForBusiness`.
- token not a decodable JWT → `teamsForConsumer`.

The last rule is deliberate: work/school accounts always receive a standard JWT
Graph access token carrying `tid`, whereas personal MSA Graph access tokens are
opaque. An undecodable token is therefore itself the signal for a consumer
account. Implemented as `teams_provider_for_token()` in `mail/graph.rs`; no
extra API call and no new stored state.

### 3. No new OAuth scope
The event-embedded `isOnlineMeeting` approach works with the already-requested
`Calendars.ReadWrite` scope. `OnlineMeetings.ReadWrite` (needed only for the
standalone `/me/onlineMeetings` endpoint, which we do not use) is **not**
required. Existing accounts need no re-consent.

### 4. Save-time toggle in the UI
Because Graph mints the join URL only on event POST, there is nothing to show
before save. The EventForm therefore exposes an **"Add Teams meeting"
checkbox** (rendered only when the selected calendar belongs to an O365
account) rather than an eager "Add link" button like the other providers. On
save the backend creates the event with the online-meeting fields, captures
`onlineMeeting.joinUrl`, and persists it. The join link surfaces in the event
detail on the next event refresh.

### 5. Add + best-effort remove on edit
The `update_event` command accepts `add_teams_meeting: Option<bool>`:
- `Some(true)` on an event without a meeting → `PATCH isOnlineMeeting: true` +
  provider; the join URL is read back from the PATCH response and stored.
- `Some(false)` on an event with a meeting → `PATCH isOnlineMeeting: false` and
  the local URL is cleared optimistically. This is **best-effort**: Graph does
  not reliably detach an existing meeting, so removal may persist server-side.
- `None` → leave the meeting untouched.

### 6. Storage: a column on `calendar_events`, fed from Graph sync
Add an `online_meeting_url TEXT` column to `calendar_events` (schema + a
guarded `ALTER TABLE` migration). The Graph event `$select` and parser are
extended with `isOnlineMeeting,onlineMeeting`, and the Graph calendar sync
writes `onlineMeeting.joinUrl` into the column on both insert and update — so
Graph stays the source of truth and the link survives edits made in Outlook or
Teams. The event detail renders a "Join Teams meeting" button that opens the
URL through the existing tracked link popup (ADR 0045). The `meet_meetings`
table and the external-provider plumbing are untouched.

## Consequences
- O365 users can add a Teams meeting to an event with a single checkbox, with
  no extra sign-in or consent.
- Personal-account Teams (`teamsForConsumer`) is best-effort: if Graph rejects
  it, the event is still created without the meeting and the failure is logged.
- Removing a Teams meeting from an existing event is best-effort and the UI
  should frame it as such, since Graph may keep the meeting server-side.
- Graph auto-injects a Teams join block into the event body; to avoid a
  duplicate link the join URL is surfaced via the dedicated `online_meeting_url`
  field rather than also stuffed into the Location field.
- Adding the `online_meeting_url` column touched every `CalendarEvent`
  construction and SQL site; the column is appended last to keep positional
  column indices stable.
- The detection heuristic assumes work/school Graph tokens are always JWTs and
  personal ones always opaque. If Microsoft changes token formats, the fallback
  (opaque → consumer) keeps personal accounts working and only mislabels an
  edge case, which the best-effort consumer path tolerates.

## References
- `docs/teams.md` — implementation plan and design-decision log.
- ADR 0025 — Microsoft 365 Graph API integration (token/scope model).
- ADR 0034 — Microsoft 365 Graph calendar sync.
- ADR 0045 — tracked link popup used to open the join URL.
- Graph: `isOnlineMeeting` / `onlineMeetingProvider` on the event resource.
