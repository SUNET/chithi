<script setup lang="ts">
import { ref, computed } from "vue";
import { useCalendarStore } from "@/stores/calendar";
import { useAccountsStore } from "@/stores/accounts";
import { useUiStore } from "@/stores/ui";
import { formatInTimezone, getDateInTimezone, toTimeInTimezone, localInputToUTC } from "@/lib/datetime";
import { message as tauriMessage } from "@tauri-apps/plugin-dialog";
import * as api from "@/lib/tauri";
import TimeInput from "@/components/common/TimeInput.vue";

const emit = defineEmits<{
  close: [];
}>();

const calendarStore = useCalendarStore();
const accountsStore = useAccountsStore();
const uiStore = useUiStore();
const event = calendarStore.selectedEvent!;

const editing = ref(false);
const saving = ref(false);
const error = ref<string | null>(null);

interface Attendee {
  email: string;
  name: string | null;
  status: string;
}

const attendees = computed<Attendee[]>(() => {
  if (!event.attendees_json) return [];
  try { return JSON.parse(event.attendees_json); } catch { return []; }
});

const hasAttendees = computed(() => attendees.value.length > 0);

const isOrganizer = computed(() => {
  if (!event.organizer_email) return true; // No organizer set = you created it
  const account = accountsStore.accounts.find(a => a.id === event.account_id);
  return account?.email === event.organizer_email;
});

const calendarInfo = computed(() => {
  const cal = calendarStore.calendars.find(c => c.id === event.calendar_id);
  const account = accountsStore.accounts.find(a => a.id === event.account_id);
  return {
    name: cal?.name || "Unknown calendar",
    color: cal?.color || "#4285f4",
    accountEmail: account?.email || "",
  };
});

// Edit form state — convert UTC to display timezone
const editTitle = ref(event.title);
const editStartDate = ref(getDateInTimezone(event.start_time, uiStore.displayTimezone));
const editStartTime = ref(toTimeInTimezone(new Date(event.start_time), uiStore.displayTimezone));
const editEndDate = ref(getDateInTimezone(event.end_time, uiStore.displayTimezone));
const editEndTime = ref(toTimeInTimezone(new Date(event.end_time), uiStore.displayTimezone));
const editAllDay = ref(event.all_day);
const editLocation = ref(event.location || "");
const editDescription = ref(event.description || "");

function formatDateTime(iso: string): string {
  return formatInTimezone(iso, uiStore.displayTimezone, { hour12: uiStore.hour12 });
}

function statusLabel(status: string | null): string {
  switch (status) {
    case "accepted": return "Accepted";
    case "tentative": return "Maybe";
    case "declined": return "Declined";
    default: return "No response";
  }
}

function statusClass(status: string | null): string {
  switch (status) {
    case "accepted": return "status-accepted";
    case "tentative": return "status-tentative";
    case "declined": return "status-declined";
    default: return "";
  }
}

function getAttendees(): Array<{ email: string; name: string | null; status: string }> {
  if (!event.attendees_json) return [];
  try {
    return JSON.parse(event.attendees_json);
  } catch {
    return [];
  }
}

function startEditing() {
  editing.value = true;
  error.value = null;
}

async function saveEdit() {
  saving.value = true;
  error.value = null;
  try {
    const startISO = editAllDay.value
      ? `${editStartDate.value}T00:00:00Z`
      : localInputToUTC(editStartDate.value, editStartTime.value, uiStore.displayTimezone);
    const endISO = editAllDay.value
      ? `${editEndDate.value}T23:59:59Z`
      : localInputToUTC(editEndDate.value, editEndTime.value, uiStore.displayTimezone);

    // Use the real event ID (strip occurrence suffix for recurring events)
    const realId = event.id.includes("_2") && event.recurrence_rule
      ? event.id.split("_").slice(0, -1).join("_")
      : event.id;

    await api.updateEvent(realId, {
      account_id: event.account_id,
      calendar_id: event.calendar_id,
      title: editTitle.value,
      description: editDescription.value || null,
      location: editLocation.value || null,
      start_time: startISO,
      end_time: endISO,
      all_day: editAllDay.value,
      timezone: event.timezone,
      recurrence_rule: event.recurrence_rule,
      attendees: [],
    });

    // Notify attendees if organizer and event has attendees
    if (hasAttendees.value && isOrganizer.value) {
      const result = await tauriMessage(
        "This event has attendees. Send an update notification?",
        {
          title: "Notify Attendees",
          kind: "info",
          buttons: { yes: "Send Update", no: "Don't Notify", cancel: "Cancel" },
        },
      );
      if (result === "Cancel") {
        saving.value = false;
        return;
      }
      if (result === "Send Update" || result === "Yes") {
        const accountId = event.account_id || accountsStore.activeAccountId || "";
        const emails = attendees.value.map(a => a.email);
        try {
          await api.sendInvites(accountId, realId, emails);
        } catch (e) {
          console.error("Failed to notify attendees:", e);
        }
      }
    }

    editing.value = false;
    await calendarStore.fetchEvents();
    emit("close");
  } catch (e) {
    error.value = String(e);
  } finally {
    saving.value = false;
  }
}

