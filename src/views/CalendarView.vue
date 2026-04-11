<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useCalendarStore } from "@/stores/calendar";
import { useAccountsStore } from "@/stores/accounts";
import type { CalendarViewMode } from "@/stores/calendar";
import CalendarSidebar from "@/components/calendar/CalendarSidebar.vue";
import WeekView from "@/components/calendar/WeekView.vue";
import MonthView from "@/components/calendar/MonthView.vue";
import EventDetail from "@/components/calendar/EventDetail.vue";
import EventForm from "@/components/calendar/EventForm.vue";

const calendarStore = useCalendarStore();
const accountsStore = useAccountsStore();
const showEventForm = ref(false);
const newEventStart = ref("");

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

onMounted(async () => {
  // Ensure accounts are loaded — App.vue loads them but it may not be done yet
  if (accountsStore.accounts.length === 0) {
    await accountsStore.fetchAccounts();
  }
  // Show cached data immediately (calendars + events live in SQLite),
  // then refresh from the server in the background. Waiting on the
  // network sync here makes the view appear empty until sync finishes.
  await calendarStore.fetchCalendars();
  await calendarStore.fetchEvents();
  calendarStore.syncCalendars().catch((e) => {
    console.error("Calendar sync error:", e);
  });
});
</script>

<template>
  <div class="calendar-view">
    <div class="calendar-sidebar-pane">
      <CalendarSidebar />
    </div>
    <div class="calendar-main">
      <!-- Toolbar -->
      <div class="calendar-toolbar">
        <div class="toolbar-left">
          <button class="btn-new-event" @click="showEventForm = true">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="12" y1="5" x2="12" y2="19" /><line x1="5" y1="12" x2="19" y2="12" /></svg>
            Event
          </button>
          <div class="toolbar-divider"></div>
          <button class="btn-today" @click="calendarStore.goToday()">Today</button>
          <button class="btn-nav" @click="calendarStore.goPrev()">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="15 18 9 12 15 6" /></svg>
          </button>
          <button class="btn-nav" @click="calendarStore.goNext()">
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
        />
        <MonthView
          v-else
          @date-click="(d) => { calendarStore.setViewMode('day'); calendarStore.goToDate(d); }"
          @event-click="onEventClick"
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
</style>
