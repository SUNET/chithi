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

const DEFAULT_ZOOM_LEVEL = 1;
const MIN_ZOOM_LEVEL = 0.5;
const MAX_ZOOM_LEVEL = 2;
const ZOOM_STEP = 0.1;

const accountsStore = useAccountsStore();
const uiStore = useUiStore();
const platformStore = usePlatformStore();
const pgpPrompts = usePgpPromptsStore();

const { isMobile } = storeToRefs(platformStore);
const { currentPrompt } = storeToRefs(pgpPrompts);

let zoomLevel = DEFAULT_ZOOM_LEVEL;

function handleZoomShortcut(event: KeyboardEvent) {
  if (!(event.ctrlKey || event.metaKey)) return;

  let nextZoomLevel: number;
  switch (event.key) {
    case "=":
    case "+":
      nextZoomLevel = zoomLevel + ZOOM_STEP;
      break;
    case "-":
      nextZoomLevel = zoomLevel - ZOOM_STEP;
      break;
    case "0":
      nextZoomLevel = DEFAULT_ZOOM_LEVEL;
      break;
    default:
      return;
  }

  event.preventDefault();
  zoomLevel = Math.min(
    MAX_ZOOM_LEVEL,
    Math.max(MIN_ZOOM_LEVEL, nextZoomLevel),
  );
  document.documentElement.style.zoom = String(zoomLevel);
}

onMounted(async () => {
  uiStore.initTheme();
  uiStore.initDecorations();

  // WebKitGTK ignores zoomHotkeysEnabled, so handle zoom shortcuts here.
  window.addEventListener("keydown", handleZoomShortcut);

  await uiStore.initTimezone();
  await accountsStore.fetchAccounts();
  await pgpPrompts.start();
});

onUnmounted(() => {
  window.removeEventListener("keydown", handleZoomShortcut);
  pgpPrompts.stop();
});
</script>

<template>
  <div :data-layout="isMobile ? 'mobile' : 'desktop'" class="chrome-root">
    <DesktopShell v-if="!isMobile" />
    <MobileShell v-else />
  </div>
  <ToastContainer />
  <PassphraseDialog
    v-if="currentPrompt?.kind === 'passphrase'"
    :prompt="currentPrompt"
  />
  <PinDialog v-if="currentPrompt?.kind === 'pin'" :prompt="currentPrompt" />
</template>

<style scoped>
.chrome-root {
  height: 100vh;
  width: 100vw;
  overflow: hidden;
}
</style>
