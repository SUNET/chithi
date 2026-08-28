# Account setup

Open **Settings**, choose **Add account**, and select the account type.
The available types provide mail, calendar, contacts, or video meetings;
one account does not necessarily provide all four.

Release builds require HTTPS for JMAP, CalDAV, CardDAV, Nextcloud Talk,
and Matrix server URLs. Debug builds additionally permit HTTP loopback
addresses for local development; public cleartext HTTP URLs are rejected.

## Gmail

Gmail setup has two independent credential steps:

1. Enter your Gmail address and a Google **app password**. Chithi uses
   it for IMAP mail at `imap.gmail.com:993` and SMTP submission at
   `smtp.gmail.com:587`.
2. Choose **Sign in with Google** in the account form. Browser OAuth
   grants access to Google Calendar and Contacts.
3. Complete both steps and save the account.

An ordinary Google password is not an app password. Google generally
requires two-step verification before an app password can be created,
and managed organizations can disable app passwords. If only one of the
two credential steps succeeds, mail and Google Calendar/Contacts can
have different working states.

## Microsoft 365

Choose **Microsoft 365** and complete browser sign-in. Chithi uses a
public-client OAuth flow with PKCE and does not store a client secret.
It requests delegated access for the signed-in user's mail, calendars,
contacts, and room lookup.

Work or school tenants can require an administrator to approve the app.
Give the administrator the application and permission details in the
[administrator guide](../admin/index.md#microsoft-365-consent).

## Fastmail

Fastmail uses JMAP and a bearer API token, not your normal password:

1. In Fastmail, open **Settings → Privacy & Security → Manage API
   tokens** and create a suitable token.
2. In Chithi, choose **Fastmail**.
3. Enter the account email address and paste the API token.
4. Save the account.

The endpoint is fixed to `https://api.fastmail.com`. Leaving the token
field blank while editing preserves the stored token.

## Generic IMAP and SMTP

Enter the email address, login name, password, IMAP host and port, and
SMTP host and port supplied by your provider. New accounts default to
IMAP port 993, SMTP port 587, and TLS.

After entering the email address, **Auto-discover IMAP / SMTP** can look
for Thunderbird-style provider configuration. Discovery fills only
empty server fields; it does not overwrite values you entered.

Calendar and contacts discovery is not part of this operation. Add
CalDAV and CardDAV as separate accounts.

## Generic JMAP

Generic JMAP supports password authentication and OIDC device
authorization:

- For **Password**, enter the email address, password, and optionally a
  separate username.
- For **OIDC**, enter the email address, choose **Sign in with OIDC**,
  and follow the browser instructions. Some providers display a code to
  enter in the browser.
- Enter the JMAP URL supplied by the provider, or leave it blank to try
  `/.well-known/jmap` discovery for the email domain.

JMAP accounts can expose mail, calendar, and contacts according to the
server's capabilities.

## Standalone CalDAV and CardDAV

Choose **CalDAV** for a calendar or **CardDAV** for an address book.
Enter an account name, server username, password, and the complete DAV
URL supplied by the provider. They are separate account types: adding
one does not automatically add the other.

## Video-conferencing accounts

Chithi can manage meetings for these providers:

- **Nextcloud Talk:** enter the base URL of the Nextcloud server and
  complete Login Flow v2 in the browser.
- **Matrix / Element Call:** enter the HTTPS Matrix homeserver URL and
  complete browser SSO. Chithi creates a private Matrix room using the
  Element Call widget at `call.element.io`.
- **Zoom:** complete browser OAuth. Chithi's local callback requires TCP
  port 47832 to be free. See the [Zoom integration guide](zoom.md) for
  prerequisites, requested permissions, usage, troubleshooting, and
  removal instructions.

Meeting accounts do not provide mail or calendars themselves. After
adding one, use **Add _account name_ (_provider_)** while creating a
calendar event. The remote room or meeting is created immediately, before
the event is saved. Cancelling the form triggers a cleanup attempt.

Nextcloud Talk and Matrix sessions cannot currently be reauthenticated
in place; remove and add the account again. Zoom has a **Sign in again
with Zoom** action.
