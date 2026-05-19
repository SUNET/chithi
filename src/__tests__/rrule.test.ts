import { describe, it, expect } from "vitest";
import { expandRRule } from "@/lib/rrule";

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
