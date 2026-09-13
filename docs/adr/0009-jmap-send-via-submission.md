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

### Outbound mailbox contract (#271)

Following direct Datatracker review, the user-approved contract below
supersedes issue #271's original exact addr-spec spelling expectation.
Original compose/outbox input and already-built raw MIME remain unchanged;
normalization applies to newly generated header and envelope output.

- **Shared parser:** `crate::mail::mailbox` owns outbound parsing and
  validation for SMTP and JMAP. Each sender or recipient item must parse
  completely as one mailbox. Groups and lists are valid header constructs
  but unsupported per composer item. [RFC 5322 §3.4][header-addresses]
  header grammar is distinct from [RFC 5321 §4.1.2][smtp-mailbox] mailbox
  semantic validity, with UTF-8 extensions from [RFC 6531 §3.3][smtp-utf8]
  and [RFC 6532 §3.2][header-utf8].
- **Header input:** support modern comments/folding whitespace (CFWS) and
  quoted strings ([RFC 5322 §3.2][header-lexical]). Obsolete display-name
  periods ([RFC 5322 §4.1][obsolete-lexical]) are accepted and normalized
  through the header encoder. This is not a general inbound
  obsolete-syntax/recovery parser.
  Parse the original value without Unicode-wide trimming: non-ASCII
  whitespace can be significant local-part data, and malformed edge CR/LF
  must reach validation. The frontend retains its control-character
  rejection and recognizes only ASCII SP/HTAB as recipient padding. PGP
  pre-checks skip only empty/SP/HTAB-only items, validate other items before
  stripping plain ASCII padding, and preserve valid folded identifiers.
- **Local-part semantics:** keep decoded local data separate from wire
  spelling; preserve case and significant whitespace. Newly generated
  addresses use minimum necessary quoting and escaping under
  [RFC 5321 §4.1.2][smtp-mailbox] and
  [RFC 5322 §3.4.1][header-addr-spec]. Equivalent quoted spellings compare
  equally without changing the mailbox's meaning.
- **Domains:** use IDNA validation and case-insensitive canonical IDNA
  comparison, not URL-host interpretation. Support IPv4 and tagged IPv6
  address literals; reject `[not-an-ip]` and untagged IPv6 literals
  ([RFC 5321 §4.1.3][smtp-literals]). The direct `idna` dependency was
  already present transitively; no library versions were updated.
- **Empty local-part:** published [RFC 5321 §4.1.2][smtp-mailbox] permits
  an empty quoted local-part (`""@example.com`). Erratum 5414 is held for
  document update, and draft 5321bis is not normative. The distinct null
  reverse-path (`<>`) remains unsupported by the composer.
- **SMTP sizes:** the 64-octet local-part and 256-octet path limits in
  [RFC 5321 §4.5.3.1][smtp-sizes] are interoperability limits, not universal
  mailbox syntax rejection rules. This change adds no hard SMTP-size
  restriction; generated raw MIME has the physical-line check below.

### Shared message serialization

`smtp::build_raw_message()` is shared by SMTP and JMAP. Lettre still owns
SMTP transport and typed header/MIME encoding, but `build_raw_message` no
longer uses `MessageBuilder`: it reparses normalized local-parts as
serialized syntax, rejects valid forms requiring quotes, and may lose
earlier recipients when joining mailbox lists.

Each complete emitted mailbox header list is serialized once. Bcc is
validated but never emitted. From, Subject, Message-ID, Date, MIME-Version,
applicable threading headers, and MIME framing are retained. Message
building fails closed if any output physical line, including nested MIME
headers, exceeds 998 octets excluding CRLF
([RFC 5322 §2.1.1][header-lines], [RFC 6532 §3.4][utf8-lines]); arbitrary
folding must not alter significant local-part whitespace.

### JMAP envelope and submission

`JmapSubmissionEnvelope` validates the sender and every To, Cc, and Bcc
item before upload and deduplicates recipients by semantic local-part plus
canonical IDNA domain. The first occurrence is emitted with minimum
necessary quoting. The explicit [RFC 8621 §7][submission] envelope avoids
losing Bcc delivery through server-side header inference: Bcc appears only
in `rcptTo`, and the uploaded MIME remains byte-for-byte unchanged.

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

[header-addresses]: https://datatracker.ietf.org/doc/html/rfc5322#section-3.4
[smtp-mailbox]: https://datatracker.ietf.org/doc/html/rfc5321#section-4.1.2
[smtp-utf8]: https://datatracker.ietf.org/doc/html/rfc6531#section-3.3
[header-utf8]: https://datatracker.ietf.org/doc/html/rfc6532#section-3.2
[header-lexical]: https://datatracker.ietf.org/doc/html/rfc5322#section-3.2
[obsolete-lexical]: https://datatracker.ietf.org/doc/html/rfc5322#section-4.1
[header-addr-spec]: https://datatracker.ietf.org/doc/html/rfc5322#section-3.4.1
[smtp-literals]: https://datatracker.ietf.org/doc/html/rfc5321#section-4.1.3
[smtp-sizes]: https://datatracker.ietf.org/doc/html/rfc5321#section-4.5.3.1
[header-lines]: https://datatracker.ietf.org/doc/html/rfc5322#section-2.1.1
[utf8-lines]: https://datatracker.ietf.org/doc/html/rfc6532#section-3.4
[submission]: https://datatracker.ietf.org/doc/html/rfc8621#section-7
