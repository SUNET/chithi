<script setup lang="ts">
import { ref } from "vue";
import { useCalendarStore } from "@/stores/calendar";
import { useAccountsStore } from "@/stores/accounts";
import * as api from "@/lib/tauri";

const calendarStore = useCalendarStore();
const accountsStore = useAccountsStore();

function getAccountName(accountId: string): string {
  const acc = accountsStore.accounts.find((a) => a.id === accountId);
  return acc ? `${acc.display_name} (${acc.email})` : "";
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
</script>

<template>
  <div class="calendar-sidebar" @click="closeContextMenu">
    <div class="sidebar-header">CALENDARS</div>
    <div class="calendar-list">
      <div
        v-for="cal in calendarStore.calendars"
        :key="cal.id"
        class="calendar-item"
        :class="{ syncing: syncing === cal.id }"
        @contextmenu="onContextMenu($event, cal.id, cal.account_id)"
      >
        <label class="calendar-label">
          <input
            type="checkbox"
            :checked="!calendarStore.hiddenCalendarIds.includes(cal.id)"
            @change="calendarStore.toggleCalendarVisibility(cal.id)"
          />
          <span
            class="calendar-color"
            :style="{ backgroundColor: getCalendarColor(cal.color) }"
          ></span>
          <span class="calendar-name" :title="getAccountName(cal.account_id)">{{ cal.name }}</span>
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
        <button class="ctx-item" @click="syncThisCalendar">Sync this calendar</button>
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

.calendar-label {
  display: flex;
  align-items: center;
  gap: 8px;
  cursor: pointer;
  font-size: 13px;
}

.calendar-color {
  width: 12px;
  height: 12px;
  border-radius: 3px;
  flex-shrink: 0;
}

.calendar-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  flex: 1;
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
</style>
