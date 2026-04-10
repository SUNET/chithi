import { ref } from "vue";

export interface Toast {
  id: number;
  message: string;
  type: "info" | "success" | "error";
}

const toasts = ref<Toast[]>([]);
let nextId = 0;

/** Show a toast notification. Auto-dismisses after `duration` ms. */
export function showToast(message: string, type: "info" | "success" | "error" = "info", duration = 3000) {
  const id = nextId++;
  toasts.value.push({ id, message, type });
  if (duration > 0) {
    setTimeout(() => dismissToast(id), duration);
  }
  return id;
}

/** Dismiss a toast by ID (e.g., when operation completes early). */
export function dismissToast(id: number) {
  toasts.value = toasts.value.filter((t) => t.id !== id);
}

/** Reactive list of active toasts (read by ToastContainer component). */
export function useToasts() {
  return toasts;
}
