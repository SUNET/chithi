# ADR 0049: Fastmail JMAP integration via API token (Bearer auth)

## Status

Accepted

## Date

2026-05-25

## Context

Fastmail exposes a full JMAP server at `https://api.fastmail.com/jmap/session`
and is the reference public deployment for mail, submission, calendars and
contacts. Two things make it incompatible with the generic JMAP path we
originally built for Stalwart (ADR 0008, ADR 0010):

1. **Authentication.** Fastmail requires
   `Authorization: Bearer <token>` using an API token generated from the
   user's account settings. HTTP Basic with the account password is
   rejected with `401 Invalid Authorization header, not bearer`.
2. **Auto-discovery.** Fastmail does not serve `.well-known/jmap` on the
   email domain. The probe sequence in `auto_discover_jmap_url`
   (`https://<domain>`, `https://mail.<domain>`, `https://jmap.<domain>`)
   never resolves to `api.fastmail.com`.
3. **OIDC.** Fastmail's OAuth is Authorization Code + PKCE (RFC 7636) and
   does not publish an OpenID discovery document. The OIDC button on the
   JMAP tab is wired for Stalwart-style device flow (RFC 8628), so it
   cannot be reused as-is.

A second, subtler issue surfaced once the auth path worked: Fastmail's
session document advertises `downloadUrl` on
`https://www.fastmail.com/...` and content on
`https://www.fastmailusercontent.com/...`, while `apiUrl` lives on
`api.fastmail.com`. The original `rewrite_url` from ADR 0008 force-merged
every session URL onto the base host, which routed downloads to the
Fastmail marketing homepage instead of message bodies.

## Decision

### Dedicated "Fastmail" account tab

The account setup form gets a `fastmail` tab alongside `gmail`, `o365`,
`imap`, and `jmap`. The user only sees fields that are specific to
Fastmail:

- **Email address** — for display and the From header.
- **API token** — labelled "API token", not "Password". Help text links
  to Fastmail's **Settings → Privacy & Security → Manage API tokens**.

The JMAP URL (`https://api.fastmail.com`) and the auth method
(`bearer`) are hardcoded and rendered read-only. When the user saves,
`provider = "fastmail"`, `mail_protocol = "jmap"`,
`jmap_auth_method = "bearer"`, and the token is stored in the
`password` column (and the OS keyring, per ADR 0011) — the same slot
every other JMAP account uses for its secret.

### Bearer routing through `JmapConfig::from_mail_account`

`JmapConfig` carries an `Option<String>` `access_token` field. In
`from_mail_account`, when `jmap_auth_method == "bearer"` and the password
is non-empty we **promote** the password into `access_token` and clear
the password field. `apply_auth()` is then a single branch: if
`access_token` is set, use `bearer_auth(token)`; otherwise
`basic_auth(username, password)`. This is the same code path OIDC
already used for access tokens, so the bearer mode adds no new
request-time logic — only the migration into `access_token` at config
construction time.

Two safety details:

- **Whitespace trim on the token.** `reqwest::bearer_auth()` embeds
  the value verbatim into the header. A trailing newline from paste
  turns `Bearer fmu1-xxx\n` into a malformed header and Fastmail
  returns `Invalid Authorization bearer parameters, not valid
  format`. The token format itself never contains whitespace, so
  trimming is always safe.
- **Empty-password fallthrough.** When editing an account, the
  password field is intentionally left blank to mean "keep the current
  token in the keyring". An empty string must not be promoted to
  `access_token`, or every subsequent request would send
  `Authorization: Bearer` (empty value).

### Edit-load detection

`populate_legacy_from_bindings` flips the form back to the Fastmail tab
on edit when either `provider == "fastmail"` or `jmap_url` starts with
`https://api.fastmail.com`. The URL check handles accounts created
before the dedicated tab existed and any future user who pastes the
Fastmail URL into the generic JMAP tab.

### URL rewriting only rewrites *internal* URLs

`rewrite_url` now consults `is_internal_url` before rewriting. A URL is
"internal" only if it has an explicit port, a loopback or RFC 1918 /
RFC 4193 private IP, or the literal host `localhost`. Public DNS
hostnames on the default scheme port are left untouched, which keeps
Fastmail's cross-host session URLs intact:

- `apiUrl` on `api.fastmail.com` — kept.
- `downloadUrl` on `www.fastmail.com` / `www.fastmailusercontent.com`
  — kept (was previously force-rewritten onto `api.fastmail.com`,
  returning the marketing homepage).
- Stalwart's `http://mail.internal:8080/jmap/...` — still rewritten
  to the HTTPS proxy, per ADR 0008.

The Stalwart-behind-nginx case is unchanged; Fastmail just doesn't trip
the heuristic.

### What is deferred

- **OIDC for Fastmail.** The OIDC button on the JMAP tab stays
  Stalwart-only. Adding Fastmail OIDC requires implementing
  Authorization Code + PKCE (no discovery document, manual endpoint
  configuration), which is a separate effort.
- **Reusing the generic JMAP tab.** We considered exposing the
  `bearer` auth method on the JMAP tab and letting Fastmail users
  type the URL manually. Rejected: it makes the common case
  (Fastmail) require multiple correct manual entries, and the
  Fastmail-specific token UX (label, placeholder, link to the
  Fastmail settings page) is meaningfully different from a generic
  password field.

## Consequences

- Adding a Fastmail account is a three-field form: email, API token,
  display name. No URL, no auth-method dropdown, no SMTP config.
- All standard JMAP operations work over Bearer: mail sync, send via
  JMAP Submission (ADR 0009), EventSource push (ADR 0020), calendars
  (JSCalendar-bis), contacts (ADR 0035).
- Public JMAP servers other than Fastmail that expose cross-host
  session URLs now also work correctly — the URL-rewrite gate is the
  fix, not a Fastmail-only special case.
- The `password` / `access_token` split lives in `JmapConfig`, so any
  future Bearer-using JMAP server can reuse the path by setting
  `jmap_auth_method = "bearer"` without touching the request layer.
- OIDC sign-in for Fastmail is still TODO; users must use the API
  token path until Auth Code + PKCE lands.

## See also

- ADR 0008 — JMAP session URL rewriting for reverse-proxy deployments.
- ADR 0009 — JMAP sending via Submission.
- ADR 0010 — Account type selection.
- ADR 0011 — System keyring for passwords.
- ADR 0020 — JMAP EventSource push notifications.
- `src-tauri/src/mail/jmap/mod.rs` — `JmapConfig::from_mail_account`,
  `apply_auth`, `rewrite_url`, `is_internal_url`.
- `src-tauri/src/mail/jmap/mail.rs` — `fetch_email_changes`.
- `src-tauri/src/db/accounts.rs` —
  `populate_legacy_from_bindings`, `list_accounts`
  (Fastmail provider recovery from the JMAP binding URL).
- `src/views/SettingsView.vue` — Fastmail tab and edit-load detection.
