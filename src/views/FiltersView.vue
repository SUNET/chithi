<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useFiltersStore } from "@/stores/filters";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import type { FilterRule, FilterAction } from "@/lib/types";

const filtersStore = useFiltersStore();
const accountsStore = useAccountsStore();
const foldersStore = useFoldersStore();

const editingRule = ref<FilterRule | null>(null);
const applyingFolder = ref<string | null>(null);
const applyResult = ref<string | null>(null);

function actionSummary(rule: FilterRule): string {
  const first = rule.actions[0];
  if (!first) return "";
  switch (first.action) {
    case "move": return `Move to ${first.target || "..."}`;
    case "copy": return `Copy to ${first.target || "..."}`;
    case "delete": return "Delete";
    case "flag": return "Add flag";
    case "unflag": return "Remove flag";
    case "mark_read": return "Mark as read";
    case "mark_unread": return "Mark as unread";
    case "stop": return "Stop processing";
  }
}

function newFilter() {
  editingRule.value = {
    id: crypto.randomUUID(),
    account_id: accountsStore.activeAccountId,
    name: "",
    enabled: true,
    priority: filtersStore.filters.length,
    match_type: "all",
    conditions: [{ field: "from", op: "contains", value: "" }],
    actions: [{ action: "move", target: "" }],
    stop_processing: true,
  };
}

function editFilter(rule: FilterRule) {
  editingRule.value = JSON.parse(JSON.stringify(rule));
}

function addCondition() {
  if (!editingRule.value) return;
  editingRule.value.conditions.push({ field: "from", op: "contains", value: "" });
}

function removeCondition(index: number) {
  if (!editingRule.value) return;
  editingRule.value.conditions.splice(index, 1);
}

function addAction() {
  if (!editingRule.value) return;
  editingRule.value.actions.push({ action: "mark_read" });
}

function removeAction(index: number) {
  if (!editingRule.value) return;
  editingRule.value.actions.splice(index, 1);
}

function updateAction(index: number, type: string) {
  if (!editingRule.value) return;
  editingRule.value.actions[index] = createDefaultAction(type);
}

function createDefaultAction(type: string): FilterAction {
  switch (type) {
    case "move": return { action: "move", target: "" };
    case "copy": return { action: "copy", target: "" };
    case "delete": return { action: "delete" };
    case "flag": return { action: "flag", value: "flagged" };
    case "unflag": return { action: "unflag", value: "flagged" };
    case "mark_read": return { action: "mark_read" };
    case "mark_unread": return { action: "mark_unread" };
    case "stop": return { action: "stop" };
    default: return { action: "mark_read" };
  }
}

function getActionType(action: FilterAction): string {
  return action.action;
}

function getActionTarget(action: FilterAction): string {
  if ("target" in action) return action.target;
  if ("value" in action) return action.value;
  return "";
}

function setActionTarget(index: number, value: string) {
  if (!editingRule.value) return;
  const action = editingRule.value.actions[index];
  if ("target" in action) action.target = value;
  else if ("value" in action) action.value = value;
}

function needsTarget(action: FilterAction): boolean {
  return action.action === "move" || action.action === "copy";
}

function needsValue(action: FilterAction): boolean {
  return action.action === "flag" || action.action === "unflag";
}

async function toggleEnabled(rule: FilterRule) {
  const updated = { ...rule, enabled: !rule.enabled };
  await filtersStore.saveFilter(updated);
}

async function saveFilter() {
  if (!editingRule.value) return;
  await filtersStore.saveFilter(editingRule.value);
  editingRule.value = null;
}

async function deleteFilter(id: string) {
  await filtersStore.deleteFilter(id);
  if (editingRule.value?.id === id) {
    editingRule.value = null;
  }
}

