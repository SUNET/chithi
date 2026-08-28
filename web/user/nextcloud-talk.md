# Nextcloud Talk integration

Chithi can create and manage Nextcloud Talk conversations from calendar
events. Nextcloud Talk provides the meeting room; it does not add mail,
calendars, or contacts to Chithi. You need a separate calendar account
before you can use the integration.

This page covers adding and authorizing Chithi, managing Talk
conversations, resolving common problems, and disconnecting safely. See
[Account setup](accounts.md) for other account types.

## Prerequisites

Before adding Nextcloud Talk, make sure that:

- You have a Nextcloud account on a server where Talk is installed and
  available to your account.
- You know the complete HTTPS base URL of the Nextcloud installation. Use
  `https://cloud.example.org` for a root installation or, for example,
  `https://example.org/nextcloud` for a path-prefixed installation.
- Chithi has a working calendar account and has loaded a calendar in which
  you can create events.
- Your default browser can open your Nextcloud server.
- On desktop, your operating-system credential store is available and
  unlocked.

## Add and authorize Chithi

1. Open **Settings** in Chithi and choose **+ Add Account**.
2. Select **Nextcloud Talk**.
3. Optionally enter an **Account Name**. If you leave it empty, Chithi uses
   `Talk @ <server host>`.
4. Enter the **Nextcloud URL**. Use the base URL, including any installation
   path, not a Talk room or API URL.
5. Choose **Sign in with Nextcloud**. Chithi opens Nextcloud Login Flow v2
   in your default browser.
6. Sign in to Nextcloud if necessary and grant access. Leave Chithi running
   and finish within five minutes.
7. When authorization completes, Chithi adds the Talk account and returns
   to the main view.

Nextcloud issues Chithi an app password tied to your account. Your real
Nextcloud password stays with Nextcloud and is not returned to Chithi. On
desktop, Chithi stores the app password in the operating-system credential
store under `in.kushaldas.chithi.oauth`. Chithi then communicates directly
with your Nextcloud server. See the [privacy policy](../privacy.md) for more
information.

Login Flow v2 returns a Nextcloud app password rather than a granular,
Talk-only OAuth token. Chithi uses it for Talk room operations, but the
Nextcloud server determines the credential's broader authority. Treat it as
an account credential and revoke it after removing the integration.

## Use Nextcloud Talk in Chithi

### Create a conversation

Talk conversations can currently be added only while creating a new event:

1. Open **Calendar** and create a new event.
2. Select the calendar and enter the event details.
3. Choose **Add _account name_ (Nextcloud Talk)** below **Location**.
4. Wait for **Creating…** to finish. Chithi places the Talk URL in
   **Location** and adds a `Join:` line to **Description**.
5. Choose **Create** to save the event.

Choosing the Talk account immediately creates a group conversation on the
Nextcloud server, before the calendar event is saved. If you cancel the new
event, replace its generated meeting, or change its generated **Location**
before saving, Chithi requests deletion of that conversation. Failed
cleanup remains queued for retry, including after Chithi restarts.

Chithi initially uses the current event title as the conversation name. If
the title is empty, it uses `Meeting`. Saving a titled event or later
renaming it makes a best-effort request to rename the Talk conversation; the
calendar change can succeed even if that request fails. Verify the name in
Talk when it matters.

Talk conversations are persistent rooms. Chithi does not use the event date,
start time, or duration to schedule or expire them. Changing those values
does not change the Talk conversation.

Chithi does not add calendar attendees as Talk participants. Invite or add
participants in Nextcloud Talk as needed. Whether a recipient can open the
saved URL depends on conversation membership and the access policies of
your Nextcloud server; a calendar invitation does not itself grant Talk
access.

### Update a conversation

Edit the saved event's title and choose **Save** to make a best-effort room
rename. Changes made directly in Talk are not imported into Chithi.

Changing or clearing **Location** on a saved event does not delete its
associated Talk conversation. Delete the event when you want Chithi to
request remote cleanup.

Do not change **Nextcloud URL** while editing an existing Talk account.
Saving a different URL does not run Login Flow v2 or obtain a matching app
password. To use another server, finish cleanup, remove the account, and add
it again.

Editing or deleting a displayed occurrence of a recurring event operates on
the series master and its single Talk association. Moving an event to a
calendar owned by a different account does not transfer the internal Talk
association: the destination can retain a copied URL while Chithi deletes
the source and requests cleanup of the conversation.

### Delete a conversation

1. Open the calendar event associated with the Talk conversation.
2. Choose **Delete** and complete any calendar-attendee notification prompt.
3. Chithi removes the local event, records a durable cleanup request, and
   requests deletion of the remote conversation.

