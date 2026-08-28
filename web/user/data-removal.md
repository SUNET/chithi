# Privacy and local data

Chithi has no project-operated backend for synchronizing your mail,
calendar, contacts, or meetings. It communicates directly with the
providers and servers you configure. Normal operation can also contact
OAuth endpoints, account-discovery services, meeting APIs, WKD servers
when fetching OpenPGP keys, and remote-image hosts when you choose to
load images.

## Data and log locations

Desktop data is stored below the platform's local application-data
directory:

| Platform | Expected base directory |
| --- | --- |
| Linux | `${XDG_DATA_HOME:-$HOME/.local/share}/chithi/` |
| macOS | `~/Library/Application Support/chithi/` |
| Windows | `%LOCALAPPDATA%\chithi\` |

Linux uses `XDG_DATA_HOME`, defaulting to `~/.local/share`. The
application derives macOS and Windows paths from the operating system.
Verify the exact location on a packaged build before scripting removal.

The base directory contains:

- `chithi.db`, the local SQLite database
- `chithi.log`, the application log
- one directory per mail account, containing downloaded mail

UI preferences and onboarding state are stored in the WebView's local
storage. Mobile platforms use their sandboxed application-data directory.

## Credentials

On desktop, account passwords use the keyring service
`in.kushaldas.chithi`; OAuth credentials use
`in.kushaldas.chithi.oauth`. Android OAuth credentials are JSON files in
the private application sandbox.

OpenPGP keys are separate. Chithi shares Tumpa's keystore, normally at
`~/.tumpa/keys.db`, unless `TUMPA_DIR` or `TUMPA_KEYSTORE` overrides it.
Deleting that keystore can affect other software and can permanently
destroy private keys. Export and verify a backup first.

## Remove one account

In **Settings**, delete the account and confirm the warning. Chithi stops
its worker, removes its database data, attempts to remove its downloaded
mail directory, and attempts to remove its normal password credential.
Mail-directory deletion errors do not make account deletion fail, so
verify the account's directory is gone when secure erasure matters.

For Zoom, first delete related calendar events and verify that their
meetings are gone in Zoom. Then delete the account in Chithi and finally
remove Chithi from [Zoom's installed-apps
page](https://marketplace.zoom.us/user/installed). Removing the
Marketplace app first can invalidate the token Chithi needs for meeting
cleanup. Normal local deletion does not itself revoke Zoom-side
authorization.

Chithi refuses normal Zoom account deletion while meetings still require
the account. The **Delete locally** fallback removes local tokens and
meeting associations but can leave remote meetings active, calendar
events and join URLs in place, and Chithi unable to manage those meetings.
Follow the [complete Zoom removal instructions](zoom.md#remove-and-deauthorize-chithi)
before using this fallback.

For Nextcloud Talk, delete related calendar events while Chithi still has
its app password, verify the conversations are gone in Talk, delete the
local account, and then revoke Chithi's app password in Nextcloud. See the
[Nextcloud Talk removal guide](nextcloud-talk.md#remove-nextcloud-talk-from-chithi).

For Matrix / Element Call, deleting an event makes Chithi leave the room;
it does not globally delete the Matrix room or erase it for other members.
After cleanup and local account deletion, revoke the Chithi Matrix session
or device. See the [Matrix removal guide](matrix-element-call.md#remove-matrix-from-chithi).

La Suite Visio's current external API cannot delete rooms. Remote rooms
remain after event and account deletion, and must be managed directly in
Visio. See the [La Suite Visio removal
guide](la-suite-visio.md#remove-la-suite-visio-from-chithi).

Account deletion should not be treated as proof that every provider
credential has been erased. If complete credential removal is required,
inspect the operating-system credential manager for both Chithi service
names after deleting the account.

## Remove Chithi data as completely as possible

1. Remove accounts in Chithi where possible, so provider-specific
   cleanup can run.
2. Exit Chithi completely.
3. Back up anything you may need to restore.
4. Uninstall the application package.
5. Remove Chithi's documented application-data directory.
6. Remove entries for `in.kushaldas.chithi` and
   `in.kushaldas.chithi.oauth` from the operating-system credential
   manager.
7. Remove the Tumpa keystore only if you intentionally want to delete
   the OpenPGP keys shared with other applications.

Uninstalling a desktop package alone does not reliably remove user data
or keyring entries. Chithi does not currently provide a general
**Clear cache** action or expose the WebView storage location. UI
preferences can therefore remain in platform- and WebView-specific
storage after these steps. Use the operating system's application-data
management tools and the application identifier `in.kushaldas.chithi`
to locate additional storage. A fully deterministic cleanup requires a
future in-app data-removal action.
