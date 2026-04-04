# ADR 0010: Account type selection — Gmail, IMAP, or JMAP

## Status

Accepted

## Date

2026-04-04

## Context

The application supports multiple email protocols (IMAP and JMAP) and provider-specific configurations (Gmail app passwords). Users need a clear way to configure accounts for each type without being overwhelmed by irrelevant fields.

## Decision

The account setup form presents three account types as tab buttons:

- **Gmail**: Email + app password only. IMAP/SMTP hosts auto-filled and shown as disabled (`imap.gmail.com:993`, `smtp.gmail.com:587`). Links to Google's app password page. Protocol is `imap`.

- **IMAP**: Full manual configuration — IMAP host/port, SMTP host/port, username, password, TLS toggle. For generic IMAP servers. Protocol is `imap`.

- **JMAP**: Email + password + optional JMAP URL. If the URL is left blank, auto-discovery tries `https://<domain>`, `https://mail.<domain>`, `https://jmap.<domain>` for `.well-known/jmap`. IMAP/SMTP fields are hidden. Protocol is `jmap`.

The `accounts` table stores `mail_protocol` (`"imap"` or `"jmap"`) and `jmap_url` (empty for auto-discovery). A database migration (`ALTER TABLE ADD COLUMN jmap_url`) handles existing databases created before JMAP support was added.

Account type cannot be changed after creation (the tab selector is disabled when editing) since switching protocol would invalidate synced data. Users should delete and recreate the account if they need to change protocol.

## Consequences

- Users see only the relevant fields for their account type — no confusion about SMTP settings for JMAP accounts.
- The backend dispatches sync, send, and body-fetch operations based on `mail_protocol`, keeping the frontend protocol-agnostic.
- Edit functionality lets users update credentials and server settings without recreating the account.
- The migration system (`app_metadata` table + `run_migrations()` in schema init) supports adding columns to existing databases.
