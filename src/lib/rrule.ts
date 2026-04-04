/**
 * Simple RRULE expansion for calendar display.
 * Expands a recurrence rule into occurrences within a date range.
 * Handles: FREQ=DAILY|WEEKLY|MONTHLY|YEARLY, INTERVAL, COUNT, UNTIL, BYDAY.
 */

interface RRuleParts {
  freq: string;
  interval: number;
  count?: number;
  until?: Date;
  byday?: string[];
}

export function parseRRule(rrule: string): RRuleParts | null {
  if (!rrule.startsWith("FREQ=")) return null;

  const parts: Record<string, string> = {};
  for (const part of rrule.split(";")) {
    const [key, value] = part.split("=");
    if (key && value) parts[key] = value;
  }

  if (!parts.FREQ) return null;

  return {
    freq: parts.FREQ,
    interval: parseInt(parts.INTERVAL || "1", 10),
    count: parts.COUNT ? parseInt(parts.COUNT, 10) : undefined,
    until: parts.UNTIL ? parseRRuleDate(parts.UNTIL) : undefined,
    byday: parts.BYDAY ? parts.BYDAY.split(",") : undefined,
  };
}

function parseRRuleDate(s: string): Date {
  // UNTIL format: 20260404T235959Z or 20260404
  if (s.length === 8) {
    return new Date(`${s.slice(0, 4)}-${s.slice(4, 6)}-${s.slice(6, 8)}`);
  }
  return new Date(
    `${s.slice(0, 4)}-${s.slice(4, 6)}-${s.slice(6, 8)}T${s.slice(9, 11)}:${s.slice(11, 13)}:${s.slice(13, 15)}Z`,
  );
}

const dayMap: Record<string, number> = {
  SU: 0, MO: 1, TU: 2, WE: 3, TH: 4, FR: 5, SA: 6,
};

/**
 * Expand a recurring event into occurrences within [rangeStart, rangeEnd].
 * Returns an array of start dates for each occurrence.
 */
export function expandRRule(
  rrule: string,
  eventStart: Date,
  eventEnd: Date,
  rangeStart: Date,
  rangeEnd: Date,
  maxOccurrences = 500,
): { start: Date; end: Date }[] {
  const parsed = parseRRule(rrule);
  if (!parsed) return [];

  const duration = eventEnd.getTime() - eventStart.getTime();
  const occurrences: { start: Date; end: Date }[] = [];
  let current = new Date(eventStart);
  let count = 0;

  while (count < maxOccurrences) {
    if (parsed.until && current > parsed.until) break;
    if (parsed.count !== undefined && count >= parsed.count) break;
    if (current > rangeEnd) break;

    const occEnd = new Date(current.getTime() + duration);

    // Check if this occurrence overlaps the range
    if (occEnd >= rangeStart && current <= rangeEnd) {
      // For WEEKLY with BYDAY, check day-of-week
      if (parsed.freq === "WEEKLY" && parsed.byday) {
        // Generate all matching days in this week
        const weekStart = new Date(current);
        weekStart.setDate(current.getDate() - current.getDay());
        for (const day of parsed.byday) {
          const dayNum = dayMap[day];
          if (dayNum === undefined) continue;
          const d = new Date(weekStart);
          d.setDate(weekStart.getDate() + dayNum);
          d.setHours(eventStart.getHours(), eventStart.getMinutes(), eventStart.getSeconds());
          const dEnd = new Date(d.getTime() + duration);
          if (d >= rangeStart && d <= rangeEnd) {
            occurrences.push({ start: d, end: dEnd });
          }
        }
      } else {
        occurrences.push({ start: new Date(current), end: occEnd });
      }
    }

    count++;

    // Advance to next occurrence
    switch (parsed.freq) {
      case "DAILY":
        current.setDate(current.getDate() + parsed.interval);
        break;
      case "WEEKLY":
        current.setDate(current.getDate() + 7 * parsed.interval);
        break;
      case "MONTHLY":
        current.setMonth(current.getMonth() + parsed.interval);
        break;
      case "YEARLY":
        current.setFullYear(current.getFullYear() + parsed.interval);
        break;
      default:
        return occurrences;
    }
  }

  return occurrences;
}

/**
 * Human-readable description of an RRULE.
 */
export function describeRRule(rrule: string): string {
  const parsed = parseRRule(rrule);
  if (!parsed) return rrule;

  let desc = "";

  switch (parsed.freq) {
    case "DAILY":
      desc = parsed.interval === 1 ? "Daily" : `Every ${parsed.interval} days`;
      break;
    case "WEEKLY":
      desc = parsed.interval === 1 ? "Weekly" : `Every ${parsed.interval} weeks`;
      if (parsed.byday) {
        const dayNames = parsed.byday.map((d) => {
          const names: Record<string, string> = {
            MO: "Mon", TU: "Tue", WE: "Wed", TH: "Thu", FR: "Fri", SA: "Sat", SU: "Sun",
          };
          return names[d] || d;
        });
        desc += ` on ${dayNames.join(", ")}`;
      }
      break;
    case "MONTHLY":
      desc = parsed.interval === 1 ? "Monthly" : `Every ${parsed.interval} months`;
      break;
    case "YEARLY":
      desc = parsed.interval === 1 ? "Yearly" : `Every ${parsed.interval} years`;
      break;
  }

  if (parsed.count) desc += `, ${parsed.count} times`;
  if (parsed.until) desc += `, until ${parsed.until.toLocaleDateString()}`;

  return desc;
}
