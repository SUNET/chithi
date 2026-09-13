import { ref } from "vue";
import { calendarMutationSupport } from "./calendar-mutation-support";
import type { CalendarEvent } from "./types";

/** Shared reactive state for calendar event drag-and-drop. */
export const dragCalendarEvent = ref<CalendarEvent | null>(null);
export const isCalendarDragging = ref(false);

/** Check the original drag payload as well as current store metadata at drop. */
export function canDragCalendarEvent(
  original: CalendarEvent | null,
  current: CalendarEvent | undefined,
): boolean {
  return calendarMutationSupport(original).supported &&
    current?.id === original?.id && calendarMutationSupport(current).supported;
}
