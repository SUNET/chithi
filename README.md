# Chithi

A desktop email client built with Tauri v2 (Rust) and Vue 3 (TypeScript).

Supports IMAP, JMAP, CalDAV, and JMAP Calendar with a Thunderbird-style three-pane layout. Passwords are stored in the system keyring (GNOME Keyring, KDE Wallet, macOS Keychain, or Windows Credential Manager).

## Features

- Multi-account support (Gmail app password, IMAP, JMAP, CalDAV-only)
- Email threading with In-Reply-To and subject-based fallback
- Calendar with day/week/month views, recurring events, and meeting invite handling
- Accept/Maybe/Decline meeting invites from email with iTIP replies
- Client-side message filtering rules
- HTML email sanitization (no scripts, no remote content by default)
- Dark and light themes

## Prerequisites

- [Rust](https://rustup.rs/) (stable toolchain)
- [Node.js](https://nodejs.org/) (v20+)
- [pnpm](https://pnpm.io/) (v10+)

## System Dependencies

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
# Clone the repository
git clone <repo-url>
cd emails

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

| Data | Location |
|------|----------|
| Email bodies (Maildir) | `~/.local/share/chithi/<account_id>/` |
| SQLite database | `~/.local/share/chithi/chithi.db` |
| Log file | `~/.local/share/chithi/chithi.log` |
| Passwords | System keyring (GNOME Keyring / KDE Wallet / macOS Keychain) |

## Architecture

- **Frontend**: Vue 3 + TypeScript + Pinia (in `src/`)
- **Backend**: Rust + Tauri v2 (in `src-tauri/`)
- **Mail**: IMAP via `imap` crate, JMAP via raw `reqwest` HTTP
- **Sending**: SMTP via `lettre` (IMAP accounts), JMAP Submission (JMAP accounts)
- **Calendar**: JMAP Calendar + CalDAV via `reqwest` + `uppsala` XML parser
- **Storage**: Maildir on disk + SQLite index, passwords in OS keyring

See `docs/adr/` for Architecture Decision Records.

## License

GPL-3.0-or-later
