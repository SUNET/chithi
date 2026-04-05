# ADR 0014: Google OAuth2 for Calendar and Contacts Sync

## Status
Accepted

## Context
Gmail accounts use app passwords for IMAP/SMTP email access, but Google's calendar and contacts APIs require OAuth2 authentication. App passwords don't work with Google CalDAV, CardDAV, or REST APIs.

## Decision
Implement a generic OAuth2 authorization code flow with local redirect, initially for Google, designed to be reusable for Microsoft O365 in the future.

### Architecture

**OAuth2 flow:**
1. App starts a temporary HTTP server on `127.0.0.1:<random_port>`
2. Opens the system browser to Google's consent page with the redirect URI
3. User logs in and grants permission (supports SSO/MFA in their browser)
4. Google redirects to `http://127.0.0.1:<port>?code=...`
5. App captures the authorization code, exchanges it for access + refresh tokens
6. Tokens stored in the system keyring (`in.kushaldas.chithi.oauth` service)

**Provider configuration:**
- Client ID and Client Secret are embedded in the binary (standard for desktop apps — Thunderbird, GNOME Online Accounts do the same)
- Provider configs (`OAuthProvider` struct) hold auth URL, token URL, and scopes
- Currently only Google; Microsoft can be added by defining another `OAuthProvider` const

**Google API strategy:**
- **IMAP/SMTP**: App password (unchanged)
- **Calendar**: Google Calendar API v3 (REST) — not CalDAV, because Google's CalDAV doesn't support standard WebDAV principal discovery
- **Contacts**: Google People API v1 (REST) — not CardDAV, for the same reason

**OAuth scopes:**
- `https://www.googleapis.com/auth/calendar` — read-write calendar access
- `https://www.googleapis.com/auth/contacts` — read-write contacts access

**Token management:**
- Access tokens auto-refresh using the refresh token when expired (60s buffer)
- Tokens stored in OS keyring, loaded on each sync
- `get_google_token()` helper handles load + refresh transparently

### UI
- Gmail account settings show both "App Password" field (for IMAP/SMTP) and "Sign in with Google" button (for calendar/contacts)
- After OAuth, shows green "Signed in with Google" status with "Sign in again" option
- Editing existing Gmail accounts checks for stored OAuth tokens on form open

### Prerequisites for developers
- Google Cloud Console project with OAuth2 "Desktop app" credentials
- Google Calendar API and Google People API enabled in the project

## Consequences
- Gmail calendar and contacts sync works alongside IMAP email
- Users click one button to authorize — no manual token management
- OAuth tokens are secure in the OS keyring, auto-refreshed
- The OAuth module is provider-agnostic — adding Microsoft O365 requires only a new `OAuthProvider` const and an Entra app registration
- CalDAV client gained `DavAuth` enum and `connect_with_token()` for bearer auth, usable by any OAuth-based CalDAV server in the future
