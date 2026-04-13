# ADR 0040: Calendar Invite Round-Trip Overhaul

## Status
Accepted

## Context

The calendar invite flow had multiple issues that prevented proper RSVP round-tripping between organizers and attendees across Gmail, O365, and JMAP accounts:

### UID mismatch between providers
When Chithi created an event on O365 via Graph API or on Google via Calendar API, it stored a Chithi-generated UID (`xxx@chithi`). However, the provider assigned its own UID (`iCalUId` on Graph, `iCalUID` on Google). When an attendee received the invite email from the provider, the iCal UID in the email was the **provider's UID**, not Chithi's. When `process_invite_reply` tried to match the reply email's UID to the local event, it failed with "no local event for UID".

### No UI refresh after accept/decline
Both `respond_to_invite` and `process_invite_reply` lacked `app: AppHandle` parameters and could not emit `calendar-changed` events. After a user accepted an invite or an organizer received a reply, the calendar view stayed stale until the next 5-minute sync cycle.

### No O365 Graph API RSVP
When an O365 account user accepted an invite, Chithi only sent an SMTP reply email. It did not call the Graph API RSVP endpoint (`POST /me/events/{id}/accept`), so the O365 calendar web interface didn't reflect the acceptance.

### Duplicate events from respond_to_invite
`respond_to_invite` created a local event without `remote_id`, then imported to Google Calendar. When Google Calendar sync ran later, it created a second local event from the same Google event (with `remote_id`). The first event had no `remote_id` so upsert-by-remote-id didn't detect the duplicate.

### Orphan "Calendar" entries
`respond_to_invite` created a new default "Calendar" when none existed for an account, even if Google Calendar sync would create the real one shortly after. This produced duplicate calendar entries in the sidebar.

### iCal CRLF + line folding parse failure
Microsoft Exchange sends iCalendar data with `\r\n` line endings and RFC 5545 line folding (long lines split with `CRLF + space`). The `icalendar` crate parser didn't handle folded lines, causing invite emails from O365 to fail parsing entirely — no Accept/Decline buttons were shown.

### No METHOD:CANCEL handling
There was no processing for `METHOD:CANCEL` emails. When an organizer cancelled an event and sent a cancellation email, the attendee's local event remained in the calendar.

### Calendar SMTP missing XOAUTH2
`send_raw_smtp` in the calendar module used basic SMTP auth (`Credentials::new`) without XOAUTH2 support. O365 accounts that require modern auth got "535 Authentication unsuccessful" errors when sending invite replies or invites.

### Duplicate invite emails
When creating an event with attendees on Gmail, Chithi called `sendUpdates=all` on the Google Calendar API (which sends invites) AND separately called `send_invites` to send SMTP invites. Attendees received two copies.

### No overlap rendering in week view
Multiple events at the same time slot (e.g., same event from two accounts) rendered on top of each other instead of side-by-side.

### Thunderbird reference

Thunderbird's iTIP pipeline (`calItipUtils.sys.mjs`) was the reference model:
- `CalMimeConverter` detects `text/calendar` MIME parts automatically
- `ItipItemFinder` matches events by UID across all calendars
- Separate handlers for REQUEST (accept/decline), REPLY (update attendee status), CANCEL (delete event)
- `modifyItem()` updates both local DB and remote calendar provider
- Calendar fires `onModifyItem` observer which triggers UI refresh
- SEQUENCE + DTSTAMP revision checking to reject outdated replies

## Decision

### 1. Capture provider UIDs on event creation

When pushing events to Google Calendar API or Microsoft Graph API, read back the `iCalUID` / `iCalUId` from the response and update the local event's `uid` column. This ensures incoming RSVP reply emails (which reference the provider's UID) can be matched to the local event.

- Google: `data["iCalUID"]` from `events.insert` response
- Graph: `resp["iCalUId"]` from `POST /me/events` response (`create_event` now returns `(String, Option<String>)`)

### 2. Emit calendar-changed from invite commands

Added `app: tauri::AppHandle` parameter to `respond_to_invite` and `process_invite_reply`. Both emit `calendar-changed` after successful DB modification, triggering automatic UI refresh via the existing listener in `calendar.ts`.

### 3. O365 Graph API RSVP

