# La Suite Visio integration

Chithi can create La Suite Visio rooms from calendar events. Visio
provides the meeting room; it does not add mail, calendars, or contacts
to Chithi. You need a separate calendar account before you can use the
integration.

La Suite Visio setup and use are currently supported only in the desktop
app. This page covers the complete lifecycle: instance requirements,
account setup, room creation, reauthorization, troubleshooting, and
removal. See [Account setup](accounts.md) for other account types.

## Prerequisites

Before adding La Suite Visio, make sure that:

- You are using the Chithi desktop app.
- You have an account on a compatible La Suite Visio instance.
- Chithi has at least one working calendar account and has loaded a
  calendar in which you can create events.
- You know the exact HTTPS site root, such as
  `https://visio.example.org`. Do not include a path, query, or fragment.
- Your operating-system credential store is available and unlocked.

The instance must run La Suite Meet 1.15.0 or later. Version 1.24.0 or
later is recommended because it includes the stable Outlook add-on.

### Instance requirements

If setup or room creation fails, ask the Visio administrator to confirm
all of these requirements:

- The backend and frontend come from compatible Meet releases, and the
  frontend includes the Outlook add-on and its JavaScript assets.
- `ADDONS_ENABLED` and `EXTERNAL_API_ENABLED` are enabled.
- `ADDONS_TOKEN_SECRET_KEY` and `ADDONS_CSRF_SECRET` are configured, with
  a cache shared by all backend replicas.
- `ADDONS_TOKEN_SCOPE` includes `rooms:create`.
- `APPLICATION_BASE_URL` is the instance's public HTTPS origin.
- `/addons/outlook/transit.html` and
  `/addons/outlook/success.html` serve their dedicated Outlook add-on
  pages from that same origin.

Some Meet releases include the Outlook add-on only in an
Outlook-capable frontend variant. A backend that supports add-ons is not
sufficient if the deployed frontend omits those pages, routes, or assets.

## Add and authorize Chithi

1. Open **Settings** in the Chithi desktop app.
2. Choose **+ Add Account** and select **La Suite Visio**.
3. Optionally enter an **Account Name**, such as `Work Visio`.
4. Enter the exact **Visio instance URL**, for example
   `https://visio.example.org`. The URL must use HTTPS and be the site
   root, with no path, query, or fragment.
5. Choose **Sign in with Visio**. Chithi opens a restricted embedded
   authentication window rather than handing the exchange to the system
   browser.
6. Sign in to Visio and complete any required authentication within
   three minutes. Leave Chithi and its authentication window open.
7. After authentication succeeds, Chithi stores the account and closes
   the authentication flow.

On Linux, the embedded WebKit window may not provide WebAuthn for a
security key or passkey. If the identity provider offers another MFA
method, choose an authenticator code, approval on another device, or
another available alternative. Chithi cannot emulate WebAuthn, and a
handoff to the system browser is not available for this exchange. Ask the
Visio administrator to provide an alternative if WebAuthn is mandatory.

### Authorization and credential storage

Chithi uses Meet's add-on exchange. It receives a short-lived bearer
token with the `rooms:create` scope, which permits Chithi to create a
room. The token does not grant a Visio room update or delete operation.

On desktop, Chithi stores the token in the operating-system credential
store under `in.kushaldas.chithi.oauth`. The token has no refresh token.
When it expires, open the account in **Settings** and choose **Sign in
again with Visio**.

The restricted authentication session lasts three minutes. The resulting
token lifetime is supplied by the Visio instance, but Chithi accepts only a
positive lifetime of at most 30 days. Chithi cannot renew it without another
sign-in.

Chithi communicates directly with the configured Visio instance. During
sign-in, the restricted window may also contact external HTTPS identity
providers selected by that instance. No Chithi-operated application backend
receives the token, calendar data, or room data. See the [privacy
policy](../privacy.md) for more information.

## Use La Suite Visio in Chithi

### Create a room

Visio rooms can currently be added only while creating a new event:

1. Open **Calendar** and create a new event.
2. Select the calendar and enter the event details.
3. Choose **Add _account name_ (La Suite Visio)** below **Location**.
4. Wait for **Creating…** to finish. Chithi places the Visio room URL in
   **Location** and adds a `Join:` line to **Description**.
5. Choose **Create** to save the calendar event.

The remote room is created immediately when you choose the Visio add
button, before the calendar event is saved. The Visio server generates
the room name. Chithi does not send the event title, start time, or
duration as room properties.

### Update an event

Changes to a saved event's title, start time, end time, or duration apply
only to the calendar event. Visio ignores them because its external room
API does not expose room rename or schedule updates.

Current limitations:

- You cannot add La Suite Visio to an event that has already been saved.
- Editing or clearing **Location** on a saved event changes only the calendar
  text. It does not remove Chithi's internal association or delete the room;
  delete the event to release that association.
