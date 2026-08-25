<script setup lang="ts">
import { ref, onBeforeUnmount, onMounted } from "vue";
import { useCalendarStore } from "@/stores/calendar";
import { useAccountsStore } from "@/stores/accounts";
import type { Calendar } from "@/lib/types";
import { dragCalendarEvent, isCalendarDragging } from "@/lib/calendar-drag-state";
import { masterEventId } from "@/lib/rrule";
import { showToast } from "@/lib/toast";
import * as api from "@/lib/tauri";

const emit = defineEmits<{
  calendarDrop: [payload: {
    eventId: string;
    targetCalendarId: string;
    targetAccountId: string;
    attendeesJson: string | null;
    organizerEmail: string | null;
  }];
}>();

const calendarStore = useCalendarStore();
const accountsStore = useAccountsStore();

function getAccountLabel(accountId: string): string {
  const acc = accountsStore.accounts.find((a) => a.id === accountId);
  return acc ? acc.email : "";
}

const contextMenu = ref<{ x: number; y: number; calendarId: string; accountId: string } | null>(null);
const syncing = ref<string | null>(null);

function getCalendarColor(color: string): string {
  return color || "#4285f4";
}

// Time the menu was opened. Used to suppress the trailing click that
// some WebKit builds synthesise on right-mouse-release; without this
// guard the menu would flash open and immediately close before the
// user could interact with it.
let menuOpenedAt = 0;

function onContextMenu(event: MouseEvent, calId: string, accountId: string) {
  event.preventDefault();
  event.stopPropagation();
  menuOpenedAt = performance.now();
  contextMenu.value = { x: event.clientX, y: event.clientY, calendarId: calId, accountId };
}

// Toggle visibility on left-click of the row. We use a plain <div>
// (not <label>) because WebKitGTK's <label> autoactivates the wrapped
// <input> on *any* mouse-button press, which means right-clicking
// flips the checkbox before the contextmenu handler can run. With a
// <div>, the click event itself only fires for the primary button,
// and we additionally guard on `event.button` for safety.
function onLabelClick(event: MouseEvent, calId: string) {
  if (event.button !== 0) return;
  // Direct clicks on the checkbox are handled by its own @change;
  // @click.stop on the input prevents this branch from running, but
  // keep the tag check as a belt-and-braces fallback.
  const target = event.target as HTMLElement | null;
  if (target?.tagName === "INPUT") return;
  calendarStore.toggleCalendarVisibility(calId);
}

function closeContextMenu() {
  contextMenu.value = null;
}

// Close the menu on any LEFT-button click that lands outside the
// teleported menu itself. Listener is attached permanently (in
// onMounted) and short-circuits when the menu isn't open — that way
// there's no watch / microtask race between setting `contextMenu`
// and the listener actually existing.
function onDocClickForMenu(e: MouseEvent) {
  if (!contextMenu.value) return;
  if (e.button !== 0) return;
  // 250ms guard against the right-click → synthesised-click sequence
  // some WebKitGTK builds produce.
  if (performance.now() - menuOpenedAt < 250) return;
  const target = e.target as HTMLElement | null;
  if (target?.closest(".cal-context-menu")) return;
  closeContextMenu();
}

onMounted(() => {
  document.addEventListener("click", onDocClickForMenu);
});
onBeforeUnmount(() => {
  document.removeEventListener("click", onDocClickForMenu);
});

const dropTargetCalendarId = ref<string | null>(null);

function onCalendarItemEnter(calId: string) {
  if (!isCalendarDragging.value || !dragCalendarEvent.value) return;
  if (dragCalendarEvent.value.calendar_id === calId) return;
  dropTargetCalendarId.value = calId;
}

function onCalendarItemLeave(calId: string) {
  if (dropTargetCalendarId.value === calId) {
    dropTargetCalendarId.value = null;
  }
}

