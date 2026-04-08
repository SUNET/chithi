# ADR 0032: Remote Image Loading via Backend Proxy

## Status
Accepted

## Context
HTML emails frequently contain remote images (`<img src="https://...">`) for logos, formatting, and content. By default, Chithi strips all `<img>` tags via ammonia to block tracking pixels and prevent information leakage. Users need a way to opt into loading images for legitimate emails without compromising the security isolation of the sandboxed email reader (ADR 0026).

The sandboxed iframe uses `sandbox="allow-scripts"` without `allow-same-origin`, giving it an opaque origin. Browsers block network requests from opaque origins, so even if `<img>` tags are preserved in the HTML, the images cannot load. Adding `allow-same-origin` would allow image fetching but also allow scripts to access `window.parent.__TAURI__`, defeating the entire sandbox isolation.

## Decision
The backend acts as an image proxy: it downloads remote images and embeds them as base64 data URIs directly in the HTML. The iframe never makes network requests.

### Flow
1. User clicks **"Load images"** button in the message reader
2. Frontend calls `get_message_html_with_images(accountId, messageId)`
3. Backend re-parses the raw email with ammonia, this time keeping `<img>` tags
4. Backend extracts all `src="https://..."` and `src="http://..."` URLs via regex
5. Backend downloads each image via `reqwest` with safeguards:
   - **HTTPS-only** — ammonia strips `http://` src attributes, regex only matches `https://`
   - **Max 20 images** per message (prevents abuse)
   - **Max 5 MB** per image (prevents memory exhaustion)
   - **10 second timeout** per request
   - **Content-type check** — only `image/*` responses are accepted
6. Backend replaces each `src` URL with `data:{content-type};base64,{data}`
7. Returns self-contained HTML — all images are inline data URIs
8. Frontend sets `imagesHtml` ref, iframe re-renders with embedded images
9. Iframe stays `sandbox="allow-scripts"` — link clicks and auto-resize keep working

### Why not allow-same-origin?
Adding `allow-same-origin` to the iframe sandbox would allow images to load via normal browser fetching. But combined with `allow-scripts` (needed for link relay and auto-resize), it would give any script inside the iframe access to `window.parent.__TAURI__` — full IPC access to credentials, file system, and commands. The backend proxy approach avoids this entirely.

### Security properties
- **Iframe sandbox unchanged** — stays `sandbox="allow-scripts"` with opaque origin, no parent access
- **No network requests from iframe** — all images are inline data URIs
- **Backend validates responses** — only image content types accepted, size-limited
- **Per-message, per-session** — `imagesHtml` resets when switching messages. Reopening the email shows images blocked again. Tracking pixels don't fire on every open.
- **ammonia sanitization** — only `<img>` is restored. `<object>`, `<embed>`, `<iframe>`, `<svg>`, `<script>`, `<form>` etc. remain stripped.
- **HTTPS-only** — enforced at two layers: ammonia strips `<img src="http://...">` (only `https` and `mailto` in URL schemes), and the download regex only matches `src="https://..."`. Plain HTTP image URLs are never fetched or embedded.

### UI
- "Remote content blocked" notice includes a "Load images" button
- Button shows "Loading..." while the backend downloads images
- Notice disappears after images are loaded
- State resets when switching messages

### Files changed
- `src-tauri/src/mail/parser.rs` — `parse_html_with_images()` keeps `<img>` tags through ammonia
- `src-tauri/src/commands/mail.rs` — `get_message_html_with_images` downloads images and embeds as data URIs
- `src/components/mail/MessageReader.vue` — "Load images" button, `imagesHtml` state, conditional notice
- `src/lib/tauri.ts` — `getMessageHtmlWithImages()` typed wrapper

## Consequences
- Users can view images in HTML emails without compromising iframe sandbox isolation
- Image loading is explicit opt-in per message — no tracking pixel risk on normal email reading
- Backend proxy adds latency (~1-2s for typical emails with a few images) but runs in parallel
- Large image-heavy emails (>20 images) will have some images missing — the limit prevents abuse
- Future: per-sender allowlist ("Always load images from this sender") can be added without architectural changes
