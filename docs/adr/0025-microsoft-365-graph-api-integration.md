# ADR 0025: Microsoft 365 Integration

## Status
Accepted (revised twice — IMAP→Graph for mail sync, SMTP retained for sending)

## Context
Added Microsoft 365 / Outlook as an account type. Three approaches were evaluated:
1. **Graph API for everything** — single token, simple, but DMARC failures on sent mail for personal accounts (From header mismatch)
2. **IMAP+SMTP with XOAUTH2** — proper From address, IMAP IDLE for push, Maildir storage, but two-resource token management
3. **Hybrid** — Graph API for mail sync + calendar + contacts, SMTP for sending

## Decision
Use **Graph API for mail sync/read, SMTP+XOAUTH2 for sending, Graph for calendar/contacts** (Option 3).

### Why Graph for mail sync (not IMAP)
- Outlook IMAP aggressively throttles connections — `SELECT INBOX` fails with `Command Error. 12`, TCP connections reset with `os error 104` (see "What we tried and failed" below)
- IMAP IDLE reconnection loops make throttling worse, not better
- Graph REST API is stateless — no persistent connections to throttle
- `GET /me/messages/{id}/$value` returns full RFC 5322 MIME — same format as IMAP, stores to Maildir for offline reading
- Single Graph token for mail + calendar + contacts (no two-resource token juggling)

### Why SMTP for sending (not Graph sendMail)
- Graph `POST /me/sendMail` sets `From: kushaldas@gmail.com` but `Sender: outlook_...@outlook.com` for personal accounts, causing **DMARC failures**
- SMTP+XOAUTH2 sends from the actual mailbox address, passing SPF/DKIM/DMARC
- SMTP supports attachments via lettre (Graph sendMail requires base64 attachment encoding)

### Why Graph for calendar/contacts
- Richer API than CalDAV for Microsoft-specific features
- Delta sync via `@odata.deltaLink`
- Same approach as Gmail (Google Calendar API + People API)

### OAuth2 with PKCE
Microsoft requires PKCE (Proof Key for Code Exchange) for public client applications (desktop apps without a client secret).

- **Authorization endpoint**: `https://login.microsoftonline.com/common/oauth2/v2.0/authorize`
- **Token endpoint**: `https://login.microsoftonline.com/common/oauth2/v2.0/token`
- **Tenant**: `common` (supports both personal and work/school accounts)
- **Redirect URI**: `http://localhost:{port}` — Microsoft's v2.0 endpoint matches `http://localhost` regardless of port for native clients
- **No client secret** — PKCE replaces it with `code_verifier` / `code_challenge` (SHA256)

**Important**: The redirect URI must use `http://localhost`, NOT `http://127.0.0.1`. Microsoft only matches `localhost` for the port-agnostic exemption.

### Token management
- Single refresh token obtains access tokens for all Graph scopes
- Microsoft may rotate the refresh token on each use — always persist the new one
- Refresh tokens expire after 90 days of inactivity (effectively never for an active email client)
- Tokens stored in system keyring under `in.kushaldas.chithi.oauth.{account_id}`
- Token migration: OAuth flow stores tokens under a temporary ID (`o365-pending-{timestamp}`), then `add_account` migrates them to the real account UUID

### Two-resource token management
A single refresh token (from the initial OAuth consent) is used to obtain access tokens for two different resources:

| Resource | Scopes | Used For |
|----------|--------|----------|
| `outlook.office.com` | `IMAP.AccessAsUser.All`, `SMTP.Send` | IMAP/SMTP mail access |
| `graph.microsoft.com` | `User.Read`, `Calendars.ReadWrite`, `Contacts.ReadWrite` | Profile, Calendar, Contacts |

**Important scope URL**: Use `https://outlook.office.com/` (NOT `outlook.office365.com`) — this works for both personal and work/school accounts.

Token refresh pattern:
```
Initial auth → IMAP scopes → get refresh_token
Refresh with IMAP scopes → IMAP access_token (for sync, IDLE, SMTP)
Refresh with Graph scopes → Graph access_token (for /me, calendar, contacts)
```

