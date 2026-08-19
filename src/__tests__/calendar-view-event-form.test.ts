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
  listCalendars: vi.fn().mockResolvedValue([]),
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
