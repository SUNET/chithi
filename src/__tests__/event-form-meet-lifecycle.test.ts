import { beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

vi.mock("@/lib/tauri", () => ({
  listRoomSuggestions: vi.fn().mockResolvedValue([]),
  checkRoomAvailability: vi.fn(),
  getParticipantSchedules: vi.fn().mockResolvedValue([]),
  meetCreateUrl: vi.fn(),
  meetDiscardPending: vi.fn().mockResolvedValue(undefined),
  createEvent: vi.fn().mockResolvedValue("event-1"),
  getEvents: vi.fn().mockResolvedValue([]),
  sendInvites: vi.fn(),
}));

import EventForm from "@/components/calendar/EventForm.vue";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "@/stores/accounts";
import { useCalendarStore } from "@/stores/calendar";
import { useUiStore } from "@/stores/ui";
import type { MeetBinding } from "@/lib/types";

function binding(id: string): MeetBinding {
  return {
    lifecycle_id: `lifecycle-${id}`,
    account_id: "meet-account",
    protocol: "zoom",
    meeting_id: `meeting-${id}`,
    join_url: `https://zoom.example/${id}`,
  };
}

function mountForm() {
  return mount(EventForm, {
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
}

describe("EventForm pending meeting lifecycle", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    vi.clearAllMocks();
    vi.mocked(api.meetDiscardPending).mockResolvedValue(undefined);
    vi.mocked(api.createEvent).mockResolvedValue("event-1");
    vi.mocked(api.getEvents).mockResolvedValue([]);

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
  });

  it("discards the pending meeting when closed", async () => {
    vi.mocked(api.meetCreateUrl).mockResolvedValue(binding("one"));
    const wrapper = mountForm();
    await wrapper.get('[data-testid="event-form-meet-meet-account"]').trigger("click");
    await flushPromises();

    await wrapper.get(".btn-cancel").trigger("click");
    await flushPromises();

    expect(api.meetDiscardPending).toHaveBeenCalledWith("lifecycle-one");
    expect(wrapper.emitted("close")).toHaveLength(1);
  });

  it("offers configured La Suite Visio accounts", () => {
    useAccountsStore().accounts[1].meet_protocol = "visio";
    const wrapper = mountForm();

    expect(
      wrapper.get('[data-testid="event-form-meet-meet-account"]').text(),
    ).toContain("La Suite Visio");
  });

  it("keeps a replacement and discards the previous meeting", async () => {
    vi.mocked(api.meetCreateUrl)
      .mockResolvedValueOnce(binding("one"))
      .mockResolvedValueOnce(binding("two"));
    const wrapper = mountForm();
    await wrapper.get("textarea").setValue(
      "Agenda notes\nJoin: https://zoom.example/one",
    );
    const add = wrapper.get('[data-testid="event-form-meet-meet-account"]');
    await add.trigger("click");
    await flushPromises();
    await add.trigger("click");
    await flushPromises();

    expect(api.meetDiscardPending).toHaveBeenCalledTimes(1);
    expect(api.meetDiscardPending).toHaveBeenCalledWith("lifecycle-one");
    expect(wrapper.get('[data-testid="event-form-location"]').element)
      .toHaveProperty("value", "https://zoom.example/two");
    expect((wrapper.get("textarea").element as HTMLTextAreaElement).value).toBe(
      "Join: https://zoom.example/two\n\nAgenda notes\nJoin: https://zoom.example/one",
    );
  });

  it("discards a generated meeting when its location is removed", async () => {
    vi.mocked(api.meetCreateUrl).mockResolvedValue(binding("one"));
    const wrapper = mountForm();
    await wrapper.get("textarea").setValue(
      "Keep this user-authored text\nJoin: https://zoom.example/one",
    );
    await wrapper.get('[data-testid="event-form-meet-meet-account"]').trigger("click");
    await flushPromises();
    await wrapper.get('[data-testid="event-form-location"]').setValue("");
    await wrapper.get('[data-testid="event-form-title"]').setValue("Event");
    await wrapper.get('[data-testid="event-form-save"]').trigger("click");
    await flushPromises();

    expect(api.meetDiscardPending).toHaveBeenCalledWith("lifecycle-one");
    const saved = vi.mocked(api.createEvent).mock.calls[0]?.[0];
    expect(saved.meet_binding).toBeNull();
    expect(saved.description).toBe(
      "Keep this user-authored text\nJoin: https://zoom.example/one",
    );
  });

  it("transfers ownership on successful save without discarding", async () => {
    const created = binding("one");
    vi.mocked(api.meetCreateUrl).mockResolvedValue(created);
    const wrapper = mountForm();
    await wrapper.get('[data-testid="event-form-meet-meet-account"]').trigger("click");
    await flushPromises();
    await wrapper.get('[data-testid="event-form-title"]').setValue("Event");
    await wrapper.get('[data-testid="event-form-save"]').trigger("click");
    await flushPromises();

    expect(vi.mocked(api.createEvent).mock.calls[0]?.[0].meet_binding).toEqual(created);
    expect(api.meetDiscardPending).not.toHaveBeenCalled();
    expect(wrapper.emitted("saved")).toHaveLength(1);
    expect(wrapper.emitted("close")).toHaveLength(1);
  });

  it("retains ownership after a failed save until the form closes", async () => {
    vi.mocked(api.meetCreateUrl).mockResolvedValue(binding("one"));
    vi.mocked(api.createEvent).mockRejectedValueOnce(new Error("save failed"));
    const wrapper = mountForm();
    await wrapper.get('[data-testid="event-form-meet-meet-account"]').trigger("click");
    await flushPromises();
    await wrapper.get('[data-testid="event-form-title"]').setValue("Event");
    await wrapper.get('[data-testid="event-form-save"]').trigger("click");
    await flushPromises();
    expect(api.meetDiscardPending).not.toHaveBeenCalled();

    await wrapper.get(".btn-cancel").trigger("click");
    await flushPromises();
    expect(api.meetDiscardPending).toHaveBeenCalledWith("lifecycle-one");
  });

  it("waits for in-flight creation before closing and discards its result", async () => {
    let resolveCreation!: (value: MeetBinding) => void;
    vi.mocked(api.meetCreateUrl).mockReturnValue(
      new Promise((resolve) => {
        resolveCreation = resolve;
      }),
    );
    const wrapper = mountForm();
    void wrapper.get('[data-testid="event-form-meet-meet-account"]').trigger("click");
    await flushPromises();
    await wrapper.get(".btn-cancel").trigger("click");
    expect(wrapper.emitted("close")).toBeUndefined();

    resolveCreation(binding("late"));
    await flushPromises();
    expect(api.meetDiscardPending).toHaveBeenCalledWith("lifecycle-late");
    expect(wrapper.emitted("close")).toHaveLength(1);
  });

  it("waits for in-flight creation before snapshotting save ownership", async () => {
    let resolveCreation!: (value: MeetBinding) => void;
    vi.mocked(api.meetCreateUrl).mockReturnValue(
      new Promise((resolve) => {
        resolveCreation = resolve;
      }),
    );
    const created = binding("late-save");
    const wrapper = mountForm();
    await wrapper.get('[data-testid="event-form-title"]').setValue("Event");
    void wrapper.get('[data-testid="event-form-meet-meet-account"]').trigger("click");
    await flushPromises();

    const save = wrapper.get('[data-testid="event-form-save"]');
    expect(save.attributes("disabled")).toBeDefined();
    // Simulate a click already queued as generation changed the disabled
    // state; save() must still defensively await the in-flight operation.
    save.element.removeAttribute("disabled");
    await save.trigger("click");
    await flushPromises();
    expect(api.createEvent).not.toHaveBeenCalled();

    resolveCreation(created);
    await flushPromises();

    expect(api.createEvent).toHaveBeenCalledTimes(1);
    expect(vi.mocked(api.createEvent).mock.calls[0]?.[0].meet_binding).toEqual(created);
    expect(api.meetDiscardPending).not.toHaveBeenCalled();
  });
});
