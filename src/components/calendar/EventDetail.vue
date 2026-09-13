<script setup lang="ts">
import { ref, computed, watch, useId } from "vue";
import { useCalendarStore } from "@/stores/calendar";
import { useAccountsStore } from "@/stores/accounts";
import { useUiStore } from "@/stores/ui";
import { formatInTimezone, getDateInTimezone, toTimeInTimezone, localInputToUTC } from "@/lib/datetime";
import { calendarMutationSupport } from "@/lib/calendar-mutation-support";
import { message as tauriMessage } from "@tauri-apps/plugin-dialog";
import * as api from "@/lib/tauri";
import type { Calendar, CalendarEvent } from "@/lib/types";
import TimeInput from "@/components/common/TimeInput.vue";
import DateInput from "@/components/common/DateInput.vue";
import LinkifiedText from "@/components/common/LinkifiedText.vue";

const emit = defineEmits<{
  close: [];
}>();

const calendarStore = useCalendarStore();
const accountsStore = useAccountsStore();
const uiStore = useUiStore();
const event = computed(() => {
  const selected = calendarStore.selectedEvent;
  if (!selected) return null;
  // Resolve the original identity on every refresh, retaining occurrence dates.
  return calendarStore.getCachedEvent(selected.id)
    ?? calendarStore.visibleEvents.find((candidate) => candidate.id === selected.id)
    ?? selected;
});
const mutationSupport = computed(() =>
  calendarStore.getEventMutationSupport(event.value?.id ?? ""),
);
const mutationReasonId = useId();

const editing = ref(false);
const saving = ref(false);
const error = ref<string | null>(null);
const editingEventId = ref<string | null>(null);
let selectionVersion = 0;

watch(() => calendarStore.selectedEvent?.id, () => {
  selectionVersion++;
}, { flush: "sync" });

watch([() => event.value?.id, () => mutationSupport.value.reason], () => {
  editing.value = false;
  editingEventId.value = null;
  error.value = null;
}, { flush: "sync" });

interface Attendee {
  email: string;
  name: string | null;
  status: string;
}

const attendees = computed<Attendee[]>(() => {
  if (!event.value?.attendees_json) return [];
  try { return JSON.parse(event.value.attendees_json); } catch { return []; }
});

function canNotify(current: CalendarEvent): boolean {
  const account = accountsStore.accounts.find(a => a.id === current.account_id);
  if (!current.organizer_email || account?.email.toLowerCase() !== current.organizer_email.toLowerCase()) return false;
  try { return JSON.parse(current.attendees_json || "[]").length > 0; } catch { return false; }
}

const calendarInfo = computed(() => {
  const cal = calendarStore.calendars.find(c => c.id === event.value?.calendar_id);
  const account = accountsStore.accounts.find(a => a.id === event.value?.account_id);
  return {
    name: cal?.name || "Unknown calendar",
    color: cal?.color || "#4285f4",
    accountEmail: account?.email || "",
  };
});

// Edit form state — convert UTC to display timezone
const editTitle = ref("");
const editStartDate = ref("");
const editStartTime = ref("");
const editEndDate = ref("");
const editEndTime = ref("");
const editAllDay = ref(false);
const editLocation = ref("");
const editDescription = ref("");
const editCalendarId = ref("");

function calendarLabel(cal: Calendar): string {
  // Same label format as EventForm's calendar picker.
  const account = accountsStore.accounts.find((a) => a.id === cal.account_id);
  return `${cal.name} (${account?.display_name || cal.account_id})`;
}

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

function startEditing() {
  if (saving.value || !event.value || !mutationSupport.value.supported) return;
  const current = event.value;
  editTitle.value = current.title;
  editStartDate.value = getDateInTimezone(current.start_time, uiStore.displayTimezone);
  editStartTime.value = toTimeInTimezone(new Date(current.start_time), uiStore.displayTimezone);
  editEndDate.value = getDateInTimezone(current.end_time, uiStore.displayTimezone);
  editEndTime.value = toTimeInTimezone(new Date(current.end_time), uiStore.displayTimezone);
  editAllDay.value = current.all_day;
  editLocation.value = current.location || "";
  editDescription.value = current.description || "";
  editCalendarId.value = current.calendar_id;
  editingEventId.value = current.id;
  editing.value = true;
  error.value = null;
}

