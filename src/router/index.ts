import { createRouter, createWebHistory } from "vue-router";
import MailView from "@/views/MailView.vue";
import CalendarView from "@/views/CalendarView.vue";

const router = createRouter({
  history: createWebHistory(),
  routes: [
    {
      path: "/",
      name: "mail",
      component: MailView,
    },
    {
      // Eager-imported so its module chunk is in memory before the user
      // first navigates (see #72). Component still only mounts on
      // navigation, but KeepAlive (App.vue) keeps the instance warm after
      // the first mount.
      path: "/calendar",
      name: "calendar",
      component: CalendarView,
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
      // Mobile-only thread detail (pushed from MailView on tap).
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

// Redirect to onboarding if no accounts are configured. The guard only
// runs on the main window — standalone compose/reader windows have
// their own param-driven initialization and should pass through.
router.beforeEach(async (to) => {
  if (to.name === "onboarding") return true;
  if (to.name === "compose" || to.name === "reader") return true;
  // Onboarding hands off to Settings with ?addAccount=<provider> — let it
  // through even with zero accounts, otherwise the first-run provider tap
  // would bounce right back to /onboarding.
  if (to.name === "settings" && to.query.addAccount) return true;

  const params = new URLSearchParams(window.location.search);
  if (params.get("messageId") || params.get("draftId")) return true;

  try {
    const { useAccountsStore } = await import("@/stores/accounts");
    const accountsStore = useAccountsStore();
    if (accountsStore.accounts.length === 0 && !accountsStore.loading) {
      // Attempt a fetch before redirecting — guard runs before onMounted
      // in App.vue, so the store may just be empty from cold start.
      await accountsStore.fetchAccounts();
    }
    if (accountsStore.accounts.length === 0) {
      return { name: "onboarding" };
    }
  } catch (e) {
    // Guard can fire before Pinia is fully active on very first navigation;
    // fall through and let App.vue handle it after mount.
    console.warn("router guard: account check skipped", e);
  }
  return true;
});

export default router;
