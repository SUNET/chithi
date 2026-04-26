<script setup lang="ts">
import { ref, onBeforeUnmount, watch } from "vue";
import { useCalendarStore } from "@/stores/calendar";
import { useAccountsStore } from "@/stores/accounts";
import type { Calendar } from "@/lib/types";
import { dragCalendarEvent, isCalendarDragging } from "@/lib/calendar-drag-state";
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

function onContextMenu(event: MouseEvent, calId: string, accountId: string) {
  event.preventDefault();
  contextMenu.value = { x: event.clientX, y: event.clientY, calendarId: calId, accountId };
}

function closeContextMenu() {
  contextMenu.value = null;
}

// Close the context menu on any LEFT-button click that lands outside the
// teleported menu itself. WebKitGTK synthesises a click event on
// right-mouse-release (button === 2), so without the button guard the
// menu would close immediately when the user lets go of the right button.
function onDocClickForMenu(e: MouseEvent) {
  if (!contextMenu.value) return;
  if (e.button !== 0) return;
  const target = e.target as HTMLElement | null;
  if (target?.closest(".cal-context-menu")) return;
  closeContextMenu();
}

watch(contextMenu, (open) => {
  if (open) {
    document.addEventListener("click", onDocClickForMenu);
  } else {
    document.removeEventListener("click", onDocClickForMenu);
  }
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
    eventId: ev.id,
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
    <div class="sidebar-header">CALENDARS</div>
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
        <label class="calendar-label">
          <input
            type="checkbox"
            :checked="!calendarStore.hiddenCalendarIds.includes(cal.id)"
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
        </label>
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
  </div>
</template>

<style scoped>
.calendar-sidebar {
  height: 100%;
  display: flex;
  flex-direction: column;
  background: var(--color-bg-secondary);
  padding: 16px 12px;
}

.sidebar-header {
  font-size: 10px;
  font-weight: 700;
  color: var(--color-text-muted);
  text-transform: uppercase;
  letter-spacing: 1px;
  padding: 0 4px 12px;
  border-bottom: 1px solid var(--color-border);
  margin-bottom: 12px;
}

.calendar-list {
  flex: 1;
  overflow-y: auto;
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