Added `GraphClient::find_event_by_ical_uid()` to search O365 calendar by `iCalUId`. When an O365 attendee accepts an invite, `respond_to_invite` now calls `client.rsvp_event()` via the Graph API in addition to sending the SMTP reply. The Graph event ID is stored as `remote_id` on the local event.

### 4. Store remote_id from respond_to_invite

After importing an event to Google Calendar via the `/import` endpoint, the returned Google event ID is now stored as `remote_id` on the local event. This prevents Google Calendar sync from creating a duplicate.

### 5. Smart calendar selection

`respond_to_invite` now picks the best calendar with priority: default with remote_id (server-synced) > default > any with remote_id > any existing > create new. This prevents creating orphan "Calendar" entries.

### 6. iCal CRLF + line folding normalization

`parse_ical_data` now unfolds RFC 5545 continuation lines (`\r\n ` and `\r\n\t`) before normalizing `\r\n` to `\n`. This fixes parsing of Microsoft Exchange iCalendar data.

### 7. METHOD:CANCEL handling

New `process_cancelled_invite` Tauri command that parses `METHOD:CANCEL` from email, finds the local event by UID, deletes it, and emits `calendar-changed`. `MessageReader.vue` auto-processes CANCEL emails alongside REPLY.

### 8. Calendar SMTP XOAUTH2

`send_raw_smtp` now accepts `use_xoauth2: bool` and uses `Mechanism::Xoauth2` when true. New `get_smtp_credentials()` helper handles OAuth token refresh. Both `respond_to_invite` and `send_invites` callers use it.

### 9. Provider-delegated invite emails

Gmail and O365 accounts skip manual SMTP invite sending in `send_invites` — the provider's server handles it via `sendUpdates=all` (Google) or Graph API notifications (O365). Only JMAP and generic IMAP accounts send their own invites.

### 10. Google Calendar deletion reconciliation

Incremental sync now checks `"status": "cancelled"` in Google Calendar API responses and deletes those events locally. Full sync (triggered by manual Sync button with `force_full_sync=true`) reconciles all local events against server, removing orphans. Orphan events without `remote_id` are also cleaned up by matching UID.

### 11. Week view overlap rendering

`WeekView.vue` computes overlap columns for events sharing the same time slot. Overlapping events render side-by-side with proportional widths instead of stacking.

### 12. Calendar info in event detail

Event detail dialog shows the calendar name (with color dot) and account email, so users can distinguish which account an event belongs to.

## Consequences

### Positive
- RSVP replies correctly match organizer's events via provider UID
- Calendar UI refreshes immediately after accept/decline (no 5-minute wait)
- O365 calendar web reflects acceptance via Graph API RSVP
- No duplicate events from respond_to_invite + Google sync
- Microsoft Exchange invite emails parse correctly (CRLF + folding)
- Cancelled events are properly removed
- Overlapping events visible side-by-side

### Negative
- `auto_process_calendar_emails` during sync requires body prefetch (not yet implemented — envelope-only sync doesn't fetch bodies). Currently invite processing happens on email open. The function batches all DB writes into a single writer acquisition to avoid blocking the async runtime.
- `respond_to_invite` timezone handling for the local event copy still uses the raw iCal DTSTART without full timezone normalization. The Google-synced copy has correct timezone; the local copy may show wrong time until sync replaces it.
- Auto-processing CANCEL emails on open does not verify that the sender matches the event's organizer. A spoofed CANCEL could briefly remove an event locally until the next calendar sync restores it from the server.

### Files changed
- `src-tauri/src/commands/calendar.rs` — bulk of changes (respond_to_invite, process_invite_reply, send_invites, create_event, sync_calendars_google, process_cancelled_invite, auto_process_calendar_emails, get_smtp_credentials)
- `src-tauri/src/calendar/ical.rs` — CRLF + line folding normalization
- `src-tauri/src/mail/graph.rs` — find_event_by_ical_uid, create_event returns iCalUId
- `src-tauri/src/commands/compose.rs` — background send with events
- `src/components/calendar/WeekView.vue` — overlap layout
- `src/components/calendar/EventDetail.vue` — calendar info row
- `src/components/calendar/CalendarSidebar.vue` — account email under calendar name