async function handleDelete() {
  if (hasAttendees.value && isOrganizer.value) {
    const result = await tauriMessage(
      "This event has attendees. Send a cancellation notification?",
      {
        title: "Notify Attendees",
        kind: "warning",
        buttons: { yes: "Send Cancellation", no: "Delete Only", cancel: "Cancel" },
      },
    );
    if (result === "Cancel") return;
    if (result === "Send Cancellation" || result === "Yes") {
      // TODO: Send METHOD:CANCEL iCalendar to attendees
      // For now, just log — full cancel flow requires generating CANCEL ical
      const accountId = event.account_id || accountsStore.activeAccountId || "";
      const emails = attendees.value.map(a => a.email);
      try {
        await api.sendInvites(accountId, event.id, emails);
      } catch (e) {
        console.error("Failed to send cancellation:", e);
      }
    }
  }

  await calendarStore.deleteEvent(event.id);
  emit("close");
}
</script>

<template>
  <div class="event-detail-overlay" @click.self="emit('close')">
    <div class="event-detail">
      <div class="detail-header">
        <h3 v-if="!editing">{{ event.title }}</h3>
        <input v-else v-model="editTitle" class="edit-title" type="text" data-testid="event-form-title" />
        <button class="close-btn" @click="emit('close')">&times;</button>
      </div>

      <div v-if="error" class="detail-error">{{ error }}</div>

      <!-- View mode -->
      <div v-if="!editing" class="detail-body">
        <div class="detail-row">
          <span class="detail-icon">&#x1F4C5;</span>
          <div>
            <div>{{ formatDateTime(event.start_time) }}</div>
            <div v-if="!event.all_day" class="detail-secondary">
              to {{ formatDateTime(event.end_time) }}
            </div>
            <div v-else class="detail-secondary">All day</div>
          </div>
        </div>

        <div class="detail-row">
          <span class="calendar-dot" :style="{ backgroundColor: calendarInfo.color }"></span>
          <div>
            <div>{{ calendarInfo.name }}</div>
            <div class="detail-secondary">{{ calendarInfo.accountEmail }}</div>
          </div>
        </div>

        <div v-if="event.location" class="detail-row">
          <span class="detail-icon">&#x1F4CD;</span>
          <span>{{ event.location }}</span>
        </div>

        <div v-if="event.my_status" class="detail-row">
          <span class="detail-icon">&#x2713;</span>
          <span :class="statusClass(event.my_status)">
            {{ statusLabel(event.my_status) }}
          </span>
        </div>

        <div v-if="event.organizer_email" class="detail-row">
          <span class="detail-icon">&#x1F464;</span>
          <span>Organizer: {{ event.organizer_email }}</span>
        </div>

        <div v-if="getAttendees().length > 0" class="detail-row">
          <span class="detail-icon">&#x1F465;</span>
          <div>
            <div v-for="a in getAttendees()" :key="a.email" class="attendee">
              {{ a.name || a.email }}
              <span class="attendee-status" :class="statusClass(a.status)">
                ({{ a.status }})
              </span>
            </div>
          </div>
        </div>

        <div v-if="event.description" class="detail-row">
          <span class="detail-icon">&#x1F4DD;</span>
          <pre class="description">{{ event.description }}</pre>
        </div>

        <div v-if="event.recurrence_rule" class="detail-row">
          <span class="detail-icon">&#x21BB;</span>
          <span class="detail-secondary">{{ event.recurrence_rule }}</span>
        </div>
      </div>

      <!-- Edit mode -->
      <div v-else class="detail-body edit-mode">
        <label class="checkbox-row">
          <input type="checkbox" v-model="editAllDay" data-testid="event-form-allday" />
          All day
        </label>
        <div class="edit-row">
          <div class="edit-group">
            <label>Start date</label>
            <input v-model="editStartDate" type="date" data-testid="event-form-start" />
          </div>
          <div v-if="!editAllDay" class="edit-group">
            <label>Start time</label>
            <TimeInput v-model="editStartTime" testid="event-form-start-time" />
          </div>
        </div>
        <div class="edit-row">
          <div class="edit-group">
            <label>End date</label>
            <input v-model="editEndDate" type="date" data-testid="event-form-end" />
          </div>
          <div v-if="!editAllDay" class="edit-group">
            <label>End time</label>
            <TimeInput v-model="editEndTime" testid="event-form-end-time" />
          </div>
        </div>
        <div class="edit-group">
          <label>Location</label>
          <input v-model="editLocation" type="text" placeholder="Location" data-testid="event-form-location" />
        </div>
        <div class="edit-group">
          <label>Description</label>
          <textarea v-model="editDescription" rows="3" placeholder="Description"></textarea>
        </div>
      </div>

      <div class="detail-footer">
        <template v-if="!editing">
          <button class="btn-edit" @click="startEditing">Edit</button>
          <button class="btn-danger" @click="handleDelete" data-testid="event-form-delete">Delete</button>
        </template>
        <template v-else>
          <button class="btn-save" :disabled="saving" @click="saveEdit" data-testid="event-form-save">
            {{ saving ? "Saving..." : "Save" }}
          </button>
          <button class="btn-cancel" @click="editing = false">Cancel</button>
        </template>
      </div>
    </div>
  </div>
