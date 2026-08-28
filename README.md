# Chithi

A desktop email, calendar, and contacts client built with Tauri v2
(Rust) and Vue 3 (TypeScript).

Supports IMAP, SMTP, JMAP, CalDAV, and CardDAV with a
Thunderbird-style three-pane layout. On desktop, passwords are stored
in the system keyring (Secret Service, macOS Keychain, or Windows
Credential Manager).

## Why?

Need a client with first class support for OpenPGP. Also allowing doing Calendar in the application without a plugin :)

## Features

- Multi-account support for Gmail, Microsoft 365, Fastmail, IMAP,
  JMAP, CalDAV, and CardDAV
- Email threading with In-Reply-To and subject-based fallback
- Calendar with day/week/month views, recurring events, and meeting invite handling
- Accept/Maybe/Decline meeting invites from email with iTIP replies
- Client-side message filtering rules
- HTML email sanitization (no scripts, no remote content by default)
- OpenPGP signing and encryption, including smartcard support
- Nextcloud Talk, Matrix / Element Call, Zoom, and La Suite Visio meeting integration
- Dark and light themes

### Zoom OAuth scopes

Chithi requests these four Zoom scopes for its user-managed meeting
integration:

- `meeting:write:meeting`
- `meeting:update:meeting`
- `meeting:delete:meeting`
- `user:read:user`

See the complete [Zoom end-user guide](web/user/zoom.md) for setup, usage,
troubleshooting, and removal. The [Zoom Marketplace test
plan](web/zoom-test-plan.md) maps each scope to its user action and REST
endpoint.

## Prerequisites

- [Rust](https://rustup.rs/) (stable toolchain)
- [Node.js](https://nodejs.org/) (v20.19.x or v22.12+)
- [pnpm](https://pnpm.io/) (v10; the repository pins the exact version)

## System Dependencies

### Arch
```bash
  sudo pacman -S --needed \
    base-devel \
    openssl \
    dbus \
    gtk3 \
    webkit2gtk-4.1 \
    libayatana-appindicator \
    pcsclite \
    librsvg \
    curl \
    wget
```

### Debian / Ubuntu

```bash
sudo apt update
sudo apt install -y \
  build-essential \
  pkg-config \
  libssl-dev \
  libdbus-1-dev \
  libgtk-3-dev \
  libwebkit2gtk-4.1-dev \
  libayatana-appindicator3-dev \
  libpcsclite-dev \
  librsvg2-dev \
  curl \
  wget
```

### Fedora

```bash
sudo dnf install -y \
  gcc gcc-c++ make \
  pkg-config \
  openssl-devel \
  dbus-devel \
  gtk3-devel \
  webkit2gtk4.1-devel \
  libappindicator-gtk3-devel \
  librsvg2-devel \
  curl \
  cargo \
  pnpm \
  pcsc-lite-devel \
  wget
```

### macOS

```bash
xcode-select --install
brew install openssl
```

Tauri uses the built-in WebKit framework on macOS. The `keyring` crate uses the native Keychain — no extra dependencies needed.

## Build & Run

```bash
git clone https://github.com/SUNET/chithi.git
cd chithi

# Install frontend dependencies
pnpm install

# Run in development mode (hot-reload frontend + Rust backend)
pnpm tauri dev

# Build a release binary
pnpm tauri build
```

The release binary will be in `src-tauri/target/release/`.

## Running Tests

```bash
# Frontend tests (Vitest)
pnpm test

# Rust backend tests
cd src-tauri && cargo test

# Type-check frontend
pnpm exec vue-tsc --noEmit
```

## Data Storage

Desktop application data is stored below the platform's local data
directory:

| Platform | Base directory |
|----------|----------------|
| Linux | `${XDG_DATA_HOME:-$HOME/.local/share}/chithi/` |
| macOS | `~/Library/Application Support/chithi/` |
| Windows | `%LOCALAPPDATA%\chithi\` |

Linux honors `XDG_DATA_HOME`. The exact macOS and Windows locations
should be verified with packaged builds. Within the base directory,
`chithi.db` is the SQLite database,
`chithi.log` is the log, and each mail account has its own directory.
Desktop passwords and OAuth tokens are stored separately in the system
keyring. See the [data-removal guide](web/user/data-removal.md) before
deleting data manually.

## Architecture

- **Frontend**: Vue 3 + TypeScript + Pinia (in `src/`)
- **Backend**: Rust + Tauri v2 (in `src-tauri/`)
- **Mail**: IMAP via `imap` crate, JMAP via raw `reqwest` HTTP
- **Sending**: SMTP via `lettre` (IMAP accounts), JMAP Submission (JMAP accounts)
- **Calendar**: JMAP Calendar + CalDAV via `reqwest` + `uppsala` XML parser
- **Storage**: Maildir on disk + SQLite index, passwords in OS keyring

See `docs/adr/` for Architecture Decision Records.

## To enable usage in your work/school O365

Ask the admin to allow the application for permissions.

```
App details:
- Name: chithi
- Type: Public desktop client
- Auth flow: OAuth 2.0 Authorization Code with PKCE
- Redirect URI: http://localhost

- Client Application ID: b5941cd4-0385-40f1-953a-2c3b36f2a331

Access model:
- Delegated permissions only
- Access is limited to the signed-in user’s own mailbox, calendars, shared calendars, and contacts
- No client secret is stored on the device
- On desktop, OAuth tokens are stored locally in the OS keyring

Requested permissions:

- Microsoft Graph: `User.Read`, `Mail.ReadWrite`,
  `Calendars.ReadWrite`, `Contacts.ReadWrite`, `Place.Read.All`
- Outlook: `IMAP.AccessAsUser.All`, `SMTP.Send`
- Sign-in: `offline_access`, `openid`, `profile`, `email`
```



## License

GPL-3.0-or-later
