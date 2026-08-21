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
const BACKGROUND_RETRY_MAX_DELAY_MS = 30_000;

const app = createApp(App);
const pinia = createPinia();
app.use(pinia);
app.use(router);

const activityStore = useActivityStore(pinia);
const activityInitializationError =
  await initializeActivityListenersWithRetry(
    (signal) => activityStore.initEventListeners(signal),
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

  const scheduleRetry = (delayMs: number) => {
    window.setTimeout(() => {
      void initializeActivityListenersWithRetry(
        (signal) => activityStore.initEventListeners(signal),
        1,
        0,
        STARTUP_ATTEMPT_TIMEOUT_MS,
      )
        .then((error) => {
          if (error === null) {
            dismissToast(unavailableToastId);
            showToast("Background activity updates restored", "success");
            return;
          }
          console.error("Activity listener retry failed:", error);
          scheduleRetry(Math.min(delayMs * 2, BACKGROUND_RETRY_MAX_DELAY_MS));
        })
        .catch((error) => {
          console.error("Unexpected activity listener retry failure:", error);
          scheduleRetry(Math.min(delayMs * 2, BACKGROUND_RETRY_MAX_DELAY_MS));
        });
    }, delayMs);
  };

  scheduleRetry(1000);
}