async function applyToFolder() {
  const accountId = accountsStore.activeAccountId;
  const folder = applyingFolder.value;
  if (!accountId || !folder) return;
  try {
    const count = await filtersStore.applyToFolder(accountId, folder);
    applyResult.value = `Filters applied: ${count} messages affected`;
    setTimeout(() => (applyResult.value = null), 5000);
  } catch (e) {
    applyResult.value = `Error: ${e}`;
  }
}

onMounted(() => {
  filtersStore.fetchFilters();
  if (accountsStore.activeAccountId) {
    foldersStore.fetchFolders();
  }
});

const fieldLabels: Record<string, string> = {
  from: "From",
  to: "To",
  cc: "Cc",
  subject: "Subject",
  size: "Size (bytes)",
  has_attachment: "Has Attachment",
};

const opLabels: Record<string, string> = {
  contains: "contains",
  not_contains: "does not contain",
  equals: "equals",
  not_equals: "does not equal",
  matches_regex: "matches regex",
  greater_than: "greater than",
  less_than: "less than",
};
</script>

<template>
  <div class="filters-view">
    <!-- Left Panel: filter list -->
    <div class="filters-left">
      <div class="left-header">
        <h2 class="left-title">Message Filters</h2>
        <select class="account-select" :value="accountsStore.activeAccountId ?? ''" @change="accountsStore.setActiveAccount(($event.target as HTMLSelectElement).value)">
          <option v-for="acc in accountsStore.accounts" :key="acc.id" :value="acc.id">
            {{ acc.display_name }}
          </option>
        </select>
        <button class="btn-new-filter" :disabled="!accountsStore.activeAccountId" @click="newFilter">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="12" y1="5" x2="12" y2="19" /><line x1="5" y1="12" x2="19" y2="12" /></svg>
          New Filter
        </button>
      </div>

      <div class="filter-list">
        <div
          v-for="rule in filtersStore.filters"
          :key="rule.id"
          class="filter-item"
          :class="{ active: editingRule?.id === rule.id, disabled: !rule.enabled }"
          @click="editFilter(rule)"
        >
          <div class="filter-drag">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="9" cy="6" r="1" /><circle cx="15" cy="6" r="1" /><circle cx="9" cy="12" r="1" /><circle cx="15" cy="12" r="1" /><circle cx="9" cy="18" r="1" /><circle cx="15" cy="18" r="1" /></svg>
          </div>
          <div class="filter-info">
            <span class="filter-name">{{ rule.name || "(unnamed)" }}</span>
            <span class="filter-summary">
              {{ rule.conditions.length }} condition{{ rule.conditions.length !== 1 ? "s" : "" }}
              &rarr; {{ actionSummary(rule) }}
            </span>
          </div>
          <button
            class="toggle-switch"
            :class="{ on: rule.enabled }"
            @click.stop="toggleEnabled(rule)"
          >
            <span class="toggle-knob"></span>
          </button>
        </div>
        <div v-if="filtersStore.filters.length === 0" class="empty-list">
          <svg width="32" height="32" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1" stroke-linecap="round" stroke-linejoin="round" style="opacity:0.3">
            <polygon points="22 3 2 3 10 12.46 10 19 14 21 14 12.46 22 3" />
          </svg>
          <span>No filters configured</span>
          <span class="empty-hint">Create your first filter to automatically organize incoming mail</span>
        </div>
      </div>

      <div class="apply-section">
        <label class="apply-label">Apply filters to folder</label>
        <select v-model="applyingFolder" class="apply-select">
          <option :value="null" disabled>Select folder...</option>
          <option v-for="f in foldersStore.folders" :key="f.path" :value="f.path">
            {{ f.name }}
          </option>
        </select>
        <button class="btn-apply" :disabled="!applyingFolder" @click="applyToFolder">
          Apply Filters to Folder
        </button>
        <span v-if="applyResult" class="apply-result">{{ applyResult }}</span>
      </div>
    </div>

    <!-- Right Panel: editor -->
    <div class="filters-right">
      <div v-if="!editingRule" class="editor-empty">
        <span>Select a filter to edit or create a new one</span>
      </div>
      <div v-else class="editor-form">
        <!-- Basics -->
        <div class="form-group">
          <label class="field-label">Filter Name *</label>
          <input v-model="editingRule.name" type="text" class="field-input" placeholder="Enter filter name" />
        </div>

        <div class="basics-row">
          <div class="form-group priority-group">
            <label class="field-label">Priority</label>
            <input v-model.number="editingRule.priority" type="number" class="field-input" />
          </div>
          <div class="form-group match-group">
            <label class="field-label">Match Type</label>
            <select v-model="editingRule.match_type" class="field-select">
              <option value="all">All conditions (AND)</option>
              <option value="any">Any condition (OR)</option>
            </select>
          </div>
          <label class="checkbox-label">
            <input v-model="editingRule.enabled" type="checkbox" />
            Enabled
          </label>
        </div>

        <div class="options-row">
          <label class="checkbox-label">
            <input v-model="editingRule.stop_processing" type="checkbox" />
            Stop processing after this filter matches
          </label>
          <span class="badge-local">
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2" /><path d="M8 21h8M12 17v4" /></svg>
            Local
          </span>
        </div>

        <!-- Conditions -->
        <div class="section-header">
          <span class="section-title">Conditions</span>
          <button class="btn-add-link" @click="addCondition">
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><line x1="12" y1="5" x2="12" y2="19" /><line x1="5" y1="12" x2="19" y2="12" /></svg>
            Add Condition
          </button>
        </div>
        <div
          v-for="(cond, i) in editingRule.conditions"
          :key="'c' + i"
          class="condition-row"
        >
          <select v-model="cond.field" class="field-select cond-field">
            <option v-for="(label, key) in fieldLabels" :key="key" :value="key">{{ label }}</option>
          </select>
          <select v-model="cond.op" class="field-select cond-op">
            <option v-for="(label, key) in opLabels" :key="key" :value="key">{{ label }}</option>
          </select>
          <input v-model="cond.value" type="text" class="field-input cond-value" placeholder="Value" />
          <button class="btn-remove" @click="removeCondition(i)">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" /></svg>
          </button>
        </div>

        <!-- Actions -->
        <div class="section-header">
          <span class="section-title">Actions</span>
          <button class="btn-add-link" @click="addAction">
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><line x1="12" y1="5" x2="12" y2="19" /><line x1="5" y1="12" x2="19" y2="12" /></svg>
            Add Action
          </button>
        </div>
        <div
          v-for="(action, i) in editingRule.actions"
          :key="'a' + i"
          class="action-row"
        >
          <select class="field-select action-type" :value="getActionType(action)" @change="updateAction(i, ($event.target as HTMLSelectElement).value)">
            <option value="move">Move to folder</option>
            <option value="copy">Copy to folder</option>
            <option value="delete">Delete</option>
            <option value="flag">Add flag</option>
            <option value="unflag">Remove flag</option>
            <option value="mark_read">Mark as read</option>
            <option value="mark_unread">Mark as unread</option>
            <option value="stop">Stop processing</option>
          </select>
          <select
            v-if="needsTarget(action)"
            class="field-select action-target"
            :value="getActionTarget(action)"
            @change="setActionTarget(i, ($event.target as HTMLSelectElement).value)"
          >
            <option value="" disabled>Select folder...</option>
            <option v-for="f in foldersStore.folders" :key="f.path" :value="f.path">
              {{ f.name }}
            </option>
          </select>
          <select
            v-else-if="needsValue(action)"
            class="field-select action-target"
            :value="getActionTarget(action)"
            @change="setActionTarget(i, ($event.target as HTMLSelectElement).value)"
          >
            <option value="flagged">Flagged</option>
            <option value="seen">Seen</option>
            <option value="answered">Answered</option>
          </select>
          <button class="btn-remove" @click="removeAction(i)">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" /></svg>
          </button>
        </div>

        <!-- Footer -->
        <div class="editor-footer">
          <button v-if="editingRule.id && filtersStore.filters.some(f => f.id === editingRule?.id)" class="btn-delete" @click="deleteFilter(editingRule.id)">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" /></svg>
            Delete
          </button>
          <div class="footer-right">
            <button class="btn-cancel" @click="editingRule = null">Cancel</button>
            <button class="btn-save" @click="saveFilter">Save Filter</button>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.filters-view {
  display: flex;
  height: 100%;
  background: var(--color-bg);
}

