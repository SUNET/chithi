<script setup lang="ts">
import { computed } from "vue";
import { useCalendarStore } from "@/stores/calendar";

const props = defineProps<{
  singleDay?: boolean;
}>();

const emit = defineEmits<{
  timeClick: [dateTime: string];
  eventClick: [eventId: string];
}>();

const calendarStore = useCalendarStore();

const hours = Array.from({ length: 24 }, (_, i) => i);

const days = computed(() => {
  const d = new Date(calendarStore.currentDate);
  if (props.singleDay) {
    return [new Date(d)];
  }
  const start = new Date(d);
  start.setDate(d.getDate() - d.getDay()); // Sunday
  return Array.from({ length: 7 }, (_, i) => {
    const day = new Date(start);
    day.setDate(start.getDate() + i);
    return day;
  });
});

function formatDayHeader(date: Date): string {
  const dayName = date.toLocaleDateString(undefined, { weekday: "short" });
  const dayNum = date.getDate();
  return `${dayName} ${dayNum}`;
}

function formatHour(hour: number): string {
  if (hour === 0) return "12 AM";
  if (hour < 12) return `${hour} AM`;
  if (hour === 12) return "12 PM";
  return `${hour - 12} PM`;
}

function isToday(date: Date): boolean {
  const today = new Date();
  return date.toDateString() === today.toDateString();
}

function getEventsForDayHour(date: Date, hour: number) {
  const slotStart = new Date(date);
  slotStart.setHours(hour, 0, 0, 0);
  const slotEnd = new Date(date);
  slotEnd.setHours(hour + 1, 0, 0, 0);

  return calendarStore.visibleEvents.filter((e) => {
    const eStart = new Date(e.start_time);
    const eEnd = new Date(e.end_time);
    return eStart < slotEnd && eEnd > slotStart && !e.all_day;
  });
}

function getAllDayEvents(date: Date) {
  const dayStr = date.toISOString().split("T")[0];
  return calendarStore.visibleEvents.filter((e) => {
    if (!e.all_day) return false;
    const eStart = e.start_time.split("T")[0];
    const eEnd = e.end_time.split("T")[0];
    return eStart <= dayStr && eEnd >= dayStr;
  });
}

function getEventColor(event: { calendar_id: string }): string {
  const cal = calendarStore.calendars.find((c) => c.id === event.calendar_id);
  return cal?.color || "#4285f4";
}

function getEventStyle(event: { my_status: string | null }): Record<string, string> {
  if (event.my_status === "declined") {
    return { opacity: "0.5", textDecoration: "line-through" };
  }
  if (event.my_status === "tentative") {
    return { borderStyle: "dashed" };
  }
  return {};
}

function onSlotClick(date: Date, hour: number) {
  const dt = new Date(date);
  dt.setHours(hour, 0, 0, 0);
  emit("timeClick", dt.toISOString());
}
</script>

<template>
  <div class="week-view">
    <!-- All-day events banner -->
    <div class="all-day-row">
      <div class="time-gutter all-day-label">All day</div>
      <div
        v-for="day in days"
        :key="day.toISOString()"
        class="all-day-cell"
      >
        <div
          v-for="event in getAllDayEvents(day)"
          :key="event.id"
          class="all-day-event"
          :style="{ backgroundColor: getEventColor(event), ...getEventStyle(event) }"
          @click="emit('eventClick', event.id)"
        >
          {{ event.title }}
        </div>
      </div>
    </div>

    <!-- Day headers -->
    <div class="day-headers">
      <div class="time-gutter"></div>
      <div
        v-for="day in days"
        :key="day.toISOString()"
        class="day-header"
        :class="{ today: isToday(day) }"
      >
        {{ formatDayHeader(day) }}
      </div>
    </div>

    <!-- Time grid -->
    <div class="time-grid">
      <div v-for="hour in hours" :key="hour" class="hour-row">
        <div class="time-gutter time-label">{{ formatHour(hour) }}</div>
        <div
          v-for="day in days"
          :key="day.toISOString() + hour"
          class="time-cell"
          @click="onSlotClick(day, hour)"
        >
          <div
            v-for="event in getEventsForDayHour(day, hour)"
            :key="event.id"
            class="event-block"
            :style="{ backgroundColor: getEventColor(event), ...getEventStyle(event) }"
            @click.stop="emit('eventClick', event.id)"
          >
            <span class="event-time">
              {{ new Date(event.start_time).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }) }}
            </span>
            <span class="event-title">{{ event.title }}</span>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.week-view {
  height: 100%;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

.all-day-row {
  display: flex;
  border-bottom: 1px solid var(--color-border);
  min-height: 28px;
  flex-shrink: 0;
}

.all-day-label {
  font-size: 10px;
  color: var(--color-text-muted);
  display: flex;
  align-items: center;
  justify-content: center;
}

.all-day-cell {
  flex: 1;
  padding: 2px;
  border-left: 1px solid var(--color-border);
  display: flex;
  flex-direction: column;
  gap: 1px;
}

.all-day-event {
  font-size: 11px;
  color: white;
  padding: 1px 4px;
  border-radius: 3px;
  cursor: pointer;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.day-headers {
  display: flex;
  border-bottom: 1px solid var(--color-border);
  flex-shrink: 0;
}

.day-header {
  flex: 1;
  text-align: center;
  padding: 6px 0;
  font-size: 12px;
  color: var(--color-text-secondary);
  border-left: 1px solid var(--color-border);
}

.day-header.today {
  color: var(--color-accent);
  font-weight: 600;
}

.time-gutter {
  width: 60px;
  flex-shrink: 0;
}

.time-grid {
  flex: 1;
  overflow-y: auto;
}

.hour-row {
  display: flex;
  min-height: 48px;
  border-bottom: 1px solid var(--color-border);
}

.time-label {
  font-size: 10px;
  color: var(--color-text-muted);
  padding: 2px 8px 0 0;
  text-align: right;
}

.time-cell {
  flex: 1;
  border-left: 1px solid var(--color-border);
  position: relative;
  cursor: pointer;
  padding: 1px;
}

.time-cell:hover {
  background: var(--color-bg-hover);
}

.event-block {
  font-size: 11px;
  color: white;
  padding: 2px 4px;
  border-radius: 3px;
  cursor: pointer;
  margin-bottom: 1px;
  overflow: hidden;
}

.event-block:hover {
  filter: brightness(0.9);
}

.event-time {
  font-size: 10px;
  opacity: 0.9;
}

.event-title {
  display: block;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
