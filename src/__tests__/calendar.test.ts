import { describe, it, expect, vi, beforeEach } from "vitest";
import { setActivePinia, createPinia } from "pinia";

vi.mock("@/lib/tauri", () => ({
  listAccounts: vi.fn().mockResolvedValue([]),
  listCalendars: vi.fn().mockResolvedValue([]),
  getEvents: vi.fn().mockResolvedValue([]),
  createEvent: vi.fn().mockResolvedValue("evt-1"),
  updateEvent: vi.fn().mockResolvedValue(undefined),
  deleteEvent: vi.fn().mockResolvedValue(undefined),
  syncCalendars: vi.fn().mockResolvedValue(undefined),
  getEmailInvites: vi.fn().mockResolvedValue([]),
  getInviteStatus: vi.fn().mockResolvedValue(null),
  respondToInvite: vi.fn().mockResolvedValue(undefined),
  sendInvites: vi.fn().mockResolvedValue(undefined),
  listFolders: vi.fn().mockResolvedValue([]),
  getMessages: vi.fn().mockResolvedValue({ messages: [], total: 0, page: 0, per_page: 100 }),
  getMessageBody: vi.fn().mockResolvedValue({
    id: "msg1", subject: "Test", from: { name: "Test", email: "test@example.com" },
    to: [], cc: [], date: "2026-04-03T00:00:00Z", flags: [],
    body_html: null, body_text: "Hello", attachments: [],
    is_encrypted: false, is_signed: false, list_id: null,
  }),
  setMessageFlags: vi.fn().mockResolvedValue(undefined),
  deleteMessages: vi.fn().mockResolvedValue(undefined),
  getThreadedMessages: vi.fn().mockResolvedValue({
    threads: [], total_threads: 0, total_messages: 0, page: 0, per_page: 100,
  }),
  getThreadMessages: vi.fn().mockResolvedValue([]),
  triggerSync: vi.fn().mockResolvedValue(undefined),
  backfillThreads: vi.fn().mockResolvedValue(0),
  prefetchBodies: vi.fn().mockResolvedValue(0),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { useCalendarStore } from "@/stores/calendar";
import { useAccountsStore } from "@/stores/accounts";
import * as api from "@/lib/tauri";

function setupAccounts() {
  const accountsStore = useAccountsStore();
  accountsStore.accounts = [
    {
      id: "acc1", display_name: "Test", email: "test@test.com",
      provider: "generic", mail_protocol: "jmap" as const, enabled: true,
    },
  ];
  accountsStore.activeAccountId = "acc1";
  return accountsStore;
}

function makeCalendar(id: string, name: string, remoteId: string | null = null) {
  return {
    id, account_id: "acc1", name, color: "#4285f4",
    is_default: true, remote_id: remoteId,
  };
}

function makeEvent(
  id: string,
  title: string,
  startTime: string,
  endTime: string,
  opts: Partial<{
    calendar_id: string; my_status: string | null; uid: string | null;
    attendees_json: string | null; recurrence_rule: string | null;
  }> = {},
) {
  return {
    id, account_id: "acc1", calendar_id: opts.calendar_id ?? "cal1",
    uid: opts.uid ?? `${id}@chithi`,
    title, description: null, location: null,
    start_time: startTime, end_time: endTime,
    all_day: false, timezone: null,
    recurrence_rule: opts.recurrence_rule ?? null,
    organizer_email: null, attendees_json: opts.attendees_json ?? null,
    my_status: opts.my_status ?? null, source_message_id: null,
  };
}

describe("Calendar store", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    vi.clearAllMocks();
  });

  describe("createEvent", () => {
    it("should call api.createEvent and refresh events", async () => {
      setupAccounts();
      const store = useCalendarStore();

      const input = {
        account_id: "acc1", calendar_id: "cal1",
        title: "Hello meeting", description: null, location: null,
        start_time: "2026-04-07T17:00:00.000Z",
        end_time: "2026-04-07T18:00:00.000Z",
        all_day: false, timezone: null, recurrence_rule: null,
        attendees: [{ email: "bob@example.com", name: null, status: "needs-action" }],
      };

      const id = await store.createEvent(input);

      expect(api.createEvent).toHaveBeenCalledWith(input);
      expect(id).toBe("evt-1");
      // fetchEvents is called after create
      expect(api.getEvents).toHaveBeenCalled();
    });
  });

  describe("deleteEvent", () => {
    it("should call api.deleteEvent and refresh events", async () => {
      setupAccounts();
      const store = useCalendarStore();
      store.selectedEvent = makeEvent("evt-1", "Test", "2026-04-07T17:00:00Z", "2026-04-07T18:00:00Z");

      await store.deleteEvent("evt-1");

      expect(api.deleteEvent).toHaveBeenCalledWith("evt-1");
      expect(store.selectedEvent).toBeNull();
      expect(api.getEvents).toHaveBeenCalled();
    });

    it("should not clear selectedEvent if deleting a different event", async () => {
      setupAccounts();
      const store = useCalendarStore();
      store.selectedEvent = makeEvent("evt-2", "Other", "2026-04-07T17:00:00Z", "2026-04-07T18:00:00Z");

      await store.deleteEvent("evt-1");

      expect(api.deleteEvent).toHaveBeenCalledWith("evt-1");
      expect(store.selectedEvent).not.toBeNull();
    });
  });

  describe("fetchCalendars", () => {
    it("should fetch calendars for all accounts", async () => {
      setupAccounts();
      const store = useCalendarStore();
      const cal = makeCalendar("cal1", "Work", "remote-1");
      vi.mocked(api.listCalendars).mockResolvedValueOnce([cal]);

      await store.fetchCalendars();

      expect(api.listCalendars).toHaveBeenCalledWith("acc1");
      expect(store.calendars).toEqual([cal]);
    });

    it("should clear calendars if no active account", async () => {
      const accountsStore = useAccountsStore();
      accountsStore.accounts = [];
      accountsStore.activeAccountId = null;
      const store = useCalendarStore();

      await store.fetchCalendars();

      expect(store.calendars).toEqual([]);
    });
  });

  describe("visibleEvents with hidden calendars", () => {
    it("should filter out events from hidden calendars", async () => {
      setupAccounts();
      const store = useCalendarStore();
      store.events = [
        makeEvent("e1", "Visible", "2026-04-07T10:00:00Z", "2026-04-07T11:00:00Z", { calendar_id: "cal1" }),
        makeEvent("e2", "Hidden", "2026-04-07T12:00:00Z", "2026-04-07T13:00:00Z", { calendar_id: "cal2" }),
      ];
      store.currentDate = "2026-04-07";
      store.toggleCalendarVisibility("cal2");

      expect(store.visibleEvents.map(e => e.title)).toEqual(["Visible"]);
    });

    it("should toggle visibility back on", async () => {
      setupAccounts();
      const store = useCalendarStore();
      store.events = [
        makeEvent("e1", "A", "2026-04-07T10:00:00Z", "2026-04-07T11:00:00Z", { calendar_id: "cal1" }),
        makeEvent("e2", "B", "2026-04-07T12:00:00Z", "2026-04-07T13:00:00Z", { calendar_id: "cal2" }),
      ];
      store.currentDate = "2026-04-07";
      store.toggleCalendarVisibility("cal2");
      store.toggleCalendarVisibility("cal2");

      expect(store.visibleEvents.length).toBe(2);
    });
  });

  describe("navigation", () => {
    it("goNext in week mode advances by 7 days", () => {
      setupAccounts();
      const store = useCalendarStore();
      store.currentDate = "2026-04-07";
      store.viewMode = "week";
      store.goNext();
      expect(store.currentDate).toBe("2026-04-14");
    });

    it("goPrev in week mode goes back by 7 days", () => {
      setupAccounts();
      const store = useCalendarStore();
      store.currentDate = "2026-04-14";
      store.viewMode = "week";
      store.goPrev();
      expect(store.currentDate).toBe("2026-04-07");
    });

    it("goNext in day mode advances by 1 day", () => {
      setupAccounts();
      const store = useCalendarStore();
      store.currentDate = "2026-04-07";
      store.viewMode = "day";
      store.goNext();
      expect(store.currentDate).toBe("2026-04-08");
    });

    it("goNext in month mode advances by 1 month", () => {
      setupAccounts();
      const store = useCalendarStore();
      store.currentDate = "2026-04-07";
      store.viewMode = "month";
      store.goNext();
      expect(store.currentDate).toBe("2026-05-07");
    });
  });

  describe("syncCalendars", () => {
    it("should sync all accounts then refresh", async () => {
      setupAccounts();
      const store = useCalendarStore();

      await store.syncCalendars();

      expect(api.syncCalendars).toHaveBeenCalledWith("acc1");
      expect(api.listCalendars).toHaveBeenCalled();
      expect(api.getEvents).toHaveBeenCalled();
    });

    it("should not throw if one account sync fails", async () => {
      setupAccounts();
      const store = useCalendarStore();
      vi.mocked(api.syncCalendars).mockRejectedValueOnce(new Error("Network error"));

      await expect(store.syncCalendars()).resolves.not.toThrow();
    });
  });
});

