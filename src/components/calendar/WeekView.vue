<script setup lang="ts">
import { computed, onMounted, ref, nextTick } from "vue";
import { useCalendarStore } from "@/stores/calendar";
import type { CalendarEvent } from "@/lib/types";

const props = defineProps<{
  singleDay?: boolean;
}>();

const emit = defineEmits<{
  timeClick: [dateTime: string];
  eventClick: [eventId: string];
}>();

const calendarStore = useCalendarStore();
const gridRef = ref<HTMLElement | null>(null);

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

const now = ref(new Date());

onMounted(() => {
  now.value = new Date(); // Update when view is shown
});

setInterval(() => { now.value = new Date(); }, 60000);

function formatDayName(date: Date): string {
  return date.toLocaleDateString(undefined, { weekday: "short" }).toUpperCase();
}

function formatDayNum(date: Date): string {
  return String(date.getDate());
}

function formatHour(hour: number): string {
  if (hour === 0) return "12 AM";
  if (hour < 12) return `${hour} AM`;
  if (hour === 12) return "12 PM";
  return `${hour - 12} PM`;
}

function isToday(date: Date): boolean {
  return date.toDateString() === now.value.toDateString();
}

function isWeekend(date: Date): boolean {
  return date.getDay() === 0 || date.getDay() === 6;
}

function isCurrentHour(hour: number): boolean {
  const todayVisible = days.value.some((d) => isToday(d));
  return todayVisible && now.value.getHours() === hour;
}

// Minutes past the hour as percentage (0-100) for positioning the time line
function currentMinutePercent(): string {
  return `${(now.value.getMinutes() / 60) * 100}%`;
}

// A display segment represents one day's portion of an event.
// Cross-midnight events are split so each day gets its own segment.
interface EventSegment {
  event: CalendarEvent;
  segStart: Date;  // clamped to day start if event started before this day
  segEnd: Date;    // clamped to day end (midnight) if event continues next day
}

function getSegmentsForDay(date: Date): EventSegment[] {
  const dayStart = new Date(date);
  dayStart.setHours(0, 0, 0, 0);
  const dayEnd = new Date(date);
  dayEnd.setHours(23, 59, 59, 999);

  const segments: EventSegment[] = [];
  for (const e of calendarStore.visibleEvents) {
    const eStart = new Date(e.start_time);
    const eEnd = new Date(e.end_time);
    // Skip all-day and multi-day (>24h) events — handled by getAllDayEvents
    if (e.all_day || (eEnd.getTime() - eStart.getTime() > 24 * 60 * 60 * 1000)) continue;
    // Check overlap with this day
    if (eStart > dayEnd || eEnd <= dayStart) continue;
    // Clamp to this day's boundaries
    const segStart = eStart < dayStart ? dayStart : eStart;
    const segEnd = eEnd > dayEnd ? new Date(dayEnd.getTime() + 1) : eEnd; // midnight = 00:00 next day
    segments.push({ event: e, segStart, segEnd });
  }
  return segments;
}

// Return segments whose start hour matches this slot (one render per segment)
function getEventsForDayHour(date: Date, hour: number) {
  return getSegmentsForDay(date).filter((s) => s.segStart.getHours() === hour);
}

const HOUR_HEIGHT = 52; // must match .hour-row min-height in CSS

function eventBlockStyle(seg: EventSegment): Record<string, string> {
  const durationMs = seg.segEnd.getTime() - seg.segStart.getTime();
  const durationHours = Math.max(durationMs / (60 * 60 * 1000), 0.25);
  const topOffset = (seg.segStart.getMinutes() / 60) * HOUR_HEIGHT;
  const height = durationHours * HOUR_HEIGHT;

  const style: Record<string, string> = {
    position: "absolute",
    top: `${topOffset}px`,
    height: `${height}px`,
    left: "2px",
    right: "2px",
    zIndex: "2",
    backgroundColor: getEventColor(seg.event),
  };

  Object.assign(style, getEventStyle(seg.event));
  return style;
}

