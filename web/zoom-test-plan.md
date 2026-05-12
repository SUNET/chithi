# Zoom Marketplace test plan

This page is written for the Zoom App Marketplace reviewers. It
describes, step by step, how to install Chithi, connect a Zoom account,
and exercise the three Zoom REST scopes that Chithi requests:
`meeting:write:meeting`, `meeting:update:meeting`, and
`meeting:delete:meeting`. The three scopes correspond, one-to-one, to
the three Zoom REST endpoints Chithi calls; each is exercised by a
distinct user action in the calendar event editor.

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
| Scopes requested           | `meeting:write:meeting`, `meeting:update:meeting`, `meeting:delete:meeting`         |
| Zoom REST endpoints called | `POST /v2/users/me/meetings`, `PATCH /v2/meetings/{id}`, `DELETE /v2/meetings/{id}` |
| Where credentials live     | OS-native secret store (Secret Service / Keychain / Cred. Mgr)                      |
| Webhooks / SDK / S2S OAuth | None                                                                                |

Each scope maps to exactly one user-visible action:

| Scope                    | Action in Chithi                                 | REST call                    |
| ------------------------ | ------------------------------------------------ | ---------------------------- |
| `meeting:write:meeting`  | "Add Zoom" button in the event editor            | `POST /v2/users/me/meetings` |
| `meeting:update:meeting` | Saving the event after editing its title or time | `PATCH /v2/meetings/{id}`    |
| `meeting:delete:meeting` | Deleting a calendar event that has a Zoom link   | `DELETE /v2/meetings/{id}`   |

## Prerequisites for the reviewer

The reviewer needs:

1. A working Zoom account (any tier — a free account is sufficient to
   test meeting creation).
2. A mail and calendar account the reviewer already uses. Any of the
   following works, because Chithi only needs _somewhere_ to show a
   calendar event. **Any standards-compliant IMAP + CalDAV or JMAP
   account works without third-party gating; the OAuth-based providers
   are subject to the publisher-verification state of Chithi's other
   pending app reviews.**
   - A generic IMAP + CalDAV account (e.g. Apple Mail, Migadu, a
     self-hosted Dovecot + Radicale server). **Recommended.** No
     third-party review involved.
   - A generic JMAP account (Fastmail, a self-hosted Stalwart server).
     **Recommended.** Same as above.
   - A Microsoft 365 / Outlook account (OAuth). Works today, but
     Microsoft still flags Chithi as an unverified publisher at the
     consent screen, so the reviewer sees a warning and has to click
     through it; sign-in completes normally afterwards.
   - A Gmail account (OAuth or app password). **Likely to fail right now:**
     Chithi's Google OAuth verification is itself still pending review, so
     the Google consent screen typically blocks sign-in with an
     `access_denied` / unverified-app error. Use one of the other three
     account types unless the reviewer wants to confirm the failure mode.
