<script setup lang="ts">
import { computed, ref, onUnmounted } from "vue";
import { useCalendarStore } from "@/stores/calendar";
import { useUiStore } from "@/stores/ui";
import type { CalendarEvent } from "@/lib/types";
import { dragCalendarEvent, isCalendarDragging } from "@/lib/calendar-drag-state";

const emit = defineEmits<{
  dateClick: [date: string];
  eventClick: [eventId: string];
  eventReschedule: [payload: {
    eventId: string;
    newStart: string;
    newEnd: string;
    attendeesJson: string | null;
    organizerEmail: string | null;
  }];
}>();

const calendarStore = useCalendarStore();
const uiStore = useUiStore();

const weeks = computed(() => {
  const d = new Date(calendarStore.currentDate);
  const year = d.getFullYear();
  const month = d.getMonth();
  const firstDay = new Date(year, month, 1);
  const lastDay = new Date(year, month + 1, 0);

  // Start from the configured week start day
  const start = new Date(firstDay);
  const offset = (start.getDay() - uiStore.weekStartDay + 7) % 7;
  start.setDate(start.getDate() - offset);

  const rows: Date[][] = [];
  const current = new Date(start);
  while (current <= lastDay || rows.length < 5) {
    const week: Date[] = [];
    for (let i = 0; i < 7; i++) {
      week.push(new Date(current));
      current.setDate(current.getDate() + 1);
    }
    rows.push(week);
    if (rows.length >= 6) break;
  }
  return rows;
});

const currentMonth = computed(() => new Date(calendarStore.currentDate).getMonth());

function isToday(date: Date): boolean {
  return date.toDateString() === new Date().toDateString();
}

function isCurrentMonth(date: Date): boolean {
  return date.getMonth() === currentMonth.value;
}

function getEventsForDay(date: Date) {
  const dayStart = new Date(date);
  dayStart.setHours(0, 0, 0, 0);
  const dayEnd = new Date(date);
  dayEnd.setHours(23, 59, 59, 999);
  return calendarStore.visibleEvents.filter((e) => {
    const eStart = new Date(e.start_time);
    const eEnd = new Date(e.end_time);
    // Overlap check with exclusive end — events ending exactly at midnight
    // don't spill onto the next day
    return eStart <= dayEnd && eEnd > dayStart;
  });
}

function getEventColor(event: { calendar_id: string }): string {
  const cal = calendarStore.calendars.find((c) => c.id === event.calendar_id);
  return cal?.color || "#4285f4";
}

const allDayNames = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const dayNames = computed(() => {
  const s = uiStore.weekStartDay;
  return [...allDayNames.slice(s), ...allDayNames.slice(0, s)];
});

// Drag-to-reschedule
const dragStartPos = ref<{ x: number; y: number } | null>(null);
const dragGhost = ref<HTMLElement | null>(null);
const dragOverDay = ref<string | null>(null);
const DRAG_THRESHOLD = 5;
let dragCleanup: (() => void) | null = null;

function onEventMouseDown(event: MouseEvent, ev: CalendarEvent) {
  if (event.button !== 0) return;
  if (/_\d{4}-/.test(ev.id) && ev.recurrence_rule) return;
  if (ev.all_day) return;

  dragStartPos.value = { x: event.clientX, y: event.clientY };
  const sourceEvent = ev;

  const handleMove = (e: MouseEvent) => {
    if (!dragStartPos.value) return;
    const dx = e.clientX - dragStartPos.value.x;
    const dy = e.clientY - dragStartPos.value.y;
    if (!isCalendarDragging.value && Math.sqrt(dx * dx + dy * dy) < DRAG_THRESHOLD) return;

    if (!isCalendarDragging.value) {
      dragCalendarEvent.value = sourceEvent;
      isCalendarDragging.value = true;
      const ghost = document.createElement("div");
      ghost.textContent = sourceEvent.title;
      ghost.dataset.testid = "cal-drag-ghost";
      ghost.style.cssText = "position:fixed;z-index:99999;padding:4px 10px;background:#3366cc;color:white;border-radius:4px;font-size:12px;font-weight:500;white-space:nowrap;pointer-events:none;";
      document.body.appendChild(ghost);
      dragGhost.value = ghost;
      document.body.style.cursor = "grabbing";
    }

    if (dragGhost.value) {
      dragGhost.value.style.left = e.clientX + 12 + "px";
      dragGhost.value.style.top = e.clientY + 12 + "px";
    }
  };

  const handleUp = () => {
    document.body.style.cursor = "";
    if (isCalendarDragging.value) {
      setTimeout(() => {
        isCalendarDragging.value = false;
        dragCalendarEvent.value = null;
        dragOverDay.value = null;
        if (dragGhost.value) {
          dragGhost.value.remove();
          dragGhost.value = null;
        }
      }, 0);
    }
    dragStartPos.value = null;
    document.removeEventListener("mousemove", handleMove);
    document.removeEventListener("mouseup", handleUp);
    dragCleanup = null;
  };

  document.addEventListener("mousemove", handleMove);
  document.addEventListener("mouseup", handleUp);
  dragCleanup = handleUp;
}

