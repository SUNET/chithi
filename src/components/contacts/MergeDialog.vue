<script setup lang="ts">
import { computed, ref, watch } from "vue";
import type { Contact } from "@/lib/types";
import * as api from "@/lib/tauri";
import {
  applyMergeChoices,
  defaultChoices,
  type MergeChoices,
} from "@/lib/contact-merge";

const props = defineProps<{
  pair: { keeper: Contact; loser: Contact } | null;
}>();
const emit = defineEmits<{ merged: [surviving: Contact]; cancel: [] }>();

const mergeChoices = ref<MergeChoices | null>(null);
const merging = ref(false);
const mergeError = ref<string | null>(null);

watch(
  () => props.pair,
  (pair) => {
    mergeError.value = null;
    merging.value = false;
    mergeChoices.value = pair ? defaultChoices(pair.keeper, pair.loser) : null;
  },
  { immediate: true },
);

const mergePreview = computed(() => {
  if (!props.pair || !mergeChoices.value) return null;
  return applyMergeChoices(props.pair.keeper, props.pair.loser, mergeChoices.value);
});

// Only non-empty conflicts require a choice.
function fieldsDiffer(
  a: string | null | undefined,
  b: string | null | undefined,
): boolean {
  const at = (a ?? "").trim();
  const bt = (b ?? "").trim();
  return at.length > 0 && bt.length > 0 && at !== bt;
}

function addressItemDisplay(entry: { item: Record<string, unknown> }): string {
  return JSON.stringify(entry.item);
}

// Update first so a failed write never deletes the loser.
async function applyMerge() {
  const pair = props.pair;
  const surviving = mergePreview.value;
  if (!pair || !surviving) return;
  merging.value = true;
  mergeError.value = null;
  try {
    await api.updateContact(surviving);
    await api.deleteContact(pair.loser.id);
  } catch (error) {
    // Keep the choices available for retry.
    mergeError.value = error instanceof Error ? error.message : String(error);
    return;
  } finally {
    merging.value = false;
  }
  emit("merged", surviving);
}
</script>

<template>
  <Teleport to="body">
    <div
      v-if="pair && mergeChoices"
      class="modal-overlay"
      data-testid="merge-dialog"
      @click.self="emit('cancel')"
    >
      <div class="modal">
        <div class="modal-header">
          <h3>Merge contacts</h3>
          <button class="modal-close" @click="emit('cancel')">&times;</button>
        </div>
        <div class="modal-body merge-dialog-body">
          <p class="merge-picker-hint">
            Keeping <strong>{{ pair.keeper.display_name }}</strong>'s identity. <strong>{{ pair.loser.display_name }}</strong> will be deleted after the merge. Pick which value to keep for any field where the two disagree.
          </p>
          <div v-if="mergeError" class="form-error" data-testid="merge-error">{{ mergeError }}</div>

          <fieldset class="merge-field" data-testid="merge-field-name">
            <legend>Name</legend>
            <template v-if="fieldsDiffer(pair.keeper.display_name, pair.loser.display_name)">
              <label class="merge-radio">
                <input
                  type="radio"
                  value="keeper"
                  v-model="mergeChoices.display_name"
                  data-testid="merge-name-keeper"
                />
                <span>{{ pair.keeper.display_name }}</span>
              </label>
              <label class="merge-radio">
                <input
                  type="radio"
                  value="loser"
                  v-model="mergeChoices.display_name"
                  data-testid="merge-name-loser"
                />
                <span>{{ pair.loser.display_name }}</span>
              </label>
            </template>
            <span v-else class="merge-resolved">{{ mergePreview?.display_name || "—" }}</span>
          </fieldset>

          <fieldset class="merge-field" data-testid="merge-field-org">
            <legend>Organization</legend>
            <template v-if="fieldsDiffer(pair.keeper.organization, pair.loser.organization)">
              <label class="merge-radio">
                <input type="radio" value="keeper" v-model="mergeChoices.organization" />
                <span>{{ pair.keeper.organization }}</span>
              </label>
              <label class="merge-radio">
                <input type="radio" value="loser" v-model="mergeChoices.organization" />
                <span>{{ pair.loser.organization }}</span>
              </label>
            </template>
            <span v-else class="merge-resolved">{{ mergePreview?.organization || "—" }}</span>
          </fieldset>

          <fieldset class="merge-field" data-testid="merge-field-title">
            <legend>Title</legend>
            <template v-if="fieldsDiffer(pair.keeper.title, pair.loser.title)">
              <label class="merge-radio">
                <input type="radio" value="keeper" v-model="mergeChoices.title" />
                <span>{{ pair.keeper.title }}</span>
              </label>
              <label class="merge-radio">
                <input type="radio" value="loser" v-model="mergeChoices.title" />
                <span>{{ pair.loser.title }}</span>
              </label>
            </template>
            <span v-else class="merge-resolved">{{ mergePreview?.title || "—" }}</span>
          </fieldset>

          <fieldset class="merge-field" data-testid="merge-field-notes">
            <legend>Notes</legend>
            <template v-if="fieldsDiffer(pair.keeper.notes, pair.loser.notes)">
              <label class="merge-radio">
                <input type="radio" value="keeper" v-model="mergeChoices.notes" />
                <span class="merge-notes-snippet">{{ pair.keeper.notes }}</span>
              </label>
              <label class="merge-radio">
                <input type="radio" value="loser" v-model="mergeChoices.notes" />
                <span class="merge-notes-snippet">{{ pair.loser.notes }}</span>
              </label>
              <label class="merge-radio">
                <input type="radio" value="both" v-model="mergeChoices.notes" />
                <span>Combine both</span>
              </label>
            </template>
            <span v-else class="merge-resolved merge-notes-snippet">{{ mergePreview?.notes || "—" }}</span>
          </fieldset>

          <fieldset
            v-if="mergeChoices.emails.length"
            class="merge-field"
            data-testid="merge-field-emails"
          >
            <legend>Emails</legend>
            <label
              v-for="(it, idx) in mergeChoices.emails"
              :key="String(it.item.email ?? '') + '-' + idx"
              class="merge-checkbox"
            >
              <input type="checkbox" v-model="it.include" />
              <span class="merge-source-tag" :class="`source-${it.source}`">
                {{ it.source === "keeper" ? pair.keeper.display_name : pair.loser.display_name }}
              </span>
              <span>{{ String(it.item.label || "") }}: {{ String(it.item.email || "") }}</span>
            </label>
          </fieldset>

          <fieldset
            v-if="mergeChoices.phones.length"
            class="merge-field"
            data-testid="merge-field-phones"
          >
            <legend>Phones</legend>
            <label
              v-for="(it, idx) in mergeChoices.phones"
              :key="String(it.item.number ?? '') + '-' + idx"
              class="merge-checkbox"
            >
              <input type="checkbox" v-model="it.include" />
              <span class="merge-source-tag" :class="`source-${it.source}`">
                {{ it.source === "keeper" ? pair.keeper.display_name : pair.loser.display_name }}
              </span>
              <span>{{ String(it.item.label || "") }}: {{ String(it.item.number || "") }}</span>
            </label>
          </fieldset>

          <fieldset
            v-if="mergeChoices.addresses.length"
            class="merge-field"
            data-testid="merge-field-addresses"
          >
            <legend>Addresses</legend>
            <label
              v-for="(it, idx) in mergeChoices.addresses"
              :key="addressItemDisplay(it) + '-' + idx"
              class="merge-checkbox"
            >
              <input type="checkbox" v-model="it.include" />
              <span class="merge-source-tag" :class="`source-${it.source}`">
                {{ it.source === "keeper" ? pair.keeper.display_name : pair.loser.display_name }}
              </span>
              <span class="merge-address-snippet">{{ addressItemDisplay(it) }}</span>
            </label>
          </fieldset>
        </div>
        <div class="modal-footer">
          <button class="btn-cancel" :disabled="merging" @click="emit('cancel')">Cancel</button>
          <button
            class="btn-save"
            data-testid="merge-confirm-btn"
            :disabled="merging"
            @click="applyMerge"
          >
            {{ merging ? "Merging…" : "Merge" }}
          </button>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.modal-overlay {
  position: fixed; inset: 0;
  background: rgba(0, 0, 0, 0.6);
  display: grid; place-items: center;
  z-index: 1000;
}

