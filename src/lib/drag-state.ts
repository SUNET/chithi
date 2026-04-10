import { ref } from "vue";

/** Shared reactive state for custom drag-and-drop (WebKitGTK doesn't support HTML5 DnD). */
export const dragMessageIds = ref<string[]>([]);
export const dragSourceAccountId = ref<string | null>(null);
export const isDragging = ref(false);
