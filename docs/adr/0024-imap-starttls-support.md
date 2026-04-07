# ADR 0024: IMAP STARTTLS Support

## Status
Accepted

## Context
The IMAP connection always used implicit TLS via `imap::connect()`, which wraps the entire TCP connection in TLS from the start. This works for port 993 (the standard IMAPS port) but fails for servers that use STARTTLS on port 143, such as `mail.sunet.se`. The error manifested as "SSL routines: packet length too long" because the client tried to negotiate TLS on a plaintext connection.

The `use_tls` field existed on `ImapConfig` but was never used.

## Decision
Use the port number to determine the TLS mode:

- **Port 993** → `imap::connect()` — implicit TLS, entire connection encrypted from the start
- **Any other port (including 143)** → `imap::connect_starttls()` — connects plain, sends the IMAP `STARTTLS` command, then upgrades to TLS before any credentials are transmitted

Both paths produce a fully encrypted `TlsStream`. No plaintext authentication is ever performed. The `imap` crate's `connect_starttls` handles the STARTTLS command and TLS upgrade in a single call.

Insecure (unencrypted) connections are not supported. There is no code path that sends credentials over a plaintext connection.

## Consequences
- Servers using STARTTLS on port 143 (Dovecot, Cyrus, university mail servers, etc.) now work
- Gmail and other port-993 servers continue to work unchanged
- No configuration change needed — the port number determines the behavior automatically
- The `use_tls` field on `ImapConfig` is preserved but not used for mode selection (port is authoritative)
