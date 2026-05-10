# Chithi

Chithi is a desktop email, calendar, and contacts client built with
[Tauri 2](https://v2.tauri.app/) (Rust backend) and [Vue 3](https://vuejs.org/)
(TypeScript frontend). It speaks the open protocols you already use —
**IMAP**, **SMTP**, **JMAP**, **CalDAV**, **CardDAV** — and integrates
with **Gmail**, **Microsoft 365**, **Nextcloud Talk**, **Matrix /
Element Call**, and **Zoom** for accounts that have them.

## Project links

- Source code on GitHub: [SUNET/chithi](https://github.com/SUNET/chithi)
- File issues: [github.com/SUNET/chithi/issues](https://github.com/SUNET/chithi/issues)
- The privacy and architecture decisions that shaped what's here live
  under [Architecture decisions](adr/index.md) (one short ADR per choice).

## What's documented here

- **[Features](features.md)** — the user-facing surface today.
- **[Architecture decisions](adr/index.md)** — why each protocol / UX call was
  made, in chronological order.
- **OAuth bounce pages** — small static HTML helpers used by the
  desktop app's OAuth flows (Zoom production callback). Not meant for
  direct browsing.

The site is built with [MkDocs](https://www.mkdocs.org/) and the
[Material](https://squidfunk.github.io/mkdocs-material/) theme; source
lives under `docs/` in the same repository as the app, and changes
land via the same review process.
