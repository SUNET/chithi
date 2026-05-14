# ADR 0045: Link click opens a popup; tracking parameters stripped before opening

## Status

Accepted

## Date

2026-05-14

## Supersedes

[ADR 0001: Copy link to clipboard on click instead of opening browser](0001-copy-link-on-click.md)

## Context

ADR 0001 settled on copying a clicked URL to the clipboard, because every
attempt to spawn a browser from a Tauri webview on Linux ran into
`GDK_BACKEND` / `MOZ_LAUNCHED_CHILD` leakage that broke Firefox's profile
lock detection. The user then had to paste into a browser themselves.

Two things have changed since:

1. **`tauri-plugin-opener` works**, and we already rely on it from the
   `open_oauth_url` command (ADR 0014 / 0030) without the breakage seen
   when calling Rust's `opener` crate or `xdg-open` directly. The plugin
   sanitizes its own child environment before exec.
2. **Tracking parameters are a privacy concern.** `utm_*`, `fbclid`,
   `gclid`, `mc_eid`, and per-provider rules can identify the user to
   the destination site even when the rest of the URL is legitimate. The
   ClearURLs project maintains a crowd-sourced ruleset for exactly this.

The clipboard-only flow also hid useful information: users could not see
where a link pointed before committing to it, and a misclick on a long
link copied something they had to inspect by pasting.

## Decision

A click on a link in mail, calendar, or contact content opens a small
modal popup that shows:

- The original URL.
- The URL that would actually be opened, after stripping tracking
  parameters via the embedded ClearURLs ruleset (or "No tracking
  parameters detected" when the two are identical).
- Three actions: **Copy** (original to clipboard), **Open** (sanitized,
  via `tauri-plugin-opener`), **Cancel**.

Two Tauri commands back the flow:

- `clean_url(url)` returns the sanitized form for the popup preview.
- `open_link(url)` re-sanitizes and opens. It refuses any scheme that is
  not `http://` or `https://`, so a crafted href smuggled through the
  iframe `postMessage` bridge or a hand-edited calendar field cannot
  shell out to `mailto:`, `file:`, `javascript:`, etc.

The hovered URL is mirrored into the status bar (left-aligned, ellipsis,
full URL on tooltip) for both mail-iframe links and the linkified
plain-text fields, so the user can preview the destination without
opening the popup at all.

For plain-text fields (calendar `location`, calendar `description`,
contact `notes`), a `LinkifiedText` component detects `https?://...`
matches client-side and renders them as `<a>` wired into the same popup
and hover flow. Trailing punctuation is trimmed off the matched URL.
Exchange/Outlook calendar descriptions that arrive as a full HTML
document are decoded via `DOMParser` first, so the visible text is
readable and any embedded anchor `href` values are exposed for
linkification.

## Consequences

- The "one extra paste step" cost from ADR 0001 is gone; the popup turns
  it into a one-click confirmation while keeping the user in control of
  whether to copy or open.
- Tracking parameters that previously survived to whichever browser the
  user pasted into are now stripped before either copy or open. (Copy
  copies the original because the user may have a reason to keep
  parameters; Open uses the cleaned form.)
- The browser-spawning failure modes ADR 0001 was avoiding are now the
  plugin's responsibility, not ours. If a particular distro/browser
  combination regresses, that becomes an upstream concern.
- ClearURLs rules are vendored in the `clearurls` crate (~35 KB
  minified JSON). Refreshing the rules means bumping the crate version.
- `mailto:` links are intercepted in the mail iframe and parsed per
  RFC 6068 (`to`/`cc`/`bcc`/`subject`/`body`, query and path forms),
  then handed to `openComposeWindow` with the current active account,
  bypassing the popup. This finally resolves the deferred follow-up
  from ADR 0001.
- `tel:` links are passed through `tauri-plugin-opener` so the OS
  handler picks them up.
- Other schemes (`javascript:`, `file:`, `data:`, ...) remain refused.
