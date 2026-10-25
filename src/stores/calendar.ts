import { defineStore } from "pinia";
import { ref, computed, watch, onScopeDispose } from "vue";
import { listen } from "@tauri-apps/api/event";
import type {
  ArchivedGraphCalendar,
  Calendar,
  CalendarActionPlan,
  CalendarConfirmations,
  CalendarEdit,
  CalendarEvent,
  CalendarOccurrence,
  CalendarOccurrencePage,
  NewEventInput,
} from "@/lib/types";
import {
  expandRRule,
  isOccurrenceId,
  masterEventId,
  occurrenceId,
  parseRRule,
} from "@/lib/rrule";
import {
  calendarEditSupport,
  calendarMutationSupport,
  calendarScopedEditSupport,
} from "@/lib/calendar-mutation-support";
import * as api from "@/lib/tauri";
import { calendarDay, monthGridDays, parseCalendarDay } from "@/lib/calendar-days";
import { startOfDayUTC, toDateInTimezone } from "@/lib/datetime";
import { useAccountsStore } from "./accounts";
import { useUiStore } from "./ui";

export type CalendarViewMode = "day" | "week" | "month";

const HIDDEN_CALENDARS_KEY = "chithi-hidden-calendars";
const DEFAULT_CALENDAR_SYNC_INTERVAL_MS = 5 * 60_000;
const CALENDAR_SYNC_TICK_MS = 60_000;
const MILLISECONDS_PER_SECOND = 1_000;
const MAX_TIMEOUT_MS = 2_147_483_647;
const MAX_DATE_MS = 8_639_999_999_999_999;

function eventAnchorId(event: CalendarEvent): string {
  return isOccurrenceId(event.id) ? masterEventId(event.id) : event.id;
}

function requiresConfirmation(confirmations: CalendarConfirmations): boolean {
  return (
    confirmations.replacement_meeting_identity || confirmations.reset_exceptions
  );
}

