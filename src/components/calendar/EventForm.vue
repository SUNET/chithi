<script setup lang="ts">
import { ref, computed, watch } from "vue";
import { useCalendarStore } from "@/stores/calendar";
import { useAccountsStore } from "@/stores/accounts";
import { useUiStore } from "@/stores/ui";
import { localInputToUTC, toDateInTimezone, toTimeInTimezone } from "@/lib/datetime";
import * as api from "@/lib/tauri";
import RecurrenceEditor from "./RecurrenceEditor.vue";
import AttendeeEditor from "./AttendeeEditor.vue";
import TimeInput from "@/components/common/TimeInput.vue";
import DateInput from "@/components/common/DateInput.vue";
import Select from "@/components/common/Select.vue";

const props = defineProps<{
  initialStart?: string;
}>();

const emit = defineEmits<{
  close: [];
  saved: [];
}>();

const calendarStore = useCalendarStore();
const accountsStore = useAccountsStore();
const uiStore = useUiStore();

const calendarOptions = computed(() =>
  calendarStore.calendars.map((cal) => ({
    value: cal.id,
    label: `${cal.name} (${accountsStore.accounts.find((a) => a.id === cal.account_id)?.display_name || cal.account_id})`,
  })),
);

const defaultStart = props.initialStart
  ? new Date(props.initialStart)
  : new Date();
const defaultEnd = new Date(defaultStart.getTime() + 60 * 60 * 1000);

/// Accounts that can produce a meeting URL (#148). Pulled from the
/// account summary's `meet_protocol` so we can label the dropdown
/// with "Nextcloud Talk" / "Matrix" alongside the account name.
/// Map `meet_protocol` values to the human-readable label that
/// goes on the "Add <provider>" buttons. Adding a new provider
/// = one new entry in this map; rest of the component picks it
/// up from `meetAccountOptions`.
const MEET_PROTOCOL_LABELS: Record<string, string> = {
  talk: "Nextcloud Talk",
  matrix: "Matrix",
  zoom: "Zoom",
};

const meetAccountOptions = computed(() =>
  accountsStore.accounts
    .filter((a) => a.meet_protocol in MEET_PROTOCOL_LABELS)
    .map((a) => ({
      value: a.id,
      label: `${a.display_name || a.email || a.id} (${MEET_PROTOCOL_LABELS[a.meet_protocol]})`,
    })),
);
const generatingMeetUrl = ref(false);
const meetError = ref<string | null>(null);
/** Provider handle for the meeting we just created in this form
 * session. Persisted with the event on save so later edits /
 * cancellations can act on the same remote room. Reset when the
 * user replaces the meet URL (handled in `save()`). */
const pendingMeetBinding = ref<import("@/lib/types").MeetBinding | null>(null);

async function addVideoLink(accountId: string) {
  if (!accountId || generatingMeetUrl.value) return;
  generatingMeetUrl.value = true;
  meetError.value = null;
  try {
    // Pass the event's start + duration so time-bound providers
    // (Zoom) schedule the meeting on the event's day, not today.
    // All-day events pin the start at noon in the user's display
    // timezone (converted to UTC) so Zoom's UI shows the meeting
    // on the right calendar day regardless of timezone, and use a
    // 24h duration to cover the full day. Pinning at midnight
    // instead would flip to the previous/next day in some
    // timezones; noon avoids that.
    let startIso: string;
    let durationMinutes: number;
    if (allDay.value) {
      startIso = localInputToUTC(
        startDate.value,
        "12:00",
        uiStore.displayTimezone,
      );
      durationMinutes = 24 * 60;
    } else {
      const startUTC = localInputToUTC(
        startDate.value,
        startTime.value,
        uiStore.displayTimezone,
      );
      const endUTC = localInputToUTC(
        endDate.value,
        endTime.value,
        uiStore.displayTimezone,
      );
      startIso = startUTC;
      durationMinutes = Math.max(
        1,
        Math.round((new Date(endUTC).getTime() - new Date(startUTC).getTime()) / 60000),
      );
    }
    const binding = await api.meetCreateUrl(
      accountId,
      title.value || "Meeting",
      startIso,
      durationMinutes,
    );
    pendingMeetBinding.value = binding;
    // `location` is an <input type="text">: newlines aren't
    // preserved there, so we replace the field outright rather
    // than appending. The full link history lives in
    // `description` (a textarea), where multi-line works.
    location.value = binding.join_url;
    description.value = description.value
      ? `Join: ${binding.join_url}\n\n${description.value}`
      : `Join: ${binding.join_url}`;
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    meetError.value = `Could not create meeting: ${msg}`;
  } finally {
    generatingMeetUrl.value = false;
  }
}

