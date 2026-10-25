import { defineStore } from "pinia";
import { ref, computed } from "vue";
import * as api from "@/lib/tauri";
import type {
  PgpKeySummary,
  PgpCardSummary,
} from "@/lib/types";

export const usePgpStore = defineStore("pgp", () => {
  const keys = ref<PgpKeySummary[]>([]);
  const cards = ref<PgpCardSummary[]>([]);
  const selectedFingerprint = ref<string | null>(null);
  const loadingKeys = ref(false);
  const loadingCards = ref(false);
  const searchQuery = ref("");
  const lastError = ref<string | null>(null);

  const selectedKey = computed<PgpKeySummary | null>(() => {
    if (!selectedFingerprint.value) return null;
    return (
      keys.value.find(
        (key) => key.fingerprint === selectedFingerprint.value,
      ) ?? null
    );
  });

  const filteredKeys = computed<PgpKeySummary[]>(() => {
    const query = searchQuery.value.trim().toLowerCase();
    if (!query) return keys.value;
    return keys.value.filter((key) => {
      if (key.fingerprint.toLowerCase().includes(query)) return true;
      if (key.primaryUid?.toLowerCase().includes(query)) return true;
      return key.userIds.some(
        (userId) =>
          userId.uid.toLowerCase().includes(query) ||
          (userId.email ?? "").toLowerCase().includes(query),
      );
    });
  });

  function setError(error: unknown) {
    lastError.value = error instanceof Error ? error.message : String(error);
  }

  async function fetchKeys() {
    loadingKeys.value = true;
    try {
      keys.value = await api.pgpListKeys();
      if (
        selectedFingerprint.value &&
        !keys.value.some(
          (key) => key.fingerprint === selectedFingerprint.value,
        )
      ) {
        selectedFingerprint.value = null;
      }
      lastError.value = null;
    } catch (error) {
      setError(error);
    } finally {
      loadingKeys.value = false;
    }
  }

  async function fetchCards() {
    loadingCards.value = true;
    try {
      cards.value = await api.pgpListCards();
      lastError.value = null;
    } catch (error) {
      // Card access failure must not prevent the key list from loading.
      setError(error);
      cards.value = [];
    } finally {
      loadingCards.value = false;
    }
  }

  function selectKey(fingerprint: string | null) {
    selectedFingerprint.value = fingerprint;
  }

  async function importArmored(armored: string) {
    const bytes = new TextEncoder().encode(armored);
    const result = await api.pgpImportKey(bytes);
    await fetchKeys();
    selectedFingerprint.value = result.fingerprint;
    return result;
  }

  async function deleteKey(fingerprint: string) {
    await api.pgpDeleteKey(fingerprint);
    if (selectedFingerprint.value === fingerprint) {
      selectedFingerprint.value = null;
    }
    await fetchKeys();
  }

  function exportPublic(fingerprint: string): Promise<string> {
    return api.pgpExportPublic(fingerprint);
  }

  async function fetchViaWkd(email: string): Promise<string> {
    const fingerprint = await api.pgpWkdFetch(email);
    await fetchKeys();
    selectedFingerprint.value = fingerprint;
    return fingerprint;
  }

  async function autoLinkCards() {
    const detections = await api.pgpAutoLinkCards();
    await Promise.all([fetchKeys(), fetchCards()]);
    return detections;
  }

  return {
    keys,
    cards,
    selectedFingerprint,
    loadingKeys,
    loadingCards,
    searchQuery,
    lastError,
    selectedKey,
    filteredKeys,
    fetchKeys,
    fetchCards,
    selectKey,
    importArmored,
    deleteKey,
    exportPublic,
    fetchViaWkd,
    autoLinkCards,
  };
});
