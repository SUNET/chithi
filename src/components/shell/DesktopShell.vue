<script setup lang="ts">
import { computed } from "vue";
import { useRoute } from "vue-router";
import Sidebar from "@/components/common/Sidebar.vue";
import MenuBar from "@/components/common/MenuBar.vue";
import StatusBar from "@/components/common/StatusBar.vue";
import OperationsPanel from "@/components/common/OperationsPanel.vue";

const route = useRoute();

// Compose and reader windows are standalone — hide the main app chrome
const isStandaloneWindow = computed(
  () => route.name === "compose" || route.name === "reader",
);
</script>

<template>
  <div v-if="isStandaloneWindow" class="standalone-shell">
    <router-view />
  </div>

  <div v-else class="app-shell">
    <Sidebar />
    <div class="app-main">
      <MenuBar />
      <main class="app-content">
        <router-view v-slot="{ Component }">
          <!-- Only CalendarView is kept alive — its WeekView subtree is
               the heavy one (see #72) and cold-mount is ~400ms. Other
               views (ContactsView, FiltersView, SettingsView, MailView)
               rely on onMounted/onUnmounted for listener and interval
               cleanup, so caching them would leak background work. -->
          <KeepAlive include="CalendarView">
            <component :is="Component" />
          </KeepAlive>
        </router-view>
      </main>
      <OperationsPanel />
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

.standalone-shell {
  height: 100vh;
  width: 100vw;
  overflow: hidden;
}
</style>