function onCalendarItemDrop(cal: Calendar) {
  if (!isCalendarDragging.value || !dragCalendarEvent.value) return;
  const ev = dragCalendarEvent.value;
  if (ev.calendar_id === cal.id) {
    dropTargetCalendarId.value = null;
    return;
  }
  dropTargetCalendarId.value = null;
  emit("calendarDrop", {
    // Recurring occurrences carry a synthetic `<masterId>_<start ISO>` id;
    // downstream lookups and the move itself operate on the master row.
    eventId: masterEventId(ev.id),
    targetCalendarId: cal.id,
    targetAccountId: cal.account_id,
    attendeesJson: ev.attendees_json,
    organizerEmail: ev.organizer_email,
  });
}

async function syncThisCalendar() {
  if (!contextMenu.value) return;
  const accountId = contextMenu.value.accountId;
  syncing.value = contextMenu.value.calendarId;
  closeContextMenu();

  try {
    await api.syncCalendars(accountId);
    await calendarStore.fetchCalendars();
    await calendarStore.fetchEvents();
  } catch (e) {
    console.error("Calendar sync failed:", e);
  } finally {
    syncing.value = null;
  }
}

const renaming = ref<{ calendar: Calendar; value: string } | null>(null);
const renameSaving = ref(false);
const renameError = ref<string | null>(null);

// Color-picker dialog state (#132). Mirrors the rename-dialog
// shape: a snapshot of the calendar being edited plus a draft
// `value` (the picked hex) and saving / error refs the modal binds
// to. Cleared on cancel + on a successful save.
const recoloring = ref<{ calendar: Calendar; value: string } | null>(null);
const recolorSaving = ref(false);
const recolorError = ref<string | null>(null);

// Curated palette mirrors `random_calendar_color()` in
// commands/calendar.rs so freshly-synced calendars and
// user-picked colors come from the same set. If the user's
// current color isn't in the palette (e.g. a server-supplied
// custom hex) the picker still highlights it via the
// `current === value` check below; the dialog also accepts a
// freeform input.
const PALETTE: { hex: string; name: string }[] = [
  { hex: "#4285f4", name: "Blue" },
  { hex: "#0b8043", name: "Green" },
  { hex: "#8e24aa", name: "Purple" },
  { hex: "#d50000", name: "Red" },
  { hex: "#f4511e", name: "Orange" },
  { hex: "#039be5", name: "Cyan" },
  { hex: "#616161", name: "Grey" },
  { hex: "#e67c73", name: "Salmon" },
  { hex: "#f6bf26", name: "Yellow" },
  { hex: "#33b679", name: "Teal" },
];

function startRecolor() {
  if (!contextMenu.value) return;
  const cal = calendarStore.calendars.find(
    (c) => c.id === contextMenu.value!.calendarId,
  );
  closeContextMenu();
  if (!cal) return;
  recolorError.value = null;
  recoloring.value = { calendar: cal, value: cal.color || "#4285f4" };
}

function cancelRecolor() {
  recoloring.value = null;
  recolorError.value = null;
}

/// Match either `#rgb`, `#rgba`, `#rrggbb`, or `#rrggbbaa`. Anything
/// else is rejected before we hit the backend so we can't end up
/// persisting a string the server will choke on (Graph returned an
/// ISE on bogus inputs, CalDAV PROPPATCH'd a literal "blueish" into
/// the calendar color).
const HEX_COLOR_RE = /^#(?:[0-9a-f]{3,4}|[0-9a-f]{6}|[0-9a-f]{8})$/i;

async function confirmRecolor() {
  if (!recoloring.value) return;
  const newColor = recoloring.value.value.trim().toLowerCase();
  if (!newColor || newColor === recoloring.value.calendar.color.toLowerCase()) {
    cancelRecolor();
    return;
  }
  if (!HEX_COLOR_RE.test(newColor)) {
    recolorError.value = "Color must be a hex value like #4285f4 or #4285f4ff.";
    return;
  }
  recolorSaving.value = true;
  recolorError.value = null;
  try {
    await api.updateCalendar(
      recoloring.value.calendar.id,
      recoloring.value.calendar.name,
      newColor,
    );
  } catch (e) {
    recolorError.value = e instanceof Error ? e.message : String(e);
    recolorSaving.value = false;
    return;
  }
  // Close the modal as soon as the backend returns — fetchCalendars
  // can be slow when one account's listCalendars takes its time, and
  // we don't want the dialog to sit there while the sidebar refreshes
  // in the background.
  recoloring.value = null;
  recolorSaving.value = false;
  showToast(`Color updated`, "success");
  calendarStore
    .fetchCalendars()
    .catch((e) => console.error("post-recolor refresh failed:", e));
}

