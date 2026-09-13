import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

import { invoke } from "@tauri-apps/api/core";
import { getCalendarEvent, moveEventToCalendar, notifyCalendarEvent } from "@/lib/tauri";

describe("calendar move IPC contract", () => {
  beforeEach(() => vi.mocked(invoke).mockReset());

  it("fetches one authoritative event by its original ID", async () => {
    const row = { id: "original-event", recurrence_kind: "standalone" };
    vi.mocked(invoke).mockResolvedValueOnce(row);
    await expect(getCalendarEvent("original-event")).resolves.toEqual(row);
    expect(invoke).toHaveBeenCalledExactlyOnceWith("get_calendar_event", { eventId: "original-event" });
  });

  it("uses the guarded ordinary notification command with camelCase arguments", async () => {
    vi.mocked(invoke).mockResolvedValueOnce(undefined);
    await notifyCalendarEvent("account", "original-event", ["guest@example.test"]);
    expect(invoke).toHaveBeenCalledExactlyOnceWith("notify_calendar_event", {
      accountId: "account", eventId: "original-event", attendeeEmails: ["guest@example.test"],
    });
  });

  it("passes the original ID and camelCase destination arguments and returns the resulting ID", async () => {
    vi.mocked(invoke).mockResolvedValueOnce("destination-event");
    await expect(moveEventToCalendar("original-event", "calendar", "account"))
      .resolves.toBe("destination-event");
    expect(invoke).toHaveBeenCalledExactlyOnceWith("move_event_to_calendar", {
      eventId: "original-event", targetCalendarId: "calendar", targetAccountId: "account",
    });
  });
});