### XOAUTH2 SASL authentication
IMAP and SMTP use the XOAUTH2 mechanism:
```
base64("user={login_email}\x01auth=Bearer {access_token}\x01\x01")
```

The `imap` crate's `Authenticator` trait is implemented for XOAUTH2. `lettre` uses `Mechanism::Xoauth2`.

**Critical**: The `user=` field MUST be the Microsoft login identity (e.g., `kushaldas@gmail.com`), NOT the Outlook mailbox alias. The mailbox alias is used as the display `email` field.

### Account identity for personal Microsoft accounts
Personal Microsoft accounts created with an external email (e.g., `kushaldas@gmail.com`) have three different email addresses:

- **Login identity**: `kushaldas@gmail.com` — returned by Graph `/me` as `mail` and `userPrincipalName`. Used for IMAP XOAUTH2 `user=` field.
- **Internal alias**: `outlook_A634C77E51D17412@outlook.com` — the auto-generated mailbox address. Found in Sent Items From header.
- **User alias**: `chithiapp@outlook.com` — a user-configured alias set in Microsoft account settings. This is the address others send to.

### Email address auto-discovery
The profile fetch tries multiple sources in priority order:

1. **Inbox To address** — checks the most recent inbox message's `toRecipients[0]`. Finds user-configured aliases (e.g., `chithiapp@outlook.com`). This is the most user-facing address.
2. **Sent Items From** — checks the most recent sent message's `from.emailAddress.address`. Finds the internal alias (e.g., `outlook_A634...@outlook.com`). Used as fallback when inbox is empty.
3. **Graph /me** — returns the login identity (e.g., `kushaldas@gmail.com`). Last resort.

The discovered email goes into `account.email` (display/From). The login identity stays in `account.username` (for IMAP XOAUTH2 auth).

### Routing
```
provider == "o365" && mail_protocol == "graph"
├── Sync: Graph API GET /me/mailFolders, GET /me/messages (full MIME download)
├── Send: SMTP with XOAUTH2 via lettre (not Graph sendMail — DMARC)
├── Body read: Local Maildir (downloaded during sync, no live API call)
├── Move/Delete/Flag: Graph API (move_message, delete_message, set_read_status)
├── Draft save: Graph API POST /me/messages
├── Push: Polling via periodic trigger_sync (no IMAP IDLE, no Graph webhooks)
├── Calendar: Graph API (list/create/update/delete events)
└── Contacts: Graph API (list/create/update/delete contacts)
```

### Work/school vs personal accounts
| Aspect | Personal (consumers) | Work/school (organizations) |
|--------|---------------------|----------------------------|
| Login identity | External email (gmail.com) | Organization email (company.com) |
| Mailbox email | `outlook_...@outlook.com` | Same as login email |
| XOAUTH2 user= | Login email | Login email |
| IMAP scopes | `outlook.office.com` | `outlook.office.com` or `outlook.office365.com` |
| DMARC | May mismatch if From uses login email | No issue |

**TODO**: Test with a work/school account to verify scope URLs and token behavior.

### Graph mail sync architecture

The sync function `sync_graph_account` works in two phases to avoid blocking the UI:

**Phase 1 — Download (no DB lock):**
- Fetch folder list via `GET /me/mailFolders`
- For each folder, fetch message list via `GET /me/mailFolders/{id}/messages`
- For each new message, download full MIME via `GET /me/messages/{id}/$value`
- Write raw RFC 5322 bytes to Maildir (`{account_id}/{folder}/cur/{msg_id}:2,{flags}`)

**Phase 2 — Insert (DB lock held briefly):**
- Open a single SQLite transaction
- Batch-insert all new message records with `maildir_path` pointing to the Maildir file
- Commit — lock held for <10ms regardless of message count

When a user clicks a message, `get_message_body` reads from local Maildir (same code path as IMAP/JMAP). No live API call, no network dependency, works offline.

