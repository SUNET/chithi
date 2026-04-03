<script setup lang="ts">
import { useMessagesStore } from "@/stores/messages";

defineProps<{
  standalone?: boolean;
}>();

const emit = defineEmits<{
  close: [];
}>();

const messagesStore = useMessagesStore();
</script>

<template>
  <div class="message-reader">
    <div v-if="standalone" class="reader-toolbar">
      <button class="close-btn" title="Close" @click="emit('close')">&times;</button>
    </div>
    <div v-if="messagesStore.loadingBody" class="loading">Loading message...</div>
    <div v-else-if="!messagesStore.activeMessage" class="empty">
      Select a message to read
    </div>
    <div v-else class="message-content">
      <div class="message-headers">
        <h2 class="message-subject">{{ messagesStore.activeMessage.subject || "(no subject)" }}</h2>
        <div class="header-row">
          <span class="header-label">From:</span>
          <span class="header-value">
            {{ messagesStore.activeMessage.from.name }}
            &lt;{{ messagesStore.activeMessage.from.email }}&gt;
          </span>
        </div>
        <div class="header-row">
          <span class="header-label">To:</span>
          <span class="header-value">
            <span v-for="(addr, i) in messagesStore.activeMessage.to" :key="i">
              {{ addr.name || addr.email }}{{ i < messagesStore.activeMessage.to.length - 1 ? ", " : "" }}
            </span>
          </span>
        </div>
        <div v-if="messagesStore.activeMessage.cc.length" class="header-row">
          <span class="header-label">Cc:</span>
          <span class="header-value">
            <span v-for="(addr, i) in messagesStore.activeMessage.cc" :key="i">
              {{ addr.name || addr.email }}{{ i < messagesStore.activeMessage.cc.length - 1 ? ", " : "" }}
            </span>
          </span>
        </div>
        <div class="header-row">
          <span class="header-label">Date:</span>
          <span class="header-value">{{ new Date(messagesStore.activeMessage.date).toLocaleString() }}</span>
        </div>
      </div>
      <div class="message-body">
        <!-- HTML emails rendered on white background for readability -->
        <div
          v-if="messagesStore.activeMessage.body_html"
          class="body-html-wrapper"
        >
          <div
            class="body-html"
            v-html="messagesStore.activeMessage.body_html"
          />
        </div>
        <pre v-else class="body-text">{{ messagesStore.activeMessage.body_text }}</pre>
      </div>
    </div>
  </div>
</template>

<style scoped>
.message-reader {
  height: 100%;
  overflow-y: auto;
  background: var(--color-bg);
}

.reader-toolbar {
  display: flex;
  justify-content: flex-end;
  padding: 4px 8px;
  border-bottom: 1px solid var(--color-border);
  background: var(--color-bg-secondary);
}

.close-btn {
  width: 24px;
  height: 24px;
  border-radius: 4px;
  font-size: 18px;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--color-text-muted);
}

.close-btn:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.loading,
.empty {
  padding: 24px;
  text-align: center;
  color: var(--color-text-muted);
}

.message-content {
  padding: 16px;
}

.message-headers {
  border-bottom: 1px solid var(--color-border);
  padding-bottom: 12px;
  margin-bottom: 16px;
}

.message-subject {
  font-size: 18px;
  font-weight: 600;
  margin-bottom: 12px;
  line-height: 1.3;
}

.header-row {
  display: flex;
  gap: 8px;
  margin-bottom: 4px;
  font-size: 13px;
}

.header-label {
  color: var(--color-text-muted);
  flex-shrink: 0;
  min-width: 40px;
}

.header-value {
  color: var(--color-text-secondary);
}

.message-body {
  line-height: 1.5;
}

/* HTML emails are designed for light backgrounds — always render on white */
.body-html-wrapper {
  background: var(--color-email-body-bg);
  color: var(--color-email-body-text);
  border-radius: 6px;
  padding: 16px;
  border: 1px solid var(--color-border);
}

.body-html {
  word-wrap: break-word;
  overflow-wrap: break-word;
}

.body-html :deep(img) {
  max-width: 100%;
  height: auto;
}

.body-html :deep(a) {
  color: #1a73e8;
}

.body-text {
  white-space: pre-wrap;
  font-family: var(--font-mono);
  font-size: 13px;
}
</style>
