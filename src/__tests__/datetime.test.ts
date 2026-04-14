import { describe, it, expect } from "vitest";
import {
  formatInTimezone,
  getHourInTimezone,
  getMinutesInTimezone,
  getDateInTimezone,
  toDateInTimezone,
  toTimeInTimezone,
  localInputToUTC,
  startOfDayUTC,
  endOfDayUTC,
} from "@/lib/datetime";

describe("getDateInTimezone", () => {
  it("returns correct date for UTC timezone", () => {
    expect(getDateInTimezone("2026-04-14T12:00:00Z", "UTC")).toBe("2026-04-14");
  });

  it("shifts date forward for positive-offset timezone", () => {
    // 23:00 UTC = 01:00+02:00 next day in Stockholm (CEST)
    expect(getDateInTimezone("2026-04-14T23:00:00Z", "Europe/Stockholm")).toBe("2026-04-15");
  });

  it("shifts date backward for negative-offset timezone", () => {
    // 01:00 UTC = 21:00 previous day in New York (EDT, UTC-4)
    expect(getDateInTimezone("2026-04-14T01:00:00Z", "America/New_York")).toBe("2026-04-13");
  });

  it("returns date-only strings as-is (all-day events)", () => {
    expect(getDateInTimezone("2026-04-14", "America/Los_Angeles")).toBe("2026-04-14");
    expect(getDateInTimezone("2026-04-14", "Asia/Tokyo")).toBe("2026-04-14");
  });

  it("returns invalid strings unchanged", () => {
    expect(getDateInTimezone("not-a-date", "UTC")).toBe("not-a-date");
  });
});

describe("getHourInTimezone", () => {
  it("returns UTC hour for UTC timezone", () => {
    expect(getHourInTimezone("2026-04-14T14:30:00Z", "UTC")).toBe(14);
  });

  it("applies positive offset", () => {
    // 12:00 UTC = 14:00 Stockholm (CEST, UTC+2)
    expect(getHourInTimezone("2026-04-14T12:00:00Z", "Europe/Stockholm")).toBe(14);
  });

  it("applies negative offset", () => {
    // 12:00 UTC = 08:00 New York (EDT, UTC-4)
    expect(getHourInTimezone("2026-04-14T12:00:00Z", "America/New_York")).toBe(8);
  });

  it("returns 0 for invalid input", () => {
    expect(getHourInTimezone("bad", "UTC")).toBe(0);
  });
});

describe("getMinutesInTimezone", () => {
  it("returns correct minutes", () => {
    expect(getMinutesInTimezone("2026-04-14T14:45:00Z", "UTC")).toBe(45);
  });

  it("minutes are timezone-independent for whole-hour offsets", () => {
    expect(getMinutesInTimezone("2026-04-14T14:45:00Z", "Europe/Stockholm")).toBe(45);
  });

  it("handles half-hour offsets", () => {
    // India is UTC+5:30, so 14:00 UTC = 19:30 IST
    expect(getMinutesInTimezone("2026-04-14T14:00:00Z", "Asia/Kolkata")).toBe(30);
  });
});

describe("toDateInTimezone", () => {
  it("formats date in timezone", () => {
    const d = new Date("2026-04-14T23:00:00Z");
    expect(toDateInTimezone(d, "UTC")).toBe("2026-04-14");
    expect(toDateInTimezone(d, "Europe/Stockholm")).toBe("2026-04-15");
  });
});

describe("toTimeInTimezone", () => {
  it("formats time in timezone", () => {
    const d = new Date("2026-04-14T12:30:00Z");
    expect(toTimeInTimezone(d, "UTC")).toBe("12:30");
    expect(toTimeInTimezone(d, "Europe/Stockholm")).toBe("14:30");
  });

  it("pads single-digit hours", () => {
    const d = new Date("2026-04-14T07:05:00Z");
    expect(toTimeInTimezone(d, "UTC")).toBe("07:05");
  });
});

describe("localInputToUTC", () => {
  it("converts UTC input to UTC (identity)", () => {
    const result = localInputToUTC("2026-04-14", "12:00", "UTC");
    expect(result).toBe(new Date("2026-04-14T12:00:00Z").toISOString());
  });

  it("converts Stockholm time to UTC (subtracts offset)", () => {
    // 14:00 Stockholm (CEST, UTC+2) = 12:00 UTC
    const result = localInputToUTC("2026-04-14", "14:00", "Europe/Stockholm");
    expect(new Date(result).toISOString()).toBe("2026-04-14T12:00:00.000Z");
  });

  it("converts New York time to UTC (adds offset)", () => {
    // 08:00 New York (EDT, UTC-4) = 12:00 UTC
    const result = localInputToUTC("2026-04-14", "08:00", "America/New_York");
    expect(new Date(result).toISOString()).toBe("2026-04-14T12:00:00.000Z");
  });

  it("handles day boundary crossing (positive offset)", () => {
    // 01:00 Stockholm (CEST) on April 15 = 23:00 UTC on April 14
    const result = localInputToUTC("2026-04-15", "01:00", "Europe/Stockholm");
    expect(new Date(result).toISOString()).toBe("2026-04-14T23:00:00.000Z");
  });

  it("handles day boundary crossing (negative offset)", () => {
    // 23:00 New York (EDT) on April 14 = 03:00 UTC on April 15
    const result = localInputToUTC("2026-04-14", "23:00", "America/New_York");
    expect(new Date(result).toISOString()).toBe("2026-04-15T03:00:00.000Z");
  });

  it("handles winter time (CET, UTC+1)", () => {
    // 13:00 Stockholm (CET) on Jan 14 = 12:00 UTC
    const result = localInputToUTC("2026-01-14", "13:00", "Europe/Stockholm");
    expect(new Date(result).toISOString()).toBe("2026-01-14T12:00:00.000Z");
  });
});

describe("startOfDayUTC / endOfDayUTC", () => {
  it("returns midnight in timezone as UTC ms", () => {
    // Midnight Stockholm (CEST, UTC+2) on April 14 = 22:00 UTC on April 13
    const ms = startOfDayUTC("2026-04-14", "Europe/Stockholm");
    expect(new Date(ms).toISOString()).toBe("2026-04-13T22:00:00.000Z");
  });

  it("returns end of day in timezone as UTC ms", () => {
    // 23:59:59.999 Stockholm on April 14 = 21:59:59.999 UTC on April 14
    const ms = endOfDayUTC("2026-04-14", "Europe/Stockholm");
    const d = new Date(ms);
    expect(d.getUTCHours()).toBe(21);
    expect(d.getUTCMinutes()).toBe(59);
  });

  it("UTC start of day is midnight UTC", () => {
    const ms = startOfDayUTC("2026-04-14", "UTC");
    expect(new Date(ms).toISOString()).toBe("2026-04-14T00:00:00.000Z");
  });
});

describe("formatInTimezone", () => {
  it("formats with default options", () => {
    const result = formatInTimezone("2026-04-14T12:00:00Z", "UTC");
    expect(result).toContain("2026");
    expect(result).toContain("14");
  });

  it("returns invalid strings unchanged", () => {
    expect(formatInTimezone("not-a-date", "UTC")).toBe("not-a-date");
  });

  it("accepts custom options", () => {
    const result = formatInTimezone("2026-04-14T12:00:00Z", "UTC", {
      hour: "numeric",
      minute: "2-digit",
    });
    // Should contain time but not weekday
    expect(result).toBeTruthy();
  });
});
