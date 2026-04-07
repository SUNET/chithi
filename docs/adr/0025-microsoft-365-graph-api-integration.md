# ADR 0025: Microsoft 365 Integration

## Status
Accepted (revised — originally Graph-only, now IMAP+SMTP for mail, Graph for calendar/contacts)

## Context
Added Microsoft 365 / Outlook as an account type. Three approaches were evaluated:
1. **Graph API for everything** — single token, simple, but DMARC failures on sent mail for personal accounts (From header mismatch)
2. **IMAP+SMTP with XOAUTH2** — proper From address, IMAP IDLE for push, Maildir storage, but two-resource token management
3. **Hybrid** — IMAP+SMTP for mail, Graph for calendar/contacts

## Decision
Use **IMAP+SMTP with XOAUTH2 for mail, Graph API for calendar/contacts** (Option 3).

### Why IMAP+SMTP for mail (not Graph)
- Graph-only sends with `From: kushaldas@gmail.com` but `Sender: outlook_...@outlook.com`, causing **DMARC failures**
- IMAP+SMTP sends from the actual mailbox address, passing SPF/DKIM/DMARC
- IMAP IDLE provides push notifications (Graph requires webhooks, not feasible for desktop)
- Maildir storage enables offline reading
- Consistent architecture with Gmail (IMAP+SMTP for mail, API for calendar/contacts)

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
provider == "o365" && mail_protocol == "imap"
├── Sync: IMAP with XOAUTH2 (same engine as Gmail/generic IMAP)
├── Send: SMTP with XOAUTH2 via lettre Mechanism::Xoauth2
├── IDLE: IMAP IDLE with XOAUTH2 (push notifications)
├── Move/Delete/Flag: IMAP operations (same as other IMAP accounts)
├── Calendar: Graph API (future — same pattern as Google Calendar)
└── Contacts: Graph API (future — same pattern as Google People API)
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

## Consequences
- O365 accounts use IMAP+SMTP like Gmail — consistent architecture
- IMAP IDLE provides push notifications
- Maildir storage enables offline reading
- SMTP sends from the actual mailbox address (no DMARC issues)
- Two-resource token management adds complexity but is handled transparently
- Graph API client (`graph.rs`) is available for future calendar/contacts integration
- Personal accounts with external login emails need `username` ≠ `email` (login vs mailbox)
