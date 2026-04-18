# Security Policy

## Reporting a vulnerability

Please **do not** open public GitHub issues for security vulnerabilities.

Instead, email the Chithi maintainers directly:

- Kushal Das, <kushal@sunet.se>
- Micke Nordin, <kano@sunet.se>

Include, where possible:

- A description of the issue and its impact.
- Steps to reproduce (proof-of-concept code or a minimal test case if you have one).
- The affected version or commit.
- Any suggested mitigations.

We aim to acknowledge reports within a few working days and to provide a first
assessment within two weeks. Once a fix is available, we will coordinate
disclosure with you.

## Scope

Chithi is a desktop mail and calendar client built on Tauri. Reports in scope include:

- Memory-safety or logic bugs in the Rust backend (`src-tauri/`).
- Handling of credentials, tokens, or user data in the frontend or storage layer.
- Issues in the handling of untrusted network input (IMAP/SMTP/JMAP/CalDAV/CardDAV responses, iCalendar/vCard payloads, HTML mail).
- Insecure defaults in built releases.

Vulnerabilities in upstream dependencies should normally be reported to those
projects directly; if a dependency CVE materially affects Chithi users, we
still appreciate a heads-up.

## Supported versions

Chithi is currently pre-1.0. Only the `main` branch receives security fixes.
