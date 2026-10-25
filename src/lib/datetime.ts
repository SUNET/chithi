const MILLISECONDS_PER_MINUTE = 60_000;

interface DateTimeParts {
  year: number;
  month: number;
  day: number;
  hour: number;
  minute: number;
  second: number;
}

function parseDate(value: string): Date | null {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? null : date;
}

function getDateTimeParts(date: Date, timezone: string): DateTimeParts {
  const formatter = new Intl.DateTimeFormat("en-US", {
    timeZone: timezone,
    year: "numeric",
    month: "numeric",
    day: "numeric",
    hour: "numeric",
    minute: "numeric",
    second: "numeric",
    hour12: false,
  });
  const parts = formatter.formatToParts(date);
  const getPart = (type: string) =>
    Number.parseInt(parts.find((part) => part.type === type)?.value || "0", 10);
  return {
    year: getPart("year"),
    month: getPart("month"),
    day: getPart("day"),
    hour: getPart("hour") % 24,
    minute: getPart("minute"),
    second: getPart("second"),
  };
}

function timezoneOffsetMilliseconds(date: Date, timezone: string): number {
  const parts = getDateTimeParts(date, timezone);
  // Interpreting the formatted wall-clock parts as UTC reveals the zone offset.
  const wallClockAsUtc = Date.UTC(
    parts.year,
    parts.month - 1,
    parts.day,
    parts.hour,
    parts.minute,
    parts.second,
  );
  return wallClockAsUtc - date.getTime();
}

export function formatInTimezone(
  iso: string,
  timezone: string,
  options?: Intl.DateTimeFormatOptions,
): string {
  const date = parseDate(iso);
  if (!date) return iso;

  return date.toLocaleString(undefined, {
    weekday: "long",
    month: "long",
    day: "numeric",
    year: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    timeZone: timezone,
    ...options,
  });
}

export function getHourInTimezone(iso: string, timezone: string): number {
  const date = parseDate(iso);
  return date ? getDateTimeParts(date, timezone).hour : 0;
}

export function getMinutesInTimezone(iso: string, timezone: string): number {
  const date = parseDate(iso);
  return date ? getDateTimeParts(date, timezone).minute : 0;
}

export function getDateInTimezone(iso: string, timezone: string): string {
  // Date-only values represent civil days and must not shift between timezones.
  if (!iso.includes("T")) return iso;
  const date = parseDate(iso);
  return date
    ? toDateInTimezone(date, timezone)
    : iso.split("T")[0] || iso;
}

export function toDateInTimezone(date: Date, timezone: string): string {
  return date.toLocaleDateString("sv-SE", { timeZone: timezone });
}

export function toTimeInTimezone(date: Date, timezone: string): string {
  const { hour, minute } = getDateTimeParts(date, timezone);
  return `${String(hour).padStart(2, "0")}:${String(minute).padStart(2, "0")}`;
}

export function localInputToUTC(
  date: string,
  time: string,
  timezone: string,
): string {
  const wallClockAsUtc = new Date(`${date}T${time}:00Z`);
  const offset = timezoneOffsetMilliseconds(wallClockAsUtc, timezone);
  return new Date(wallClockAsUtc.getTime() - offset).toISOString();
}

export function startOfDayUTC(date: string, timezone: string): number {
  return new Date(localInputToUTC(date, "00:00", timezone)).getTime();
}

export function endOfDayUTC(date: string, timezone: string): number {
  return (
    new Date(localInputToUTC(date, "23:59", timezone)).getTime() +
    MILLISECONDS_PER_MINUTE -
    1
  );
}
