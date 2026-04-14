<script setup lang="ts">
import { ref, computed, nextTick } from "vue";
import { useCalendarStore } from "@/stores/calendar";
import { useAccountsStore } from "@/stores/accounts";
import { useUiStore } from "@/stores/ui";
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
const uiStore = useUiStore();

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

const tzSearch = ref("");
const tzDropdownOpen = ref(false);
const tzHighlightIndex = ref(-1);
const tzDropdownRef = ref<HTMLElement | null>(null);

const filteredTimezones = computed(() => {
  const query = tzSearch.value.toLowerCase();
  if (!query) return uiStore.timezoneList;
  return uiStore.timezoneList.filter((tz: string) => tz.toLowerCase().includes(query));
});

function selectTimezone(tz: string) {
  uiStore.setDisplayTimezone(tz);
  tzSearch.value = "";
  tzDropdownOpen.value = false;
  tzHighlightIndex.value = -1;
}

function onTzInputFocus() {
  tzDropdownOpen.value = true;
  tzSearch.value = "";
  tzHighlightIndex.value = -1;
}

function onTzInputBlur() {
  setTimeout(() => {
    tzDropdownOpen.value = false;
    tzSearch.value = "";
    tzHighlightIndex.value = -1;
  }, 200);
}

function onTzKeydown(e: KeyboardEvent) {
  if (!tzDropdownOpen.value) return;
  const list = filteredTimezones.value;
  if (e.key === "ArrowDown") {
    e.preventDefault();
    tzHighlightIndex.value = Math.min(tzHighlightIndex.value + 1, list.length - 1);
    scrollHighlightedIntoView();
  } else if (e.key === "ArrowUp") {
    e.preventDefault();
    tzHighlightIndex.value = Math.max(tzHighlightIndex.value - 1, 0);
    scrollHighlightedIntoView();
  } else if (e.key === "Enter") {
    e.preventDefault();
    if (tzHighlightIndex.value >= 0 && tzHighlightIndex.value < list.length) {
      selectTimezone(list[tzHighlightIndex.value]);
      (e.target as HTMLInputElement)?.blur();
    }
  } else if (e.key === "Escape") {
    tzDropdownOpen.value = false;
    (e.target as HTMLInputElement)?.blur();
  }
}

function scrollHighlightedIntoView() {
  nextTick(() => {
    const el = tzDropdownRef.value?.querySelector(".tz-option.highlighted");
    if (el) el.scrollIntoView({ block: "nearest" });
  });
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
  <div class="calendar-sidebar" @click="closeContextMenu">
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

    <div class="week-start-section">
      <div class="section-header">Week starts on</div>
      <div class="week-start-options">
        <button
          v-for="opt in [{ day: 0, label: 'Sunday' }, { day: 1, label: 'Monday' }, { day: 6, label: 'Saturday' }]"
          :key="opt.day"
          class="week-start-btn"
          :class="{ active: uiStore.weekStartDay === opt.day }"
          :data-testid="`week-start-${opt.day}`"
          @click="uiStore.setWeekStartDay(opt.day)"
        >{{ opt.label }}</button>
      </div>
    </div>

    <div class="timezone-section">
      <div class="section-header">Use timezone</div>
      <div class="timezone-picker">
        <input
          type="text"
          class="tz-search-input"
          :placeholder="uiStore.displayTimezone"
          :value="tzDropdownOpen ? tzSearch : ''"
          @input="tzSearch = ($event.target as HTMLInputElement).value; tzHighlightIndex = 0"
          @focus="onTzInputFocus"
          @blur="onTzInputBlur"
          @keydown="onTzKeydown"
          data-testid="timezone-search"
        />
        <div v-if="tzDropdownOpen" ref="tzDropdownRef" class="tz-dropdown">
          <button
            v-for="(tz, idx) in filteredTimezones"
            :key="tz"
            class="tz-option"
            :class="{ active: tz === uiStore.displayTimezone, highlighted: idx === tzHighlightIndex }"
            @mousedown.prevent="selectTimezone(tz)"
            @mouseenter="tzHighlightIndex = idx"
          >
            {{ tz }}
          </button>
          <div v-if="filteredTimezones.length === 0" class="tz-empty">
            No matching timezones
          </div>
        </div>
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
        <button class="ctx-item" @click="unsubscribeThisCalendar" data-testid="calendar-unsubscribe">Unsubscribe</button>
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
  width: 12px;
  height: 12px;
  border-radius: 3px;
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

.week-start-section {
  margin-top: 16px;
  padding-top: 12px;
  border-top: 1px solid var(--color-border);
}

.section-header {
  font-size: 10px;
  font-weight: 700;
  color: var(--color-text-muted);
  text-transform: uppercase;
  letter-spacing: 1px;
  padding: 0 4px 8px;
}

.week-start-options {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.week-start-btn {
  padding: 4px 8px;
  font-size: 12px;
  color: var(--color-text-secondary);
  text-align: left;
  border-radius: 4px;
  transition: background 0.1s;
}

.week-start-btn:hover {
  background: var(--color-bg-hover);
}

.week-start-btn.active {
  color: var(--color-accent);
  font-weight: 600;
}

.timezone-section {
  margin-top: 16px;
  padding-top: 12px;
  border-top: 1px solid var(--color-border);
}

.timezone-picker {
  position: relative;
  padding: 0 4px;
}

.tz-search-input {
  width: 100%;
  padding: 4px 8px;
  font-size: 12px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  color: var(--color-text);
  outline: none;
  box-sizing: border-box;
}

.tz-search-input:focus {
  border-color: var(--color-accent);
}

.tz-search-input::placeholder {
  color: var(--color-text-secondary);
}

.tz-dropdown {
  position: absolute;
  bottom: 100%;
  left: 4px;
  right: 4px;
  max-height: 200px;
  overflow-y: auto;
  background: var(--color-bg-secondary);
  border: 1px solid var(--color-border);
  border-radius: 4px;
  margin-bottom: 2px;
  z-index: 50;
  box-shadow: 0 -4px 12px rgba(0, 0, 0, 0.15);
}

.tz-option {
  display: block;
  width: 100%;
  padding: 4px 8px;
  text-align: left;
  font-size: 12px;
  color: var(--color-text-secondary);
  background: none;
  border: none;
  cursor: pointer;
}

.tz-option:hover,
.tz-option.highlighted {
  background: var(--color-bg-hover);
}

.tz-option.active {
  color: var(--color-accent);
  font-weight: 600;
}

.tz-empty {
  padding: 8px;
  font-size: 12px;
  color: var(--color-text-muted);
  text-align: center;
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
