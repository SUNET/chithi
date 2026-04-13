import { defineStore } from "pinia";
import { ref, computed } from "vue";
import type { Calendar, CalendarEvent, NewEventInput } from "@/lib/types";
import { expandRRule } from "@/lib/rrule";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "./accounts";
import { useUiStore } from "./ui";

export type CalendarViewMode = "day" | "week" | "month";

export const useCalendarStore = defineStore("calendar", () => {
  const calendars = ref<Calendar[]>([]);
  const events = ref<CalendarEvent[]>([]);
  const viewMode = ref<CalendarViewMode>("week");
  const currentDate = ref(new Date().toISOString().split("T")[0]); // YYYY-MM-DD
  const loading = ref(false);
  const selectedEvent = ref<CalendarEvent | null>(null);

  const accountsStore = useAccountsStore();
  const uiStore = useUiStore();

  // Visible calendars (all by default). Persisted to localStorage so the
  // user's hide/show picks survive across sessions.
  const HIDDEN_CALENDARS_KEY = "chithi-hidden-calendars";
  const hiddenCalendarIds = ref<string[]>(loadHiddenCalendarIds());

  function loadHiddenCalendarIds(): string[] {
    try {
      const raw = localStorage.getItem(HIDDEN_CALENDARS_KEY);
      if (!raw) return [];
      const parsed = JSON.parse(raw);
      return Array.isArray(parsed)
        ? parsed.filter((v): v is string => typeof v === "string")
        : [];
    } catch {
      return [];
    }
  }

  function saveHiddenCalendarIds() {
    try {
      localStorage.setItem(
        HIDDEN_CALENDARS_KEY,
        JSON.stringify(hiddenCalendarIds.value),
      );
    } catch {
      // Swallow quota / disabled-storage errors so toggling visibility
      // never breaks the calendar UI.
    }
  }

  // Expand recurring events into individual occurrences for display
  const visibleEvents = computed(() => {
    const range = getDateRange();
    const rangeStart = new Date(range.start);
    const rangeEnd = new Date(range.end);
    const result: CalendarEvent[] = [];

    for (const e of events.value) {
      if (hiddenCalendarIds.value.includes(e.calendar_id)) continue;

      if (e.recurrence_rule) {
        // Expand RRULE into occurrences
        const occurrences = expandRRule(
          e.recurrence_rule,
          new Date(e.start_time),
          new Date(e.end_time),
          rangeStart,
          rangeEnd,
        );
        for (const occ of occurrences) {
          result.push({
            ...e,
            id: `${e.id}_${occ.start.toISOString()}`, // Unique ID per occurrence
            start_time: occ.start.toISOString(),
            end_time: occ.end.toISOString(),
          });
        }
      } else {
        result.push(e);
      }
    }

    return result;
  });

  function getDateRange(): { start: string; end: string } {
    const d = new Date(currentDate.value);
    let start: Date;
    let end: Date;

    if (viewMode.value === "day") {
      start = new Date(d);
      start.setHours(0, 0, 0, 0);
      end = new Date(d);
      end.setHours(23, 59, 59, 999);
    } else if (viewMode.value === "week") {
      start = new Date(d);
      const offset = (d.getDay() - uiStore.weekStartDay + 7) % 7;
      start.setDate(d.getDate() - offset);
      start.setHours(0, 0, 0, 0);
      end = new Date(start);
      end.setDate(start.getDate() + 6);
      end.setHours(23, 59, 59, 999);
    } else {
      // month
      start = new Date(d.getFullYear(), d.getMonth(), 1);
      end = new Date(d.getFullYear(), d.getMonth() + 1, 0, 23, 59, 59, 999);
    }

    return {
      start: start.toISOString(),
      end: end.toISOString(),
    };
  }

  async function syncCalendars() {
    if (accountsStore.accounts.length === 0) {
      await accountsStore.fetchAccounts();
    }
    for (const account of accountsStore.accounts) {
      try {
        await api.syncCalendars(account.id);
      } catch (e) {
        console.error("Calendar sync failed for", account.id, e);
      }
    }
    await fetchCalendars();
    await fetchEvents();
  }

  async function fetchCalendars() {
    // Ensure accounts are loaded
    if (accountsStore.accounts.length === 0) {
      await accountsStore.fetchAccounts();
    }
    if (accountsStore.accounts.length === 0) {
      calendars.value = [];
      return;
    }
    // Fetch calendars for all accounts
    let all: Calendar[] = [];
    for (const account of accountsStore.accounts) {
      try {
        const cals = await api.listCalendars(account.id);
        all = all.concat(cals.filter((c) => c.is_subscribed));
      } catch (e) {
        console.error("Failed to fetch calendars for", account.id, e);
      }
    }
    calendars.value = all;
  }

  async function fetchEvents() {
    loading.value = true;
    try {
      const range = getDateRange();
      let all: CalendarEvent[] = [];
      for (const account of accountsStore.accounts) {
        try {
          const evts = await api.getEvents(
            account.id,
            range.start,
            range.end,
          );
          all = all.concat(evts);
        } catch (e) {
          console.error("Failed to fetch events for", account.id, e);
        }
      }
      events.value = all;
    } finally {
      loading.value = false;
    }
  }

  async function createEvent(event: NewEventInput): Promise<string> {
    const id = await api.createEvent(event);
    await fetchEvents();
    return id;
  }

  async function updateEvent(
    eventId: string,
    patch: Partial<NewEventInput>,
  ): Promise<void> {
    // Save original values for rollback on failure
    const idx = events.value.findIndex((e) => e.id === eventId);
    const snapshot = idx !== -1 ? { ...events.value[idx] } : null;

    // Optimistic local update first for instant UI feedback
    if (idx !== -1) {
      if (patch.start_time) events.value[idx].start_time = patch.start_time;
      if (patch.end_time) events.value[idx].end_time = patch.end_time;
      if (patch.calendar_id) events.value[idx].calendar_id = patch.calendar_id;
    }
    try {
      await api.updateEvent(eventId, patch);
      await fetchEvents();
    } catch (e) {
      // Rollback optimistic update
      if (snapshot && idx !== -1 && idx < events.value.length) {
        Object.assign(events.value[idx], snapshot);
      }
      throw e;
    }
  }

  function safeParseAttendees(json: string | null): Array<{ email: string; name: string | null; status: string }> {
    if (!json) return [];
    try { return JSON.parse(json); } catch { return []; }
  }

  async function moveEventToCalendar(
    eventId: string,
    targetCalendarId: string,
    targetAccountId: string,
  ): Promise<string> {
    const ev = events.value.find((e) => e.id === eventId);
    if (!ev) return eventId;

    if (ev.account_id === targetAccountId) {
      // Same account — just update the calendar_id
      await updateEvent(eventId, { calendar_id: targetCalendarId });
      return eventId;
    } else {
      // Cross-account — create on destination, then delete source
      const attendees = safeParseAttendees(ev.attendees_json);
      const newId = await api.createEvent({
        account_id: targetAccountId,
        calendar_id: targetCalendarId,
        title: ev.title,
        description: ev.description,
        location: ev.location,
        start_time: ev.start_time,
        end_time: ev.end_time,
        all_day: ev.all_day,
        timezone: ev.timezone,
        recurrence_rule: ev.recurrence_rule,
        attendees,
      });
      await api.deleteEvent(eventId);
      await fetchEvents();
      return newId;
    }
  }

  async function unsubscribeCalendar(calendarId: string) {
    await api.unsubscribeCalendar(calendarId);
    await fetchCalendars();
    await fetchEvents();
  }

  async function deleteEvent(eventId: string) {
    await api.deleteEvent(eventId);
    if (selectedEvent.value?.id === eventId) {
      selectedEvent.value = null;
    }
    await fetchEvents();
  }

  function setViewMode(mode: CalendarViewMode) {
    viewMode.value = mode;
    fetchEvents();
  }

  function goToDate(date: string) {
    currentDate.value = date;
    fetchEvents();
  }

  function goToday() {
    goToDate(new Date().toISOString().split("T")[0]);
  }

  function goPrev() {
    const d = new Date(currentDate.value);
    if (viewMode.value === "day") d.setDate(d.getDate() - 1);
    else if (viewMode.value === "week") d.setDate(d.getDate() - 7);
    else d.setMonth(d.getMonth() - 1);
    goToDate(d.toISOString().split("T")[0]);
  }

  function goNext() {
    const d = new Date(currentDate.value);
    if (viewMode.value === "day") d.setDate(d.getDate() + 1);
    else if (viewMode.value === "week") d.setDate(d.getDate() + 7);
    else d.setMonth(d.getMonth() + 1);
    goToDate(d.toISOString().split("T")[0]);
  }

  function toggleCalendarVisibility(calendarId: string) {
    const idx = hiddenCalendarIds.value.indexOf(calendarId);
    if (idx !== -1) {
      hiddenCalendarIds.value = hiddenCalendarIds.value.filter(
        (id) => id !== calendarId,
      );
    } else {
      hiddenCalendarIds.value = [...hiddenCalendarIds.value, calendarId];
    }
    saveHiddenCalendarIds();
  }

  function selectEvent(event: CalendarEvent | null) {
    selectedEvent.value = event;
  }

  return {
    calendars,
    events,
    visibleEvents,
    viewMode,
    currentDate,
    loading,
    selectedEvent,
    hiddenCalendarIds,
    syncCalendars,
    fetchCalendars,
    fetchEvents,
    createEvent,
    updateEvent,
    moveEventToCalendar,
    deleteEvent,
    unsubscribeCalendar,
    setViewMode,
    goToDate,
    goToday,
    goPrev,
    goNext,
    toggleCalendarVisibility,
    selectEvent,
  };
});
