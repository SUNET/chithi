# Zoom Marketplace test plan

This page is written for the Zoom App Marketplace reviewers. It
describes, step by step, how to install Chithi, connect a Zoom account,
and exercise the four Zoom REST scopes that Chithi requests:
`meeting:write:meeting`, `meeting:update:meeting`,
`meeting:delete:meeting`, and `user:read:user`. The first three scopes
manage meetings from the calendar event editor. The fourth verifies that
reauthorization is for the same Zoom user and account.

Chithi is a community-driven, open-source desktop project hosted by
SUNET (the Swedish University Computer Network) under the GPL-3.0
license. There is no Chithi-operated backend and no hosted service, so
we cannot issue test accounts. Instead, the reviewer runs Chithi locally
from source and signs in with their own mail / calendar account and
their own Zoom account, exactly the way an end user would. The flow
below has been rehearsed end-to-end on a fresh machine.

A pre-recorded walkthrough video covering the same steps is embedded at
the bottom of this page.

## What is being tested

| Item                       | Value                                                                               |
| -------------------------- | ----------------------------------------------------------------------------------- |
| App name                   | Chithi                                                                              |
| App type                   | OAuth, user-managed, public client with PKCE (no secret)                            |
| Scopes requested           | `meeting:write:meeting`, `meeting:update:meeting`, `meeting:delete:meeting`, `user:read:user` |
| Zoom REST endpoints called | `GET /v2/users/me`, `POST /v2/users/me/meetings`, `PATCH /v2/meetings/{id}`, `DELETE /v2/meetings/{id}` |
| Where credentials live     | OS-native secret store (Secret Service / Keychain / Cred. Mgr)                      |
| Webhooks / SDK / S2S OAuth | None                                                                                |

Each scope maps to exactly one user-visible action:

| Scope                    | Action in Chithi                                 | REST call                    |
| ------------------------ | ------------------------------------------------ | ---------------------------- |
| `meeting:write:meeting`  | "Add account name (Zoom)" in the event editor    | `POST /v2/users/me/meetings` |
| `meeting:update:meeting` | Saving the event after editing its title or time | `PATCH /v2/meetings/{id}`    |
| `meeting:delete:meeting` | Deleting a calendar event that has a Zoom link   | `DELETE /v2/meetings/{id}`   |
| `user:read:user`         | Sign-in and reauthorization identity check       | `GET /v2/users/me`           |

## Prerequisites for the reviewer

The reviewer needs:

1. A working Zoom account (any tier — a free account is sufficient to
   test meeting creation).
2. A calendar account the reviewer already uses. Chithi only needs
   somewhere to store the event used by this test. OAuth-based providers
   can be subject to publisher verification and tenant consent policy.
   - A standalone CalDAV account, such as Radicale. **Recommended.** No
     third-party review is involved. Add an IMAP account separately only
     if mail is also needed.
   - A generic JMAP account on a server with calendar support, such as
     Stalwart. **Recommended.** No third-party review is involved.
   - A Fastmail account, added through Chithi's dedicated Fastmail type
     with a Fastmail API token.
   - A Microsoft 365 account (OAuth), if the tenant permits or an
     administrator approves Chithi's delegated permissions.
   - A Gmail account, if Google permits the OAuth consent flow. Gmail
     mail additionally requires an app password; Calendar uses OAuth.