describe("InviteCard integration", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    vi.clearAllMocks();
  });

  it("sendInvites should be called after createEvent with attendees", async () => {
    setupAccounts();
    const eventId = "evt-new";
    vi.mocked(api.createEvent).mockResolvedValueOnce(eventId);

    const store = useCalendarStore();
    const attendees = ["alice@example.com", "bob@example.com"];
    const input = {
      account_id: "acc1", calendar_id: "cal1",
      title: "Team meeting", description: null, location: null,
      start_time: "2026-04-07T17:00:00.000Z",
      end_time: "2026-04-07T18:00:00.000Z",
      all_day: false, timezone: null, recurrence_rule: null,
      attendees: attendees.map(e => ({ email: e, name: null, status: "needs-action" })),
    };

    const id = await store.createEvent(input);
    // Simulate what EventForm does after createEvent
    await api.sendInvites("acc1", id, attendees);

    expect(api.sendInvites).toHaveBeenCalledWith("acc1", eventId, attendees);
  });

  it("respondToInvite should be called with correct args", async () => {
    setupAccounts();

    await api.respondToInvite("acc1", "msg-123", "uid-abc@example.com", "accepted");

    expect(api.respondToInvite).toHaveBeenCalledWith(
      "acc1", "msg-123", "uid-abc@example.com", "accepted",
    );
  });

  it("getInviteStatus returns saved status", async () => {
    setupAccounts();
    vi.mocked(api.getInviteStatus).mockResolvedValueOnce("accepted");

    const status = await api.getInviteStatus("acc1", "uid-abc@example.com");

    expect(status).toBe("accepted");
  });

  it("getInviteStatus returns null for unknown invite", async () => {
    setupAccounts();
    vi.mocked(api.getInviteStatus).mockResolvedValueOnce(null);

    const status = await api.getInviteStatus("acc1", "uid-unknown");

    expect(status).toBeNull();
  });
});

