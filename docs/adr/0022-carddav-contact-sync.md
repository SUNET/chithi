# ADR 0022: CardDAV Contact Sync for IMAP Accounts

## Status
Accepted

## Context
Contact sync was only available for JMAP accounts (via JSContact) and Gmail accounts (via Google People API). Generic IMAP accounts — typically backed by servers like Dovecot, Nextcloud, Radicale, or Stalwart — had no contact sync at all. These servers almost universally support CardDAV (RFC 6352) for address book access.

## Decision
Add a CardDAV client as a separate module (`carddav.rs`) and wire it into the contact sync flow for generic IMAP accounts.

### Architecture

**Separate module, shared helpers:**
- `carddav.rs` is its own module (not merged into `caldav.rs`) since CardDAV and CalDAV are distinct protocols with different discovery paths, XML namespaces, and data formats
- Shared WebDAV primitives (`find_text_in`, `find_elements`, `has_descendant`, `parse_href_from_xml`, `DavAuth`) are exposed as `pub(crate)` from `caldav.rs` and reused by `carddav.rs`

**Discovery flow:**
```
.well-known/carddav (PROPFIND)
  → current-user-principal (PROPFIND)
    → addressbook-home-set (PROPFIND)
      → address books (PROPFIND Depth:1, filter resourcetype=addressbook)
        → contacts (REPORT addressbook-query with address-data + getetag)
```

If the account has a `caldav_url` configured, it is used as the CardDAV base URL too (same server typically hosts both). If empty, auto-discovery tries `https://<domain>/.well-known/carddav` and `https://mail.<domain>/.well-known/carddav`.

**Sync strategy:**
- Each address book maps to a `contact_books` row with `sync_type = "carddav"` and `remote_id = href`
- Contacts are matched by vCard UID
- ETag-based change detection: only update local contact if server etag differs
- Server-deleted contacts (present locally with `remote_id` but absent from server response) are removed locally
- Local contacts without `remote_id` are not deleted (they were created locally and not yet pushed)

### vCard parsing
Implemented inline rather than using a vCard crate:
- Supports vCard 3.0 and 4.0
- RFC 6350 line unfolding (continuation lines starting with space/tab)
- Extracts: FN, N (fallback if no FN), EMAIL with type labels, TEL with type labels, ORG, TITLE, NOTE, UID
- Generates vCard 3.0 for pushing local contacts to server

### Routing in sync_contacts
```
JMAP accounts    → sync_contacts_jmap (JSContact)
Gmail accounts   → sync_contacts_google (People API)
IMAP accounts    → sync_contacts_carddav (new)
```

## Consequences
- Generic IMAP accounts now have contact sync (Nextcloud, Radicale, Dovecot, Stalwart, etc.)
- All three contact sync protocols (JMAP, Google, CardDAV) follow the same pattern: discover books → fetch contacts → upsert locally → remove orphans
- The `caldav_url` field on accounts serves double duty for both CalDAV and CardDAV (same server)
- Servers that don't support CardDAV will log a warning and skip (non-blocking)
- vCard parsing covers the most common fields; photo, address, and custom properties are stored in `vcard_data` for lossless roundtrip but not parsed into structured fields
