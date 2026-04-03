import { createRouter, createWebHistory } from "vue-router";
import MailView from "@/views/MailView.vue";

const router = createRouter({
  history: createWebHistory(),
  routes: [
    {
      path: "/",
      name: "mail",
      component: MailView,
    },
    {
      path: "/calendar",
      name: "calendar",
      component: () => import("@/views/CalendarView.vue"),
    },
    {
      path: "/filters",
      name: "filters",
      component: () => import("@/views/FiltersView.vue"),
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