function isCurrentSelection(id: string, version: number): boolean {
  return calendarStore.selectedEvent?.id === id && selectionVersion === version;
}

async function refreshMutationTarget(eventId: string, purpose: "attendee notification" | "deletion") {
  try {
    const fresh = await calendarStore.refreshSingleEvent(eventId);
    const support = calendarMutationSupport(fresh);
    if (!support.supported) throw new Error(support.reason);
    return fresh;
  } catch (cause) {
    throw new Error(`Could not verify the event for ${purpose}. Please try again. ${String(cause)}`);
  }
}

async function saveEdit() {
  if (saving.value || !editing.value || !event.value ||
    editingEventId.value !== event.value.id || !mutationSupport.value.supported) return;
  const original = event.value;
  const version = selectionVersion;
  const targetCalendarId = editCalendarId.value;
  saving.value = true;
  error.value = null;
  try {
    const startISO = editAllDay.value
      ? `${editStartDate.value}T00:00:00Z`
      : localInputToUTC(editStartDate.value, editStartTime.value, uiStore.displayTimezone);
    const endISO = editAllDay.value
      ? `${editEndDate.value}T23:59:59Z`
      : localInputToUTC(editEndDate.value, editEndTime.value, uiStore.displayTimezone);

    await calendarStore.updateEvent(original.id, {
      account_id: original.account_id,
      calendar_id: original.calendar_id,
      title: editTitle.value,
      description: editDescription.value || null,
      location: editLocation.value || null,
      start_time: startISO,
      end_time: endISO,
      all_day: editAllDay.value,
      timezone: original.timezone,
      recurrence_rule: original.recurrence_rule,
    });

    if (!isCurrentSelection(original.id, version) ||
      !calendarStore.getEventMutationSupport(original.id).supported) return;

    let notifyEventId = original.id;
    if (targetCalendarId !== original.calendar_id) {
      const target = calendarStore.calendars.find(
        (c) => c.id === targetCalendarId,
      );
      if (!target) throw new Error("The destination calendar is unavailable.");
      notifyEventId = await calendarStore.moveEventToCalendar(
        original.id, target.id, target.account_id,
      );
    }

    if (!isCurrentSelection(original.id, version)) return;
    const fresh = await refreshMutationTarget(notifyEventId, "attendee notification");
    if (!isCurrentSelection(original.id, version)) return;
    // Notify attendees if organizer and event has attendees
    if (canNotify(fresh)) {
      const result = await tauriMessage(
        "This event has attendees. Send an update notification?",
        {
          title: "Notify Attendees",
          kind: "info",
          buttons: { yes: "Send Update", no: "Don't Notify", cancel: "Cancel" },
        },
      );
      if (result === "Cancel") {
        return;
      }
      if (!isCurrentSelection(original.id, version)) return;
      // A background range refresh can evict the moved destination while
      // the dialog is open. Revalidate the captured target, not the source.
      const notificationTarget = await refreshMutationTarget(notifyEventId, "attendee notification");
      if (!isCurrentSelection(original.id, version)) return;
      if ((result === "Send Update" || result === "Yes") && canNotify(notificationTarget)) {
        await api.notifyCalendarEvent(notifyEventId);
      }
    }

    if (isCurrentSelection(original.id, version)) {
      editing.value = false;
      emit("close");
    }
  } catch (e) {
    if (isCurrentSelection(original.id, version)) error.value = String(e);
  } finally {
    saving.value = false;
  }
}

