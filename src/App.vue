<script setup lang="ts">
import { onMounted, onUnmounted } from "vue";
import { storeToRefs } from "pinia";
import DesktopShell from "@/components/shell/DesktopShell.vue";
import MobileShell from "@/components/shell/MobileShell.vue";
import ToastContainer from "@/components/common/ToastContainer.vue";
import PassphraseDialog from "@/components/pgp/PassphraseDialog.vue";
import PinDialog from "@/components/pgp/PinDialog.vue";
import { useAccountsStore } from "@/stores/accounts";
import { useUiStore } from "@/stores/ui";
import { usePlatformStore } from "@/stores/platform";
import { usePgpPromptsStore } from "@/stores/pgp-prompts";

const accountsStore = useAccountsStore();
const uiStore = useUiStore();
const platformStore = usePlatformStore();
const pgpPrompts = usePgpPromptsStore();

const { isMobile } = storeToRefs(platformStore);
const { currentPrompt } = storeToRefs(pgpPrompts);

onMounted(async () => {
  uiStore.initTheme();
  uiStore.initDecorations();
  await uiStore.initTimezone();
  await accountsStore.fetchAccounts();
  // Subscribe globally so any view that triggers a sign/decrypt can
  // receive its prompt.
  await pgpPrompts.start();

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

onUnmounted(() => {
  pgpPrompts.stop();
});
</script>

<template>
  <div :data-layout="isMobile ? 'mobile' : 'desktop'" class="chrome-root">
    <DesktopShell v-if="!isMobile" />
    <MobileShell v-else />
  </div>
  <ToastContainer />
  <!-- Global PGP secret prompts. The head of the queue is rendered;
       when it resolves the next prompt (if any) takes its place. -->
  <PassphraseDialog
    v-if="currentPrompt && currentPrompt.kind === 'passphrase'"
    :prompt="currentPrompt"
  />
  <PinDialog
    v-if="currentPrompt && currentPrompt.kind === 'pin'"
    :prompt="currentPrompt"
  />
</template>

<style scoped>
.chrome-root {
  height: 100vh;
  width: 100vw;
  overflow: hidden;
}
</style>
