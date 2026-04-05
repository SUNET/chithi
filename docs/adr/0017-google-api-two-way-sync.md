# ADR 0017: Google Calendar & Contacts Two-Way Sync

## Status
Accepted

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
1. **Create** (`people:createContact`): pushes new contacts with names, emailAddresses, phoneNumbers. Stores `resourceName` as `remote_id`.
2. **Update** (`people:updateContact`): patches contact with `updatePersonFields=names,emailAddresses,phoneNumbers`.
3. **Delete** (`people:deleteContact`): removes contact from Google.
4. **Sync** (`people/me/connections`): fetches all contacts with names, emails, phones, organizations. Upserts by `resourceName`.

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
- Events imported to Google Calendar when attendee responds to invites
- Attendees receive Google Calendar notifications via `sendUpdates=all`
- All operations authenticated via OAuth2 bearer tokens stored in OS keyring
