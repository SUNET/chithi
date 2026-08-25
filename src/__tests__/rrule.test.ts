import { describe, it, expect } from "vitest";
import {
  expandRRule,
  isOccurrenceId,
  masterEventId,
  occurrenceId,
} from "@/lib/rrule";

describe("occurrence id helpers", () => {
  const masterId = "4484f3cd-4641-41db-a4ca-1c35e49aa191";
  const start = new Date("2026-08-25T09:00:00.000Z");

  it("mints and resolves round-trip", () => {
    const id = occurrenceId(masterId, start);
    expect(id).toBe(`${masterId}_2026-08-25T09:00:00.000Z`);
    expect(isOccurrenceId(id)).toBe(true);
    expect(masterEventId(id)).toBe(masterId);
  });

  it("leaves plain DB ids untouched", () => {
    expect(isOccurrenceId(masterId)).toBe(false);
    expect(masterEventId(masterId)).toBe(masterId);
  });

  it("supports master ids that themselves contain underscores", () => {
    const weird = "remote_id_with_underscores";
    const id = occurrenceId(weird, start);
    expect(masterEventId(id)).toBe(weird);
  });

  it("handles second-precision ISO suffixes (no milliseconds)", () => {
    expect(masterEventId(`${masterId}_2026-08-25T09:00:00Z`)).toBe(masterId);
  });
});

describe("expandRRule", () => {
  it("expands a normal in-window weekly series", () => {
    const start = new Date("2026-05-04T10:00:00Z");
    const end = new Date("2026-05-04T11:00:00Z");
    const occ = expandRRule(
      "FREQ=WEEKLY",
      start,
      end,
      new Date("2026-05-01T00:00:00Z"),
      new Date("2026-05-31T23:59:59Z"),
    );
    expect(occ.length).toBe(4); // May 4, 11, 18, 25
  });

  it("reaches the window for a long-running daily series (fast-forward)", () => {
    // DTSTART is ~3 years before the window. Without fast-forwarding,
    // expansion from DTSTART would exhaust the iteration cap long before
    // reaching the window and return nothing.
    const start = new Date("2023-01-01T09:00:00Z");
    const end = new Date("2023-01-01T09:30:00Z");
    const rangeStart = new Date("2026-05-01T00:00:00Z");
    const occ = expandRRule(
      "FREQ=DAILY",
      start,
      end,
      rangeStart,
      new Date("2026-05-07T23:59:59Z"),
    );
    expect(occ.length).toBe(7); // one per day, May 1..7
    expect(occ[0].start.getTime()).toBeGreaterThanOrEqual(rangeStart.getTime());
    expect(occ[0].start.getTime()).toBeLessThan(
      rangeStart.getTime() + 86_400_000,
    );
  });

  it("still honours COUNT after fast-forwarding past an exhausted series", () => {
    // Only 10 daily occurrences, all in early 2023 — the series is long
    // exhausted before the 2026 window, so nothing should be emitted.
    const occ = expandRRule(
      "FREQ=DAILY;COUNT=10",
      new Date("2023-01-01T09:00:00Z"),
      new Date("2023-01-01T09:30:00Z"),
      new Date("2026-05-01T00:00:00Z"),
      new Date("2026-05-31T23:59:59Z"),
    );
    expect(occ).toHaveLength(0);
  });

  it("still honours UNTIL after fast-forwarding", () => {
    const occ = expandRRule(
      "FREQ=DAILY;UNTIL=20240101T000000Z",
      new Date("2023-01-01T09:00:00Z"),
      new Date("2023-01-01T09:30:00Z"),
      new Date("2026-05-01T00:00:00Z"),
      new Date("2026-05-31T23:59:59Z"),
    );
    expect(occ).toHaveLength(0);
  });

  it("reaches the window for a long-running monthly series", () => {
    const occ = expandRRule(
      "FREQ=MONTHLY",
      new Date("2020-01-15T12:00:00Z"),
      new Date("2020-01-15T13:00:00Z"),
      new Date("2026-05-01T00:00:00Z"),
      new Date("2026-07-31T23:59:59Z"),
    );
    expect(occ.length).toBe(3); // May, Jun, Jul 2026
  });
});
