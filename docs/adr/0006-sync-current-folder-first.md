# ADR 0006: Sync the current folder first

## Status

Accepted

## Date

2026-04-04

## Context

When syncing an account with many folders (Gmail accounts typically have 10+), the user has to wait for all folders to sync before seeing new messages in the folder they're currently viewing. If the user is looking at INBOX but the sync processes Drafts, Sent Mail, Spam, Trash, All Mail, Important, and Starred first, there's a noticeable delay before the folder they care about updates.

## Decision

The `trigger_sync` command accepts an optional `current_folder` parameter indicating which folder the user is currently viewing. The sync engine orders folders as:

1. **Current folder** (if specified) — the folder the user is actively looking at
2. **INBOX** (if not already the current folder) — the most important folder
3. **All other folders** — synced in whatever order the IMAP server returns them

The frontend passes `foldersStore.activeFolderPath` when triggering sync from the Sync button, periodic sync, or any other sync trigger.

## Consequences

- New messages in the viewed folder appear within seconds of clicking Sync, regardless of how many other folders exist
- INBOX is always second priority so important mail is never delayed behind low-priority folders
- No change to total sync time — all folders are still synced, just in a different order
- The `current_folder` parameter is optional — callers that don't have a folder context (e.g., initial sync after account creation) pass None and get the default INBOX-first ordering
