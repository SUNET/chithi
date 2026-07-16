<script setup lang="ts">
/// Right pane of the desktop contacts view: the selected contact's
/// details with clickable email (opens compose) and phone (hands off
/// to the OS dialer) rows. Edit/delete are emitted; the modals stay in
/// ContactsView. The root keeps the `detail-panel` class so the view's
/// layout-mode rules (right: fixed 400px / bottom: flexible) still
/// size it.
import { useAccountsStore } from "@/stores/accounts";
import { useUiStore } from "@/stores/ui";
import type { Contact, ContactBook } from "@/lib/types";
import * as api from "@/lib/tauri";
import { acctColor } from "@/lib/account-colors";
import LinkifiedText from "@/components/common/LinkifiedText.vue";
import { openComposeWindow } from "@/lib/compose-window";
import { encodeMailtoAddress, parseMailto, sanitizeTel } from "@/lib/mailto";
import { parseEmails, parsePhones } from "@/lib/contact-json";

const props = defineProps<{
  contact: Contact | null;
  books: ContactBook[];
}>();
const emit = defineEmits<{ edit: [contact: Contact]; delete: [contactId: string] }>();

const accountsStore = useAccountsStore();
const uiStore = useUiStore();

function bookAccountId(bookId: string): string {
  return props.books.find((b) => b.id === bookId)?.account_id ?? "";
}

// Clicking a contact's email opens compose with `to` prefilled, matching
// how a mailto: link inside a mail body behaves. The address is built
// via mailto: so it goes through the same parser and routing.
function onEmailClick(email: string) {
  const params = parseMailto(`mailto:${encodeMailtoAddress(email)}`);
  if (!params) return;
  openComposeWindow({
    accountId: accountsStore.activeAccountId ?? undefined,
    ...params,
  });
}

// Phone numbers hand off to the OS via the same backend command that
// powers the LinkPopup's Open button; tel: is in its allow-list.
function onPhoneClick(number: string) {
  const tel = sanitizeTel(number);
  if (!tel) return;
  api.openLink(`tel:${tel}`).catch((e) => console.error("openLink tel: failed:", e));
}

function onLinkEnter(url: string) {
  uiStore.setHoverUrl(url);
}
function onLinkLeave() {
  uiStore.setHoverUrl(null);
}
</script>

<template>
  <div class="detail-panel">
    <template v-if="contact">
      <div class="detail-header">
        <div
          class="detail-avatar"
          :style="{ background: acctColor(bookAccountId(contact.book_id)).fill }"
        >{{ contact.display_name.charAt(0).toUpperCase() }}</div>
        <div class="detail-info">
          <h2 data-testid="contact-detail-name">{{ contact.display_name }}</h2>
          <span v-if="contact.organization" class="detail-org">
            {{ contact.title ? `${contact.title}, ` : "" }}{{ contact.organization }}
          </span>
        </div>
      </div>
      <div class="detail-actions">
        <button class="action-btn" data-testid="contact-edit-btn" @click="emit('edit', contact)">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M12 20h9M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z" /></svg>
          Edit
        </button>
        <button class="action-btn danger" data-testid="contact-delete-btn" @click="emit('delete', contact.id)">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" /></svg>
          Delete
        </button>
      </div>
      <div class="detail-fields">
        <div v-for="em in parseEmails(contact.emails_json)" :key="em.email" class="field-row">
          <span class="field-label">{{ em.label }}</span>
          <a
            class="field-value field-link"
            data-testid="contact-detail-email"
            :href="`mailto:${encodeMailtoAddress(em.email)}`"
            @click.prevent="onEmailClick(em.email)"
            @mouseenter="onLinkEnter(`mailto:${encodeMailtoAddress(em.email)}`)"
            @mouseleave="onLinkLeave"
          >{{ em.email }}</a>
        </div>
        <div v-for="ph in parsePhones(contact.phones_json)" :key="ph.number" class="field-row">
          <span class="field-label">{{ ph.label }}</span>
          <a
            class="field-value field-link"
            data-testid="contact-detail-phone"
            :href="`tel:${sanitizeTel(ph.number)}`"
            @click.prevent="onPhoneClick(ph.number)"
            @mouseenter="onLinkEnter(`tel:${sanitizeTel(ph.number)}`)"
            @mouseleave="onLinkLeave"
          >{{ ph.number }}</a>
        </div>
        <div v-if="contact.notes" class="field-row">
          <span class="field-label">Notes</span>
          <LinkifiedText
            :text="contact.notes"
            class="field-value notes"
            data-testid="contact-detail-notes"
          />
        </div>
      </div>
    </template>
    <div v-else class="empty-text">Select a contact to view details</div>
  </div>
</template>

<style scoped>
.detail-header {
  display: flex;
  align-items: center;
  gap: 16px;
  padding: 24px 20px;
  border-bottom: 0.8px solid var(--color-border);
}

.detail-avatar {
  width: 56px;
  height: 56px;
  border-radius: 50%;
  /* background set inline by acctColor() */
  color: #fff;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 20px;
  font-weight: 600;
  flex-shrink: 0;
}

.detail-info { flex: 1; }
.detail-info h2 { font-size: 20px; font-weight: 600; }
.detail-org { font-size: 14px; color: var(--color-text-muted); }

.detail-actions {
  display: flex;
  gap: 8px;
  padding: 12px 20px;
  border-bottom: 0.8px solid var(--color-border);
}

.action-btn {
  display: flex;
  align-items: center;
  gap: 6px;
  height: 32px;
  padding: 0 12px;
  background: var(--color-bg-tertiary);
  border-radius: 4px;
  font-size: 14px;
  font-weight: 500;
  color: var(--color-text);
  transition: background 0.12s;
}
.action-btn:hover { background: var(--color-border); }
.action-btn.danger { color: var(--color-danger-text); }
.action-btn.danger:hover { background: rgba(251, 44, 54, 0.08); }

.detail-fields { padding: 16px 20px; }

.field-row {
  display: flex;
  gap: 12px;
  padding: 10px 0;
  border-bottom: 0.8px solid var(--color-border);
  align-items: baseline;
}

.field-label {
  width: 70px;
  flex-shrink: 0;
  font-size: 12px;
  font-weight: 500;
  color: var(--color-text-muted);
  text-transform: capitalize;
}

.field-value { font-size: 14px; color: var(--color-text); }
.field-value.notes { white-space: pre-wrap; color: var(--color-text-secondary); }
.field-link {
  color: var(--color-accent);
  text-decoration: underline;
  cursor: pointer;
}
.field-link:hover { filter: brightness(1.1); }

.empty-text { padding: 32px 20px; text-align: center; color: var(--color-text-muted); font-size: 14px; }
</style>
