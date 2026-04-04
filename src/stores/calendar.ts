import { defineStore } from "pinia";
import { ref, computed } from "vue";
import type { Calendar, CalendarEvent, NewEventInput } from "@/lib/types";
import { expandRRule } from "@/lib/rrule";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "./accounts";

export type CalendarViewMode = "day" | "week" | "month";

export const useCalendarStore = defineStore("calendar", () => {
  const calendars = ref<Calendar[]>([]);
  const events = ref<CalendarEvent[]>([]);
  const viewMode = ref<CalendarViewMode>("week");
  const currentDate = ref(new Date().toISOString().split("T")[0]); // YYYY-MM-DD
  const loading = ref(false);
  const selectedEvent = ref<CalendarEvent | null>(null);

  const accountsStore = useAccountsStore();

  // Visible calendars (all by default)
  const hiddenCalendarIds = ref<string[]>([]);

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
      start.setDate(d.getDate() - d.getDay()); // Sunday
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
    const accountId = accountsStore.activeAccountId;
    if (!accountId) {
      calendars.value = [];
      return;
    }
    // Fetch calendars for all accounts
    let all: Calendar[] = [];
    for (const account of accountsStore.accounts) {
      try {
        const cals = await api.listCalendars(account.id);
        all = all.concat(cals);
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
    deleteEvent,
    setViewMode,
    goToDate,
    goToday,
    goPrev,
    goNext,
    toggleCalendarVisibility,
    selectEvent,
  };
});
