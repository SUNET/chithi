# ADR 0043: Light Theme — Warm Amber Redesign

## Status
Accepted

## Context

The original light theme was a mechanical inversion of the dark theme: cool-grey neutrals (`#fafafa` ground, `#155dfc` blue accent) lifted from Tailwind's `neutral-*` palette to mirror the dark mode's contrast pattern. It worked but felt like an afterthought — high-density spreadsheet UI on a dim Office-blue accent.

A separate redesign (`PATCHES.md`, 12 numbered sections) proposed a warm off-white ground (`#faf7f2`) with a deep-amber accent (`#b54708`), filled per-type folder glyphs in distinct warm hues, and per-account colored avatars/edges. The goal: turn the light theme into a deliberate, calm, paper-like surface — distinct from the dark theme rather than its inverse.

Two sub-decisions inside the redesign were non-trivial:

1. **Per-account colors.** PATCHES.md proposed mapping `provider in {gmail, outlook, jmap}` → fixed hex. This breaks for users with multiple accounts on the same provider (two Gmails would collide) and assumes the hard-coded provider set will never grow.
2. **Active folder color.** Previously the entire folder row turned blue when active (icon + text + bg tint). The redesign keeps each folder glyph in its own type hue at all times, so the eye can locate "Trash" or "Drafts" without parsing text.

## Decision

**Adopt the warm-amber palette as a deliberate, light-only redesign.** The dark theme is unchanged.

### Token architecture

All colors live in `src/assets/styles/main.css` as CSS custom properties on `:root, [data-theme="light"]`. Components use `var(--color-*)` exclusively — no inline hex. New tokens introduced by this redesign:

- `--color-bg-elevated` (white card / reader pane against warm ground)
- `--color-border-heavy`, `--color-divider` (two weights of warm border)
- `--color-star-flag` (warm orange star, distinct from amber accent)
- `--color-sync-green` (`#6ca04f`, replaces the saturated `#00c950` "online" green which clashes with the warm palette)
- `--shadow-sm`, `--shadow-mdl`, `--radius` (shared elevation/corner tokens)

### Per-account color: UID hash, not provider

Account avatars (FolderTree), contact circles (ContactsView), settings card edge (SettingsView), and From-field swatch (ComposeView) all derive their color from a stable hash of the account UID, not from `provider`/`mail_protocol`.

A single helper `acctColor(accountId)` in `src/lib/account-colors.ts` computes `FNV-1a(accountId) mod 7` and returns `{ fill: var(--acct-N), soft: var(--acct-N-soft) }` referencing one of seven warm-palette entries (`--acct-1` … `--acct-7`) defined in `main.css`. Two Gmail accounts get distinct colors; the assignment is consistent across sessions because the hash is deterministic.

### Folder glyphs keep their hue

The `.folder-item.active .folder-svg { color: var(--color-accent); }` rule is removed. Active state is now communicated by row background + text weight only. Each folder type keeps its own hue (Inbox amber, Drafts ochre, Sent olive, Spam vermillion, Trash terracotta, Archive sienna, Outbox rose, Starred orange, user folders neutral warm-grey).

### Selection: warm bg + inset accent bar

Selected message/thread/contact rows use `background: var(--color-bg-active); box-shadow: inset 3px 0 0 var(--color-accent);` instead of the previous translucent blue overlay (`#3b82f633`). The 3 px amber bar gives a clear visual anchor against the warm ground.

## Consequences

- **Light + dark themes diverge intentionally.** The dark theme remains the cool-grey, blue-accent default; the light theme is now its own thing rather than a mirror. Tokens with the same name resolve to different palette spaces in each theme — components must use tokens, never hex.
- **`acctColor()` is the only blessed way to color an account.** Any new feature that surfaces a per-account visual cue must use it. Adding palette entries beyond 7 means adding `--acct-8` etc. to `main.css` and bumping `PALETTE_SIZE` in the helper.
- **Server-side calendar color seeding is out of scope.** PATCHES.md §7 included a table of default colors keyed by calendar email. `cal.color` is per-calendar server data; populating defaults requires a backend migration and was deferred. WeekView already renders each event in its `calendar.color`.
- **Dependency-free.** No new crates, no new npm packages — the redesign is pure CSS + a 30-line hash helper.
- **Density and layout unchanged.** Row heights, pane widths, sidebar width all stayed the same. This is a color/elevation pass, not a layout pass.

## See also

- `PATCHES.md` — the full patch-level handoff this ADR implements
- `src/lib/account-colors.ts` — UID-hash helper
- `src/assets/styles/main.css` — light + dark theme tokens