const title = ref("");
const startDate = ref(toDateInTimezone(defaultStart, uiStore.displayTimezone));
const startTime = ref(toTimeInTimezone(defaultStart, uiStore.displayTimezone));
const endDate = ref(toDateInTimezone(defaultEnd, uiStore.displayTimezone));
const endTime = ref(toTimeInTimezone(defaultEnd, uiStore.displayTimezone));

/** Minimum end date: cannot be before start date */
const minEndDate = computed(() => startDate.value);

/** Minimum end time: if same day, cannot be before start time */
const minEndTime = computed(() => {
  if (endDate.value === startDate.value) {
    return startTime.value;
  }
  return undefined;
});

// When start moves past end, push end forward
watch([startDate, startTime], () => {
  const sISO = localInputToUTC(startDate.value, startTime.value, uiStore.displayTimezone);
  const eISO = localInputToUTC(endDate.value, endTime.value, uiStore.displayTimezone);
  if (new Date(eISO) <= new Date(sISO)) {
    const newEnd = new Date(new Date(sISO).getTime() + 60 * 60 * 1000);
    endDate.value = toDateInTimezone(newEnd, uiStore.displayTimezone);
    endTime.value = toTimeInTimezone(newEnd, uiStore.displayTimezone);
  }
});
const allDay = ref(false);
const location = ref("");
const description = ref("");
const calendarId = ref(calendarStore.calendars[0]?.id ?? "");

// Account that owns the picked calendar — passed to AttendeeEditor
// so its autocomplete can hit that account's calendar binding's
// default contact book first (#137).
const selectedCalendarAccountId = computed(() => {
  const cal = calendarStore.calendars.find((c) => c.id === calendarId.value);
  return cal?.account_id ?? null;
});
const recurrenceRule = ref<string | null>(null);
const attendeeEmails = ref<string[]>([]);
const saving = ref(false);
const error = ref<string | null>(null);
const roomSuggestions = ref<import("@/lib/types").RoomSuggestion[]>([]);
const loadingRoomSuggestions = ref(false);
const roomAvailability = ref<import("@/lib/types").RoomAvailability | null>(null);
const checkingRoomAvailability = ref(false);
let roomAvailabilityRequestId = 0;
let roomSuggestionsRequestId = 0;

function selectedRoom() {
  const query = location.value.trim().toLowerCase();
  if (!query) {
    return null;
  }
  return roomSuggestions.value.find((room) =>
    room.name.toLowerCase() === query || room.address.toLowerCase() === query,
  ) ?? null;
}

function currentScheduleRange() {
  // All-day events still need a concrete UTC window for the Graph
  // getSchedule query, and that window is the day boundaries in the
  // user's display timezone — not UTC midnight. A Stockholm all-day
  // event on May 19 actually runs 2026-05-18T22:00Z..2026-05-19T22:00Z.
  const tz = uiStore.displayTimezone;
  return {
    start: allDay.value
      ? localInputToUTC(startDate.value, "00:00", tz)
      : localInputToUTC(startDate.value, startTime.value, tz),
    end: allDay.value
      ? localInputToUTC(endDate.value, "23:59", tz)
      : localInputToUTC(endDate.value, endTime.value, tz),
  };
}

function formatAvailabilityTime(value: string | null) {
  if (!value) {
    return "";
  }
  const date = new Date(`${value}Z`);
  if (Number.isNaN(date.getTime())) {
    return value;
  }
  return new Intl.DateTimeFormat(undefined, {
    hour: "numeric",
    minute: "2-digit",
    month: "short",
    day: "numeric",
    timeZone: uiStore.displayTimezone,
  }).format(date);
}

const roomAvailabilityMessage = computed(() => {
  if (checkingRoomAvailability.value) {
    return "Checking room availability...";
  }
  if (!roomAvailability.value) {
    return null;
  }
  if (roomAvailability.value.state === "available") {
    return "Available for the selected time";
  }
  if (roomAvailability.value.state === "busy") {
    return `Busy ${formatAvailabilityTime(roomAvailability.value.busy_start)} - ${formatAvailabilityTime(roomAvailability.value.busy_end)}`;
  }
  return "Availability unknown";
});

