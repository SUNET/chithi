<script setup lang="ts">
import { onMounted, ref, computed, watch } from "vue";
import { storeToRefs } from "pinia";
import { useCalendarStore } from "@/stores/calendar";
import { useAccountsStore } from "@/stores/accounts";
import { usePlatformStore } from "@/stores/platform";
import type { CalendarViewMode } from "@/stores/calendar";
import type { CalendarEvent } from "@/lib/types";
import { showToast, dismissToast } from "@/lib/toast";
import * as api from "@/lib/tauri";
import CalendarSidebar from "@/components/calendar/CalendarSidebar.vue";
import WeekView from "@/components/calendar/WeekView.vue";
import MonthView from "@/components/calendar/MonthView.vue";
import EventDetail from "@/components/calendar/EventDetail.vue";
import EventForm from "@/components/calendar/EventForm.vue";
import MobileAppBar from "@/components/mobile/MobileAppBar.vue";
import MobileIconButton from "@/components/mobile/MobileIconButton.vue";

// Explicit name so <KeepAlive include="CalendarView"> in App.vue matches.
defineOptions({ name: "CalendarView" });

const calendarStore = useCalendarStore();
const accountsStore = useAccountsStore();
const platformStore = usePlatformStore();
const { isMobile } = storeToRefs(platformStore);
const showEventForm = ref(false);
const newEventStart = ref("");

// Mobile defaults to Day view. Flip once when the store is in a wider mode.
watch(isMobile, (mobile) => {
  if (mobile && calendarStore.viewMode !== "day" && calendarStore.viewMode !== "week") {
    calendarStore.setViewMode("day");
  }
}, { immediate: true });

// --- Mobile helpers (Day / Week / Month) ---
const HOUR_ROW = 44; // px per hour; used for both day and mobile-week grids
const DAY_HOURS = Array.from({ length: 24 }, (_, h) => h); // 0..23
const WEEK_HOURS = Array.from({ length: 24 }, (_, h) => h);

const mobileHeader = computed(() => {
  const d = new Date(calendarStore.currentDate);
  if (calendarStore.viewMode === "day") {
    return d.toLocaleDateString(undefined, {
      weekday: "long",
      month: "long",
      day: "numeric",
    });
  }
  if (calendarStore.viewMode === "week") {
    const start = weekStart(d);
    const end = new Date(start);
    end.setDate(end.getDate() + 6);
    const sameMonth = start.getMonth() === end.getMonth();
    if (sameMonth) {
      return `${start.toLocaleDateString(undefined, { month: "short", day: "numeric" })} – ${end.getDate()}`;
    }
    return `${start.toLocaleDateString(undefined, { month: "short", day: "numeric" })} – ${end.toLocaleDateString(undefined, { month: "short", day: "numeric" })}`;
  }
  return d.toLocaleDateString(undefined, { month: "long", year: "numeric" });
});

function weekStart(d: Date): Date {
  const start = new Date(d);
  // Reset to start of day
  start.setHours(0, 0, 0, 0);
  const day = start.getDay();
  start.setDate(start.getDate() - day); // Sunday start — matches desktop
  return start;
}

function weekDates(): Date[] {
  const start = weekStart(new Date(calendarStore.currentDate));
  return Array.from({ length: 7 }, (_, i) => {
    const d = new Date(start);
    d.setDate(start.getDate() + i);
    return d;
  });
}

function isSameDate(a: Date, b: Date): boolean {
  return (
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate()
  );
}

function isToday(d: Date): boolean {
  return isSameDate(d, new Date());
}

interface PositionedEvent {
  event: CalendarEvent;
  top: number;
  height: number;
  color: string;
}

function calendarColor(event: CalendarEvent): string {
  return (
    calendarStore.calendars.find((c) => c.id === event.calendar_id)?.color ?? "#b54708"
  );
}

