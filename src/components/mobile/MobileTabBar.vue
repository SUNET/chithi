<script setup lang="ts">
import { computed } from "vue";
import { useRoute, useRouter } from "vue-router";

type TabId = "mail" | "calendar" | "contacts" | "filters" | "settings";

interface Tab {
  id: TabId;
  label: string;
  path: string;
}

const tabs: Tab[] = [
  { id: "mail", label: "Mail", path: "/" },
  { id: "calendar", label: "Calendar", path: "/calendar" },
  { id: "contacts", label: "Contacts", path: "/contacts" },
  { id: "filters", label: "Filters", path: "/filters" },
  { id: "settings", label: "Settings", path: "/settings" },
];

const router = useRouter();
const route = useRoute();

const active = computed<TabId>(() => {
  const p = route.path;
  if (p.startsWith("/calendar")) return "calendar";
  if (p.startsWith("/contacts")) return "contacts";
  if (p.startsWith("/filters")) return "filters";
  if (p.startsWith("/settings")) return "settings";
  return "mail";
});

function onTabClick(tab: Tab) {
  if (active.value === tab.id) return;
  router.push(tab.path);
}
</script>

<template>
  <nav class="mobile-tab-bar" role="tablist" aria-label="Primary">
    <button
      v-for="t in tabs"
      :key="t.id"
      class="mobile-tab"
      :class="{ active: active === t.id }"
      role="tab"
      :aria-selected="active === t.id"
      :aria-label="t.label"
      @click="onTabClick(t)"
    >
      <span class="mobile-tab-icon-wrap">
        <svg
          v-if="t.id === 'mail'"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-linecap="round"
          stroke-linejoin="round"
        >
          <rect x="2" y="4" width="20" height="16" rx="2" />
          <path d="M22 7l-10 6L2 7" />
        </svg>
        <svg
          v-else-if="t.id === 'calendar'"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-linecap="round"
          stroke-linejoin="round"
        >
          <rect x="3" y="4" width="18" height="18" rx="2" />
          <path d="M16 2v4M8 2v4M3 10h18" />
        </svg>
        <svg
          v-else-if="t.id === 'contacts'"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-linecap="round"
          stroke-linejoin="round"
        >
          <path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" />
          <circle cx="9" cy="7" r="4" />
          <path d="M23 21v-2a4 4 0 0 0-3-3.87" />
          <path d="M16 3.13a4 4 0 0 1 0 7.75" />
        </svg>
        <svg
          v-else-if="t.id === 'filters'"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-linecap="round"
          stroke-linejoin="round"
        >
          <polygon points="22 3 2 3 10 12.46 10 19 14 21 14 12.46 22 3" />
        </svg>
        <svg
          v-else
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-linecap="round"
          stroke-linejoin="round"
        >
          <circle cx="12" cy="12" r="3" />
          <path
            d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"
          />
        </svg>
      </span>
      <span class="mobile-tab-label">{{ t.label }}</span>
    </button>
  </nav>
</template>

<style scoped>
.mobile-tab-bar {
  flex-shrink: 0;
  height: var(--mobile-tab-bar-h);
  display: flex;
  align-items: stretch;
  padding: 6px 4px 2px;
  background: rgba(250, 247, 242, 0.92);
  backdrop-filter: blur(20px) saturate(180%);
  -webkit-backdrop-filter: blur(20px) saturate(180%);
  border-top: 1px solid var(--color-divider, #e9e0cd);
  padding-bottom: max(2px, env(safe-area-inset-bottom));
}

.mobile-tab {
  flex: 1;
  border: 0;
  background: transparent;
  cursor: pointer;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 2px;
  padding: 4px 2px;
  color: var(--color-text-muted);
  font-family: inherit;
}

.mobile-tab.active {
  color: var(--color-accent);
}

.mobile-tab-icon-wrap {
  position: relative;
  display: inline-flex;
  width: 28px;
  height: 28px;
  align-items: center;
  justify-content: center;
}

.mobile-tab-icon-wrap svg {
  width: 24px;
  height: 24px;
  stroke-width: 1.6;
}

.mobile-tab.active .mobile-tab-icon-wrap svg {
  stroke-width: 2;
}

.mobile-tab-label {
  font-size: 10.5px;
  font-weight: 500;
  letter-spacing: -0.1px;
  position: relative;
  z-index: 1;
}

.mobile-tab.active .mobile-tab-label {
  font-weight: 600;
}

[data-platform="android"] .mobile-tab-bar {
  background: var(--color-bg-secondary);
  backdrop-filter: none;
  -webkit-backdrop-filter: none;
  height: var(--mobile-tab-bar-h-android);
  padding: 4px 2px;
  padding-bottom: max(4px, env(safe-area-inset-bottom));
}

[data-platform="android"] .mobile-tab-icon-wrap::before {
  content: "";
  position: absolute;
  inset: -2px -16px;
  border-radius: 100px;
  background: transparent;
  transition: background 0.12s;
  z-index: 0;
}

[data-platform="android"] .mobile-tab.active .mobile-tab-icon-wrap::before {
  background: var(--color-accent-light);
}

[data-platform="android"] .mobile-tab-icon-wrap svg {
  position: relative;
  z-index: 1;
}
</style>
