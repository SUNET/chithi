# Administrator guide

Chithi is a desktop client, not a service to deploy. The project does not
operate a backend that receives users' mail, calendar, contacts, or
meeting data. Organizations can distribute their own builds and may need
to approve provider access for their users.

## Build and distribution

See [Install Chithi](../user/install.md#build-from-source) for toolchain
requirements and build commands. The repository currently has no
automatic updater or workflow that publishes desktop installers.
Organizations distributing Chithi should define their own package
signing, update, rollback, and platform-test processes.

## Microsoft 365 consent

Chithi is an OAuth public desktop client using Authorization Code with
PKCE. It stores no Microsoft client secret. The built-in client
application ID is:

```text
b5941cd4-0385-40f1-953a-2c3b36f2a331
```

It requests delegated permissions for the signed-in user only:

- Microsoft Graph: `User.Read`, `Mail.ReadWrite`,
  `Calendars.ReadWrite`, `Contacts.ReadWrite`, and `Place.Read.All`
- Outlook: `IMAP.AccessAsUser.All` and `SMTP.Send`
- Sign-in and refresh: `offline_access`, `openid`, `profile`, and `email`

`Place.Read.All` supports room discovery and availability. Chithi treats
failure to obtain the room-specific token as a soft failure so older
accounts can continue to synchronize mail, calendars, and contacts.

Tenant policy determines whether users can grant these permissions or an
administrator must approve them.

## Zoom Marketplace registration

The default build uses Chithi's user-managed public Zoom OAuth
registration with PKCE. It requests:

- `meeting:write:meeting`
- `meeting:update:meeting`
- `meeting:delete:meeting`
- `user:read:user`

See the end-user [Zoom integration guide](../user/zoom.md) for setup,
usage, troubleshooting, and removal. The [Zoom Marketplace test
plan](../zoom-test-plan.md) maps reviewer actions and API endpoints to
each scope.

Forks can select their own registration at build time with
`CHITHI_ZOOM_CLIENT_ID`. Production Zoom OAuth uses an HTTPS redirect
through a static page because Zoom does not accept the loopback URL for
that registration. Set `CHITHI_ZOOM_REDIRECT_URI` to the fork's static
redirect page. The page must forward the complete query string to:

```text
http://127.0.0.1:47832/
```

Port 47832 is fixed and must be free on the user's machine. The redirect
page is security-sensitive: preserve OAuth `state`, do not add analytics,
and disclose that the page host receives the requested URL containing
the short-lived authorization code.

## La Suite Visio instances

Chithi reuses La Suite Meet's add-on authentication exchange and external
room API. It does not require an OAuth client registration or application
client secret. Meet 1.15.0 is the minimum supported server release: it
introduced the complete exchange as an alpha feature. Meet 1.24.0 or later is
recommended because 1.24.0 is the first release with the stable Outlook
add-on.

The backend and frontend deployment must come from compatible releases and
the frontend must include the Outlook add-on. In releases that package the
add-on only in the DINUM frontend variant, use `lasuite/meet-frontend-dinum`
or build an equivalent image; the generic `lasuite/meet-frontend` image is
not sufficient. The instance must:

- enable the add-on and external API features (`ADDONS_ENABLED` and
  `EXTERNAL_API_ENABLED`);
- configure `ADDONS_TOKEN_SECRET_KEY` and `ADDONS_CSRF_SECRET` plus a cache
  shared by all backend replicas;
- set `ADDONS_TOKEN_SCOPE` to include `rooms:create`;
- set `APPLICATION_BASE_URL` to its public HTTPS origin; and
- serve `/addons/outlook/transit.html` and
  `/addons/outlook/success.html`, including their add-on JavaScript assets,
  from that same origin.

Both add-on paths must return their dedicated Outlook pages. A deployment
that returns the normal Visio "Verify your meeting code" application at
either path is not compatible, even when its backend exposes the add-on API.
This usually means the add-on frontend build or its web-server aliases are
missing or do not match the backend release.

Chithi calls `/api/v1.0/addons/sessions/init/` and `/poll/`, while the
restricted authentication window uses `/api/v1.0/authenticate/` and the
existing success page posts to `/exchange/`. Room creation uses
`/external-api/v1.0/rooms/`.

The current external API does not expose room rename or deletion. Chithi
therefore treats Visio rooms as persistent and only removes their links from
local calendar data. Add-on tokens have no refresh token; users sign in again
after the instance-configured token lifetime expires. This integration is
currently desktop-only because reliable document-start injection into remote
authentication pages is unavailable on Android WebView. On Linux, the
embedded WebKit authentication window may not expose WebAuthn. Deployments
that require a security key or passkey must offer another MFA method for
Chithi users; Chithi does not emulate WebAuthn or transfer the exchange to a
less restricted browser window.

## Architecture references

Implementation decisions and protocol details are recorded in the
[architecture-decision log](https://github.com/SUNET/chithi/tree/main/docs/adr).
ADRs explain design history; the user guide remains the source for
supported user workflows.
