import { defineStore } from "pinia";
import { ref, computed } from "vue";
import { listen } from "@tauri-apps/api/event";
import type { UnlistenFn } from "@tauri-apps/api/event";
import * as api from "@/lib/tauri";
import type {
  PgpKeySummary,
  PgpCardSummary,
  PgpCardDetection,
} from "@/lib/types";

// Pinia store for the OpenPGP view. Backs a flat key list (the libtumpa
// keystore is global, not per-account) plus the connected smartcards. All
// CRUD goes through the Tauri commands exported from `@/lib/tauri`.
export const usePgpStore = defineStore("pgp", () => {
  const keys = ref<PgpKeySummary[]>([]);
  const cards = ref<PgpCardSummary[]>([]);
  const selectedFingerprint = ref<string | null>(null);
  const loadingKeys = ref(false);
  const loadingCards = ref(false);
  /** Plain-text search query. Matches uid OR fingerprint substring. */
  const searchQuery = ref("");
  /** Last surfaced error message, if any. Cleared on the next successful op. */
  const lastError = ref<string | null>(null);

  let pgpChangedUnlisten: UnlistenFn | null = null;

  const selectedKey = computed<PgpKeySummary | null>(() => {
    if (!selectedFingerprint.value) return null;
    return (
      keys.value.find((k) => k.fingerprint === selectedFingerprint.value) ??
      null
    );
  });

  const filteredKeys = computed<PgpKeySummary[]>(() => {
    const q = searchQuery.value.trim().toLowerCase();
    if (!q) return keys.value;
    return keys.value.filter((k) => {
      if (k.fingerprint.toLowerCase().includes(q)) return true;
      if (k.primaryUid && k.primaryUid.toLowerCase().includes(q)) return true;
      return k.userIds.some(
        (u) =>
          u.uid.toLowerCase().includes(q) ||
          (u.email ?? "").toLowerCase().includes(q),
      );
    });
  });

  function setError(e: unknown) {
    lastError.value = e instanceof Error ? e.message : String(e);
  }

  async function fetchKeys() {
    loadingKeys.value = true;
    try {
      keys.value = await api.pgpListKeys();
      // Drop the selection if its key was deleted out from under us.
      if (
        selectedFingerprint.value &&
        !keys.value.some((k) => k.fingerprint === selectedFingerprint.value)
      ) {
        selectedFingerprint.value = null;
      }
      lastError.value = null;
    } catch (e) {
      setError(e);
    } finally {
      loadingKeys.value = false;
    }
  }

  async function fetchCards() {
    loadingCards.value = true;
    try {
      cards.value = await api.pgpListCards();
      lastError.value = null;
    } catch (e) {
      // Card enumeration failing is common (no card connected, no PCSC
      // daemon) — log to lastError but don't blow up the rest of the view.
      setError(e);
      cards.value = [];
    } finally {
      loadingCards.value = false;
    }
  }

  function selectKey(fingerprint: string | null) {
    selectedFingerprint.value = fingerprint;
  }

  function setSearch(q: string) {
    searchQuery.value = q;
  }

  async function importArmored(armored: string) {
    const bytes = new TextEncoder().encode(armored);
    const result = await api.pgpImportKey(bytes);
    await fetchKeys();
    selectedFingerprint.value = result.fingerprint;
    return result;
  }

  async function importBinary(data: Uint8Array) {
    const result = await api.pgpImportKey(data);
    await fetchKeys();
    selectedFingerprint.value = result.fingerprint;
    return result;
  }

  async function importFromPath(path: string) {
    const result = await api.pgpImportKeyFile(path);
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

  async function exportPublic(fingerprint: string): Promise<string> {
    return api.pgpExportPublic(fingerprint);
  }

  async function fetchViaWkd(email: string): Promise<string> {
    const fingerprint = await api.pgpWkdFetch(email);
    await fetchKeys();
    selectedFingerprint.value = fingerprint;
    return fingerprint;
  }

  async function autoLinkCards(): Promise<PgpCardDetection[]> {
    const detections = await api.pgpAutoLinkCards();
    // Card links are surfaced on the key list, so re-fetch keys too.
    await Promise.all([fetchKeys(), fetchCards()]);
    return detections;
  }

  /** Idempotently subscribe to the backend "pgp-changed" event. */
  async function ensureListener() {
    if (pgpChangedUnlisten) return;
    pgpChangedUnlisten = await listen<string>("pgp-changed", () => {
      // Fire-and-forget — store action handles its own errors.
      void fetchKeys();
    });
  }

  function disposeListener() {
    if (pgpChangedUnlisten) {
      pgpChangedUnlisten();
      pgpChangedUnlisten = null;
    }
  }

  return {
    // state
    keys,
    cards,
    selectedFingerprint,
    loadingKeys,
    loadingCards,
    searchQuery,
    lastError,
    // computed
    selectedKey,
    filteredKeys,
    // actions
    fetchKeys,
    fetchCards,
    selectKey,
    setSearch,
    importArmored,
    importBinary,
    importFromPath,
    deleteKey,
    exportPublic,
    fetchViaWkd,
    autoLinkCards,
    ensureListener,
    disposeListener,
  };
});
