# ADR 0027: Content Security Policy

## Status
Accepted

## Context
The application had `"csp": null` in `tauri.conf.json`, completely disabling Content Security Policy for the main webview. Any XSS vector — even outside the email rendering path — had zero browser-level mitigation. This was item 2 in the pre-public-preview security audit (security0.md).

## Decision
Define a strict CSP in `tauri.conf.json`. Tauri v2 automatically merges its own requirements (nonces for injected scripts, IPC protocol handlers) into the provided policy.

### Policy
```
default-src 'self';
script-src 'self';
style-src 'self' 'unsafe-inline';
img-src 'self' data: blob: asset:;
font-src 'self';
connect-src ipc: http://ipc.localhost;
object-src 'none';
base-uri 'none';
form-action 'none'
```

### Directive rationale

| Directive | Value | Why |
|-----------|-------|-----|
| `default-src` | `'self'` | Baseline: only load resources from the app origin |
| `script-src` | `'self'` | Only bundled Vite scripts. Tauri adds nonces for its own injected scripts automatically |
| `style-src` | `'self' 'unsafe-inline'` | Vue scoped styles inject `<style>` tags at runtime; inline `:style` bindings used in 10+ components |
| `img-src` | `'self' data: blob: asset:` | Bundled assets, inline SVG data URIs, Tauri `asset:` protocol. No external images |
| `font-src` | `'self'` | Inter and Liberation Mono fonts bundled as woff2/ttf in `src/assets/fonts/` |
| `connect-src` | `ipc: http://ipc.localhost` | Tauri IPC only. The frontend makes zero direct HTTP requests — all API calls go through Tauri commands in the Rust backend |
| `object-src` | `'none'` | Blocks `<object>`, `<embed>`, `<applet>` — no plugins |
| `base-uri` | `'none'` | Prevents `<base>` tag injection that could redirect relative URLs |
| `form-action` | `'none'` | Prevents form submissions to external URLs |

### What this blocks
- Inline scripts (XSS payloads via `<script>` injection)
- External script loading (`<script src="https://evil.com">`)
- `eval()`, `new Function()`, `setTimeout("string")` (blocked by `script-src 'self'`)
- External image/font/media loading (tracking pixels, font fingerprinting)
- External fetch/XHR/WebSocket from the renderer
- Plugin embeds, form submissions, base URI hijacking

### Relationship to the sandboxed email iframe (ADR 0026)
This CSP protects the **main application webview**. The sandboxed email iframe has its own stricter CSP set via `<meta>` tag:
```
default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src https: data:;
```
The iframe's `script-src 'unsafe-inline'` is safe because `sandbox="allow-scripts"` without `allow-same-origin` gives it an opaque origin with no parent access. The two policies are independent layers of defense.

## Consequences
- XSS payloads that bypass sanitization are now blocked at the browser level
- No external resource loading from the renderer — reduces attack surface and prevents data exfiltration
- `'unsafe-inline'` for styles is a known trade-off required by Vue's runtime style injection; acceptable given that style injection alone cannot execute JavaScript in modern browsers
- Future changes that need external resources (e.g., remote image loading) must go through the Rust backend, not direct renderer fetches