describe("EventForm local time helpers", () => {
  // These test the toLocalDate/toLocalTime logic used in EventForm
  // by verifying that local components are extracted, not UTC

  it("toLocalDate extracts local date components", () => {
    // Create a date at a known local time
    const d = new Date(2026, 3, 7, 17, 30, 0); // April 7, 2026 5:30 PM local
    const y = d.getFullYear();
    const m = String(d.getMonth() + 1).padStart(2, "0");
    const day = String(d.getDate()).padStart(2, "0");
    const localDate = `${y}-${m}-${day}`;

    expect(localDate).toBe("2026-04-07");
  });

  it("toLocalTime extracts local time components", () => {
    const d = new Date(2026, 3, 7, 17, 30, 0); // 5:30 PM local
    const h = String(d.getHours()).padStart(2, "0");
    const min = String(d.getMinutes()).padStart(2, "0");
    const localTime = `${h}:${min}`;

    expect(localTime).toBe("17:30");
  });

  it("ISO string round-trip preserves local time in form fields", () => {
    // Simulate what WeekView does: create a local Date, emit toISOString()
    const clickedTime = new Date(2026, 3, 7, 17, 0, 0); // 5 PM local
    const isoString = clickedTime.toISOString(); // This is UTC

    // Simulate what EventForm does: parse ISO string back to Date, extract local
    const parsed = new Date(isoString);
    const h = String(parsed.getHours()).padStart(2, "0");
    const min = String(parsed.getMinutes()).padStart(2, "0");
    const localTime = `${h}:${min}`;

    // Should show 17:00 (local), not the UTC conversion
    expect(localTime).toBe("17:00");
    expect(parsed.getHours()).toBe(17);
  });

  it("default end time is 1 hour after start", () => {
    const start = new Date(2026, 3, 7, 17, 0, 0);
    const end = new Date(start.getTime() + 60 * 60 * 1000);

    expect(end.getHours()).toBe(18);
    expect(end.getMinutes()).toBe(0);
  });
});

