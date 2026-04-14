# ADR 0042: Calendar Timezone Support

## Status
Proposed

## Context

All calendar event times are displayed in UTC regardless of the user's local timezone. The root cause is multi-layered:

1. **Graph API sync** (`graph.rs:797-825`) — When Microsoft returns `{dateTime: "2026-04-14T14:00:00", timeZone: "Europe/Stockholm"}`, the code only handles `tz == "UTC"`. For any other timezone, it stores the bare local datetime as if it were UTC.

2. **iCal parser** (`ical.rs:305-379`) — Correctly extracts `TZID` from `DTSTART;TZID=Europe/Stockholm`, but `ical_datetime_to_iso()` accepts `_tzid` (unused parameter). The timezone is discarded.

3. **JMAP sync** (`jmap.rs:893-900`) — Blindly appends `Z` to whatever time string JSCalendar returns, assuming everything is UTC.

4. **All sync paths** (`calendar.rs:748, 1040, 2603`) — Every `CalendarEvent` construction hardcodes `timezone: None`, even when timezone data is available from the source.

5. **Frontend** (`EventForm.vue:119`, `EventDetail.vue:135`) — Always sends `timezone: null` when creating/editing events.

The database schema already has a `timezone TEXT` column on `calendar_events` — it's just never populated.

### Approaches considered

**A) Normalize to UTC on ingest, display in user-chosen timezone** — Every sync path converts incoming times to UTC before storing. A global `display_timezone` setting controls how the frontend renders. The `timezone` column stores the original source timezone for round-trip fidelity (replies, updates).

**B) Store original timezone times, convert on display** — Minimal backend changes, store times as-is from each provider. Frontend handles mixed formats (UTC, offset, bare local times). Makes sorting/filtering complex and storage format inconsistent.

**C) Convert to UTC on ingest, no display setting** — Same as A but rely on `new Date().toLocaleString()` to auto-convert to the browser's locale timezone. No user control over display timezone.

## Decision

**Approach A: Normalize to UTC on ingest, display in user-chosen timezone.**

### Crate choices

- **`chrono-tz`** over `jiff` — the project already uses `chrono` throughout; `chrono-tz` is its natural IANA timezone companion. Adding `jiff` would introduce a parallel datetime library for no benefit.
- **`iana_time_zone`** — single-purpose crate to read the OS timezone. Used to provide a sensible default so the calendar works correctly from first launch without configuration.

### Timezone list source

The frontend timezone dropdown fetches the list from the Rust backend (`chrono_tz::TZ_VARIANTS`) rather than using the browser's `Intl.supportedValuesOf('timeZone')`. This guarantees frontend and backend agree on what constitutes a valid timezone identifier.

### Default timezone

Auto-detected from the OS via `iana_time_zone::get_timezone()` on first launch. Stored in `localStorage` once the user sees it. Falls back to `"UTC"` if detection fails.

### Display timezone setting

A single global setting in the calendar sidebar (dropdown below "Week starts on", with type-to-search). No per-calendar timezone override — one timezone for the whole app, matching the existing `weekStartDay` pattern.

### Backend normalization

A `to_utc(datetime, tzid)` utility converts naive datetimes to UTC:
- If the datetime already has `Z` or an offset (`+02:00`), parse and convert directly.
- If a valid IANA `tzid` is provided, interpret the naive datetime in that zone and convert.
- If `tzid` is invalid or missing, treat as UTC (safe fallback, matches current behavior).

Each sync path is fixed to use this utility:

| Sync path | Current behavior | Fix |
|-----------|-----------------|-----|
| Google | Takes `dateTime` as-is (usually has offset) | Parse offset → UTC. Store tz from `start.timeZone` |
| Graph | Ignores `timeZone` for non-UTC | `to_utc(dt, tz)`. Store original tz |
| iCal/CalDAV | `TZID` extracted but unused | Pass to `to_utc()`. Store tz |
| JMAP | Blindly appends Z | Check `timeZone` property, use `to_utc()` |
| Email invites | `timezone: None` | Use `ParsedInvite.timezone` |

### Round-trip fidelity

The `timezone` column stores the original source timezone (e.g. `Europe/Stockholm`). When generating iCalendar for replies, updates, or CalDAV writes, this timezone is used to emit `DTSTART;TZID=...` instead of bare UTC — preserving the organizer's intended timezone.

### Frontend display

All time display uses `Intl.DateTimeFormat` with the `timeZone` option set to the user's chosen display timezone. Event creation/editing interprets user input in the display timezone and converts to UTC before sending to the backend.

## Consequences

- Two new crate dependencies: `chrono-tz` (compile-time IANA database, increases build time) and `iana_time_zone` (minimal).
- All four sync paths (Google, Graph, CalDAV/iCal, JMAP) must be updated.
- Events already stored without timezone info will continue to display as UTC — a one-time re-sync corrects them.
- The `timezone` column transitions from always-NULL to populated, enabling future features like per-event timezone display.
