<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import {
  dispatch,
  formatShortcut,
  type ShortcutBinding,
  type ShortcutDef,
} from "@/lib/shortcuts";
import AboutDialog from "@/components/common/AboutDialog.vue";

defineProps<{
  showCc: boolean;
  showBcc: boolean;
}>();

const emit = defineEmits<{
  saveDraft: [];
  send: [];
  closeWindow: [];
  attach: [];
  toggleCc: [];
  toggleBcc: [];
}>();

const openMenu = ref<string | null>(null);
const aboutOpen = ref(false);

function toggleMenu(name: string) {
  openMenu.value = openMenu.value === name ? null : name;
}

function closeMenus() {
  openMenu.value = null;
}

const actions = {
  saveDraft: () => emit("saveDraft"),
  send: () => emit("send"),
  closeWindow: () => emit("closeWindow"),
  attach: () => emit("attach"),
  toggleCc: () => emit("toggleCc"),
  toggleBcc: () => emit("toggleBcc"),
  about: () => (aboutOpen.value = true),
};

function runAction(action: keyof typeof actions) {
  closeMenus();
  actions[action]();
}

// Keep Edit shortcuts browser-managed while focus is inside a text field.
function execEdit(cmd: "undo" | "redo" | "cut" | "copy" | "paste" | "selectAll") {
  closeMenus();
  document.execCommand(cmd);
}

const shortcuts = {
  saveDraft: { key: "s", ctrl: true } satisfies ShortcutDef,
  send: { key: "Enter", ctrl: true } satisfies ShortcutDef,
  closeWindow: { key: "Escape" } satisfies ShortcutDef,
  attach: { key: "a", ctrl: true, shift: true } satisfies ShortcutDef,
  undo: { key: "z", ctrl: true } satisfies ShortcutDef,
  redo: { key: "z", ctrl: true, shift: true } satisfies ShortcutDef,
  cut: { key: "x", ctrl: true } satisfies ShortcutDef,
  copy: { key: "c", ctrl: true } satisfies ShortcutDef,
  paste: { key: "v", ctrl: true } satisfies ShortcutDef,
  selectAll: { key: "a", ctrl: true } satisfies ShortcutDef,
} as const;

const bindings: readonly ShortcutBinding[] = [
  { ...shortcuts.saveDraft, handler: () => runAction("saveDraft") },
  { ...shortcuts.send, handler: () => runAction("send") },
  { ...shortcuts.closeWindow, handler: () => runAction("closeWindow") },
  { ...shortcuts.attach, handler: () => runAction("attach") },
];

function onKeyDown(event: KeyboardEvent) {
  // Compose actions remain active in text fields; Edit shortcuts are not bound.
  dispatch(event, bindings);
}

onMounted(() => window.addEventListener("keydown", onKeyDown));
onUnmounted(() => window.removeEventListener("keydown", onKeyDown));
</script>

<template>
  <div class="menu-bar" @mouseleave="closeMenus" data-testid="compose-menu-bar">
    <div class="menu-item" @click.stop="toggleMenu('file')">
      <span class="menu-label">File</span>
      <div v-if="openMenu === 'file'" class="menu-dropdown" @click.stop data-testid="compose-menu-file-dropdown">
        <button class="menu-action" data-testid="compose-menu-save-draft" @click="runAction('saveDraft')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Save Draft</span>
          <span class="action-shortcut">{{ formatShortcut(shortcuts.saveDraft) }}</span>
        </button>
        <button class="menu-action" data-testid="compose-menu-send" @click="runAction('send')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Send</span>
          <span class="action-shortcut">{{ formatShortcut(shortcuts.send) }}</span>
        </button>
        <div class="menu-separator"></div>
        <button class="menu-action" data-testid="compose-menu-close-window" @click="runAction('closeWindow')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Close Window</span>
          <span class="action-shortcut">{{ formatShortcut(shortcuts.closeWindow) }}</span>
        </button>
      </div>
    </div>

    <div class="menu-item" @click.stop="toggleMenu('edit')">
      <span class="menu-label">Edit</span>
      <div v-if="openMenu === 'edit'" class="menu-dropdown" @click.stop data-testid="compose-menu-edit-dropdown">
        <button class="menu-action" data-testid="compose-menu-undo" @click="execEdit('undo')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Undo</span>
          <span class="action-shortcut">{{ formatShortcut(shortcuts.undo) }}</span>
        </button>
        <button class="menu-action" data-testid="compose-menu-redo" @click="execEdit('redo')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Redo</span>
          <span class="action-shortcut">{{ formatShortcut(shortcuts.redo) }}</span>
        </button>
        <div class="menu-separator"></div>
        <button class="menu-action" data-testid="compose-menu-cut" @click="execEdit('cut')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Cut</span>
          <span class="action-shortcut">{{ formatShortcut(shortcuts.cut) }}</span>
        </button>
        <button class="menu-action" data-testid="compose-menu-copy" @click="execEdit('copy')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Copy</span>
          <span class="action-shortcut">{{ formatShortcut(shortcuts.copy) }}</span>
        </button>
        <button class="menu-action" data-testid="compose-menu-paste" @click="execEdit('paste')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Paste</span>
          <span class="action-shortcut">{{ formatShortcut(shortcuts.paste) }}</span>
        </button>
        <div class="menu-separator"></div>
        <button class="menu-action" data-testid="compose-menu-select-all" @click="execEdit('selectAll')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Select All</span>
          <span class="action-shortcut">{{ formatShortcut(shortcuts.selectAll) }}</span>
        </button>
      </div>
    </div>

    <div class="menu-item" @click.stop="toggleMenu('view')">
      <span class="menu-label">View</span>
      <div v-if="openMenu === 'view'" class="menu-dropdown" @click.stop data-testid="compose-menu-view-dropdown">
        <button class="menu-action" data-testid="compose-menu-show-cc" @click="runAction('toggleCc')">
          <span class="action-prefix">{{ showCc ? '\u2713' : '\u00A0' }}</span>
          <span class="action-label">Show Cc</span>
        </button>
        <button class="menu-action" data-testid="compose-menu-show-bcc" @click="runAction('toggleBcc')">
          <span class="action-prefix">{{ showBcc ? '\u2713' : '\u00A0' }}</span>
          <span class="action-label">Show Bcc</span>
        </button>
      </div>
    </div>

    <div class="menu-item" @click.stop="toggleMenu('options')">
      <span class="menu-label">Options</span>
      <div v-if="openMenu === 'options'" class="menu-dropdown" @click.stop data-testid="compose-menu-options-dropdown">
        <button class="menu-action" data-testid="compose-menu-attach" @click="runAction('attach')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Attach File&hellip;</span>
          <span class="action-shortcut">{{ formatShortcut(shortcuts.attach) }}</span>
        </button>
      </div>
    </div>

    <div class="menu-item" @click.stop="toggleMenu('help')">
      <span class="menu-label">Help</span>
      <div v-if="openMenu === 'help'" class="menu-dropdown" @click.stop data-testid="compose-menu-help-dropdown">
        <button class="menu-action" data-testid="compose-menu-about" @click="runAction('about')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">About Chithi&hellip;</span>
        </button>
      </div>
    </div>

    <AboutDialog :open="aboutOpen" @close="aboutOpen = false" />
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

.action-shortcut {
  font-size: 11px;
  color: var(--color-text-muted);
  margin-left: 24px;
}

.menu-separator {
  height: 1px;
  background: var(--color-border);
  margin: 4px 0;
}
</style>
