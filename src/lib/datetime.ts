/**
 * Format an ISO 8601 UTC datetime string in the given IANA timezone.
 */
export function formatInTimezone(
  iso: string,
  timezone: string,
  options?: Intl.DateTimeFormatOptions,
): string {
  const date = new Date(iso);
  if (isNaN(date.getTime())) return iso;

  const defaults: Intl.DateTimeFormatOptions = {
    weekday: "long",
    month: "long",
    day: "numeric",
    year: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    timeZone: timezone,
  };

  return date.toLocaleString(undefined, { ...defaults, ...options });
}

/**
 * Get the hour (0-23) of a UTC ISO datetime in the given timezone.
 * Used for positioning events on the time grid.
 */
export function getHourInTimezone(iso: string, timezone: string): number {
  const date = new Date(iso);
  if (isNaN(date.getTime())) return 0;
  return parseInt(
    date.toLocaleString("en-US", { hour: "numeric", hour12: false, timeZone: timezone }),
    10,
  );
}

/**
 * Get the minutes (0-59) of a UTC ISO datetime in the given timezone.
 */
export function getMinutesInTimezone(iso: string, timezone: string): number {
  const date = new Date(iso);
  if (isNaN(date.getTime())) return 0;
  return parseInt(
    date.toLocaleString("en-US", { minute: "numeric", timeZone: timezone }),
    10,
  );
}

/**
 * Get YYYY-MM-DD date string of a UTC ISO datetime in the given timezone.
 * Used for bucketing events into calendar days.
 */
export function getDateInTimezone(iso: string, timezone: string): string {
  const date = new Date(iso);
  if (isNaN(date.getTime())) return iso.split("T")[0] || iso;
  return date.toLocaleDateString("sv-SE", { timeZone: timezone });
}

/**
 * Format a Date in the display timezone to YYYY-MM-DD.
 */
export function toDateInTimezone(date: Date, timezone: string): string {
  return date.toLocaleDateString("sv-SE", { timeZone: timezone });
}

/**
 * Format a Date in the display timezone to HH:MM.
 */
export function toTimeInTimezone(date: Date, timezone: string): string {
  const h = date.toLocaleString("en-US", { hour: "numeric", hour12: false, timeZone: timezone });
  const m = date.toLocaleString("en-US", { minute: "numeric", timeZone: timezone });
  return `${h.padStart(2, "0")}:${m.padStart(2, "0")}`;
}

/**
 * Convert a user-entered local date + time in the display timezone to a UTC ISO string.
 */
export function localInputToUTC(date: string, time: string, timezone: string): string {
  const utcGuess = new Date(`${date}T${time}:00Z`);

  const utcParts = getDateTimeParts(utcGuess, "UTC");
  const tzParts = getDateTimeParts(utcGuess, timezone);

  const utcMinutes = utcParts.hour * 60 + utcParts.minute;
  const tzMinutes = tzParts.hour * 60 + tzParts.minute;
  let offsetMinutes = tzMinutes - utcMinutes;

  if (utcParts.day !== tzParts.day) {
    if (tzParts.day > utcParts.day || (utcParts.day > 25 && tzParts.day < 5)) {
      offsetMinutes += 24 * 60;
    } else {
      offsetMinutes -= 24 * 60;
    }
  }

  const userMs = new Date(`${date}T${time}:00Z`).getTime();
  const utcMs = userMs - offsetMinutes * 60 * 1000;
  return new Date(utcMs).toISOString();
}

/**
 * Get the UTC timestamp (ms) for midnight (00:00) of a given date in a timezone.
 * E.g., startOfDayUTC("2026-04-14", "Europe/Stockholm") returns the UTC ms
 * for 2026-04-14T00:00:00 Stockholm time (= 2026-04-13T22:00:00Z).
 */
export function startOfDayUTC(dateStr: string, timezone: string): number {
  return new Date(localInputToUTC(dateStr, "00:00", timezone)).getTime();
}

/**
 * Get the UTC timestamp (ms) for the last millisecond of a given date in a timezone.
 * Uses 23:59 + 59999 ms to cover the full minute.
 */
export function endOfDayUTC(dateStr: string, timezone: string): number {
  return new Date(localInputToUTC(dateStr, "23:59", timezone)).getTime() + 59999;
}

interface DateTimeParts {
  year: number;
  month: number;
  day: number;
  hour: number;
  minute: number;
}

function getDateTimeParts(date: Date, timezone: string): DateTimeParts {
  const fmt = new Intl.DateTimeFormat("en-US", {
    timeZone: timezone,
    year: "numeric",
    month: "numeric",
    day: "numeric",
    hour: "numeric",
    minute: "numeric",
    hour12: false,
  });
  const parts = fmt.formatToParts(date);
  const get = (type: string) => parseInt(parts.find((p) => p.type === type)?.value || "0", 10);
  return {
    year: get("year"),
    month: get("month"),
    day: get("day"),
    hour: get("hour") % 24,
    minute: get("minute"),
  };
}
