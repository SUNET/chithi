# ADR 0005: No JavaScript execution in HTML email rendering

## Status

Accepted

## Date

2026-04-04

## Context

HTML emails can contain JavaScript via `<script>` tags, inline event handlers (`onclick`, `onerror`), `javascript:` URIs, and embedded interactive elements (`<iframe>`, `<object>`, `<embed>`, `<form>`). Executing any of these in the Tauri webview would give the email sender access to the application's JS context, enabling data exfiltration, UI spoofing, or worse.

## Decision

JavaScript execution is blocked at two independent layers:

### Layer 1: Rust backend (ammonia sanitizer)

The `ammonia` HTML sanitizer processes all email HTML server-side before it reaches the frontend. It:

- Strips `<script>` and `<style>` tags including their contents
- Removes all event handler attributes (`onclick`, `onerror`, `onload`, etc.)
- Restricts URL schemes to `http`, `https`, and `mailto` — `javascript:` URIs are blocked
- Removes interactive and embedding elements: `<img>`, `<iframe>`, `<object>`, `<embed>`, `<form>`, `<input>`, `<button>`, `<textarea>`, `<select>`, `<video>`, `<audio>`, `<source>`, `<svg>`, `<math>`
- Allows inline `style` attributes for basic formatting (CSS cannot execute JS in modern browsers)

### Layer 2: Frontend (defense-in-depth)

Before rendering via Vue's `v-html`, a `sanitizeHtml()` function performs a second pass:

- Removes any `<script>`, `<style>`, `<iframe>`, `<object>`, `<embed>` elements
- Strips all attributes starting with `on` (event handlers)
- Removes `href` attributes containing `javascript:` URIs

This layer exists as a safety net — if ammonia has a bug or a bypass is discovered, the frontend layer independently blocks the same attack vectors.

## Consequences

- No JavaScript from email content can execute in the application context
- HTML emails render as static formatted text — no interactivity (forms, buttons, embedded media)
- Some visually rich emails may appear degraded without images and embedded content, but this is an acceptable trade-off for security
- The two-layer approach means a vulnerability in either layer alone is not sufficient for exploitation
- `<img>` removal also serves the privacy goal of blocking tracking pixels (see ADR 0003)
