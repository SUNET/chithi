<script setup lang="ts">
import { ref } from "vue";

const props = defineProps<{
  modelValue: string[];
}>();

const emit = defineEmits<{
  "update:modelValue": [value: string[]];
}>();

const newEmail = ref("");

function addAttendee() {
  const email = newEmail.value.trim();
  if (email && email.includes("@") && !props.modelValue.includes(email)) {
    emit("update:modelValue", [...props.modelValue, email]);
    newEmail.value = "";
  }
}

function removeAttendee(email: string) {
  emit("update:modelValue", props.modelValue.filter((e) => e !== email));
}

function onKeydown(event: KeyboardEvent) {
  if (event.key === "Enter" || event.key === ",") {
    event.preventDefault();
    addAttendee();
  }
}
</script>

<template>
  <div class="attendee-editor">
    <div v-if="modelValue.length > 0" class="attendee-list">
      <div v-for="email in modelValue" :key="email" class="attendee-chip">
        <span class="attendee-email">{{ email }}</span>
        <button class="remove-btn" @click="removeAttendee(email)">&times;</button>
      </div>
    </div>
    <div class="add-row">
      <input
        v-model="newEmail"
        type="email"
        placeholder="Add attendee email..."
        @keydown="onKeydown"
      />
      <button class="add-btn" @click="addAttendee">Add</button>
    </div>
  </div>
</template>

<style scoped>
.attendee-editor {
  padding: 4px 0;
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
</style>
