<script setup lang="ts">
import { computed } from "vue";
import { useCalendarStore } from "@/stores/calendar";

const emit = defineEmits<{
  dateClick: [date: string];
  eventClick: [eventId: string];
}>();

const calendarStore = useCalendarStore();

const weeks = computed(() => {
  const d = new Date(calendarStore.currentDate);
  const year = d.getFullYear();
  const month = d.getMonth();
  const firstDay = new Date(year, month, 1);
  const lastDay = new Date(year, month + 1, 0);

  // Start from Sunday of the first week
  const start = new Date(firstDay);
  start.setDate(start.getDate() - start.getDay());

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
  const dayStr = date.toISOString().split("T")[0];
  return calendarStore.visibleEvents.filter((e) => {
    const eStart = e.start_time.split("T")[0];
    return eStart === dayStr;
  });
}

function getEventColor(event: { calendar_id: string }): string {
  const cal = calendarStore.calendars.find((c) => c.id === event.calendar_id);
  return cal?.color || "#4285f4";
}

const dayNames = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
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
          }"
          @click="emit('dateClick', day.toISOString().split('T')[0])"
        >
          <span class="day-number">{{ day.getDate() }}</span>
          <div class="month-events">
            <div
              v-for="event in getEventsForDay(day).slice(0, 3)"
              :key="event.id"
              class="month-event"
              :data-testid="`cal-event-${event.id}`"
              :style="{ backgroundColor: getEventColor(event) }"
              @click.stop="emit('eventClick', event.id)"
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

.month-more {
  font-size: 10px;
  color: var(--color-text-muted);
  padding: 1px 4px;
}
</style>