/* --- Left Panel --- */
.filters-left {
  width: 360px;
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  border-right: 1px solid var(--color-border);
  background: white;
}

.left-header {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 16px;
  border-bottom: 1px solid var(--color-border);
}

.left-title {
  font-size: 16px;
  font-weight: 600;
  color: var(--color-text);
  margin: 0;
}

.account-select {
  width: 100%;
  height: 36px;
  padding: 0 8px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg-secondary);
  color: var(--color-text);
  font-size: 13px;
}

.btn-new-filter {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  width: 100%;
  height: 36px;
  background: var(--color-accent);
  color: white;
  border: none;
  border-radius: 4px;
  font-size: 14px;
  font-weight: 500;
  cursor: pointer;
}

.btn-new-filter:hover {
  background: var(--color-accent-hover);
}

.btn-new-filter:disabled {
  opacity: 0.5;
  cursor: default;
}

/* Filter list */
.filter-list {
  flex: 1;
  overflow-y: auto;
  padding: 8px;
}

.filter-item {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  padding: 12px;
  border-radius: 4px;
  cursor: pointer;
  margin-bottom: 4px;
}

.filter-item:hover {
  background: var(--color-bg-hover);
}

.filter-item.active {
  background: var(--color-accent-light);
}

.filter-item.disabled {
  opacity: 0.5;
}

