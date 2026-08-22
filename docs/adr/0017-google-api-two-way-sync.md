# ADR 0017: Google Calendar & Contacts Two-Way Sync

## Status
Accepted; amended for complete contact reconciliation on 2026-08-22.

## Context
Gmail accounts need full two-way sync for calendar events and contacts with Google's servers. Google's CalDAV/CardDAV endpoints don't support standard WebDAV discovery, so we use Google's REST APIs directly.

## Decision

### Calendar: Google Calendar API v3

**Full CRUD cycle:**
1. **Create** (`events.insert`): pushes new events with summary, start/end, description, location, attendees, iCalUID. Uses `sendUpdates=all` when attendees present.
2. **Update** (`events.patch`): partial update of changed fields on edit. Uses `sendUpdates=all` when attendees present.
3. **Delete** (`events.delete`): removes event from Google Calendar. Uses the calendar's actual `remote_id` (not hardcoded "primary"). Uses `sendUpdates=all`.
4. **RSVP** (`events.patch`): when attendee responds to an invite, patches `attendees[].responseStatus`. First searches by `iCalUID`, imports via `events.import` if not found.
5. **Import** (`events.import`): adds a private copy of an invite to the attendee's calendar when responding, so the event appears on Google Calendar before/after accepting.

**Incremental sync:**
- First sync: fetches all events with `timeMin`/`timeMax`, saves `nextSyncToken` from response in `app_metadata` table.
- Subsequent syncs: uses `syncToken` parameter to get only changed events.
- On 410 Gone: clears stored token, falls back to full sync on next cycle.

**Routing:** `provider == "gmail"` is checked BEFORE `mail_protocol == "jmap"` in all operations (create, update, delete, sync) because Gmail accounts have `mail_protocol="imap"`.

### Contacts: Google People API v1

**Full CRUD cycle:**
1. **Create** (`people:createContact`): replaces the Google-owned names, email
   addresses, phone numbers, organization and title fields. It requests contact
   metadata and persists the returned `resourceName` and CONTACT-source ETag.
2. **Update** (`people:updateContact`): first reads the current contact-only
   Person and compares its CONTACT-source ETag with the locally persisted
   version. A mismatch aborts the write and forces a full reconciliation. The
   update mask contains only locally changed Google-owned fields, so an
   unchanged structured name is not rewritten. Changed repeated fields merge
   retained entries into the fresh Person, preserving email display names,
   secondary organizations, departments and primary metadata. A local-only
   edit sends no PATCH.
   Intentional empty arrays still clear a changed repeated field; malformed
   local repeated-field JSON fails before a request rather than clearing data.
3. **Delete** (`people:deleteContact`): removes the contact; already-absent 404
   and 410 responses are idempotent success.

**Incremental reconciliation:**

- The first sync follows every `people/me/connections` page with
  `requestSyncToken=true`, `sources=READ_SOURCE_TYPE_CONTACT`, and a metadata
  field mask. Every continuation repeats the original parameters. The final
  nonblank `nextSyncToken` is required.
- Later syncs use the persisted `syncToken`. HTTP 410 discards the expired
  cursor in memory and performs a bounded full reseed in the same sync. The new
  token is persisted only if reconciliation commits.
- Connections pages establish changed identities and explicit
  `metadata.deleted` tombstones. Non-deleted identities are fetched through
  correlated `people.getBatchGet` requests of at most 200 names so email and
  phone arrays are not subject to the connections-list limit of 100.
- `metadata.previousResourceNames` and a batch response whose
  `requestedResourceName` differs from `person.resourceName` are validated as
  identity aliases. They migrate the existing local row instead of performing
  delete-plus-insert, preserving local-only fields.
- Omitted or null ProtoJSON repeated/default fields are accepted where they
  represent protocol defaults. Invalid consumed types, malformed identities,
  missing batch correlations, non-NOT_FOUND item failures, repeated page
  tokens, premature/missing sync tokens, and conflicting aliases fail closed.

The alias-aware remote-ID delta is reconciled transactionally. Google owns the
display name, emails, phones, organization, title, remote ID and CONTACT-source
ETag; local UID, addresses, notes and raw vCard remain preserved. Primary
emails and phones are ordered first locally; absent remote type labels remain
absent rather than being invented. Unmentioned rows and rows without a
nonblank remote ID remain untouched. Explicit incremental tombstones delete
immediately. A full-list omission never deletes by itself:
contact-only batch NOT_FOUND first becomes a persisted absence candidate and
must be observed again after a ten-minute propagation allowance before it is
treated as deletion. This state and the replacement sync token are JSON in an
account-scoped `app_metadata` entry and commit atomically with contact changes.
The same entry carries a `pending_recoveries` counter: commands increment it in
the transaction containing each optimistic local mutation, and decrement only
that mutation's successful remote push and returned-metadata persistence. Any
credential, payload, network, response, cancellation or persistence failure
leaves its count outstanding; a later successful mutation cannot clear it. A
nonzero count forces a full reseed, whose successful commit resets the counter.
Account deletion removes the entry.

Legacy duplicate managed remote IDs are detached losslessly before baseline
capture, in the same final transaction as contact and cursor changes. Multiple
Google books for one account fail closed rather than selecting and ignoring one.
Google contact sync failures continue to be logged and returned as success
under ADR 0050, so accounts lacking People consent do not fail the whole
contacts sync. Failed or uncertain CRUD pushes force a full cursor reseed
through the durable recovery marker; durable
CRUD retry remains outside ADR 0050. A first failed or malformed remote sync,
or a failed initial state write, does not leave an empty Google contact book.

### Calendar ID Mapping
Each Google calendar has a unique ID (e.g., `user@gmail.com`, `addressbook#contacts@group.v.calendar.google.com`). This is stored as `remote_id` in the `calendars` table and used in API URLs instead of hardcoded "primary".

### Color Mapping
Google returns `backgroundColor` as hex color directly in the calendarList response. This is stored as-is in the `color` field — no colorId-to-hex conversion needed.

## Deferred Items

1. **iCalUID cross-reference**: matching events across accounts by UID to avoid duplicates. Needs cross-account query + dedup logic.
2. **Recurring event handling**: currently uses `singleEvents=true` which expands recurrences. Creating recurring events on Google, editing single instances, and the `events.instances` endpoint are not yet supported.
3. **Push notifications** (`events.watch`): requires a publicly accessible HTTPS webhook URL, not feasible for desktop apps without a relay server.
4. **Calendar management**: creating/deleting Google calendars from the UI. No calendar management UI exists yet.

## Consequences
- Gmail calendar events sync two-way: create, update, delete, RSVP all reflected on Google
- Incremental sync reduces API calls and improves performance after first sync
- Contact CRUD pushes to Google People API
- Contact sync uses persisted People API sync tokens, explicit tombstones and
  alias-preserving delta reconciliation
- Events imported to Google Calendar when attendee responds to invites
- Attendees receive Google Calendar notifications via `sendUpdates=all`
- All operations authenticated via OAuth2 bearer tokens stored in OS keyring
