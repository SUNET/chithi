<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import {
  dispatch,
  formatShortcut,
  type ShortcutBinding,
  type ShortcutDef,
} from "@/lib/shortcuts";
import AboutDialog from "@/components/common/AboutDialog.vue";

const props = defineProps<{
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

function toggleMenu(name: string) {
  openMenu.value = openMenu.value === name ? null : name;
}

function closeMenus() {
  openMenu.value = null;
}

// --- Action wrappers --------------------------------------------------------

function onSaveDraft() {
  closeMenus();
  emit("saveDraft");
}

function onSend() {
  closeMenus();
  emit("send");
}

function onCloseWindow() {
  closeMenus();
  emit("closeWindow");
}

function onAttach() {
  closeMenus();
  emit("attach");
}

function onToggleCc() {
  closeMenus();
  emit("toggleCc");
}

function onToggleBcc() {
  closeMenus();
  emit("toggleBcc");
}

/**
 * Edit menu items invoke document.execCommand directly because the menu is
 * effectively a clickable surface for the same operations the OS exposes via
 * the standard editing shortcuts. The keyboard shortcuts are intentionally
 * NOT registered with our dispatch table — the browser (or the WebKitGTK
 * workaround in ComposeView) handles them so platform behaviour stays
 * intact when focus is inside a text input.
 */
function execEdit(cmd: "undo" | "redo" | "cut" | "copy" | "paste" | "selectAll") {
  closeMenus();
  document.execCommand(cmd);
}

const aboutOpen = ref(false);

function onAbout() {
  closeMenus();
  aboutOpen.value = true;
}

// --- Shortcut definitions ---------------------------------------------------

const sc = {
  saveDraft: { key: "s", ctrl: true } satisfies ShortcutDef,
  send: { key: "Enter", ctrl: true } satisfies ShortcutDef,
  closeWindow: { key: "Escape" } satisfies ShortcutDef,
  attach: { key: "a", ctrl: true, shift: true } satisfies ShortcutDef,
  // Edit shortcuts are display-only — see execEdit() comment.
  undo: { key: "z", ctrl: true } satisfies ShortcutDef,
  redo: { key: "z", ctrl: true, shift: true } satisfies ShortcutDef,
  cut: { key: "x", ctrl: true } satisfies ShortcutDef,
  copy: { key: "c", ctrl: true } satisfies ShortcutDef,
  paste: { key: "v", ctrl: true } satisfies ShortcutDef,
  selectAll: { key: "a", ctrl: true } satisfies ShortcutDef,
} as const;

const bindings: readonly ShortcutBinding[] = [
  { ...sc.saveDraft, handler: onSaveDraft },
  { ...sc.send, handler: onSend },
  { ...sc.closeWindow, handler: onCloseWindow },
  { ...sc.attach, handler: onAttach },
];

function onKeyDown(event: KeyboardEvent) {
  // Compose-window shortcuts intentionally fire even with focus in the
  // compose textarea so Ctrl+S / Ctrl+Return / Esc work while writing.
  // The Edit shortcuts (Z/X/C/V/A) are NOT in this dispatch table; they
  // fall through to the browser / WebKitGTK workaround.
  // The AboutDialog's own Esc handler stops propagation when the modal
  // is open, so we don't need a guard here.
  dispatch(event, bindings);
}

onMounted(() => window.addEventListener("keydown", onKeyDown));
onUnmounted(() => window.removeEventListener("keydown", onKeyDown));
</script>

<template>
  <div class="menu-bar" @mouseleave="closeMenus" data-testid="compose-menu-bar">
    <!-- File -->
    <div class="menu-item" @click.stop="toggleMenu('file')">
      <span class="menu-label">File</span>
      <div v-if="openMenu === 'file'" class="menu-dropdown" @click.stop data-testid="compose-menu-file-dropdown">
        <button class="menu-action" data-testid="compose-menu-save-draft" @click="onSaveDraft">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Save Draft</span>
          <span class="action-shortcut">{{ formatShortcut(sc.saveDraft) }}</span>
        </button>
        <button class="menu-action" data-testid="compose-menu-send" @click="onSend">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Send</span>
          <span class="action-shortcut">{{ formatShortcut(sc.send) }}</span>
        </button>
        <div class="menu-separator"></div>
        <button class="menu-action" data-testid="compose-menu-close-window" @click="onCloseWindow">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Close Window</span>
          <span class="action-shortcut">{{ formatShortcut(sc.closeWindow) }}</span>
        </button>
      </div>
    </div>

    <!-- Edit -->
    <div class="menu-item" @click.stop="toggleMenu('edit')">
      <span class="menu-label">Edit</span>
      <div v-if="openMenu === 'edit'" class="menu-dropdown" @click.stop data-testid="compose-menu-edit-dropdown">
        <button class="menu-action" data-testid="compose-menu-undo" @click="execEdit('undo')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Undo</span>
          <span class="action-shortcut">{{ formatShortcut(sc.undo) }}</span>
        </button>
        <button class="menu-action" data-testid="compose-menu-redo" @click="execEdit('redo')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Redo</span>
          <span class="action-shortcut">{{ formatShortcut(sc.redo) }}</span>
        </button>
        <div class="menu-separator"></div>
        <button class="menu-action" data-testid="compose-menu-cut" @click="execEdit('cut')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Cut</span>
          <span class="action-shortcut">{{ formatShortcut(sc.cut) }}</span>
        </button>
        <button class="menu-action" data-testid="compose-menu-copy" @click="execEdit('copy')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Copy</span>
          <span class="action-shortcut">{{ formatShortcut(sc.copy) }}</span>
        </button>
        <button class="menu-action" data-testid="compose-menu-paste" @click="execEdit('paste')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Paste</span>
          <span class="action-shortcut">{{ formatShortcut(sc.paste) }}</span>
        </button>
        <div class="menu-separator"></div>
        <button class="menu-action" data-testid="compose-menu-select-all" @click="execEdit('selectAll')">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Select All</span>
          <span class="action-shortcut">{{ formatShortcut(sc.selectAll) }}</span>
        </button>
      </div>
    </div>

    <!-- View -->
    <div class="menu-item" @click.stop="toggleMenu('view')">
      <span class="menu-label">View</span>
      <div v-if="openMenu === 'view'" class="menu-dropdown" @click.stop data-testid="compose-menu-view-dropdown">
        <button class="menu-action" data-testid="compose-menu-show-cc" @click="onToggleCc">
          <span class="action-prefix">{{ props.showCc ? '\u2713' : '\u00A0' }}</span>
          <span class="action-label">Show Cc</span>
          <span class="action-shortcut"></span>
        </button>
        <button class="menu-action" data-testid="compose-menu-show-bcc" @click="onToggleBcc">
          <span class="action-prefix">{{ props.showBcc ? '\u2713' : '\u00A0' }}</span>
          <span class="action-label">Show Bcc</span>
          <span class="action-shortcut"></span>
        </button>
      </div>
    </div>

    <!-- Options -->
    <div class="menu-item" @click.stop="toggleMenu('options')">
      <span class="menu-label">Options</span>
      <div v-if="openMenu === 'options'" class="menu-dropdown" @click.stop data-testid="compose-menu-options-dropdown">
        <button class="menu-action" data-testid="compose-menu-attach" @click="onAttach">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">Attach File&hellip;</span>
          <span class="action-shortcut">{{ formatShortcut(sc.attach) }}</span>
        </button>
      </div>
    </div>

    <!-- Help -->
    <div class="menu-item" @click.stop="toggleMenu('help')">
      <span class="menu-label">Help</span>
      <div v-if="openMenu === 'help'" class="menu-dropdown" @click.stop data-testid="compose-menu-help-dropdown">
        <button class="menu-action" data-testid="compose-menu-about" @click="onAbout">
          <span class="action-prefix">&#160;</span>
          <span class="action-label">About Chithi&hellip;</span>
          <span class="action-shortcut"></span>
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
