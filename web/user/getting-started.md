# First launch

When Chithi starts without an account, it opens the onboarding screen.
You can begin with one of four account families:

- Stalwart or another JMAP server
- Microsoft 365
- Google / Gmail
- Another IMAP / SMTP provider

Selecting one opens the account form in **Settings**. Chithi supports
additional account types there, including Fastmail, standalone CalDAV
and CardDAV, Nextcloud Talk, Matrix, Zoom, and La Suite Visio.

You can choose **Skip and add an account later** to inspect Chithi
without an account. Open **Settings** when you are ready to add one.

## Add your first account

1. Select a provider during onboarding. Its account form opens directly.
2. If you skipped onboarding, open **Settings**, choose **Add account**,
   and select the account type.
3. Enter the requested address, credentials, and server information.
4. Complete browser or embedded-window authorization if the account type
   requires it.
5. Save the account.

See [Account setup](accounts.md) for provider-specific requirements. For
video meetings, continue with the guide for [Nextcloud
Talk](nextcloud-talk.md), [Matrix / Element Call](matrix-element-call.md),
[Zoom](zoom.md), or [La Suite Visio](la-suite-visio.md).

## What happens after saving

Chithi starts synchronization for the services enabled on the account.
The status bar reports connection, synchronization, and operation errors.
The Mail view restores the last selected enabled mail account and folder
when possible.

Calendar-only, contacts-only, and video-conferencing accounts do not
become the selected Mail account. Add them separately in Settings and
use calendar and contacts accounts in their corresponding views. Meeting
accounts are used from the Calendar event editor.

## Credential storage

On desktop, passwords and OAuth tokens are stored in the operating
system's credential store. Account configuration and synchronized data
are stored under your local application-data directory. Android OAuth
tokens use the application's private sandbox instead of the desktop
keyring implementation.

Read [Privacy and local data](data-removal.md) before adding sensitive
accounts or deleting local files.
