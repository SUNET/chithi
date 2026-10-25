import { isOccurrenceId } from "./rrule";
import type { CalendarEvent } from "./types";

type RecurrenceMetadata = Pick<CalendarEvent, "id"> &
  Partial<
    Pick<CalendarEvent, "recurrence_kind" | "recurrence_rule" | "attendees_json">
  >;

type SupportResult<Details extends object = object> = Details &
  (
    | { supported: true; reason: null }
    | { supported: false; reason: string }
  );

export type CalendarEditScope = "standalone" | "this-occurrence" | "entire-series";

const RECURRING_REASON =
  "Editing, deleting, or moving recurring events is not supported in Chithi.";
const MEETING_OCCURRENCE_REASON =
  "Editing one occurrence of a meeting with attendees is not supported yet.";
const UNKNOWN_REASON =
  "Recurrence information is unavailable. Refresh this event before editing.";

function hasNoAttendees(attendeesJson: string | null | undefined): boolean {
  try {
    const attendees = JSON.parse(attendeesJson || "[]");
    return Array.isArray(attendees) && attendees.length === 0;
  } catch {
    return false;
  }
}

/** Ordinary mutations require positive evidence that this is standalone. */
export function calendarMutationSupport(
  event: RecurrenceMetadata | null | undefined,
): SupportResult {
  if (
    event &&
    (isOccurrenceId(event.id) ||
      event.recurrence_kind === "series" ||
      event.recurrence_kind === "occurrence" ||
      Boolean(event.recurrence_rule))
  ) {
    return { supported: false, reason: RECURRING_REASON };
  }
  if (event?.recurrence_kind !== "standalone") {
    return { supported: false, reason: UNKNOWN_REASON };
  }
  return { supported: true, reason: null };
}

/** Editing additionally permits an attendee-free occurrence. */
export function calendarEditSupport(
  event: RecurrenceMetadata | null | undefined,
): SupportResult<{ occurrence: boolean }> {
  const ordinary = calendarMutationSupport(event);
  if (ordinary.supported) {
    return { supported: true, occurrence: false, reason: null };
  }
  const occurrence =
    !!event &&
    (isOccurrenceId(event.id) || event.recurrence_kind === "occurrence");
  if (!occurrence) {
    return { supported: false, occurrence: false, reason: ordinary.reason };
  }
  if (!hasNoAttendees(event?.attendees_json)) {
    return {
      supported: false,
      occurrence: true,
      reason: MEETING_OCCURRENCE_REASON,
    };
  }
  return { supported: true, occurrence: true, reason: null };
}

/** Scoped actions may edit a trustworthy occurrence or its complete series. */
export function calendarScopedEditSupport(
  event: RecurrenceMetadata | null | undefined,
): SupportResult<{ scopes: CalendarEditScope[] }> {
  const ordinary = calendarMutationSupport(event);
  if (ordinary.supported) {
    return { supported: true, scopes: ["standalone"], reason: null };
  }
  if (!event || event.recurrence_kind === "unknown") {
    return { supported: false, scopes: [], reason: UNKNOWN_REASON };
  }
  const hasOccurrenceId = isOccurrenceId(event.id);
  if (event.recurrence_kind === "series" && !hasOccurrenceId) {
    return { supported: true, scopes: ["entire-series"], reason: null };
  }
  if (
    !hasOccurrenceId ||
    (event.recurrence_kind !== "occurrence" &&
      event.recurrence_kind !== "series")
  ) {
    return { supported: false, scopes: [], reason: RECURRING_REASON };
  }
  const scopes: CalendarEditScope[] = hasNoAttendees(event.attendees_json)
    ? ["this-occurrence", "entire-series"]
    : ["entire-series"];
  return { supported: true, scopes, reason: null };
}
