<script setup lang="ts">
import { useRouter, useRoute } from "vue-router";

const router = useRouter();
const route = useRoute();

const navItems = [
  { path: "/", icon: "\u2709", label: "Mail", name: "mail" },
  { path: "/calendar", icon: "\uD83D\uDCC5", label: "Calendar", name: "calendar" },
  { path: "/settings", icon: "\u2699", label: "Settings", name: "settings" },
];
</script>

<template>
  <nav class="sidebar">
    <button
      v-for="item in navItems"
      :key="item.name"
      class="sidebar-item"
      :class="{ active: route.name === item.name }"
      :title="item.label"
      @click="router.push(item.path)"
    >
      <span class="sidebar-icon">{{ item.icon }}</span>
    </button>
  </nav>
</template>

<style scoped>
.sidebar {
  width: var(--sidebar-width);
  background: var(--color-bg-secondary);
  border-right: 1px solid var(--color-border);
  display: flex;
  flex-direction: column;
  align-items: center;
  padding-top: 8px;
  gap: 4px;
  flex-shrink: 0;
}

.sidebar-item {
  width: 36px;
  height: 36px;
  border-radius: 8px;
  display: flex;
  align-items: center;
  justify-content: center;
  transition: background 0.15s;
}

.sidebar-item:hover {
  background: var(--color-bg-hover);
}

.sidebar-item.active {
  background: var(--color-bg-active);
}

.sidebar-icon {
  font-size: 18px;
}
</style>
