import { defineStore } from "pinia";
import { computed, ref } from "vue";

const MOBILE_MAX = 720;

type PlatformKind = "ios" | "android" | "desktop";

export const usePlatformStore = defineStore("platform", () => {
  const width = ref(
    typeof window !== "undefined" ? window.innerWidth : 1280,
  );
  const kind = ref<PlatformKind>("desktop");
  const platformReady = ref(false);

  const isMobile = computed(() => width.value < MOBILE_MAX);

  const onResize = () => {
    width.value = window.innerWidth;
  };

  async function detectPlatform() {
    try {
      const { platform } = await import("@tauri-apps/plugin-os");
      const p = await platform();
      if (p === "ios") kind.value = "ios";
      else if (p === "android") kind.value = "android";
      else kind.value = "desktop";
    } catch {
      const ua = typeof navigator !== "undefined" ? navigator.userAgent : "";
      if (/android/i.test(ua)) kind.value = "android";
      else if (/iphone|ipad|ipod/i.test(ua)) kind.value = "ios";
      else kind.value = "desktop";
    }
    if (typeof document !== "undefined") {
      document.documentElement.dataset.platform = kind.value;
    }
    platformReady.value = true;
  }

  function init() {
    if (typeof window === "undefined") return;
    window.addEventListener("resize", onResize, { passive: true });
    onResize();
    void detectPlatform();
  }

  return {
    width,
    kind,
    platformReady,
    isMobile,
    init,
  };
});
