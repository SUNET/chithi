<script setup lang="ts">
import { ref } from "vue";
import { useRoute } from "vue-router";
import { useAccountsStore } from "@/stores/accounts";
import { getCurrentWindow } from "@tauri-apps/api/window";
import * as api from "@/lib/tauri";

const route = useRoute();
const accountsStore = useAccountsStore();
const currentWindow = getCurrentWindow();

// Prefill from query params (reply/reply-all/forward)
const replyToMessageId = (route.query.replyTo as string) || "";
const to = ref((route.query.to as string) || "");
const cc = ref((route.query.cc as string) || "");
const bcc = ref("");
const subject = ref((route.query.subject as string) || "");
const bodyText = ref((route.query.body as string) || "");
const sending = ref(false);
const error = ref<string | null>(null);
const showCcBcc = ref(!!cc.value);

function parseAddresses(input: string): string[] {
  return input
    .split(/[,;]/)
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

async function send() {
  const accountId = accountsStore.activeAccountId;
  if (!accountId) {
    error.value = "No account selected";
    return;
  }

  const toAddrs = parseAddresses(to.value);
  if (toAddrs.length === 0) {
    error.value = "At least one recipient is required";
    return;
  }

  sending.value = true;
  error.value = null;

  try {
    await api.sendMessage(accountId, {
      to: toAddrs,
      cc: parseAddresses(cc.value),
      bcc: parseAddresses(bcc.value),
      subject: subject.value,
      body_text: bodyText.value,
      body_html: null,
    });
    if (replyToMessageId) {
      api.setMessageFlags(accountId, [replyToMessageId], ["answered"], true)
        .catch((e) => console.error("Failed to set answered flag:", e));
    }
    currentWindow.close();
  } catch (e) {
    error.value = String(e);
  } finally {
    sending.value = false;
  }
}

function discard() {
  currentWindow.close();
}
</script>

<template>
  <div class="compose-view">
    <div class="compose-toolbar">
      <button class="btn-send" :disabled="sending" @click="send">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <line x1="22" y1="2" x2="11" y2="13" /><polygon points="22 2 15 22 11 13 2 9 22 2" />
        </svg>
        {{ sending ? "Sending..." : "Send" }}
      </button>
      <button class="btn-discard" @click="discard">
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" />
        </svg>
        Discard
      </button>
    </div>

    <div v-if="error" class="compose-error">{{ error }}</div>

    <div class="compose-content">
      <div class="compose-fields">
        <div class="field-row">
          <label>To:</label>
          <input v-model="to" type="text" placeholder="recipient@example.com" />
          <button v-if="!showCcBcc" class="btn-cc" @click="showCcBcc = true">Cc Bcc</button>
        </div>
        <div v-if="showCcBcc" class="field-row">
          <label>Cc:</label>
          <input v-model="cc" type="text" placeholder="cc@example.com" />
        </div>
        <div v-if="showCcBcc" class="field-row">
          <label>Bcc:</label>
          <input v-model="bcc" type="text" placeholder="bcc@example.com" />
        </div>
        <div class="field-row">
          <label>Subject:</label>
          <input v-model="subject" type="text" placeholder="Email subject" />
        </div>
      </div>

      <textarea
        v-model="bodyText"
        class="compose-body"
        placeholder="Write your message..."
      ></textarea>
    </div>
  </div>
</template>

<style scoped>
.compose-view {
  display: flex;
  flex-direction: column;
  height: 100%;
  background: var(--color-bg);
}

.compose-toolbar {
  display: flex;
  gap: 8px;
  padding: 8px 16px;
  border-bottom: 1px solid var(--color-border);
  flex-shrink: 0;
}

.btn-send {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 6px 16px;
  background: var(--color-accent);
  color: white;
  border-radius: 18px;
  font-weight: 500;
  font-size: 13px;
  transition: background 0.12s;
}

.btn-send:hover {
  background: var(--color-accent-hover);
}

.btn-send:disabled {
  opacity: 0.5;
}

.btn-discard {
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 6px 14px;
  border: 1px solid var(--color-border);
  border-radius: 18px;
  font-size: 13px;
  color: var(--color-text-secondary);
  transition: all 0.12s;
}

.btn-discard:hover {
  background: var(--color-bg-hover);
}

.compose-error {
  padding: 8px 16px;
  background: rgba(220, 53, 69, 0.06);
  color: var(--color-danger);
  font-size: 12px;
}

.compose-content {
  flex: 1;
  display: flex;
  flex-direction: column;
  max-width: 700px;
  margin: 0 auto;
  width: 100%;
  padding: 0 16px;
}

.compose-fields {
  padding: 12px 0;
  border-bottom: 1px solid var(--color-border);
  flex-shrink: 0;
}

.field-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 4px 0;
}

.field-row label {
  width: 50px;
  flex-shrink: 0;
  font-size: 13px;
  color: var(--color-text-muted);
  text-align: right;
}

.field-row input {
  flex: 1;
  padding: 6px 0;
  border: none;
  border-bottom: 1px solid var(--color-border);
  background: transparent;
  font-size: 13px;
}

.field-row input:focus {
  outline: none;
  border-bottom-color: var(--color-accent);
}

.btn-cc {
  padding: 2px 8px;
  font-size: 11px;
  color: var(--color-text-muted);
  white-space: nowrap;
}

.btn-cc:hover {
  color: var(--color-text);
}

.compose-body {
  flex: 1;
  padding: 16px 0;
  border: none;
  background: var(--color-bg-secondary);
  color: var(--color-text);
  font-size: 13px;
  line-height: 1.6;
  resize: none;
  border-radius: 6px;
  margin-top: 12px;
  padding: 16px;
}

.compose-body:focus {
  outline: none;
}
</style>
