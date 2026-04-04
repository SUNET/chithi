<script setup lang="ts">
import { ref } from "vue";
import { useCalendarStore } from "@/stores/calendar";
import { useAccountsStore } from "@/stores/accounts";
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

const defaultStart = props.initialStart
  ? new Date(props.initialStart)
  : new Date();
const defaultEnd = new Date(defaultStart.getTime() + 60 * 60 * 1000);

const title = ref("");
const startDate = ref(defaultStart.toISOString().slice(0, 10));
const startTime = ref(defaultStart.toISOString().slice(11, 16));
const endDate = ref(defaultEnd.toISOString().slice(0, 10));
const endTime = ref(defaultEnd.toISOString().slice(11, 16));
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

  saving.value = true;
  error.value = null;

  const cal = calendarStore.calendars.find((c) => c.id === calendarId.value);
  const accountId = cal?.account_id ?? accountsStore.activeAccountId ?? "";

  try {
    const startISO = allDay.value
      ? `${startDate.value}T00:00:00`
      : `${startDate.value}T${startTime.value}:00`;
    const endISO = allDay.value
      ? `${endDate.value}T23:59:59`
      : `${endDate.value}T${endTime.value}:00`;

    const eventId = await calendarStore.createEvent({
      account_id: accountId,
      calendar_id: calendarId.value,
      title: title.value,
      description: description.value || null,
      location: location.value || null,
      start_time: new Date(startISO).toISOString(),
      end_time: new Date(endISO).toISOString(),
      all_day: allDay.value,
      timezone: null,
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
        <button class="close-btn" @click="emit('close')">&times;</button>
      </div>

      <div v-if="error" class="form-error">{{ error }}</div>

      <div class="form-body">
        <div class="form-group">
          <label>Title</label>
          <input v-model="title" type="text" placeholder="Event title" autofocus />
        </div>

        <div class="form-group">
          <label>Calendar</label>
          <select v-model="calendarId">
            <option v-for="cal in calendarStore.calendars" :key="cal.id" :value="cal.id">
              {{ cal.name }}
            </option>
          </select>
        </div>

        <label class="checkbox-row">
          <input type="checkbox" v-model="allDay" />
          All day
        </label>

        <div class="form-row">
          <div class="form-group">
            <label>Start date</label>
            <input v-model="startDate" type="date" />
          </div>
          <div v-if="!allDay" class="form-group">
            <label>Start time</label>
            <input v-model="startTime" type="time" />
          </div>
        </div>
        <div class="form-row">
          <div class="form-group">
            <label>End date</label>
            <input v-model="endDate" type="date" />
          </div>
          <div v-if="!allDay" class="form-group">
            <label>End time</label>
            <input v-model="endTime" type="time" />
          </div>
        </div>

        <div class="form-group">
          <label>Location</label>
          <input v-model="location" type="text" placeholder="Location" />
        </div>

        <div class="form-group">
          <label>Description</label>
          <textarea v-model="description" rows="3" placeholder="Description"></textarea>
        </div>

        <RecurrenceEditor v-model="recurrenceRule" />

        <div class="form-group">
          <label>Attendees</label>
          <AttendeeEditor v-model="attendeeEmails" />
          <p v-if="attendeeEmails.length > 0" class="hint">
            Invite emails will be sent when you create the event.
          </p>
        </div>
      </div>

      <div class="form-footer">
        <button class="btn-primary" :disabled="saving" @click="save">
          {{ saving ? "Saving..." : "Create" }}
        </button>
        <button class="btn-secondary" @click="emit('close')">Cancel</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.event-form-overlay {
  position: fixed;
  top: 0; left: 0; right: 0; bottom: 0;
  background: rgba(0, 0, 0, 0.3);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 100;
}

.event-form {
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: 8px;
  width: 440px;
  max-height: 85vh;
  overflow-y: auto;
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.2);
}

.form-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 16px;
  border-bottom: 1px solid var(--color-border);
}

.close-btn {
  font-size: 20px;
  color: var(--color-text-muted);
  width: 28px; height: 28px;
  border-radius: 4px;
  display: flex;
  align-items: center;
  justify-content: center;
}

.close-btn:hover { background: var(--color-bg-hover); }

.form-error {
  padding: 8px 16px;
  background: rgba(243, 139, 168, 0.1);
  color: var(--color-danger);
  font-size: 12px;
}

.form-body { padding: 16px; }

.form-group { margin-bottom: 12px; }

.form-group label {
  display: block;
  margin-bottom: 4px;
  font-size: 12px;
  color: var(--color-text-secondary);
}

.form-group input,
.form-group select,
.form-group textarea {
  width: 100%;
  padding: 6px 8px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  font-size: 13px;
}

.form-group textarea { resize: vertical; }

.form-row { display: flex; gap: 12px; }
.form-row .form-group { flex: 1; }

.checkbox-row {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 13px;
  margin-bottom: 12px;
  cursor: pointer;
}

.form-footer {
  padding: 12px 16px;
  border-top: 1px solid var(--color-border);
  display: flex;
  gap: 8px;
}

.btn-primary {
  padding: 6px 16px;
  background: var(--color-accent);
  color: var(--color-bg);
  border-radius: 6px;
  font-weight: 600;
}

.btn-primary:disabled { opacity: 0.5; }

.btn-secondary {
  padding: 6px 16px;
  border: 1px solid var(--color-border);
  border-radius: 6px;
}

.hint {
  font-size: 11px;
  color: var(--color-text-muted);
  margin-top: 4px;
}
</style>
