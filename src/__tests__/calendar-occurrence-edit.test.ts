import { beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

vi.mock("@/lib/tauri", () => ({
  readCalendarEventSet: vi.fn(),
  planCalendarAction: vi.fn(),
  executeCalendarAction: vi.fn(),
  getEvents: vi.fn().mockResolvedValue([]),
  listCalendars: vi.fn().mockResolvedValue([]),
  syncCalendars: vi.fn().mockResolvedValue(undefined),
}));

import * as api from "@/lib/tauri";
import type {
  CalendarEvent, CalendarEventSetView, CalendarOccurrence,
} from "@/lib/types";
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

function view(occurrences = [occurrence()]): CalendarEventSetView {
  return {
    master: occurrence({
      selection: {
        event_id: "master",
        token: "selection-token",
        original_start: null,
      },
    }),
    page: { occurrences, has_more: false, needs_hydration: [] },
    exception_count: 0,
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
  vi.mocked(api.readCalendarEventSet).mockResolvedValue(view());
  vi.mocked(api.planCalendarAction).mockResolvedValue({
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

    expect(api.readCalendarEventSet).toHaveBeenCalledWith(
      "master", expect.any(String), expect.any(String), 200,
    );
    expect(api.planCalendarAction).toHaveBeenCalledWith({
      selection: occurrence().selection,
      scope: "this-occurrence",
      edit: { title: "Only this one" },
      destination_calendar_id: null,
      reset_exceptions: false,
    });
    expect(api.executeCalendarAction).toHaveBeenCalledWith("operation", {
      replacement_meeting_identity: false,
      reset_exceptions: false,
    });
    expect(api.getEvents).toHaveBeenCalled();
  });

  it("rejects attendee-bearing occurrences before backend reads", async () => {
    const store = useCalendarStore();
    const selected = event({
      attendees_json: JSON.stringify([{ email: "guest@example.test" }]),
    });

    await expect(
      store.updateOccurrence(selected, { title: "Unsafe" }),
    ).rejects.toThrow("meeting with attendees");
    expect(api.readCalendarEventSet).not.toHaveBeenCalled();
    expect(api.planCalendarAction).not.toHaveBeenCalled();
  });

  it("fails closed when an occurrence selection is ambiguous", async () => {
    const store = useCalendarStore();
    vi.mocked(api.readCalendarEventSet).mockResolvedValue(
      view([occurrence(), occurrence({ is_exception: true })]),
    );

    await expect(
      store.updateOccurrence(event(), { title: "Ambiguous" }),
    ).rejects.toThrow("identified uniquely");
    expect(api.planCalendarAction).not.toHaveBeenCalled();
  });

  it("does not map a stale generated slot onto a moved exception", async () => {
    const store = useCalendarStore();
    vi.mocked(api.readCalendarEventSet).mockResolvedValue(view([
      occurrence({
        fields: {
          ...occurrence().fields,
          start_time: "2026-09-16T09:00:00Z",
          end_time: "2026-09-16T09:30:00Z",
        },
        is_exception: true,
      }),
    ]));

    await expect(
      store.updateOccurrence(event(), { title: "Wrong slot" }),
    ).rejects.toThrow("identified uniquely");
    expect(api.planCalendarAction).not.toHaveBeenCalled();
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
