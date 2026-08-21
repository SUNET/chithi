# ADR 0009: JMAP accounts send email via JMAP Submission, not SMTP

## Status

Accepted

## Date

2026-04-04

## Context

IMAP accounts use SMTP for sending email (separate protocol, separate host/port configuration). JMAP accounts have no SMTP configuration — JMAP provides its own sending mechanism via the `urn:ietf:params:jmap:submission` capability (RFC 8621).

When a JMAP account tried to send via SMTP, it failed because `smtp_host` was empty and `smtp_port` was 0.

## Decision

The `send_message` command resolves the enabled mail binding and routes to the
appropriate sending method:

- **IMAP accounts and legacy unknown non-empty protocols**: send via SMTP and
  perform a best-effort IMAP Sent-folder append after delivery is complete.
- **Microsoft Graph accounts**: send via SMTP with Outlook-scoped XOAUTH2, but
  do not attempt IMAP Sent-folder handling.
- **JMAP accounts**: Send via JMAP Submission after resolving every
  server-side prerequisite before blob upload:
  1. Validate SMTPUTF8 support, when required, against
     `accounts[accountId].accountCapabilities` in the JMAP Session.
  2. Fetch `id` and `email` via `Identity/get`. Select the first exact
     sender match, or the first `*@same-domain` identity only when there
     is no exact match. Local-part comparisons are case-sensitive;
     domains use case-insensitive IDNA comparison.
  3. Resolve the Sent mailbox, querying Inbox lazily only when Sent is
     absent.
  4. **Upload blob**: POST the unchanged raw RFC5322 message to the JMAP
     upload endpoint as `message/rfc822`.
  5. **Email/import** and **EmailSubmission/set**: import the blob into
     Sent with `$seen`, then create a submission referencing the imported
     email and selected identity. The submission carries a mandatory
     explicit RFC 8621 envelope assembled from the authoritative sender
     and To, Cc, and Bcc fields.
- **No enabled mail binding**: fail before outbox persistence or transport.

The raw RFC5322 message is built using `lettre`'s message builder (`build_raw_message`) — the same code path as SMTP, just without the transport step. This ensures consistent message formatting regardless of the sending protocol.

The original implementation omitted the JMAP `envelope` property and relied on the server to derive recipients from the imported message headers. That lost Bcc delivery whenever Bcc was absent from the raw MIME. Since 2026-08-21, `JmapSubmissionEnvelope` validates display-name mailboxes before upload, preserves the first parsed addr-spec, and deduplicates by semantic quoted local-part plus canonical IDNA domain. Bcc appears only in `rcptTo`; the uploaded MIME remains byte-for-byte unchanged.

RFC 8621 envelope address objects always include `parameters`. Ordinary
addresses use `null`. If an emitted envelope addr-spec or transmitted RFC 5322
header contains UTF-8, only `mailFrom.parameters` contains
`{ "SMTPUTF8": null }`; all `rcptTo` parameters remain `null`. Submission
fails before upload when the selected account does not advertise that
extension.

Success requires positive, correctly correlated `Email/import` (`i1`) and
`EmailSubmission/set` (`s1`) creation responses for the expected account ID.
Chithi makes a best-effort `Email/set` cleanup request only when import
succeeds and submission is explicitly rejected. The final compound submission
POST uses a dedicated no-redirect client: every HTTP 3xx response is
indeterminate and is never followed with a replayed POST. A missing, malformed,
contradictory, `serverPartialFail`, or transport-lost successful submission
response is also indeterminate. In those cases the outbox row is quarantined
for manual review without cleanup or automatic replay. HTTP 4xx request
rejection and connection failure before the request reaches the JMAP server
remain definite, retryable failures; HTTP 5xx gateway/server responses are
indeterminate. Bcc values and server-returned response descriptions and bodies
are excluded from JMAP send errors and logs; ordinary compose telemetry may
still include visible To recipients.

The identity ID is fetched dynamically via `Identity/get` rather than assumed to be the account ID, since Stalwart (and other JMAP servers) use separate identity identifiers.

## Consequences

- JMAP accounts can send email without any SMTP configuration.
- The Sent mailbox is found by querying for the mailbox with `role: "sent"`, falling back to Inbox if no Sent folder exists.
- The message building code is shared between SMTP and JMAP paths via `smtp::build_raw_message()`.
- Invalid senders, invalid recipients, empty recipient lists, unsupported
  SMTPUTF8, missing matching identities, and missing mailboxes fail before a
  JMAP blob is uploaded.
- JMAP delivery no longer depends on recipient headers in the MIME; outbox retries persist and replay the same explicit To, Cc, and Bcc envelope data.
- Definite submission rejections remain retryable, but an outcome without
  trustworthy completion evidence requires an explicit manual retry to avoid
  duplicate delivery.
- Background body prefetch (`prefetch_bodies`) is skipped for JMAP accounts since bodies are fetched on-demand via the JMAP API.
