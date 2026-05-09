<script setup lang="ts">
import { computed, ref } from "vue";
import * as api from "@/lib/tauri";

const props = defineProps<{
  modelValue: string[];
  accountId?: string | null;
}>();

const emit = defineEmits<{
  "update:modelValue": [value: string[]];
}>();

const newEmail = ref("");
const suggestions = ref<{ display: string; email: string }[]>([]);
const acVisible = ref(false);
const acIndex = ref(-1);
let acDebounce: ReturnType<typeof setTimeout> | null = null;
// Monotonic id of the latest user input. Each runSearch captures the
// id at call time and discards its result if a newer input has
// landed in the meantime — without this the IPC roundtrip can race
// (type "alic", "alice", in quick succession, the older response
// arrives second and overwrites the fresher results).
let acRequestSeq = 0;

const accountIdForSearch = computed(() => props.accountId ?? null);

function addAttendee(email?: string) {
  const value = (email ?? newEmail.value).trim();
  if (value && value.includes("@") && !props.modelValue.includes(value)) {
    emit("update:modelValue", [...props.modelValue, value]);
  }
  newEmail.value = "";
  suggestions.value = [];
  acVisible.value = false;
  acIndex.value = -1;
}

function removeAttendee(email: string) {
  emit("update:modelValue", props.modelValue.filter((e) => e !== email));
}

async function runSearch(query: string, seq: number) {
  try {
    const contacts = accountIdForSearch.value
      ? await api.searchContactsForAccount(query, accountIdForSearch.value, "calendar")
      : await api.searchContacts(query);
    if (seq !== acRequestSeq) return;
    const items: { display: string; email: string }[] = [];
    const seen = new Set<string>();
    for (const c of contacts) {
      let emails: { email: string; label?: string }[] = [];
      try { emails = JSON.parse(c.emails_json); } catch { continue; }
      for (const e of emails) {
        const key = e.email.toLowerCase();
        if (seen.has(key) || props.modelValue.includes(e.email)) continue;
        seen.add(key);
        items.push({ display: c.display_name, email: e.email });
        if (items.length >= 8) break;
      }
      if (items.length >= 8) break;
    }
    suggestions.value = items;
    acVisible.value = items.length > 0;
    acIndex.value = -1;
  } catch (e) {
    if (seq !== acRequestSeq) return;
    console.warn("AttendeeEditor: search failed", e);
    suggestions.value = [];
    acVisible.value = false;
  }
}

function onInput() {
  // Always cancel any pending debounce — even on the short-query
  // branch — so a search scheduled for an earlier longer query can't
  // fire after the user has backspaced down to a too-short input
  // and silently re-populate the dropdown they just dismissed.
  if (acDebounce) {
    clearTimeout(acDebounce);
    acDebounce = null;
  }
  // Bump the request id so any in-flight runSearch from a previous
  // keystroke discards its result on completion.
  acRequestSeq += 1;
  const seq = acRequestSeq;
  const q = newEmail.value.trim();
  if (q.length < 2) {
    suggestions.value = [];
    acVisible.value = false;
    return;
  }
  acDebounce = setTimeout(() => runSearch(q, seq), 150);
}

function onKeydown(event: KeyboardEvent) {
  if (acVisible.value && suggestions.value.length > 0) {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      acIndex.value = (acIndex.value + 1) % suggestions.value.length;
      return;
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      acIndex.value =
        acIndex.value <= 0 ? suggestions.value.length - 1 : acIndex.value - 1;
      return;
    }
    if (event.key === "Escape") {
      acVisible.value = false;
      acIndex.value = -1;
      return;
    }
    if (event.key === "Enter" && acIndex.value >= 0) {
      event.preventDefault();
      addAttendee(suggestions.value[acIndex.value].email);
      return;
    }
  }
  if (event.key === "Enter" || event.key === ",") {
    event.preventDefault();
    addAttendee();
  }
}
</script>

<template>
  <div class="attendee-editor" data-testid="attendee-editor">
    <div v-if="modelValue.length > 0" class="attendee-list">
      <div v-for="email in modelValue" :key="email" class="attendee-chip">
        <span class="attendee-email">{{ email }}</span>
        <button
          class="remove-btn"
          :data-testid="`attendee-remove-${email}`"
          @click="removeAttendee(email)"
        >&times;</button>
      </div>
    </div>
    <div class="add-row">
      <input
        v-model="newEmail"
        type="email"
        placeholder="Add attendee email or name..."
        data-testid="attendee-input"
        @input="onInput"
        @keydown="onKeydown"
      />
      <button class="add-btn" data-testid="attendee-add-btn" @click="addAttendee()">Add</button>
    </div>
    <ul
      v-if="acVisible && suggestions.length > 0"
      class="ac-list"
      data-testid="attendee-suggestions"
    >
      <li
        v-for="(s, i) in suggestions"
        :key="s.email"
        class="ac-item"
        :class="{ active: i === acIndex }"
        :data-testid="`attendee-suggestion-${s.email}`"
        @mousedown.prevent="addAttendee(s.email)"
      >
        <span class="ac-name">{{ s.display }}</span>
        <span class="ac-email">{{ s.email }}</span>
      </li>
    </ul>
  </div>
</template>

<style scoped>
.attendee-editor {
  padding: 4px 0;
  position: relative;
}

.attendee-list {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
  margin-bottom: 8px;
}

.attendee-chip {
  display: flex;
  align-items: center;
  gap: 4px;
  background: var(--color-bg-tertiary);
  border: 1px solid var(--color-border);
  border-radius: 16px;
  padding: 3px 6px 3px 10px;
  font-size: 12px;
}

.attendee-email {
  color: var(--color-text-secondary);
}

.remove-btn {
  width: 18px;
  height: 18px;
  border-radius: 50%;
  font-size: 13px;
  color: var(--color-text-muted);
  display: flex;
  align-items: center;
  justify-content: center;
}

.remove-btn:hover {
  background: var(--color-bg-hover);
  color: var(--color-danger);
}

.add-row {
  display: flex;
  gap: 6px;
}

.add-row input {
  flex: 1;
  padding: 5px 8px;
  border: 1px solid var(--color-border);
  border-radius: 6px;
  background: var(--color-bg);
  font-size: 12px;
}

.add-btn {
  padding: 5px 12px;
  border-radius: 6px;
  font-size: 12px;
  font-weight: 500;
  color: var(--color-accent);
  border: 1px solid var(--color-accent);
}

.add-btn:hover {
  background: rgba(137, 180, 250, 0.1);
}

.ac-list {
  position: absolute;
  left: 0;
  right: 0;
  top: 100%;
  margin: 2px 0 0 0;
  padding: 0;
  list-style: none;
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: 6px;
  z-index: 20;
  max-height: 240px;
  overflow-y: auto;
  box-shadow: 0 4px 14px rgba(0, 0, 0, 0.15);
}

.ac-item {
  display: flex;
  flex-direction: column;
  padding: 6px 10px;
  cursor: pointer;
  font-size: 12px;
}

.ac-item.active,
.ac-item:hover {
  background: var(--color-bg-hover);
}

.ac-name {
  color: var(--color-text);
  font-weight: 500;
}

.ac-email {
  color: var(--color-text-muted);
  font-size: 11px;
}
</style>
