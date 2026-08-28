# Troubleshooting

## Start with the status bar

The status bar reports disconnected and reconnecting accounts, sync
errors, and failed operations. Open the operations panel for more detail.
The log file can provide additional context; see
[Data and log locations](data-removal.md#data-and-log-locations).

Do not post logs publicly without checking them for email addresses,
server names, message metadata, and other personal information.

## Browser sign-in does not complete

- Leave Chithi running while the browser flow is open.
- Complete the flow within five minutes.
- Check whether a firewall or browser extension blocks loopback HTTP
  callbacks.
- For Zoom, ensure another process is not using TCP port 47832.
- If credentials were revoked by the provider, open Settings and use
  **Sign in again** where available. Nextcloud Talk and Matrix currently
  require removing and adding the account again.

For Zoom-specific callback, meeting-creation, reauthorization, cleanup,
and disconnection problems, see [Zoom troubleshooting](zoom.md#troubleshooting).

## Gmail mail or calendar works, but not both

Gmail mail and Google Calendar/Contacts use separate credentials. Mail
uses a Google app password over IMAP/SMTP. Calendar and contacts use the
browser OAuth sign-in. Edit the account and check both steps.

## Microsoft says administrator approval is required

Work or school tenants can restrict user consent. Ask the tenant
administrator to approve Chithi's delegated permissions. See
[Microsoft 365 consent](../admin/index.md#microsoft-365-consent).

## The keyring is unavailable

On Linux, ensure a Secret Service provider such as GNOME Keyring or
KWallet is installed, running, and unlocked. On macOS and Windows, check
whether Keychain Access or Credential Manager is available to your user
session. Unlock the credential store, restart Chithi, and retry.

## IMAP auto-discovery found nothing

Discovery is a convenience, not a guarantee. Ask the provider for its
IMAP and SMTP hosts, ports, encryption requirements, and login name,
then enter them manually. CalDAV and CardDAV must be added separately.

## Remote images are missing

This is the privacy-preserving default. Chithi initially removes remote
content from HTML messages. Use **Load images** for the individual
message if you trust the sender. Chithi permits HTTPS images and renders
them without a referrer, but requesting an image can still reveal your
IP address and access time to its host.

## Chithi does not keep running after I close it

Chithi does not currently implement close-to-tray, autostart, or
background operation after the last window closes. Closing the main
window exits after account workers stop cleanly.

## Updates are not offered in the app

There is no automatic updater. Check the
[GitHub releases page](https://github.com/SUNET/chithi/releases) or
update the source tree manually.

## Report a problem

Search or open a report in the
[GitHub issue tracker](https://github.com/SUNET/chithi/issues). Security
vulnerabilities should not be filed publicly; follow the repository's
[security policy](https://github.com/SUNET/chithi/security/policy).
