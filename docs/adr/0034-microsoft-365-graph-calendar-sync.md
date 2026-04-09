# ADR 0034: Microsoft 365 Graph Calendar Sync

## Status
Accepted

## Context
O365 accounts use IMAP/SMTP for mail (with XOAUTH2), but calendar sync requires the Microsoft Graph API. The Graph Calendar API is REST-based at `https://graph.microsoft.com/v1.0` and uses OAuth2 Bearer tokens with `Calendars.ReadWrite` scope.

## Decision
Implement full calendar CRUD via Microsoft Graph API for O365 accounts, following the same patterns as existing Google Calendar and JMAP calendar integrations.

### Token management
O365 uses a single refresh token for two resource servers (IMAP and Graph). The stored access token is IMAP-scoped, so `get_graph_token()` always refreshes with Graph-specific scopes (`User.Read Calendars.ReadWrite Contacts.ReadWrite offline_access`) rather than checking expiry on the cached token. The refresh may rotate the refresh token — only the refreshed token is saved back, preserving the stored IMAP access token.

### Timezone handling
Graph API returns event times in the calendar's local timezone by default (e.g., "W. Europe Standard Time"), not UTC. This causes mismatches since Chithi stores all times in UTC internally. The fix: send `Prefer: outlook.timezone="UTC"` header on all `calendarView` requests so times come back in UTC.

The EventDetail edit form was also updated to convert UTC times to local timezone via `new Date()` + `getHours()`/`getDate()` instead of raw string slicing.

### Graph API methods (`src-tauri/src/mail/graph.rs`)

| Method | Endpoint | Purpose |
|--------|----------|---------|
| `list_calendars()` | `GET /me/calendars` | Fetch all calendars |
| `list_events(start, end)` | `GET /me/calendarView` | Fetch events in time range (UTC) |
| `create_event(event)` | `POST /me/events` | Create a new event |
| `update_event(id, updates)` | `PATCH /me/events/{id}` | Update an existing event |
| `delete_event(id)` | `DELETE /me/events/{id}` | Delete an event |
| `rsvp_event(id, response, comment)` | `POST /me/events/{id}/{action}` | Accept/decline/tentative |

### Sync flow (`sync_calendars_graph` in `src-tauri/src/commands/calendar.rs`)
1. Get Graph-scoped token via `get_graph_token()`
2. Fetch calendar list, upsert locally (match by `remote_id`)
3. Fetch events for 6-month window (+/- 90 days) with UTC preference
4. Upsert events locally (update if `remote_id` matches, insert otherwise)
5. Delete local events whose `remote_id` is no longer on the server

### Routing
O365 branch (`provider == "o365"`) added to:
- `sync_calendars` — calls `sync_calendars_graph()`
- `create_event` — builds Graph event JSON, calls `create_event()`
- `update_event` — builds Graph patch JSON, calls `update_event()`
- `delete_event` — calls `delete_event()` with Graph token

### Known limitations
- **RSVP not wired** — `rsvp_event()` method exists but the calendar RSVP command doesn't have an O365 branch yet
- **Recurring events** — individual instances show via `calendarView` but series editing is not supported
- **No delta sync** — re-fetches full 6-month window each time. Graph's `/me/calendarView/delta` could optimize this
- **Multiple calendars** — events assigned to default calendar only. Non-default calendar events won't be correctly categorized

## Consequences
- O365 users can view, create, edit, and delete calendar events synced with Outlook
- Calendar shows events at correct local times (UTC stored, converted for display)
- Token infrastructure handles the two-resource-server complexity transparently
- Future: RSVP wiring, delta sync, and multi-calendar support can be added incrementally
