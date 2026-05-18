import { beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

vi.mock("@/lib/tauri", () => ({
  listRoomSuggestions: vi.fn().mockResolvedValue([]),
  checkRoomAvailability: vi.fn().mockResolvedValue({ state: "unknown", busy_start: null, busy_end: null }),
  meetCreateUrl: vi.fn(),
  sendInvites: vi.fn(),
}));

import EventForm from "@/components/calendar/EventForm.vue";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "@/stores/accounts";
import { useCalendarStore } from "@/stores/calendar";
import { useUiStore } from "@/stores/ui";

describe("EventForm room suggestions", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    vi.clearAllMocks();

    const accountsStore = useAccountsStore();
    accountsStore.accounts = [
      {
        id: "acc-o365",
        display_name: "Work",
        email: "me@work.example",
        username: "me@work.example",
        provider: "o365",
        mail_protocol: "graph",
        enabled: true,
        mail_sync_interval_seconds: null,
        calendar_sync_interval_seconds: null,
        contacts_sync_interval_seconds: null,
        has_calendar_binding: true,
        has_contacts_binding: false,
        meet_protocol: "",
      },
    ];
    accountsStore.activeAccountId = "acc-o365";

    const calendarStore = useCalendarStore();
    calendarStore.calendars = [
      {
        id: "cal-1",
        account_id: "acc-o365",
        name: "Calendar",
        color: "#4285f4",
        is_default: true,
        remote_id: null,
        is_subscribed: true,
      },
    ];

    const uiStore = useUiStore();
    uiStore.displayTimezone = "UTC";
  });

  it("loads O365 room suggestions into the location datalist", async () => {
    vi.mocked(api.listRoomSuggestions).mockResolvedValueOnce([
      { name: "Board Room", address: "board@example.com" },
      { name: "Focus Room", address: "focus@example.com" },
    ]);

    const wrapper = mount(EventForm, {
      global: {
        stubs: {
          RecurrenceEditor: { template: "<div />" },
          AttendeeEditor: { template: "<div />" },
          TimeInput: { template: "<input />" },
          DateInput: { template: "<input />" },
          Select: { template: "<div />" },
        },
      },
    });

    await flushPromises();

    expect(api.listRoomSuggestions).toHaveBeenCalledWith("acc-o365");
    const datalist = wrapper.get('[data-testid="event-form-room-suggestions"]');
    const options = datalist.findAll("option");
    expect(options).toHaveLength(2);
    expect(options[0].attributes("value")).toBe("Board Room");
    expect(options[0].text()).toContain("board@example.com");
  });

  it("shows room availability for the selected time", async () => {
    vi.mocked(api.listRoomSuggestions).mockResolvedValueOnce([
      { name: "Board Room", address: "board@example.com" },
    ]);
    vi.mocked(api.checkRoomAvailability).mockResolvedValueOnce({
      state: "busy",
      busy_start: "2026-05-19T10:30:00.0000000",
      busy_end: "2026-05-19T11:00:00.0000000",
    });

    const wrapper = mount(EventForm, {
      props: {
        initialStart: "2026-05-19T10:00:00Z",
      },
      global: {
        stubs: {
          RecurrenceEditor: { template: "<div />" },
          AttendeeEditor: { template: "<div />" },
          TimeInput: { template: "<input />" },
          DateInput: { template: "<input />" },
          Select: { template: "<div />" },
        },
      },
    });

    await flushPromises();
    await wrapper.get('[data-testid="event-form-location"]').setValue("Board Room");
    await flushPromises();

    expect(api.checkRoomAvailability).toHaveBeenCalledWith(
      "acc-o365",
      "board@example.com",
      "2026-05-19T10:00:00.000Z",
      "2026-05-19T11:00:00.000Z",
    );
    expect(wrapper.get('[data-testid="event-form-room-availability"]').text()).toContain("Busy");
  });

  it("checks all-day room availability in the display timezone, not UTC midnight", async () => {
    const uiStore = useUiStore();
    uiStore.displayTimezone = "Europe/Stockholm";

    vi.mocked(api.listRoomSuggestions).mockResolvedValueOnce([
      { name: "Board Room", address: "board@example.com" },
    ]);

    const wrapper = mount(EventForm, {
      props: {
        initialStart: "2026-05-19T10:00:00Z",
      },
      global: {
        stubs: {
          RecurrenceEditor: { template: "<div />" },
          AttendeeEditor: { template: "<div />" },
          TimeInput: { template: "<input />" },
          DateInput: { template: "<input />" },
          Select: { template: "<div />" },
        },
      },
    });

    await flushPromises();
    await wrapper.get('[data-testid="event-form-location"]').setValue("Board Room");
    await wrapper.get('[data-testid="event-form-allday"]').setValue(true);
    await flushPromises();

    // May 19 (all-day) in Stockholm (CEST, UTC+2) runs from the
    // previous day 22:00Z, not 2026-05-19T00:00:00Z.
    const calls = vi.mocked(api.checkRoomAvailability).mock.calls;
    const lastCall = calls[calls.length - 1];
    expect(lastCall?.[0]).toBe("acc-o365");
    expect(lastCall?.[1]).toBe("board@example.com");
    expect(lastCall?.[2]).toBe("2026-05-18T22:00:00.000Z");
    expect(lastCall?.[3]).toBe("2026-05-19T21:59:00.000Z");
  });
});