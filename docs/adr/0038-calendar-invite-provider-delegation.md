# ADR 0038: Delegate Calendar Invite Emails to Providers

## Status
Accepted

## Context

When a user creates a calendar event with attendees, invite emails must be sent to those attendees. Chithi originally handled this by:

1. Pushing the event to the provider's calendar API (Google Calendar REST API, Microsoft Graph API, or JMAP)
2. Then separately sending its own invite email via SMTP through `send_invites`

This caused **duplicate invite emails** for Gmail and O365 accounts because:

- **Google Calendar API**: When called with `?sendUpdates=all`, Google's server sends its own properly formatted invite emails to all attendees. Chithi's subsequent `send_invites` SMTP call produced a second copy.
- **Microsoft Graph API**: Graph also sends invite notifications when events with attendees are created/updated. Chithi's SMTP invite was a duplicate.

The duplicates were confirmed in testing: an O365 inbox received two identical invite emails for a single event created from a Gmail account — one from Google's servers and one from Chithi's SMTP.

Additionally, the provider-sent invites are superior because:
- They use proper calendar-native formatting that recipient clients recognize
- They thread correctly with the event in the recipient's calendar app
- They include one-click RSVP buttons that work with the provider's ecosystem
- They handle recurrence, timezone, and attendee display correctly

## Decision

For **Gmail** and **O365** accounts, skip the manual SMTP invite send in `send_invites` and let the provider's server handle invite emails.

```rust
// In send_invites():
if account.provider == "gmail" || account.provider == "o365" {
    log::info!("send_invites: skipping manual send for {} account (server handles invites)");
    // Update attendees in local DB only
    return Ok(());
}
```

For **JMAP** and **generic IMAP/CalDAV** accounts, Chithi continues to send its own invite emails via SMTP or JMAP Submission, since those servers typically don't send invites automatically.

### Provider behavior summary

| Provider | Calendar API | Server sends invites? | Chithi sends invites? |
|----------|-------------|----------------------|----------------------|
| Gmail | Google Calendar REST with `sendUpdates=all` | Yes | No (skip) |
| O365 | Microsoft Graph Calendar | Yes | No (skip) |
| JMAP | JMAP CalendarEvent | No (typically) | Yes (via JMAP Submission) |
| IMAP+CalDAV | CalDAV PUT | No | Yes (via SMTP) |

### RSVP replies

The same principle applies to `respond_to_invite`: for Gmail and O365, the RSVP reply could also be delegated to the provider. However, this requires a different mechanism (PATCH the event's attendee status via the calendar API rather than sending an iCal REPLY email). This is left as future work — currently `respond_to_invite` still sends SMTP replies for all account types.

## Consequences

### Positive
- No more duplicate invite emails for Gmail and O365 accounts
- Recipients get properly formatted, provider-native invite emails
- One-click RSVP works correctly in recipient calendar apps
- Reduces SMTP traffic and authentication complexity (especially for O365 XOAUTH2)

### Negative
- Chithi has less control over the invite email content and formatting for Gmail/O365
- If the Google Calendar API or Graph API push fails, no invite email is sent at all (the local event is created but attendees aren't notified). The `create_event` function logs the error but doesn't surface it to the user as a failed invite.
- Two code paths to maintain: provider-delegated (Gmail/O365) and self-sent (JMAP/IMAP)

### Related fixes

- `send_raw_smtp` was also updated to support XOAUTH2 authentication for O365 accounts (it previously only used basic auth, causing `535 Authentication unsuccessful` errors). A shared `get_smtp_credentials()` helper now handles OAuth token refresh for both `respond_to_invite` and `send_invites` callers.
