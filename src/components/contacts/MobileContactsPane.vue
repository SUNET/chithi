<script setup lang="ts">
/// The entire mobile contacts layout: large-title app bar, search,
/// account filter chips, and the flat A–Z list with sticky letter
/// headers and the edge index rail. Owns the cross-book flat list and
/// its loader; the view keeps data ownership of the books and routes
/// `select` through the same handler as the desktop list (which also
/// resets multi-select and mirrors the store). The search input binds
/// to the view-owned query via v-model so the shared-ref behavior
/// survives if the platform ever flips at runtime.
import { computed, ref, watch } from "vue";
import { useAccountsStore } from "@/stores/accounts";
import type { Contact, ContactBook } from "@/lib/types";
import * as api from "@/lib/tauri";
import { acctColor } from "@/lib/account-colors";
import { parseFirstEmail } from "@/lib/contact-json";
import MobileAppBar from "@/components/mobile/MobileAppBar.vue";
import MobileIconButton from "@/components/mobile/MobileIconButton.vue";

const props = defineProps<{ books: ContactBook[] }>();
const emit = defineEmits<{ newContact: []; select: [contact: Contact] }>();

const search = defineModel<string>("search", { required: true });

const accountsStore = useAccountsStore();

// Which account the list is filtered to ("all" or an account id).
const mobileAccountFilter = ref<string>("all");

// Flatten every contact across every book so the mobile list can be
// sorted alphabetically and filtered by account. Desktop still uses the
// per-book sidebar.
const allContactsFlat = ref<Array<Contact & { _accountId: string }>>([]);

async function loadAllContacts() {
  const collected: Array<Contact & { _accountId: string }> = [];
  for (const book of props.books) {
    try {
      const list = await api.listContacts(book.id);
      for (const c of list) {
        collected.push({ ...c, _accountId: book.account_id });
      }
    } catch (e) {
      console.error("mobile contacts load failed:", e);
    }
  }
  collected.sort((a, b) =>
    a.display_name.localeCompare(b.display_name, undefined, { sensitivity: "base" }),
  );
  allContactsFlat.value = collected;
}

watch(() => props.books, () => {
  loadAllContacts();
});

const filteredMobileContacts = computed(() => {
  let list = allContactsFlat.value;
  if (mobileAccountFilter.value !== "all") {
    list = list.filter((c) => c._accountId === mobileAccountFilter.value);
  }
  if (search.value.trim()) {
    const q = search.value.toLowerCase();
    list = list.filter((c) =>
      c.display_name.toLowerCase().includes(q) ||
      c.emails_json.toLowerCase().includes(q) ||
      (c.organization ?? "").toLowerCase().includes(q),
    );
  }
  return list;
});

// Group contacts by their first letter for sticky section headers.
interface LetterGroup {
  letter: string;
  items: typeof filteredMobileContacts.value;
}
const letterGroups = computed<LetterGroup[]>(() => {
  const groups = new Map<string, LetterGroup>();
  for (const c of filteredMobileContacts.value) {
    const first = (c.display_name.trim().charAt(0) || "#").toUpperCase();
    const key = /[A-Z]/.test(first) ? first : "#";
    if (!groups.has(key)) groups.set(key, { letter: key, items: [] });
    groups.get(key)!.items.push(c);
  }
  return Array.from(groups.values()).sort((a, b) => {
    if (a.letter === "#") return 1;
    if (b.letter === "#") return -1;
    return a.letter.localeCompare(b.letter);
  });
});

const indexRailLetters = computed(() =>
  "ABCDEFGHIJKLMNOPQRSTUVWXYZ#".split(""),
);

function mobileContactInitial(c: Contact): string {
  return (c.display_name.trim().charAt(0) || "?").toUpperCase();
}
</script>

