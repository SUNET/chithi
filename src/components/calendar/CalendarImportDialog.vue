<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import ModalShell from "@/components/common/ModalShell.vue";
import Select from "@/components/common/Select.vue";
import { useAccountsStore } from "@/stores/accounts";
import { useCalendarStore } from "@/stores/calendar";
import { useUiStore } from "@/stores/ui";
import { formatInTimezone } from "@/lib/datetime";
import type {
  Attachment,
  Calendar,
  CalendarImportPreview,
  CalendarImportResult,
} from "@/lib/types";
import * as api from "@/lib/tauri";

const props = defineProps<{
  sourceAccountId: string;
  messageId: string;
  attachment: Attachment;
}>();

const emit = defineEmits<{
  close: [];
  download: [];
  imported: [result: CalendarImportResult];
}>();

const accountsStore = useAccountsStore();
const calendarStore = useCalendarStore();
const uiStore = useUiStore();
const previews = ref<CalendarImportPreview[]>([]);
const selectedUids = ref<string[]>([]);
const calendarId = ref("");
const loading = ref(true);
const importing = ref(false);
const error = ref<string | null>(null);
const importTargets = ref<Calendar[]>([]);
let targetsLoaded = false;
let previewVersion = 0;

const calendarOptions = computed(() =>
  importTargets.value.map((calendar) => ({
    value: calendar.id,
    label: `${calendar.name} (${accountsStore.accounts.find(
      (account) => account.id === calendar.account_id,
    )?.display_name || calendar.account_id})`,
  })),
);

const selectedCount = computed(() => selectedUids.value.length);

function chooseDefaultCalendar(preferredCalendarId: string | null) {
  const preferred = importTargets.value.find(
    (calendar) => calendar.id === preferredCalendarId,
  );
  const sourceDefault = importTargets.value.find(
    (calendar) =>
      calendar.account_id === props.sourceAccountId && calendar.is_default,
  );
  const fallback =
    preferred ??
    sourceDefault ??
    importTargets.value.find((calendar) => calendar.is_default) ??
    importTargets.value[0];
  calendarId.value = fallback?.id ?? "";
}

function formatRange(event: CalendarImportPreview): string {
  if (event.all_day) return formatAllDayDate(event.start_time);
  const start = formatInTimezone(event.start_time, uiStore.displayTimezone, {
    hour12: uiStore.hour12,
  });
  const end = formatInTimezone(event.end_time, uiStore.displayTimezone, {
    hour12: uiStore.hour12,
  });
  return `${start} – ${end}`;
}

function formatAllDayDate(value: string): string {
  const match = /^(\d{4})-(\d{2})-(\d{2})/.exec(value);
  if (!match) return value;
  const date = new Date(Number(match[1]), Number(match[2]) - 1, Number(match[3]));
  return date.toLocaleDateString(undefined, {
    weekday: "long",
    year: "numeric",
    month: "long",
    day: "numeric",
  });
}

function toggle(uid: string) {
  selectedUids.value = selectedUids.value.includes(uid)
    ? selectedUids.value.filter((selected) => selected !== uid)
    : [...selectedUids.value, uid];
}

function close() {
  if (!importing.value) emit("close");
}

async function loadPreview() {
  if (!calendarId.value) {
    previews.value = [];
    selectedUids.value = [];
    loading.value = false;
    return;
  }
  const version = ++previewVersion;
  loading.value = true;
  error.value = null;
  try {
    const nextPreviews = await api.previewCalendarAttachment(
      props.sourceAccountId,
      props.messageId,
      props.attachment.index,
      calendarId.value,
    );
    if (version !== previewVersion) return;
    previews.value = nextPreviews;
    selectedUids.value = nextPreviews
      .filter((event) => event.importable)
      .map((event) => event.uid);
  } catch (cause) {
    if (version !== previewVersion) return;
    previews.value = [];
    selectedUids.value = [];
    error.value = cause instanceof Error ? cause.message : String(cause);
  } finally {
    if (version === previewVersion) loading.value = false;
  }
}

async function load() {
  loading.value = true;
  error.value = null;
  try {
    const [targets, preferredCalendarId] = await Promise.all([
      api.listCalendarImportTargets(),
      api.getDefaultImportCalendar(props.sourceAccountId).catch(() => null),
    ]);
    importTargets.value = targets;
    chooseDefaultCalendar(preferredCalendarId);
    targetsLoaded = true;
    await loadPreview();
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause);
    loading.value = false;
  }
}

async function importEvents() {
  if (!calendarId.value || selectedUids.value.length === 0 || importing.value) {
    return;
  }
  importing.value = true;
  error.value = null;
  try {
    const result = await api.importCalendarAttachment(
      props.sourceAccountId,
      props.messageId,
      props.attachment.index,
      calendarId.value,
      selectedUids.value,
    );
    await calendarStore.fetchEvents();
    emit("imported", result);
    emit("close");
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause);
  } finally {
    importing.value = false;
  }
}

