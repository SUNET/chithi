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
  ],
});

export default router;
