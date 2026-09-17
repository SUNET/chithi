import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

import { invoke } from "@tauri-apps/api/core";
import {
  executeCalendarAction, listCalendarOccurrences, listPendingCalendarActions,
  planCalendarAction, planCalendarOccurrenceAction, readCalendarEventSet,
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

  it("lists bounded occurrences with camelCase arguments", async () => {
    vi.mocked(invoke).mockResolvedValueOnce({ occurrences: [] });

    await listCalendarOccurrences("account", null, "start", "end", 2000);

    expect(invoke).toHaveBeenCalledExactlyOnceWith(
      "list_calendar_occurrences",
      {
        accountId: "account",
        calendarId: null,
        start: "start",
        end: "end",
        limit: 2000,
      },
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

  it("plans an occurrence atomically with displayed fields", async () => {
    vi.mocked(invoke).mockResolvedValueOnce({ operation_id: "operation" });
    const expected = {
      title: "Standup",
      description: null,
      location: null,
      start_time: "2026-09-15T09:00:00Z",
      end_time: "2026-09-15T09:30:00Z",
      all_day: false,
      timezone: "UTC",
    };
    const edit = { title: "Only this one" };

    await planCalendarOccurrenceAction(
      "master",
      "2026-09-15T09:00:00Z",
      expected,
      edit,
    );

    expect(invoke).toHaveBeenCalledExactlyOnceWith(
      "plan_calendar_occurrence_action",
      {
        eventId: "master",
        originalStart: "2026-09-15T09:00:00Z",
        expected,
        edit,
      },
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

  it("lists pending actions for one account", async () => {
    vi.mocked(invoke).mockResolvedValueOnce([]);

    await listPendingCalendarActions("account");

    expect(invoke).toHaveBeenCalledExactlyOnceWith(
      "list_pending_calendar_actions",
      { accountId: "account" },
    );
  });
});