function startRename() {
  if (!contextMenu.value) return;
  const cal = calendarStore.calendars.find(
    (c) => c.id === contextMenu.value!.calendarId,
  );
  closeContextMenu();
  if (!cal) return;
  renameError.value = null;
  renaming.value = { calendar: cal, value: cal.name };
}

function cancelRename() {
  renaming.value = null;
  renameError.value = null;
}

async function confirmRename() {
  if (!renaming.value) return;
  const newName = renaming.value.value.trim();
  if (!newName || newName === renaming.value.calendar.name) {
    cancelRename();
    return;
  }
  renameSaving.value = true;
  renameError.value = null;
  try {
    await api.updateCalendar(
      renaming.value.calendar.id,
      newName,
      renaming.value.calendar.color,
    );
    await calendarStore.fetchCalendars();
    showToast(`Renamed to "${newName}"`, "success");
    renaming.value = null;
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    renameError.value = msg;
  } finally {
    renameSaving.value = false;
  }
}

async function unsubscribeThisCalendar() {
  if (!contextMenu.value) return;
  const calendarId = contextMenu.value.calendarId;
  const cal = calendarStore.calendars.find((c) => c.id === calendarId);
  const calName = cal?.name || calendarId;
  closeContextMenu();

  if (!confirm(`Unsubscribe from "${calName}"? Local events will be removed.`)) return;

  try {
    await calendarStore.unsubscribeCalendar(calendarId);
    showToast(`Unsubscribed from "${calName}"`, "success");
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    showToast(`Failed to unsubscribe: ${msg}`, "error", 5000);
  }
}
</script>