export const useCalendarStore = defineStore("calendar", () => {
  const uiStore = useUiStore();
  const calendars = ref<Calendar[]>([]);
  const archivedGraphCalendars = ref<ArchivedGraphCalendar[]>([]);
  const archivedGraphCalendarsError = ref<string | null>(null);
  const unacknowledgedArchivedGraphCalendars = computed(() =>
    archivedGraphCalendars.value.filter((calendar) => !calendar.acknowledged));
  const acknowledgingArchivedGraphCalendars = ref(false);
  const archivedGraphAcknowledgementError = ref<string | null>(null);
  const events = ref<CalendarEvent[]>([]);
  const projectedEvents = ref<CalendarEvent[] | null>(null);
  const viewMode = ref<CalendarViewMode>("week");
  // Keep rendering the last coherent range while the requested range loads.
  const currentDate = ref(toDateInTimezone(new Date(), uiStore.displayTimezone));
  const loading = ref(false);
  const eventsError = ref<string | null>(null);
  const calendarsError = ref<string | null>(null);
  const syncErrors = ref<Record<string, string>>({});
  const displayErrors = ref<string[]>([]);
  const loadError = computed(() => calendarsError.value ?? eventsError.value);
  const unresolvedOccurrences = ref<CalendarOccurrencePage["unresolved"]>([]);
  const unverifiedSeriesIds = ref<string[]>([]);
  const repairingOccurrences = ref(false);
  const repairCooldowns = ref(new Map<string, number>());
  const repairTimers = new Map<string, ReturnType<typeof setTimeout>>();
  const repairRetryAt = computed(() => {
    if (unresolvedOccurrences.value.length === 0) return null;
    let earliest = Infinity;
    for (const row of unresolvedOccurrences.value) {
      const until = repairCooldowns.value.get(row.account_id);
      if (until === undefined || Date.now() >= until) return null;
      earliest = Math.min(earliest, until);
    }
    return earliest;
  });
  function scheduleRepairCooldown(accountId: string, until: number) {
    const previous = repairTimers.get(accountId);
    if (previous !== undefined) clearTimeout(previous);
    const remaining = until - Date.now();
    if (remaining <= 0) {
      repairCooldowns.value.delete(accountId);
      repairTimers.delete(accountId);
      return;
    }
    // Re-arm cooldowns that exceed the browser's maximum timer duration.
    repairTimers.set(
      accountId,
      setTimeout(
        () => scheduleRepairCooldown(accountId, until),
        Math.min(remaining, MAX_TIMEOUT_MS),
      ),
    );
  }
  let repairOffset = 0;
  const failedNavigation = ref<{ date: string; mode: CalendarViewMode } | null>(null);
  const pendingDisplay = ref<{ date: string; mode: CalendarViewMode } | null>(null);
  const displayDate = computed(() => pendingDisplay.value?.date ?? currentDate.value);
  const displayViewMode = computed(() => pendingDisplay.value?.mode ?? viewMode.value);
  const navigationPending = computed(() => pendingDisplay.value !== null);
  let rangeRequest = 0;
  let calendarListRequest = 0;
  const selectedEvent = ref<CalendarEvent | null>(null);
  // Null cache entries revoke mutation authorization after failed exact reads.
  const singleEventCache = ref(new Map<string, CalendarEvent | null>());
  const singleEventRequests = new Map<string, symbol>();

  watch(events, () => {
    projectedEvents.value = null;
    singleEventCache.value.clear();
    singleEventRequests.clear();
  }, { flush: "sync" });

  const accountsStore = useAccountsStore();
  const calendarSyncCooldowns = new Map<string, number>();

  function recordCalendarSyncThrottle(accountId: string, error: unknown) {
    const text = String(error);
    const match = /Graph throttled \(429\); retry after (\d+) seconds/.exec(text);
    if (!match) return;
    const seconds = Number(match[1]);
    if (!Number.isFinite(seconds) || seconds <= 0) return;
    calendarSyncCooldowns.set(
      accountId,
      Math.min(MAX_DATE_MS, Date.now() + seconds * MILLISECONDS_PER_SECOND),
    );
  }

  function calendarSyncIsCoolingDown(accountId: string): boolean {
    return Date.now() < (calendarSyncCooldowns.get(accountId) ?? 0);
  }

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
      // Visibility changes remain usable when storage is unavailable.
    }
  }

  const visibleEvents = computed(() => {
    const range = getDateRange(displayDate.value, displayViewMode.value);
    const rangeStart = new Date(range.start);
    const rangeEnd = new Date(range.end);

    const visibleCalendarIds = new Set(
      calendars.value.map((calendar) => calendar.id),
    );
    const source =
      projectedEvents.value ?? expandRawEvents(events.value, rangeStart, rangeEnd);
    return source.filter(
      (event) =>
        visibleCalendarIds.has(event.calendar_id) &&
        !hiddenCalendarIds.value.includes(event.calendar_id),
    );
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
    const anchor = parseCalendarDay(date);
    let start: Date;
    let end: Date;

    if (mode === "day") {
      start = new Date(anchor);
      start.setHours(0, 0, 0, 0);
      end = new Date(anchor);
      end.setHours(23, 59, 59, 999);
    } else if (mode === "week") {
      start = new Date(anchor);
      const offset = (anchor.getDay() - uiStore.weekStartDay + 7) % 7;
      start.setDate(anchor.getDate() - offset);
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
      // calendar-changed refreshes the UI after this per-account sync.
      if (calendarSyncIsCoolingDown(accountId)) {
        throw new Error("Graph calendar sync is waiting for its Retry-After cooldown.");
      }
      try {
        await api.syncCalendars(accountId);
        delete syncErrors.value[accountId];
      } catch (error) {
        syncErrors.value[accountId] = String(error);
        recordCalendarSyncThrottle(accountId, error);
        throw error;
      }
      return;
    }
    // Settle accounts independently, then perform one final coherent refresh.
    const targets = accountsStore.accounts.filter(
      (account) => !calendarSyncIsCoolingDown(account.id),
    );
    const results = await Promise.allSettled(
      targets.map((account) => api.syncCalendars(account.id)),
    );
    for (let index = 0; index < results.length; index += 1) {
      const result = results[index];
      const target = targets[index];
      if (result.status === "rejected") {
        syncErrors.value[target.id] = String(result.reason);
        recordCalendarSyncThrottle(target.id, result.reason);
        console.error("Calendar sync failed for", target.id, result.reason);
      } else {
        delete syncErrors.value[target.id];
      }
    }
    await fetchCalendars();
    await fetchEvents();
  }

  async function fetchCalendars() {
    const request = ++calendarListRequest;
    try {
      if (accountsStore.accounts.length === 0) {
        await accountsStore.fetchAccounts();
      }
      if (request !== calendarListRequest) return;
      if (accountsStore.accounts.length === 0) {
        calendars.value = [];
        archivedGraphCalendars.value = [];
        calendarsError.value = null;
        archivedGraphCalendarsError.value = null;
        return;
      }
      // Active and archived reads retain independent last-known-good state.
      const [active, archived] = await Promise.allSettled([
        Promise.all(
          accountsStore.accounts.map((account) => api.listCalendars(account.id)),
        ),
        Promise.all(
          accountsStore.accounts.map((account) =>
            api.listArchivedGraphCalendars(account.id),
          ),
        ),
      ]);
      if (request !== calendarListRequest) return;
      if (archived.status === "fulfilled") {
        archivedGraphCalendars.value = archived.value.flat();
        archivedGraphCalendarsError.value = null;
        if (unacknowledgedArchivedGraphCalendars.value.length === 0) {
          archivedGraphAcknowledgementError.value = null;
        }
      } else {
        archivedGraphCalendarsError.value =
          "Could not check archived Graph calendars. Please retry.";
      }
      if (active.status === "rejected") throw active.reason;
      calendars.value = active.value
        .flat()
        .filter((calendar) => calendar.is_subscribed);
      calendarsError.value = null;
    } catch (error) {
      if (request !== calendarListRequest) return;
      calendarsError.value = "Could not load calendars. Please retry.";
      throw error;
    }
  }

  async function acknowledgeArchivedGraphCalendars() {
    if (acknowledgingArchivedGraphCalendars.value) return;
    const pending = unacknowledgedArchivedGraphCalendars.value;
    if (!pending.length) return;
    acknowledgingArchivedGraphCalendars.value = true;
    archivedGraphAcknowledgementError.value = null;
    // Invalidate older reads so they cannot resurrect acknowledged warnings.
    calendarListRequest += 1;
    try {
      for (const calendar of pending) {
        await api.acknowledgeArchivedGraphCalendar(
          calendar.account_id,
          calendar.id,
        );
        archivedGraphCalendars.value = archivedGraphCalendars.value.map(
          (item) =>
            item.id === calendar.id && item.account_id === calendar.account_id
              ? { ...item, acknowledged: true }
              : item,
        );
      }
    } catch (error) {
      archivedGraphAcknowledgementError.value =
        "Could not acknowledge every archived calendar. Please retry.";
      console.error("Archived Graph calendar acknowledgement failed:", error);
    } finally {
      acknowledgingArchivedGraphCalendars.value = false;
      try {
        await fetchCalendars();
      } catch (error) {
        console.error("Archived Graph calendar refresh failed:", error);
      }
    }
  }

  async function fetchEvents({ refreshSelected = true } = {}) {
    const request = ++rangeRequest;
    loading.value = true;
    eventsError.value = null;
    try {
      const range = getDateRange();
      // Each account is a coherent read unit. A failed account contributes no
      // stale dates, but cannot discard another account's current projection.
      const accounts = [...accountsStore.accounts];
      const results = await Promise.allSettled(accounts.map(async (account) => {
        const [events, page] = await Promise.all([
          api.getEvents(account.id, range.start, range.end),
          api.listCalendarOccurrences(account.id, null, range.start, range.end, 2000),
        ]);
        const masters = new Map(events.map((event) => [event.id, event]));
        const projectedIds = new Set<string>();
        const projected = page.occurrences.map((occurrence) => {
          const master = masters.get(occurrence.event_id);
          if (!master) throw new Error("Calendar occurrence has no matching local master event.");
          const event = occurrenceEvent(occurrence, master);
          if (projectedIds.has(event.id)) throw new Error("Calendar occurrence projection contains duplicate IDs.");
          projectedIds.add(event.id);
          return event;
        });
        return { events, page, projected };
      }));
      const errors: string[] = [];
      const rawEvents: CalendarEvent[] = [];
      const projected: CalendarEvent[] = [];
      const needsHydration = new Set<string>();
      const unresolved = new Map<string, CalendarOccurrencePage["unresolved"][number]>();
      for (const [index, result] of results.entries()) {
        if (result.status === "rejected") {
          errors.push(`${accounts[index].display_name}: could not load calendar dates: ${String(result.reason)}`);
          continue;
        }
        const { events, page, projected: dates } = result.value;
        rawEvents.push(...events);
        projected.push(...dates);
        if (page.has_more) errors.push(`${accounts[index].display_name}: the visible range exceeds the display limit; some events are not shown.`);
        for (const error of page.display_errors ?? []) errors.push(`${accounts[index].display_name}: event ${error.event_id} could not be displayed: ${error.message}`);
        for (const row of page.unresolved) unresolved.set(row.event_id, row);
        for (const id of page.needs_hydration) needsHydration.add(id);
      }
      if (results.length && results.every((result) => result.status === "rejected")) {
        throw new Error(errors.join("; "));
      }
      // Provider masters cannot prove their exceptions or current existence.
      // Only verified occurrence projections may supply provider dates.
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
      displayErrors.value = errors;
      unresolvedOccurrences.value = [...unresolved.values()];
      unverifiedSeriesIds.value = [...needsHydration];
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
        displayErrors.value = [String(error)];
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

  function getDisplayedEvent(eventId: string): CalendarEvent | undefined {
    if (singleEventCache.value.has(eventId)) return getCachedEvent(eventId);
    if (isOccurrenceId(eventId)) {
      const master = events.value.find((event) => event.id === masterEventId(eventId));
      if (master?.recurrence_kind !== "series") return undefined;
    }
    return visibleEvents.value.find((event) => event.id === eventId)
      ?? getCachedEvent(eventId);
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

  const creationRequests = new Map<string, string>();
  async function createEvent(event: NewEventInput): Promise<string> {
    // A retry of the same failed request reuses the provider idempotency key.
    // This is write intent only; it never contributes a visible event.
    const request = JSON.stringify(event);
    const operationId = creationRequests.get(request) ?? crypto.randomUUID();
    creationRequests.set(request, operationId);
    const id = await api.createEvent(event, operationId);
    creationRequests.delete(request);
    await fetchEvents();
    return id;
  }

  async function updateEvent(
    eventId: string,
    patch: Partial<NewEventInput>,
  ): Promise<void> {
    requireMutableEvent(eventId);
    await api.updateEvent(eventId, patch);
    await refreshAfterMutation(eventId);
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
    if (requiresConfirmation(action.requires)) {
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
    const anchorId = eventAnchorId(event);
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
    if (requiresConfirmation(plan.requires)) {
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

  async function planEntireSeriesAction(
    event: CalendarEvent,
    edit: CalendarEdit,
    destinationCalendarId: string | null = null,
  ): Promise<CalendarActionPlan> {
    const support = calendarScopedEditSupport(event);
    if (!support.supported || !support.scopes.includes("entire-series")) {
      throw new Error(support.reason || "The selected event has no editable series.");
    }
    const anchorId = eventAnchorId(event);
    const start = event.start_time;
    const end = event.end_time;
    const view = await api.readCalendarEventSet(anchorId, start, end, 1);
    if (view.master.selection.event_id !== anchorId || !view.master.selection.token) {
      throw new Error("The recurring event identity changed. Refresh and try again.");
    }
    return api.planCalendarAction({
      selection: {
        ...view.master.selection,
        original_start: selectedOriginalStart(event),
      },
      scope: "entire-series",
      edit,
      destination_calendar_id: destinationCalendarId === event.calendar_id
        ? null : destinationCalendarId,
      reset_exceptions: false,
    });
  }

  async function resumePendingCalendarAction(
    event: CalendarEvent,
  ): Promise<string | null> {
    const anchorId = eventAnchorId(event);
    const pending = (await api.listPendingCalendarActions(event.account_id))
      .filter((action) => action.event_id === anchorId);
    const abandonedPlans = pending.filter((action) =>
      action.stage === "planned" && action.auto_resume !== true);
    for (const action of abandonedPlans) {
      await api.discardCalendarAction(action.operation_id);
    }
    const resumable = pending.filter((action) =>
      action.stage !== "planned" || action.auto_resume === true);
    if (resumable.length === 0) return null;
    if (resumable.length !== 1) {
      throw new Error(
        "Multiple unfinished calendar actions own this series. Resolve them before editing it again.",
      );
    }
    const action = resumable[0];
    const result = await api.executeCalendarAction(action.operation_id, {
      replacement_meeting_identity: action.requires.replacement_meeting_identity,
      reset_exceptions: action.requires.reset_exceptions,
    });
    if (result.stage !== "completed") {
      throw new Error("The unfinished recurring action still needs reconciliation.");
    }
    await fetchEvents({ refreshSelected: false });
    return result.event_id;
  }

  async function executeCalendarPlan(
    plan: CalendarActionPlan,
    confirmations: CalendarConfirmations,
  ): Promise<string> {
    const result = await api.executeCalendarAction(plan.operation_id, confirmations);
    if (result.stage !== "completed") {
      throw new Error("The recurring event action still needs reconciliation.");
    }
    await fetchEvents({ refreshSelected: false });
    return result.event_id;
  }

  function discardCalendarPlan(plan: CalendarActionPlan): Promise<void> {
    return api.discardCalendarAction(plan.operation_id);
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
      eventId,
      targetCalendarId,
      targetAccountId,
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
      rangeRequest += 1;
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
      if (calendarsError.value || archivedGraphCalendarsError.value) {
        await fetchCalendars();
      }
      if (failedNavigation.value) {
        goToDate(failedNavigation.value.date, failedNavigation.value.mode);
      } else {
        await fetchEvents();
      }
    } catch (error) {
      console.error("Calendar retry failed:", error);
    }
  }

  /** User-triggered, bounded provider verification; never invents identities. */
  async function repairIncompleteOccurrences() {
    if (repairingOccurrences.value || unresolvedOccurrences.value.length === 0 ||
      repairRetryAt.value !== null) return;
    repairingOccurrences.value = true;
    try {
      const rows = unresolvedOccurrences.value;
      const index = Array.from({ length: rows.length }, (_, offset) =>
        (repairOffset + offset) % rows.length).find((candidate) => {
        const until = repairCooldowns.value.get(rows[candidate].account_id);
        return until === undefined || Date.now() >= until;
      });
      if (index === undefined) return;
      const row = rows[index];
      repairOffset = (index + 1) % rows.length;
      try {
        const outcome = await api.repairCalendarOccurrence(row.event_id);
        if (outcome.retry_after_seconds !== null) {
          const retryDelayMs =
            Math.max(1, outcome.retry_after_seconds) * MILLISECONDS_PER_SECOND;
          const until = Math.min(MAX_DATE_MS, Date.now() + retryDelayMs);
          repairCooldowns.value.set(row.account_id, until);
          scheduleRepairCooldown(row.account_id, until);
          return;
        }
      } catch (error) {
        console.warn("Calendar occurrence verification failed:", row.event_id, error);
        return;
      }
      await fetchEvents({ refreshSelected: false });
    } catch (error) {
      console.error("Calendar occurrence refresh failed:", error);
    } finally {
      repairingOccurrences.value = false;
    }
  }

  function goToday() {
    goToDate(toDateInTimezone(new Date(), uiStore.displayTimezone));
  }

  function navigateBy(direction: -1 | 1): void {
    const date = parseCalendarDay(currentDate.value);
    if (viewMode.value === "day") {
      date.setDate(date.getDate() + direction);
    } else if (viewMode.value === "week") {
      date.setDate(date.getDate() + 7 * direction);
    } else {
      const originalDay = date.getDate();
      date.setDate(1);
      date.setMonth(date.getMonth() + direction);
      const lastDay = new Date(
        date.getFullYear(),
        date.getMonth() + 1,
        0,
      ).getDate();
      date.setDate(Math.min(originalDay, lastDay));
    }
    goToDate(calendarDay(date));
  }

  function goPrev(): void {
    navigateBy(-1);
  }

  function goNext(): void {
    navigateBy(1);
  }

  function toggleCalendarVisibility(calendarId: string) {
    const index = hiddenCalendarIds.value.indexOf(calendarId);
    if (index !== -1) {
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

  let calendarSyncIntervalId: ReturnType<typeof setInterval> | null = null;
  const lastCalendarSync = new Map<string, number>();

  async function tick(): Promise<void> {
    const now = Date.now();
    for (const account of accountsStore.accounts) {
      if (!account.enabled || calendarSyncIsCoolingDown(account.id)) continue;
      const intervalSeconds = account.calendar_sync_interval_seconds;
      const intervalMs =
        intervalSeconds !== null && intervalSeconds > 0
          ? intervalSeconds * MILLISECONDS_PER_SECOND
          : DEFAULT_CALENDAR_SYNC_INTERVAL_MS;
      const last = lastCalendarSync.get(account.id) ?? 0;
      if (now - last < intervalMs) continue;
      lastCalendarSync.set(account.id, now);
      try {
        await syncCalendars(account.id);
      } catch (error) {
        console.error(`Periodic calendar sync failed for ${account.id}:`, error);
      }
    }
  }

  function startCalendarSync() {
    if (calendarSyncIntervalId) return;
    calendarSyncIntervalId = setInterval(() => {
      void tick().catch((error) =>
        console.error("Calendar tick failed:", error),
      );
    }, CALENDAR_SYNC_TICK_MS);
  }

  function stopCalendarSync() {
    if (calendarSyncIntervalId) {
      clearInterval(calendarSyncIntervalId);
      calendarSyncIntervalId = null;
    }
  }

  let calendarDisposed = false;
  const calendarUnlistenFns: Array<() => void> = [];

  function registerCalendarListener<T>(
    name: string,
    handler: (payload: T) => void,
  ): void {
    void listen<T>(name, ({ payload }) => {
      if (!calendarDisposed) handler(payload);
    })
      .then((unlisten) => {
        if (calendarDisposed) {
          unlisten();
        } else {
          calendarUnlistenFns.push(unlisten);
        }
      })
      .catch((error) =>
        console.error(`Failed to subscribe to ${name}:`, error),
      );
  }

  registerCalendarListener<{ account_id: string; error: string }>(
    "calendar-sync-error",
    (payload) => {
      syncErrors.value[payload.account_id] = payload.error;
    },
  );
  registerCalendarListener<string>("calendar-sync-complete", (accountId) => {
    delete syncErrors.value[accountId];
  });
  registerCalendarListener<unknown>("calendar-changed", () => {
    void fetchCalendars()
      .then(() => fetchEvents())
      .catch((error) => console.error("Calendar refresh failed:", error));
  });

  onScopeDispose(() => {
    calendarDisposed = true;
    stopCalendarSync();
    for (const unlisten of calendarUnlistenFns) unlisten();
    for (const timer of repairTimers.values()) clearTimeout(timer);
  });

  return {
    calendars,
    archivedGraphCalendars,
    archivedGraphCalendarsError,
    unacknowledgedArchivedGraphCalendars,
    acknowledgingArchivedGraphCalendars,
    archivedGraphAcknowledgementError,
    acknowledgeArchivedGraphCalendars,
    events,
    visibleEvents,
    viewMode,
    displayViewMode,
    currentDate,
    displayDate,
    loading,
    loadError,
    syncErrors,
    displayErrors,
    unresolvedOccurrences,
    repairingOccurrences,
    unverifiedSeriesIds,
    repairRetryAt,
    navigationPending,
    selectedEvent,
    hiddenCalendarIds,
    unsubscribeCalendar,
    syncCalendars,
    fetchCalendars,
    fetchEvents,
    refreshSingleEvent,
    getCachedEvent,
    getDisplayedEvent,
    createEvent,
    updateEvent,
    updateOccurrence,
    resumePendingCalendarAction,
    planEntireSeriesAction,
    executeCalendarPlan,
    discardCalendarPlan,
    getEventMutationSupport,
    moveEventToCalendar,
    deleteEvent,
    setViewMode,
    goToDate,
    retryNavigation,
    repairIncompleteOccurrences,
    goToday,
    goPrev,
    goNext,
    toggleCalendarVisibility,
    selectEvent,
    startCalendarSync,
  };
});