On-demand fallback: if `maildir_path` is empty or has legacy `graph:` prefix (from before this migration), the body is downloaded from Graph, stored to Maildir, and the DB path updated — self-healing on first click.

### What we tried and failed

#### Attempt 1: IMAP+XOAUTH2 for everything (original implementation)

Used Outlook IMAP (`outlook.office365.com:993`) with XOAUTH2 authentication for mail sync, IDLE for push, and all message operations.

**Problems encountered:**
- `SELECT INBOX` fails with `Bad Response: Command Error. 12` — Outlook throttles when too many IMAP sessions open in quick succession
- `Connection reset by peer (os error 104)` — Outlook drops TCP connections during XOAUTH2 auth under load
- IDLE reconnection loops make throttling worse — each failed reconnect attempt counts against the rate limit
- Concurrent connections from sync + IDLE + manual operations trigger throttling
- Partial sync state: `last_seen_uid` advances past messages that were never actually downloaded, requiring manual DB repair to backfill

**Mitigation attempted:** Per-account IMAP connection limiter (max 2 concurrent sessions), IDLE suspension during sync operations. This reduced the frequency of throttling but did not eliminate it — Outlook's rate limits are aggressive and unpredictable.

**Result:** Abandoned. IMAP is fundamentally unreliable for Outlook personal accounts due to session throttling that cannot be fully worked around client-side.

#### Attempt 2: Graph for sync with on-demand body fetch

Switched mail sync to Graph API but kept the body as `maildir_path = "graph:{msg_id}"` — a marker that triggers a live `GET /me/messages/{id}` call (JSON body, not MIME) when the user clicks a message.

**Problems encountered:**
- Clicking an email took ~1 second (Graph API latency for each click)
- No offline reading — every click requires network
- `subject: None`, `date: String::new()` in the body response — the Graph JSON body endpoint doesn't return envelope fields, and the code wasn't reading them from the DB
- Attachments returned empty — needed a separate `GET /me/messages/{id}/attachments` call

**Result:** Abandoned. On-demand fetch is the wrong architecture for a desktop email client.

#### Attempt 3: Graph sync with MIME download, single-phase (DB lock during download)

Downloaded full MIME during sync via `GET /me/messages/{id}/$value`, stored to Maildir. But the MIME download loop ran inside a `BEGIN/COMMIT` DB transaction, holding the SQLite mutex lock for the entire download time.

**Problems encountered:**
- 15 messages × ~800ms per MIME download = ~12 seconds of DB lock
- Any `get_message_body` call during sync blocked waiting for the lock — user clicks an email and waits 7+ seconds
- UI completely unresponsive during sync

**Result:** Fixed by splitting into two phases (current implementation). Phase 1 downloads without the lock, Phase 2 does a fast batch insert.

## Consequences
- O365 mail sync uses Graph API — no IMAP throttling, no connection management
- SMTP+XOAUTH2 retained for sending — avoids DMARC failures on personal accounts
- Full MIME bodies stored locally in Maildir — offline reading, instant click (~10ms)
- No push notifications — relies on periodic polling via `trigger_sync` (same as calendar)
- Single Graph token for all operations (no two-resource IMAP/Graph token juggling)
- Graph API calendar sync implemented (ADR 0034) — list/create/update/delete events
- Graph API contacts sync implemented — list/create/update/delete contacts
- Personal accounts with external login emails need `username` ≠ `email` (login vs mailbox)
- Filter rules (apply to folder) not supported for Graph accounts — IMAP-only feature
- New O365 accounts created with `mail_protocol = "graph"`, IMAP fields cleared
- Existing O365 accounts need DB migration: `UPDATE accounts SET mail_protocol = 'graph' WHERE provider = 'o365'`

### Token management (simplified)
- Single refresh token → Graph-scoped access token for mail/calendar/contacts
- SMTP still needs IMAP-scoped token for XOAUTH2 (`SMTP.Send` is in the IMAP scope set)
- `get_graph_token()` always refreshes with Graph scopes — cannot reuse stored IMAP token
- Calendar auto-sync runs after every mail `sync-complete` event
