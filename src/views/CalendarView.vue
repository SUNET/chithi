<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useCalendarStore } from "@/stores/calendar";
import type { CalendarViewMode } from "@/stores/calendar";
import CalendarSidebar from "@/components/calendar/CalendarSidebar.vue";
import WeekView from "@/components/calendar/WeekView.vue";
import MonthView from "@/components/calendar/MonthView.vue";
import EventDetail from "@/components/calendar/EventDetail.vue";
import EventForm from "@/components/calendar/EventForm.vue";

const calendarStore = useCalendarStore();
const showEventForm = ref(false);
const newEventStart = ref("");

function formatCurrentDate(): string {
  const d = new Date(calendarStore.currentDate);
  if (calendarStore.viewMode === "month") {
    return d.toLocaleDateString(undefined, { month: "long", year: "numeric" });
  }
  if (calendarStore.viewMode === "week") {
    const start = new Date(d);
    start.setDate(d.getDate() - d.getDay());
    const end = new Date(start);
    end.setDate(start.getDate() + 6);
    return `${start.toLocaleDateString(undefined, { month: "short", day: "numeric" })} - ${end.toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" })}`;
  }
  return d.toLocaleDateString(undefined, { weekday: "long", month: "long", day: "numeric", year: "numeric" });
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
  // Sync calendars from server, then fetch local data
  await calendarStore.syncCalendars();
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
          <button class="btn-new-event" @click="showEventForm = true">+ Event</button>
          <button class="btn-today" @click="calendarStore.goToday()">Today</button>
          <button class="btn-nav" @click="calendarStore.goPrev()">&lsaquo;</button>
          <button class="btn-nav" @click="calendarStore.goNext()">&rsaquo;</button>
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
  border-right: 1px solid var(--color-border);
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
  padding: 8px 16px;
  border-bottom: 1px solid var(--color-border);
  background: var(--color-bg-secondary);
  flex-shrink: 0;
}

.toolbar-left {
  display: flex;
  align-items: center;
  gap: 8px;
}

.btn-new-event {
  padding: 4px 12px;
  background: var(--color-accent);
  color: var(--color-bg);
  border-radius: 4px;
  font-size: 12px;
  font-weight: 600;
}

.btn-new-event:hover {
  background: var(--color-accent-hover);
}

.btn-today {
  padding: 4px 12px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  font-size: 12px;
  color: var(--color-text-secondary);
}

.btn-today:hover {
  background: var(--color-bg-hover);
}

.btn-nav {
  width: 28px;
  height: 28px;
  border-radius: 50%;
  font-size: 18px;
  color: var(--color-text-secondary);
  display: flex;
  align-items: center;
  justify-content: center;
}

.btn-nav:hover {
  background: var(--color-bg-hover);
}

.current-date {
  font-size: 14px;
  font-weight: 600;
  color: var(--color-text);
}

.view-toggle {
  display: flex;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  overflow: hidden;
}

.view-btn {
  padding: 4px 12px;
  font-size: 12px;
  color: var(--color-text-secondary);
  border-right: 1px solid var(--color-border);
}

.view-btn:last-child {
  border-right: none;
}

.view-btn:hover {
  background: var(--color-bg-hover);
}

.view-btn.active {
  background: var(--color-accent);
  color: var(--color-bg);
}

.calendar-content {
  flex: 1;
  overflow: hidden;
}
</style>
