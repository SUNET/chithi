<script setup lang="ts">
import { ref, watch } from "vue";
import { useMessagesStore } from "@/stores/messages";

defineProps<{
  standalone?: boolean;
}>();

const emit = defineEmits<{
  close: [];
}>();

const messagesStore = useMessagesStore();

// View mode: plain text by default
const showHtml = ref(false);

// Reset to plain text when switching messages
watch(
  () => messagesStore.activeMessageId,
  () => {
    showHtml.value = false;
  },
);

const hasHtml = () => !!messagesStore.activeMessage?.body_html;
const hasText = () => !!messagesStore.activeMessage?.body_text;

// Toast for "link copied" feedback
const toast = ref<string | null>(null);
let toastTimer: ReturnType<typeof setTimeout> | null = null;

function showToast(msg: string) {
  toast.value = msg;
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    toast.value = null;
  }, 2000);
}

function handleLinkClick(event: MouseEvent) {
  const target = (event.target as HTMLElement).closest("a");
  if (!target) return;

  event.preventDefault();
  event.stopPropagation();

  const href = target.getAttribute("href");
  if (!href) return;

  navigator.clipboard.writeText(href).then(() => {
    showToast("Link copied to clipboard");
  });
}

function handleContextMenu(event: MouseEvent) {
  event.preventDefault();
}
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
        <div class="headers-top">
          <h2 class="message-subject">{{ messagesStore.activeMessage.subject || "(no subject)" }}</h2>
          <div v-if="hasHtml()" class="view-toggle">
            <button
              class="toggle-btn"
              :class="{ active: !showHtml }"
              @click="showHtml = false"
            >Plain Text</button>
            <button
              class="toggle-btn"
              :class="{ active: showHtml }"
              @click="showHtml = true"
            >HTML</button>
          </div>
        </div>
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
        <!-- HTML view (user opted in) -->
        <div
          v-if="showHtml && hasHtml()"
          class="body-html-wrapper"
          @click="handleLinkClick"
          @contextmenu="handleContextMenu"
        >
          <div class="no-remote-notice">Remote content blocked</div>
          <div
            class="body-html"
            v-html="messagesStore.activeMessage.body_html"
          />
        </div>
        <!-- Plain text view (default) -->
        <pre
          v-else-if="hasText()"
          class="body-text"
          @contextmenu="handleContextMenu"
        >{{ messagesStore.activeMessage.body_text }}</pre>
        <!-- Fallback: only HTML available, no text part -->
        <div
          v-else-if="hasHtml()"
          class="body-html-wrapper"
          @click="handleLinkClick"
          @contextmenu="handleContextMenu"
        >
          <div class="no-remote-notice">Remote content blocked</div>
          <div
            class="body-html"
            v-html="messagesStore.activeMessage.body_html"
          />
        </div>
        <div v-else class="empty">No message content</div>
      </div>
    </div>

    <!-- Toast notification -->
    <div v-if="toast" class="toast">{{ toast }}</div>
  </div>
</template>

<style scoped>
.message-reader {
  height: 100%;
  overflow-y: auto;
  background: var(--color-bg);
  position: relative;
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

.headers-top {
  display: flex;
  justify-content: space-between;
  align-items: flex-start;
  gap: 12px;
  margin-bottom: 12px;
}

.message-subject {
  font-size: 18px;
  font-weight: 600;
  line-height: 1.3;
  flex: 1;
}

.view-toggle {
  display: flex;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  overflow: hidden;
  flex-shrink: 0;
}

.toggle-btn {
  padding: 3px 10px;
  font-size: 11px;
  color: var(--color-text-muted);
  border-right: 1px solid var(--color-border);
}

.toggle-btn:last-child {
  border-right: none;
}

.toggle-btn:hover {
  background: var(--color-bg-hover);
}

.toggle-btn.active {
  background: var(--color-bg-active);
  color: var(--color-text);
  font-weight: 600;
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

.no-remote-notice {
  font-size: 11px;
  color: var(--color-text-muted);
  background: #f0f0f0;
  padding: 4px 8px;
  border-radius: 3px;
  margin-bottom: 8px;
}

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

.body-html :deep(a) {
  color: #1a73e8;
  cursor: pointer;
}

.body-text {
  white-space: pre-wrap;
  font-family: var(--font-mono);
  font-size: 13px;
}

.toast {
  position: absolute;
  bottom: 16px;
  left: 50%;
  transform: translateX(-50%);
  background: var(--color-bg-active);
  color: var(--color-text);
  padding: 6px 16px;
  border-radius: 6px;
  font-size: 12px;
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3);
  pointer-events: none;
}
</style>
