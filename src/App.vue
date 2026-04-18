<script setup lang="ts">
import { onMounted } from "vue";
import { storeToRefs } from "pinia";
import DesktopShell from "@/components/shell/DesktopShell.vue";
import MobileShell from "@/components/shell/MobileShell.vue";
import ToastContainer from "@/components/common/ToastContainer.vue";
import { useActivityStore } from "@/stores/activity";
import { useAccountsStore } from "@/stores/accounts";
import { useUiStore } from "@/stores/ui";
import { usePlatformStore } from "@/stores/platform";

const activityStore = useActivityStore();
const accountsStore = useAccountsStore();
const uiStore = useUiStore();
const platformStore = usePlatformStore();

const { isMobile } = storeToRefs(platformStore);

onMounted(async () => {
  uiStore.initTheme();
  uiStore.initDecorations();
  await uiStore.initTimezone();
  activityStore.initEventListeners();
  await accountsStore.fetchAccounts();

  // Zoom with Ctrl+/Ctrl- (WebKitGTK doesn't support zoomHotkeysEnabled)
  let zoomLevel = 1.0;
  window.addEventListener("keydown", (e) => {
    if (!(e.ctrlKey || e.metaKey)) return;
    if (e.key === "=" || e.key === "+") {
      e.preventDefault();
      zoomLevel = Math.min(zoomLevel + 0.1, 2.0);
      document.documentElement.style.zoom = String(zoomLevel);
    } else if (e.key === "-") {
      e.preventDefault();
      zoomLevel = Math.max(zoomLevel - 0.1, 0.5);
      document.documentElement.style.zoom = String(zoomLevel);
    } else if (e.key === "0") {
      e.preventDefault();
      zoomLevel = 1.0;
      document.documentElement.style.zoom = "1";
    }
  });
});
</script>

<template>
  <div :data-layout="isMobile ? 'mobile' : 'desktop'" class="chrome-root">
    <DesktopShell v-if="!isMobile" />
    <MobileShell v-else />
  </div>
  <ToastContainer />
</template>

<style scoped>
.chrome-root {
  height: 100vh;
  width: 100vw;
  overflow: hidden;
}
</style>
