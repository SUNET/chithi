<script setup lang="ts">
import { computed } from "vue";
import { useRoute } from "vue-router";
import { storeToRefs } from "pinia";
import MobileTabBar from "@/components/mobile/MobileTabBar.vue";
import FolderDrawer from "@/components/mobile/FolderDrawer.vue";
import ComposeSheet from "@/components/mobile/ComposeSheet.vue";
import { useUiStore } from "@/stores/ui";

const route = useRoute();
const uiStore = useUiStore();
const { composeOpen } = storeToRefs(uiStore);

// Hide tab bar inside full-immersion routes. Reader is shown as a pushed
// screen with its own back chevron; Compose is a sheet — hide tab bar
// while it's open too.
const hideTabBarRoutes = new Set(["reader", "compose", "onboarding"]);

const showTabBar = computed(() => {
  if (composeOpen.value) return false;
  if (hideTabBarRoutes.has(String(route.name))) return false;
  // The mobile thread detail route uses `/mail/thread/:id` (name: "mobile-reader")
  if (route.path.startsWith("/mail/thread/")) return false;
  return true;
});
</script>

<template>
  <div class="mobile-shell">
    <div class="mobile-shell-content">
      <router-view v-slot="{ Component }">
        <KeepAlive include="CalendarView">
          <component :is="Component" />
        </KeepAlive>
      </router-view>
    </div>
    <MobileTabBar v-if="showTabBar" />
    <FolderDrawer />
    <ComposeSheet v-if="composeOpen" />
  </div>
</template>

<style scoped>
.mobile-shell {
  position: fixed;
  inset: 0;
  background: var(--color-bg);
  color: var(--color-text);
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

.mobile-shell-content {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}
</style>
