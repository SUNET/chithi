# ADR 0015: Calendar Invite Reply Processing

## Status
Accepted

## Context
When an attendee accepts/declines a calendar invitation, the organizer needs to see the updated status on their calendar event. The standard flow is:

1. Organizer creates event with attendees → sends METHOD:REQUEST emails
2. Attendee responds → sends METHOD:REPLY email back to organizer
3. Organizer's calendar should update the attendee's participationStatus

Stalwart's JMAP server does not automatically process incoming iTIP REPLY emails to update calendar event participants (see Discussion #2700). The server stores participants correctly when set via CalendarEvent/set, but doesn't process incoming iMIP scheduling messages.

## Decision

### Outgoing (organizer creates event)
- Set `participationStatus: "accepted"` for the organizer participant (they implicitly accept their own event)
- Use JSCalendar-bis `calendarAddress` format (not old `sendTo.imip`) for participant URIs
- Send invite emails ourselves via SMTP/JMAP Submission (don't rely on server iMIP)

### Incoming (organizer receives REPLY)
- When the organizer opens a METHOD:REPLY email, auto-process it via `process_invite_reply` command
- Parse the REPLY to extract attendee email and PARTSTAT (accepted/declined/tentative)
- Update the local `attendees_json` on the calendar event
- Patch the participant's `participationStatus` on the JMAP server via `CalendarEvent/set update` with path `participants/<key>/participationStatus`

### Display
- Only the organizer sees "Notify Attendees" dialogs on edit/delete
- Organizer is identified by comparing `organizer_email` with the account's email
- When syncing from server, if the organizer participant has no status or "needs-action", default to "accepted"

## Implementation

### Commands
- `process_invite_reply(account_id, message_id)` — reads METHOD:REPLY from email, updates local + server participant status
- `update_participant_status(event_id, participant_key, status)` — JMAP patch for a single participant

### Auto-processing
- `MessageReader.vue` detects METHOD:REPLY emails and automatically calls `processInviteReply` when the organizer opens them

### JSCalendar-bis format
Per draft-ietf-calext-jscalendarbis-14, participant structure:
```json
{
  "organizer": {
    "@type": "Participant",
    "calendarAddress": "mailto:organizer@example.com",
    "roles": {"owner": true, "attendee": true},
    "participationStatus": "accepted",
    "expectReply": false
  },
  "att0": {
    "@type": "Participant",
    "calendarAddress": "mailto:attendee@example.com",
    "roles": {"attendee": true},
    "participationStatus": "needs-action",
    "expectReply": true
  }
}
```

## Consequences
- Attendee responses are reflected on the organizer's calendar after opening the reply email
- The organizer always shows as "accepted" on their own events
- Stalwart's lack of automatic iMIP processing is worked around by client-side processing
- Both old events (organizer with no status) and new events (organizer with "accepted") display correctly