.filter-drag {
  flex-shrink: 0;
  color: var(--color-border);
  cursor: grab;
  margin-top: 2px;
}

.filter-drag:hover {
  color: var(--color-text-muted);
}

.filter-info {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.filter-name {
  font-size: 14px;
  font-weight: 600;
  color: var(--color-text);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.filter-summary {
  font-size: 12px;
  color: var(--color-text-muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* Toggle switch */
.toggle-switch {
  position: relative;
  width: 32px;
  height: 20px;
  border-radius: 9999px;
  background: var(--color-border);
  border: none;
  cursor: pointer;
  flex-shrink: 0;
  margin-top: 2px;
  transition: background 0.15s;
}

.toggle-switch.on {
  background: var(--color-accent);
}

.toggle-knob {
  position: absolute;
  top: 2px;
  left: 2px;
  width: 16px;
  height: 16px;
  border-radius: 50%;
  background: white;
  transition: transform 0.15s;
}

.toggle-switch.on .toggle-knob {
  transform: translateX(12px);
}

.empty-list {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 8px;
  padding: 48px 24px;
  color: var(--color-text-muted);
  font-size: 13px;
  text-align: center;
}

.empty-hint {
  font-size: 12px;
  max-width: 200px;
}

/* Apply section */
.apply-section {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 16px;
  border-top: 1px solid var(--color-border);
}

.apply-label {
  font-size: 12px;
  font-weight: 500;
  color: var(--color-text-muted);
}

.apply-select {
  width: 100%;
  height: 36px;
  padding: 0 8px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg-secondary);
  color: var(--color-text);
  font-size: 13px;
}

.btn-apply {
  width: 100%;
  height: 32px;
  background: var(--color-bg-hover);
  color: var(--color-text);
  border: none;
  border-radius: 4px;
  font-size: 14px;
  font-weight: 500;
  cursor: pointer;
}

.btn-apply:hover {
  background: var(--color-bg-active);
}

.btn-apply:disabled {
  opacity: 0.5;
  cursor: default;
}

.apply-result {
  font-size: 12px;
  color: var(--color-text-muted);
}

/* --- Right Panel --- */
.filters-right {
  flex: 1;
  overflow-y: auto;
  background: white;
}

.editor-empty {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 100%;
  color: var(--color-text-muted);
  font-size: 13px;
}

.editor-form {
  padding: 24px;
  display: flex;
  flex-direction: column;
  gap: 24px;
}

/* Form fields */
.form-group {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.field-label {
  font-size: 12px;
  font-weight: 500;
  color: var(--color-text-muted);
}

.field-input,
.field-select {
  height: 36px;
  padding: 0 12px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg-secondary);
  color: var(--color-text);
  font-size: 14px;
}

.field-input:focus,
.field-select:focus {
  outline: none;
  border-color: var(--color-accent);
}

.field-input[type="text"],
.field-input[type="number"] {
  width: 100%;
  box-sizing: border-box;
}

/* Basics row */
.basics-row {
  display: flex;
  gap: 16px;
  align-items: flex-end;
}

.priority-group {
  width: 100px;
}

.match-group {
  flex: 1;
}

.checkbox-label {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 14px;
  font-weight: 500;
  color: var(--color-text);
  cursor: pointer;
  white-space: nowrap;
  height: 36px;
}

.checkbox-label input[type="checkbox"] {
  width: 16px;
  height: 16px;
}

.options-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
}

.badge-local {
  display: flex;
  align-items: center;
  gap: 4px;
  background: var(--color-bg-hover);
  color: var(--color-text-secondary);
  font-size: 12px;
  padding: 4px 8px;
  border-radius: 4px;
}

/* Section headers */
.section-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding-bottom: 8px;
  border-bottom: 1px solid var(--color-border);
}

.section-title {
  font-size: 11px;
  font-weight: 500;
  color: var(--color-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.275px;
}

.btn-add-link {
  display: flex;
  align-items: center;
  gap: 4px;
  background: none;
  border: none;
  color: var(--color-accent);
  font-size: 12px;
  font-weight: 500;
  cursor: pointer;
}

.btn-add-link:hover {
  text-decoration: underline;
}

/* Condition/action rows */
.condition-row,
.action-row {
  display: flex;
  gap: 8px;
  align-items: center;
}

.cond-field,
.action-type {
  width: 160px;
  flex-shrink: 0;
}

.cond-op {
  width: 160px;
  flex-shrink: 0;
}

.cond-value {
  flex: 1;
  min-width: 0;
}

.action-target {
  flex: 1;
  min-width: 0;
}

.btn-remove {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 32px;
  height: 32px;
  border-radius: 4px;
  background: none;
  border: none;
  color: var(--color-danger-text, #dc2626);
  cursor: pointer;
  flex-shrink: 0;
}

.btn-remove:hover {
  background: rgba(251, 44, 54, 0.06);
}

/* Footer */
.editor-footer {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding-top: 16px;
  border-top: 1px solid var(--color-border);
}

.footer-right {
  display: flex;
  gap: 8px;
  margin-left: auto;
}

.btn-cancel {
  height: 36px;
  padding: 0 16px;
  background: var(--color-bg-hover);
  color: var(--color-text);
  border: none;
  border-radius: 4px;
  font-size: 14px;
  font-weight: 500;
  cursor: pointer;
}

.btn-cancel:hover {
  background: var(--color-bg-active);
}

.btn-save {
  height: 36px;
  padding: 0 16px;
  background: var(--color-accent);
  color: white;
  border: none;
  border-radius: 4px;
  font-size: 14px;
  font-weight: 500;
  cursor: pointer;
}

.btn-save:hover {
  background: var(--color-accent-hover);
}

.btn-delete {
  display: flex;
  align-items: center;
  gap: 6px;
  height: 36px;
  padding: 0 12px;
  background: none;
  border: none;
  color: var(--color-danger-text, #dc2626);
  font-size: 13px;
  cursor: pointer;
  border-radius: 4px;
}

.btn-delete:hover {
  background: rgba(251, 44, 54, 0.06);
}
</style>