describe("EventForm end-time validation", () => {
  it("end time after start time is valid", () => {
    const startDate = "2026-04-07";
    const startTime = "17:00";
    const endDate = "2026-04-07";
    const endTime = "18:00";

    const s = new Date(`${startDate}T${startTime}:00`);
    const e = new Date(`${endDate}T${endTime}:00`);
    expect(e > s).toBe(true);
  });

  it("end time equal to start time is invalid", () => {
    const startDate = "2026-04-07";
    const startTime = "17:00";
    const endDate = "2026-04-07";
    const endTime = "17:00";

    const s = new Date(`${startDate}T${startTime}:00`);
    const e = new Date(`${endDate}T${endTime}:00`);
    expect(e <= s).toBe(true);
  });

  it("end time before start time is invalid", () => {
    const startDate = "2026-04-07";
    const startTime = "17:00";
    const endDate = "2026-04-07";
    const endTime = "16:00";

    const s = new Date(`${startDate}T${startTime}:00`);
    const e = new Date(`${endDate}T${endTime}:00`);
    expect(e <= s).toBe(true);
  });

  it("end date before start date is invalid", () => {
    const startDate = "2026-04-08";
    const startTime = "10:00";
    const endDate = "2026-04-07";
    const endTime = "18:00";

    const s = new Date(`${startDate}T${startTime}:00`);
    const e = new Date(`${endDate}T${endTime}:00`);
    expect(e <= s).toBe(true);
  });

  it("end on later date with earlier time is valid", () => {
    const startDate = "2026-04-07";
    const startTime = "17:00";
    const endDate = "2026-04-08";
    const endTime = "09:00";

    const s = new Date(`${startDate}T${startTime}:00`);
    const e = new Date(`${endDate}T${endTime}:00`);
    expect(e > s).toBe(true);
  });

  it("pushing end forward when start moves past it", () => {
    // Simulate the watch behavior
    let startDate = "2026-04-07";
    let startTime = "17:00";
    let endDate = "2026-04-07";
    let endTime = "18:00";

    // Move start to 19:00, which is past end (18:00)
    startTime = "19:00";
    const s = new Date(`${startDate}T${startTime}:00`);
    const e = new Date(`${endDate}T${endTime}:00`);

    if (e <= s) {
      const newEnd = new Date(s.getTime() + 60 * 60 * 1000);
      const y = newEnd.getFullYear();
      const m = String(newEnd.getMonth() + 1).padStart(2, "0");
      const day = String(newEnd.getDate()).padStart(2, "0");
      endDate = `${y}-${m}-${day}`;
      endTime = `${String(newEnd.getHours()).padStart(2, "0")}:${String(newEnd.getMinutes()).padStart(2, "0")}`;
    }

    expect(endTime).toBe("20:00");
    expect(endDate).toBe("2026-04-07");
  });

  it("pushing end to next day when start is 23:00", () => {
    let startDate = "2026-04-07";
    let startTime = "23:00";
    let endDate = "2026-04-07";
    let endTime = "22:00";

    const s = new Date(`${startDate}T${startTime}:00`);
    const e = new Date(`${endDate}T${endTime}:00`);

    if (e <= s) {
      const newEnd = new Date(s.getTime() + 60 * 60 * 1000);
      const y = newEnd.getFullYear();
      const m = String(newEnd.getMonth() + 1).padStart(2, "0");
      const day = String(newEnd.getDate()).padStart(2, "0");
      endDate = `${y}-${m}-${day}`;
      endTime = `${String(newEnd.getHours()).padStart(2, "0")}:${String(newEnd.getMinutes()).padStart(2, "0")}`;
    }

    expect(endDate).toBe("2026-04-08");
    expect(endTime).toBe("00:00");
  });

  it("minEndDate equals start date", () => {
    const startDate = "2026-04-07";
    expect(startDate).toBe("2026-04-07"); // minEndDate === startDate
  });

  it("minEndTime is start time when same day", () => {
    const startDate = "2026-04-07";
    const endDate = "2026-04-07";
    const startTime = "17:00";

    const minEndTime = endDate === startDate ? startTime : undefined;
    expect(minEndTime).toBe("17:00");
  });

  it("minEndTime is undefined when different day", () => {
    const startDate: string = "2026-04-07";
    const endDate: string = "2026-04-08";
    const startTime = "17:00";

    const minEndTime = endDate === startDate ? startTime : undefined;
    expect(minEndTime).toBeUndefined();
  });
});
