<script setup lang="ts">
/// First step of "Add Account": pick a type. Replaces the cramped
/// in-modal tab row with a dialog that lists every supported
/// account type (currently ten — Gmail / O365 / Fastmail / IMAP /
/// JMAP / CalDAV / CardDAV / Talk / Matrix / Zoom) as cards, and
/// on pick the parent opens the account form pre-set to that type.
/// Edit-existing skips this step. (#148 cleanup)
import ModalShell from "@/components/common/ModalShell.vue";
import {
  ADD_ACCOUNT_TYPES,
  accountTypeDescription,
  accountTypeLabelLong,
  type AccountType,
} from "@/lib/account-types";

defineProps<{ open: boolean }>();
const emit = defineEmits<{ pick: [type: AccountType]; cancel: [] }>();
</script>

<template>
  <ModalShell
    :open="open"
    title="Add Account"
    modal-class="modal-picker"
    data-testid="account-type-picker"
    @close="emit('cancel')"
  >
    <p class="picker-help">Pick the kind of account you want to add. You can add more later.</p>
    <div class="picker-grid">
      <button
        v-for="t in ADD_ACCOUNT_TYPES"
        :key="t"
        class="picker-card"
        :data-testid="`picker-${t}`"
        @click="emit('pick', t)"
      >
        <span class="picker-card-title">{{ accountTypeLabelLong(t) }}</span>
        <span class="picker-card-desc">{{ accountTypeDescription(t) }}</span>
      </button>
    </div>
    <template #footer>
      <button class="btn-secondary" @click="emit('cancel')">Cancel</button>
    </template>
  </ModalShell>
</template>

<style scoped>
.picker-help {
  margin: 0 0 12px;
  font-size: 13px;
  color: var(--color-text-muted);
}
.picker-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 8px;
}
.picker-card {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 12px 14px;
  text-align: left;
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: 8px;
  cursor: pointer;
  transition: background 0.1s, border-color 0.1s;
}
.picker-card:hover {
  background: var(--color-bg-hover);
  border-color: var(--color-accent);
}
.picker-card-title {
  font-size: 14px;
  font-weight: 600;
  color: var(--color-text);
}
.picker-card-desc {
  font-size: 12px;
  color: var(--color-text-muted);
}
.btn-secondary {
  height: 40px;
  padding: 0 20px;
  background: var(--color-bg-tertiary);
  border-radius: 4px;
  font-size: 16px;
  font-weight: 500;
  color: var(--color-text);
  transition: background 0.12s;
}
.btn-secondary:hover {
  background: var(--color-border);
}
</style>
