<script setup lang="ts">
import { useRouter, useRoute } from "vue-router";
import { useAccountsStore } from "@/stores/accounts";
import { openComposeWindow } from "@/lib/compose-window";

const router = useRouter();
const route = useRoute();
const accountsStore = useAccountsStore();

const topItems = [
  { path: "/", label: "Mail", name: "mail" },
  { path: "/calendar", label: "Calendar", name: "calendar" },
  { path: "", label: "Compose", name: "compose" },
  { path: "/contacts", label: "Contacts", name: "contacts" },
];

function handleNavClick(item: typeof topItems[0]) {
  if (item.name === "compose") {
    openComposeWindow({ accountId: accountsStore.activeAccountId ?? undefined });
  } else {
    router.push(item.path);
  }
}
</script>

<template>
  <nav class="sidebar">
    <div class="sidebar-top">
      <button
        v-for="item in topItems"
        :key="item.name"
        class="sidebar-item"
        :class="{ active: item.name !== 'compose' && route.name === item.name }"
        :title="item.label"
        @click="handleNavClick(item)"
      >
        <!-- Mail icon -->
        <svg v-if="item.name === 'mail'" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <rect x="2" y="4" width="20" height="16" rx="2" />
          <path d="M22 7l-10 6L2 7" />
        </svg>
        <!-- Calendar icon -->
        <svg v-else-if="item.name === 'calendar'" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <rect x="3" y="4" width="18" height="18" rx="2" />
          <path d="M16 2v4M8 2v4M3 10h18" />
        </svg>
        <!-- Compose icon -->
        <svg v-else-if="item.name === 'compose'" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12 20h9M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z" />
        </svg>
        <!-- Contacts icon -->
        <svg v-else-if="item.name === 'contacts'" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M23 21v-2a4 4 0 0 0-3-3.87" /><path d="M16 3.13a4 4 0 0 1 0 7.75" />
        </svg>
      </button>
    </div>
    <div class="sidebar-bottom">
      <button
        class="sidebar-item"
        :class="{ active: route.name === 'settings' }"
        title="Settings"
        @click="router.push('/settings')"
      >
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <circle cx="12" cy="12" r="3" />
          <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
        </svg>
      </button>
    </div>
  </nav>
</template>

<style scoped>
.sidebar {
  width: var(--sidebar-width);
  background: var(--color-bg-secondary);
  border-right: 0.8px solid var(--color-border);
  display: flex;
  flex-direction: column;
  justify-content: space-between;
  flex-shrink: 0;
}

.sidebar-top,
.sidebar-bottom {
  display: flex;
  flex-direction: column;
  align-items: center;
  padding: 12px 0;
  gap: 12px;
}

.sidebar-item {
  width: 36px;
  height: 36px;
  border-radius: 10px;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--color-text-muted);
  transition: all 0.12s;
}

.sidebar-item:hover {
  color: var(--color-text);
  background: var(--color-bg-hover);
}

.sidebar-item.active {
  color: white;
  background: var(--color-accent);
}
</style>
