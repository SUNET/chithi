<script setup lang="ts">
import { onMounted } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import { useMessagesStore } from "@/stores/messages";
import { useUiStore } from "@/stores/ui";
import MessageReader from "@/components/mail/MessageReader.vue";

const accountsStore = useAccountsStore();
const foldersStore = useFoldersStore();
const messagesStore = useMessagesStore();
const uiStore = useUiStore();

// Close the window when MessageReader emits close
function closeWindow() {
  getCurrentWindow().close();
}

onMounted(async () => {
  // Reader window has its own Vue instance — stores are empty. Populate
  // enough state for MessageReader to work (activeAccount, folders, and
  // the message body).
  uiStore.initTheme();

  const params = new URLSearchParams(window.location.search);
  const messageId = params.get("messageId");
  const accountId = params.get("accountId");
  if (!messageId || !accountId) return;

  try {
    await accountsStore.fetchAccounts();
    accountsStore.setActiveAccount(accountId);
    await foldersStore.fetchFolders();
    await messagesStore.loadMessage(messageId);
  } catch (e) {
    console.error("Failed to initialize reader window:", e);
  }
});
</script>

<template>
  <div class="reader-view">
    <MessageReader :standalone="true" @close="closeWindow" />
  </div>
</template>

<style scoped>
.reader-view {
  height: 100vh;
  width: 100vw;
  overflow: auto;
}
</style>