<template>
  <div class="calendar-sidebar">
    <div class="app-sidebar-header">Calendars</div>
    <div class="calendar-list">
      <div
        v-for="cal in calendarStore.calendars"
        :key="cal.id"
        class="calendar-item"
        :class="{ syncing: syncing === cal.id, 'drag-over': dropTargetCalendarId === cal.id }"
        :data-testid="`calendar-item-${cal.id}`"
        @contextmenu="onContextMenu($event, cal.id, cal.account_id)"
        @mouseenter="onCalendarItemEnter(cal.id)"
        @mouseleave="onCalendarItemLeave(cal.id)"
        @mouseup="onCalendarItemDrop(cal)"
      >
        <div
          class="calendar-label"
          role="checkbox"
          tabindex="0"
          :aria-checked="!calendarStore.hiddenCalendarIds.includes(cal.id)"
          @click="onLabelClick($event, cal.id)"
          @keydown.space.prevent="calendarStore.toggleCalendarVisibility(cal.id)"
          @keydown.enter.prevent="calendarStore.toggleCalendarVisibility(cal.id)"
        >
          <input
            type="checkbox"
            :checked="!calendarStore.hiddenCalendarIds.includes(cal.id)"
            @click.stop
            @change="calendarStore.toggleCalendarVisibility(cal.id)"
            data-testid="calendar-toggle"
          />
          <span
            class="calendar-color"
            :style="{ backgroundColor: getCalendarColor(cal.color) }"
          ></span>
          <span class="calendar-name-group">
            <span class="calendar-name">{{ cal.name }}</span>
            <span class="calendar-account">{{ getAccountLabel(cal.account_id) }}</span>
          </span>
          <span v-if="syncing === cal.id" class="sync-spinner"></span>
        </div>
      </div>
      <div v-if="calendarStore.calendars.length === 0" class="empty">
        No calendars
      </div>
    </div>

    <!-- Right-click context menu -->
    <Teleport to="body">
      <div
        v-if="contextMenu"
        class="cal-context-menu"
        :style="{ left: contextMenu.x + 'px', top: contextMenu.y + 'px' }"
      >
        <button class="ctx-item" @click="syncThisCalendar" data-testid="calendar-sync">Sync this calendar</button>
        <button class="ctx-item" @click="startRename" data-testid="calendar-rename">Rename…</button>
        <button class="ctx-item" @click="startRecolor" data-testid="calendar-recolor">Change color…</button>
        <button class="ctx-item" @click="unsubscribeThisCalendar" data-testid="calendar-unsubscribe">Unsubscribe</button>
      </div>
    </Teleport>

    <!-- Rename modal -->
    <Teleport to="body">
      <div
        v-if="renaming"
        class="cal-rename-overlay"
        data-testid="calendar-rename-modal"
        @click.self="cancelRename"
      >
        <div class="rename-modal">
          <div class="rename-body">
            <h3>Rename Calendar</h3>
            <p class="rename-sub">Renaming will update the calendar on the server.</p>
            <input
              v-model="renaming.value"
              type="text"
              class="rename-input"
              data-testid="calendar-rename-input"
              :disabled="renameSaving"
              placeholder="Calendar name"
              @keyup.enter="confirmRename"
              @keyup.escape="cancelRename"
            />
            <p v-if="renameError" class="rename-error" data-testid="calendar-rename-error">
              {{ renameError }}
            </p>
          </div>
          <div class="rename-footer">
            <button
              class="rename-btn-cancel"
              :disabled="renameSaving"
              data-testid="calendar-rename-cancel"
              @click="cancelRename"
            >
              Cancel
            </button>
            <button
              class="rename-btn-save"
              :disabled="renameSaving || !renaming.value.trim()"
              data-testid="calendar-rename-save"
              @click="confirmRename"
            >
              {{ renameSaving ? "Renaming…" : "Rename" }}
            </button>
          </div>
        </div>
      </div>
    </Teleport>

    <!-- Color picker (#132) -->
    <Teleport to="body">
      <div
        v-if="recoloring"
        class="cal-rename-overlay"
        data-testid="calendar-recolor-modal"
        @click.self="cancelRecolor"
      >
        <div class="rename-modal">
          <div class="rename-body">
            <h3>Change color</h3>
            <p class="rename-sub">
              Pick a color for "{{ recoloring.calendar.name }}". CalDAV / JMAP store the exact hex you pick. Microsoft 365 and Google use a fixed palette, so the picked color is approximated and may not match this swatch one-for-one. System calendars (Birthdays, Holidays, etc.) on Microsoft are read-only — the change is kept locally if the server rejects it.
            </p>
            <div class="color-swatches">
              <button
                v-for="entry in PALETTE"
                :key="entry.hex"
                type="button"
                class="color-swatch"
                :class="{ selected: recoloring.value.toLowerCase() === entry.hex.toLowerCase() }"
                :style="{ backgroundColor: entry.hex }"
                :title="entry.name"
                :data-testid="`calendar-color-${entry.hex.slice(1)}`"
                :disabled="recolorSaving"
                @click="recoloring.value = entry.hex"
              ></button>
            </div>
            <input
              v-model="recoloring.value"
              type="text"
              class="rename-input color-input"
              placeholder="#rrggbb"
              data-testid="calendar-color-custom"
              :disabled="recolorSaving"
              @keyup.enter="confirmRecolor"
              @keyup.escape="cancelRecolor"
            />
            <p v-if="recolorError" class="rename-error" data-testid="calendar-recolor-error">
              {{ recolorError }}
            </p>
          </div>
          <div class="rename-footer">
            <button
              class="rename-btn-cancel"
              :disabled="recolorSaving"
              data-testid="calendar-recolor-cancel"
              @click="cancelRecolor"
            >
              Cancel
            </button>
            <button
              class="rename-btn-save"
              :disabled="recolorSaving || !recoloring.value.trim()"
              data-testid="calendar-recolor-save"
              @click="confirmRecolor"
            >
              {{ recolorSaving ? "Saving…" : "Save" }}
            </button>
          </div>
        </div>
      </div>
    </Teleport>
  </div>
</template>

<style scoped>
.calendar-sidebar {
  height: 100%;
  display: flex;
  flex-direction: column;
  background: var(--color-bg-secondary);
}

/* The sidebar heading is 48px and full-width (no horizontal padding
   from the parent), matching the right-pane toolbar. Items below get
   their own padding via .calendar-list. */
.calendar-list {
  flex: 1;
  overflow-y: auto;
  padding: 12px;
}

.calendar-item {
  padding: 4px 0;
}

.calendar-item.syncing {
  opacity: 0.6;
}

.calendar-item.drag-over {
  background: rgba(66, 133, 244, 0.12);
  border-radius: 4px;
  outline: 1px dashed var(--color-accent);
  outline-offset: -1px;
}

.calendar-label {
  display: flex;
  align-items: center;
  gap: 8px;
  cursor: pointer;
  font-size: 13px;
}

.calendar-color {
  width: 10px;
  height: 10px;
  border-radius: 2px;
  flex-shrink: 0;
}

.calendar-name-group {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-width: 0;
  gap: 0;
}

.calendar-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.calendar-account {
  font-size: 10px;
  color: var(--color-text-muted);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  line-height: 1.2;
}

.sync-spinner {
  width: 10px;
  height: 10px;
  border: 2px solid var(--color-border);
  border-top-color: var(--color-accent);
  border-radius: 50%;
  animation: spin 0.8s linear infinite;
  flex-shrink: 0;
}

@keyframes spin {
  to { transform: rotate(360deg); }
}

.empty {
  color: var(--color-text-muted);
  font-size: 12px;
  padding: 8px 4px;
}

</style>

<style>
.cal-context-menu {
  position: fixed;
  z-index: 9999;
  background: var(--color-bg-secondary);
  border: 1px solid var(--color-border);
  border-radius: 6px;
  padding: 4px 0;
  min-width: 180px;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.3);
}