3. A development machine with these tools installed:
   - Rust stable toolchain (via [rustup](https://rustup.rs/)).
   - Node.js v20 or newer.
   - pnpm v10 or newer.
   - Git.
   - Platform-specific system packages listed in the
     [project README](https://github.com/SUNET/chithi#system-dependencies)
     (GTK + WebKit on Linux, Xcode CLT on macOS, MSVC build tools on
     Windows).
   - Chithi is still under development and Zoom-integration is one of
     the features we want before entering public beta where we provide
     prebuilt artifacts to end users.

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

## Step 2 — Add a mail and calendar account

From the account-setup screen, choose the account type the reviewer
prefers. The IMAP / CalDAV and JMAP options are the most reliable
because they don't depend on third-party OAuth verification:

- **Generic IMAP / CalDAV (recommended):** click "Add IMAP account",
  enter server, port, username, password.
- **Generic JMAP (recommended):** click "Add JMAP account", enter the
  JMAP session URL and credentials.
- **Microsoft 365:** click "Add Microsoft account", complete the
  Microsoft OAuth flow. Microsoft still shows an unverified-publisher
  warning for Chithi at the consent screen; click through to proceed.
- **Gmail (likely to fail):** Chithi's Google OAuth verification is
  pending review, so Google currently blocks sign-in with an
  `access_denied` / unverified-app error. Skip this unless the reviewer
  specifically wants to confirm the failure mode.

**Expected result:** Chithi's three-pane mail view loads with the
reviewer's inbox, and the calendar view (left sidebar → Calendar) shows
their existing events.

This step is required because the Zoom integration is exercised from
_inside_ a calendar event editor — there is no standalone "create
meeting" UI.

## Step 3 — Connect the reviewer's Zoom account

In Chithi's main window, open **Settings** (gear icon) from the sidebar.
Zoom is exposed as one of Chithi's _account types_, alongside the mail
and calendar account types.

1. In Settings, click **+ Add Account**.
2. From the account-type chips at the top of the form (Gmail / Microsoft
   365 / IMAP / JMAP / CalDAV / CardDAV / Nextcloud Talk / Matrix /
   **Zoom**), select **Zoom**. The form collapses to a single button
   because Zoom is hosted and needs no per-user server URL.
3. Click **Sign in with Zoom**.

Chithi opens the reviewer's default system browser at Zoom's OAuth
authorize URL. The request is:

- `response_type=code`
- `client_id=<Chithi's public client id>`
- `redirect_uri=https://chithi.org/oauth/zoom`
- `scope=meeting:write:meeting meeting:update:meeting meeting:delete:meeting`
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
listener receives the redirect. The bounce page is purely client-side —
nothing on `chithi.org` reads, logs, or stores the OAuth code. The page
source is visible at
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
4. Click **Add video conference** and choose **Zoom**.

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
case where the reviewer clicks **Add Zoom** _before_ typing the event
title; without it, the Zoom meeting would stay named "Meeting" forever.

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

## Step 8 — Verify that no other endpoints are exercised

The three REST endpoints listed in the table at the top of this page
(plus `zoom.us/oauth/token` for token exchange and refresh) are the only
Zoom REST calls Chithi ever makes. To confirm, the reviewer can:

- Inspect network traffic from the Chithi process (e.g. `mitmproxy`
  configured as the system HTTPS proxy with Chithi's CA store trusting
  the mitmproxy cert). Only the three `api.zoom.us/v2/...` paths above
  and `zoom.us/oauth/token` will appear.
- Read the Zoom-touching source. All Zoom-specific code lives in a
  single file,
  [`src-tauri/src/meet/zoom.rs`](https://github.com/SUNET/chithi/blob/main/src-tauri/src/meet/zoom.rs),
  containing `create_meeting` (`POST /v2/users/me/meetings`),
  `api_update_meeting_topic` and `api_update_meeting_schedule` (both
  `PATCH /v2/meetings/{id}`), `api_delete_meeting`
  (`DELETE /v2/meetings/{id}`), and the `get_access_token` helper that
  drives the OAuth refresh. The generic PKCE / code-exchange / keychain
  plumbing it sits on top of lives in
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
- The **Add video conference → Zoom** option no longer appears in the
  calendar event editor (until a Zoom account is added again).

Note: removing the account in Chithi clears the _local_ credentials only
— it does not call Zoom's token revocation endpoint, because Chithi only
ever talks to `api.zoom.us/v2/users/me/meetings`,
`api.zoom.us/v2/meetings/{id}` (for PATCH and DELETE), and
`zoom.us/oauth/token`. A reviewer who wants Zoom-side revocation as well
should additionally uninstall Chithi from
[Zoom's installed-apps page](https://marketplace.zoom.us/user/installed).

A reviewer who cloned Chithi solely to run this test plan can uninstall
it after this step by deleting the cloned repository; no system files
outside the (now-deleted) keychain entry need to be cleaned up.

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
  <source src="zoom-test-plan-demo.mp4" type="video/mp4">
  Your browser does not support inline video playback. The file is
  also available at
  <a href="zoom-test-plan-demo.mp4">zoom-test-plan-demo.mp4</a>.
</video>

## Contact

The maintainer monitoring the Zoom Marketplace contact email is
reachable at `hej@mic.ke`. Replies to reviewer questions are typically
sent the same business day (Europe/Stockholm).
