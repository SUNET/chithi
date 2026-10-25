import { defineStore } from "pinia";
import { computed, ref, watch } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import * as api from "@/lib/tauri";

const MESSAGE_VIEW_MODES = ["right", "bottom", "tab", "none"] as const;
const CONTACT_VIEW_MODES = ["right", "bottom"] as const;
const THEMES = ["system", "light", "dark"] as const;
const TIME_FORMATS = ["auto", "12", "24"] as const;
const VALID_WEEK_STARTS = [0, 1, 6];

export type MessageViewMode = (typeof MESSAGE_VIEW_MODES)[number];
export type ContactViewMode = (typeof CONTACT_VIEW_MODES)[number];
export type Theme = (typeof THEMES)[number];
export type TimeFormat = (typeof TIME_FORMATS)[number];
type ComposeKind = "new" | "reply" | "reply-all" | "forward";

function storedOption<T extends string>(
  key: string,
  options: readonly T[],
  fallback: T,
): T {
  const stored = localStorage.getItem(key) as T | null;
  return stored && options.includes(stored) ? stored : fallback;
}

export const useUiStore = defineStore("ui", () => {
  const threadingEnabled = ref(
    localStorage.getItem("chithi-threading") !== "false",
  );
  const folderPaneWidth = ref(200);
  const messageListWidth = ref(400);
  const readerVisible = ref(true);
  const messageViewMode = ref<MessageViewMode>(
    storedOption("chithi-message-view-mode", MESSAGE_VIEW_MODES, "right"),
  );
  const contactViewMode = ref<ContactViewMode>(
    storedOption("chithi-contact-view-mode", CONTACT_VIEW_MODES, "right"),
  );
  const theme = ref<Theme>(
    storedOption("chithi-theme", THEMES, "system"),
  );

  // A ref keeps OS theme changes reactive.
  const systemDark = ref(
    typeof window !== "undefined" && window.matchMedia
      ? window.matchMedia("(prefers-color-scheme: dark)").matches
      : false,
  );

  const resolvedTheme = computed<"light" | "dark">(() => {
    if (theme.value === "system") return systemDark.value ? "dark" : "light";
    return theme.value;
  });
  const decorationsEnabled = ref(
    localStorage.getItem("chithi-decorations") !== "false",
  );

  const preferHtmlBody = ref(
    localStorage.getItem("chithi-prefer-html-body") === "true",
  );
  function setPreferHtmlBody(prefer: boolean) {
    preferHtmlBody.value = prefer;
    localStorage.setItem("chithi-prefer-html-body", String(prefer));
  }

  const showInviteBadge = ref(
    localStorage.getItem("chithi-show-invite-badge") !== "false",
  );
  function setShowInviteBadge(enabled: boolean) {
    showInviteBadge.value = enabled;
    localStorage.setItem("chithi-show-invite-badge", String(enabled));
  }

  // 0 = Sunday, 1 = Monday, 6 = Saturday.
  const weekStartDay = ref<number>(
    (() => {
      const stored = parseInt(localStorage.getItem("chithi-week-start-day") || "0", 10);
      return VALID_WEEK_STARTS.includes(stored) ? stored : 0;
    })(),
  );

  const storedTimezone = localStorage.getItem("chithi-display-timezone");
  const displayTimezone = ref<string>(storedTimezone || "UTC");
  const timezoneList = ref<string[]>([]);

  const timeFormat = ref<TimeFormat>(
    storedOption("chithi-time-format", TIME_FORMATS, "auto"),
  );

  // Undefined delegates 12/24-hour selection to Intl.
  const hour12 = computed<boolean | undefined>(() => {
    if (timeFormat.value === "12") return true;
    if (timeFormat.value === "24") return false;
    return undefined;
  });

  function showReader() {
    readerVisible.value = true;
  }

  function hideReader() {
    readerVisible.value = false;
  }

  function setMessageViewMode(mode: MessageViewMode) {
    messageViewMode.value = mode;
    localStorage.setItem("chithi-message-view-mode", mode);
    // Selecting a visible mode reopens a reader hidden for this session.
    if (mode !== "none") {
      readerVisible.value = true;
    }
  }

  function setContactViewMode(mode: ContactViewMode) {
    contactViewMode.value = mode;
    localStorage.setItem("chithi-contact-view-mode", mode);
  }

  function setTheme(nextTheme: Theme) {
    theme.value = nextTheme;
    localStorage.setItem("chithi-theme", nextTheme);
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
    if (!VALID_WEEK_STARTS.includes(day)) return;
    weekStartDay.value = day;
    localStorage.setItem("chithi-week-start-day", String(day));
  }

  function setTimeFormat(format: TimeFormat) {
    if (!TIME_FORMATS.includes(format)) return;
    timeFormat.value = format;
    localStorage.setItem("chithi-time-format", format);
  }

  function setDisplayTimezone(timezone: string) {
    if (
      timezoneList.value.length > 0 &&
      !timezoneList.value.includes(timezone)
    ) {
      console.warn(
        `setDisplayTimezone: unknown timezone '${timezone}', ignoring`,
      );
      return;
    }
    displayTimezone.value = timezone;
    localStorage.setItem("chithi-display-timezone", timezone);
  }

  async function initTimezone() {
    try {
      timezoneList.value = await api.listTimezones();
    } catch (error) {
      console.error("Failed to load timezones:", error);
    }

    if (!storedTimezone) {
      try {
        const osTimezone = await api.getDefaultTimezone();
        setDisplayTimezone(osTimezone);
      } catch {
        setDisplayTimezone("UTC");
      }
    }
  }

  let stopMediaQuery: (() => void) | null = null;

  function initTheme() {
    document.documentElement.setAttribute("data-theme", resolvedTheme.value);
    if (typeof window !== "undefined" && window.matchMedia && !stopMediaQuery) {
      const mediaQuery = window.matchMedia("(prefers-color-scheme: dark)");
      const onChange = (event: MediaQueryListEvent) => {
        systemDark.value = event.matches;
      };
      mediaQuery.addEventListener("change", onChange);
      stopMediaQuery = () => mediaQuery.removeEventListener("change", onChange);
    }
  }

  watch(resolvedTheme, (nextTheme) => {
    if (typeof document !== "undefined") {
      document.documentElement.setAttribute("data-theme", nextTheme);
    }
  });

  function initDecorations() {
    if (!decorationsEnabled.value) {
      getCurrentWindow().setDecorations(false);
    }
  }

  const operationsPanelOpen = ref(false);

  function toggleOperationsPanel() {
    operationsPanelOpen.value = !operationsPanelOpen.value;
  }

  const composeOpen = ref(false);
  const drawerOpen = ref(false);
  // Mobile replies retain their source and intent while the sheet opens.
  const composeContext = ref<{ replyTo: string | null; kind: ComposeKind }>(
    { replyTo: null, kind: "new" },
  );

  function openCompose(params?: { replyTo?: string | null; kind?: ComposeKind }) {
    composeContext.value = {
      replyTo: params?.replyTo ?? null,
      kind: params?.kind ?? "new",
    };
    composeOpen.value = true;
  }
  function closeCompose() {
    composeOpen.value = false;
  }
  function openDrawer() {
    drawerOpen.value = true;
  }
  function closeDrawer() {
    drawerOpen.value = false;
  }

  const hoverUrl = ref<string | null>(null);
  function setHoverUrl(url: string | null) {
    hoverUrl.value = url;
  }

  const linkPopupUrl = ref<string | null>(null);
  function openLinkPopup(url: string) {
    linkPopupUrl.value = url;
  }
  function closeLinkPopup() {
    linkPopupUrl.value = null;
  }

  return {
    threadingEnabled,
    folderPaneWidth,
    messageListWidth,
    readerVisible,
    messageViewMode,
    contactViewMode,
    theme,
    resolvedTheme,
    showReader,
    hideReader,
    setMessageViewMode,
    setContactViewMode,
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
    displayTimezone,
    timezoneList,
    setDisplayTimezone,
    initTimezone,
    timeFormat,
    hour12,
    setTimeFormat,
    composeOpen,
    composeContext,
    drawerOpen,
    openCompose,
    closeCompose,
    openDrawer,
    closeDrawer,
    hoverUrl,
    setHoverUrl,
    linkPopupUrl,
    openLinkPopup,
    closeLinkPopup,
    preferHtmlBody,
    setPreferHtmlBody,
    showInviteBadge,
    setShowInviteBadge,
  };
});