function positionedEventsForDay(date: Date): PositionedEvent[] {
  const day = new Date(date);
  day.setHours(0, 0, 0, 0);
  const nextDay = new Date(day);
  nextDay.setDate(day.getDate() + 1);

  const positioned: PositionedEvent[] = [];
  for (const ev of calendarStore.visibleEvents) {
    if (ev.all_day) continue; // keep the day grid strictly timed
    const start = new Date(ev.start_time);
    const end = new Date(ev.end_time);
    if (end <= day || start >= nextDay) continue;
    // Clamp to this day so multi-day events render sanely.
    const segStart = start < day ? day : start;
    const segEnd = end > nextDay ? nextDay : end;
    const startHour = (segStart.getTime() - day.getTime()) / (1000 * 60 * 60);
    const durationHours = (segEnd.getTime() - segStart.getTime()) / (1000 * 60 * 60);
    positioned.push({
      event: ev,
      top: startHour * HOUR_ROW,
      height: Math.max(20, durationHours * HOUR_ROW - 2),
      color: calendarColor(ev),
    });
  }
  return positioned;
}

function allDayEventsForDay(date: Date): CalendarEvent[] {
  return calendarStore.visibleEvents.filter((ev) => {
    if (!ev.all_day) return false;
    const start = new Date(ev.start_time);
    return isSameDate(start, date);
  });
}

// Month-view cells
interface MonthCell {
  date: Date;
  inMonth: boolean;
  events: CalendarEvent[];
}

const monthCells = computed<MonthCell[]>(() => {
  const anchor = new Date(calendarStore.currentDate);
  anchor.setDate(1);
  anchor.setHours(0, 0, 0, 0);
  const monthIdx = anchor.getMonth();

  // Grid always starts on Sunday and has 6 rows × 7 cols = 42 cells.
  const gridStart = new Date(anchor);
  gridStart.setDate(1 - anchor.getDay());
  const cells: MonthCell[] = [];
  for (let i = 0; i < 42; i++) {
    const d = new Date(gridStart);
    d.setDate(gridStart.getDate() + i);
    const dayEvents = calendarStore.visibleEvents.filter((ev) => {
      const evStart = new Date(ev.start_time);
      return isSameDate(evStart, d);
    });
    cells.push({
      date: d,
      inMonth: d.getMonth() === monthIdx,
      events: dayEvents,
    });
  }
  return cells;
});

function todayEvents(): CalendarEvent[] {
  const today = new Date();
  return calendarStore.visibleEvents
    .filter((ev) => isSameDate(new Date(ev.start_time), today))
    .sort((a, b) => new Date(a.start_time).getTime() - new Date(b.start_time).getTime());
}