function getAllDayEvents(date: Date) {
  const dayStr = date.toISOString().split("T")[0];
  return calendarStore.visibleEvents.filter((e) => {
    const eStart = new Date(e.start_time);
    const eEnd = new Date(e.end_time);
    const isMultiDay = eEnd.getTime() - eStart.getTime() > 24 * 60 * 60 * 1000;
    if (!e.all_day && !isMultiDay) return false;
    const startDate = e.start_time.split("T")[0];
    const endDate = e.end_time.split("T")[0];
    return startDate <= dayStr && endDate >= dayStr;
  });
}

function getEventColor(event: { calendar_id: string }): string {
  const cal = calendarStore.calendars.find((c) => c.id === event.calendar_id);
  return cal?.color || "#4285f4";
}

function getEventStyle(event: { my_status: string | null }): Record<string, string> {
  if (event.my_status === "declined") {
    return { opacity: "0.4", textDecoration: "line-through" };
  }
  if (event.my_status === "tentative") {
    return { borderLeft: "3px dashed rgba(255,255,255,0.6)" };
  }
  return {};
}

function onSlotClick(date: Date, hour: number) {
  const dt = new Date(date);
  dt.setHours(hour, 0, 0, 0);
  emit("timeClick", dt.toISOString());
}

// Scroll to current hour (or 8 AM if before that) on mount
onMounted(async () => {
  now.value = new Date();
  await nextTick();
  if (gridRef.value) {
    const hourHeight = 52;
    const scrollToHour = Math.max(now.value.getHours() - 2, 0);
    gridRef.value.scrollTop = hourHeight * scrollToHour;
  }
});
</script>

<template>
  <div class="week-view" :data-testid="singleDay ? 'cal-day-view' : 'cal-week-view'">
    <!-- All-day events banner -->
    <div class="all-day-row">
      <div class="time-gutter all-day-label">all-day</div>
      <div
        v-for="day in days"
        :key="day.toISOString() + 'ad'"
        class="all-day-cell"
        :class="{ today: isToday(day) }"
      >
        <div
          v-for="event in getAllDayEvents(day)"
          :key="event.id"
          class="all-day-event"
          :data-testid="`cal-event-${event.id}`"
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
        :key="day.toISOString() + 'h'"
        class="day-header"
        :class="{ today: isToday(day), weekend: isWeekend(day) }"
      >
        <span class="day-name">{{ formatDayName(day) }}</span>
        <span class="day-num" :class="{ 'today-badge': isToday(day) }">{{ formatDayNum(day) }}</span>
      </div>
    </div>

    <!-- Time grid -->
    <div ref="gridRef" class="time-grid">
      <div v-for="hour in hours" :key="hour" class="hour-row" :class="{ 'current-hour': isCurrentHour(hour) }">
        <div class="time-gutter time-label">
          <span v-if="hour > 0">{{ formatHour(hour) }}</span>
        </div>
        <div
          v-for="day in days"
          :key="day.toISOString() + hour"
          class="time-cell"
          :class="{ today: isToday(day), weekend: isWeekend(day) }"
          @click="onSlotClick(day, hour)"
        >
          <!-- Current time marker positioned within the hour -->
          <div
            v-if="isCurrentHour(hour) && isToday(day)"
            class="now-marker"
            :style="{ top: currentMinutePercent() }"
          ></div>
          <div
            v-for="seg in getEventsForDayHour(day, hour)"
            :key="seg.event.id + '-' + seg.segStart.toISOString()"
            class="event-block"
            :data-testid="`cal-event-${seg.event.id}`"
            :style="eventBlockStyle(seg)"
            @click.stop="emit('eventClick', seg.event.id)"
          >
            <span class="event-time">
              {{ seg.segStart.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }) }}
            </span>
            <span class="event-title">{{ seg.event.title }}</span>
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
  background: var(--color-bg);
}

/* All-day row */
.all-day-row {
  display: flex;
  border-bottom: 2px solid var(--color-border);
  min-height: 32px;
  flex-shrink: 0;
}

.all-day-label {
  font-size: 10px;
  color: var(--color-text-muted);
  display: flex;
  align-items: center;
  justify-content: flex-end;
  padding-right: 12px;
  text-transform: lowercase;
  letter-spacing: 0.3px;
}

.all-day-cell {
  flex: 1;
  padding: 3px 2px;
  border-left: 1px solid var(--color-border);
  display: flex;
  flex-direction: column;
  gap: 1px;
}

