# ADR 0009: JMAP accounts send email via JMAP Submission, not SMTP

## Status

Accepted

## Date

2026-04-04

## Context

IMAP accounts use SMTP for sending email (separate protocol, separate host/port configuration). JMAP accounts have no SMTP configuration — JMAP provides its own sending mechanism via the `urn:ietf:params:jmap:submission` capability (RFC 8621).

When a JMAP account tried to send via SMTP, it failed because `smtp_host` was empty and `smtp_port` was 0.

## Decision

The `send_message` command checks `account.mail_protocol` and routes to the appropriate sending method:

- **IMAP accounts**: Send via SMTP using `lettre` (existing behavior).
- **JMAP accounts**: Send via JMAP Submission using three steps:
  1. **Upload blob**: POST the raw RFC5322 message to the JMAP upload endpoint as `message/rfc822`.
  2. **Email/import**: Import the blob into the Sent mailbox with `$seen` keyword set.
  3. **EmailSubmission/set**: Create a submission referencing the imported email and the account's identity ID (fetched via `Identity/get`).

The raw RFC5322 message is built using `lettre`'s message builder (`build_raw_message`) — the same code path as SMTP, just without the transport step. This ensures consistent message formatting regardless of the sending protocol.

The identity ID is fetched dynamically via `Identity/get` rather than assumed to be the account ID, since Stalwart (and other JMAP servers) use separate identity identifiers.

## Consequences

- JMAP accounts can send email without any SMTP configuration.
- The Sent mailbox is found by querying for the mailbox with `role: "sent"`, falling back to Inbox if no Sent folder exists.
- The message building code is shared between SMTP and JMAP paths via `smtp::build_raw_message()`.
- Background body prefetch (`prefetch_bodies`) is skipped for JMAP accounts since bodies are fetched on-demand via the JMAP API.