onUnmounted(() => {
  if (dragCleanup) dragCleanup();
});

function onCellEnter(day: Date) {
  if (!isCalendarDragging.value) return;
  dragOverDay.value = day.toISOString().split("T")[0];
}

function onCellLeave(day: Date) {
  if (dragOverDay.value === day.toISOString().split("T")[0]) {
    dragOverDay.value = null;
  }
}

function onCellDrop(day: Date) {
  if (!isCalendarDragging.value || !dragCalendarEvent.value) return;
  dragOverDay.value = null;

  const ev = dragCalendarEvent.value;
  const originalStart = new Date(ev.start_time);
  const originalEnd = new Date(ev.end_time);
  const durationMs = originalEnd.getTime() - originalStart.getTime();

  // Keep same time-of-day, change the date
  const newStart = new Date(day);
  newStart.setHours(originalStart.getHours(), originalStart.getMinutes(), 0, 0);
  const newEnd = new Date(newStart.getTime() + durationMs);

  if (newStart.getTime() === originalStart.getTime()) return;

  emit("eventReschedule", {
    eventId: ev.id,
    newStart: newStart.toISOString(),
    newEnd: newEnd.toISOString(),
    attendeesJson: ev.attendees_json,
    organizerEmail: ev.organizer_email,
  });
}
</script>

<template>
  <div class="month-view" data-testid="cal-month-view">
    <div class="month-header">
      <div v-for="name in dayNames" :key="name" class="month-day-name">{{ name }}</div>
    </div>
    <div class="month-grid">
      <div v-for="(week, wi) in weeks" :key="wi" class="month-week">
        <div
          v-for="day in week"
          :key="day.toISOString()"
          class="month-cell"
          :class="{
            today: isToday(day),
            'other-month': !isCurrentMonth(day),
            'drag-over': isCalendarDragging && dragOverDay === day.toISOString().split('T')[0],
          }"
          :data-testid="`cal-month-cell-${day.toISOString().split('T')[0]}`"
          @click="emit('dateClick', day.toISOString().split('T')[0])"
          @mouseenter="onCellEnter(day)"
          @mouseleave="onCellLeave(day)"
          @mouseup="onCellDrop(day)"
        >
          <span class="day-number">{{ day.getDate() }}</span>
          <div class="month-events">
            <div
              v-for="event in getEventsForDay(day).slice(0, 3)"
              :key="event.id"
              class="month-event"
              :class="{ dragging: isCalendarDragging && dragCalendarEvent?.id === event.id }"
              :data-testid="`cal-event-${event.id}`"
              :style="{ backgroundColor: getEventColor(event) }"
              @click.stop="emit('eventClick', event.id)"
              @mousedown="onEventMouseDown($event, event)"
            >
              {{ event.title }}
            </div>
            <div
              v-if="getEventsForDay(day).length > 3"
              class="month-more"
            >
              +{{ getEventsForDay(day).length - 3 }} more
            </div>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.month-view {
  height: 100%;
  display: flex;
  flex-direction: column;
}

.month-header {
  display: flex;
  border-bottom: 1px solid var(--color-border);
  flex-shrink: 0;
}

.month-day-name {
  flex: 1;
  text-align: center;
  padding: 6px 0;
  font-size: 11px;
  font-weight: 600;
  color: var(--color-text-muted);
  text-transform: uppercase;
}

.month-grid {
  flex: 1;
  display: flex;
  flex-direction: column;
}

.month-week {
  display: flex;
  flex: 1;
  border-bottom: 1px solid var(--color-border);
}

.month-cell {
  flex: 1;
  border-right: 1px solid var(--color-border);
  padding: 4px;
  cursor: pointer;
  overflow: hidden;
  min-height: 80px;
}

.month-cell:last-child {
  border-right: none;
}

.month-cell:hover {
  background: var(--color-bg-hover);
}

.month-cell.today .day-number {
  background: var(--color-accent);
  color: white;
  border-radius: 50%;
  width: 24px;
  height: 24px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
}

.month-cell.other-month {
  opacity: 0.4;
}

.month-cell.drag-over {
  background: rgba(66, 133, 244, 0.15);
  outline: 1px dashed var(--color-accent);
  outline-offset: -1px;
}

.day-number {
  font-size: 12px;
  color: var(--color-text-secondary);
  margin-bottom: 2px;
}

.month-events {
  display: flex;
  flex-direction: column;
  gap: 1px;
}

.month-event {
  font-size: 10px;
  color: white;
  padding: 1px 4px;
  border-radius: 2px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  cursor: pointer;
}

.month-event.dragging {
  opacity: 0.4;
  pointer-events: none;
}

.month-more {
  font-size: 10px;
  color: var(--color-text-muted);
  padding: 1px 4px;
}
</style>
