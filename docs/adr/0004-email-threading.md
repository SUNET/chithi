# ADR 0004: Email threading strategy

## Status

Accepted

## Date

2026-04-04

## Context

Email threads (conversations) need to be grouped so that related messages appear together. Different email systems use different threading strategies:

- **RFC 5256 (IMAP THREAD)**: Server-side threading, not universally supported.
- **JWZ algorithm**: Complex reference-chain threading used by Thunderbird and Mustrstrstrstrstr. Walks the `References` header chain to build a tree.
- **Gmail conversations**: Groups by subject, which is simple but can over-thread notification emails.

We needed a threading approach that works with envelope-only sync (no full headers downloaded upfront), handles Gmail's folder-as-label model, and doesn't falsely group unrelated automated emails.

## Decision

### Threading algorithm

Thread IDs are computed during sync and stored in the `thread_id` column of the `messages` table. The computation uses a three-step strategy:

1. **In-Reply-To lookup**: If the message has an `In-Reply-To` header, find the referenced `Message-ID` in the database and reuse its `thread_id`. This is the most reliable signal — it directly identifies the parent message.

2. **Reverse lookup**: Check if any existing message has its `In-Reply-To` pointing to our `Message-ID`. If so, we're the parent of an existing thread and should join it.

3. **Subject-based fallback, replies only**: If the subject starts with `Re:`, `Fwd:`, or `FW:`, strip the prefix and search for an existing thread with the matching base subject. This catches cases where `In-Reply-To` doesn't match (different clients, Gmail conversations). **Crucially, this only applies to explicit replies** — messages without a reply/forward prefix are never subject-matched. This prevents automated emails ("OSSEC Notification", "Daily report") from being falsely grouped.

4. **New thread root**: If no existing thread is found, use the message's own `Message-ID` as a new `thread_id`.

### Per-folder threading

The threaded message list query groups messages **within the current folder only**. This keeps the query fast (simple GROUP BY on an indexed column, no cross-folder JOINs) and avoids Gmail's duplicate-label problem where the same physical email appears in INBOX, All Mail, Sent Mail, and Important simultaneously.

When the user expands a thread, child messages are fetched from the **same folder** — not cross-folder — to avoid showing duplicates.

### Backfill

Existing messages synced before threading was implemented have empty `thread_id`. A one-time backfill runs on first app start per account, processing messages in date-ascending order (so parents get threaded before replies). The backfill is wrapped in a SQLite transaction for performance and its completion is recorded in the `app_metadata` table to avoid re-running.

### Empty string vs NULL

The `thread_id` column uses empty string `''` rather than NULL for "no thread" in many cases (legacy from initial sync). All queries treat both as equivalent: `thread_id IS NULL OR thread_id = ''`. The grouping expression uses `CASE WHEN thread_id IS NOT NULL AND thread_id != '' THEN thread_id ELSE id END`.

### UI

- **Toggle**: Threading can be enabled/disabled globally via View menu, persisted to localStorage. Switching triggers a full re-fetch.
- **Thread rows**: Show an expand/collapse chevron, message count badge, and the latest message's sender and date. Unread count shown via blue dot.
- **Expand**: Clicking the chevron loads child messages from the current folder.
- **Right-click**: "Remove from Thread" sets a message's `thread_id` to its own `message_id`, breaking it out. "Show as Thread" (flat mode) finds and displays the thread for a message.
- **Mark as read**: Clicking a threaded message updates both the message's flags and the parent thread's unread count for instant UI feedback, then syncs the `\Seen` flag to IMAP in the background.
- **Reply icon**: Shows ↩ before the subject when the message has the "answered" flag OR when the subject starts with Re:/Fwd:/FW:.

## Consequences

- Threading works reliably for human conversations (In-Reply-To matching) and catches Gmail-style conversations (subject matching for replies).
- Automated/notification emails with repeated subjects are not falsely grouped, since subject matching is restricted to messages with Re:/Fwd: prefixes.
- Per-folder threading avoids the performance cliff of cross-folder JOINs on large mailboxes (26K+ messages) and Gmail's label-duplication issue.
- The backfill is a one-time cost; subsequent messages are threaded during sync with negligible overhead.
- Users who don't want threading can disable it globally — the flat view is always available.
