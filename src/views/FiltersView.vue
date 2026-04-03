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

function newFilter() {
  editingRule.value = {
    id: crypto.randomUUID(),
    account_id: accountsStore.activeAccountId,
    name: "",
    enabled: true,
    priority: 0,
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
  editingRule.value.conditions.push({
    field: "from",
    op: "contains",
    value: "",
  });
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
  const newAction = createDefaultAction(type);
  editingRule.value.actions[index] = newAction;
}

function createDefaultAction(type: string): FilterAction {
  switch (type) {
    case "move":
      return { action: "move", target: "" };
    case "copy":
      return { action: "copy", target: "" };
    case "delete":
      return { action: "delete" };
    case "flag":
      return { action: "flag", value: "flagged" };
    case "unflag":
      return { action: "unflag", value: "flagged" };
    case "mark_read":
      return { action: "mark_read" };
    case "mark_unread":
      return { action: "mark_unread" };
    case "stop":
      return { action: "stop" };
    default:
      return { action: "mark_read" };
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
    <div class="filters-content">
      <div class="filters-header">
        <h2>Message Filters</h2>
        <span class="account-label">Account: {{ accountsStore.activeAccount()?.display_name ?? "None" }}</span>
        <div class="header-actions">
          <button class="btn-primary" :disabled="!accountsStore.activeAccountId" @click="newFilter">New Filter</button>
        </div>
      </div>

      <!-- Apply to existing -->
      <div class="apply-section">
        <select v-model="applyingFolder" class="folder-select">
          <option :value="null" disabled>Select folder...</option>
          <option v-for="f in foldersStore.folders" :key="f.path" :value="f.path">
            {{ f.name }}
          </option>
        </select>
        <button
          class="btn-secondary"
          :disabled="!applyingFolder"
          @click="applyToFolder"
        >
          Apply Filters to Folder
        </button>
        <span v-if="applyResult" class="apply-result">{{ applyResult }}</span>
      </div>

      <!-- Filter list -->
      <div class="filter-list">
        <div
          v-for="rule in filtersStore.filters"
          :key="rule.id"
          class="filter-item"
          :class="{ disabled: !rule.enabled }"
        >
          <div class="filter-info">
            <span class="filter-name">{{ rule.name || "(unnamed)" }}</span>
            <span class="filter-summary">
              {{ rule.conditions.length }} condition{{ rule.conditions.length !== 1 ? 's' : '' }},
              {{ rule.actions.length }} action{{ rule.actions.length !== 1 ? 's' : '' }}
              <span v-if="!rule.enabled" class="badge-disabled">disabled</span>
            </span>
          </div>
          <div class="filter-actions">
            <button class="btn-small" @click="editFilter(rule)">Edit</button>
            <button class="btn-small btn-danger" @click="deleteFilter(rule.id)">Delete</button>
          </div>
        </div>
        <div v-if="filtersStore.filters.length === 0" class="empty">
          No filters configured
        </div>
      </div>

      <!-- Edit form -->
      <div v-if="editingRule" class="filter-form">
        <h3>{{ editingRule.id ? 'Edit' : 'New' }} Filter</h3>

        <div class="form-group">
          <label>Name</label>
          <input v-model="editingRule.name" type="text" placeholder="Filter name" />
        </div>

        <div class="form-row">
          <div class="form-group">
            <label>Priority</label>
            <input v-model.number="editingRule.priority" type="number" />
          </div>
          <div class="form-group">
            <label>Match</label>
            <select v-model="editingRule.match_type">
              <option value="all">All conditions (AND)</option>
              <option value="any">Any condition (OR)</option>
            </select>
          </div>
          <div class="form-group">
            <label>&nbsp;</label>
            <label class="checkbox-label">
              <input v-model="editingRule.enabled" type="checkbox" />
              Enabled
            </label>
          </div>
          <div class="form-group">
            <label>&nbsp;</label>
            <label class="checkbox-label">
              <input v-model="editingRule.stop_processing" type="checkbox" />
              Stop after match
            </label>
          </div>
        </div>

        <!-- Conditions -->
        <div class="section-header">
          <span>Conditions</span>
          <button class="btn-small" @click="addCondition">+ Add</button>
        </div>
        <div
          v-for="(cond, i) in editingRule.conditions"
          :key="i"
          class="condition-row"
        >
          <select v-model="cond.field">
            <option v-for="(label, key) in fieldLabels" :key="key" :value="key">{{ label }}</option>
          </select>
          <select v-model="cond.op">
            <option v-for="(label, key) in opLabels" :key="key" :value="key">{{ label }}</option>
          </select>
          <input v-model="cond.value" type="text" placeholder="value" />
          <button class="btn-small btn-danger" @click="removeCondition(i)">&times;</button>
        </div>

        <!-- Actions -->
        <div class="section-header">
          <span>Actions</span>
          <button class="btn-small" @click="addAction">+ Add</button>
        </div>
        <div
          v-for="(action, i) in editingRule.actions"
          :key="i"
          class="action-row"
        >
          <select :value="getActionType(action)" @change="updateAction(i, ($event.target as HTMLSelectElement).value)">
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
            :value="getActionTarget(action)"
            @change="setActionTarget(i, ($event.target as HTMLSelectElement).value)"
          >
            <option value="" disabled>Select folder...</option>
            <option v-for="f in foldersStore.folders" :key="f.path" :value="f.path">
              {{ f.name }}
            </option>
          </select>
          <button class="btn-small btn-danger" @click="removeAction(i)">&times;</button>
        </div>

        <div class="form-actions">
          <button class="btn-primary" @click="saveFilter">Save</button>
          <button class="btn-secondary" @click="editingRule = null">Cancel</button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.filters-view {
  height: 100%;
  overflow-y: auto;
  padding: 24px;
}

.filters-content {
  max-width: 800px;
}

.filters-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin-bottom: 16px;
  gap: 12px;
}

.account-label {
  font-size: 12px;
  color: var(--color-text-muted);
  flex: 1;
}

.apply-section {
  display: flex;
  gap: 8px;
  align-items: center;
  margin-bottom: 16px;
  padding: 8px 12px;
  background: var(--color-bg-secondary);
  border-radius: 6px;
}

.folder-select {
  padding: 4px 8px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  min-width: 150px;
}

.apply-result {
  font-size: 12px;
  color: var(--color-success);
}

.filter-list {
  margin-bottom: 16px;
}

.filter-item {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 10px 12px;
  border: 1px solid var(--color-border);
  border-radius: 6px;
  margin-bottom: 6px;
}

.filter-item.disabled {
  opacity: 0.5;
}

.filter-info {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.filter-name {
  font-weight: 600;
}

.filter-summary {
  font-size: 11px;
  color: var(--color-text-muted);
}

.badge-disabled {
  background: var(--color-bg-active);
  padding: 1px 6px;
  border-radius: 4px;
  margin-left: 4px;
}

.filter-actions {
  display: flex;
  gap: 4px;
}

.filter-form {
  padding: 16px;
  border: 1px solid var(--color-border);
  border-radius: 8px;
  background: var(--color-bg-secondary);
}

.filter-form h3 {
  margin-bottom: 12px;
}

.form-group {
  margin-bottom: 10px;
}

.form-group label {
  display: block;
  margin-bottom: 4px;
  font-size: 12px;
  color: var(--color-text-secondary);
}

.form-group input[type="text"],
.form-group input[type="number"],
.form-group select {
  width: 100%;
  padding: 4px 8px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
}

.form-row {
  display: flex;
  gap: 12px;
}

.form-row .form-group {
  flex: 1;
}

.checkbox-label {
  display: flex !important;
  align-items: center;
  gap: 6px;
  font-size: 13px !important;
  color: var(--color-text) !important;
  cursor: pointer;
}

.section-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin: 12px 0 6px;
  font-size: 12px;
  font-weight: 600;
  color: var(--color-text-secondary);
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.condition-row,
.action-row {
  display: flex;
  gap: 6px;
  margin-bottom: 6px;
  align-items: center;
}

.condition-row select,
.action-row select {
  padding: 4px 6px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  font-size: 12px;
}

.condition-row input {
  flex: 1;
  padding: 4px 8px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  font-size: 12px;
}

.form-actions {
  display: flex;
  gap: 8px;
  margin-top: 16px;
}

.btn-primary {
  padding: 6px 16px;
  background: var(--color-accent);
  color: var(--color-bg);
  border-radius: 6px;
  font-weight: 600;
}

.btn-secondary {
  padding: 6px 16px;
  border: 1px solid var(--color-border);
  border-radius: 6px;
}

.btn-small {
  padding: 2px 8px;
  font-size: 11px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
}

.btn-danger {
  color: var(--color-danger);
}

.empty {
  color: var(--color-text-muted);
  padding: 12px;
}
</style>
