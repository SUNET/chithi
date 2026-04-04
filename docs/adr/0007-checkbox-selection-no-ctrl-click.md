# ADR 0007: Checkbox-based message selection instead of Ctrl+click

## Status

Accepted

## Date

2026-04-04

## Context

Standard desktop email clients use Ctrl+click for toggling individual message selection and Shift+click for range selection. We implemented both using two detection methods in parallel:

1. **MouseEvent.ctrlKey** — reading the modifier flag from the native click event
2. **Keyboard tracking** — listening to `window.addEventListener("keydown"/"keyup")` to track whether Ctrl/Shift are held

Testing revealed that **Shift+click works** with both methods in Tauri's WebKitGTK webview on Linux. However, **Ctrl+click is completely swallowed** by WebKitGTK before JavaScript receives it — neither `event.ctrlKey` on the MouseEvent nor the `keydown` event for the Control key fires. This was confirmed via console logging in the running app: Shift shows `shift:true(key:true,evt:true)` while Ctrl shows `ctrl:false(key:false,evt:false)`.

This is a known WebKitGTK behavior on Linux where Ctrl+click is intercepted at the GTK level before reaching the web content.

## Decision

Replace Ctrl+click with **per-row checkboxes** for toggle selection, matching the pattern used by Gmail, Outlook web, and other web-based email clients:

- **Checkbox click** — toggles that message's selection without affecting others (replaces Ctrl+click). Uses `@click.stop` so it doesn't trigger the row click handler.
- **Row click** — selects only that message and loads it in the reader (single select).
- **Shift+click** — range select from last clicked (works via keyboard tracking).
- **Deselect** — click a checked checkbox to remove it from the selection.

The checkbox column appears before the read/star icons in both `MessageListItem` and `ThreadRow`.

## Consequences

- Selection works reliably in Tauri's WebKitGTK webview on Linux without depending on any modifier keys that GTK might intercept.
- The UI pattern is familiar from web email clients — users don't need to discover Ctrl+click.
- Adds a narrow (24px) checkbox column to the message list, slightly reducing space for other columns.
- Shift+click for range selection is retained since it works correctly.
- The keyboard tracking infrastructure (`keydown`/`keyup` listeners) is kept for Shift detection and potential future use if WebKitGTK behavior changes.
