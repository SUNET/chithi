import type { CalendarEvent } from "./types";
import { getDateInTimezone } from "./datetime";

/** Calendar dates are civil days, not UTC instants. */
export function parseCalendarDay(day: string): Date {
  const [year, month, date] = day.split("-").map(Number);
  return new Date(year, month - 1, date);
}

export function calendarDay(date: Date): string {
  return [
    date.getFullYear(),
    String(date.getMonth() + 1).padStart(2, "0"),
    String(date.getDate()).padStart(2, "0"),
  ].join("-");
}

export function addCalendarDays(day: string, days: number): string {
  const date = parseCalendarDay(day);
  date.setDate(date.getDate() + days);
  return calendarDay(date);
}

export function eventOccursOnDay(
  event: Pick<CalendarEvent, "start_time" | "end_time" | "all_day">,
  day: string,
  timezone: string,
): boolean {
  if (event.all_day) {
    return event.start_time <= day && day < event.end_time;
  }
  const start = getDateInTimezone(event.start_time, timezone);
  const lastInstant = Math.max(Date.parse(event.start_time), Date.parse(event.end_time) - 1);
  const end = getDateInTimezone(new Date(lastInstant).toISOString(), timezone);
  return start <= day && day <= end;
}

/** The desktop grid has 5–6 rows; mobile always has six. */
export function monthGridDays(
  day: string,
  weekStartDay: number,
  fixedSixWeeks = false,
): Date[] {
  const anchor = parseCalendarDay(day);
  const first = new Date(anchor.getFullYear(), anchor.getMonth(), 1);
  const offset = (first.getDay() - weekStartDay + 7) % 7;
  const daysInMonth = new Date(anchor.getFullYear(), anchor.getMonth() + 1, 0).getDate();
  const weeks = fixedSixWeeks
    ? 6
    : Math.min(6, Math.max(5, Math.ceil((offset + daysInMonth) / 7)));
  first.setDate(first.getDate() - offset);
  return Array.from({ length: weeks * 7 }, (_, index) =>
    new Date(first.getFullYear(), first.getMonth(), first.getDate() + index));
}
