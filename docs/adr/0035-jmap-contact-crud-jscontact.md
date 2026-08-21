# ADR 0035: JMAP Contact CRUD with JSContact

## Status

Accepted; amended for contact migration checkpoint 1 on 2026-08-21.

## Context

JMAP contact sync originally pulled contacts and created new cards, but local
edits and deletions were not pushed. The transport also treated malformed or
partial JMAP responses as empty success, fetched all cards with one unbounded
`ContactCard/get`, and retained only one address-book membership.

Two standards apply and their roles are distinct:

- RFC 9553 defines the JSContact card data model and JSON representation.
- RFC 9610 defines JMAP Contacts, including `AddressBook/*`,
  `ContactCard/*`, the contacts capability, and card membership in address
  books.

This historical gap motivated the original ADR. The current command layer now
attempts update and delete pushes, but those command-triggered pushes remain
best-effort under ADR 0050. This ADR does not claim durable remote delivery or
queued retry for those commands.

## Decision

Use the RFC 9610 capability URI `urn:ietf:params:jmap:contacts` independently
from the mail capability. A JMAP connection keeps the primary mail account for
mail and calendar operations and separately selects a contacts-capable account.
Contact operations fail if the session has no such account; mail connection
setup remains valid.

### Complete reads

Contact sync first validates a complete `AddressBook/get` result, then obtains
the account-wide card ID set with paginated `ContactCard/query` calls and
fetches cards in `ContactCard/get` chunks. Query pages and get chunks are
bounded by the smaller of the session's `maxObjectsInGet` and the client cap of
500. A zero advertised limit prevents contact reads, but does not prevent
`ContactCard/set` writes. Query and get states must remain stable, and the query
state is checked again after all gets. A changed collection restarts the
complete attempt; an incomplete attempt is never reconciled locally.

Every method response must have exactly one correctly correlated tuple, the
contacts account ID, and all required response fields. Method errors expose
only a bounded error type. Missing required fields, malformed consumed fields,
duplicate IDs or UIDs, and requested/result ID mismatches fail the whole
fetch. Malformed unconsumed JSContact fields are outside this projection's
validation scope. AddressBook/get requires empty `notFound`. ContactCard/get
treats a valid `notFound` subset of the requested IDs as a concurrent snapshot
change and retries from the start.

Each ContactCard has a valid JMAP `id`, a mandatory nonblank JSContact `uid`,
and one or more valid `addressBookIds` whose values are `true`. Email and phone
map IDs are retained in local JSON as the optional `jmap_id` property so an
update can preserve stable JSContact map keys.

The local model remains a deliberate down-projection of JSContact. Rich fields
and metadata that Chithi does not model are ignored on read and are not retained
as a raw card. A subsequent full owned-field update can therefore lose
unsupported remote metadata. Full raw JSContact preservation is not part of
this checkpoint.

### Creates and updates

Create and update payloads are built by fallible helpers before network I/O.
Malformed local email or phone JSON, blank required values, and invalid or
non-string `jmap_id` values are errors. Provider-specific members that Chithi
does not consume, such as Graph email `name`, are ignored. If merged local rows
contain duplicate valid `jmap_id` values, the first occurrence keeps its key
and later occurrences receive deterministic fresh keys. All explicit unique
keys are reserved before generated keys are allocated, preventing collisions
with entries later in the local array.

A created card includes:

| Property | Value |
|----------|-------|
| `@type` | `"Card"` |
| `version` | `"1.0"` |
| `uid` | The mandatory local contact UID |
| `addressBookIds` | The actual dynamic address-book ID mapped to `true` |
| `name` | Ordered name components when a local name is present |
| `emails`, `phones` | Stable map IDs and representable labels |
| `organizations`, `titles`, `notes` | Present nonblank local values |

Updates send every locally owned mutable field rather than attempting a
changed-field diff. Removing a name, email collection, phone collection,
organization, title, or note sends `null` for that optional property, using
JMAP PatchObject deletion semantics so stale remote values are explicitly
cleared. `@type`, `version`, `uid`, and `addressBookIds` are not changed by the
ordinary local field-update operation.

### Positive set outcomes

Create, update, and destroy use `ContactCard/set`. A successful HTTP response
is not sufficient: the response tuple, call ID, method, contacts account ID,
new state, optional old state, and per-item outcome must all validate. An old
state may be omitted, null, or a string.

- Create requires exactly the requested creation ID in `created` and a valid,
  nonblank server card ID.
- Update requires exactly the requested card ID in `updated`.
- Delete requires exactly the requested card ID in `destroyed`.
- A sole `notDestroyed` outcome of type `notFound` is accepted as idempotent
  delete success.
- Contradictory, missing, malformed, or extraneous outcomes fail. Server error
  descriptions are not returned because they may contain contact data.

Dynamic JMAP map keys are always constructed with `serde_json::Map`; writing an
identifier in a `serde_json::json!` object would serialize the identifier name
rather than its runtime value.

### Local reconciliation

The backend validates all remote books and cards before mutating local books or
contacts. A card that references a book absent from the validated
`AddressBook/get` result fails sync.

One remote ContactCard is materialized as one local contact row in every
address-book membership. The same remote card ID in different local books is
therefore valid. Existing local book IDs are preserved, and book selection is
scoped by account, `sync_type = 'jmap'`, and remote ID.

Deferred local creates run only after every validated remote book has been
reconciled. Legacy rows without a UID receive and persist a new
`<UUID>@chithi` UID before create. A returned remote ID is attached only to the
same still-unpushed local row in the same book, with collision and one-row
checks. If attachment fails, the backend attempts a strict compensating remote
delete and reports the sync failure.

Checkpoint 1 deliberately retains the handwritten remote-ID-based per-book
reconciliation loop. It does not match by UID and does not provide shared
interrupted-sync recovery. Checkpoint 2 will adopt the UID-aware shared
reconciler and its recovery semantics.

Remote address books absent from a later AddressBook/get result are not deleted
locally in this checkpoint. Duplicate and stale local book rows are likewise
left in place.

Destroying a ContactCard is global: it deletes the remote card and therefore
all of its address-book memberships. This decision does not introduce an
operation for removing only one membership.

## Consequences

- Command-triggered local JMAP edits and deletes make best-effort remote pushes;
  strict response validation does not make their delivery durable.
- Large contact collections are fetched completely without exceeding the
  advertised get limit or the client page cap of 500.
- Multi-book cards remain visible in every membership and retain stable map
  IDs where available.
- JMAP contact sync and deferred-create failures propagate according to ADR
  0050; no partial remote snapshot is treated as authoritative.
- UID-aware shared reconciliation, raw rich-card preservation, stale remote
  address-book deletion, CardDAV push-back, and single-membership removal remain
  separate concerns.
