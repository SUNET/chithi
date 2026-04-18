<script setup lang="ts">
import { onMounted, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import { useMessagesStore } from "@/stores/messages";
import MessageReader from "@/components/mail/MessageReader.vue";
import MobileIconButton from "@/components/mobile/MobileIconButton.vue";

const route = useRoute();
const router = useRouter();
const accountsStore = useAccountsStore();
const foldersStore = useFoldersStore();
const messagesStore = useMessagesStore();

async function loadFromRoute() {
  const rawId = route.params.id;
  const messageId = Array.isArray(rawId) ? rawId[0] : rawId;
  if (!messageId) return;
  try {
    if (accountsStore.accounts.length === 0) {
      await accountsStore.fetchAccounts();
    }
    if (foldersStore.folders.length === 0 && accountsStore.activeAccountId) {
      await foldersStore.fetchFolders();
    }
    await messagesStore.loadMessage(messageId);
  } catch (e) {
    console.error("MobileThreadView: loadMessage failed", e);
  }
}

onMounted(loadFromRoute);
watch(() => route.params.id, loadFromRoute);

function onClose() {
  if (window.history.length > 1) {
    router.back();
  } else {
    router.replace("/");
  }
}
</script>

<template>
  <div class="mobile-thread-view">
    <header class="thread-bar">
      <MobileIconButton aria-label="Back" @click="onClose">
        <svg
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-linecap="round"
          stroke-linejoin="round"
        >
          <polyline points="15 18 9 12 15 6" />
        </svg>
      </MobileIconButton>
      <div class="thread-title">
        {{ messagesStore.activeMessage?.subject ?? "Message" }}
      </div>
    </header>
    <div class="thread-body">
      <MessageReader :standalone="false" @close="onClose" />
    </div>
  </div>
</template>

<style scoped>
.mobile-thread-view {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-height: 0;
  background: var(--color-bg);
}

.thread-bar {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 6px 8px;
  padding-top: max(10px, env(safe-area-inset-top));
  border-bottom: 1px solid var(--color-divider, #e9e0cd);
  background: var(--color-bg);
}

.thread-title {
  flex: 1;
  min-width: 0;
  font-size: 15px;
  font-weight: 600;
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  padding-right: 8px;
}

.thread-body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
}
</style>
