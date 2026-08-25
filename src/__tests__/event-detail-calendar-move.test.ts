import { beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

vi.mock("@/lib/tauri", () => ({
  updateEvent: vi.fn().mockResolvedValue(undefined),
  deleteEvent: vi.fn().mockResolvedValue(undefined),
  createEvent: vi.fn().mockResolvedValue("evt-new"),
  getEvents: vi.fn().mockResolvedValue([]),
  listCalendars: vi.fn().mockResolvedValue([]),
  listAccounts: vi.fn().mockResolvedValue([]),
  syncCalendars: vi.fn().mockResolvedValue(undefined),
  sendInvites: vi.fn().mockResolvedValue(undefined),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({
  message: vi.fn().mockResolvedValue("No"),
}));

import EventDetail from "@/components/calendar/EventDetail.vue";
import * as api from "@/lib/tauri";
import { occurrenceId } from "@/lib/rrule";
import { useAccountsStore } from "@/stores/accounts";
import { useCalendarStore } from "@/stores/calendar";
import { useUiStore } from "@/stores/ui";

// Regression for "recurring event can't be moved to another calendar":
// the sidebar drag was the only move path and recurring occurrences
// couldn't use it. EventDetail now exposes a calendar picker in edit
// mode that routes through moveEventToCalendar with the master id.

const masterEvent = {
  id: "evt-r",
  account_id: "acc1",
  calendar_id: "cal1",
  uid: "evt-r@chithi",
  title: "OCM checkpoint meeting",
  description: null,
  location: null,
  start_time: "2026-08-25T09:00:00.000Z",
  end_time: "2026-08-25T10:00:00.000Z",
  all_day: false,
  timezone: null,
  recurrence_rule: "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU",
  organizer_email: null,
  attendees_json: null,
  my_status: null,
  source_message_id: null,
};

function makeAccount(id: string, email: string) {
  return {
    id, display_name: email, email, username: email,
    provider: "generic" as const, mail_protocol: "jmap" as const, enabled: true,
    mail_sync_interval_seconds: null,
    calendar_sync_interval_seconds: null,
    contacts_sync_interval_seconds: null,
    has_calendar_binding: true,
    has_contacts_binding: false,
    meet_protocol: "" as const,
  };
}

function setupStores() {
  useAccountsStore().accounts = [
    makeAccount("acc1", "kano@example.test"),
    makeAccount("acc2", "other@example.test"),
  ];
  useUiStore().displayTimezone = "UTC";

  const calendarStore = useCalendarStore();
  calendarStore.calendars = [
    { id: "cal1", account_id: "acc1", name: "Calendar", color: "#000", is_default: true, remote_id: null, is_subscribed: true },
    { id: "cal2", account_id: "acc1", name: "Stalwart Calendar", color: "#000", is_default: false, remote_id: "b", is_subscribed: true },
    { id: "cal3", account_id: "acc2", name: "Elsewhere", color: "#000", is_default: true, remote_id: null, is_subscribed: true },
  ];
  calendarStore.events = [masterEvent];
  // The detail panel is opened from a clicked occurrence (synthetic id,
  // occurrence-local times) — exactly what onEventClick selects.
  calendarStore.selectedEvent = {
    ...masterEvent,
    id: occurrenceId("evt-r", new Date("2026-09-08T09:00:00.000Z")),
    start_time: "2026-09-08T09:00:00.000Z",
    end_time: "2026-09-08T10:00:00.000Z",
  };
  return calendarStore;
}

function mountDetail() {
  return mount(EventDetail, {
    global: {
      stubs: {
        TimeInput: { template: "<input />" },
        DateInput: { template: "<input />" },
        LinkifiedText: { template: "<span />" },
      },
    },
  });
}

describe("EventDetail calendar picker", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    vi.clearAllMocks();
    vi.mocked(api.getEvents).mockImplementation(async (accountId: string) =>
      accountId === "acc1" ? [masterEvent] : [],
    );
  });

  it("lists all subscribed calendars in edit mode", async () => {
    setupStores();
    const wrapper = mountDetail();

    await wrapper.get(".btn-edit").trigger("click");

    const options = wrapper
      .get('[data-testid="event-detail-calendar"]')
      .findAll("option");
    expect(options.map((o) => o.attributes("value"))).toEqual([
      "cal1", "cal2", "cal3",
    ]);
    expect((wrapper.get('[data-testid="event-detail-calendar"]').element as HTMLSelectElement).value).toBe("cal1");
  });

  it("moves the series (same account) when a different calendar is picked", async () => {
    setupStores();
    const wrapper = mountDetail();

    await wrapper.get(".btn-edit").trigger("click");
    await wrapper.get('[data-testid="event-detail-calendar"]').setValue("cal2");
    await wrapper.get('[data-testid="event-form-save"]').trigger("click");
    await flushPromises();

    // Field edits are saved against the master id first...
    expect(api.updateEvent).toHaveBeenNthCalledWith(
      1,
      "evt-r",
      expect.objectContaining({ title: "OCM checkpoint meeting", calendar_id: "cal1" }),
    );
    // ...then the move patches the calendar id on the master.
    expect(api.updateEvent).toHaveBeenNthCalledWith(2, "evt-r", { calendar_id: "cal2" });
    expect(wrapper.emitted("close")).toBeTruthy();
  });

  it("saves a later occurrence's edits against the master dates and attendees", async () => {
    setupStores();
    const calendarStore = useCalendarStore();
    calendarStore.events = [{
      ...masterEvent,
      attendees_json: JSON.stringify([{ email: "guest@example.test", name: "Guest", status: "accepted" }]),
    }];
    calendarStore.selectedEvent = {
      ...calendarStore.events[0],
      id: occurrenceId("evt-r", new Date("2026-09-08T09:00:00.000Z")),
      start_time: "2026-09-08T09:00:00.000Z",
      end_time: "2026-09-08T10:00:00.000Z",
    };
    const wrapper = mountDetail();

    await wrapper.get(".btn-edit").trigger("click");
    await wrapper.get('[data-testid="event-form-title"]').setValue("Renamed series");
    await wrapper.get('[data-testid="event-form-save"]').trigger("click");
    await flushPromises();

    expect(api.updateEvent).toHaveBeenCalledWith("evt-r", expect.objectContaining({
      title: "Renamed series",
      start_time: "2026-08-25T09:00:00.000Z",
      end_time: "2026-08-25T10:00:00.000Z",
      attendees: [{ email: "guest@example.test", name: "Guest", status: "accepted" }],
    }));
  });

  it("recreates the series on the destination for cross-account moves", async () => {
    setupStores();
    const wrapper = mountDetail();

    await wrapper.get(".btn-edit").trigger("click");
    await wrapper.get('[data-testid="event-detail-calendar"]').setValue("cal3");
    await wrapper.get('[data-testid="event-form-save"]').trigger("click");
    await flushPromises();

    expect(api.createEvent).toHaveBeenCalledWith(
      expect.objectContaining({
        account_id: "acc2",
        calendar_id: "cal3",
        recurrence_rule: "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU",
        attendees: [],
        // Master times — the clicked occurrence's dates must not leak in.
        start_time: "2026-08-25T09:00:00.000Z",
        end_time: "2026-08-25T10:00:00.000Z",
      }),
    );
    expect(api.deleteEvent).toHaveBeenCalledWith("evt-r");
    expect(wrapper.emitted("close")).toBeTruthy();
  });

  it("does not move when the calendar is unchanged", async () => {
    setupStores();
    const wrapper = mountDetail();

    await wrapper.get(".btn-edit").trigger("click");
    await wrapper.get('[data-testid="event-form-save"]').trigger("click");
    await flushPromises();

    expect(api.updateEvent).toHaveBeenCalledTimes(1);
    expect(api.createEvent).not.toHaveBeenCalled();
    expect(api.deleteEvent).not.toHaveBeenCalled();
  });
});
