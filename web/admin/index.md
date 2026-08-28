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

## Architecture references

Implementation decisions and protocol details are recorded in the
[architecture-decision log](https://github.com/SUNET/chithi/tree/main/docs/adr).
ADRs explain design history; the user guide remains the source for
supported user workflows.
