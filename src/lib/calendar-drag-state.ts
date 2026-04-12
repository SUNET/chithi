import { ref } from "vue";
import type { CalendarEvent } from "./types";

/** Shared reactive state for calendar event drag-and-drop. */
export const dragCalendarEvent = ref<CalendarEvent | null>(null);
export const isCalendarDragging = ref(false);
