# ADR 0026: Sandboxed HTML Email Rendering

## Status
Accepted

## Context
HTML emails were rendered using Vue's `v-html` directive directly in the main Tauri webview. Despite double-sanitization (ammonia on the backend, a second JS pass on the frontend), a sanitization bypass would give an attacker full access to `window.__TAURI__` and every IPC command — credentials, message operations, file system access, and shell execution.

This was the highest-severity finding in the pre-public-preview security audit (security0.md item 1): untrusted email content shared the same JavaScript context and origin as the application shell.

## Decision
Replace `v-html` with a **sandboxed `<iframe srcdoc>`** so untrusted HTML runs in a completely isolated browsing context.

### Iframe element
```html
<iframe
  class="email-sandbox"
  :srcdoc="iframeSrcdoc()"
  sandbox="allow-scripts"
  referrerpolicy="no-referrer"
/>
```

### Why these sandbox flags
- **`allow-scripts`** — the iframe contains a small inline relay script for link interception and height reporting (see below). Scripts are needed for these features.
- **No `allow-same-origin`** — this is the critical flag. Without it the iframe receives an **opaque origin**, which means:
  - It cannot access `window.parent`, `window.top`, or `window.__TAURI_INTERNALS__`
  - It cannot read or write the parent's DOM, cookies, localStorage, or sessionStorage
  - It cannot call Tauri IPC commands
  - It is fully cross-origin isolated from the application
- **`referrerpolicy="no-referrer"`** — prevents leaking the application URL if remote content is loaded in future.

### Content Security Policy (inside the iframe)
The srcdoc document includes a `<meta>` CSP header:
```
default-src 'none';
script-src 'unsafe-inline';
style-src 'unsafe-inline';
img-src https: data:;
```
- **`script-src 'unsafe-inline'`** — required so the postMessage relay script runs. Without this, `default-src 'none'` silently blocks all inline scripts and the link/resize features fail. Safe because the opaque origin prevents any script from reaching the parent or Tauri IPC.
- **No `connect-src`** — inherits `'none'` from `default-src`, blocking fetch/XHR/WebSocket.
- **`img-src https: data:`** — prepared for future "load remote images" opt-in (security0.md item 7). Currently all `<img>` tags are stripped by ammonia before reaching the iframe.
- **`style-src 'unsafe-inline'`** — HTML emails rely heavily on inline CSS for layout.

### Communication: postMessage relay
The iframe cannot touch the parent, but it contains a small inline script that forwards events via `postMessage`:

1. **Link clicks** — intercepts all `<a>` clicks, sends `{ type: 'link-click', href }` to the parent. The parent copies the href to the clipboard and shows a toast (preserving existing behavior).
2. **Auto-resize** — a `ResizeObserver` watches `document.documentElement.scrollHeight` and sends `{ type: 'resize', height }` so the parent can size the iframe to fit the email content.
3. **Context menu** — `contextmenu` is suppressed inside the iframe.

### Sender verification
The parent's `handleIframeMessage()` listener checks `event.source` against the `contentWindow` of each `.email-sandbox` iframe **before** processing any message. Messages from other windows, tabs, or injected frames are silently dropped. This prevents cross-window spoofing of link-click or resize events.

### Threat model after this change
Even if ammonia has a zero-day bypass and an attacker achieves arbitrary HTML/JS execution inside the iframe:
- Cannot call `window.__TAURI_INTERNALS__.invoke()` — opaque origin, no parent access
- Cannot read or modify the parent DOM
- Cannot access stored credentials, cookies, or localStorage
- Cannot create new windows or navigate the parent (`sandbox` blocks these)
- Cannot make network requests — CSP `default-src 'none'` with no `connect-src`
- Can only send `postMessage` to the parent, which validates the sender and only accepts two safe message types (link href string, height number)

### What changed in the code
- **Removed:** `sanitizeHtml()`, `safeHtml()`, `handleLinkClick()` functions
- **Added:** `iframeSrcdoc()` builds a complete HTML document with CSP meta tag, inline styles, email body, and relay script
- **Added:** `handleIframeMessage()` with `event.source` verification, registered via `onMounted`/`onUnmounted`
- **Template:** both `v-html="safeHtml()"` blocks replaced with `<iframe class="email-sandbox" :srcdoc="iframeSrcdoc()" ...>`
- **CSS:** `.body-html` and `.body-html :deep(a)` replaced with `.email-sandbox` (borderless, auto-height)
- ammonia backend sanitization (`parser.rs`) unchanged — remains as defense-in-depth

## Consequences
- HTML emails are fully isolated from Tauri IPC — eliminates the highest-severity attack surface
- Email CSS is contained within the iframe — no bleed in either direction
- Link click → clipboard copy preserved via postMessage relay
- Auto-resize via ResizeObserver avoids double scrollbars
- ammonia remains as belt-and-suspenders (strips dangerous tags before content even reaches the iframe)
- Future "load remote images" (security0.md item 7) benefits from this isolation — images will load inside the sandbox with no application access
