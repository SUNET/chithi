import { createRouter, createWebHistory } from "vue-router";
import MailView from "@/views/MailView.vue";
import CalendarView from "@/views/CalendarView.vue";
import { useAccountsStore } from "@/stores/accounts";

const ACCOUNT_OPTIONAL_ROUTES = new Set([
  "onboarding",
  "compose",
  "reader",
  "settings",
]);

const router = createRouter({
  history: createWebHistory(),
  routes: [
    {
      path: "/",
      name: "mail",
      component: MailView,
    },
    {
      // Eager loading avoids a delay on the first calendar navigation.
      path: "/calendar",
      name: "calendar",
      component: CalendarView,
    },
    {
      path: "/invites",
      name: "invites",
      component: () => import("@/views/InvitesView.vue"),
    },
    {
      path: "/filters",
      name: "filters",
      component: () => import("@/views/FiltersView.vue"),
    },
    {
      path: "/contacts",
      name: "contacts",
      component: () => import("@/views/ContactsView.vue"),
    },
    {
      path: "/openpgp",
      name: "openpgp",
      component: () => import("@/views/OpenPGPView.vue"),
    },
    {
      path: "/reader",
      name: "reader",
      component: () => import("@/views/ReaderView.vue"),
    },
    {
      path: "/compose",
      name: "compose",
      component: () => import("@/views/ComposeView.vue"),
    },
    {
      path: "/settings",
      name: "settings",
      component: () => import("@/views/SettingsView.vue"),
    },
    {
      path: "/preferences",
      name: "preferences",
      component: () => import("@/views/PreferencesView.vue"),
    },
    {
      path: "/mail/thread/:id",
      name: "mobile-reader",
      component: () => import("@/views/MobileThreadView.vue"),
    },
    {
      path: "/onboarding",
      name: "onboarding",
      component: () => import("@/views/OnboardingView.vue"),
    },
  ],
});

router.beforeEach(async (to) => {
  if (
    typeof to.name === "string" &&
    ACCOUNT_OPTIONAL_ROUTES.has(to.name)
  ) {
    return true;
  }

  if (localStorage.getItem("chithi-onboarding-skipped") === "true") return true;

  try {
    const accountsStore = useAccountsStore();
    if (accountsStore.accounts.length === 0) {
      await accountsStore.fetchAccounts();
    }
    return accountsStore.accounts.length === 0
      ? { name: "onboarding" }
      : true;
  } catch (error) {
    console.warn("router guard: account check skipped", error);
    return true;
  }
});

export default router;
