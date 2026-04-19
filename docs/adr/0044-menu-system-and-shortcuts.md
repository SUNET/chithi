# ADR 0044: Menu System and Keyboard Shortcuts

## Status

Proposed

## Context

All application menus are custom Vue dropdowns, not Tauri's native
`Menu` API. They have grown organically and are inconsistent today:

- **Main window `File`** has two items: `Settings` (opens the account
  editor, works) and `Close` (only closes the open dropdown, does
  nothing else, a handler-name collision that looks like the menu item
  is broken).
- **Main window `View`** has ten or more items with mixed formatting.
  Some live under a bold group heading (`Message View Position`,
  `Theme`), some sit loose with no grouping (`Show Message Pane`,
  `Threading`, `Message Filters`, `Hide Window Decorations`).
  Capitalization is inconsistent (`Show/Hide Message Pane`, `Threading`
  vs `Message Filters`).
- **Compose window** renders six placeholder `<span>` elements labelled
  `File`, `Edit`, `View`, `Options`, `Tools`, `Help`. None have click
  handlers, dropdowns, or state. Pure decoration.
- **Calendar sidebar** hosts three live settings below the calendar list
  (week start day, time format, display timezone), plus per-calendar
  visibility toggles. These are real app-wide preferences crammed into a
  context where the user expects calendar list management.
- **No Preferences page exists.** The `Settings` route is accounts only.
  Persistent app preferences live partly in the `View` menu, partly in
  the calendar sidebar, with no single home.
- **Keyboard shortcuts are not wired anywhere.** Power users cannot save
  a draft, send, or toggle the message pane from the keyboard.

The structural choice most worth documenting: we stick with Vue
dropdowns rather than adopting Tauri's native `Menu` API. Native menus
integrate with the OS menu bar on macOS and would give us free
accelerator support, but they reset our visual language, cannot be
styled, and are awkward when the app hosts multiple windows (compose,
main, future preferences) that each want their own menu. Vue dropdowns
keep us in charge of the look and let every window own its menus
directly.

## Decision

Adopt a single menu specification across all windows, add a dedicated
Preferences window for app-wide settings, and wire keyboard shortcuts at
the window level.

### Conventions

| Convention             | Meaning                                                                        |
| ---------------------- | ------------------------------------------------------------------------------ |
| Title Case             | All menu labels.                                                               |
| `…` suffix             | Item opens a window, dialog, or prompt. Omit for immediate actions.            |
| `✓` prefix             | Boolean toggle; shown when the toggle is on.                                   |
| `●` prefix             | Radio selection inside a mutually exclusive group; shown on the active option. |
| Bold group heading     | Non-clickable heading that introduces a mutually exclusive group.              |
| `────` separator       | Divides logically distinct sections within a menu.                             |
| Right-aligned shortcut | Accelerator label, rendered dim. Wired at runtime.                             |

Platform modifier: `Ctrl` on Linux and Windows, `Cmd` on macOS. Resolved
at runtime; the ADR and source strings write `Ctrl+X` throughout, the
renderer substitutes `⌘X` on macOS.

### Main window

#### File

```
Preferences…                Ctrl+,
────────────────────────────
Close Window                Ctrl+W
Quit                        Ctrl+Q
```

Account management is no longer a top-level File item, and it is not a
Preferences section either. It stays accessible from its own button in
the left pane, following Thunderbird's pattern.

#### View

```
Show Message Pane ✓         Ctrl+\
Message Pane Position
    ● Right
    ● Bottom
    ● Tabs
────────────────────────────
Threaded View ✓             Ctrl+T
Message Filters…
────────────────────────────
Hide Title Bar ✓
```

Renames:

- `Show/Hide Message Pane` becomes `Show Message Pane`. The checkmark
  communicates state; the label names the thing, not the verb.
- `Threading` becomes `Threaded View`, parallel to `Show Message Pane`.
- `Hide Window Decorations` becomes `Hide Title Bar`. That is what the
  toggle actually does; calling it "decorations" hides the intent behind
  jargon.
- Theme moves out of the View menu and into Preferences > General. Theme
  is a persistent preference, not a transient view state.

#### Help

Deferred. Can be added in a later phase if we need `About Chithi…`,
`Keyboard Shortcuts…`, `Report Issue…`. Not blocking the refactor.