.cal-context-menu .ctx-item {
  display: block;
  width: 100%;
  padding: 6px 16px;
  text-align: left;
  font-size: 12px;
  color: var(--color-text);
  background: none;
  border: none;
  cursor: pointer;
}

.cal-context-menu .ctx-item:hover {
  background: var(--color-bg-hover);
}

.cal-rename-overlay {
  position: fixed;
  top: 0;
  left: 0;
  right: 0;
  bottom: 0;
  background: rgba(0, 0, 0, 0.4);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 10000;
}

.rename-modal {
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: 8px;
  width: 360px;
  max-width: calc(100vw - 32px);
  box-shadow: 0 8px 32px rgba(0, 0, 0, 0.3);
}

.rename-body {
  padding: 18px 20px 4px;
}

.rename-body h3 {
  margin: 0 0 6px;
  font-size: 15px;
  color: var(--color-text);
}

.rename-sub {
  margin: 0 0 12px;
  font-size: 12px;
  color: var(--color-text-muted);
}

.rename-input {
  width: 100%;
  padding: 8px 10px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg-secondary);
  color: var(--color-text);
  font-size: 14px;
}

.rename-input:focus {
  outline: none;
  border-color: var(--color-accent);
}

/* Color-picker (#132) */
.color-swatches {
  display: grid;
  grid-template-columns: repeat(5, 1fr);
  gap: 8px;
  margin: 12px 0;
}
.color-swatch {
  width: 100%;
  aspect-ratio: 1 / 1;
  border-radius: 6px;
  border: 2px solid transparent;
  cursor: pointer;
  transition: transform 0.08s, border-color 0.12s;
  padding: 0;
}
.color-swatch:hover { transform: scale(1.06); }
.color-swatch.selected {
  border-color: var(--color-text);
}
.color-swatch:disabled {
  cursor: not-allowed;
  opacity: 0.5;
}
.color-input {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  text-transform: lowercase;
}

.rename-error {
  margin: 8px 0 0;
  font-size: 12px;
  color: #dc2626;
}

.rename-footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding: 12px 20px 16px;
}

.rename-btn-cancel,
.rename-btn-save {
  padding: 6px 14px;
  border-radius: 4px;
  font-size: 13px;
  cursor: pointer;
}

.rename-btn-cancel {
  background: var(--color-bg-tertiary);
  color: var(--color-text);
  border: 1px solid var(--color-border);
}

.rename-btn-save {
  background: var(--color-accent);
  color: white;
  border: 1px solid var(--color-accent);
}

.rename-btn-save:disabled,
.rename-btn-cancel:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
