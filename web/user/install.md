# Install Chithi

Chithi is currently a pre-1.0 project. Check the
[GitHub releases page](https://github.com/SUNET/chithi/releases) for a
package built for your platform. The repository does not currently have
an automated workflow that publishes desktop installers, so a suitable
prebuilt package may not be available.

## Platform status

| Platform | Status |
| --- | --- |
| Linux | Primary development and CI platform; DEB, RPM, Arch, and AppImage packaging recipes exist. |
| macOS | Tauri and Keychain support exist, but no macOS release workflow or documented tested version exists. |
| Windows | Tauri and Windows Credential Manager support exist, but no Windows release workflow or documented tested version exists. |
| Android and iOS | Development work exists; do not treat it as a supported production release. |

Packaging support in the source tree is not the same as a tested release.
If you depend on a particular operating system, verify it before moving
important accounts into Chithi.

## Build from source

You need:

- Rust's stable toolchain
- Node.js 20.19.x, or 22.12 or newer
- pnpm 10 (the repository pins the exact version)
- Tauri's platform development dependencies
- PC/SC development libraries, because OpenPGP smartcard support is
  compiled in

Clone and start Chithi:

```bash
git clone https://github.com/SUNET/chithi.git
cd chithi
pnpm install
pnpm tauri dev
```

Build release packages for the current platform with:

```bash
pnpm tauri build
```

The complete Linux dependency lists are maintained in the
[repository README](https://github.com/SUNET/chithi#system-dependencies).
Tauri also documents its
[platform prerequisites](https://v2.tauri.app/start/prerequisites/).

!!! note
    `pnpm tauri build` creates only the bundle types supported by the
    machine on which it runs. It does not create packages for every
    operating system from one build host.

## Updating

Chithi has no automatic updater. Install a newer package using the same
method you used for the current version. Back up the application-data
directory before testing an upgrade or downgrade.

Continue with [First launch](getting-started.md).