### Preferences window

New window, opened from `File > Preferences…` or `Ctrl+,`. Left sidebar
of sections, right-hand detail panel. Reuses existing Vue components
where it can.

- **General**: Theme (● System / ● Light / ● Dark).
- **Mail**: default threaded view, default message pane position.
- **Calendar**: week starts on, time format, display timezone. Moved out
  of the calendar sidebar, which thereafter hosts only the calendar list
  and its per-calendar toggles.

Persistence stays in `localStorage` for the settings that already live
there. A later ADR can move to `tauri-plugin-store` for file-backed
config if that becomes worth it.

### Compose window

#### File

```
Save Draft                  Ctrl+S
Send                        Ctrl+Return
────────────────────────────
Close Window                Ctrl+W
```

#### Edit

```
Undo                        Ctrl+Z
Redo                        Ctrl+Shift+Z
────────────────────────────
Cut                         Ctrl+X
Copy                        Ctrl+C
Paste                       Ctrl+V
────────────────────────────
Select All                  Ctrl+A
```

Edit items dispatch to `document.execCommand` for the standard clipboard
and history operations. The keyboard shortcuts match the OS defaults, so
the menu is effectively documentation; the shortcuts work whether or not
the user opens the menu.

#### View

```
Show Cc ✓
Show Bcc ✓
```

#### Options

```
Attach File…                Ctrl+Shift+A
```

#### Help

```
About Chithi…
```

**Dropped**: Compose > Tools. Nothing in the app belongs there.

### Keyboard shortcut implementation

- Each window attaches a single `keydown` listener on mount and removes
  it on unmount. No document-level global listeners that outlive the
  window.
- Shortcuts are expressed as a table mapping `{ key, ctrl, shift, alt }`
  to a handler. The same handler is invoked by both the menu click and
  the keystroke, so menu-item and shortcut behaviour cannot drift.
- Platform modifier normalisation lives in one helper. Labels render
  `Ctrl+X` on Linux/Windows and `⌘X` on macOS; event handlers match
  `event.metaKey` on macOS, `event.ctrlKey` elsewhere.
- Unknown shortcuts fall through to the browser default, so platform
  text-editing shortcuts in the compose textarea keep working.

## Consequences

- A new `PreferencesView.vue` component and `/preferences` route ship.
- The calendar sidebar loses three settings blocks. It becomes a purer
  calendar-list view.
- The `MenuBar.vue` component grows a shortcut registry and the matching
  keydown listener. Menu item click handlers and shortcut handlers
  collapse to a single dispatch table.
- The compose window gains a functional menu bar. The placeholder
  `<span>` rendering is replaced.
- `Hide Window Decorations` renames to `Hide Title Bar` in user-visible
  strings. The underlying `uiStore.decorationsEnabled` key is unchanged
  so stored state survives.
- Theme moves out of the View menu. Users who changed theme from there
  will find it under Preferences > General instead.
- Shortcuts are display-only on first render until the keydown listener
  wires up. We ship the listener and the display together; no
  half-state.

## Alternatives considered

- **Native Tauri menus via `Menu::new()`.** Gives us the macOS menu bar,
  free accelerators, and OS integration. Costs: we cannot style them to
  match the rest of the UI, cross-window state (which compose menu
  toggles are on?) is harder to share, and the visual language between
  the top menu and in-app dropdowns would diverge. Rejected because the
  app is a single-user desktop client, not a polished macOS-native
  product; consistency matters more than OS integration.
- **Keep the current structure and fix only the broken items.** Cheapest
  path. Rejected because the inconsistency keeps compounding as we add
  menu items; "consistent language" was a user-level requirement, not a
  cosmetic one.
- **Move every persistent setting into Preferences, including the
  message pane layout toggles.** Cleanest by convention. Rejected
  because the layout toggles are genuinely view state that the user
  wants to flip quickly; hiding them two windows deep would hurt daily
  use.

## Rollout

Three PRs, each self-contained.

1. Main window menu cleanup plus functional `File > Close Window` and
   `Quit`. Normalised View menu. Shortcut table for main window.
2. Preferences window. Moves calendar-sidebar settings and theme.
3. Compose window menus and shortcuts. Drops the placeholder Tools
   entry.

Each phase lands as its own PR with the ADR as its north star.
