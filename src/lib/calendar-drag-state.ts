import { ref } from "vue";
import { calendarScopedEditSupport } from "./calendar-mutation-support";
import type { CalendarEvent } from "./types";

export const dragCalendarEvent = ref<CalendarEvent | null>(null);
export const isCalendarDragging = ref(false);

/** Revalidate store metadata so a stale drag payload cannot authorize a drop. */
export function canDragCalendarEvent(
  draggedEvent: CalendarEvent | null,
  currentEvent: CalendarEvent | undefined,
): boolean {
  return (
    calendarScopedEditSupport(draggedEvent).supported &&
    currentEvent?.id === draggedEvent?.id &&
    calendarScopedEditSupport(currentEvent).supported
  );
}
