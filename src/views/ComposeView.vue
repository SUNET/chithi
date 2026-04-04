<script setup lang="ts">
import { ref } from "vue";
import { useRouter, useRoute } from "vue-router";
import { useAccountsStore } from "@/stores/accounts";
import * as api from "@/lib/tauri";

const router = useRouter();
const route = useRoute();
const accountsStore = useAccountsStore();

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
    // Mark the original message as answered if this is a reply
    if (replyToMessageId) {
      api.setMessageFlags(accountId, [replyToMessageId], ["answered"], true)
        .catch((e) => console.error("Failed to set answered flag:", e));
    }
    router.push("/");
  } catch (e) {
    error.value = String(e);
  } finally {
    sending.value = false;
  }
}

function discard() {
  router.push("/");
}
</script>

<template>
  <div class="compose-view">
    <div class="compose-toolbar">
      <button class="btn-send" :disabled="sending" @click="send">
        {{ sending ? "Sending..." : "Send" }}
      </button>
      <button class="btn-discard" @click="discard">Discard</button>
    </div>

    <div v-if="error" class="compose-error">{{ error }}</div>

    <div class="compose-fields">
      <div class="field-row">
        <label>From:</label>
        <span class="from-display">{{ accountsStore.activeAccount()?.email ?? "No account" }}</span>
      </div>
      <div class="field-row">
        <label>To:</label>
        <input v-model="to" type="text" placeholder="recipient@example.com" />
        <button v-if="!showCcBcc" class="btn-cc" @click="showCcBcc = true">Cc/Bcc</button>
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
        <input v-model="subject" type="text" placeholder="Subject" />
      </div>
    </div>

    <textarea
      v-model="bodyText"
      class="compose-body"
      placeholder="Write your message..."
    ></textarea>
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
  background: var(--color-bg-secondary);
  flex-shrink: 0;
}

.btn-send {
  padding: 6px 20px;
  background: var(--color-accent);
  color: var(--color-bg);
  border-radius: 6px;
  font-weight: 600;
}

.btn-send:disabled {
  opacity: 0.5;
}

.btn-discard {
  padding: 6px 16px;
  border: 1px solid var(--color-border);
  border-radius: 6px;
  color: var(--color-text-secondary);
}

.btn-discard:hover {
  background: var(--color-bg-hover);
}

.compose-error {
  padding: 8px 16px;
  background: rgba(243, 139, 168, 0.1);
  color: var(--color-danger);
  font-size: 12px;
}

.compose-fields {
  padding: 8px 16px;
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
  width: 60px;
  flex-shrink: 0;
  font-size: 12px;
  color: var(--color-text-muted);
  text-align: right;
}

.field-row input {
  flex: 1;
  padding: 4px 8px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  font-size: 13px;
}

.field-row input:focus {
  outline: 1px solid var(--color-accent);
  border-color: var(--color-accent);
}

.from-display {
  font-size: 13px;
  color: var(--color-text-secondary);
}

.btn-cc {
  padding: 2px 8px;
  font-size: 11px;
  color: var(--color-accent);
  border: 1px solid var(--color-border);
  border-radius: 4px;
}

.compose-body {
  flex: 1;
  padding: 16px;
  border: none;
  background: var(--color-bg);
  color: var(--color-text);
  font-size: 13px;
  line-height: 1.6;
  resize: none;
}

.compose-body:focus {
  outline: none;
}
</style>