<template>
  <div class="contacts-view mobile">
    <MobileAppBar
      large
      title="Contacts"
      :subtitle="`All accounts · ${allContactsFlat.length} people`"
    >
      <template #trailing>
        <MobileIconButton aria-label="New contact" @click="emit('newContact')">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
            <line x1="12" y1="5" x2="12" y2="19" />
            <line x1="5" y1="12" x2="19" y2="12" />
          </svg>
        </MobileIconButton>
      </template>
    </MobileAppBar>

    <!-- Search -->
    <div class="mobile-search">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
        <circle cx="11" cy="11" r="8" />
        <line x1="21" y1="21" x2="16.65" y2="16.65" />
      </svg>
      <input v-model="search" type="search" placeholder="Search contacts" />
    </div>

    <!-- Account filter chips -->
    <div class="mobile-chips" role="tablist">
      <button
        class="chip"
        :class="{ active: mobileAccountFilter === 'all' }"
        role="tab"
        :aria-selected="mobileAccountFilter === 'all'"
        @click="mobileAccountFilter = 'all'"
      >All</button>
      <button
        v-for="account in accountsStore.accounts"
        :key="account.id"
        class="chip"
        :class="{ active: mobileAccountFilter === account.id }"
        role="tab"
        :aria-selected="mobileAccountFilter === account.id"
        :style="{
          '--chip-color': acctColor(account.id).fill,
          '--chip-soft': acctColor(account.id).soft,
        } as Record<string, string>"
        @click="mobileAccountFilter = account.id"
      >
        <span class="chip-dot" :style="{ background: acctColor(account.id).fill }" />
        <span>{{ account.display_name || account.email }}</span>
      </button>
    </div>

    <!-- List with sticky letter headers + edge rail -->
    <div class="mobile-list-wrap">
      <div class="mobile-list">
        <template v-for="group in letterGroups" :key="group.letter">
          <div :id="`letter-${group.letter}`" class="letter-header">
            {{ group.letter }}
          </div>
          <button
            v-for="contact in group.items"
            :key="contact.id"
            class="mobile-row"
            @click="emit('select', contact)"
          >
            <span class="mobile-row-avatar-wrap">
              <span
                class="mobile-row-avatar"
                :style="{
                  background: acctColor(contact._accountId).soft,
                  color: acctColor(contact._accountId).fill,
                  boxShadow: 'inset 0 0 0 1.5px ' + acctColor(contact._accountId).fill,
                }"
              >{{ mobileContactInitial(contact) }}</span>
              <span
                class="mobile-row-dot"
                :style="{ background: acctColor(contact._accountId).fill }"
                aria-hidden="true"
              />
            </span>
            <span class="mobile-row-body">
              <span class="mobile-row-name">{{ contact.display_name }}</span>
              <span class="mobile-row-email">{{ parseFirstEmail(contact.emails_json) }}</span>
            </span>
            <svg class="mobile-row-chevron" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
              <polyline points="9 18 15 12 9 6" />
            </svg>
          </button>
        </template>
        <div v-if="filteredMobileContacts.length === 0" class="empty-text">
          {{ search ? "No matches" : "No contacts yet" }}
        </div>
      </div>

      <!-- Edge index rail -->
      <nav class="index-rail" aria-label="Jump to letter">
        <a
          v-for="letter in indexRailLetters"
          :key="letter"
          :href="`#letter-${letter}`"
        >{{ letter }}</a>
      </nav>
    </div>
  </div>
</template>

<style scoped>
.contacts-view.mobile {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-height: 0;
  background: var(--color-bg);
}

.mobile-search {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  gap: 8px;
  margin: 6px 12px 4px;
  padding: 0 12px;
  height: 36px;
  background: var(--color-bg-secondary);
  border: 1px solid var(--color-border);
  border-radius: 10px;
  color: var(--color-text-muted);
}

.mobile-search svg {
  width: 16px;
  height: 16px;
  flex-shrink: 0;
}

.mobile-search input {
  flex: 1;
  border: 0;
  background: transparent;
  font-size: 15px;
  outline: none;
  color: var(--color-text);
}

.mobile-chips {
  flex-shrink: 0;
  display: flex;
  gap: 6px;
  padding: 6px 12px 10px;
  overflow-x: auto;
  scrollbar-width: none;
}

.mobile-chips::-webkit-scrollbar {
  display: none;
}

.chip {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  flex-shrink: 0;
  height: 30px;
  padding: 0 12px;
  border: 1px solid var(--color-border);
  border-radius: 999px;
  background: transparent;
  font-family: inherit;
  font-size: 13px;
  color: var(--color-text);
}

.chip.active {
  background: var(--color-accent);
  border-color: var(--color-accent);
  color: #fff;
}

.chip-dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
}

.chip.active .chip-dot {
  display: none;
}

.mobile-list-wrap {
  position: relative;
  flex: 1;
  min-height: 0;
  display: flex;
}

.mobile-list {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding-right: 14px; /* leave room for the edge index rail */
  scroll-behavior: smooth;
}

.letter-header {
  position: sticky;
  top: 0;
  z-index: 1;
  padding: 4px 14px;
  font-size: 11px;
  font-weight: 700;
  letter-spacing: 0.5px;
  color: var(--color-text-muted);
  background: var(--color-bg-secondary);
  border-bottom: 1px solid var(--color-divider, #e9e0cd);
  text-transform: uppercase;
}

.mobile-row {
  display: flex;
  align-items: center;
  gap: 12px;
  width: 100%;
  padding: 10px 14px;
  border: 0;
  border-bottom: 1px solid var(--color-border);
  background: transparent;
  text-align: left;
  cursor: pointer;
}

.mobile-row:active {
  background: var(--color-bg-hover);
}

.mobile-row-avatar-wrap {
  position: relative;
  flex-shrink: 0;
  width: 38px;
  height: 38px;
}

.mobile-row-avatar {
  width: 38px;
  height: 38px;
  border-radius: 50%;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 14px;
  font-weight: 600;
}

.mobile-row-dot {
  position: absolute;
  right: -1px;
  bottom: -1px;
  width: 12px;
  height: 12px;
  border-radius: 50%;
  box-shadow: 0 0 0 2px var(--color-bg);
}

.mobile-row-body {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.mobile-row-name {
  font-size: 15px;
  font-weight: 500;
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.mobile-row-email {
  font-size: 12px;
  color: var(--color-text-muted);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.mobile-row-chevron {
  width: 16px;
  height: 16px;
  flex-shrink: 0;
  stroke-width: 1.8;
  color: var(--color-text-muted);
}

.index-rail {
  position: absolute;
  right: 2px;
  top: 0;
  bottom: 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  padding: 4px 0;
  pointer-events: auto;
}

.index-rail a {
  display: block;
  font-size: 10px;
  line-height: 1.1;
  color: var(--color-text-muted);
  text-decoration: none;
  padding: 0 4px;
}

.index-rail a:active {
  color: var(--color-accent);
}

.empty-text { padding: 32px 20px; text-align: center; color: var(--color-text-muted); font-size: 14px; }
</style>