async function refreshRoomAvailability() {
  // Claim a request id up front so any in-flight check is invalidated
  // immediately — including when we bail out below. Otherwise a slower
  // earlier response could still match its id and repopulate a stale
  // message for a room/time that is no longer selected.
  const requestId = ++roomAvailabilityRequestId;

  const room = selectedRoom();
  const accountId = selectedCalendarAccountId.value;
  const account = accountsStore.accounts.find((entry) => entry.id === accountId);
  if (!room || !accountId || !account || (account.provider !== "o365" && account.provider !== "microsoft365")) {
    roomAvailability.value = null;
    checkingRoomAvailability.value = false;
    return;
  }

  const { start, end } = currentScheduleRange();
  if (new Date(end) <= new Date(start)) {
    roomAvailability.value = null;
    checkingRoomAvailability.value = false;
    return;
  }

  checkingRoomAvailability.value = true;
  try {
    const availability = await api.checkRoomAvailability(accountId, room.address, start, end);
    if (requestId === roomAvailabilityRequestId) {
      roomAvailability.value = availability;
    }
  } catch (e) {
    if (requestId === roomAvailabilityRequestId) {
      console.error("Failed to check room availability:", e);
      roomAvailability.value = { state: "unknown", busy_start: null, busy_end: null };
    }
  } finally {
    if (requestId === roomAvailabilityRequestId) {
      checkingRoomAvailability.value = false;
    }
  }
}

async function refreshRoomSuggestions() {
  // Claim a request id so a slower load for a previously selected
  // account can't overwrite the current account's suggestions when
  // the user switches calendars/accounts mid-flight.
  const requestId = ++roomSuggestionsRequestId;

  const accountId = selectedCalendarAccountId.value;
  const account = accountsStore.accounts.find((entry) => entry.id === accountId);
  if (!accountId || !account || (account.provider !== "o365" && account.provider !== "microsoft365")) {
    roomSuggestions.value = [];
    roomAvailability.value = null;
    return;
  }

  loadingRoomSuggestions.value = true;
  try {
    const suggestions = await api.listRoomSuggestions(accountId);
    if (requestId !== roomSuggestionsRequestId) {
      return;
    }
    roomSuggestions.value = suggestions;
  } catch (e) {
    if (requestId !== roomSuggestionsRequestId) {
      return;
    }
    console.error("Failed to load room suggestions:", e);
    roomSuggestions.value = [];
    roomAvailability.value = null;
  } finally {
    if (requestId === roomSuggestionsRequestId) {
      loadingRoomSuggestions.value = false;
    }
  }

  await refreshRoomAvailability();
}

watch(selectedCalendarAccountId, () => {
  void refreshRoomSuggestions();
}, { immediate: true });

watch([location, startDate, startTime, endDate, endTime, allDay], () => {
  void refreshRoomAvailability();
});

