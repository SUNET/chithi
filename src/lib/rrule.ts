const RRULE_FREQUENCIES = ["DAILY", "WEEKLY", "MONTHLY", "YEARLY"] as const;
const DAY_MILLISECONDS = 86_400_000;

type RRuleFrequency = (typeof RRULE_FREQUENCIES)[number];

interface RRuleParts {
  freq: RRuleFrequency;
  interval: number;
  count?: number;
  until?: Date;
  byday?: string[];
}

interface RRuleOccurrence {
  start: Date;
  end: Date;
}

const WEEKDAYS: Record<string, { index: number; label: string }> = {
  SU: { index: 0, label: "Sun" },
  MO: { index: 1, label: "Mon" },
  TU: { index: 2, label: "Tue" },
  WE: { index: 3, label: "Wed" },
  TH: { index: 4, label: "Thu" },
  FR: { index: 5, label: "Fri" },
  SA: { index: 6, label: "Sat" },
};

/** Synthetic occurrences append their UTC start so the master ID is recoverable. */
const OCCURRENCE_SUFFIX_RE =
  /_\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$/;

export function occurrenceId(masterId: string, start: Date): string {
  return `${masterId}_${start.toISOString()}`;
}

export function isOccurrenceId(id: string): boolean {
  return OCCURRENCE_SUFFIX_RE.test(id);
}

export function masterEventId(id: string): string {
  return id.replace(OCCURRENCE_SUFFIX_RE, "");
}

function isRRuleFrequency(value: string): value is RRuleFrequency {
  return RRULE_FREQUENCIES.includes(value as RRuleFrequency);
}

export function parseRRule(rrule: string): RRuleParts | null {
  if (!rrule.startsWith("FREQ=")) return null;

  const values: Record<string, string> = Object.fromEntries(
    rrule
      .split(";")
      .map((part) => part.split("=", 2))
      .filter(([key, value]) => key && value),
  );

  const frequency = values.FREQ;
  if (!frequency || !isRRuleFrequency(frequency)) return null;

  const interval = Number.parseInt(values.INTERVAL || "1", 10);
  const count = values.COUNT ? Number.parseInt(values.COUNT, 10) : undefined;
  if (
    !Number.isInteger(interval) ||
    interval < 1 ||
    (count !== undefined && (!Number.isInteger(count) || count < 1))
  ) return null;

  const until = values.UNTIL ? parseRRuleDate(values.UNTIL) : undefined;
  if (until && Number.isNaN(until.getTime())) return null;

  return {
    freq: frequency,
    interval,
    count,
    until,
    byday: values.BYDAY ? values.BYDAY.split(",") : undefined,
  };
}

function parseRRuleDate(value: string): Date {
  const date = `${value.slice(0, 4)}-${value.slice(4, 6)}-${value.slice(6, 8)}`;
  if (value.length === 8) return new Date(date);
  return new Date(
    `${date}T${value.slice(9, 11)}:${value.slice(11, 13)}:${value.slice(13, 15)}Z`,
  );
}

function periodsToSkip(
  rule: RRuleParts,
  current: Date,
  rangeStart: Date,
): number {
  const millisecondsBehind = rangeStart.getTime() - current.getTime();
  switch (rule.freq) {
    case "DAILY":
      return Math.floor(
        millisecondsBehind / (DAY_MILLISECONDS * rule.interval),
      );
    case "WEEKLY":
      return Math.floor(
        millisecondsBehind / (7 * DAY_MILLISECONDS * rule.interval),
      );
    case "MONTHLY": {
      const monthsBehind =
        (rangeStart.getFullYear() - current.getFullYear()) * 12 +
        rangeStart.getMonth() -
        current.getMonth();
      return Math.floor(monthsBehind / rule.interval);
    }
    case "YEARLY":
      return Math.floor(
        (rangeStart.getFullYear() - current.getFullYear()) / rule.interval,
      );
  }
}

function advanceDate(date: Date, rule: RRuleParts, periods = 1): void {
  const amount = rule.interval * periods;
  switch (rule.freq) {
    case "DAILY":
      date.setDate(date.getDate() + amount);
      break;
    case "WEEKLY":
      date.setDate(date.getDate() + 7 * amount);
      break;
    case "MONTHLY":
      date.setMonth(date.getMonth() + amount);
      break;
    case "YEARLY":
      date.setFullYear(date.getFullYear() + amount);
      break;
  }
}

export function expandRRule(
  rrule: string,
  eventStart: Date,
  eventEnd: Date,
  rangeStart: Date,
  rangeEnd: Date,
  maxIterations = 500,
): RRuleOccurrence[] {
  const rule = parseRRule(rrule);
  if (!rule) return [];

  const duration = eventEnd.getTime() - eventStart.getTime();
  const occurrences: RRuleOccurrence[] = [];
  const current = new Date(eventStart);
  let count = 0;
  let iterations = 0;

  // Skip whole periods so distant series do not exhaust the iteration limit.
  if (current < rangeStart) {
    const skippedPeriods = periodsToSkip(rule, current, rangeStart);
    if (skippedPeriods > 0) {
      advanceDate(current, rule, skippedPeriods);
      count += skippedPeriods;
    }
  }

  while (iterations < maxIterations) {
    if (rule.until && current > rule.until) break;
    if (rule.count !== undefined && count >= rule.count) break;
    if (current > rangeEnd) break;

    const occurrenceEnd = new Date(current.getTime() + duration);
    if (occurrenceEnd >= rangeStart && current <= rangeEnd) {
      if (rule.freq === "WEEKLY" && rule.byday) {
        const weekStart = new Date(current);
        weekStart.setDate(current.getDate() - current.getDay());
        for (const day of rule.byday) {
          const weekday = WEEKDAYS[day];
          if (!weekday) continue;
          const occurrenceStart = new Date(weekStart);
          occurrenceStart.setDate(weekStart.getDate() + weekday.index);
          occurrenceStart.setHours(
            eventStart.getHours(),
            eventStart.getMinutes(),
            eventStart.getSeconds(),
          );
          if (occurrenceStart >= rangeStart && occurrenceStart <= rangeEnd) {
            occurrences.push({
              start: occurrenceStart,
              end: new Date(occurrenceStart.getTime() + duration),
            });
          }
        }
      } else {
        occurrences.push({ start: new Date(current), end: occurrenceEnd });
      }
    }

    count += 1;
    iterations += 1;
    advanceDate(current, rule);
  }

  return occurrences;
}

export function describeRRule(rrule: string): string {
  const rule = parseRRule(rrule);
  if (!rule) return rrule;

  let description = "";
  switch (rule.freq) {
    case "DAILY":
      description = rule.interval === 1 ? "Daily" : `Every ${rule.interval} days`;
      break;
    case "WEEKLY":
      description = rule.interval === 1 ? "Weekly" : `Every ${rule.interval} weeks`;
      if (rule.byday) {
        const dayNames = rule.byday.map(
          (day) => WEEKDAYS[day]?.label ?? day,
        );
        description += ` on ${dayNames.join(", ")}`;
      }
      break;
    case "MONTHLY":
      description =
        rule.interval === 1 ? "Monthly" : `Every ${rule.interval} months`;
      break;
    case "YEARLY":
      description =
        rule.interval === 1 ? "Yearly" : `Every ${rule.interval} years`;
      break;
  }

  if (rule.count) description += `, ${rule.count} times`;
  if (rule.until) description += `, until ${rule.until.toLocaleDateString()}`;
  return description;
}
