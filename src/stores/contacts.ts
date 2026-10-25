/** Persists selected contact IDs across ContactsView remounts. */
import { defineStore } from "pinia";
import { ref } from "vue";

export const useContactsStore = defineStore("contacts", () => {
  const selectedBookId = ref<string | null>(null);
  const selectedContactId = ref<string | null>(null);

  function setSelectedBook(id: string | null) {
    selectedBookId.value = id;
    // Contact IDs belong to one book.
    selectedContactId.value = null;
  }

  function setSelectedContact(id: string | null) {
    selectedContactId.value = id;
  }

  return {
    selectedBookId,
    selectedContactId,
    setSelectedBook,
    setSelectedContact,
  };
});