.all-day-cell.today {
  background: rgba(66, 133, 244, 0.04);
}

.all-day-event {
  font-size: 11px;
  color: white;
  padding: 2px 6px;
  border-radius: 4px;
  cursor: pointer;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-weight: 500;
}

/* Day headers — add right padding to compensate for time-grid scrollbar */
.day-headers {
  display: flex;
  border-bottom: 1px solid var(--color-border);
  flex-shrink: 0;
  background: var(--color-bg);
  padding-right: 6px; /* scrollbar width compensation */
}

.all-day-row {
  padding-right: 6px; /* scrollbar width compensation */
}

.day-header {
  flex: 1;
  text-align: center;
  padding: 8px 0 10px;
  border-left: 1px solid var(--color-border);
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 2px;
}

.day-name {
  font-size: 10px;
  font-weight: 600;
  color: var(--color-text-muted);
  letter-spacing: 0.8px;
}

.day-header.today .day-name {
  color: var(--color-accent);
}

.day-header.weekend .day-name {
  color: var(--color-text-muted);
  opacity: 0.7;
}

.day-num {
  font-size: 22px;
  font-weight: 300;
  color: var(--color-text-secondary);
  line-height: 1;
}

.day-num.today-badge {
  background: var(--color-accent);
  color: white;
  width: 36px;
  height: 36px;
  border-radius: 50%;
  display: flex;
  align-items: center;
  justify-content: center;
  font-weight: 500;
  font-size: 18px;
}

.day-header.weekend .day-num {
  opacity: 0.6;
}

/* Time gutter */
.time-gutter {
  width: 64px;
  flex-shrink: 0;
}

/* Time grid */
.time-grid {
  flex: 1;
  overflow-y: scroll;
}

.hour-row {
  display: flex;
  min-height: 52px;
  border-bottom: 1px solid color-mix(in srgb, var(--color-border) 50%, transparent);
}

.hour-row:nth-child(even) {
  /* subtle alternating stripe */
}

.time-label {
  font-size: 10px;
  color: var(--color-text-muted);
  padding: 0 12px 0 0;
  text-align: right;
  position: relative;
  top: -7px;
  line-height: 1;
}

.time-cell {
  flex: 1;
  border-left: 1px solid color-mix(in srgb, var(--color-border) 50%, transparent);
  position: relative;
  cursor: pointer;
  padding: 0;
  transition: background 0.1s;
  overflow: visible;
}

.time-cell:hover {
  background: var(--color-bg-hover);
}

.time-cell.today {
  background: rgba(66, 133, 244, 0.03);
}

.time-cell.today:hover {
  background: rgba(66, 133, 244, 0.07);
}

.time-cell.weekend {
  background: color-mix(in srgb, var(--color-bg-tertiary) 30%, transparent);
}

/* Current hour highlight */
.hour-row.current-hour .time-label {
  color: #ea4335;
  font-weight: 700;
}

.now-marker {
  position: absolute;
  left: -1px;
  right: 0;
  height: 2px;
  background: #ea4335;
  z-index: 5;
  pointer-events: none;
}

.now-marker::before {
  content: "";
  position: absolute;
  left: -5px;
  top: -4px;
  width: 10px;
  height: 10px;
  background: #ea4335;
  border-radius: 50%;
}

/* Event blocks — absolutely positioned within time-cell to span duration */
.event-block {
  position: absolute;
  font-size: 11px;
  color: white;
  padding: 3px 6px;
  border-radius: 4px;
  cursor: pointer;
  overflow: hidden;
  line-height: 1.3;
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.1);
  transition: box-shadow 0.15s, transform 0.1s;
  box-sizing: border-box;
}

.event-block:hover {
  box-shadow: 0 2px 6px rgba(0, 0, 0, 0.2);
  transform: translateY(-1px);
}

.event-time {
  font-size: 10px;
  opacity: 0.85;
  font-weight: 500;
}

.event-title {
  display: block;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-weight: 500;
}

/* Scrollbar */
.time-grid::-webkit-scrollbar {
  width: 6px;
}

.time-grid::-webkit-scrollbar-thumb {
  background: var(--color-border);
  border-radius: 3px;
}
</style>
