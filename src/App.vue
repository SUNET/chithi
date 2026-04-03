<script setup lang="ts">
import { onMounted } from "vue";
import Sidebar from "@/components/common/Sidebar.vue";
import MenuBar from "@/components/common/MenuBar.vue";
import StatusBar from "@/components/common/StatusBar.vue";
import { useActivityStore } from "@/stores/activity";
import { useUiStore } from "@/stores/ui";

const activityStore = useActivityStore();
const uiStore = useUiStore();

onMounted(() => {
  uiStore.initTheme();
  activityStore.initEventListeners();
});
</script>

<template>
  <div class="app-shell">
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
</style>