function onKeydown(event: KeyboardEvent) {
  if (event.key === "Escape") close();
}

onMounted(() => {
  window.addEventListener("keydown", onKeydown);
  void load();
});
onUnmounted(() => window.removeEventListener("keydown", onKeydown));

watch(
  calendarId,
  () => {
    if (targetsLoaded) void loadPreview();
  },
  { flush: "sync" },
);
</script>

<template>
  <ModalShell
    :open="true"
    title="Import calendar events"
    modal-class="calendar-import-modal"
    role="dialog"
    aria-modal="true"
    aria-label="Import calendar events"
    data-testid="calendar-import-dialog"
    @close="close"
  >
    <p class="filename">{{ attachment.filename || "Calendar attachment" }}</p>

    <div v-if="loading" class="state">Reading calendar attachment…</div>
    <div v-else-if="error && previews.length === 0" class="error">
      {{ error }}
    </div>
    <template v-else>
      <div v-if="previews.length > 0" class="event-list">
        <label
          v-for="event in previews"
          :key="event.uid"
          class="event-row"
          :class="{ disabled: !event.importable }"
        >
          <input
            type="checkbox"
            :checked="selectedUids.includes(event.uid)"
            :disabled="!event.importable || importing"
            :data-testid="`calendar-import-event-${event.uid}`"
            @change="toggle(event.uid)"
          />
          <span class="event-details">
            <strong>{{ event.title }}</strong>
            <span>{{ formatRange(event) }}</span>
            <span v-if="event.location">{{ event.location }}</span>
            <span v-if="event.component_count > 1">
              Recurring series with {{ event.component_count - 1 }} exception{{
                event.component_count === 2 ? "" : "s"
              }}
            </span>
            <span v-if="!event.importable">
              {{ event.import_error || `${event.method} messages cannot be imported.` }}
            </span>
          </span>
        </label>
      </div>

      <div v-if="previews.some((event) => event.attendee_count > 0)" class="notice">
        Imported events are personal copies. Importing does not RSVP or notify
        the organizer and attendees.
      </div>

      <label class="calendar-field">
        <span>Destination calendar</span>
        <Select
          v-model="calendarId"
          :options="calendarOptions"
          placeholder="Choose a calendar"
          aria-label="Destination calendar"
          testid="calendar-import-calendar"
        />
      </label>

      <div v-if="calendarOptions.length === 0" class="error">
        No subscribed calendar is available.
      </div>
      <div v-else-if="error" class="error">{{ error }}</div>
    </template>

    <template #footer>
      <button class="btn secondary" :disabled="importing" @click="emit('download')">
        Download instead
      </button>
      <button class="btn secondary" :disabled="importing" @click="close">
        Cancel
      </button>
      <button
        class="btn primary"
        :disabled="loading || importing || !calendarId || selectedCount === 0"
        data-testid="calendar-import-submit"
        @click="importEvents"
      >
        {{
          importing
            ? "Importing…"
            : `Import ${selectedCount} event${selectedCount === 1 ? "" : "s"}`
        }}
      </button>
    </template>
  </ModalShell>
</template>

<style scoped>
.filename {
  margin: 0 0 14px;
  color: var(--color-text-secondary);
  font-size: 13px;
  overflow-wrap: anywhere;
}

.state {
  padding: 24px 0;
  color: var(--color-text-secondary);
  text-align: center;
}

.event-list {
  display: flex;
  flex-direction: column;
  gap: 8px;
  max-height: 42vh;
  overflow-y: auto;
}

.event-row {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 11px;
  border: 1px solid var(--color-border);
  border-radius: 8px;
  cursor: pointer;
}

.event-row.disabled {
  cursor: not-allowed;
  opacity: 0.65;
}

.event-row input {
  margin-top: 3px;
}

.event-details {
  display: flex;
  min-width: 0;
  flex: 1;
  flex-direction: column;
  gap: 3px;
  color: var(--color-text-secondary);
  font-size: 12px;
}

.event-details strong {
  color: var(--color-text);
  font-size: 14px;
}

.notice,
.error {
  margin-top: 12px;
  padding: 9px 11px;
  border-radius: 7px;
  font-size: 12px;
}

.notice {
  background: var(--color-bg-secondary);
  color: var(--color-text-secondary);
}

.error {
  background: rgba(251, 44, 54, 0.08);
  color: var(--color-danger-text);
}

.calendar-field {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin-top: 16px;
  color: var(--color-text-secondary);
  font-size: 13px;
}

.btn {
  padding: 7px 13px;
  border-radius: 7px;
  font-size: 13px;
  font-weight: 500;
}

.btn.secondary {
  border: 1px solid var(--color-border);
  color: var(--color-text);
}

.btn.primary {
  background: var(--color-accent);
  color: white;
}

.btn:disabled {
  cursor: not-allowed;
  opacity: 0.5;
}
</style>
