# Admin guide

Operational documentation for organizations deploying Chithi or
running their own builds. Like the [user guide](../user/index.md),
this section is being filled in over coming releases.

## Coming soon

- **Building from source** — Tauri / Rust / Node prerequisites and
  the build commands.
- **OAuth Marketplace registrations** — registering Zoom, Gmail, and
  Microsoft 365 apps for an organization, scope choices, redirect
  URI requirements.
- **OAuth bounce hosting** — the static
  [Zoom redirect helper](https://chithi.org/oauth/zoom/) deployed under
  `https://chithi.org/oauth/zoom/`. Forks shipping their own
  Marketplace app point at their own equivalent and override
  `CHITHI_ZOOM_CLIENT_ID` at build time.
- **DNS and TLS** — apex `A` records for GitHub Pages, www CNAME,
  cert provisioning timeline.
- **Update channels** — release cadence, how upgrades land.

For now, the canonical reference is the
[architecture-decisions log](https://github.com/SUNET/chithi/tree/main/docs/adr)
in the source repository — short ADRs covering every major
choice the project has made.
