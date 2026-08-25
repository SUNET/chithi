import { computed, ref } from "vue";
import { isOccurrenceId } from "./rrule";
import type { CalendarEvent } from "./types";

/** Shared reactive state for calendar event drag-and-drop. */
export const dragCalendarEvent = ref<CalendarEvent | null>(null);
export const isCalendarDragging = ref(false);

/**
 * True when the dragged event is an expanded occurrence of a recurring
 * series. Those may be dropped on a sidebar calendar (moves the whole
 * series) but not on the grid — per-occurrence exceptions don't exist in
 * the schema, so rescheduling one occurrence would rewrite the series.
 */
export const isDraggingSeriesOccurrence = computed(
  () =>
    !!dragCalendarEvent.value?.recurrence_rule &&
    isOccurrenceId(dragCalendarEvent.value.id),
);
