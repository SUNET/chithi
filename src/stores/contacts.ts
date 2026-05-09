/**
 * Contacts UI state that needs to survive ContactsView un/remount —
 * specifically the selected book and contact, so navigating away to
 * Mail / Calendar and back doesn't lose what the user was looking at
 * (#150 follow-up). Only stores ids; the actual list of books and
 * contacts is fetched fresh by ContactsView each time it mounts.
 */
import { defineStore } from "pinia";
import { ref } from "vue";

export const useContactsStore = defineStore("contacts", () => {
  const selectedBookId = ref<string | null>(null);
  const selectedContactId = ref<string | null>(null);

  function setSelectedBook(id: string | null) {
    selectedBookId.value = id;
    // Switching books invalidates the contact selection since the
    // contact ids belong to a particular book.
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
