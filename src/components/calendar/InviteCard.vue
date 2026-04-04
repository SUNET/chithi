<script setup lang="ts">
import { ref, onMounted } from "vue";
import type { ParsedInvite } from "@/lib/types";
import { useAccountsStore } from "@/stores/accounts";
import * as api from "@/lib/tauri";

const props = defineProps<{
  invite: ParsedInvite;
  messageId: string;
}>();

const accountsStore = useAccountsStore();
const responding = ref(false);
const responded = ref<string | null>(null);
const error = ref<string | null>(null);

onMounted(async () => {
  const accountId = accountsStore.activeAccountId;
  if (!accountId || !props.invite.uid) return;
  try {
    const status = await api.getInviteStatus(accountId, props.invite.uid);
    if (status) {
      responded.value = status;
    }
  } catch {
    // ignore — just show buttons if lookup fails
  }
});

function formatDateTime(iso: string): string {
  return new Date(iso).toLocaleString(undefined, {
    weekday: "long",
    month: "long",
    day: "numeric",
    year: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

async function respond(response: string) {
  const accountId = accountsStore.activeAccountId;
  if (!accountId) return;

  responding.value = true;
  error.value = null;
  try {
    await api.respondToInvite(accountId, props.messageId, props.invite.uid, response);
    responded.value = response;
  } catch (e) {
    error.value = String(e);
  } finally {
    responding.value = false;
  }
}
</script>

<template>
  <div class="invite-card">
    <div class="invite-header">
      <span class="invite-icon">&#x1F4C5;</span>
      <span class="invite-label">Meeting Invite</span>
    </div>

    <div class="invite-body">
      <h4 class="invite-title">{{ invite.summary || "(No title)" }}</h4>

      <div class="invite-detail">
        <span class="detail-label">When:</span>
        <span>{{ formatDateTime(invite.dtstart) }}</span>
        <span v-if="!invite.all_day"> — {{ formatDateTime(invite.dtend) }}</span>
        <span v-else> (all day)</span>
      </div>

      <div v-if="invite.location" class="invite-detail">
        <span class="detail-label">Where:</span>
        <span>{{ invite.location }}</span>
      </div>

      <div v-if="invite.organizer_email" class="invite-detail">
        <span class="detail-label">Organizer:</span>
        <span>{{ invite.organizer_name || invite.organizer_email }}</span>
      </div>

      <div v-if="invite.attendees.length > 0" class="invite-detail">
        <span class="detail-label">Attendees:</span>
        <span>{{ invite.attendees.map(a => a.name || a.email).join(", ") }}</span>
      </div>

      <div v-if="invite.recurrence_rule" class="invite-detail">
        <span class="detail-label">Recurrence:</span>
        <span class="detail-secondary">{{ invite.recurrence_rule }}</span>
      </div>

      <div v-if="invite.description" class="invite-description">
        {{ invite.description }}
      </div>
    </div>

    <div v-if="error" class="invite-error">{{ error }}</div>

    <div v-if="responded" class="invite-responded">
      You {{ responded === "accepted" ? "accepted" : responded === "tentative" ? "tentatively accepted" : "declined" }} this invite.
    </div>

    <div v-else class="invite-actions">
      <button
        class="btn-accept"
        :disabled="responding"
        @click="respond('accepted')"
      >Accept</button>
      <button
        class="btn-maybe"
        :disabled="responding"
        @click="respond('tentative')"
      >Maybe</button>
      <button
        class="btn-decline"
        :disabled="responding"
        @click="respond('declined')"
      >Decline</button>
    </div>
  </div>
</template>

<style scoped>
.invite-card {
  border: 2px solid var(--color-accent);
  border-radius: 8px;
  margin-bottom: 16px;
  overflow: hidden;
}

.invite-header {
  background: var(--color-accent);
  color: var(--color-bg);
  padding: 8px 12px;
  font-size: 12px;
  font-weight: 600;
  display: flex;
  align-items: center;
  gap: 6px;
}

.invite-body {
  padding: 12px;
}

.invite-title {
  font-size: 16px;
  font-weight: 600;
  margin-bottom: 10px;
}

.invite-detail {
  font-size: 13px;
  margin-bottom: 6px;
}

.detail-label {
  color: var(--color-text-muted);
  margin-right: 4px;
}

.detail-secondary {
  font-size: 12px;
  color: var(--color-text-muted);
  font-family: var(--font-mono);
}

.invite-description {
  margin-top: 8px;
  padding-top: 8px;
  border-top: 1px solid var(--color-border);
  font-size: 13px;
  color: var(--color-text-secondary);
  white-space: pre-wrap;
}

.invite-error {
  padding: 8px 12px;
  background: rgba(243, 139, 168, 0.1);
  color: var(--color-danger);
  font-size: 12px;
}

.invite-responded {
  padding: 8px 12px;
  background: rgba(166, 227, 161, 0.1);
  color: var(--color-success);
  font-size: 12px;
  font-weight: 500;
}

.invite-actions {
  display: flex;
  gap: 8px;
  padding: 8px 12px;
  border-top: 1px solid var(--color-border);
}

.btn-accept {
  padding: 6px 16px;
  background: var(--color-success);
  color: var(--color-bg);
  border-radius: 6px;
  font-weight: 600;
  font-size: 12px;
}

.btn-maybe {
  padding: 6px 16px;
  background: var(--color-warning);
  color: var(--color-bg);
  border-radius: 6px;
  font-weight: 600;
  font-size: 12px;
}

.btn-decline {
  padding: 6px 16px;
  border: 1px solid var(--color-danger);
  color: var(--color-danger);
  border-radius: 6px;
  font-weight: 600;
  font-size: 12px;
}

.btn-accept:disabled,
.btn-maybe:disabled,
.btn-decline:disabled {
  opacity: 0.5;
}
</style>
