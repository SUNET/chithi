import { isOccurrenceId } from "./rrule";
import type { CalendarEvent } from "./types";

type RecurrenceMetadata = Pick<CalendarEvent, "id"> &
  Partial<Pick<CalendarEvent, "recurrence_kind" | "recurrence_rule">>;

/** Ordinary mutations require positive evidence that this is standalone. */
export function calendarMutationSupport(
  event: RecurrenceMetadata | null | undefined,
): { supported: true; reason: null } | { supported: false; reason: string } {
  if (event && (
    isOccurrenceId(event.id) ||
    event.recurrence_kind === "series" ||
    event.recurrence_kind === "occurrence" ||
    !!event.recurrence_rule
  )) {
    return {
      supported: false,
      reason: "Editing, deleting, or moving recurring events is not supported in Chithi.",
    };
  }
  if (event?.recurrence_kind !== "standalone") {
    return {
      supported: false,
      reason: "Recurrence information is unavailable. Refresh this event before editing.",
    };
  }
  return { supported: true, reason: null };
}
