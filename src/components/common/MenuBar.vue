<script setup lang="ts">
import { ref } from "vue";
import { useRouter } from "vue-router";
import { useUiStore } from "@/stores/ui";

const router = useRouter();
const uiStore = useUiStore();
const openMenu = ref<string | null>(null);

function toggleMenu(name: string) {
  openMenu.value = openMenu.value === name ? null : name;
}

function closeMenus() {
  openMenu.value = null;
}

function goSettings() {
  closeMenus();
  router.push("/settings");
}

function goFilters() {
  closeMenus();
  router.push("/filters");
}

function setViewMode(mode: "right" | "tab") {
  uiStore.setMessageViewMode(mode);
  closeMenus();
}

function setTheme(theme: "dark" | "light") {
  uiStore.setTheme(theme);
  closeMenus();
}

function toggleReader() {
  uiStore.toggleReader();
  closeMenus();
}

function toggleDecorations() {
  uiStore.setDecorations(!uiStore.decorationsEnabled);
  closeMenus();
}
</script>

<template>
  <div class="menu-bar" @mouseleave="closeMenus">
    <!-- File menu -->
    <div class="menu-item" @click.stop="toggleMenu('file')">
      <span class="menu-label">File</span>
      <div v-if="openMenu === 'file'" class="menu-dropdown">
        <button class="menu-action" @click="goSettings">Settings</button>
        <div class="menu-separator"></div>
        <button class="menu-action" @click="closeMenus">Close</button>
      </div>
    </div>

    <!-- View menu -->
    <div class="menu-item" @click.stop="toggleMenu('view')">
      <span class="menu-label">View</span>
      <div v-if="openMenu === 'view'" class="menu-dropdown">
        <button class="menu-action" @click="toggleReader">
          {{ uiStore.readerVisible ? 'Hide' : 'Show' }} Message Pane
        </button>
        <button
          class="menu-action"
          @click="uiStore.setThreading(!uiStore.threadingEnabled); closeMenus()"
        >
          {{ uiStore.threadingEnabled ? '\u2713 ' : '\u00A0\u00A0\u00A0' }}Threading
        </button>
        <button class="menu-action" @click="goFilters">Message Filters</button>
        <div class="menu-separator"></div>
        <div class="menu-group-label">Message View Position</div>
        <button
          class="menu-action"
          :class="{ checked: uiStore.messageViewMode === 'right' }"
          @click="setViewMode('right')"
        >
          {{ uiStore.messageViewMode === 'right' ? '\u2713 ' : '\u00A0\u00A0\u00A0' }}Right Side
        </button>
        <button
          class="menu-action"
          :class="{ checked: uiStore.messageViewMode === 'tab' }"
          @click="setViewMode('tab')"
        >
          {{ uiStore.messageViewMode === 'tab' ? '\u2713 ' : '\u00A0\u00A0\u00A0' }}New Tab
        </button>
        <div class="menu-separator"></div>
        <div class="menu-group-label">Theme</div>
        <button
          class="menu-action"
          :class="{ checked: uiStore.theme === 'dark' }"
          @click="setTheme('dark')"
        >
          {{ uiStore.theme === 'dark' ? '\u2713 ' : '\u00A0\u00A0\u00A0' }}Dark
        </button>
        <button
          class="menu-action"
          :class="{ checked: uiStore.theme === 'light' }"
          @click="setTheme('light')"
        >
          {{ uiStore.theme === 'light' ? '\u2713 ' : '\u00A0\u00A0\u00A0' }}Light
        </button>
        <div class="menu-separator"></div>
        <button
          class="menu-action"
          :class="{ checked: !uiStore.decorationsEnabled }"
          @click="toggleDecorations"
        >
          {{ !uiStore.decorationsEnabled ? '\u2713 ' : '\u00A0\u00A0\u00A0' }}Hide Window Decorations
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
  min-width: 200px;
  background: var(--color-bg-secondary);
  border: 1px solid var(--color-border);
  border-radius: 6px;
  padding: 4px 0;
  z-index: 1000;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.1);
}

.menu-action {
  display: block;
  width: 100%;
  padding: 6px 16px;
  text-align: left;
  font-size: 12px;
  color: var(--color-text);
  white-space: nowrap;
}

.menu-action:hover {
  background: var(--color-bg-hover);
}

.menu-action.checked {
  color: var(--color-accent);
}

.menu-separator {
  height: 1px;
  background: var(--color-border);
  margin: 4px 0;
}

.menu-group-label {
  padding: 4px 16px 2px;
  font-size: 10px;
  color: var(--color-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.5px;
}
</style>
