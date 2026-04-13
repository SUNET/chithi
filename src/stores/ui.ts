import { defineStore } from "pinia";
import { ref } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";

export type MessageViewMode = "right" | "bottom" | "tab";
export type Theme = "dark" | "light";

export const useUiStore = defineStore("ui", () => {
  const threadingEnabled = ref(
    localStorage.getItem("chithi-threading") !== "false",
  );
  const folderPaneWidth = ref(200);
  const messageListWidth = ref(400);
  const readerVisible = ref(true);
  const messageViewMode = ref<MessageViewMode>(
    (localStorage.getItem("chithi-message-view-mode") as MessageViewMode) || "right",
  );
  const theme = ref<Theme>(
    (localStorage.getItem("chithi-theme") as Theme) || "light",
  );
  const decorationsEnabled = ref(
    localStorage.getItem("chithi-decorations") !== "false",
  );

  // Week start day: 0 = Sunday, 1 = Monday, 6 = Saturday
  const weekStartDay = ref<number>(
    parseInt(localStorage.getItem("chithi-week-start-day") || "0", 10),
  );

  function toggleReader() {
    readerVisible.value = !readerVisible.value;
  }

  function showReader() {
    readerVisible.value = true;
  }

  function hideReader() {
    readerVisible.value = false;
  }

  function setMessageViewMode(mode: MessageViewMode) {
    messageViewMode.value = mode;
    localStorage.setItem("chithi-message-view-mode", mode);
  }

  function setTheme(t: Theme) {
    theme.value = t;
    localStorage.setItem("chithi-theme", t);
    document.documentElement.setAttribute("data-theme", t);
  }

  function setThreading(enabled: boolean) {
    threadingEnabled.value = enabled;
    localStorage.setItem("chithi-threading", String(enabled));
  }

  function setDecorations(enabled: boolean) {
    decorationsEnabled.value = enabled;
    localStorage.setItem("chithi-decorations", String(enabled));
    getCurrentWindow().setDecorations(enabled);
  }

  function setWeekStartDay(day: number) {
    weekStartDay.value = day;
    localStorage.setItem("chithi-week-start-day", String(day));
  }

  function initTheme() {
    document.documentElement.setAttribute("data-theme", theme.value);
  }

  function initDecorations() {
    if (!decorationsEnabled.value) {
      getCurrentWindow().setDecorations(false);
    }
  }

  // Operations panel (slide-up from status bar)
  const operationsPanelOpen = ref(false);

  function toggleOperationsPanel() {
    operationsPanelOpen.value = !operationsPanelOpen.value;
  }

  return {
    threadingEnabled,
    folderPaneWidth,
    messageListWidth,
    readerVisible,
    messageViewMode,
    theme,
    toggleReader,
    showReader,
    hideReader,
    setMessageViewMode,
    setTheme,
    setThreading,
    initTheme,
    decorationsEnabled,
    setDecorations,
    initDecorations,
    operationsPanelOpen,
    toggleOperationsPanel,
    weekStartDay,
    setWeekStartDay,
  };
});
