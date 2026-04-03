# ADR 0001: Copy link to clipboard on click instead of opening browser

## Status

Accepted

## Date

2026-04-03

## Context

When a user clicks a link inside an HTML email rendered in the Tauri webview, we need to decide how to handle that navigation. The default webview behavior navigates away from the app, which is unacceptable.

We initially implemented opening the system default browser using:
1. `@tauri-apps/plugin-shell` `open()` API
2. The `opener` Rust crate
3. Spawning `xdg-open` / `gio open` with environment scrubbing

All three approaches failed on Linux because Tauri's WebKitGTK runtime leaks environment variables (`GDK_BACKEND`, `MOZ_LAUNCHED_CHILD`, etc.) that cause Firefox's profile lock detection to believe another instance is already running, resulting in a "Firefox is already running, but is not responding" error dialog. Even fully detaching the child process via `setsid` and stripping known problematic environment variables did not reliably resolve the issue across all configurations.

## Decision

Clicking a link in an email copies the URL to the clipboard and shows a brief toast notification ("Link copied to clipboard"). The user then pastes the URL into whichever browser they choose.

Right-click context menus from the underlying webview are suppressed on the email body to prevent accidental navigation or exposing browser-internal options that don't apply to an email client.

## Consequences

- No browser-spawning code or associated platform-specific workarounds to maintain.
- User has full control over which browser and profile receives the URL.
- One extra step (paste) compared to direct browser opening, but predictable and reliable across all Linux desktop environments, display servers (X11/Wayland), and browser configurations.
- `mailto:` links are also intercepted; they will open the compose window once implemented.
