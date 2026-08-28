# Matrix / Element Call integration

Chithi can create Matrix rooms with an Element Call widget from calendar
events. Matrix provides the meeting room; it does not add mail, calendars,
or contacts to Chithi. You need a separate calendar account before you can
use the integration.

This page covers the complete lifecycle of the integration: adding and
authorizing Chithi, creating and managing rooms, resolving common problems,
and disconnecting it safely. For a shorter account overview, see
[Account setup](accounts.md#video-conferencing-accounts).

## Prerequisites

Before adding Matrix, make sure that:

- Chithi is installed and running.
- You have a Matrix account whose homeserver supports browser SSO and lets
  your account create rooms.
- Chithi has at least one working calendar account and has loaded a calendar
  in which you can create events.
- You know the HTTPS base URL of your Matrix homeserver. This is not an
  Element Web URL or an Element Call URL. A homeserver served below a path,
  such as `https://example.org/matrix`, is accepted; include that path.
- Your default browser can return to an HTTP loopback address on your
  computer. Chithi chooses a random local TCP port for each sign-in.
- On desktop, your operating-system credential store is available and
  unlocked.
- For working calls, your Matrix client must support Element Call widgets
  and the deployment must provide compatible RTC focus/SFU infrastructure.
  Chithi verifies room creation, not the complete call infrastructure.

These instructions describe the verified desktop flow. Availability and
browser callback behavior can differ on mobile builds.

## Add and authorize Chithi

1. Open **Settings** in Chithi.
2. Choose **+ Add Account** and select **Matrix**.
3. Optionally enter an **Account Name**, such as `Work`. If you leave it
   empty, Chithi uses `Matrix (<your Matrix ID>)`.
4. Enter the **Homeserver URL**. Use the Matrix Client-Server API base URL,
   for example `https://matrix.example.org` or
   `https://example.org/matrix`. Do not enter an Element Web address or
   `https://call.element.io`.
5. Choose **Sign in with Matrix**. There is no separate save step.
6. Leave Chithi running. In the browser, complete the homeserver's SSO flow
   within five minutes.
7. The homeserver returns the browser to
   `http://localhost:<random-port>/`. **Matrix sign-in successful** confirms
   that Chithi received the callback; token exchange or credential storage
   can still report an error in Chithi.
8. Return to Chithi and wait for the account form to close. After full
   success, open **Settings** again to see the Matrix account.

Chithi exchanges the one-time SSO login token directly with the homeserver.
It stores only the resulting Matrix access token in the desktop operating-
system credential store under `in.kushaldas.chithi.oauth`; the stored record
has no refresh token or expiry, and Chithi does not refresh it. The Matrix
server records a session or device for Chithi, normally labelled with
`Chithi/<version>`.

Matrix SSO does not use Zoom-style granular scopes. The access token is a
Matrix client credential that Chithi uses to create rooms, write room names
and widget state, and leave rooms. Treat it as an account credential and
revoke the Chithi session after removing the integration.

Chithi communicates directly with the configured homeserver; no
Chithi-operated application backend receives the Matrix token or room data.
The event title becomes room state. The `matrix.to` URL contains the room ID
and routing homeserver and can be synchronized through the calendar provider
or sent to attendees. Opening it contacts `matrix.to`; loading the widget
contacts `call.element.io` and the homeserver's RTC infrastructure. Chithi
does not send the Matrix access token to those public frontends. See the
[privacy policy](../privacy.md) for more information.

## Use Matrix in Chithi

### Create a room

Matrix rooms can currently be added only while creating a new event:

1. Open **Calendar** and create a new event.
2. Select the calendar and enter the event details.
3. Choose **Add _account name_ (Matrix)** below **Location**.
4. Wait for **Creating…** to finish. Chithi puts a `matrix.to` room link
   in **Location** and adds a `Join:` line to **Description**.
5. Choose **Create** to save the calendar event.

The private Matrix room is created as soon as you add Matrix, before the
calendar event is saved. Chithi also attempts to add an Element Call widget
whose frontend is fixed at `https://call.element.io`; a self-hosted Element
Call URL cannot currently be selected. The saved `matrix.to` URL opens a
Matrix redirect or chooser, which can hand the room to an installed or web
Matrix client rather than sending participants directly to the widget.

Chithi does not invite calendar attendees or any other Matrix users to the
room. A `matrix.to` link is not a Matrix membership invitation. Use a Matrix
client to invite each intended Matrix user; calendar invitations and Matrix
room invitations are separate.

The room is created with private visibility, but **private does not promise
end-to-end encryption**. Chithi does not enable or verify room encryption.
Check the room's encryption state in your Matrix client before using it for
sensitive conversations.

The current event title becomes the initial room name. If it is empty when
you add Matrix, Chithi uses `Meeting`; saving a titled event then attempts to
rename the room. Matrix rooms are persistent, so the event's start time and
duration do not schedule or restrict the room and are ignored by Matrix.

If you close the new-event editor, replace its generated meeting, or change
the generated **Location** before saving, Chithi requests cleanup of the room
it just created. Cleanup means leaving the room, not deleting it. A failed
leave remains queued for retry, including after Chithi restarts.

### Update a room

Changing a saved event's title makes Chithi attempt to rename the associated
Matrix room. The calendar save and room rename are separate operations, so
the event can be saved even if the rename cannot be applied.

Changing the event's date, start time, end time, or duration does not change
the Matrix room. Editing or clearing **Location** on a saved event does not
remove Chithi's internal room association. Chithi does not import room-name
or membership changes made in another Matrix client.

**Do not change or clear Homeserver URL on an existing Matrix account.**
Saving that field does not reauthenticate. Changing it makes Chithi use the
existing token with the new URL, while clearing it can remove the binding
needed for cleanup. Finish cleanup and add a new account when changing
homeservers.

Editing or deleting a displayed occurrence of a recurring event operates on
the series master. Moving an event to a calendar owned by a different account
does not transfer its internal Matrix association: the copied destination
can retain the old URL while Chithi deletes the source and leaves the room.

### Delete an event and leave its room

1. Open the calendar event associated with the Matrix room.
2. Choose **Delete**.
3. If Chithi asks about attendee notification, choose the appropriate
   calendar action.

Chithi deletes the local event, queues a Matrix leave request, and then
attempts it. If the homeserver is temporarily unavailable, the request is
retained and retried later, including after Chithi restarts.

Deleting the event never globally deletes the Matrix room. Matrix rooms can
outlive their creators, and Chithi only makes its signed-in Matrix user leave.
It does not remove the room for other members, remove those members, or erase
their room history. Copies of the calendar event or join URL held by other
people can also remain.

## Troubleshooting

### Browser sign-in does not return to Chithi

- Keep Chithi open and complete SSO within five minutes.
- Allow the browser to connect to `http://localhost` on the random port
  chosen for this sign-in. There is no fixed Matrix callback port.
- Check whether a firewall, browser extension, proxy, or endpoint-security
  tool blocks loopback HTTP callbacks.
- Start a new sign-in if the flow timed out, was cancelled, or reports a
  state mismatch. Do not reuse an old browser page.
- Confirm that **Homeserver URL** is the HTTPS Matrix homeserver API base,
  including any required path prefix, rather than an Element Web or Call URL.

### The Matrix button is missing from the event editor

Confirm that the Matrix account appears in **Settings** and that Chithi has
loaded at least one writable calendar. **Add _account name_ (Matrix)** appears
only in the new-event editor, not while editing an existing event.

### Room creation fails

The event editor displays **Could not create meeting:** followed by the
Matrix, credential-store, URL, or network error. Check the homeserver URL,
network connection, credential store, and Matrix service before retrying.
After an interrupted request, check your Matrix client for an unwanted room:
the homeserver may have created it before Chithi received or tracked the
response.

### The Element Call widget is missing

Room creation can succeed even when the widget state cannot be written. Open
the `matrix.to` link in a full Matrix client and check whether the homeserver,
room permissions, or client policy blocks widgets or access to
`call.element.io`. The room and link can remain usable as a Matrix room, but
Chithi does not currently offer a separate action to reinstall the widget.

If the event has not been saved, close the editor to queue a leave before
trying again. If it has been saved, delete the event and let cleanup complete
before creating a replacement.

### Another participant cannot enter the private room

Invite the participant's Matrix ID from a Matrix client. Chithi does not turn
calendar attendees into Matrix members, and possession of the `matrix.to`
link alone does not grant membership in a private room.

### Chithi reports a keyring or access-token error

Unlock or repair the desktop credential store and restart Chithi. On Linux,
ensure a Secret Service provider such as GNOME Keyring or KWallet is running;
on macOS or Windows, check Keychain Access or Credential Manager.

Matrix credentials cannot currently be refreshed or reauthenticated in
place. After related room cleanup has completed, remove the account and add
it again to create a new Matrix session. Do not revoke the existing Matrix
session while Chithi still needs it to leave rooms.

### Matrix account deletion is blocked

Normal deletion is blocked while saved events or queued cleanup still refer
to the account. Delete the related calendar events, keep the Matrix session
and credential store available, restart Chithi to retry durable cleanup, and
then try deleting the account again.

Unlike Zoom, Matrix has no **Delete locally** fallback in Chithi. If the
credential exists but the keyring is unavailable, repair the keyring and
retry. If the token itself was revoked or lost while cleanup is pending,
there is no supported in-app recovery. A second Matrix account cannot take
ownership of the old account's queued rooms, and normal deletion can remain
blocked.

See [general troubleshooting](troubleshooting.md) for status, log, and
keyring guidance. Logs can contain Matrix IDs, homeserver names, room IDs,
and other personal information, so review them before sharing.

## Remove Matrix from Chithi

Account removal, leaving rooms, revoking the Matrix session, and removing the
stored credential are separate operations. Use this order so Chithi retains
permission to finish room cleanup:

1. In Chithi, close any unsaved event editor that created a Matrix room, and
   delete every saved calendar event whose Matrix room was created with the
   account you are removing.
2. Keep Chithi online with the desktop credential store unlocked. If a leave
   failed, restart Chithi to retry it, then retry account deletion.
3. Open **Settings**, locate the Matrix account, and choose its trash or
   **Delete** control. Confirm that it no longer appears in Chithi.
4. In another Matrix client or your homeserver's session-management page,
   find the Chithi session or device, normally labelled `Chithi/<version>`,
   and sign it out or revoke it.
5. Current Chithi versions do not remove the Matrix OAuth-keyring entry when
   the local account is deleted. For complete local removal, manually remove
   the deleted account's entry under `in.kushaldas.chithi.oauth` from the
   operating-system credential manager.

Removing the meeting-only Matrix account does not remove the separate
calendar account. Chithi never globally deletes Matrix rooms during this
process, and rooms remain available to any members who have not left.

If you revoked the Chithi Matrix session first, room cleanup can fail and
account deletion can remain blocked. Do not remove the credential-store entry
until the related events are gone and Chithi has successfully released its
room references.

For the locations of Chithi's local data and credentials, see
[Privacy and local data](data-removal.md). Follow that guide as well when you
want to remove all Chithi data from the device.
