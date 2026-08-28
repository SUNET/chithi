# Zoom integration

Chithi can create and manage Zoom meetings from calendar events. Zoom
provides the meeting; it does not add mail, calendars, or contacts to
Chithi. You need a separate calendar account before you can use the
integration.

This page covers the complete lifecycle of the integration: adding and
authorizing Chithi, using it to manage meetings, resolving common
problems, and disconnecting it safely.

## Prerequisites

Before adding Zoom, make sure that:

- Chithi is installed and running.
- You have a Zoom account. A free Zoom account is sufficient for meeting
  creation.
- Chithi has at least one working calendar account and has loaded a
  calendar in which you can create events.
- Your default browser can open links to Zoom and return to a loopback
  address on your computer.
- TCP port 47832 is free while you sign in.
- On desktop, your operating-system credential store is available and
  unlocked.

## Add and authorize Chithi

1. Open **Settings** in Chithi.
2. Choose **+ Add Account** on desktop or **Add account** on mobile.
3. Select **Zoom**.
4. Optionally enter an **Account Name**, such as `Work`. If you leave it
   empty, Chithi uses `Zoom`.
5. Choose **Sign in with Zoom**. There is no Zoom server URL to enter and
   no separate save step.
6. Leave Chithi running. In the browser, sign in to Zoom if necessary and
   approve the requested permissions within five minutes.
7. Zoom sends the browser to `https://chithi.org/oauth/zoom`. The page
   returns the authorization response to Chithi on your computer. If it
   does not return automatically, choose **Open Chithi** on that page.
8. After authorization succeeds, Chithi closes the account form and
   returns to the main view. Open **Settings** again to see the new Zoom
   account in the accounts list.

### Permissions requested

Chithi requests permissions for the signed-in user only:

| Zoom permission | How Chithi uses it |
| --- | --- |
| `meeting:write:meeting` | Create a Zoom meeting from a new calendar event. |
| `meeting:update:meeting` | Rename or reschedule a meeting when its event changes. |
| `meeting:delete:meeting` | Delete a meeting when its calendar event is deleted. |
| `user:read:user` | Verify the Zoom user and account during sign-in and reauthorization. |

Chithi is a public desktop OAuth client. It uses Authorization Code with
PKCE and does not contain a Zoom client secret. The HTTPS redirect is a
static page hosted on GitHub Pages; GitHub Pages receives the request URL,
including the short-lived authorization code, before client-side
JavaScript forwards it to `http://127.0.0.1:47832/`. PKCE prevents that
code from being redeemed without the verifier held by Chithi.

Chithi exchanges the code and communicates with Zoom directly. No
Chithi-operated application backend receives your authorization code,
tokens, calendar data, or meeting data. On desktop, Zoom tokens are
stored in the operating-system credential store under
`in.kushaldas.chithi.oauth`. See the [privacy policy](../privacy.md) for
more information.

## Use Zoom in Chithi

### Create a meeting

Zoom meetings can currently be added only while creating a new event:

1. Open **Calendar** and create a new event.
2. Select the calendar and enter the title, date, and time.
3. Choose **Add _account name_ (Zoom)** below **Location**.
4. Wait for **Creating…** to finish. Chithi places the Zoom join URL in
   **Location** and adds a `Join:` line to **Description**.
5. Choose **Create** to save the event.

The Zoom meeting is created as soon as you choose **Add _account name_
(Zoom)**, before the calendar event is saved. If you cancel the event,
replace the generated link with another meeting provider, or change the
generated **Location** before choosing **Create**, Chithi requests cleanup
of the meeting it just created. Failed cleanup remains queued for a later
retry, including after Chithi restarts.

Chithi creates a scheduled meeting using the event title, start time, and
duration. If the title is empty when you add Zoom, the initial topic is
`Meeting`; saving a titled event then updates it. Meetings are created
with join-before-host enabled and the Zoom waiting room disabled. Review
the meeting in Zoom before sharing it if those defaults are not suitable.

### Update a meeting

Open the saved event, choose **Edit**, change its title or timed start/end
values, and choose **Save**. Chithi attempts to apply the corresponding
topic or schedule change to Zoom.

Calendar saving and Zoom updating are separate operations. The calendar
change can succeed even if Zoom is temporarily unavailable, so verify the
meeting in Zoom when the remote schedule is important.

Current limitations:

- You cannot add Zoom to an event that has already been saved.
- Editing or clearing **Location** on a saved event does not remove the
  internal Zoom association or delete the Zoom meeting.
- Changing an all-day event does not reschedule its existing Zoom
  meeting.
- Editing a displayed occurrence of a recurring event changes the series
  master rather than only that occurrence.
- Chithi does not use Zoom webhooks and does not import changes made in
  Zoom.

### Delete a meeting

1. Open the calendar event that contains the Zoom meeting.
2. Choose **Delete**.
3. If the event has attendees and Chithi asks about notification, choose
   **Send Cancellation**, **Delete Only**, or **Cancel**.

Chithi removes the local event and queues deletion of its associated Zoom
meeting. It then attempts the Zoom deletion. If Zoom is temporarily
unavailable, Chithi retains the cleanup request for retry. A meeting that
was already deleted in Zoom is treated as successfully cleaned up.

If complete removal is important, verify that the meeting no longer
appears under **Meetings → Upcoming** in Zoom. Merely editing the event's
**Location** field does not delete the meeting.

## Troubleshooting

