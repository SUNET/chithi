import { beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

vi.mock("@/lib/tauri", () => ({
  planCalendarOccurrenceAction: vi.fn(),
  executeCalendarAction: vi.fn(),
  listPendingCalendarActions: vi.fn().mockResolvedValue([]),
  getEvents: vi.fn().mockResolvedValue([]),
  listCalendarOccurrences: vi.fn().mockResolvedValue({
    occurrences: [], has_more: false, needs_hydration: [], unresolved: [],
  }),
  listCalendars: vi.fn().mockResolvedValue([]),
  listArchivedGraphCalendars: vi.fn().mockResolvedValue([]),
  syncCalendars: vi.fn().mockResolvedValue(undefined),
}));

import * as api from "@/lib/tauri";
import type { CalendarEvent, CalendarOccurrence } from "@/lib/types";
import { calendarEditSupport } from "@/lib/calendar-mutation-support";
import { useAccountsStore } from "@/stores/accounts";
import { useCalendarStore } from "@/stores/calendar";

const originalStart = "2026-09-15T09:00:00Z";

function event(patch: Partial<CalendarEvent> = {}): CalendarEvent {
  return {
    id: "master_2026-09-15T09:00:00.000Z",
    account_id: "account",
    calendar_id: "calendar",
    uid: "series@example.test",
    title: "Standup",
    description: null,
    location: null,
    start_time: "2026-09-15T09:00:00.000Z",
    end_time: "2026-09-15T09:30:00.000Z",
    all_day: false,
    timezone: "UTC",
    recurrence_rule: "FREQ=WEEKLY",
    recurrence_kind: "occurrence",
    organizer_email: "owner@example.test",
    attendees_json: null,
    my_status: null,
    source_message_id: null,
    ...patch,
  };
}

function occurrence(patch: Partial<CalendarOccurrence> = {}): CalendarOccurrence {
  return {
    selection: {
      event_id: "master",
      token: "selection-token",
      original_start: originalStart,
    },
    event_id: "master",
    account_id: "account",
    calendar_id: "calendar",
    fields: {
      title: "Standup",
      description: null,
      location: null,
      start_time: "2026-09-15T09:00:00Z",
      end_time: "2026-09-15T09:30:00Z",
      all_day: false,
      timezone: "UTC",
    },
    recurrence_kind: "series",
    recurrence_rule: "FREQ=WEEKLY",
    is_exception: false,
    ...patch,
  };
}

beforeEach(() => {
  setActivePinia(createPinia());
  vi.clearAllMocks();
  useAccountsStore().accounts = [{
    id: "account",
    display_name: "Account",
    email: "owner@example.test",
    username: "owner@example.test",
    provider: "generic",
    mail_protocol: "jmap",
    enabled: true,
    mail_sync_interval_seconds: null,
    calendar_sync_interval_seconds: null,
    contacts_sync_interval_seconds: null,
    has_calendar_binding: true,
    has_contacts_binding: false,
    meet_protocol: "",
  }];
  vi.mocked(api.listPendingCalendarActions).mockResolvedValue([]);
  vi.mocked(api.planCalendarOccurrenceAction).mockResolvedValue({
    operation_id: "operation",
    requires: {
      replacement_meeting_identity: false,
      reset_exceptions: false,
    },
    preview: occurrence().fields,
  });
  vi.mocked(api.executeCalendarAction).mockResolvedValue({
    operation_id: "operation",
    stage: "completed",
    event_id: "master",
    requires: {
      replacement_meeting_identity: false,
      reset_exceptions: false,
    },
  });
  vi.mocked(api.getEvents).mockResolvedValue([]);
});

describe("safe occurrence editing", () => {
  it("resolves a synthetic occurrence and executes this-occurrence scope", async () => {
    const store = useCalendarStore();
    const selected = event();

    await store.updateOccurrence(selected, { title: "Only this one" });

    expect(api.planCalendarOccurrenceAction).toHaveBeenCalledWith(
      "master",
      "2026-09-15T09:00:00.000Z",
      {
        title: "Standup",
        description: null,
        location: null,
        start_time: "2026-09-15T09:00:00.000Z",
        end_time: "2026-09-15T09:30:00.000Z",
        all_day: false,
        timezone: "UTC",
      },
      { title: "Only this one" },
    );
    expect(api.executeCalendarAction).toHaveBeenCalledWith("operation", {
      replacement_meeting_identity: false,
      reset_exceptions: false,
    });
    expect(api.getEvents).toHaveBeenCalled();
  });

  it("resumes an applying occurrence edit before reading a stale selection", async () => {
    vi.mocked(api.listPendingCalendarActions).mockResolvedValue([{
      operation_id: "pending-operation",
      stage: "applying",
      event_id: "master",
      requires: {
        replacement_meeting_identity: false,
        reset_exceptions: false,
      },
    }]);

    const store = useCalendarStore();
    await store.updateOccurrence(event(), {
      start_time: "2026-09-15T10:00:00.000Z",
      end_time: "2026-09-15T10:30:00.000Z",
    });

    expect(api.executeCalendarAction).toHaveBeenCalledExactlyOnceWith(
      "pending-operation",
      {
        replacement_meeting_identity: false,
        reset_exceptions: false,
      },
    );
    expect(api.planCalendarOccurrenceAction).not.toHaveBeenCalled();
    expect(api.getEvents).toHaveBeenCalled();
  });

  it("resumes a claimed atomic plan after a renderer interruption", async () => {
    vi.mocked(api.listPendingCalendarActions).mockResolvedValue([{
      operation_id: "pending-operation",
      stage: "planned",
      event_id: "master",
      auto_resume: true,
      requires: {
        replacement_meeting_identity: false,
        reset_exceptions: false,
      },
    }]);

    await useCalendarStore().updateOccurrence(event(), {
      title: "Resume atomic plan",
    });

    expect(api.executeCalendarAction).toHaveBeenCalledExactlyOnceWith(
      "pending-operation",
      {
        replacement_meeting_identity: false,
        reset_exceptions: false,
      },
    );
    expect(api.planCalendarOccurrenceAction).not.toHaveBeenCalled();
  });

  it("does not resume an action that still requires confirmation", async () => {
    vi.mocked(api.listPendingCalendarActions).mockResolvedValue([{
      operation_id: "pending-operation",
      stage: "applying",
      event_id: "master",
      requires: {
        replacement_meeting_identity: true,
        reset_exceptions: false,
      },
    }]);

    await expect(
      useCalendarStore().updateOccurrence(event(), { title: "Unsafe recovery" }),
    ).rejects.toThrow("requires confirmation");
    expect(api.executeCalendarAction).not.toHaveBeenCalled();
    expect(api.planCalendarOccurrenceAction).not.toHaveBeenCalled();
  });

  it("rejects attendee-bearing occurrences before backend reads", async () => {
    const store = useCalendarStore();
    const selected = event({
      attendees_json: JSON.stringify([{ email: "guest@example.test" }]),
    });

    await expect(
      store.updateOccurrence(selected, { title: "Unsafe" }),
    ).rejects.toThrow("meeting with attendees");
    expect(api.planCalendarOccurrenceAction).not.toHaveBeenCalled();
  });

  it("fails closed without a trusted synthetic original position", async () => {
    await expect(
      useCalendarStore().updateOccurrence(
        event({ id: "detached-occurrence" }),
        { title: "Ambiguous" },
      ),
    ).rejects.toThrow("no trusted original position");
    expect(api.planCalendarOccurrenceAction).not.toHaveBeenCalled();
  });

  it("binds a moved exception to its original slot and displayed fields", async () => {
    const store = useCalendarStore();
    const moved = event({
      start_time: "2026-09-16T09:00:00.000Z",
      end_time: "2026-09-16T09:30:00.000Z",
    });

    await store.updateOccurrence(moved, { title: "Moved slot" });

    expect(api.planCalendarOccurrenceAction).toHaveBeenCalledWith(
      "master",
      "2026-09-15T09:00:00.000Z",
      expect.objectContaining({
        start_time: "2026-09-16T09:00:00.000Z",
        end_time: "2026-09-16T09:30:00.000Z",
      }),
      { title: "Moved slot" },
    );
  });

  it("preserves an all-day occurrence's date-valued original position", async () => {
    await useCalendarStore().updateOccurrence(event({
      id: "master_2026-09-15T00:00:00.000Z",
      start_time: "2026-09-15",
      end_time: "2026-09-16",
      all_day: true,
    }), { title: "All day" });

    expect(api.planCalendarOccurrenceAction).toHaveBeenCalledWith(
      "master",
      "2026-09-15",
      expect.objectContaining({
        start_time: "2026-09-15",
        end_time: "2026-09-16",
        all_day: true,
      }),
      { title: "All day" },
    );
  });

  it("keeps series masters blocked while allowing attendee-free occurrences", () => {
    expect(calendarEditSupport(event())).toEqual({
      supported: true,
      occurrence: true,
      reason: null,
    });
    expect(calendarEditSupport(event({
      id: "master",
      recurrence_kind: "series",
    })).supported).toBe(false);
  });
});
