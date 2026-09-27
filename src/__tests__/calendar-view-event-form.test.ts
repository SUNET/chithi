import { beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { nextTick } from "vue";

vi.mock("@/lib/tauri", () => ({
  listRoomSuggestions: vi.fn().mockResolvedValue([]),
  checkRoomAvailability: vi.fn(),
  getParticipantSchedules: vi.fn().mockResolvedValue([]),
  meetCreateUrl: vi.fn(),
  meetDiscardPending: vi.fn().mockResolvedValue(undefined),
  createEvent: vi.fn().mockResolvedValue("event"),
  getEvents: vi.fn().mockResolvedValue([]),
  listCalendarOccurrences: vi.fn().mockResolvedValue({
    occurrences: [], has_more: false, needs_hydration: [], unresolved: [],
  }),
  listCalendars: vi.fn().mockResolvedValue([]),
  listArchivedGraphCalendars: vi.fn().mockResolvedValue([]),
  acknowledgeArchivedGraphCalendar: vi.fn().mockResolvedValue(undefined),
  syncCalendars: vi.fn().mockResolvedValue(undefined),
  sendInvites: vi.fn(),
}));

import CalendarView from "@/views/CalendarView.vue";
import EventForm from "@/components/calendar/EventForm.vue";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "@/stores/accounts";
import { useCalendarStore } from "@/stores/calendar";
import { usePlatformStore } from "@/stores/platform";
import { useUiStore } from "@/stores/ui";

describe("CalendarView responsive event form lifecycle", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    vi.clearAllMocks();

    useAccountsStore().accounts = [
      {
        id: "calendar-account",
        display_name: "Calendar",
        email: "calendar@example.test",
        username: "calendar@example.test",
        provider: "generic",
        mail_protocol: "imap",
        enabled: true,
        mail_sync_interval_seconds: null,
        calendar_sync_interval_seconds: null,
        contacts_sync_interval_seconds: null,
        has_calendar_binding: true,
        has_contacts_binding: false,
        meet_protocol: "",
      },
      {
        id: "meet-account",
        display_name: "Meet",
        email: "meet@example.test",
        username: "meet@example.test",
        provider: "generic",
        mail_protocol: "imap",
        enabled: true,
        mail_sync_interval_seconds: null,
        calendar_sync_interval_seconds: null,
        contacts_sync_interval_seconds: null,
        has_calendar_binding: false,
        has_contacts_binding: false,
        meet_protocol: "zoom",
      },
    ];
    useCalendarStore().calendars = [
      {
        id: "calendar",
        account_id: "calendar-account",
        name: "Calendar",
        color: "#123456",
        is_default: true,
        remote_id: null,
        is_subscribed: true,
      },
    ];
    useUiStore().displayTimezone = "UTC";
    usePlatformStore().width = 1280;
  });

  it("renders cached events before starting network sync", async () => {
    const calendarStore = useCalendarStore();
    let resolveEvents!: () => void;
    vi.spyOn(calendarStore, "fetchCalendars").mockResolvedValue();
    vi.spyOn(calendarStore, "fetchEvents").mockImplementation(
      () => new Promise<void>((resolve) => { resolveEvents = resolve; }),
    );
    const sync = vi.spyOn(calendarStore, "syncCalendars").mockResolvedValue();
    const start = vi.spyOn(calendarStore, "startCalendarSync").mockResolvedValue();

    const wrapper = mount(CalendarView, {
      global: {
        stubs: {
          CalendarSidebar: true,
          WeekView: true,
          MonthView: true,
          EventDetail: true,
          MobileAppBar: true,
          MobileIconButton: true,
          EventForm: true,
        },
      },
    });
    await flushPromises();

    expect(calendarStore.fetchEvents).toHaveBeenCalledOnce();
    expect(sync).not.toHaveBeenCalled();
    expect(start).not.toHaveBeenCalled();

    resolveEvents();
    await flushPromises();

    expect(sync).toHaveBeenCalledOnce();
    expect(start).toHaveBeenCalledOnce();
    wrapper.unmount();
  });

  it("keeps the previous month visible while its destination is loading", async () => {
    const calendarStore = useCalendarStore();
    calendarStore.currentDate = "2026-04-07";
    calendarStore.viewMode = "month";
    calendarStore.events = [{
      id: "april", account_id: "calendar-account", calendar_id: "calendar",
      uid: "april", title: "April event", description: null, location: null,
      start_time: "2026-04-07T10:00:00Z", end_time: "2026-04-07T11:00:00Z",
      all_day: false, timezone: null, recurrence_rule: null,
      recurrence_kind: "standalone", organizer_email: null, attendees_json: null,
      my_status: null, source_message_id: null,
    }];
    vi.spyOn(calendarStore, "fetchCalendars").mockResolvedValue();
    vi.spyOn(calendarStore, "fetchEvents").mockResolvedValue();
    vi.spyOn(calendarStore, "syncCalendars").mockResolvedValue();
    vi.spyOn(calendarStore, "startCalendarSync").mockResolvedValue();
    let resolveMay!: (events: []) => void;
    const mayRead = new Promise<[]>((resolve) => { resolveMay = resolve; });
    vi.mocked(api.getEvents).mockImplementation((id) =>
      id === "calendar-account" ? mayRead : Promise.resolve([]));
    const wrapper = mount(CalendarView, {
      global: { stubs: {
        CalendarSidebar: true, WeekView: true, EventDetail: true,
        MobileAppBar: true, MobileIconButton: true, EventForm: true,
      } },
    });
    await flushPromises();
    expect(wrapper.find("[data-testid=cal-event-april]").exists()).toBe(true);

    await wrapper.find("[data-testid=cal-btn-next]").trigger("click");
    expect(wrapper.find(".current-date").text()).toBe("April 2026");
    expect(wrapper.find("[role=status]").text()).toContain("Loading May 2026");
    expect(wrapper.find("[data-testid=cal-month-cell-2026-04-07]").exists()).toBe(true);
    expect(wrapper.find("[data-testid=cal-event-april]").exists()).toBe(true);
    expect(wrapper.find(".calendar-content").attributes("inert")).toBeDefined();

    resolveMay([]);
    await flushPromises();
    expect(wrapper.find(".current-date").text()).toBe("May 2026");
    expect(wrapper.find("[role=status]").exists()).toBe(false);
    expect(wrapper.find(".calendar-content").attributes("inert")).toBeUndefined();
    expect(wrapper.find("[data-testid=cal-event-april]").exists()).toBe(false);
    wrapper.unmount();
  });

  it("renders events in leading and trailing cells of a six-week month", async () => {
    const calendarStore = useCalendarStore();
    calendarStore.currentDate = "2026-08-15";
    calendarStore.viewMode = "month";
    const event = (id: string, start: string) => ({
      id, account_id: "calendar-account", calendar_id: "calendar", uid: id,
      title: id, description: null, location: null,
      start_time: `${start}T10:00:00Z`, end_time: `${start}T11:00:00Z`,
      all_day: false, timezone: null, recurrence_rule: null,
      recurrence_kind: "standalone" as const, organizer_email: null,
      attendees_json: null, my_status: null, source_message_id: null,
    });
    calendarStore.events = [event("leading", "2026-07-26"), event("trailing", "2026-09-05")];
    vi.spyOn(calendarStore, "fetchCalendars").mockResolvedValue();
    vi.spyOn(calendarStore, "fetchEvents").mockResolvedValue();
    vi.spyOn(calendarStore, "syncCalendars").mockResolvedValue();
    vi.spyOn(calendarStore, "startCalendarSync").mockResolvedValue();
    const wrapper = mount(CalendarView, {
      global: { stubs: {
        CalendarSidebar: true, WeekView: true, EventDetail: true,
        MobileAppBar: true, MobileIconButton: true, EventForm: true,
      } },
    });
    await flushPromises();
    for (const [day, id] of [["2026-07-26", "leading"], ["2026-09-05", "trailing"]]) {
      expect(wrapper.find(`[data-testid=cal-month-cell-${day}]`).exists()).toBe(true);
      expect(wrapper.find(`[data-testid=cal-event-${id}]`).exists()).toBe(true);
    }
    wrapper.unmount();
  });

  it("keeps the incomplete-series warning visible and offers verification", async () => {
    const calendarStore = useCalendarStore();
    calendarStore.viewMode = "month";
    calendarStore.unresolvedOccurrences = [{
      event_id: "child", calendar_id: "calendar", account_id: "calendar-account",
    }];
    vi.spyOn(calendarStore, "fetchCalendars").mockResolvedValue();
    vi.spyOn(calendarStore, "fetchEvents").mockResolvedValue();
    vi.spyOn(calendarStore, "syncCalendars").mockResolvedValue();
    vi.spyOn(calendarStore, "startCalendarSync").mockResolvedValue();
    const verify = vi.spyOn(calendarStore, "repairIncompleteOccurrences").mockResolvedValue();
    const wrapper = mount(CalendarView, {
      global: { stubs: {
        CalendarSidebar: true, WeekView: true, MonthView: true,
        EventDetail: true, MobileAppBar: true, MobileIconButton: true,
        EventForm: true,
      } },
    });
    await flushPromises();
    const warning = wrapper.find(".calendar-incomplete");
    expect(warning.attributes("role")).toBe("alert");
    expect(warning.text()).toContain("Some recurring dates may be incomplete until verified");
    await warning.find("button").trigger("click");
    expect(verify).toHaveBeenCalledOnce();
    expect(wrapper.find(".calendar-incomplete").exists()).toBe(true);
    wrapper.unmount();
  });

  it("shows archived Graph data outside the sidebar on desktop and mobile", async () => {
    const calendarStore = useCalendarStore();
    calendarStore.archivedGraphCalendars = [{
      id: "archived", account_id: "calendar-account", name: "Calendar",
      retained_event_count: 245, replay_address_count: 3,
      acknowledged: false,
    }];
    vi.spyOn(calendarStore, "fetchCalendars").mockResolvedValue();
    vi.spyOn(calendarStore, "fetchEvents").mockResolvedValue();
    vi.spyOn(calendarStore, "syncCalendars").mockResolvedValue();
    vi.spyOn(calendarStore, "startCalendarSync").mockResolvedValue();
    const wrapper = mount(CalendarView, {
      global: { stubs: {
        CalendarSidebar: true, WeekView: true, MonthView: true,
        EventDetail: true, MobileAppBar: true, MobileIconButton: true,
        EventForm: true,
      } },
    });
    await flushPromises();
    const notice = wrapper.get(".archived-graph-notice");
    expect(notice.attributes("role")).toBe("alert");
    expect(notice.text()).toContain("not shown in the live calendar");
    expect(notice.text()).toContain("245 cached event(s)");
    expect(notice.get("button").text()).toBe("Acknowledge");
    expect(wrapper.findAll("calendar-sidebar-stub")).toHaveLength(1);

    usePlatformStore().width = 500;
    await nextTick();
    expect(wrapper.findAll(".archived-graph-notice")).toHaveLength(1);
    expect(wrapper.get(".archived-graph-notice").text()).toContain("245 cached event(s)");
    vi.mocked(api.listArchivedGraphCalendars).mockResolvedValueOnce([{
      ...calendarStore.archivedGraphCalendars[0], acknowledged: true,
    }]);
    await wrapper.get(".archived-graph-notice button").trigger("click");
    await flushPromises();
    expect(api.acknowledgeArchivedGraphCalendar).toHaveBeenCalledWith(
      "calendar-account", "archived");
    expect(wrapper.find(".archived-graph-notice").exists()).toBe(false);
    expect(calendarStore.archivedGraphCalendars).toHaveLength(1);
    wrapper.unmount();
  });

  it("preserves a pending meeting when the responsive branch switches", async () => {
    vi.mocked(api.meetCreateUrl).mockResolvedValue({
      lifecycle_id: "lifecycle",
      account_id: "meet-account",
      protocol: "zoom",
      meeting_id: "meeting",
      join_url: "https://zoom.example/meeting",
    });
    const calendarStore = useCalendarStore();
    vi.spyOn(calendarStore, "fetchCalendars").mockResolvedValue();
    vi.spyOn(calendarStore, "fetchEvents").mockResolvedValue();
    vi.spyOn(calendarStore, "syncCalendars").mockResolvedValue();
    vi.spyOn(calendarStore, "startCalendarSync").mockResolvedValue();

    const wrapper = mount(CalendarView, {
      global: {
        stubs: {
          CalendarSidebar: { template: "<div />" },
          WeekView: { template: "<div />" },
          MonthView: { template: "<div />" },
          EventDetail: { template: "<div />" },
          MobileAppBar: { template: "<div><slot name='leading' /><slot name='trailing' /></div>" },
          MobileIconButton: { template: "<button><slot /></button>" },
          RecurrenceEditor: { template: "<div />" },
          AttendeeEditor: { template: "<div />" },
          TimeInput: { template: "<input />" },
          DateInput: { template: "<input />" },
          Select: { template: "<div />" },
        },
      },
    });

    await wrapper.get('[data-testid="cal-btn-new-event"]').trigger("click");
    const originalForm = wrapper.getComponent(EventForm);
    await originalForm
      .get('[data-testid="event-form-meet-meet-account"]')
      .trigger("click");
    await flushPromises();
    expect(originalForm.get('[data-testid="event-form-location"]').element)
      .toHaveProperty("value", "https://zoom.example/meeting");

    usePlatformStore().width = 500;
    await nextTick();

    const responsiveForm = wrapper.getComponent(EventForm);
    expect(wrapper.findAllComponents(EventForm)).toHaveLength(1);
    expect(responsiveForm.get('[data-testid="event-form-location"]').element)
      .toHaveProperty("value", "https://zoom.example/meeting");
    expect(api.meetDiscardPending).not.toHaveBeenCalled();
  });
});
