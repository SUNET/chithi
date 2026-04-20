<script setup lang="ts">
import { onMounted, onBeforeUnmount, watch } from "vue";
import { storeToRefs } from "pinia";
import { useUiStore } from "@/stores/ui";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import { useRouter } from "vue-router";
import FolderTree from "@/components/mail/FolderTree.vue";

const uiStore = useUiStore();
const accountsStore = useAccountsStore();
const foldersStore = useFoldersStore();
const router = useRouter();

const { drawerOpen } = storeToRefs(uiStore);
const { accounts, activeAccountId } = storeToRefs(accountsStore);

function onScrimClick() {
  uiStore.closeDrawer();
}

async function selectAccount(accountId: string) {
  if (accountsStore.activeAccountId === accountId) return;
  accountsStore.setActiveAccount(accountId);
  try {
    await foldersStore.fetchFolders();
  } catch (e) {
    console.error("FolderDrawer: fetchFolders failed", e);
  }
}

function openSettings() {
  uiStore.closeDrawer();
  router.push("/settings");
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === "Escape" && drawerOpen.value) {
    uiStore.closeDrawer();
  }
}

onMounted(() => {
  window.addEventListener("keydown", onKeydown);
});

onBeforeUnmount(() => {
  window.removeEventListener("keydown", onKeydown);
});

// Close drawer when a folder is tapped — FolderTree updates the
// active folder in foldersStore; we watch for that and dismiss.
watch(
  () => foldersStore.activeFolderPath,
  (next, prev) => {
    if (next !== prev && drawerOpen.value) {
      uiStore.closeDrawer();
    }
  },
);
</script>

<template>
  <div
    class="folder-drawer"
    :class="{ open: drawerOpen }"
    :aria-hidden="!drawerOpen"
    :inert="!drawerOpen"
  >
    <div class="scrim" @click="onScrimClick" />
    <aside
      class="pane"
      role="dialog"
      aria-label="Folders"
      :aria-modal="drawerOpen"
    >
      <header class="brand">
        <span class="brand-wordmark">Chithi</span>
      </header>

      <div v-if="accounts.length > 0" class="account-strip">
        <button
          v-for="acct in accounts"
          :key="acct.id"
          class="account-card"
          :class="{ active: activeAccountId === acct.id }"
          @click="selectAccount(acct.id)"
        >
          <span class="account-avatar" :aria-hidden="true">
            {{ (acct.display_name || acct.email || "?").charAt(0).toUpperCase() }}
          </span>
          <span class="account-label">
            {{ acct.display_name || acct.email }}
          </span>
        </button>
      </div>

      <div class="folders">
        <FolderTree />
      </div>

      <footer class="drawer-footer">
        <button class="footer-row" @click="openSettings">
          <svg
            width="18"
            height="18"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.5"
            stroke-linecap="round"
            stroke-linejoin="round"
          >
            <circle cx="12" cy="12" r="3" />
            <path
              d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"
            />
          </svg>
          <span>Settings</span>
        </button>
      </footer>
    </aside>
  </div>
</template>

<style scoped>
.folder-drawer {
  position: fixed;
  inset: 0;
  z-index: 40;
  pointer-events: none;
}

.folder-drawer.open {
  pointer-events: auto;
}

.scrim {
  position: absolute;
  inset: 0;
  background: rgba(20, 14, 6, 0.4);
  opacity: 0;
  transition: opacity 200ms ease;
}

.folder-drawer.open .scrim {
  opacity: 1;
}

.pane {
  position: absolute;
  top: 0;
  bottom: 0;
  left: 0;
  width: 86%;
  max-width: 340px;
  background: var(--color-bg-secondary);
  box-shadow: 6px 0 24px rgba(30, 20, 10, 0.12);
  transform: translateX(-100%);
  transition: transform 220ms cubic-bezier(0.2, 0.8, 0.2, 1);
  display: flex;
  flex-direction: column;
  padding-top: env(safe-area-inset-top);
  padding-bottom: env(safe-area-inset-bottom);
}

.folder-drawer.open .pane {
  transform: translateX(0);
}

.brand {
  padding: 14px 16px 10px;
}

.brand-wordmark {
  font-size: 22px;
  font-weight: 700;
  color: var(--color-accent);
  letter-spacing: -0.3px;
}

.account-strip {
  display: flex;
  gap: 8px;
  padding: 4px 12px 12px;
  overflow-x: auto;
}

.account-card {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 6px;
  padding: 8px 10px;
  border-radius: 12px;
  background: transparent;
  border: 1.5px solid transparent;
  min-width: 84px;
  color: var(--color-text);
  font-family: inherit;
  cursor: pointer;
}

.account-card.active {
  background: var(--color-accent-light);
  border-color: var(--color-accent);
}

.account-avatar {
  width: 36px;
  height: 36px;
  border-radius: 50%;
  background: var(--color-bg-tertiary);
  color: var(--color-text);
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-weight: 600;
  font-size: 14px;
}

.account-label {
  font-size: 11px;
  line-height: 1.2;
  max-width: 76px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.folders {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 4px 0;
}

.drawer-footer {
  flex-shrink: 0;
  border-top: 1px solid var(--color-divider, #e9e0cd);
  padding: 4px 0;
}

.footer-row {
  width: 100%;
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 12px 16px;
  min-height: var(--touch-min);
  color: var(--color-text);
  font-family: inherit;
  font-size: 14px;
  background: transparent;
  border: 0;
  cursor: pointer;
}

.footer-row:active {
  background: var(--color-bg-hover);
}
</style>