- Chithi does not rename or reschedule a Visio room when its event
  changes.
- Editing a displayed occurrence of a recurring event changes the series
  master. Deleting a displayed occurrence deletes the recurring series;
  per-occurrence exceptions are not supported.

### Cancel, replace, or delete

The current Visio external API has no room delete operation. A remote
room therefore always remains on the Visio server when you:

- cancel the new-event form after creating the room;
- change or clear its generated **Location** before saving;
- replace its generated link with another meeting provider;
- delete the saved calendar event; or
- remove the La Suite Visio account from Chithi.

Chithi removes the corresponding local link or association when the
event lifecycle permits it, but that local cleanup does not affect the
remote room. Manage the room directly in Visio if it should no longer be
available. There is no deferred Chithi cleanup that can delete it later.

## Troubleshooting

### The authentication window shows Verify your meeting code

Seeing Visio's normal **Verify your meeting code** page during setup is
not a successful add-on sign-in. It means the instance is not serving the
Outlook add-on pages, routes, or JavaScript assets correctly. Ask the
Visio administrator to verify the Outlook-capable frontend and both
`/addons/outlook/` pages listed under instance requirements.

### Sign-in times out or the window closes

Start **Sign in with Visio** again and finish the complete flow within
three minutes. Do not close Chithi or the restricted authentication
window while sign-in is in progress.

On Linux, choose a non-WebAuthn MFA method if one is available. The flow
cannot be moved to the system browser to gain security-key or passkey
support.

### Room creation returns 404 Not Found

A `404 Not Found` response from the external rooms API usually means the
instance is not configured for external room creation. Ask the Visio
administrator to enable `EXTERNAL_API_ENABLED` and set
`APPLICATION_BASE_URL` to the public HTTPS origin.

### Room creation fails for another reason

The event editor displays **Could not create meeting:** followed by the
provider, network, or local-storage error. If the message asks you to sign in
again, reauthenticate the Visio account.

If the error says that Chithi cannot delete an orphaned remote room and
includes a URL, Visio created the room but Chithi could not store its local
lifecycle record. Keep that URL and manage the room directly in Visio. Check
for an unwanted room before retrying after any interrupted request.

### Chithi says the Visio session expired

Visio add-on tokens do not have refresh tokens:

1. Open **Settings**.
2. Open the existing La Suite Visio account.
3. Choose **Sign in again with Visio**.
4. Authenticate on the same Visio origin as the existing account and as
   the same Visio user.

Chithi rejects reauthorization from a different origin or user. The
instance URL cannot be changed after authentication; add a separate
account if you need to use another instance.

If Chithi says the account predates identity-bound sign-in, in-place
reauthentication is unavailable. Close pending event forms, delete related
saved events, remove the account, and then add it again. Existing remote
rooms remain active.

### The credential store is unavailable or the token is missing

Unlock or repair the operating-system credential store, restart Chithi,
and choose **Sign in again with Visio**. On Linux, ensure a Secret Service
provider such as GNOME Keyring or KWallet is running and unlocked. On
macOS or Windows, check Keychain Access or Credential Manager.

### The Visio button is missing from the event editor

Confirm that the La Suite Visio account appears in **Settings** and that
Chithi has loaded a writable calendar. The Visio button appears only in
the new-event editor, not while editing an existing event. Visio account
setup is not offered in the mobile app.

For status, log, and general credential-store guidance, see
[Troubleshooting](troubleshooting.md).

## Remove La Suite Visio from Chithi

Deleting a Visio account removes its local account and stored token only
after no saved or pending meeting association still references it. It cannot
delete rooms from the Visio server.

1. Close any open new-event form that created a Visio room.
2. Delete every related saved event. Editing or clearing **Location** is
   not sufficient. Remote rooms remain even when their events are deleted.
3. If account deletion still reports that meetings require the account,
   wait for local cleanup to finish and retry. Any final advice about Zoom
   authentication in that error does not apply to Visio.
4. Manage any rooms that should no longer be available directly in
   Visio.
5. Open **Settings**, locate the La Suite Visio account, and choose its
   trash or **Delete** control.
6. Read the warning that remote Visio rooms will remain, then confirm
   **Delete**.

Successful deletion removes the local Visio account, its stored bearer
token, and its room-creation button. It does not revoke or delete remote
rooms, including rooms left by cancelled or replaced new events. A
calendar event stored under another account and any saved Visio URL can
also remain until you remove them separately.

Chithi does not call a Visio token-revocation endpoint. Account deletion
removes Chithi's local token copy but does not itself invalidate the
server-issued token, which remains subject to the Visio server's expiry and
revocation policy.

For local database, log, credential, and uninstall details, see
[Privacy and local data](data-removal.md). Removing Chithi's local data or
uninstalling the app likewise does not delete remote Visio rooms.
