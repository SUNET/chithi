<script setup lang="ts">
import { ref, computed, watch } from "vue";
import { useCalendarStore } from "@/stores/calendar";
import { useAccountsStore } from "@/stores/accounts";
import { useUiStore } from "@/stores/ui";
import { localInputToUTC, toDateInTimezone, toTimeInTimezone } from "@/lib/datetime";
import * as api from "@/lib/tauri";
import RecurrenceEditor from "./RecurrenceEditor.vue";
import AttendeeEditor from "./AttendeeEditor.vue";

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

const defaultStart = props.initialStart
  ? new Date(props.initialStart)
  : new Date();
const defaultEnd = new Date(defaultStart.getTime() + 60 * 60 * 1000);

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
const recurrenceRule = ref<string | null>(null);
const attendeeEmails = ref<string[]>([]);
const saving = ref(false);
const error = ref<string | null>(null);

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
    const s = new Date(`${startDate.value}T${startTime.value}:00`);
    const e = new Date(`${endDate.value}T${endTime.value}:00`);
    if (e <= s) {
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
          <select v-model="calendarId" data-testid="event-form-calendar">
            <option v-for="cal in calendarStore.calendars" :key="cal.id" :value="cal.id">
              {{ cal.name }} ({{ accountsStore.accounts.find(a => a.id === cal.account_id)?.display_name || cal.account_id }})
            </option>
          </select>
        </div>

        <label class="checkbox-row">
          <input type="checkbox" v-model="allDay" data-testid="event-form-allday" />
          All day event
        </label>

        <div class="form-row-datetime">
          <div class="form-group">
            <label>Start</label>
            <div class="datetime-inputs">
              <input v-model="startDate" type="date" class="date-input" data-testid="event-form-start" />
              <input v-if="!allDay" v-model="startTime" type="time" class="time-input" data-testid="event-form-start-time" />
            </div>
          </div>
          <div class="form-group">
            <label>End</label>
            <div class="datetime-inputs">
              <input v-model="endDate" type="date" class="date-input" :min="minEndDate" data-testid="event-form-end" />
              <input v-if="!allDay" v-model="endTime" type="time" class="time-input" :min="minEndTime" data-testid="event-form-end-time" />
            </div>
          </div>
        </div>

        <div class="form-group">
          <label>Location</label>
          <input v-model="location" type="text" placeholder="Add location" data-testid="event-form-location" />
        </div>

        <div class="form-group">
          <label>Repeat</label>
          <RecurrenceEditor v-model="recurrenceRule" />
        </div>

        <div class="form-group">
          <label>Attendees</label>
          <AttendeeEditor v-model="attendeeEmails" />
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

.form-group input,
.form-group select,
.form-group textarea {
  width: 100%;
  height: 36px;
  padding: 0 12px;
  border: 0.8px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg-secondary);
  font-size: 16px;
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