</template>

<style scoped>
.event-detail-overlay {
  position: fixed;
  top: 0; left: 0; right: 0; bottom: 0;
  background: rgba(0, 0, 0, 0.3);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 100;
}

.event-detail {
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: 8px;
  width: 440px;
  max-height: 80vh;
  overflow-y: auto;
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.2);
}

.detail-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 16px;
  border-bottom: 1px solid var(--color-border);
}

.detail-header h3 {
  font-size: 16px;
  font-weight: 600;
}

.edit-title {
  flex: 1;
  font-size: 16px;
  font-weight: 600;
  padding: 4px 8px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  margin-right: 8px;
}

.close-btn {
  font-size: 20px;
  color: var(--color-text-muted);
  width: 28px; height: 28px;
  border-radius: 4px;
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
}

.close-btn:hover { background: var(--color-bg-hover); }

.detail-error {
  padding: 8px 16px;
  background: rgba(243, 139, 168, 0.1);
  color: var(--color-danger);
  font-size: 12px;
}

.detail-body { padding: 16px; }

.detail-row {
  display: flex;
  gap: 12px;
  margin-bottom: 12px;
  font-size: 13px;
}

.detail-icon {
  flex-shrink: 0;
  width: 20px;
  text-align: center;
}

.calendar-dot {
  flex-shrink: 0;
  width: 12px;
  height: 12px;
  border-radius: 3px;
  margin: 3px 4px 0 4px;
}

.detail-secondary {
  font-size: 12px;
  color: var(--color-text-muted);
}

.attendee { margin-bottom: 2px; }
.attendee-status { font-size: 11px; }
.status-accepted { color: var(--color-success); }
.status-tentative { color: var(--color-warning); }
.status-declined { color: var(--color-danger); text-decoration: line-through; }

.description {
  white-space: pre-wrap;
  font-family: var(--font-sans);
  font-size: 13px;
  margin: 0;
}

/* Edit mode */
.edit-mode { display: flex; flex-direction: column; gap: 10px; }

.edit-row { display: flex; gap: 12px; }
.edit-row .edit-group { flex: 1; }

.edit-group { display: flex; flex-direction: column; gap: 4px; }

.edit-group label {
  font-size: 12px;
  color: var(--color-text-secondary);
}

.edit-group input,
.edit-group textarea {
  padding: 6px 8px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  font-size: 13px;
}

.edit-group textarea { resize: vertical; }

.checkbox-row {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 13px;
  cursor: pointer;
}

.detail-footer {
  padding: 12px 16px;
  border-top: 1px solid var(--color-border);
  display: flex;
  gap: 8px;
  justify-content: flex-end;
}

.btn-edit {
  padding: 6px 16px;
  border: 1px solid var(--color-accent);
  color: var(--color-accent);
  border-radius: 6px;
  font-size: 12px;
}

.btn-edit:hover { background: rgba(137, 180, 250, 0.1); }

.btn-save {
  padding: 6px 16px;
  background: var(--color-accent);
  color: var(--color-bg);
  border-radius: 6px;
  font-weight: 600;
  font-size: 12px;
}

.btn-save:disabled { opacity: 0.5; }

.btn-cancel {
  padding: 6px 16px;
  border: 1px solid var(--color-border);
  border-radius: 6px;
  font-size: 12px;
}

.btn-danger {
  padding: 6px 16px;
  color: var(--color-danger);
  border: 1px solid var(--color-danger);
  border-radius: 6px;
  font-size: 12px;
}

.btn-danger:hover { background: rgba(243, 139, 168, 0.1); }
</style>