function formatTime(iso: string): string {
  return new Date(iso).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

function nowTopOffset(): number {
  const now = new Date();
  const minutes = now.getHours() * 60 + now.getMinutes();
  return (minutes / 60) * HOUR_ROW;
}

function setMobileViewMode(mode: CalendarViewMode) {
  calendarStore.setViewMode(mode);
}

function jumpToDay(date: Date) {
  calendarStore.setViewMode("day");
  calendarStore.goToDate(date.toISOString().split("T")[0]);
}

function onMobileEventClick(event: CalendarEvent) {
  calendarStore.selectEvent(event);
}

function formatCurrentDate(): string {
  const d = new Date(calendarStore.currentDate);
  return d.toLocaleDateString(undefined, { month: "long", year: "numeric" });
}

function onTimeSlotClick(dateTime: string) {
  newEventStart.value = dateTime;
  showEventForm.value = true;
}

function onEventClick(eventId: string) {
  const event = calendarStore.events.find((e) => e.id === eventId);
  if (event) calendarStore.selectEvent(event);
}

function tryParseAttendees(json: string | null): Array<{ email: string }> {
  if (!json) return [];
  try { return JSON.parse(json); } catch { return []; }
}

function isOrganizer(accountId: string, organizerEmail: string | null): boolean {
  if (!organizerEmail) return true;
  const account = accountsStore.accounts.find((a) => a.id === accountId);
  return account?.email === organizerEmail;
}

async function promptAttendeeNotification(
  accountId: string,
  eventId: string,
  attendeesJson: string | null,
  organizerEmail: string | null,
) {
  const attendees = tryParseAttendees(attendeesJson);
  if (attendees.length === 0) return;
  const ev = calendarStore.events.find((e) => e.id === eventId);
  if (!ev || !isOrganizer(accountId, organizerEmail)) return;

  // Simple confirm — Tauri dialog requires plugin import, use browser confirm for now
  const send = confirm("This event has attendees. Send an update notification?");
  if (send) {
    try {
      await api.sendInvites(accountId, eventId, attendees.map((a) => a.email));
      showToast("Update sent to attendees", "success");
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      showToast(`Failed to send updates: ${msg}`, "error", 5000);
    }
  }
}

async function onEventReschedule(payload: {
  eventId: string;
  newStart: string;
  newEnd: string;
  attendeesJson: string | null;
  organizerEmail: string | null;
}) {
  const toastId = showToast("Moving event...", "info", 0);
  try {
    await calendarStore.updateEvent(payload.eventId, {
      start_time: payload.newStart,
      end_time: payload.newEnd,
    });
    dismissToast(toastId);
    showToast("Event rescheduled", "success");

    const ev = calendarStore.events.find((e) => e.id === payload.eventId);
    if (ev) {
      await promptAttendeeNotification(ev.account_id, payload.eventId, payload.attendeesJson, payload.organizerEmail);
    }
  } catch (e) {
    dismissToast(toastId);
    const msg = e instanceof Error ? e.message : String(e);
    showToast(`Failed to reschedule: ${msg}`, "error", 5000);
  }
}

async function onCalendarDrop(payload: {
  eventId: string;
  targetCalendarId: string;
  targetAccountId: string;
  attendeesJson: string | null;
  organizerEmail: string | null;
}) {
  const ev = calendarStore.events.find((e) => e.id === payload.eventId);
  if (!ev) return;

  const toastId = showToast("Moving to calendar...", "info", 0);
  try {
    const newId = await calendarStore.moveEventToCalendar(
      payload.eventId,
      payload.targetCalendarId,
      payload.targetAccountId,
    );
    dismissToast(toastId);
    showToast("Event moved to calendar", "success");
    // Use the destination account + new event ID for attendee notification
    await promptAttendeeNotification(
      payload.targetAccountId,
      newId,
      payload.attendeesJson,
      payload.organizerEmail,
    );
  } catch (e) {
    dismissToast(toastId);
    const msg = e instanceof Error ? e.message : String(e);
    showToast(`Failed to move event: ${msg}`, "error", 5000);
  }
}

onMounted(() => {
  // Ensure accounts are loaded — App.vue loads them but it may not be done yet.
  // Everything below runs without blocking the mount: the template renders
  // against the (cached) reactive refs immediately and updates as the
  // parallel fetches resolve.
  const ready =
    accountsStore.accounts.length === 0
      ? accountsStore.fetchAccounts()
      : Promise.resolve();

  ready
    .then(() => calendarStore.fetchCalendars())
    .catch((e) => console.error("fetchCalendars error:", e));
  ready
    .then(() => calendarStore.fetchEvents())
    .catch((e) => console.error("fetchEvents error:", e));

  // Initial sync + start independent interval (5 min).
  // The interval is intentionally NOT cleared on unmount — it keeps
  // calendars fresh in the background for the lifetime of the app,
  // matching how mail sync runs continuously. The calendar store's
  // stopCalendarSync() is available if explicit teardown is needed.
  ready
    .then(() => calendarStore.syncCalendars())
    .catch((e) => console.error("Calendar sync error:", e));
  ready
    .then(() => calendarStore.startCalendarSync())
    .catch((e) => console.error("startCalendarSync error:", e));
});
</script>

<template>
  <!-- Mobile: Day (default) / Week / Month -->
  <div v-if="isMobile" class="calendar-view mobile">
    <MobileAppBar :title="mobileHeader">
      <template #leading>
        <MobileIconButton aria-label="Today" @click="calendarStore.goToday()">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="12" r="9" />
            <line x1="12" y1="8" x2="12" y2="12" />
            <line x1="12" y1="16" x2="12" y2="16" />
          </svg>
        </MobileIconButton>
      </template>
      <template #trailing>
        <MobileIconButton aria-label="Previous" @click="calendarStore.goPrev()">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="15 18 9 12 15 6" />
          </svg>
        </MobileIconButton>
        <MobileIconButton aria-label="Next" @click="calendarStore.goNext()">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="9 18 15 12 9 6" />
          </svg>
        </MobileIconButton>
        <MobileIconButton aria-label="New event" @click="showEventForm = true">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
            <line x1="12" y1="5" x2="12" y2="19" />
            <line x1="5" y1="12" x2="19" y2="12" />
          </svg>
        </MobileIconButton>
      </template>
    </MobileAppBar>

    <!-- Segmented Day/Week/Month -->
    <div class="mobile-segmented" role="tablist">
      <button
        v-for="mode in (['day', 'week', 'month'] as CalendarViewMode[])"
        :key="mode"
        class="seg-btn"
        :class="{ active: calendarStore.viewMode === mode }"
        role="tab"
        :aria-selected="calendarStore.viewMode === mode"
        @click="setMobileViewMode(mode)"
      >
        {{ mode.charAt(0).toUpperCase() + mode.slice(1) }}
      </button>
    </div>

    <!-- DAY VIEW -->
    <div v-if="calendarStore.viewMode === 'day'" class="day-view">
      <div
        v-if="allDayEventsForDay(new Date(calendarStore.currentDate)).length"
        class="all-day-strip"
      >
        <button
          v-for="ev in allDayEventsForDay(new Date(calendarStore.currentDate))"
          :key="ev.id"
          class="all-day-chip"
          :style="{ background: calendarColor(ev) }"
          @click="onMobileEventClick(ev)"
        >{{ ev.title }}</button>
      </div>

      <div class="day-grid">
        <div class="hour-column">
          <div v-for="h in DAY_HOURS" :key="h" class="hour-slot">
            <span class="hour-label">{{ h.toString().padStart(2, "0") }}:00</span>
          </div>
        </div>
        <div
          class="event-column"
          @click="(e) => onTimeSlotClick(
            (() => {
              const target = e.currentTarget as HTMLElement;
              const rect = target.getBoundingClientRect();
              const offsetY = e.clientY - rect.top;
              const hour = Math.floor(offsetY / HOUR_ROW);
              const base = new Date(calendarStore.currentDate);
              base.setHours(hour, 0, 0, 0);
              return base.toISOString();
            })(),
          )"
        >
          <!-- background grid lines -->
          <div
            v-for="h in DAY_HOURS"
            :key="h"
            class="grid-line"
            :style="{ top: h * HOUR_ROW + 'px' }"
          />

          <!-- "Now" indicator -->
          <template v-if="isToday(new Date(calendarStore.currentDate))">
            <span
              class="now-dot"
              :style="{ top: nowTopOffset() - 5 + 'px' }"
              aria-hidden="true"
            />
            <div
              class="now-line"
              :style="{ top: nowTopOffset() + 'px' }"
              aria-hidden="true"
            />
          </template>

          <!-- events -->
          <button
            v-for="pe in positionedEventsForDay(new Date(calendarStore.currentDate))"
            :key="pe.event.id"
            class="day-event"
            :style="{
              top: pe.top + 'px',
              height: pe.height + 'px',
              background: pe.color,
            }"
            @click.stop="onMobileEventClick(pe.event)"
          >
            <span class="day-event-title">{{ pe.event.title }}</span>
            <span class="day-event-time">
              {{ formatTime(pe.event.start_time) }} – {{ formatTime(pe.event.end_time) }}
            </span>
          </button>
        </div>
      </div>
    </div>

    <!-- WEEK VIEW -->
    <div v-else-if="calendarStore.viewMode === 'week'" class="week-view">
      <div class="week-strip">
        <button
          v-for="d in weekDates()"
          :key="d.toISOString()"
          class="week-day"
          :class="{ today: isToday(d) }"
          @click="jumpToDay(d)"
        >
          <span class="week-dow">{{ d.toLocaleDateString(undefined, { weekday: "short" }).slice(0, 2) }}</span>
          <span class="week-date-pill">{{ d.getDate() }}</span>
        </button>
      </div>

      <div class="week-grid">
        <div class="hour-column">
          <div v-for="h in WEEK_HOURS" :key="h" class="hour-slot">
            <span class="hour-label">{{ h.toString().padStart(2, "0") }}</span>
          </div>
        </div>
        <div
          v-for="d in weekDates()"
          :key="d.toISOString()"
          class="week-day-column"
          :class="{ today: isToday(d) }"
        >
          <div v-for="h in WEEK_HOURS" :key="h" class="grid-line" :style="{ top: h * HOUR_ROW + 'px' }" />
          <button
            v-for="pe in positionedEventsForDay(d)"
            :key="pe.event.id"
            class="week-event"
            :style="{
              top: pe.top + 'px',
              height: pe.height + 'px',
              background: pe.color,
            }"
            @click="onMobileEventClick(pe.event)"
          >
            <span class="week-event-title">{{ pe.event.title }}</span>
          </button>
        </div>
      </div>
    </div>

    <!-- MONTH VIEW -->
    <div v-else class="month-view">
      <div class="month-dow">
        <span v-for="d in ['Sun','Mon','Tue','Wed','Thu','Fri','Sat']" :key="d">{{ d }}</span>
      </div>
      <div class="month-grid">
        <button
          v-for="(cell, i) in monthCells"
          :key="i"
          class="month-cell"
          :class="{
            'not-in-month': !cell.inMonth,
            today: isToday(cell.date),
          }"
          @click="jumpToDay(cell.date)"
        >
          <span class="month-date-pill">{{ cell.date.getDate() }}</span>
          <span
            v-for="ev in cell.events.slice(0, 3)"
            :key="ev.id"
            class="month-event-strip"
            :style="{ background: calendarColor(ev) }"
            :title="ev.title"
          />
          <span v-if="cell.events.length > 3" class="month-more">+{{ cell.events.length - 3 }}</span>
        </button>
      </div>
      <div class="month-today">
        <div class="month-today-label">Today</div>
        <div v-if="todayEvents().length === 0" class="month-today-empty">No events</div>
        <button
          v-for="ev in todayEvents()"
          :key="ev.id"
          class="month-today-row"
          @click="onMobileEventClick(ev)"
        >
          <span class="month-today-swatch" :style="{ background: calendarColor(ev) }" />
          <span class="month-today-time">{{ ev.all_day ? "All day" : formatTime(ev.start_time) }}</span>
          <span class="month-today-title">{{ ev.title }}</span>
        </button>
      </div>
    </div>

    <!-- Detail + form dialogs reuse the desktop components -->
    <EventDetail
      v-if="calendarStore.selectedEvent"
      @close="calendarStore.selectEvent(null)"
    />
    <EventForm
      v-if="showEventForm"
      :initial-start="newEventStart || undefined"
      @close="showEventForm = false; newEventStart = '';"
      @saved="calendarStore.fetchEvents()"
    />
  </div>

  <!-- Desktop -->
  <div v-else class="calendar-view">
    <div class="calendar-sidebar-pane">
      <CalendarSidebar @calendar-drop="onCalendarDrop" />
    </div>
    <div class="calendar-main">
      <!-- Toolbar -->
      <div class="calendar-toolbar">
        <div class="toolbar-left">
          <button class="btn-new-event" data-testid="cal-btn-new-event" @click="showEventForm = true">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="12" y1="5" x2="12" y2="19" /><line x1="5" y1="12" x2="19" y2="12" /></svg>
            Event
          </button>
          <div class="toolbar-divider"></div>
          <button class="btn-today" data-testid="cal-btn-today" @click="calendarStore.goToday()">Today</button>
          <button class="btn-nav" data-testid="cal-btn-prev" @click="calendarStore.goPrev()">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="15 18 9 12 15 6" /></svg>
          </button>
          <button class="btn-nav" data-testid="cal-btn-next" @click="calendarStore.goNext()">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="9 18 15 12 9 6" /></svg>
          </button>
          <span class="current-date">{{ formatCurrentDate() }}</span>
        </div>
        <div class="toolbar-right">
          <div class="view-toggle">
            <button
              v-for="mode in (['day', 'week', 'month'] as CalendarViewMode[])"
              :key="mode"
              class="view-btn"
              :class="{ active: calendarStore.viewMode === mode }"
              :data-testid="`cal-view-${mode}`"
              @click="calendarStore.setViewMode(mode)"
            >{{ mode.charAt(0).toUpperCase() + mode.slice(1) }}</button>
          </div>
        </div>
      </div>

      <!-- Calendar grid -->
      <div class="calendar-content">
        <WeekView
          v-if="calendarStore.viewMode === 'day' || calendarStore.viewMode === 'week'"
          :single-day="calendarStore.viewMode === 'day'"
          @time-click="onTimeSlotClick"
          @event-click="onEventClick"
          @event-reschedule="onEventReschedule"
        />
        <MonthView
          v-else
          @date-click="(d) => { calendarStore.setViewMode('day'); calendarStore.goToDate(d); }"
          @event-click="onEventClick"
          @event-reschedule="onEventReschedule"
        />
      </div>
    </div>

    <!-- Event detail panel -->
    <EventDetail
      v-if="calendarStore.selectedEvent"
      @close="calendarStore.selectEvent(null)"
    />

    <!-- New event form -->
    <EventForm
      v-if="showEventForm"
      :initial-start="newEventStart || undefined"
      @close="showEventForm = false; newEventStart = '';"
      @saved="calendarStore.fetchEvents()"
    />
  </div>
