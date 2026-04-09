# ADR 0035: JMAP Contact Update and Delete via JSContact

## Status
Accepted

## Context
JMAP contacts sync could pull contacts from the server and create new ones via `ContactCard/set create`, but edits and deletions made locally were never pushed back. Users editing a contact in Chithi's Contacts view would see the change locally but the JMAP server would still have the old data — and the next sync would overwrite the local edit.

## Decision
Add `update_contact_card` and `delete_contact_card` methods to the JMAP connection, following RFC 9553 (JSContact) for the card format and JMAP for Contacts for the set operations.

### JSContact (RFC 9553) compliance

The card format used for create and update follows these JSContact properties:

| Property | JSContact type | Our implementation |
|----------|---------------|-------------------|
| `name` | `Name` object | `components` array with `{kind, value}` objects (`given`, `given2`, `surname`), `isOrdered: true` |
| `emails` | `Id[EmailAddress]` map | Keys like `"e0"`, `"e1"`, values `{"address": "..."}` |
| `phones` | `Id[Phone]` map | Keys like `"p0"`, `"p1"`, values `{"number": "..."}` |
| `organizations` | `Id[Organization]` map | Key `"o0"`, value `{"name": "..."}` |
| `titles` | `Id[Title]` map | Key `"t0"`, value `{"name": "..."}` |
| `notes` | `Id[Note]` map | Key `"n0"`, value `{"note": "..."}` |
| `@type` | String | `"Card"` (required, set on create only) |
| `version` | String | `"1.0"` (required, set on create only) |
| `addressBookIds` | `Id[Boolean]` map | Set on create only, maps book ID to `true` |

### JMAP operations

**Update** — `ContactCard/set` with `update` map:
```json
["ContactCard/set", {
  "accountId": "...",
  "update": {
    "<remote_id>": { "name": {...}, "emails": {...}, ... }
  }
}, "u1"]
```

The update map key must be the server-assigned contact ID (the `remote_id` stored locally). Only changed properties are sent — `@type`, `version`, and `addressBookIds` are omitted since they don't change.

**Delete** — `ContactCard/set` with `destroy` array:
```json
["ContactCard/set", {
  "accountId": "...",
  "destroy": ["<remote_id>"]
}, "d1"]
```

### Bug fixed during spec review

The initial `update_contact_card` implementation used `serde_json::json!({ remote_id: updates })` which serialized the literal string `"remote_id"` as the key instead of the variable's value. Fixed by building a `serde_json::Map` explicitly:
```rust
let mut update_map = serde_json::Map::new();
update_map.insert(remote_id.to_string(), updates);
```

This pattern is necessary whenever a JMAP `update` or `notUpdated` map uses a dynamic ID as the key.

### Routing

The `update_contact` and `delete_contact` Tauri commands now check `sync_type == "jmap"` and connect to the JMAP server to push changes. The flow:
1. Local DB is updated first (optimistic)
2. JMAP connection established
3. `ContactCard/set` sent with update/destroy
4. Errors logged but don't roll back the local change (eventual consistency on next sync)

## Consequences
- Contact edits and deletions now propagate to JMAP servers (tested with Stalwart)
- Follows RFC 9553 JSContact format for all card properties
- The `serde_json::json!` literal-key pitfall is documented — same pattern must be avoided in future JMAP update operations
- CardDAV contact push-back remains unimplemented (separate concern, noted in next0.md)