async function handleDelete() {
  if (saving.value || !event.value || !mutationSupport.value.supported) return;
  const original = event.value;
  const version = selectionVersion;
  saving.value = true;
  error.value = null;
  try {
    const fresh = await refreshMutationTarget(original.id, "deletion");
    if (!isCurrentSelection(original.id, version)) return;
    if (canNotify(fresh)) {
      const result = await tauriMessage(
        "Delete this event with attendees? Chithi does not send manual cancellation notifications. Your calendar provider may notify attendees automatically.",
        {
          title: "Delete Event",
          kind: "warning",
          buttons: { ok: "Delete", cancel: "Cancel" },
        },
      );
      if (result !== "Delete" && result !== "Ok") return;
      if (!isCurrentSelection(original.id, version)) return;
      await refreshMutationTarget(original.id, "deletion");
      if (!isCurrentSelection(original.id, version)) return;
    }

    if (!isCurrentSelection(original.id, version) ||
      !calendarStore.getEventMutationSupport(original.id).supported) return;
    await calendarStore.deleteEvent(original.id);
    if (!calendarStore.selectedEvent) emit("close");
  } catch (e) {
    if (isCurrentSelection(original.id, version)) error.value = String(e);
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <div v-if="event" class="event-detail-overlay" @click.self="emit('close')">
    <div class="event-detail">
      <div class="detail-header">
        <h3 v-if="!editing">{{ event.title }}</h3>
        <input v-else v-model="editTitle" class="edit-title" type="text" data-testid="event-form-title" />
        <button class="close-btn" @click="emit('close')">&times;</button>
      </div>

      <div v-if="error" class="detail-error">{{ error }}</div>
      <p v-if="!mutationSupport.supported" :id="mutationReasonId" class="mutation-reason">
        {{ mutationSupport.reason }}
      </p>

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
          <LinkifiedText :text="event.location" data-testid="event-location" />
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

        <div v-if="attendees.length > 0" class="detail-row">
          <span class="detail-icon">&#x1F465;</span>
          <div>
            <div v-for="a in attendees" :key="a.email" class="attendee">
              {{ a.name || a.email }}
              <span class="attendee-status" :class="statusClass(a.status)">
                ({{ a.status }})
              </span>
            </div>
          </div>
        </div>

        <div v-if="event.description" class="detail-row">
          <span class="detail-icon">&#x1F4DD;</span>
          <LinkifiedText :text="event.description" class="description" data-testid="event-description" />
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
            <DateInput v-model="editStartDate" testid="event-form-start" />
          </div>
          <div v-if="!editAllDay" class="edit-group">
            <label>Start time</label>
            <TimeInput v-model="editStartTime" testid="event-form-start-time" />
          </div>
        </div>
        <div class="edit-row">
          <div class="edit-group">
            <label>End date</label>
            <DateInput v-model="editEndDate" testid="event-form-end" />
          </div>
          <div v-if="!editAllDay" class="edit-group">
            <label>End time</label>
            <TimeInput v-model="editEndTime" testid="event-form-end-time" />
          </div>
        </div>
        <div class="edit-group">
          <label>Calendar</label>
          <select v-model="editCalendarId" data-testid="event-detail-calendar">
            <option v-for="cal in calendarStore.calendars" :key="cal.id" :value="cal.id">
              {{ calendarLabel(cal) }}
            </option>
          </select>
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
          <button class="btn-edit" :disabled="saving || !mutationSupport.supported" :aria-describedby="mutationSupport.reason ? mutationReasonId : undefined" @click="startEditing">Edit</button>
          <button class="btn-danger" :disabled="saving || !mutationSupport.supported" :aria-describedby="mutationSupport.reason ? mutationReasonId : undefined" @click="handleDelete" data-testid="event-form-delete">Delete</button>
        </template>
        <template v-else>
          <button class="btn-save" :disabled="saving || !mutationSupport.supported" :aria-describedby="mutationSupport.reason ? mutationReasonId : undefined" @click="saveEdit" data-testid="event-form-save">
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

.mutation-reason {
  padding: 12px 16px;
  color: var(--color-text-secondary);
  font-size: 13px;
}

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

/* Sizing tokens consumed by DateInput / TimeInput so they match the
   native inputs in this form. See .date-input-trigger / .time-input-text
   in src/components/common/{DateInput,TimeInput}.vue. */
.edit-group {
  --input-height: 28px;
  --input-padding: 6px 8px;
  --input-border: 1px solid var(--color-border);
  --input-bg: var(--color-bg);
  --input-font-size: 13px;
}

.edit-group input,
.edit-group select,
.edit-group textarea {
  padding: var(--input-padding);
  border: var(--input-border);
  border-radius: 4px;
  background: var(--input-bg);
  font-size: var(--input-font-size);
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

.detail-footer button:disabled { opacity: 0.5; cursor: not-allowed; }

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