</template>

<style scoped>
.calendar-view {
  display: flex;
  height: 100%;
  width: 100%;
}

.calendar-sidebar-pane {
  width: 200px;
  flex-shrink: 0;
  border-right: 0.8px solid var(--color-border);
  background: var(--color-bg-secondary);
}

.calendar-main {
  flex: 1;
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.calendar-toolbar {
  display: flex;
  justify-content: space-between;
  align-items: center;
  height: 48px;
  padding: 0 16px;
  border-bottom: 0.8px solid var(--color-border);
  background: var(--color-bg-secondary);
  flex-shrink: 0;
}

.toolbar-left {
  display: flex;
  align-items: center;
  gap: 4px;
}

.toolbar-divider {
  width: 1px;
  height: 24px;
  background: var(--color-border);
  margin: 0 8px;
}

.btn-new-event {
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 6px 16px;
  background: var(--color-accent);
  color: white;
  border-radius: 999px;
  font-size: 14px;
  font-weight: 500;
  transition: background 0.12s;
}

.btn-new-event:hover {
  background: var(--color-accent-hover);
}

.btn-today {
  padding: 5px 14px;
  background: var(--color-bg-tertiary);
  border-radius: 4px;
  font-size: 14px;
  font-weight: 500;
  color: var(--color-text);
}

.btn-today:hover {
  background: var(--color-border);
}

.btn-nav {
  width: 32px;
  height: 32px;
  border-radius: 4px;
  color: var(--color-text-muted);
  display: flex;
  align-items: center;
  justify-content: center;
  transition: background 0.1s;
}

.btn-nav:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.current-date {
  font-size: 14px;
  font-weight: 500;
  color: var(--color-text);
  margin-left: 4px;
}

.view-toggle {
  display: flex;
  background: var(--color-bg-tertiary);
  border-radius: 999px;
  padding: 2px;
}

.view-btn {
  padding: 3px 14px;
  font-size: 14px;
  font-weight: 500;
  color: var(--color-text);
  border-radius: 999px;
  transition: all 0.12s;
}

.view-btn:hover {
  background: var(--color-border);
}

.view-btn.active {
  background: var(--color-accent);
  color: white;
}

.calendar-content {
  flex: 1;
  overflow: hidden;
}

/* ============================================================
   Mobile: Day / Week / Month (§10)
   ============================================================ */
.calendar-view.mobile {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-height: 0;
  background: var(--color-bg);
  overflow: hidden;
  height: 100%;
  width: 100%;
}

.mobile-segmented {
  flex-shrink: 0;
  display: flex;
  gap: 4px;
  padding: 6px 12px;
  background: var(--color-bg);
  border-bottom: 1px solid var(--color-divider, #e9e0cd);
}

.seg-btn {
  flex: 1;
  height: 32px;
  border: 0;
  border-radius: 999px;
  background: var(--color-bg-tertiary);
  font-family: inherit;
  font-size: 13px;
  font-weight: 500;
  color: var(--color-text);
  cursor: pointer;
}

.seg-btn.active {
  background: var(--color-accent);
  color: #fff;
  font-weight: 600;
}

/* --- Day view --- */
.day-view {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  overflow-y: auto;
}

.all-day-strip {
  flex-shrink: 0;
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
  padding: 6px 10px;
  border-bottom: 1px solid var(--color-divider, #e9e0cd);
  background: var(--color-bg-secondary);
}

.all-day-chip {
  border: 0;
  border-radius: 6px;
  padding: 4px 8px;
  font-size: 11px;
  font-weight: 600;
  color: #fff;
  cursor: pointer;
}

.day-grid {
  position: relative;
  display: flex;
  flex: 1;
  min-height: 1056px; /* 24 * 44 */
}

.hour-column {
  width: 48px;
  flex-shrink: 0;
  position: relative;
}

.hour-slot {
  position: relative;
  height: 44px;
}

.hour-label {
  position: absolute;
  top: -7px;
  right: 6px;
  font-size: 10px;
  color: var(--color-text-muted);
}

.event-column {
  position: relative;
  flex: 1;
  min-width: 0;
  background: var(--color-bg);
  border-left: 1px solid var(--color-border);
}

.grid-line {
  position: absolute;
  left: 0;
  right: 0;
  height: 1px;
  background: var(--color-border-soft, var(--color-border));
  opacity: 0.6;
}

.now-dot {
  position: absolute;
  left: -5px;
  width: 10px;
  height: 10px;
  border-radius: 50%;
  background: var(--color-accent);
  z-index: 3;
}

.now-line {
  position: absolute;
  left: 0;
  right: 0;
  height: 2px;
  background: var(--color-accent);
  z-index: 2;
}

.day-event {
  position: absolute;
  left: 6px;
  right: 6px;
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 6px 10px;
  color: #fff;
  border: 0;
  border-radius: 8px;
  box-shadow: inset 0 -3px 0 rgba(0, 0, 0, 0.18);
  text-align: left;
  cursor: pointer;
  overflow: hidden;
}

.day-event-title {
  font-size: 13px;
  font-weight: 600;
  line-height: 1.2;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.day-event-time {
  font-size: 11px;
  opacity: 0.9;
}

/* --- Week view --- */
.week-view {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

.week-strip {
  flex-shrink: 0;
  display: flex;
  justify-content: space-between;
  padding: 8px 12px;
  background: var(--color-bg);
  border-bottom: 1px solid var(--color-divider, #e9e0cd);
}

.week-day {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
  border: 0;
  background: transparent;
  cursor: pointer;
}

.week-dow {
  font-size: 10px;
  letter-spacing: 0.5px;
  text-transform: uppercase;
  color: var(--color-text-muted);
}

.week-date-pill {
  width: 26px;
  height: 26px;
  border-radius: 50%;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 13px;
  font-weight: 600;
  color: var(--color-text);
}

.week-day.today .week-date-pill {
  background: var(--color-accent);
  color: #fff;
}

.week-grid {
  position: relative;
  display: flex;
  flex: 1;
  min-height: 1056px;
  overflow-y: auto;
}

.week-day-column {
  position: relative;
  flex: 1;
  min-width: 0;
  border-left: 1px solid var(--color-border);
}

.week-day-column.today {
  background: #fdf4e7;
}

.week-event {
  position: absolute;
  left: 2px;
  right: 2px;
  border: 2px solid rgba(0, 0, 0, 0.08);
  border-radius: 4px;
  padding: 2px 4px;
  font-size: 10px;
  color: #fff;
  cursor: pointer;
  overflow: hidden;
  text-align: left;
  font-family: inherit;
  font-weight: 500;
}

.week-event-title {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  display: block;
}

/* --- Month view --- */
.month-view {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  overflow-y: auto;
}

.month-dow {
  flex-shrink: 0;
  display: grid;
  grid-template-columns: repeat(7, 1fr);
  padding: 6px 0;
  background: var(--color-bg-secondary);
  font-size: 11px;
  font-weight: 600;
  text-align: center;
  color: var(--color-text-muted);
  border-bottom: 1px solid var(--color-divider, #e9e0cd);
}

.month-grid {
  display: grid;
  grid-template-columns: repeat(7, 1fr);
  grid-auto-rows: 1fr;
  min-height: 52vh;
  gap: 1px;
  background: var(--color-border);
}

.month-cell {
  position: relative;
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 4px 4px 6px;
  background: var(--color-bg);
  border: 0;
  min-height: 52px;
  cursor: pointer;
  text-align: left;
}

.month-cell.not-in-month {
  background: var(--color-bg-secondary);
  opacity: 0.6;
}

.month-date-pill {
  width: 22px;
  height: 22px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-size: 12px;
  font-weight: 600;
  border-radius: 50%;
  color: var(--color-text);
  margin-bottom: 2px;
}

.month-cell.today .month-date-pill {
  background: var(--color-accent);
  color: #fff;
}

.month-event-strip {
  height: 3px;
  border-radius: 2px;
}

.month-more {
  font-size: 10px;
  color: var(--color-text-muted);
}

.month-today {
  flex-shrink: 0;
  padding: 10px 14px 16px;
  border-top: 1px solid var(--color-divider, #e9e0cd);
}

.month-today-label {
  font-size: 11px;
  font-weight: 700;
  letter-spacing: 0.5px;
  text-transform: uppercase;
  color: var(--color-text-muted);
  margin-bottom: 6px;
}

.month-today-empty {
  font-size: 13px;
  color: var(--color-text-muted);
}

.month-today-row {
  display: flex;
  align-items: center;
  gap: 10px;
  width: 100%;
  padding: 6px 4px;
  background: transparent;
  border: 0;
  border-bottom: 1px solid var(--color-border-soft, var(--color-border));
  cursor: pointer;
  text-align: left;
  font-family: inherit;
}

.month-today-row:last-child {
  border-bottom: 0;
}

.month-today-swatch {
  width: 4px;
  height: 24px;
  border-radius: 2px;
  flex-shrink: 0;
}

.month-today-time {
  min-width: 56px;
  font-size: 12px;
  color: var(--color-text-muted);
}

.month-today-title {
  flex: 1;
  min-width: 0;
  font-size: 14px;
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
