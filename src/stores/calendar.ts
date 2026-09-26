import { defineStore } from "pinia";
import { ref, computed, watch, onScopeDispose } from "vue";
import { listen } from "@tauri-apps/api/event";
import type {
  Calendar, CalendarEdit, CalendarEvent, CalendarOccurrence, NewEventInput,
} from "@/lib/types";
import {
  expandRRule, isOccurrenceId, masterEventId, occurrenceId, parseRRule,
} from "@/lib/rrule";
import {
  calendarEditSupport, calendarMutationSupport,
} from "@/lib/calendar-mutation-support";
import * as api from "@/lib/tauri";
import { calendarDay, monthGridDays, parseCalendarDay } from "@/lib/calendar-days";
import { startOfDayUTC, toDateInTimezone } from "@/lib/datetime";
import { useAccountsStore } from "./accounts";
import { useUiStore } from "./ui";

export type CalendarViewMode = "day" | "week" | "month";

export const useCalendarStore = defineStore("calendar", () => {
  const uiStore = useUiStore();
  const calendars = ref<Calendar[]>([]);
  const events = ref<CalendarEvent[]>([]);
  const projectedEvents = ref<CalendarEvent[] | null>(null);
  const viewMode = ref<CalendarViewMode>("week");
  // currentDate/viewMode are the requested target. While it loads, the grid
  // keeps rendering the previous coherent date/mode with its old projection.
  const currentDate = ref(toDateInTimezone(new Date(), uiStore.displayTimezone));
  const loading = ref(false);
  const eventsError = ref<string | null>(null);
  const calendarsError = ref<string | null>(null);
  const loadError = computed(() => calendarsError.value ?? eventsError.value);
  const failedNavigation = ref<{ date: string; mode: CalendarViewMode } | null>(null);
  const pendingDisplay = ref<{ date: string; mode: CalendarViewMode } | null>(null);
  const displayDate = computed(() => pendingDisplay.value?.date ?? currentDate.value);
  const displayViewMode = computed(() => pendingDisplay.value?.mode ?? viewMode.value);
  const navigationPending = computed(() => pendingDisplay.value !== null);
  let rangeRequest = 0;
  let calendarListRequest = 0;
  const selectedEvent = ref<CalendarEvent | null>(null);
  // Exact-ID detail/capability data is independent of the rendered date range.
  // Null revokes authorization while a refresh is pending or has failed.
  const singleEventCache = ref(new Map<string, CalendarEvent | null>());
  const singleEventRequests = new Map<string, symbol>();

  watch(events, () => {
    projectedEvents.value = null;
    singleEventCache.value.clear();
    singleEventRequests.clear();
  }, { flush: "sync" });

  const accountsStore = useAccountsStore();

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
    const range = getDateRange(displayDate.value, displayViewMode.value);
    const rangeStart = new Date(range.start);
    const rangeEnd = new Date(range.end);

    // Drop events whose calendar isn't in the (subscribed-only)
    // calendar list. Without this filter, events linger from
    // calendars the user unsubscribed from in the past — the next
    // backend sync re-pulls them and they become "ghost" events
    // visible in the grid even though the calendar is hidden in the
    // sidebar.
    const visibleCalendarIds = new Set(calendars.value.map((c) => c.id));

    if (projectedEvents.value) {
      return projectedEvents.value.filter((event) =>
        visibleCalendarIds.has(event.calendar_id) &&
        !hiddenCalendarIds.value.includes(event.calendar_id));
    }
    return expandRawEvents(events.value, rangeStart, rangeEnd)
      .filter((event) =>
        visibleCalendarIds.has(event.calendar_id) &&
        !hiddenCalendarIds.value.includes(event.calendar_id));
  });

  function expandRawEvents(
    source: CalendarEvent[],
    rangeStart: Date,
    rangeEnd: Date,
  ): CalendarEvent[] {
    const result: CalendarEvent[] = [];
    for (const event of source) {
      if (event.recurrence_rule) {
        // Unsupported JSCalendar rules stay as one raw master rather than being
        // expanded into an incorrect schedule.
        if (!parseRRule(event.recurrence_rule)) {
          result.push(event);
          continue;
        }
        for (const occurrence of expandRRule(
          event.recurrence_rule,
          new Date(event.start_time),
          new Date(event.end_time),
          rangeStart,
          rangeEnd,
        )) {
          result.push({
            ...event,
            id: occurrenceId(event.id, occurrence.start),
            recurrence_kind: "occurrence",
            start_time: occurrence.start.toISOString(),
            end_time: occurrence.end.toISOString(),
          });
        }
      } else if (
        new Date(event.start_time) <= rangeEnd &&
        new Date(event.end_time) >= rangeStart
      ) {
        result.push(event);
      }
    }
    return result;
  }

  function occurrenceEvent(
    occurrence: CalendarOccurrence,
    master: CalendarEvent,
  ): CalendarEvent {
    const originalStart = occurrence.selection.original_start;
    const originalDate = originalStart ? new Date(originalStart) : null;
    if (originalDate && !Number.isFinite(originalDate.getTime())) {
      throw new Error("Calendar occurrence has an invalid original position.");
    }
    return {
      ...master,
      ...occurrence.fields,
      id: originalDate
        ? occurrenceId(occurrence.event_id, originalDate)
        : occurrence.event_id,
      recurrence_kind: originalStart
        ? "occurrence"
        : occurrence.recurrence_kind,
      recurrence_rule: occurrence.recurrence_rule,
    };
  }

  function getDateRange(
    date = currentDate.value,
    mode = viewMode.value,
  ): { start: string; end: string } {
    const d = parseCalendarDay(date);
    let start: Date;
    let end: Date;

    if (mode === "day") {
      start = new Date(d);
      start.setHours(0, 0, 0, 0);
      end = new Date(d);
      end.setHours(23, 59, 59, 999);
    } else if (mode === "week") {
      start = new Date(d);
      const offset = (d.getDay() - uiStore.weekStartDay + 7) % 7;
      start.setDate(d.getDate() - offset);
      start.setHours(0, 0, 0, 0);
      end = new Date(start);
      end.setDate(start.getDate() + 6);
      end.setHours(23, 59, 59, 999);
    } else {
      // Both month layouts show adjacent-month cells. Cover their union.
      const desktop = monthGridDays(date, uiStore.weekStartDay);
      const mobile = monthGridDays(date, 0, true);
      const days = [...desktop, ...mobile].map(calendarDay).sort();
      const last = parseCalendarDay(days[days.length - 1]);
      last.setDate(last.getDate() + 1);
      return {
        start: new Date(startOfDayUTC(days[0], uiStore.displayTimezone)).toISOString(),
        end: new Date(startOfDayUTC(calendarDay(last), uiStore.displayTimezone) - 1)
          .toISOString(),
      };
    }

    return {
      start: start.toISOString(),
      end: end.toISOString(),
    };
  }

  async function unsubscribeCalendar(calendarId: string) {
    await api.unsubscribeCalendar(calendarId);
    await fetchCalendars();
    await fetchEvents();
  }

  async function syncCalendars(accountId?: string) {
    if (accountsStore.accounts.length === 0) {
      await accountsStore.fetchAccounts();
    }
    if (accountId) {
      // Single-account sync used by the per-binding tick (#43): one
      // account per call so each can run on its own cadence. The
      // backend emits `calendar-changed` when the sync completes, and
      // the listener below already triggers fetchCalendars() +
      // fetchEvents() — so don't run them inline or we'd refresh twice.
      await api.syncCalendars(accountId);
      return;
    }
    // Sync all accounts in parallel so a hanging account doesn't block others.
    // Each backend sync_calendars emits "calendar-changed" when done, which
    // triggers fetchCalendars + fetchEvents via the event listener.
    // The final fetchCalendars/fetchEvents below is a safety net to ensure
    // the UI is consistent after all syncs settle.
    const results = await Promise.allSettled(
      accountsStore.accounts.map((account) =>
        api.syncCalendars(account.id),
      ),
    );
    for (let i = 0; i < results.length; i++) {
      const r = results[i];
      if (r.status === "rejected") {
        console.error("Calendar sync failed for", accountsStore.accounts[i]?.id, r.reason);
      }
    }
    await fetchCalendars();
    await fetchEvents();
  }

  async function fetchCalendars() {
    const request = ++calendarListRequest;
    // Ensure accounts are loaded
    try {
      if (accountsStore.accounts.length === 0) {
        await accountsStore.fetchAccounts();
      }
      if (request !== calendarListRequest) return;
      if (accountsStore.accounts.length === 0) {
        calendars.value = [];
        calendarsError.value = null;
        return;
      }
      // Fan out across accounts in parallel; incomplete reads cannot replace
      // the subscribed list and hide all its events.
      const results = await Promise.all(accountsStore.accounts.map((account) =>
        api.listCalendars(account.id)));
      if (request !== calendarListRequest) return;
      calendars.value = results.flat().filter((c) => c.is_subscribed);
      calendarsError.value = null;
    } catch (error) {
      if (request !== calendarListRequest) return;
      calendarsError.value = "Could not load calendars. Please retry.";
      throw error;
    }
  }

  async function fetchEvents({ refreshSelected = true } = {}) {
    const request = ++rangeRequest;
    loading.value = true;
    eventsError.value = null;
    try {
      const range = getDateRange();
      // Same parallelization as fetchCalendars — purely local reads.
      const results = await Promise.all(accountsStore.accounts.map((account) =>
        api.getEvents(account.id, range.start, range.end)));
      const occurrencePages = await Promise.all(
        accountsStore.accounts.map((account) =>
          api.listCalendarOccurrences(
            account.id,
            null,
            range.start,
            range.end,
            2000,
          )),
      );
      if (occurrencePages.some((page) => page.has_more)) {
        throw new Error(
          "The visible calendar range exceeds the occurrence display limit.",
        );
      }
      const rawEvents = results.flat();
      const masters = new Map(rawEvents.map((event) => [event.id, event]));
      const projected: CalendarEvent[] = [];
      const projectedIds = new Set<string>();
      const projectedMasters = new Set<string>();
      const needsHydration = new Set<string>();
      for (const page of occurrencePages) {
        for (const id of page.needs_hydration) needsHydration.add(id);
        for (const occurrence of page.occurrences) {
          projectedMasters.add(occurrence.event_id);
          const master = masters.get(occurrence.event_id);
          if (!master) {
            throw new Error(
              "Calendar occurrence has no matching local master event.",
            );
          }
          const event = occurrenceEvent(occurrence, master);
          if (projectedIds.has(event.id)) {
            throw new Error("Calendar occurrence projection contains duplicate IDs.");
          }
          projectedIds.add(event.id);
          projected.push(event);
        }
      }
      projected.push(...expandRawEvents(
        rawEvents.filter((event) =>
          needsHydration.has(event.id) && !projectedMasters.has(event.id)),
        new Date(range.start),
        new Date(range.end),
      ));
      if (request !== rangeRequest) return;
      const activeRange = getDateRange();
      if (activeRange.start !== range.start || activeRange.end !== range.end) {
        throw new Error("Calendar display range changed during the read. Please retry.");
      }
      // An exact read may have completed while the range request was pending.
      const selectedId = selectedEvent.value?.id;
      const hadSingleEvent = selectedId && singleEventCache.value.has(selectedId);
      events.value = rawEvents;
      projectedEvents.value = projected;
      pendingDisplay.value = null;
      failedNavigation.value = null;
      if (refreshSelected && selectedId && hadSingleEvent) {
        try {
          await refreshSingleEvent(selectedId);
        } catch (error) {
          console.error("Failed to refresh selected calendar event:", error);
        }
      }
    } catch (error) {
      if (request === rangeRequest) {
        if (pendingDisplay.value) {
          failedNavigation.value = {
            date: currentDate.value,
            mode: viewMode.value,
          };
          currentDate.value = pendingDisplay.value.date;
          viewMode.value = pendingDisplay.value.mode;
          pendingDisplay.value = null;
        }
        eventsError.value = "Could not load calendar dates. Please retry.";
      }
      if (request === rangeRequest) throw error;
    } finally {
      if (request === rangeRequest) loading.value = false;
    }
  }

  function getCachedEvent(eventId: string): CalendarEvent | undefined {
    if (singleEventCache.value.has(eventId)) {
      return singleEventCache.value.get(eventId) ?? undefined;
    }
    return events.value.find((event) => event.id === eventId);
  }

  async function refreshSingleEvent(eventId: string): Promise<CalendarEvent> {
    if (isOccurrenceId(eventId)) {
      throw new Error(calendarMutationSupport({ id: eventId }).reason!);
    }
    const request = Symbol();
    singleEventRequests.set(eventId, request);
    singleEventCache.value.set(eventId, null);
    try {
      const fresh = await api.getCalendarEvent(eventId);
      if (singleEventRequests.get(eventId) !== request) {
        throw new Error("Calendar data changed while refreshing. Please try again.");
      }
      if (fresh.id !== eventId) {
        throw new Error("The refreshed calendar event has an unexpected ID.");
      }
      // Replace an existing range row, but never append out-of-range details.
      const index = events.value.findIndex((event) => event.id === eventId);
      if (index !== -1) events.value.splice(index, 1, fresh);
      singleEventCache.value.set(eventId, fresh);
      if (selectedEvent.value?.id === eventId) selectedEvent.value = fresh;
      return fresh;
    } finally {
      if (singleEventRequests.get(eventId) === request) {
        singleEventRequests.delete(eventId);
      }
    }
  }

  async function refreshAfterMutation(eventId: string) {
    await fetchEvents({ refreshSelected: false });
    const fresh = await refreshSingleEvent(eventId);
    const support = calendarMutationSupport(fresh);
    if (!support.supported) throw new Error(support.reason);
    requireMutableEvent(eventId);
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
    requireMutableEvent(eventId);
    // Save original values for rollback on failure
    const original = getCachedEvent(eventId)!;
    const snapshot = { ...original };
    const optimisticFields = ["start_time", "end_time", "calendar_id"] as const;

    // Optimistic local update first for instant UI feedback
    for (const field of optimisticFields) {
      if (patch[field]) original[field] = patch[field];
    }
    try {
      await api.updateEvent(eventId, patch);
      await refreshAfterMutation(eventId);
    } catch (e) {
      // A refresh owns its new row and recurrence metadata. Roll back only
      // our optimistic fields on the original object if they still match.
      if (getCachedEvent(eventId) === original) {
        for (const field of optimisticFields) {
          if (patch[field] && original[field] === patch[field]) {
            original[field] = snapshot[field];
          }
        }
      }
      throw e;
    }
  }

  function selectedOriginalStart(event: CalendarEvent): string | null {
    if (!isOccurrenceId(event.id)) return null;
    const master = masterEventId(event.id);
    const encoded = event.id.slice(master.length + 1);
    return event.all_day ? encoded.slice(0, 10) : encoded;
  }

  async function resumePendingOccurrenceAction(
    event: CalendarEvent,
    anchorId: string,
  ): Promise<boolean> {
    const pending = (await api.listPendingCalendarActions(event.account_id))
      .filter((action) =>
        action.event_id === anchorId &&
        (action.stage !== "planned" || action.auto_resume === true));
    if (pending.length === 0) return false;
    if (pending.length !== 1) {
      throw new Error(
        "Multiple unfinished calendar actions own this event. " +
        "Resolve them before editing it again.",
      );
    }
    const action = pending[0];
    if (action.requires.replacement_meeting_identity ||
      action.requires.reset_exceptions) {
      throw new Error(
        "The unfinished calendar action requires confirmation before recovery.",
      );
    }
    const result = await api.executeCalendarAction(action.operation_id, {
      replacement_meeting_identity: false,
      reset_exceptions: false,
    });
    if (result.stage !== "completed") {
      throw new Error(
        "The unfinished occurrence edit still needs reconciliation.",
      );
    }
    await fetchEvents({ refreshSelected: false });
    return true;
  }

  async function updateOccurrence(
    event: CalendarEvent,
    edit: CalendarEdit,
  ): Promise<void> {
    const support = calendarEditSupport(event);
    if (!support.supported || !support.occurrence) {
      throw new Error(support.reason || "The selected event is not an occurrence.");
    }
    const anchorId = isOccurrenceId(event.id) ? masterEventId(event.id) : event.id;
    if (await resumePendingOccurrenceAction(event, anchorId)) return;
    const originalStart = selectedOriginalStart(event);
    if (!originalStart) {
      throw new Error(
        "The selected occurrence has no trusted original position. Refresh and try again.",
      );
    }
    const plan = await api.planCalendarOccurrenceAction(
      anchorId,
      originalStart,
      {
        title: event.title,
        description: event.description,
        location: event.location,
        start_time: event.start_time,
        end_time: event.end_time,
        all_day: event.all_day,
        timezone: event.timezone,
      },
      edit,
    );
    if (plan.requires.replacement_meeting_identity ||
      plan.requires.reset_exceptions) {
      throw new Error("This occurrence edit requires unsupported confirmation.");
    }
    const result = await api.executeCalendarAction(plan.operation_id, {
      replacement_meeting_identity: false,
      reset_exceptions: false,
    });
    if (result.stage !== "completed") {
      throw new Error(
        "The occurrence edit needs reconciliation before it can be shown.",
      );
    }
    await fetchEvents({ refreshSelected: false });
  }

  function getEventMutationSupport(eventId: string) {
    // Preserve the requested identity even when no cached row exists.
    return calendarMutationSupport(
      getCachedEvent(eventId) ?? { id: eventId },
    );
  }

  function requireMutableEvent(eventId: string) {
    const support = getEventMutationSupport(eventId);
    if (!support.supported) throw new Error(support.reason);
  }

  async function moveEventToCalendar(
    eventId: string,
    targetCalendarId: string,
    targetAccountId: string,
  ): Promise<string> {
    requireMutableEvent(eventId);
    const newId = await api.moveEventToCalendar(
      eventId, targetCalendarId, targetAccountId,
    );
    await refreshAfterMutation(newId);
    return newId;
  }

  async function deleteEvent(eventId: string) {
    requireMutableEvent(eventId);
    await api.deleteEvent(eventId);
    singleEventRequests.delete(eventId);
    singleEventCache.value.set(eventId, null);
    if (selectedEvent.value?.id === eventId) {
      selectedEvent.value = null;
    }
    await fetchEvents();
  }

  function setViewMode(mode: CalendarViewMode) {
    goToDate(currentDate.value, mode);
  }

  function goToDate(date: string, mode = viewMode.value) {
    if (pendingDisplay.value && date === displayDate.value && mode === displayViewMode.value) {
      rangeRequest++;
      currentDate.value = date;
      viewMode.value = mode;
      pendingDisplay.value = null;
      loading.value = false;
      eventsError.value = null;
      return;
    }
    if (!pendingDisplay.value) {
      pendingDisplay.value = { date: displayDate.value, mode: displayViewMode.value };
    }
    currentDate.value = date;
    viewMode.value = mode;
    void fetchEvents().catch((error) =>
      console.error("Calendar navigation failed:", error));
  }

  async function retryNavigation() {
    try {
      if (calendarsError.value) await fetchCalendars();
      if (failedNavigation.value) {
        goToDate(failedNavigation.value.date, failedNavigation.value.mode);
      } else {
        await fetchEvents();
      }
    } catch (error) {
      console.error("Calendar retry failed:", error);
    }
  }

  function goToday() {
    goToDate(toDateInTimezone(new Date(), uiStore.displayTimezone));
  }

  function goPrev() {
    const d = parseCalendarDay(currentDate.value);
    if (viewMode.value === "day") d.setDate(d.getDate() - 1);
    else if (viewMode.value === "week") d.setDate(d.getDate() - 7);
    else {
      const day = d.getDate();
      d.setDate(1);
      d.setMonth(d.getMonth() - 1);
      d.setDate(Math.min(day, new Date(d.getFullYear(), d.getMonth() + 1, 0).getDate()));
    }
    goToDate(calendarDay(d));
  }

  function goNext() {
    const d = parseCalendarDay(currentDate.value);
    if (viewMode.value === "day") d.setDate(d.getDate() + 1);
    else if (viewMode.value === "week") d.setDate(d.getDate() + 7);
    else {
      const day = d.getDate();
      d.setDate(1);
      d.setMonth(d.getMonth() + 1);
      d.setDate(Math.min(day, new Date(d.getFullYear(), d.getMonth() + 1, 0).getDate()));
    }
    goToDate(calendarDay(d));
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

  // --- Independent calendar sync ---
  // Calendar sync is decoupled from mail sync and runs on its own timer.
  // The timer ticks every minute and evaluates each account's calendar
  // binding interval (#43): an account whose binding has
  // sync_interval_seconds=900 syncs every 15 minutes; one with `null`
  // falls back to DEFAULT_CALENDAR_INTERVAL (5 minutes).
  const DEFAULT_CALENDAR_INTERVAL_MS = 5 * 60 * 1000;
  const TICK_MS = 60 * 1000;
  let calendarSyncIntervalId: ReturnType<typeof setInterval> | null = null;
  const lastCalendarSync = new Map<string, number>();

  // We need access to the accounts store inside the timer. Importing it
  // at module scope would create a cycle (calendar -> accounts -> ...);
  // resolve it lazily on first tick instead.
  async function tick() {
    const { useAccountsStore } = await import("@/stores/accounts");
    const accounts = useAccountsStore();
    const now = Date.now();
    for (const acc of accounts.accounts) {
      if (!acc.enabled) continue;
      const intervalMs =
        (acc.calendar_sync_interval_seconds ?? 0) > 0
          ? (acc.calendar_sync_interval_seconds as number) * 1000
          : DEFAULT_CALENDAR_INTERVAL_MS;
      const last = lastCalendarSync.get(acc.id) ?? 0;
      if (now - last < intervalMs) continue;
      lastCalendarSync.set(acc.id, now);
      try {
        await syncCalendars(acc.id);
      } catch (e) {
        console.error(`Periodic calendar sync failed for ${acc.id}:`, e);
      }
    }
  }

  function startCalendarSync() {
    if (calendarSyncIntervalId) return;
    calendarSyncIntervalId = setInterval(() => {
      tick().catch((e) => console.error("Calendar tick failed:", e));
    }, TICK_MS);
  }

  function stopCalendarSync() {
    if (calendarSyncIntervalId) {
      clearInterval(calendarSyncIntervalId);
      calendarSyncIntervalId = null;
    }
  }

  // Listen for backend calendar-changed events to refresh UI
  let stopCalendarChangedListener: null | (() => void) = null;
  let calendarDisposed = false;
  void listen<string>("calendar-changed", () => {
    if (calendarDisposed) return;
    fetchCalendars().then(() => fetchEvents()).catch((error) =>
      console.error("Calendar refresh failed:", error));
  })
    .then((unlisten) => {
      if (calendarDisposed) {
        unlisten();
        return;
      }
      stopCalendarChangedListener = unlisten;
    })
    .catch((error) => {
      console.error("Failed to subscribe to calendar-changed:", error);
    });

  onScopeDispose(() => {
    calendarDisposed = true;
    stopCalendarSync();
    stopCalendarChangedListener?.();
  });

  return {
    calendars,
    events,
    visibleEvents,
    viewMode,
    displayViewMode,
    currentDate,
    displayDate,
    loading,
    loadError,
    navigationPending,
    selectedEvent,
    singleEventCache,
    hiddenCalendarIds,
    unsubscribeCalendar,
    syncCalendars,
    fetchCalendars,
    fetchEvents,
    refreshSingleEvent,
    getCachedEvent,
    createEvent,
    updateEvent,
    updateOccurrence,
    getEventMutationSupport,
    moveEventToCalendar,
    deleteEvent,
    setViewMode,
    goToDate,
    retryNavigation,
    goToday,
    goPrev,
    goNext,
    toggleCalendarVisibility,
    selectEvent,
    startCalendarSync,
    stopCalendarSync,
  };
});
