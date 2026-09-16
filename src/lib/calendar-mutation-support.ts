import { isOccurrenceId } from "./rrule";
import type { CalendarEvent } from "./types";

type RecurrenceMetadata = Pick<CalendarEvent, "id"> &
  Partial<Pick<CalendarEvent,
    "recurrence_kind" | "recurrence_rule" | "attendees_json">>;

const recurringReason =
  "Editing, deleting, or moving recurring events is not supported in Chithi.";
const meetingOccurrenceReason =
  "Editing one occurrence of a meeting with attendees is not supported yet.";
const unknownReason =
  "Recurrence information is unavailable. Refresh this event before editing.";

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
      reason: recurringReason,
    };
  }
  if (event?.recurrence_kind !== "standalone") {
    return {
      supported: false,
      reason: unknownReason,
    };
  }
  return { supported: true, reason: null };
}

/** Editing is narrower than delete/move: one attendee-free occurrence is safe. */
export function calendarEditSupport(
  event: RecurrenceMetadata | null | undefined,
): { supported: true; occurrence: boolean; reason: null } |
   { supported: false; occurrence: boolean; reason: string } {
  const ordinary = calendarMutationSupport(event);
  if (ordinary.supported) {
    return { supported: true, occurrence: false, reason: null };
  }
  const occurrence = !!event && (
    isOccurrenceId(event.id) || event.recurrence_kind === "occurrence"
  );
  if (!occurrence) {
    return { supported: false, occurrence: false, reason: ordinary.reason };
  }
  try {
    const attendees = JSON.parse(event?.attendees_json || "[]");
    if (!Array.isArray(attendees) || attendees.length > 0) {
      return { supported: false, occurrence: true, reason: meetingOccurrenceReason };
    }
  } catch {
    return { supported: false, occurrence: true, reason: meetingOccurrenceReason };
  }
  return { supported: true, occurrence: true, reason: null };
}
