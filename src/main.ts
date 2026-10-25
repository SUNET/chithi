import { createApp } from "vue";
import { createPinia } from "pinia";
import router from "./router";
import App from "./App.vue";
import {
  initializeActivityListenersWithRetry,
  useActivityStore,
} from "./stores/activity";
import { usePlatformStore } from "./stores/platform";
import { dismissToast, showToast } from "./lib/toast";
import "./assets/styles/main.css";

const STARTUP_ATTEMPTS = 2;
const STARTUP_RETRY_DELAY_MS = 50;
const STARTUP_ATTEMPT_TIMEOUT_MS = 750;
const BACKGROUND_RETRY_INITIAL_DELAY_MS = 1_000;
const BACKGROUND_RETRY_MAX_DELAY_MS = 30_000;

const app = createApp(App);
const pinia = createPinia();
app.use(pinia);
app.use(router);

const activityStore = useActivityStore(pinia);
const initializeActivityListeners = (signal: AbortSignal) =>
  activityStore.initEventListeners(signal);
const activityInitializationError =
  await initializeActivityListenersWithRetry(
    initializeActivityListeners,
    STARTUP_ATTEMPTS,
    STARTUP_RETRY_DELAY_MS,
    STARTUP_ATTEMPT_TIMEOUT_MS,
  );

app.mount("#app");
usePlatformStore().init();

if (activityInitializationError !== null) {
  console.error(
    "Activity listeners unavailable during startup:",
    activityInitializationError,
  );
  const unavailableToastId = showToast(
    "Background activity updates are unavailable. Retrying...",
    "error",
    0,
  );

  const retryActivityListeners = async () => {
    let delayMs = BACKGROUND_RETRY_INITIAL_DELAY_MS;

    while (true) {
      await new Promise<void>((resolve) => window.setTimeout(resolve, delayMs));
      const error = await initializeActivityListenersWithRetry(
        initializeActivityListeners,
        1,
        0,
        STARTUP_ATTEMPT_TIMEOUT_MS,
      );
      if (error === null) {
        dismissToast(unavailableToastId);
        showToast("Background activity updates restored", "success");
        return;
      }

      console.error("Activity listener retry failed:", error);
      delayMs = Math.min(delayMs * 2, BACKGROUND_RETRY_MAX_DELAY_MS);
    }
  };

  void retryActivityListeners();
}