If deletion fails, Chithi retains the cleanup request. It retries requested
cleanup when Chithi starts, after calendar synchronization, and before
another attempt to delete the Talk account. Chithi does not continuously
retry on a separate timer. These retries do not guarantee that Nextcloud
deleted the conversation. If complete removal is important, open Nextcloud
Talk and verify that the conversation is gone before removing the Talk
account or revoking its app password.

Do not manually delete a Talk conversation as a way to complete Chithi's
queued cleanup. Current versions can treat an already absent room as a
cleanup failure, leaving local account deletion blocked.

## Troubleshooting

### Browser sign-in times out

- Leave Chithi running while Login Flow v2 is open.
- Complete the browser flow within five minutes. If it expires, close the
  old page and choose **Sign in with Nextcloud** again.
- Confirm that the browser can sign in to the same Nextcloud URL and that a
  proxy, firewall, or browser policy is not blocking it.

### The URL, login, or Talk API does not work

- Enter the HTTPS Nextcloud base URL, preserving any path prefix. Do not add
  `/index.php/login/v2`, `/ocs/`, `/apps/spreed`, or `/call/` yourself.
- Check that the server certificate is trusted and that the URL opens in a
  browser from the same computer.
- Ask the Nextcloud administrator to confirm that Talk is installed,
  enabled, and available to your account, and that Login Flow v2 and the
  Talk API are not blocked by server or reverse-proxy policy.

### The Talk button is missing or conversation creation fails

The **Add _account name_ (Nextcloud Talk)** button appears only in the new
event editor. Confirm that the Talk account appears in **Settings** and that
at least one writable calendar is loaded. For a creation error, also check
network access, Talk availability, your account status, and whether server
policy permits you to create group conversations.

After an interrupted creation request, check Nextcloud Talk for an unwanted
conversation before retrying. The server may have created it before Chithi
received or recorded the response. If Chithi says that tracking and
compensating cleanup both failed, manage that room directly in Talk.

### The credential store is unavailable

On Linux, ensure a Secret Service provider such as GNOME Keyring or KWallet
is running and unlocked. On macOS or Windows, check Keychain Access or
Credential Manager. Unlock or repair the credential store, restart Chithi,
and retry. See the general [troubleshooting guide](troubleshooting.md) for
more help.

### Chithi reports a missing or invalid app password

Nextcloud Talk accounts cannot currently be reauthenticated in place. Do
not revoke the Nextcloud app password while Chithi still needs to clean up
conversations. If there are no related events or pending cleanups, remove
the account and add it again through Login Flow v2.

If the app password was already removed or revoked, manage affected
conversations directly in Nextcloud Talk. Chithi cannot replace the
credential in place and may be unable to clear outstanding ownership
records. An already missing conversation can still be reported as a cleanup
failure. Do not delete the local credential entry as a workaround; report
the exact cleanup error if account deletion remains blocked.

### Talk account deletion is blocked

Chithi blocks account deletion while saved events or pending cleanup still
require that account. Delete the related calendar events, keep the app
password valid, restart Chithi to retry queued cleanup, and verify the
conversations in Nextcloud Talk before trying account deletion again.

If deletion remains blocked after the remote conversations have been
checked, do not remove credential-store entries as a workaround: that can
prevent further cleanup attempts. Consult [Troubleshooting](troubleshooting.md)
and report the exact error if normal cleanup cannot complete.

## Remove Nextcloud Talk from Chithi

Use this order so Chithi retains permission to request conversation cleanup:

1. In Chithi, delete every calendar event whose Talk conversation was
   created with the account you are removing.
2. Leave Chithi running while cleanup is attempted. Restart Chithi if a
   temporary server or network failure interrupted it.
3. Open Nextcloud Talk and verify that each conversation is gone. Chithi's
   deletion request and durable retries are not proof of remote deletion.
4. Open **Settings**, locate the Nextcloud Talk account, and choose its
   trash or **Delete** control. If deletion is blocked, return to the
   troubleshooting steps above rather than revoking access first.
5. In Nextcloud, open your personal security settings and revoke the app
   password or device session created when you authorized Chithi.
6. If complete local credential removal matters, inspect the operating-
   system credential manager for the deleted account's entry under
   `in.kushaldas.chithi.oauth` and remove it. Current Chithi versions do not
   remove this Talk credential during normal account deletion.

Removing the Talk account does not remove the separate account that stores
your calendar. Revoking the app password in Nextcloud is a separate,
server-side step and should be done only after cleanup and local account
deletion have completed. For broader local-data and credential removal, see
[Privacy and local data](data-removal.md).