3. A development machine with these tools installed:
   - Rust stable toolchain (via [rustup](https://rustup.rs/)).
   - Node.js 20.19.x, or 22.12 or newer.
   - pnpm v10 (the repository pins the exact version).
   - Git.
   - Platform-specific system packages listed in the
     [project README](https://github.com/SUNET/chithi#system-dependencies)
     (GTK + WebKit on Linux, Xcode CLT on macOS, MSVC build tools on
     Windows).
   - No prebuilt package is required; this plan runs Chithi from source.

Estimated time to complete the full test plan from a fresh checkout:
**15–25 minutes**, most of which is the first `cargo` build.

## Step 1 — Clone and run Chithi

In a terminal:

```bash
git clone https://github.com/SUNET/chithi.git
cd chithi
pnpm install
pnpm tauri dev
```

`pnpm tauri dev` starts a Vite dev server and launches the Chithi
desktop window. The first run compiles the Rust backend and will take
several minutes; subsequent runs are fast.

**Expected result:** the Chithi desktop window opens and shows the
account-setup screen.

## Step 2 — Add a calendar account

On first launch, choose **Skip and add an account later**, then open
**Settings** and choose **+ Add Account**. Pick one of these cards:

- **CalDAV (recommended):** enter the account name, server username,
  password, and complete HTTPS CalDAV URL.
- **JMAP (recommended):** enter the email address, JMAP URL, and password
  or complete OIDC sign-in. The server must advertise calendar support.
- **Fastmail:** enter the email address and a bearer API token generated
  under Fastmail's **Privacy & Security → Manage API tokens** settings.
- **Microsoft 365:** complete the Microsoft browser OAuth flow. Tenant
  policy can require administrator approval.
- **Gmail:** enter a Gmail app password for mail and complete Google
  browser OAuth for Calendar and Contacts. Google publisher-verification
  state can affect whether sign-in is permitted.

**Expected result:** the calendar view in the sidebar shows the
reviewer's calendars and existing events.

This step is required because the Zoom integration is exercised from
_inside_ a calendar event editor — there is no standalone "create
meeting" UI.

## Step 3 — Connect the reviewer's Zoom account

In Chithi's main window, open **Settings** (gear icon) from the sidebar.
Zoom is exposed as one of Chithi's _account types_, alongside the mail
and calendar account types.

1. In Settings, click **+ Add Account**.
2. In the account-type picker, select the **Zoom** card. The form contains
   only a sign-in button because Zoom is hosted and needs no per-user
   server URL.
3. Click **Sign in with Zoom**.

Chithi opens the reviewer's default system browser at Zoom's OAuth
authorize URL. The request is:

- `response_type=code`
- `client_id=<Chithi's public client id>`
- `redirect_uri=https://chithi.org/oauth/zoom`
- `scope=meeting:write:meeting meeting:update:meeting`
  `meeting:delete:meeting user:read:user`
- `code_challenge=<PKCE S256>`
- `state=<random>`

The reviewer signs in to Zoom (if not already signed in) and approves
the scope. Zoom redirects to `https://chithi.org/oauth/zoom`, a static
page hosted on GitHub Pages.

Why the HTTPS bounce exists: Zoom's production OAuth policy rejects
loopback (`http://127.0.0.1:…`) redirect URIs entirely, so the
registered redirect has to be an HTTPS URL. The page at
`chithi.org/oauth/zoom` runs a small client-side JavaScript snippet that
rewrites its own URL to `http://127.0.0.1:47832/?code=…&state=…` and
calls `window.location.replace(...)`. Chithi has already bound a TCP
listener on `127.0.0.1:47832` just before opening the browser, and that
listener receives the redirect. The forwarding logic is client-side and
no Chithi-operated application server processes the code. GitHub Pages
serves the request and therefore receives its URL. The page source is
visible at
[github.com/SUNET/chithi/tree/main/web/oauth/zoom](https://github.com/SUNET/chithi/tree/main/web/oauth/zoom).

Chithi exchanges the code for tokens directly with Zoom
(`https://zoom.us/oauth/token`, PKCE verifier, no client secret), writes
the tokens to the OS keychain, and returns to the **Accounts** screen.

**Expected result:** the **Add Account** form closes and a new entry for
the reviewer's Zoom account appears in the Settings accounts list, with
edit and delete controls beside it.

## Step 4 — Create a Zoom meeting from a calendar event

This step exercises `meeting:write:meeting` /
`POST /v2/users/me/meetings`.

1. Switch to the calendar view.
2. Click any time slot to open the new-event editor.
3. Fill in a title, e.g. "Zoom marketplace review meeting".
4. Click **Add _account name_ (Zoom)**. Chithi shows one direct button
   for each configured meeting account.

Chithi calls `POST https://api.zoom.us/v2/users/me/meetings` with the
reviewer's access token. The request body contains a topic (the event
title at the moment of the click, or "Meeting" if the title field was
still empty), the event's start time as an ISO 8601 UTC string, the
duration in minutes, and `timezone: "UTC"`, no other data. Chithi
inserts the `join_url` from Zoom's response into the event's `LOCATION`
and `DESCRIPTION` fields and stores the meeting's Zoom id in a local
SQLite side-table keyed on the event so the rename / reschedule / delete
steps below can act on it.

5. Save the event.

**Expected result:**

- The event appears in the calendar with the Zoom join URL visible.
- The same meeting is now listed in the reviewer's Zoom account under
  **Meetings → Upcoming** on web.zoom.us, on the day the calendar event
  was created for.
- The reviewer can click the join URL from the calendar event and the
  Zoom client opens the meeting normally.

## Step 5 — Rename the meeting via the event title

This step exercises `meeting:update:meeting` /
`PATCH /v2/meetings/{id}`. It is also the path that fixes the common
case where the reviewer clicks **Add _account name_ (Zoom)** before
typing the event title; without it, the Zoom meeting would stay named
"Meeting" forever.

1. Open the event created in Step 4.
2. Edit the title, e.g. to "Renamed marketplace review meeting".
3. Save.

Chithi compares the saved title with the pre-edit title (and also runs
this step unconditionally on the first save after Step 4 in case the
title was empty at button-click time). When the title needs to change on
Zoom's side it issues `PATCH https://api.zoom.us/v2/meetings/{id}` with
a body containing only the `topic` field set to the new title.

**Expected result:**

- The meeting on web.zoom.us now shows the new title under **Meetings →
  Upcoming**.

## Step 6 — Reschedule the meeting by moving the calendar event

This step also exercises `meeting:update:meeting` /
`PATCH /v2/meetings/{id}`, but with a different body shape (the start
time and duration rather than the topic).

1. Open the event from Step 4.
2. Change the start date or start time (or both); the end time adjusts
   to keep the duration unless the reviewer overrides it.
3. Save.

Chithi detects that `start_time` or `end_time` changed and issues
`PATCH https://api.zoom.us/v2/meetings/{id}` with a body containing
`start_time` (ISO 8601 UTC), `duration` (whole minutes), and
`timezone: "UTC"`.

**Expected result:**

- The meeting on web.zoom.us moves to the new slot.

## Step 7 — Cancel the meeting by deleting the event

This step exercises `meeting:delete:meeting` /
`DELETE /v2/meetings/{id}`.

1. Open the event from Step 4 (or right-click it in the calendar grid).
2. Click **Delete event** and confirm.

Chithi looks up the event's Zoom meeting id in its local side table and
issues `DELETE https://api.zoom.us/v2/meetings/{id}` _before_ removing
the local event row. A 404 from Zoom is treated as success (the meeting
was already gone, e.g. cancelled from web.zoom.us in another tab), so
the local cleanup is idempotent.

**Expected result:**

- The event disappears from Chithi's calendar.
- The meeting disappears from **Meetings → Upcoming** on web.zoom.us.

## Step 8 — Verify the endpoints exercised

The four REST endpoints listed in the table at the top of this page
(plus `zoom.us/oauth/token` for token exchange and refresh) are the Zoom
REST calls Chithi makes. To confirm, the reviewer can:

- Inspect network traffic from the Chithi process (e.g. `mitmproxy`
  configured as the system HTTPS proxy with Chithi's CA store trusting
  the mitmproxy cert). Only the four `api.zoom.us/v2/...` paths above
  and `zoom.us/oauth/token` will appear.
- Read the Zoom-touching source. API request and response handling lives
  in
  [`src-tauri/src/meet/zoom.rs`](https://github.com/SUNET/chithi/blob/main/src-tauri/src/meet/zoom.rs),
  containing the identity check (`GET /v2/users/me`), `create_meeting`
  (`POST /v2/users/me/meetings`),
  `api_update_meeting_topic` and `api_update_meeting_schedule` (both
  `PATCH /v2/meetings/{id}`), `api_delete_meeting`
  (`DELETE /v2/meetings/{id}`), and the `get_access_token` helper that
  drives the OAuth refresh. Sign-in orchestration, identity binding, and
  account persistence live in
  [`src-tauri/src/commands/meet.rs`](https://github.com/SUNET/chithi/blob/main/src-tauri/src/commands/meet.rs).
  The provider scopes and generic PKCE / code-exchange / keychain
  plumbing live in
  [`src-tauri/src/oauth.rs`](https://github.com/SUNET/chithi/blob/main/src-tauri/src/oauth.rs)
  and is shared with the Gmail and Microsoft 365 integrations.

## Step 9 — Disconnect

Disconnection is done the same way as any other account in Chithi: by
removing the Zoom account from the Settings accounts list.

1. Open **Settings**.
2. Locate the Zoom account in the accounts list.
3. Click the trash icon next to it and confirm the deletion in the
   **Delete Account** dialog.

**Expected result:**

- The Zoom account disappears from the accounts list.
- Chithi removes the OAuth access and refresh tokens for that account
  from the OS keychain.
- The **Add _account name_ (Zoom)** button no longer appears in the
  calendar event editor (until a Zoom account is added again).

Note: removing the account in Chithi clears the _local_ credentials only
— it does not call Zoom's token revocation endpoint, because Chithi only
ever talks to `api.zoom.us/v2/users/me`,
`api.zoom.us/v2/users/me/meetings`,
`api.zoom.us/v2/meetings/{id}` (for PATCH and DELETE), and
`zoom.us/oauth/token`. A reviewer who wants Zoom-side revocation as well
should additionally uninstall Chithi from
[Zoom's installed-apps page](https://marketplace.zoom.us/user/installed).

A reviewer who cloned Chithi solely to run this test plan can remove the
build by deleting the cloned repository. Chithi's database, log, and
preferences are stored separately under the user's local application-data
directory. See [Removing Chithi data](user/data-removal.md) if those
files should also be removed.

## Deauthorization

If the reviewer revokes Chithi from
[Zoom's installed-apps page](https://marketplace.zoom.us/user/installed)
_without_ first removing the account in Chithi, the locally stored
refresh token becomes invalid on Zoom's side. The next time Chithi tries
to use it — either silently when refreshing the access token for a new
"Add video conference → Zoom" click, or visibly on the next meeting
creation, reschedule, rename, or cancel — Zoom responds with
`invalid_grant` and Chithi surfaces a sign-in error. The reviewer can
clear the stale tokens by deleting the Zoom account in Chithi (Step 9)
and, if desired, adding it again.

## Demo video

A screen recording is embedded below. It covers connecting a Zoom
account and creating a meeting from a calendar event in Chithi.

<video controls preload="metadata" style="max-width: 100%; height: auto;">
  <source src="/zoom-test-plan-demo.mp4" type="video/mp4">
  Your browser does not support inline video playback. The file is
  also available at
  <a href="/zoom-test-plan-demo.mp4">zoom-test-plan-demo.mp4</a>.
</video>

## Contact

The maintainer monitoring the Zoom Marketplace contact email is
reachable at `hej@mic.ke`. Replies to reviewer questions are typically
sent the same business day (Europe/Stockholm).
