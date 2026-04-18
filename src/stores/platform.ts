import { defineStore } from "pinia";
import { computed, ref } from "vue";

/** Breakpoint: below this width, render the mobile chrome. */
const MOBILE_MAX = 720;
const TABLET_MAX = 1024;

export type PlatformKind = "ios" | "android" | "desktop";

export const usePlatformStore = defineStore("platform", () => {
  const width = ref(
    typeof window !== "undefined" ? window.innerWidth : 1280,
  );
  const kind = ref<PlatformKind>("desktop");

  const isMobile = computed(() => width.value < MOBILE_MAX);
  const isTablet = computed(
    () => width.value >= MOBILE_MAX && width.value < TABLET_MAX,
  );
  const isDesktop = computed(() => width.value >= TABLET_MAX);

  const onResize = () => {
    width.value = window.innerWidth;
  };

  async function detectPlatform() {
    // Tauri v2 mobile build exposes platform via plugin-os.
    try {
      const mod = await import("@tauri-apps/plugin-os");
      const p = mod.platform();
      if (p === "ios") kind.value = "ios";
      else if (p === "android") kind.value = "android";
      else kind.value = "desktop";
    } catch {
      // Running outside Tauri (pnpm dev in a browser) — UA sniff fallback.
      const ua = typeof navigator !== "undefined" ? navigator.userAgent : "";
      if (/android/i.test(ua)) kind.value = "android";
      else if (/iphone|ipad|ipod/i.test(ua)) kind.value = "ios";
      else kind.value = "desktop";
    }
    if (typeof document !== "undefined") {
      document.documentElement.dataset.platform = kind.value;
    }
  }

  function init() {
    if (typeof window === "undefined") return;
    window.addEventListener("resize", onResize, { passive: true });
    onResize();
    void detectPlatform();
  }

  function dispose() {
    if (typeof window === "undefined") return;
    window.removeEventListener("resize", onResize);
  }

  return {
    width,
    kind,
    isMobile,
    isTablet,
    isDesktop,
    init,
    dispose,
  };
});