### Browser sign-in does not return to Chithi

- Keep Chithi open throughout authorization and finish within five
  minutes.
- On the `chithi.org` return page, choose **Open Chithi** if automatic
  forwarding does not work.
- Check whether a firewall, browser extension, proxy, or endpoint-security
  tool blocks connections to `http://127.0.0.1:47832/`.
- Restart the sign-in flow if it timed out or if you denied the requested
  permissions.

### Chithi says port 47832 is already in use

Zoom authorization uses the fixed loopback TCP port 47832. Close the
other program or abandoned Chithi process using the port, then choose
**Sign in with Zoom** again.

### Chithi could not verify the sign-in

If Chithi reports **Zoom sign-in could not be verified — please try
again**, start a new sign-in flow. Do not reuse an old return page or edit
its URL; Chithi validates a one-time OAuth state value.

### The Zoom button is missing from the event editor

Confirm that the Zoom account appears in **Settings** and that Chithi has
loaded at least one writable calendar. The **Add _account name_ (Zoom)**
button appears only in the new-event editor, not while editing an existing
event.

### Meeting creation fails

The event editor displays **Could not create meeting:** followed by the
provider or network error. Check your network connection and Zoom service
status, then retry. If the message says that sign-in expired or no tokens
were found, reauthorize the account as described below. Check your Zoom
meeting list for an unwanted meeting before retrying after an interrupted
request.

### Zoom sign-in expired

Chithi refreshes Zoom access tokens automatically while the authorization
remains valid. If Zoom rejects the refresh token:

1. Open **Settings**.
2. Open the existing Zoom account for editing.
3. Choose **Sign in again with Zoom**.
4. Sign in as the same Zoom user in the same Zoom account.

When meetings still refer to this account, Chithi refuses to replace its
credentials with a different Zoom identity. It reports **Zoom sign-in
belongs to a different user; the existing account was not changed**.

Older Chithi accounts that predate identity-bound sign-in cannot be
safely reauthorized while they still own meetings. Delete those meetings
manually in Zoom and use the **Delete locally** procedure below if normal
cleanup is no longer possible.

### Zoom account deletion is blocked

Normal deletion can report:

> Cannot delete this account while meetings still require it. Delete the
> related calendar events and wait for meeting cleanup to finish. If Zoom
> authentication expired, sign in again first.

Delete the related events in Chithi, reauthorize Zoom if needed, and retry
account deletion. Restarting Chithi also retries queued meeting cleanup.
Verify important deletions in Zoom. If Chithi can no longer perform remote
cleanup, use **Delete locally** only after reading its impact below.

### The credential store is unavailable

On Linux, ensure a Secret Service provider such as GNOME Keyring or
KWallet is running and unlocked. On macOS or Windows, check Keychain
Access or Credential Manager. Unlock or repair the credential store,
restart Chithi, and retry sign-in or account deletion.

## Remove and deauthorize Chithi

Disconnecting the local Zoom account and deauthorizing the Marketplace
app are separate operations. Use this order so Chithi still has permission
to clean up meetings:

1. In Chithi, delete every calendar event whose Zoom meeting was created
   with the account you are removing.
2. Verify under **Meetings → Upcoming** in Zoom that those meetings are
   gone. If cleanup is pending, restart Chithi to retry it. If
   authorization expired, reauthorize the account and retry normal account
   deletion; that action also retries queued cleanup.
3. Open **Settings**, locate the Zoom account, and choose its trash or
   **Delete** control.
4. In **Delete Account**, leave **I understand that remote Zoom meetings
   may remain** unchecked and choose **Delete**.
5. Confirm that the Zoom account no longer appears in Chithi.
6. Sign in to [Zoom's installed-apps
   page](https://marketplace.zoom.us/user/installed), locate Chithi, and
   remove or uninstall it to revoke Zoom-side authorization.

Successful normal deletion removes the local Zoom account and its OAuth
tokens. It also removes the **Add _account name_ (Zoom)** button. Chithi
does not call a Zoom token-revocation endpoint and deleting the local
account does not uninstall Chithi from Zoom Marketplace; step 6 completes
deauthorization.

Normal account deletion refuses to proceed while Chithi still owns a
meeting that requires the account. It does not delete calendar events
stored under another account merely because you remove Zoom.

### Delete locally when remote cleanup is impossible

Use this fallback if Zoom authorization cannot be restored or remote
meetings cannot be cleaned up:

1. Open the Zoom account's **Delete Account** dialog in Chithi.
2. Select **I understand that remote Zoom meetings may remain**.
3. The confirmation action changes to **Delete locally**. Choose it.
4. Manage any remaining meetings directly in Zoom, then remove Chithi
   from [Zoom's installed-apps
   page](https://marketplace.zoom.us/user/installed).

**Delete locally** removes Chithi's local Zoom account, OAuth tokens,
pending cleanup records, and meeting associations. It does not guarantee
that remote Zoom meetings are deleted. Calendar events stored by another
account and their saved join URLs can remain, while Chithi loses the
information needed to update or delete those meetings. Remaining meetings
may stay active until you remove them in Zoom.

### If Chithi was removed from Zoom first

Removing Chithi through Zoom Marketplace invalidates the stored
authorization. Chithi may then be unable to clean up meetings or complete
normal account deletion. Reauthorize the existing account with **Sign in
again with Zoom** and follow the safe removal order above, or delete the
meetings directly in Zoom and use **Delete locally**.
