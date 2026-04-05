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
    // ignore
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
    <div class="invite-top">
      <div class="invite-icon-box">
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="#155dfc" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <rect x="3" y="4" width="18" height="18" rx="2" />
          <path d="M16 2v4M8 2v4M3 10h18" />
        </svg>
      </div>
      <div class="invite-details">
        <h4 class="invite-title">{{ invite.summary || "(No title)" }}</h4>
        <div class="invite-meta">
          <span>{{ formatDateTime(invite.dtstart) }}</span>
          <span v-if="!invite.all_day"> - {{ formatDateTime(invite.dtend) }}</span>
        </div>
        <div v-if="invite.location" class="invite-meta">{{ invite.location }}</div>
        <div v-if="invite.organizer_email" class="invite-meta">
          Organized by {{ invite.organizer_name || invite.organizer_email }}
        </div>
      </div>
    </div>

    <div v-if="invite.description" class="invite-description">
      {{ invite.description }}
    </div>

    <div v-if="invite.attendees.length > 0" class="invite-attendees">
      <span class="attendees-label">Attendees:</span>
      {{ invite.attendees.map(a => a.name || a.email).join(", ") }}
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
      >
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round">
          <polyline points="20 6 9 17 4 12" />
        </svg>
        Accept
      </button>
      <button
        class="btn-maybe"
        :disabled="responding"
        @click="respond('tentative')"
      >
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <circle cx="12" cy="12" r="10" /><line x1="12" y1="8" x2="12" y2="12" /><line x1="12" y1="16" x2="12.01" y2="16" />
        </svg>
        Maybe
      </button>
      <button
        class="btn-decline"
        :disabled="responding"
        @click="respond('declined')"
      >
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" />
        </svg>
        Decline
      </button>
    </div>
  </div>
</template>

<style scoped>
.invite-card {
  background: var(--color-bg-secondary);
  border: 0.8px solid var(--color-border);
  border-radius: 10px;
  padding: 16px;
  margin-bottom: 16px;
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.invite-top {
  display: flex;
  gap: 12px;
  align-items: flex-start;
}

.invite-icon-box {
  width: 40px;
  height: 40px;
  border-radius: 4px;
  background: rgba(21, 93, 252, 0.2);
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
}

.invite-details {
  flex: 1;
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.invite-title {
  font-size: 18px;
  font-weight: 600;
  line-height: 27px;
  color: var(--color-text);
}

.invite-meta {
  font-size: 14px;
  line-height: 20px;
  color: var(--color-text-secondary);
}

.invite-description {
  font-size: 14px;
  line-height: 20px;
  color: #404040;
  padding-left: 52px;
}

.invite-attendees {
  font-size: 12px;
  line-height: 16px;
  color: var(--color-text-secondary);
  padding-left: 52px;
}

.attendees-label {
  font-weight: 500;
}

.invite-error {
  padding: 8px 12px;
  background: rgba(251, 44, 54, 0.06);
  color: var(--color-danger-text);
  font-size: 12px;
  border-radius: 4px;
}

.invite-responded {
  padding: 8px 12px;
  background: rgba(0, 166, 62, 0.06);
  color: var(--color-success);
  font-size: 14px;
  font-weight: 500;
  padding-left: 52px;
}

.invite-actions {
  display: flex;
  gap: 8px;
  padding-left: 52px;
}

.btn-accept,
.btn-maybe,
.btn-decline {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 5px 16px;
  border-radius: 4px;
  font-size: 14px;
  font-weight: 500;
  line-height: 20px;
  transition: opacity 0.12s;
}

.btn-accept {
  background: #00a63e;
  color: white;
}

.btn-maybe {
  background: #e17100;
  color: white;
}

.btn-decline {
  background: transparent;
  border: 0.8px solid #fb2c36;
  color: #e7000b;
}

.btn-accept:disabled,
.btn-maybe:disabled,
.btn-decline:disabled {
  opacity: 0.5;
}
</style>