.modal {
  background: var(--color-bg-secondary);
  border-radius: 10px;
  width: 640px;
  max-width: 96vw;
  max-height: 85vh;
  overflow-y: auto;
  box-shadow: 0 20px 25px -5px rgba(0,0,0,0.1), 0 8px 10px -6px rgba(0,0,0,0.1);
}

.modal-header {
  display: flex; justify-content: space-between; align-items: center;
  padding: 16px 20px;
  border-bottom: 0.8px solid var(--color-border);
}
.modal-header h3 { font-size: 18px; font-weight: 600; }

.modal-close {
  width: 32px; height: 32px; border-radius: 4px;
  display: grid; place-items: center;
  color: var(--color-text-muted);
}
.modal-close:hover { background: var(--color-bg-hover); }

.modal-body { padding: 20px; }

.modal-footer {
  display: flex; justify-content: flex-end; gap: 8px;
  padding: 12px 20px;
  border-top: 0.8px solid var(--color-border);
}

.form-error {
  padding: 8px 12px; background: rgba(251,44,54,0.06);
  color: var(--color-danger-text); border-radius: 6px; margin-bottom: 16px; font-size: 12px;
}

.btn-cancel {
  height: 32px; padding: 0 20px; background: var(--color-bg-tertiary);
  border-radius: 4px; font-size: 16px; font-weight: 500; color: var(--color-text);
}
.btn-save {
  height: 32px; padding: 0 20px; background: var(--color-accent);
  border-radius: 4px; font-size: 16px; font-weight: 500; color: white;
}
.btn-save:disabled { opacity: 0.5; }

.merge-picker-hint {
  margin: 0 0 12px;
  font-size: 13px;
  color: var(--color-text-muted);
  line-height: 1.4;
}

.merge-dialog-body {
  max-height: 70vh;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.merge-field {
  border: 0.8px solid var(--color-border);
  border-radius: 6px;
  padding: 8px 12px 10px;
  margin: 0;
}
.merge-field legend {
  padding: 0 6px;
  font-size: 12px;
  font-weight: 500;
  color: var(--color-text-secondary);
  text-transform: uppercase;
  letter-spacing: 0.04em;
}
.merge-radio,
.merge-checkbox {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  padding: 4px 0;
  font-size: 13px;
  color: var(--color-text);
  cursor: pointer;
}
.merge-radio input,
.merge-checkbox input {
  margin-top: 2px;
}
.merge-resolved {
  display: block;
  font-size: 13px;
  color: var(--color-text);
  padding: 4px 0;
  word-break: break-word;
}
.merge-notes-snippet {
  white-space: pre-wrap;
  word-break: break-word;
}
.merge-source-tag {
  font-size: 11px;
  font-weight: 500;
  padding: 1px 6px;
  border-radius: 3px;
  flex-shrink: 0;
}
.merge-source-tag.source-keeper {
  background: rgba(21, 93, 252, 0.12);
  color: var(--color-accent);
}
.merge-source-tag.source-loser {
  background: rgba(140, 140, 140, 0.18);
  color: var(--color-text-muted);
}
.merge-address-snippet {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12px;
  word-break: break-all;
}
</style>
