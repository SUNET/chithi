<script setup lang="ts">
import { onMounted, computed } from "vue";
import { useRoute } from "vue-router";
import Sidebar from "@/components/common/Sidebar.vue";
import MenuBar from "@/components/common/MenuBar.vue";
import StatusBar from "@/components/common/StatusBar.vue";
import { useActivityStore } from "@/stores/activity";
import { useAccountsStore } from "@/stores/accounts";
import { useUiStore } from "@/stores/ui";

const route = useRoute();
const activityStore = useActivityStore();
const accountsStore = useAccountsStore();
const uiStore = useUiStore();

// Compose opens in a separate window — hide app chrome
const isComposeWindow = computed(() => route.name === "compose");

onMounted(async () => {
  uiStore.initTheme();
  activityStore.initEventListeners();
  // Load accounts globally so all views (Calendar, Contacts, etc.) have them
  await accountsStore.fetchAccounts();
});
</script>

<template>
  <!-- Compose window: standalone, no sidebar/menubar/statusbar -->
  <div v-if="isComposeWindow" class="compose-shell">
    <router-view />
  </div>

  <!-- Main app shell -->
  <div v-else class="app-shell">
    <Sidebar />
    <div class="app-main">
      <MenuBar />
      <main class="app-content">
        <router-view />
      </main>
      <StatusBar />
    </div>
  </div>
</template>

<style scoped>
.app-shell {
  display: flex;
  height: 100vh;
  width: 100vw;
  overflow: hidden;
}

.app-main {
  flex: 1;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

.app-content {
  flex: 1;
  overflow: hidden;
}

.compose-shell {
  height: 100vh;
  width: 100vw;
  overflow: hidden;
}
</style>
