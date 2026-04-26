<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import { useRouter } from "vue-router";
import { invoke } from "@tauri-apps/api/core";
import { useUiStore, type MessageViewMode } from "@/stores/ui";
import {
  dispatch,
  formatShortcut,
  type ShortcutBinding,
  type ShortcutDef,
} from "@/lib/shortcuts";

const router = useRouter();
const uiStore = useUiStore();
const openMenu = ref<string | null>(null);

function toggleMenu(name: string) {
  openMenu.value = openMenu.value === name ? null : name;
}

function closeMenus() {
  openMenu.value = null;
}

// --- Actions ----------------------------------------------------------------

function openPreferences() {
  closeMenus();
  router.push("/preferences");
}

async function quitApp() {
  closeMenus();
  await invoke("quit_app");
}

function setViewMode(mode: MessageViewMode) {
  uiStore.setMessageViewMode(mode);
  closeMenus();
}

function toggleThreading() {
  uiStore.setThreading(!uiStore.threadingEnabled);
  closeMenus();
}

// --- Shortcut definitions ---------------------------------------------------

const sc = {
  preferences: { key: ",", ctrl: true } satisfies ShortcutDef,
  quit: { key: "q", ctrl: true } satisfies ShortcutDef,
  toggleThreading: { key: "t", ctrl: true } satisfies ShortcutDef,
} as const;

const bindings: readonly ShortcutBinding[] = [
  { ...sc.preferences, handler: openPreferences },
  { ...sc.quit, handler: quitApp },
  { ...sc.toggleThreading, handler: toggleThreading },
];

function onKeyDown(event: KeyboardEvent) {
  const target = event.target;
  // Don't fight text-editing shortcuts in inputs / textareas / contenteditables.
  if (target instanceof HTMLElement && target.isContentEditable) return;
  if (target instanceof HTMLInputElement) return;
  if (target instanceof HTMLTextAreaElement) return;
  dispatch(event, bindings);
}

onMounted(() => window.addEventListener("keydown", onKeyDown));
onUnmounted(() => window.removeEventListener("keydown", onKeyDown));
</script>

<template>
  <div class="menu-bar" @mouseleave="closeMenus" data-testid="menu-bar">
    <!-- File menu -->
    <div class="menu-item" @click.stop="toggleMenu('file')">
      <span class="menu-label">File</span>
      <div v-if="openMenu === 'file'" class="menu-dropdown" data-testid="menu-file-dropdown">
        <button class="menu-action" data-testid="menu-file-preferences" @click="openPreferences">
          <span class="action-label">Preferences&hellip;</span>
          <span class="action-shortcut">{{ formatShortcut(sc.preferences) }}</span>
        </button>
        <div class="menu-separator"></div>
        <button class="menu-action" data-testid="menu-file-quit" @click="quitApp">
          <span class="action-label">Quit</span>
          <span class="action-shortcut">{{ formatShortcut(sc.quit) }}</span>
        </button>
      </div>
    </div>

    <!-- View menu -->
    <div class="menu-item" @click.stop="toggleMenu('view')">
      <span class="menu-label">View</span>
      <div v-if="openMenu === 'view'" class="menu-dropdown" data-testid="menu-view-dropdown">
        <div class="menu-group-heading">Message Pane</div>
        <button
          class="menu-action menu-action-radio"
          data-testid="menu-view-position-none"
          @click="setViewMode('none')"
        >
          <span class="action-prefix">{{ uiStore.messageViewMode === 'none' ? '\u25CF' : '\u00A0' }}</span>
          <span class="action-label">None</span>
        </button>
        <button
          class="menu-action menu-action-radio"
          data-testid="menu-view-position-right"
          @click="setViewMode('right')"
        >
          <span class="action-prefix">{{ uiStore.messageViewMode === 'right' ? '\u25CF' : '\u00A0' }}</span>
          <span class="action-label">Right</span>
        </button>
        <button
          class="menu-action menu-action-radio"
          data-testid="menu-view-position-bottom"
          @click="setViewMode('bottom')"
        >
          <span class="action-prefix">{{ uiStore.messageViewMode === 'bottom' ? '\u25CF' : '\u00A0' }}</span>
          <span class="action-label">Bottom</span>
        </button>
        <button
          class="menu-action menu-action-radio"
          data-testid="menu-view-position-tabs"
          @click="setViewMode('tab')"
        >
          <span class="action-prefix">{{ uiStore.messageViewMode === 'tab' ? '\u25CF' : '\u00A0' }}</span>
          <span class="action-label">Tabs</span>
        </button>

        <div class="menu-separator"></div>

        <button class="menu-action" data-testid="menu-view-threaded" @click="toggleThreading">
          <span class="action-prefix">{{ uiStore.threadingEnabled ? '\u2713' : '\u00A0' }}</span>
          <span class="action-label">Threaded View</span>
          <span class="action-shortcut">{{ formatShortcut(sc.toggleThreading) }}</span>
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.menu-bar {
  display: flex;
  align-items: center;
  height: 32px;
  background: var(--color-bg);
  border-bottom: 1px solid var(--color-border);
  padding: 0 8px;
  font-size: 12px;
  flex-shrink: 0;
  user-select: none;
}

.menu-item {
  position: relative;
  padding: 4px 12px;
  border-radius: 6px;
  cursor: pointer;
}

.menu-item:hover {
  background: var(--color-bg-hover);
}

.menu-label {
  color: var(--color-text-secondary);
}

.menu-dropdown {
  position: absolute;
  top: 100%;
  left: 0;
  min-width: 240px;
  background: var(--color-bg-secondary);
  border: 1px solid var(--color-border);
  border-radius: 6px;
  padding: 4px 0;
  z-index: 1000;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.1);
}

.menu-action {
  display: grid;
  grid-template-columns: 18px 1fr auto;
  align-items: center;
  width: 100%;
  padding: 6px 16px;
  text-align: left;
  font-size: 12px;
  color: var(--color-text);
  white-space: nowrap;
  background: transparent;
  border: none;
  cursor: pointer;
}

.menu-action:hover {
  background: var(--color-bg-hover);
}

.action-prefix {
  font-family: var(--font-mono, monospace);
  font-size: 11px;
  color: var(--color-accent);
  text-align: center;
}

.action-label {
  /* takes the middle column */
}

.action-shortcut {
  font-size: 11px;
  color: var(--color-text-muted);
  margin-left: 24px;
}

.menu-action-radio {
  padding-left: 28px;
}

.menu-action-radio .action-prefix {
  /* nudge so the dot aligns visually inside the indented radio cluster */
  margin-left: -12px;
}

.menu-separator {
  height: 1px;
  background: var(--color-border);
  margin: 4px 0;
}

.menu-group-heading {
  padding: 6px 16px 2px;
  font-size: 11px;
  font-weight: 600;
  color: var(--color-text);
}
</style>