async function save() {
  if (!title.value.trim()) {
    error.value = "Title is required";
    return;
  }
  if (!calendarId.value) {
    error.value = "Select a calendar";
    return;
  }

  if (!allDay.value) {
    const sUTC = localInputToUTC(startDate.value, startTime.value, uiStore.displayTimezone);
    const eUTC = localInputToUTC(endDate.value, endTime.value, uiStore.displayTimezone);
    if (new Date(eUTC) <= new Date(sUTC)) {
      error.value = "End time must be after start time";
      return;
    }
  }

  saving.value = true;
  error.value = null;

  const cal = calendarStore.calendars.find((c) => c.id === calendarId.value);
  const accountId = cal?.account_id ?? accountsStore.activeAccountId ?? "";

  try {
    const startISO = allDay.value
      ? `${startDate.value}T00:00:00Z`
      : localInputToUTC(startDate.value, startTime.value, uiStore.displayTimezone);
    const endISO = allDay.value
      ? `${endDate.value}T23:59:59Z`
      : localInputToUTC(endDate.value, endTime.value, uiStore.displayTimezone);

    // If the user blanked the location after we generated a meet
    // link, treat the meeting as discarded so we don't bind a stale
    // remote room to the saved event. The orphaned remote meeting
    // is acceptable here for the same reason cancelling the form
    // outright is.
    const meetBinding =
      pendingMeetBinding.value &&
      location.value === pendingMeetBinding.value.join_url
        ? pendingMeetBinding.value
        : null;

    const eventId = await calendarStore.createEvent({
      account_id: accountId,
      calendar_id: calendarId.value,
      title: title.value,
      description: description.value || null,
      location: location.value || null,
      start_time: startISO,
      end_time: endISO,
      all_day: allDay.value,
      timezone: uiStore.displayTimezone,
      recurrence_rule: recurrenceRule.value,
      attendees: attendeeEmails.value.map((e) => ({ email: e, name: null, status: "needs-action" })),
      meet_binding: meetBinding,
    });

    // Send invite emails if attendees were added
    if (attendeeEmails.value.length > 0) {
      try {
        await api.sendInvites(accountId, eventId, attendeeEmails.value);
      } catch (e) {
        console.error("Failed to send invites:", e);
      }
    }

    emit("saved");
    emit("close");
  } catch (e) {
    error.value = String(e);
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <div class="event-form-overlay" @click.self="emit('close')">
    <div class="event-form">
      <div class="form-header">
        <h3>New Event</h3>
        <button class="close-btn" @click="emit('close')">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" /></svg>
        </button>
      </div>

      <div class="form-body">
        <div v-if="error" class="form-error">{{ error }}</div>

        <div class="form-group">
          <label>Title *</label>
          <input v-model="title" type="text" placeholder="Event title" autofocus data-testid="event-form-title" />
        </div>

        <div class="form-group">
          <label>Calendar</label>
          <Select v-model="calendarId" :options="calendarOptions" testid="event-form-calendar" />
        </div>

        <label class="checkbox-row">
          <input type="checkbox" v-model="allDay" data-testid="event-form-allday" />
          All day event
        </label>

        <div class="form-row-datetime">
          <div class="form-group">
            <label>Start</label>
            <div class="datetime-inputs">
              <DateInput v-model="startDate" class="date-input" testid="event-form-start" />
              <TimeInput v-if="!allDay" v-model="startTime" class="time-input" testid="event-form-start-time" />
            </div>
          </div>
          <div class="form-group">
            <label>End</label>
            <div class="datetime-inputs">
              <DateInput v-model="endDate" class="date-input" :min="minEndDate" testid="event-form-end" />
              <TimeInput v-if="!allDay" v-model="endTime" class="time-input" :min="minEndTime" testid="event-form-end-time" />
            </div>
          </div>
        </div>

        <div class="form-group">
          <label>Location</label>
          <input
            v-model="location"
            type="text"
            placeholder="Add location"
            :list="roomSuggestions.length > 0 ? 'event-form-room-suggestions' : undefined"
            data-testid="event-form-location"
          />
          <datalist
            v-if="roomSuggestions.length > 0"
            id="event-form-room-suggestions"
            data-testid="event-form-room-suggestions"
          >
            <option
              v-for="room in roomSuggestions"
              :key="room.address"
              :value="room.name"
            >
              {{ room.address }}
            </option>
          </datalist>
          <span
            v-if="loadingRoomSuggestions"
            class="room-suggestions-loading"
            data-testid="event-form-room-suggestions-loading"
          >
            Loading rooms...
          </span>
          <span
            v-if="roomAvailabilityMessage"
            class="room-availability"
            :class="roomAvailability ? `room-availability--${roomAvailability.state}` : undefined"
            data-testid="event-form-room-availability"
          >
            {{ roomAvailabilityMessage }}
          </span>
          <!-- #148: one-click video conference. Only renders when
               at least one account has a meet binding configured
               in Settings. Picking an entry creates the room /
               call on that provider and appends the join URL to
               this Location field plus a Join: line in the
               description below. -->
          <div
            v-if="meetAccountOptions.length > 0"
            class="meet-row"
            data-testid="event-form-meet-row"
          >
            <button
              v-for="opt in meetAccountOptions"
              :key="opt.value"
              type="button"
              class="meet-btn"
              :disabled="generatingMeetUrl"
              :data-testid="`event-form-meet-${opt.value}`"
              @click="addVideoLink(opt.value)"
            >
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <polygon points="23 7 16 12 23 17 23 7" /><rect x="1" y="5" width="15" height="14" rx="2" ry="2" />
              </svg>
              {{ generatingMeetUrl ? "Creating…" : `Add ${opt.label}` }}
            </button>
          </div>
          <span v-if="meetError" class="meet-error" data-testid="event-form-meet-error">
            {{ meetError }}
          </span>
        </div>

        <div class="form-group">
          <label>Repeat</label>
          <RecurrenceEditor v-model="recurrenceRule" />
        </div>

        <div class="form-group">
          <label>Attendees</label>
          <AttendeeEditor
            v-model="attendeeEmails"
            :account-id="selectedCalendarAccountId"
          />
        </div>

        <div class="form-group">
          <label>Description</label>
          <textarea v-model="description" rows="3" placeholder="Add description"></textarea>
        </div>
      </div>

      <div class="form-footer">
        <div></div>
        <div class="footer-actions">
          <button class="btn-cancel" @click="emit('close')">Cancel</button>
          <button class="btn-create" :disabled="saving" @click="save" data-testid="event-form-save">
            {{ saving ? "Saving..." : "Create" }}
          </button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
/* "Add video conference" buttons under the Location input (#148). */
.meet-row {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-top: 6px;
}
.meet-btn {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 4px 10px;
  font-size: 11px;
  border-radius: 12px;
  border: 1px solid var(--color-border);
  background: var(--color-bg-secondary);
  color: var(--color-text);
  cursor: pointer;
}
.meet-btn:hover {
  background: var(--color-bg-hover);
}
.meet-btn:disabled {
  opacity: 0.5;
  cursor: default;
}
.meet-error {
  display: block;
  margin-top: 6px;
  font-size: 11px;
  color: var(--color-danger, #c0392b);
}

.room-suggestions-loading {
  display: block;
  margin-top: 6px;
  font-size: 11px;
  color: var(--color-text-secondary, #666);
}

.room-availability {
  display: block;
  margin-top: 6px;
  font-size: 11px;
}

.room-availability--available {
  color: var(--color-success, #2f7a3e);
}

.room-availability--busy {
  color: var(--color-warning, #a65a00);
}

.room-availability--unknown {
  color: var(--color-text-secondary, #666);
}

.event-form-overlay {
  position: fixed;
  top: 0; left: 0; right: 0; bottom: 0;
  background: rgba(0, 0, 0, 0.6);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 100;
}

.event-form {
  background: var(--color-bg-secondary);
  border-radius: 10px;
  width: 672px;
  max-height: 85vh;
  display: flex;
  flex-direction: column;
  box-shadow: 0 20px 25px -5px rgba(0, 0, 0, 0.1), 0 8px 10px -6px rgba(0, 0, 0, 0.1);
}

.form-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 0 16px;
  height: 64px;
  border-bottom: 0.8px solid var(--color-border);
  flex-shrink: 0;
}

.form-header h3 {
  font-size: 18px;
  font-weight: 600;
}

.close-btn {
  width: 32px;
  height: 32px;
  border-radius: 4px;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--color-text-muted);
}

.close-btn:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.form-body {
  padding: 16px;
  overflow-y: auto;
  flex: 1;
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.form-error {
  padding: 8px 12px;
  background: rgba(251, 44, 54, 0.06);
  color: var(--color-danger-text);
  font-size: 12px;
  border-radius: 4px;
}

.form-group {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.form-group label {
  font-size: 14px;
  font-weight: 500;
  color: var(--color-text);
}

/* Sizing tokens consumed by DateInput / TimeInput so they visually match
   the sibling native <input>/<select> elements. CSS custom properties
   cross the scoped-styles boundary that :deep would otherwise be needed
   for. */
.form-group {
  --input-height: 36px;
  --input-padding: 0 12px;
  --input-border: 0.8px solid var(--color-border);
  --input-bg: var(--color-bg-secondary);
  --input-font-size: 16px;
}

.form-group input,
.form-group select,
.form-group textarea {
  width: 100%;
  height: var(--input-height);
  padding: var(--input-padding);
  border: var(--input-border);
  border-radius: 4px;
  background: var(--input-bg);
  font-size: var(--input-font-size);
}

.form-group textarea {
  height: 96px;
  padding: 8px 12px;
  resize: vertical;
  line-height: 24px;
}

.form-group select {
  appearance: auto;
}

.form-row-datetime {
  display: flex;
  gap: 16px;
}

.form-row-datetime .form-group {
  flex: 1;
}

.datetime-inputs {
  display: flex;
  gap: 4px;
}

.datetime-inputs .date-input {
  flex: 1;
}

.datetime-inputs .time-input {
  width: 120px;
  flex-shrink: 0;
}

.checkbox-row {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 14px;
  font-weight: 500;
  cursor: pointer;
  color: var(--color-text);
}

.form-footer {
  padding: 16px;
  border-top: 0.8px solid var(--color-border);
  display: flex;
  justify-content: space-between;
  align-items: center;
  flex-shrink: 0;
}

.footer-actions {
  display: flex;
  gap: 8px;
}

.btn-cancel {
  height: 36px;
  padding: 0 20px;
  background: var(--color-bg-tertiary);
  border-radius: 4px;
  font-size: 16px;
  font-weight: 500;
  color: var(--color-text);
}

.btn-cancel:hover {
  background: var(--color-border);
}

.btn-create {
  height: 36px;
  padding: 0 20px;
  background: var(--color-accent);
  border-radius: 4px;
  font-size: 16px;
  font-weight: 500;
  color: white;
}

.btn-create:disabled {
  opacity: 0.5;
}
</style>
