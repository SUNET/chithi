# ADR 0016: Google Calendar API Integration

## Status
Accepted

## Context
Gmail accounts need two-way calendar sync. Google Calendar doesn't support standard CalDAV discovery well, so we use the Google Calendar API v3 (REST) directly. Similarly, Google Contacts uses the People API v1.

## Decision

### Calendar Operations (via Google Calendar API v3)

| Operation | API Call | Notes |
|-----------|----------|-------|
| List calendars | `GET /users/me/calendarList` | Returns all user calendars with colors |
| List events | `GET /calendars/{id}/events` | Uses `singleEvents=true`, `timeMin/timeMax` |
| Create event | `POST /calendars/{id}/events` | Includes `summary`, `start`, `end`, `attendees`, `iCalUID` |
| Update event | `PATCH /calendars/{id}/events/{eventId}` | Partial update of changed fields |
| Delete event | `DELETE /calendars/{id}/events/{eventId}` | Removes from Google Calendar |
| RSVP (respond) | `PATCH /calendars/primary/events/{eventId}` | Updates `attendees[].responseStatus` |
| Find by UID | `GET /calendars/primary/events?iCalUID={uid}` | Matches invite UID to Google event ID |

### sendUpdates Parameter
All mutating operations include `?sendUpdates=all` when attendees are present, `?sendUpdates=none` otherwise. This controls whether Google sends notification emails to attendees.

### RSVP Flow
When a Gmail attendee responds to an invite in Chithi:
1. Send METHOD:REPLY email to organizer via SMTP (existing flow)
2. Find the event on Google Calendar by `iCalUID`
3. PATCH the event with updated `attendees[].responseStatus`
4. Google Calendar reflects the response

### Contacts Operations (via Google People API v1)

| Operation | API Call |
|-----------|----------|
| List contacts | `GET /v1/people/me/connections?personFields=names,emailAddresses,phoneNumbers,organizations` |
| Create contact | `POST /v1/people:createContact` |
| Delete contact | `DELETE /v1/{resourceName}:deleteContact` |

### Routing Logic
Account operations check `provider` before `mail_protocol`:
```
if provider == "gmail" → Google API
else if mail_protocol == "jmap" → JMAP
else if caldav_url not empty → CalDAV
else → local only
```

Gmail is checked first because Gmail accounts have `mail_protocol="imap"` and may also have a CalDAV URL configured.

### Authentication
All Google API calls use OAuth2 bearer tokens stored in the OS keyring. Tokens auto-refresh when expired. Scopes: `calendar` (read-write) + `contacts` (read-write).

## Remaining Work
See `docs/google_calendar_todo.md` for the full TODO list. Key remaining items:
- Incremental sync with syncToken (#7)
- Recurring event handling (#9)
- Calendar ID mapping (currently uses "primary") (#3)
- Contact update via People API (#14)

## Consequences
- Gmail calendar events sync two-way: create, update, delete all reflected on Google
- RSVP responses from Chithi update Google Calendar in real-time
- Contact creation pushes to Google People API
- Attendees receive Google Calendar notifications via `sendUpdates=all`
- The routing order (gmail → jmap → caldav) prevents Gmail from falling into wrong code paths
