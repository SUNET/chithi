import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

import { invoke } from "@tauri-apps/api/core";
import {
  executeCalendarAction, planCalendarAction, readCalendarEventSet,
} from "@/lib/tauri";
import type { CalendarActionInput } from "@/lib/types";

describe("calendar action IPC contract", () => {
  beforeEach(() => vi.mocked(invoke).mockReset());

  it("reads an authoritative event set with camelCase arguments", async () => {
    vi.mocked(invoke).mockResolvedValueOnce({ master: {}, page: {} });

    await readCalendarEventSet("master", "start", "end", 25);

    expect(invoke).toHaveBeenCalledExactlyOnceWith(
      "read_calendar_event_set",
      { eventId: "master", start: "start", end: "end", limit: 25 },
    );
  });

  it("passes the scoped plan as one typed input", async () => {
    const input: CalendarActionInput = {
      selection: {
        event_id: "master",
        token: "token",
        original_start: "2026-09-15T09:00:00Z",
      },
      scope: "this-occurrence",
      edit: { title: "Only this one" },
      destination_calendar_id: null,
      reset_exceptions: false,
    };
    vi.mocked(invoke).mockResolvedValueOnce({ operation_id: "operation" });

    await planCalendarAction(input);

    expect(invoke).toHaveBeenCalledExactlyOnceWith(
      "plan_calendar_action",
      { input },
    );
  });

  it("executes the operation with explicit confirmations", async () => {
    vi.mocked(invoke).mockResolvedValueOnce({ stage: "completed" });
    const confirmations = {
      replacement_meeting_identity: false,
      reset_exceptions: false,
    };

    await executeCalendarAction("operation", confirmations);

    expect(invoke).toHaveBeenCalledExactlyOnceWith(
      "execute_calendar_action",
      { operationId: "operation", confirmations },
    );
  });
});
