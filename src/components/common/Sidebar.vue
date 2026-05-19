<script setup lang="ts">
import { useRouter, useRoute } from "vue-router";
import { useInvitesStore } from "@/stores/invites";
import { useUiStore } from "@/stores/ui";

const router = useRouter();
const route = useRoute();

// The invites store backs the needs-action badge. It only fetches in the
// background while the badge preference is enabled (or the Invites view is
// open), so instantiating it here is cheap when the badge is turned off.
const invitesStore = useInvitesStore();
const uiStore = useUiStore();

const topItems = [
  { path: "/", label: "Mail", name: "mail" },
  { path: "/calendar", label: "Calendar", name: "calendar" },
  { path: "/invites", label: "Invites", name: "invites" },
  { path: "/contacts", label: "Contacts", name: "contacts" },
  { path: "/filters", label: "Filters", name: "filters" },
];
</script>

<template>
  <nav class="sidebar">
    <div class="sidebar-top">
      <button
        v-for="item in topItems"
        :key="item.name"
        class="sidebar-item"
        :class="{ active: route.name === item.name }"
        :title="item.label"
        :data-testid="`nav-${item.name}`"
        @click="router.push(item.path)"
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
        <!-- Invites icon (calendar with a check) -->
        <svg v-else-if="item.name === 'invites'" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <path d="M21 13V6a2 2 0 0 0-2-2H5a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2h7" />
          <path d="M16 2v4M8 2v4M3 10h18" />
          <path d="M15 18l2 2 4-4" />
        </svg>
        <!-- Contacts icon -->
        <svg v-else-if="item.name === 'contacts'" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M23 21v-2a4 4 0 0 0-3-3.87" /><path d="M16 3.13a4 4 0 0 1 0 7.75" />
        </svg>
        <!-- Filters icon -->
        <svg v-else-if="item.name === 'filters'" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <polygon points="22 3 2 3 10 12.46 10 19 14 21 14 12.46 22 3" />
        </svg>
        <!-- Unanswered-invite count badge -->
        <span
          v-if="item.name === 'invites' && uiStore.showInviteBadge && invitesStore.needsActionCount > 0"
          class="sidebar-badge"
          :data-testid="`nav-invites-badge`"
        >{{ invitesStore.needsActionCount > 99 ? "99+" : invitesStore.needsActionCount }}</span>
      </button>
    </div>
    <div class="sidebar-bottom">
      <button
        class="sidebar-item"
        :class="{ active: route.name === 'settings' }"
        title="Settings"
        data-testid="nav-settings"
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
  background: var(--color-bg-tertiary);
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
  position: relative;
  width: 36px;
  height: 36px;
  border-radius: var(--radius);
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--color-text-muted);
  transition: all 0.12s;
}

.sidebar-badge {
  position: absolute;
  top: 2px;
  right: 2px;
  min-width: 16px;
  height: 16px;
  padding: 0 4px;
  border-radius: 8px;
  background: var(--color-danger, #fb2c36);
  color: #fff;
  font-size: 10px;
  font-weight: 700;
  line-height: 16px;
  text-align: center;
  box-sizing: border-box;
}

.sidebar-item:hover {
  color: var(--color-text);
  background: var(--color-bg-hover);
}

.sidebar-item.active {
  color: #fff;
  background: var(--color-accent);
}
</style>
