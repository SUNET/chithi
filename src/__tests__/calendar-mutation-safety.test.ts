import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { nextTick } from "vue";

vi.mock("@/lib/tauri", () => ({
  updateEvent: vi.fn().mockResolvedValue(undefined),
  deleteEvent: vi.fn().mockResolvedValue(undefined),
  moveEventToCalendar: vi.fn().mockResolvedValue("moved-event"),
  createEvent: vi.fn().mockResolvedValue("created-event"),
  getEvents: vi.fn().mockResolvedValue([]),
  getCalendarEvent: vi.fn(),
  listCalendars: vi.fn().mockResolvedValue([]),
  listAccounts: vi.fn().mockResolvedValue([]),
  syncCalendars: vi.fn().mockResolvedValue(undefined),
  sendInvites: vi.fn().mockResolvedValue(undefined),
  notifyCalendarEvent: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({
  message: vi.fn().mockResolvedValue("Yes"),
}));

import * as api from "@/lib/tauri";
import { message } from "@tauri-apps/plugin-dialog";
import type { CalendarEvent } from "@/lib/types";
import { occurrenceId } from "@/lib/rrule";
import { formatInTimezone } from "@/lib/datetime";
import { calendarMutationSupport } from "@/lib/calendar-mutation-support";
import { useToasts } from "@/lib/toast";
import { dragCalendarEvent, isCalendarDragging } from "@/lib/calendar-drag-state";
import { useAccountsStore } from "@/stores/accounts";
import { useCalendarStore } from "@/stores/calendar";
import { usePlatformStore } from "@/stores/platform";
import { useUiStore } from "@/stores/ui";
import EventDetail from "@/components/calendar/EventDetail.vue";
import WeekView from "@/components/calendar/WeekView.vue";
import MonthView from "@/components/calendar/MonthView.vue";
import CalendarSidebar from "@/components/calendar/CalendarSidebar.vue";
import CalendarView from "@/views/CalendarView.vue";

const recurringReason = "Editing, deleting, or moving recurring events is not supported in Chithi.";
const unknownReason = "Recurrence information is unavailable. Refresh this event before editing.";

function event(patch: Partial<CalendarEvent> = {}): CalendarEvent {
  return {
    id: "9f45c42a-cd6d-450b-a803-ff6e83d7c5a1",
    account_id: "acc1", calendar_id: "cal1", uid: "event@example.test",
    title: "Appointment", description: null, location: null,
    start_time: "2026-09-08T09:00:00.000Z",
    end_time: "2026-09-08T10:00:00.000Z",
    all_day: false, timezone: "UTC", recurrence_rule: null,
    recurrence_kind: "standalone",
    organizer_email: null,
    attendees_json: '[{"email":"guest@example.test","name":null,"status":"accepted"}]',
    my_status: null, source_message_id: null,
    ...patch,
  };
}

const blocked = [
  ["provider occurrence with a plain UUID", event({ recurrence_kind: "occurrence" }), recurringReason],
  ["series without a rule", event({ recurrence_kind: "series" }), recurringReason],
  ["series master", event({ recurrence_kind: "series", recurrence_rule: "FREQ=WEEKLY" }), recurringReason],
  ["contradictory standalone with a rule", event({ recurrence_rule: "FREQ=WEEKLY" }), recurringReason],
  ["unknown", event({ recurrence_kind: "unknown" }), unknownReason],
  ["missing", event({ recurrence_kind: undefined }), unknownReason],
  ["unrecognized", event({ recurrence_kind: "future-kind" as CalendarEvent["recurrence_kind"] }), unknownReason],
  ["synthetic with standalone metadata", event({ id: occurrenceId("master", new Date("2026-09-08T09:00:00Z")) }), recurringReason],
] as const;

const wrappers: ReturnType<typeof mount>[] = [];
const stubs = {
  TimeInput: { template: "<input />" },
  DateInput: { template: "<input />" },
  LinkifiedText: { template: "<span />" },
};

function setup(selected = event()) {
  useAccountsStore().accounts = [{
    id: "acc1", display_name: "Account", email: "me@example.test",
    username: "me@example.test", provider: "generic", mail_protocol: "jmap",
    enabled: true, mail_sync_interval_seconds: null,
    calendar_sync_interval_seconds: null, contacts_sync_interval_seconds: null,
    has_calendar_binding: true, has_contacts_binding: false, meet_protocol: "",
  }];
  const store = useCalendarStore();
  store.calendars = ["cal1", "cal2"].map((id) => ({
    id, account_id: "acc1", name: id, color: "#123456",
    is_default: id === "cal1", remote_id: null, is_subscribed: true,
  }));
  store.currentDate = "2026-09-08";
  store.events = [{ ...selected }];
  store.selectEvent({ ...selected });
  useUiStore().displayTimezone = "Europe/Stockholm";
  vi.mocked(api.getEvents).mockImplementation(async () => [...store.events]);
  vi.mocked(api.getCalendarEvent).mockImplementation(async (id) => {
    const current = store.events.find((candidate) => candidate.id === id);
    if (!current) throw new Error("Event unavailable");
    return { ...current };
  });
  return store;
}

function detail() {
  const wrapper = mount(EventDetail, { global: { stubs } });
  wrappers.push(wrapper);
  return wrapper;
}

function calendarView() {
  const store = useCalendarStore();
  vi.spyOn(store, "fetchCalendars").mockResolvedValue();
  vi.spyOn(store, "syncCalendars").mockResolvedValue();
  vi.spyOn(store, "startCalendarSync").mockImplementation(() => {});
  const wrapper = mount(CalendarView, { global: { stubs: { ...stubs, EventForm: true } } });
  wrappers.push(wrapper);
  return wrapper;
}

function expectNoMutations() {
  for (const fn of [api.updateEvent, api.deleteEvent, api.createEvent,
    api.moveEventToCalendar, api.sendInvites, api.notifyCalendarEvent, message, window.confirm]) {
    expect(fn).not.toHaveBeenCalled();
  }
}

beforeEach(() => {
  setActivePinia(createPinia());
  vi.clearAllMocks();
  vi.mocked(api.updateEvent).mockReset().mockResolvedValue(undefined);
  vi.mocked(api.moveEventToCalendar).mockReset().mockResolvedValue("moved-event");
  vi.mocked(api.getCalendarEvent).mockReset();
  vi.mocked(message).mockReset().mockResolvedValue("Yes");
  useToasts().value = [];
  localStorage.clear();
  vi.stubGlobal("confirm", vi.fn().mockReturnValue(true));
});

afterEach(() => {
  wrappers.splice(0).forEach((wrapper) => wrapper.unmount());
  isCalendarDragging.value = false;
  dragCalendarEvent.value = null;
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("calendar mutation safety", () => {
  it.each(blocked)("the shared helper explains %s", (_label, selected, reason) => {
    expect(calendarMutationSupport(selected)).toEqual({ supported: false, reason });
  });

  it("only positively classified standalone metadata is supported", () => {
    expect(calendarMutationSupport(event())).toEqual({ supported: true, reason: null });
    expect(calendarMutationSupport(event({ recurrence_rule: "" })).supported).toBe(true);
    expect(calendarMutationSupport(event({ recurrence_rule: " " })).supported).toBe(false);
    expect(calendarMutationSupport(null)).toEqual({ supported: false, reason: unknownReason });
    expect(calendarMutationSupport({ id: event().id })).toEqual({ supported: false, reason: unknownReason });
  });

  it.each(blocked)("guards all store mutations for %s before side effects", async (_label, selected, reason) => {
    const store = setup(selected);
    const snapshot = JSON.stringify(store.events);
    await expect(store.updateEvent(selected.id, { start_time: "2030-01-01T00:00:00Z" })).rejects.toThrow(reason);
    await expect(store.deleteEvent(selected.id)).rejects.toThrow(reason);
    for (const account of ["acc1", "acc2"]) {
      await expect(store.moveEventToCalendar(selected.id, "cal2", account)).rejects.toThrow(reason);
    }
    expect(JSON.stringify(store.events)).toBe(snapshot);
    expect(store.selectedEvent?.id).toBe(selected.id);
    expectNoMutations();
    expect(api.getEvents).not.toHaveBeenCalled();
  });

  it("rejects missing targets instead of reporting success", async () => {
    const store = setup();
    await expect(store.updateEvent("missing", { title: "Wrong" })).rejects.toThrow();
    await expect(store.deleteEvent("missing")).rejects.toThrow();
    await expect(store.moveEventToCalendar("missing", "cal2", "acc2")).rejects.toThrow();
    expectNoMutations();
    expect(store.selectedEvent?.id).toBe(event().id);
  });

  it.each(blocked)("disables detail controls and forced handlers for %s", async (_label, selected, reason) => {
    setup(selected);
    const wrapper = detail();
    expect(wrapper.text()).toContain(reason);
    for (const selector of [".btn-edit", ".btn-danger"]) {
      const button = wrapper.get(selector);
      expect(button.attributes("disabled")).toBeDefined();
      expect(wrapper.get(`#${button.attributes("aria-describedby")}`).text()).toBe(reason);
    }
    const vm = wrapper.vm as unknown as {
      startEditing(): void; saveEdit(): Promise<void>; handleDelete(): Promise<void>;
    };
    vm.startEditing();
    await vm.saveEdit();
    await vm.handleDelete();
    expect(wrapper.find(".edit-mode").exists()).toBe(false);
    expectNoMutations();
  });

  it.each(["unknown", "occurrence", "series"] as const)("closes an open editor when refresh changes metadata to %s", async (kind) => {
    const store = setup();
    const wrapper = detail();
    await wrapper.get(".btn-edit").trigger("click");
    await wrapper.get('[data-testid="event-form-title"]').setValue("Stale draft");
    const save = (wrapper.vm as unknown as { saveEdit(): Promise<void> }).saveEdit;
    vi.mocked(api.getEvents).mockResolvedValue([event({ recurrence_kind: kind })]);
    await store.fetchEvents();
    await nextTick();
    expect(wrapper.find(".edit-mode").exists()).toBe(false);
    expect(wrapper.text()).toContain(kind === "unknown" ? unknownReason : recurringReason);
    await save();
    expectNoMutations();
  });

  it("discards the previous selection's draft and reads refreshed fields on edit", async () => {
    const store = setup();
    const wrapper = detail();
    await wrapper.get(".btn-edit").trigger("click");
    await wrapper.get('[data-testid="event-form-title"]').setValue("Wrong draft");
    const other = event({ id: "other", title: "Other appointment" });
    store.events.push(other);
    store.selectEvent(other);
    await nextTick();
    expect(wrapper.find(".edit-mode").exists()).toBe(false);
    expect(wrapper.get("h3").text()).toBe("Other appointment");
    store.events = [event({ id: "other", title: "Refreshed title" })];
    await nextTick();
    await wrapper.get(".btn-edit").trigger("click");
    expect(wrapper.get('[data-testid="event-form-title"]').element).toHaveProperty("value", "Refreshed title");
  });

  it("keeps an absent selected event visible but invalidates its open editor", async () => {
    const store = setup();
    const wrapper = detail();
    await wrapper.get(".btn-edit").trigger("click");
    vi.mocked(api.getEvents).mockResolvedValue([]);
    await store.fetchEvents();
    await nextTick();
    expect(wrapper.text()).toContain("Appointment");
    expect(wrapper.text()).toContain(unknownReason);
    await (wrapper.vm as unknown as { saveEdit(): Promise<void> }).saveEdit();
    expectNoMutations();
  });

  it("saves standalone edits through the store using its original ID and local times", async () => {
    const store = setup();
    const update = vi.spyOn(store, "updateEvent");
    const wrapper = detail();
    await wrapper.get(".btn-edit").trigger("click");
    await wrapper.get('[data-testid="event-form-title"]').setValue("Edited appointment");
    await wrapper.get('[data-testid="event-form-save"]').trigger("click");
    await flushPromises();
    expect(update).toHaveBeenCalledExactlyOnceWith(event().id, expect.objectContaining({
      title: "Edited appointment", start_time: event().start_time, end_time: event().end_time,
    }));
    expect(api.updateEvent).toHaveBeenCalledTimes(1);
    expect(api.notifyCalendarEvent).toHaveBeenCalledExactlyOnceWith("acc1", event().id, ["guest@example.test"]);
    expect(api.sendInvites).not.toHaveBeenCalled();
    expect(wrapper.emitted("close")).toBeTruthy();
  });

  it("deletes standalone from the detail panel through the store", async () => {
    const store = setup();
    const remove = vi.spyOn(store, "deleteEvent");
    const wrapper = detail();
    await wrapper.get(".btn-danger").trigger("click");
    await flushPromises();
    expect(remove).toHaveBeenCalledExactlyOnceWith(event().id);
    expect(api.deleteEvent).toHaveBeenCalledExactlyOnceWith(event().id);
    expect(api.notifyCalendarEvent).toHaveBeenCalledExactlyOnceWith("acc1", event().id, ["guest@example.test"]);
    expect(api.sendInvites).not.toHaveBeenCalled();
    expect(store.selectedEvent).toBeNull();
    expect(wrapper.emitted("close")).toBeTruthy();
  });

  it("does not send or delete after metadata becomes blocked during a notification dialog", async () => {
    const store = setup();
    let answer!: (value: string) => void;
    vi.mocked(message).mockImplementationOnce(() => new Promise((resolve) => { answer = resolve; }));
    const wrapper = detail();
    await wrapper.get(".btn-danger").trigger("click");
    expect(message).toHaveBeenCalledTimes(1);
    store.events = [event({ recurrence_kind: "occurrence" })];
    answer("Yes");
    await flushPromises();
    expect(api.sendInvites).not.toHaveBeenCalled();
    expect(api.deleteEvent).not.toHaveBeenCalled();
    expect(api.notifyCalendarEvent).not.toHaveBeenCalled();
    expect(store.selectedEvent?.id).toBe(event().id);
  });

  it("does not continue a pending save after the selection changes", async () => {
    const store = setup();
    let finish!: () => void;
    vi.mocked(api.updateEvent).mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
    const wrapper = detail();
    await wrapper.get(".btn-edit").trigger("click");
    await wrapper.get('[data-testid="event-detail-calendar"]').setValue("cal2");
    await wrapper.get('[data-testid="event-form-save"]').trigger("click");
    const other = event({ id: "other" });
    store.events.push(other);
    store.selectEvent(other);
    finish();
    await flushPromises();
    expect(api.moveEventToCalendar).not.toHaveBeenCalled();
    expect(api.sendInvites).not.toHaveBeenCalled();
    expect(api.notifyCalendarEvent).not.toHaveBeenCalled();
    expect(message).not.toHaveBeenCalled();
    expect(wrapper.emitted("close")).toBeUndefined();
    expect(store.selectedEvent?.id).toBe("other");
  });

  it("rechecks metadata after saving before moving or notifying", async () => {
    const store = setup();
    const wrapper = detail();
    await wrapper.get(".btn-edit").trigger("click");
    await wrapper.get('[data-testid="event-detail-calendar"]').setValue("cal2");
    vi.mocked(api.getEvents).mockResolvedValue([event({ recurrence_kind: "unknown" })]);
    await wrapper.get('[data-testid="event-form-save"]').trigger("click");
    await flushPromises();
    expect(api.updateEvent).toHaveBeenCalledTimes(1);
    expect(api.moveEventToCalendar).not.toHaveBeenCalled();
    expect(api.sendInvites).not.toHaveBeenCalled();
    expect(api.notifyCalendarEvent).not.toHaveBeenCalled();
    expect(message).not.toHaveBeenCalled();
    expect(store.events[0].recurrence_kind).toBe("unknown");
    expect(wrapper.text()).toContain(unknownReason);
  });

  it("a failed optimistic update cannot overwrite refreshed recurrence metadata", async () => {
    const store = setup();
    let fail!: (error: Error) => void;
    vi.mocked(api.updateEvent).mockImplementationOnce(() => new Promise((_resolve, reject) => { fail = reject; }));
    const update = store.updateEvent(event().id, { start_time: "2030-01-01T00:00:00Z" });
    const rejected = expect(update).rejects.toThrow("Persisted occurrence");
    const refreshed = event({ recurrence_kind: "occurrence", title: "Refreshed" });
    vi.mocked(api.getEvents).mockResolvedValue([refreshed]);
    await store.fetchEvents();
    fail(new Error("Persisted occurrence"));
    await rejected;
    expect(store.events).toEqual([event({ recurrence_kind: "occurrence", title: "Refreshed" })]);
    expect(store.getEventMutationSupport(event().id).supported).toBe(false);
  });

  it.each(["acc1", "acc2"])("moves standalone to %s through one backend command", async (accountId) => {
    const store = setup();
    vi.mocked(api.getCalendarEvent).mockResolvedValue(event({
      id: "moved-event", account_id: accountId, calendar_id: "cal2",
    }));
    await expect(store.moveEventToCalendar(event().id, "cal2", accountId)).resolves.toBe("moved-event");
    expect(api.moveEventToCalendar).toHaveBeenCalledExactlyOnceWith(event().id, "cal2", accountId);
    expect(api.updateEvent).not.toHaveBeenCalled();
    expect(api.createEvent).not.toHaveBeenCalled();
    expect(api.deleteEvent).not.toHaveBeenCalled();
  });
});

describe("exact-ID post-mutation refresh", () => {
  const outside = {
    start_time: "2026-10-06T09:00:00.000Z",
    end_time: "2026-10-06T10:00:00.000Z",
  };

  it.each(["cal1", "cal2", "cross-account"])("completes an out-of-week edit and %s move before notifying and closing", async (destination) => {
    const store = setup();
    let persisted = event();
    if (destination === "cross-account") store.calendars[1].account_id = "acc2";
    vi.mocked(api.updateEvent).mockImplementationOnce(async (_id, patch) => {
      persisted = { ...persisted, start_time: patch.start_time!, end_time: patch.end_time! };
    });
    vi.mocked(api.moveEventToCalendar).mockImplementationOnce(async (id, calendarId, accountId) => {
      expect(id).toBe(event().id);
      persisted = { ...persisted, id: accountId === "acc2" ? "moved-event" : id, calendar_id: calendarId, account_id: accountId };
      return persisted.id;
    });
    vi.mocked(api.getCalendarEvent).mockImplementation(async (id) => {
      expect(id).toBe(persisted.id);
      return { ...persisted };
    });
    const wrapper = detail();
    await wrapper.get(".btn-edit").trigger("click");
    const vm = wrapper.vm as unknown as { editStartDate: string; editEndDate: string };
    vm.editStartDate = "2026-10-06";
    vm.editEndDate = "2026-10-06";
    await wrapper.get('[data-testid="event-detail-calendar"]').setValue(destination === "cal1" ? "cal1" : "cal2");
    vi.mocked(api.getEvents).mockResolvedValue([]);
    await wrapper.get('[data-testid="event-form-save"]').trigger("click");
    await flushPromises();
    expect(api.updateEvent).toHaveBeenCalledExactlyOnceWith(event().id, expect.objectContaining(outside));
    expect(api.getCalendarEvent).toHaveBeenCalledWith(event().id);
    if (destination !== "cal1") {
      expect(api.moveEventToCalendar).toHaveBeenCalledExactlyOnceWith(event().id, "cal2", destination === "cross-account" ? "acc2" : "acc1");
    }
    expect(message).toHaveBeenCalledTimes(1);
    expect(api.notifyCalendarEvent).toHaveBeenCalledExactlyOnceWith(persisted.account_id, persisted.id, ["guest@example.test"]);
    expect(api.sendInvites).not.toHaveBeenCalled();
    expect(wrapper.emitted("close")).toHaveLength(1);
    expect(store.events).toEqual([]);
    expect(store.visibleEvents).toEqual([]);
  });

  it.each(["occurrence", "unknown"] as const)("blocks the next move/notification when the exact row is %s despite a standalone range cache", async (kind) => {
    const store = setup();
    const wrapper = detail();
    await wrapper.get(".btn-edit").trigger("click");
    await wrapper.get('[data-testid="event-detail-calendar"]').setValue("cal2");
    vi.mocked(api.getCalendarEvent).mockResolvedValue(event({ ...outside, recurrence_kind: kind }));
    await wrapper.get('[data-testid="event-form-save"]').trigger("click");
    await flushPromises();
    expect(api.getCalendarEvent).toHaveBeenCalledWith(event().id);
    expect(store.selectedEvent?.recurrence_kind).toBe(kind);
    expect(store.getEventMutationSupport(event().id).supported).toBe(false);
    expect(api.moveEventToCalendar).not.toHaveBeenCalled();
    expect(api.notifyCalendarEvent).not.toHaveBeenCalled();
    expect(api.sendInvites).not.toHaveBeenCalled();
    expect(message).not.toHaveBeenCalled();
    expect(wrapper.emitted("close")).toBeUndefined();
    expect(wrapper.text()).toContain(kind === "unknown" ? unknownReason : recurringReason);
  });

  it.each(["occurrence", "unknown"] as const)("blocks notifications when the exact row after a move is %s", async (kind) => {
    setup();
    const wrapper = detail();
    await wrapper.get(".btn-edit").trigger("click");
    await wrapper.get('[data-testid="event-detail-calendar"]').setValue("cal2");
    vi.mocked(api.getCalendarEvent)
      .mockResolvedValueOnce(event(outside))
      .mockResolvedValueOnce(event({ ...outside, id: "moved-event", recurrence_kind: kind }));
    vi.mocked(api.getEvents).mockResolvedValue([]);
    await wrapper.get('[data-testid="event-form-save"]').trigger("click");
    await flushPromises();
    expect(api.moveEventToCalendar).toHaveBeenCalledTimes(1);
    expect(api.getCalendarEvent).toHaveBeenLastCalledWith("moved-event");
    expect(api.notifyCalendarEvent).not.toHaveBeenCalled();
    expect(message).not.toHaveBeenCalled();
    expect(wrapper.emitted("close")).toBeUndefined();
  });

  it.each(["reschedule", "calendar move"])("notifies an out-of-range standalone %s through CalendarView's guarded command", async (operation) => {
    const store = setup();
    usePlatformStore().width = 1280;
    const wrapper = calendarView();
    await flushPromises();
    vi.mocked(api.getEvents).mockResolvedValue([]);
    vi.mocked(api.getCalendarEvent).mockImplementation(async (id) => event({ ...outside, id }));
    const payload = {
      eventId: event().id, newStart: outside.start_time, newEnd: outside.end_time,
      targetCalendarId: "cal2", targetAccountId: "acc1",
      attendeesJson: event().attendees_json, organizerEmail: null,
    };
    const vm = wrapper.vm as unknown as {
      onEventReschedule(payload: object): Promise<void>;
      onCalendarDrop(payload: object): Promise<void>;
    };
    if (operation === "reschedule") await vm.onEventReschedule(payload);
    else await vm.onCalendarDrop(payload);
    expect(window.confirm).toHaveBeenCalledTimes(1);
    expect(api.notifyCalendarEvent).toHaveBeenCalledExactlyOnceWith(
      "acc1", operation === "reschedule" ? event().id : "moved-event", ["guest@example.test"],
    );
    expect(api.sendInvites).not.toHaveBeenCalled();
    expect(store.visibleEvents).toEqual([]);
  });

  it.each(["occurrence", "unknown"] as const)("CalendarView does not prompt or notify after an exact %s refresh", async (kind) => {
    setup();
    usePlatformStore().width = 1280;
    const wrapper = calendarView();
    await flushPromises();
    vi.mocked(api.getEvents).mockResolvedValue([]);
    vi.mocked(api.getCalendarEvent).mockResolvedValue(event({ ...outside, recurrence_kind: kind }));
    await (wrapper.vm as unknown as { onEventReschedule(payload: object): Promise<void> }).onEventReschedule({
      eventId: event().id, newStart: outside.start_time, newEnd: outside.end_time,
      attendeesJson: event().attendees_json, organizerEmail: null,
    });
    expect(window.confirm).not.toHaveBeenCalled();
    expect(api.notifyCalendarEvent).not.toHaveBeenCalled();
    expect(api.sendInvites).not.toHaveBeenCalled();
  });

  it("rejects a mismatched exact ID rather than authorizing the requested event", async () => {
    const store = setup();
    vi.mocked(api.getCalendarEvent).mockResolvedValue(event({ id: "other" }));
    await expect(store.refreshSingleEvent(event().id)).rejects.toThrow("unexpected ID");
    expect(store.getEventMutationSupport(event().id).supported).toBe(false);
    expect(store.selectedEvent?.id).toBe(event().id);
  });

  it("a superseded exact response cannot overwrite newer blocked metadata", async () => {
    const store = setup();
    let finish!: (row: CalendarEvent) => void;
    vi.mocked(api.getCalendarEvent)
      .mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }))
      .mockResolvedValueOnce(event({ recurrence_kind: "occurrence" }));
    const oldRefresh = store.refreshSingleEvent(event().id);
    const rejected = expect(oldRefresh).rejects.toThrow("changed while refreshing");
    await store.refreshSingleEvent(event().id);
    finish(event());
    await rejected;
    expect(store.getEventMutationSupport(event().id).supported).toBe(false);
    expect(store.selectedEvent?.recurrence_kind).toBe("occurrence");
  });

  it("never resolves a synthetic exact ID to its master", async () => {
    const store = setup();
    const id = occurrenceId(event().id, new Date(event().start_time));
    await expect(store.refreshSingleEvent(id)).rejects.toThrow(recurringReason);
    expect(api.getCalendarEvent).not.toHaveBeenCalled();
  });

  it("an older range response revalidates exact metadata loaded while it was pending", async () => {
    const store = setup();
    let finishRange!: (rows: CalendarEvent[]) => void;
    vi.mocked(api.getEvents).mockImplementationOnce(() => new Promise((resolve) => { finishRange = resolve; }));
    const rangeRefresh = store.fetchEvents();
    vi.mocked(api.getCalendarEvent).mockResolvedValue(event({ recurrence_kind: "occurrence" }));
    await store.refreshSingleEvent(event().id);
    finishRange([event()]);
    await rangeRefresh;
    expect(store.getEventMutationSupport(event().id).supported).toBe(false);
    expect(store.selectedEvent?.recurrence_kind).toBe("occurrence");
  });

  it("does not retain exact-ID authorization after a subsequent range refresh", async () => {
    const store = setup();
    vi.mocked(api.getCalendarEvent).mockResolvedValueOnce(event(outside));
    await store.refreshSingleEvent(event().id);
    expect(store.selectedEvent?.start_time).toBe(outside.start_time);
    expect(store.getEventMutationSupport(event().id).supported).toBe(true);
    vi.mocked(api.getEvents).mockResolvedValue([]);
    vi.mocked(api.getCalendarEvent).mockResolvedValue(event({ ...outside, recurrence_kind: "occurrence" }));
    await store.fetchEvents();
    expect(store.getEventMutationSupport(event().id).supported).toBe(false);
    expect(store.visibleEvents).toEqual([]);
  });

  it("does not reselect a captured event when its exact fetch finishes after selection changes", async () => {
    const store = setup();
    let finish!: (row: CalendarEvent) => void;
    vi.mocked(api.getCalendarEvent).mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
    const refresh = store.refreshSingleEvent(event().id);
    const other = event({ id: "other" });
    store.selectEvent(other);
    finish(event(outside));
    await refresh;
    expect(store.selectedEvent?.id).toBe("other");
  });

  it("propagates exact-fetch failure and revokes stale cached authorization", async () => {
    const store = setup();
    const wrapper = detail();
    await wrapper.get(".btn-edit").trigger("click");
    vi.mocked(api.getCalendarEvent).mockRejectedValueOnce(new Error("Exact fetch failed"));
    await wrapper.get('[data-testid="event-form-save"]').trigger("click");
    await flushPromises();
    expect(wrapper.text()).toContain("Exact fetch failed");
    expect(store.getEventMutationSupport(event().id).supported).toBe(false);
    expect(api.notifyCalendarEvent).not.toHaveBeenCalled();
    expect(wrapper.emitted("close")).toBeUndefined();
  });
});

describe("notification target revalidation after modal resolution", () => {
  const outside = {
    start_time: "2026-10-06T09:00:00.000Z",
    end_time: "2026-10-06T10:00:00.000Z",
  };
  const destination = () => event({ ...outside, id: "moved-event", account_id: "acc2", calendar_id: "cal2" });

  async function pendingDetailNotification(eraseBeforePrompt = false) {
    const store = setup();
    store.calendars[1].account_id = "acc2";
    vi.mocked(api.getCalendarEvent).mockImplementation(async (id) => {
      if (id === event().id) return event(outside);
      if (id === "moved-event") return destination();
      throw new Error("Unexpected event ID");
    });
    if (eraseBeforePrompt) {
      const move = store.moveEventToCalendar;
      vi.spyOn(store, "moveEventToCalendar").mockImplementation(async (...args) => {
        const id = await move(...args);
        await store.fetchEvents();
        return id;
      });
    }
    let answer!: (result: string) => void;
    vi.mocked(message).mockImplementationOnce(() => new Promise((resolve) => { answer = resolve; }));
    const wrapper = detail();
    await wrapper.get(".btn-edit").trigger("click");
    const vm = wrapper.vm as unknown as { editStartDate: string; editEndDate: string };
    vm.editStartDate = "2026-10-06";
    vm.editEndDate = "2026-10-06";
    await wrapper.get('[data-testid="event-detail-calendar"]').setValue("cal2");
    vi.mocked(api.getEvents).mockResolvedValue([]);
    await wrapper.get('[data-testid="event-form-save"]').trigger("click");
    await flushPromises();
    expect(api.updateEvent).toHaveBeenCalledWith(event().id, expect.objectContaining(outside));
    expect(api.moveEventToCalendar).toHaveBeenCalledExactlyOnceWith(event().id, "cal2", "acc2");
    expect(message).toHaveBeenCalledTimes(1);
    // The selected source identity survives a cross-account move. A normal
    // background range refresh therefore cannot retain the destination row.
    expect(store.selectedEvent?.id).toBe(event().id);
    await store.fetchEvents();
    expect(store.getEventMutationSupport("moved-event").supported).toBe(false);
    expect(api.notifyCalendarEvent).not.toHaveBeenCalled();
    return { store, wrapper, answer };
  }

  it.each([false, true])("notifies the captured destination and closes after a range refresh during the modal (also erase before prompt: %s)", async (eraseBeforePrompt) => {
    const { store, wrapper, answer } = await pendingDetailNotification(eraseBeforePrompt);
    const reads = vi.mocked(api.getCalendarEvent).mock.calls.length;
    answer("Send Update");
    await flushPromises();
    expect(api.getCalendarEvent).toHaveBeenCalledTimes(reads + 1);
    expect(api.getCalendarEvent).toHaveBeenLastCalledWith("moved-event");
    expect(api.notifyCalendarEvent).toHaveBeenCalledExactlyOnceWith("acc2", "moved-event", ["guest@example.test"]);
    expect(api.sendInvites).not.toHaveBeenCalled();
    expect(wrapper.emitted("close")).toHaveLength(1);
    expect(store.visibleEvents).toEqual([]);
  });

  it.each(["occurrence", "unknown", "read failure"])("shows an error and does not notify when the destination becomes %s during the modal", async (state) => {
    const { wrapper, answer } = await pendingDetailNotification();
    if (state === "read failure") {
      vi.mocked(api.getCalendarEvent).mockRejectedValueOnce(new Error("Destination fetch failed"));
    } else {
      vi.mocked(api.getCalendarEvent).mockResolvedValueOnce({ ...destination(), recurrence_kind: state as "occurrence" | "unknown" });
    }
    answer("Send Update");
    await flushPromises();
    expect(api.getCalendarEvent).toHaveBeenLastCalledWith("moved-event");
    expect(api.notifyCalendarEvent).not.toHaveBeenCalled();
    expect(wrapper.emitted("close")).toBeUndefined();
    const error = wrapper.get(".detail-error").text();
    expect(error).toContain(state === "occurrence" ? recurringReason : state === "unknown" ? unknownReason : "Destination fetch failed");
    if (state === "read failure") expect(error).toMatch(/try again/i);
  });

  it("does not fetch or notify for an abandoned selection when the dialog resolves", async () => {
    const { store, wrapper, answer } = await pendingDetailNotification();
    const reads = vi.mocked(api.getCalendarEvent).mock.calls.length;
    store.selectEvent(event({ id: "other" }));
    answer("Send Update");
    await flushPromises();
    expect(api.getCalendarEvent).toHaveBeenCalledTimes(reads);
    expect(api.notifyCalendarEvent).not.toHaveBeenCalled();
    expect(wrapper.emitted("close")).toBeUndefined();
  });

  it("rechecks selection after the post-dialog exact fetch", async () => {
    const { store, wrapper, answer } = await pendingDetailNotification();
    let finish!: (row: CalendarEvent) => void;
    vi.mocked(api.getCalendarEvent).mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
    answer("Send Update");
    await flushPromises();
    expect(api.getCalendarEvent).toHaveBeenLastCalledWith("moved-event");
    store.selectEvent(event({ id: "other" }));
    finish(destination());
    await flushPromises();
    expect(api.notifyCalendarEvent).not.toHaveBeenCalled();
    expect(wrapper.emitted("close")).toBeUndefined();
    expect(store.selectedEvent?.id).toBe("other");
  });

  it.each(["standalone", "occurrence", "unknown", "read failure", "selection changed"])("CalendarView revalidates its captured target after confirm: %s", async (state) => {
    const store = setup();
    usePlatformStore().width = 1280;
    const wrapper = calendarView();
    await flushPromises();
    let answer!: (result: boolean) => void;
    vi.stubGlobal("confirm", vi.fn(() => new Promise((resolve) => { answer = resolve; })));
    vi.mocked(api.getEvents).mockResolvedValue([]);
    vi.mocked(api.getCalendarEvent).mockResolvedValue(destination());
    const operation = (wrapper.vm as unknown as { onCalendarDrop(payload: object): Promise<void> }).onCalendarDrop({
      eventId: event().id, targetCalendarId: "cal2", targetAccountId: "acc2",
      attendeesJson: event().attendees_json, organizerEmail: null,
    });
    await flushPromises();
    expect(window.confirm).toHaveBeenCalledTimes(1);
    expect(api.notifyCalendarEvent).not.toHaveBeenCalled();
    await store.fetchEvents();
    expect(store.getEventMutationSupport("moved-event").supported).toBe(false);
    if (state === "read failure") {
      vi.mocked(api.getCalendarEvent).mockRejectedValueOnce(new Error("Destination fetch failed"));
    } else if (state === "selection changed") {
      store.selectEvent(event({ id: "other" }));
    } else {
      vi.mocked(api.getCalendarEvent).mockResolvedValueOnce({ ...destination(), recurrence_kind: state as "standalone" | "occurrence" | "unknown" });
    }
    answer(true);
    await operation;
    if (state === "standalone") {
      expect(api.notifyCalendarEvent).toHaveBeenCalledExactlyOnceWith("acc2", "moved-event", ["guest@example.test"]);
    } else {
      expect(api.notifyCalendarEvent).not.toHaveBeenCalled();
      if (state !== "selection changed") {
        const error = useToasts().value.find((toast) => toast.type === "error");
        expect(error?.message).toContain(state === "occurrence" ? recurringReason : state === "unknown" ? unknownReason : "Destination fetch failed");
        if (state === "read failure") expect(error?.message).toMatch(/try again/i);
      }
    }
    expect(api.sendInvites).not.toHaveBeenCalled();
  });
});

describe("real responsive calendar selection", () => {
  it.each([500, 1280])("keeps first/later occurrence IDs and local times at width %s", async (width) => {
    const master = event({ recurrence_kind: "series", recurrence_rule: "FREQ=DAILY;COUNT=3" });
    const store = setup(master);
    store.selectEvent(null);
    usePlatformStore().width = width;
    const wrapper = calendarView();
    await flushPromises();
    for (const occurrence of [store.visibleEvents[0], store.visibleEvents[2]]) {
      const selector = width < 600 ? ".week-event" : `[data-testid="cal-event-${occurrence.id}"]`;
      if (width < 600) {
        const buttons = wrapper.findAll(selector);
        await buttons[occurrence === store.visibleEvents[0] ? 0 : 2].trigger("click");
      } else {
        await wrapper.get(selector).trigger("click");
      }
      expect(store.selectedEvent?.id).toBe(occurrence.id);
      const panel = wrapper.getComponent(EventDetail);
      expect(panel.text()).toContain(formatInTimezone(occurrence.start_time, "Europe/Stockholm", { hour12: useUiStore().hour12 }));
      expect(panel.text()).toContain(recurringReason);
      expect(panel.get(".btn-edit").attributes("disabled")).toBeDefined();
      await panel.get(".close-btn").trigger("click");
    }
    expectNoMutations();
  });

  it.each([500, 1280])("shares detail state and fail-closed behavior across responsive branches from width %s", async (width) => {
    const store = setup();
    usePlatformStore().width = width;
    const wrapper = calendarView();
    await flushPromises();
    const originalPanel = wrapper.getComponent(EventDetail);
    await originalPanel.get(".btn-edit").trigger("click");
    await originalPanel.get('[data-testid="event-form-title"]').setValue("Responsive draft");
    usePlatformStore().width = width === 500 ? 1280 : 500;
    await nextTick();
    expect(wrapper.getComponent(EventDetail).vm.$.uid).toBe(originalPanel.vm.$.uid);
    expect(originalPanel.get('[data-testid="event-form-title"]').element).toHaveProperty("value", "Responsive draft");
    store.events = [event({ recurrence_kind: "occurrence" })];
    await nextTick();
    expect(originalPanel.find(".edit-mode").exists()).toBe(false);
    expect(originalPanel.text()).toContain(recurringReason);
    expect(originalPanel.get(".btn-danger").attributes("disabled")).toBeDefined();
    expectNoMutations();
  });

  it.each(blocked)("rejects forced parent reschedule/calendar-drop handlers for %s", async (_label, selected) => {
    const store = setup(selected);
    usePlatformStore().width = 1280;
    const wrapper = calendarView();
    await flushPromises();
    const snapshot = JSON.stringify(store.events);
    const payload = {
      eventId: selected.id, newStart: "2030-01-01T00:00:00Z", newEnd: "2030-01-01T01:00:00Z",
      targetAccountId: "acc2", targetCalendarId: "cal2",
      attendeesJson: selected.attendees_json, organizerEmail: selected.organizer_email,
    };
    const vm = wrapper.vm as unknown as {
      onEventReschedule(payload: object): Promise<void>;
      onCalendarDrop(payload: object): Promise<void>;
    };
    await vm.onEventReschedule(payload);
    await vm.onCalendarDrop(payload);
    expectNoMutations();
    expect(JSON.stringify(store.events)).toBe(snapshot);
    expect(store.selectedEvent?.id).toBe(selected.id);
  });
});

describe("calendar drag safety", () => {
  it("uses exact-fetch failure to block drag/drop even when the rendered row is still standalone", async () => {
    const store = setup();
    const week = mount(WeekView);
    const month = mount(MonthView);
    const sidebar = mount(CalendarSidebar);
    wrappers.push(week, month, sidebar);
    vi.mocked(api.getCalendarEvent).mockRejectedValueOnce(new Error("Refresh failed"));
    await expect(store.refreshSingleEvent(event().id)).rejects.toThrow("Refresh failed");
    for (const view of [week, month]) {
      await view.get('[data-testid^="cal-event-"]').trigger("mousedown", { button: 0, clientX: 10, clientY: 10 });
      document.dispatchEvent(new MouseEvent("mousemove", { clientX: 50, clientY: 50 }));
      expect(isCalendarDragging.value).toBe(false);
      document.dispatchEvent(new MouseEvent("mouseup"));
    }
    dragCalendarEvent.value = event();
    isCalendarDragging.value = true;
    await week.get(".day-column").trigger("mouseup", { clientY: 100 });
    await month.get(".month-cell").trigger("mouseup");
    await sidebar.get('[data-testid="calendar-item-cal2"]').trigger("mouseup");
    expect(week.emitted("eventReschedule")).toBeUndefined();
    expect(month.emitted("eventReschedule")).toBeUndefined();
    expect(sidebar.emitted("calendarDrop")).toBeUndefined();
    expectNoMutations();
  });

  it.each(blocked)("cannot start or force grid/sidebar drops for %s", async (_label, selected) => {
    const store = setup(selected);
    const week = mount(WeekView);
    const month = mount(MonthView);
    const sidebar = mount(CalendarSidebar);
    wrappers.push(week, month, sidebar);
    for (const view of [week, month]) {
      const block = view.get('[data-testid^="cal-event-"]');
      await block.trigger("touchstart");
      await block.trigger("touchmove");
      await block.trigger("touchend");
      await block.trigger("mousedown", { button: 0, clientX: 10, clientY: 10 });
      document.dispatchEvent(new MouseEvent("mousemove", { clientX: 50, clientY: 50 }));
      expect(isCalendarDragging.value).toBe(false);
      document.dispatchEvent(new MouseEvent("mouseup"));
    }
    dragCalendarEvent.value = selected;
    isCalendarDragging.value = true;
    await week.get(".day-column").trigger("mouseup", { clientY: 100 });
    await month.get(".month-cell").trigger("mouseup");
    const vm = sidebar.vm as unknown as { onCalendarItemDrop(cal: typeof store.calendars[number]): void };
    vm.onCalendarItemDrop(store.calendars[1]);
    expect(week.emitted("eventReschedule")).toBeUndefined();
    expect(month.emitted("eventReschedule")).toBeUndefined();
    expect(sidebar.emitted("calendarDrop")).toBeUndefined();
    expectNoMutations();
  });

  it("rechecks current metadata on grid and sidebar drop after a drag starts", async () => {
    const store = setup();
    const week = mount(WeekView);
    const month = mount(MonthView);
    const sidebar = mount(CalendarSidebar);
    wrappers.push(week, month, sidebar);
    await week.get('[data-testid^="cal-event-"]').trigger("mousedown", { button: 0, clientX: 10, clientY: 10 });
    document.dispatchEvent(new MouseEvent("mousemove", { clientX: 50, clientY: 50 }));
    expect(isCalendarDragging.value).toBe(true);
    await sidebar.get('[data-testid="calendar-item-cal2"]').trigger("mouseenter");
    expect(sidebar.get('[data-testid="calendar-item-cal2"]').classes()).toContain("drag-over");
    store.events = [event({ recurrence_kind: "unknown" })];
    await nextTick();
    expect(sidebar.get('[data-testid="calendar-item-cal2"]').classes()).not.toContain("drag-over");
    await week.get(".day-column").trigger("mouseup", { clientY: 100 });
    await month.get(".month-cell").trigger("mouseup");
    await sidebar.get('[data-testid="calendar-item-cal2"]').trigger("mouseup");
    document.dispatchEvent(new MouseEvent("mouseup"));
    expect(week.emitted("eventReschedule")).toBeUndefined();
    expect(month.emitted("eventReschedule")).toBeUndefined();
    expect(sidebar.emitted("calendarDrop")).toBeUndefined();
    expectNoMutations();
  });

  it("preserves standalone IDs for both grid and sidebar drop", async () => {
    setup();
    const week = mount(WeekView);
    const month = mount(MonthView);
    const sidebar = mount(CalendarSidebar);
    wrappers.push(week, month, sidebar);
    dragCalendarEvent.value = event();
    isCalendarDragging.value = true;
    await week.get(".day-column").trigger("mouseup", { clientY: 100 });
    await month.get(".month-cell").trigger("mouseup");
    await sidebar.get('[data-testid="calendar-item-cal2"]').trigger("mouseup");
    expect(week.emitted("eventReschedule")?.[0][0]).toMatchObject({ eventId: event().id });
    expect(month.emitted("eventReschedule")?.[0][0]).toMatchObject({ eventId: event().id });
    expect(sidebar.emitted("calendarDrop")?.[0][0]).toMatchObject({ eventId: event().id });
  });
});
