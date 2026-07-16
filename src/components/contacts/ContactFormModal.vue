<script setup lang="ts">
/// New/Edit contact modal, extracted from ContactsView (#166) and
/// shared by the desktop and mobile layouts (`compact` renders the
/// mobile field subset: no middle name, label selects, job title or
/// notes, so mobile doesn't silently gain fields). Owns the draft
/// fields and the create/update API calls; the parent reacts to the
/// `saved` emit (reload the visible list, re-point the selection).
///
/// `openNew(defaultBookId, prefill?)` / `openEdit(contact)` are the
/// imperative handles. The `prefill` seam exists for MessageReader's
/// add-from-address flow (#166 C17). On edit, the contact passed to
/// `openEdit` is snapshotted so the update spread preserves identity
/// fields (id, uid, remote_id, etag, vcard_data) without reading the
/// parent's selection at save time.
import { ref } from "vue";
import { useAccountsStore } from "@/stores/accounts";
import type { Contact, ContactBook } from "@/lib/types";
import * as api from "@/lib/tauri";
import { parseEmails, parsePhones } from "@/lib/contact-json";
import Select from "@/components/common/Select.vue";

const EMAIL_LABEL_OPTIONS = [
  { value: "work", label: "Work" },
  { value: "home", label: "Home" },
  { value: "other", label: "Other" },
];

const PHONE_LABEL_OPTIONS = [
  { value: "mobile", label: "Mobile" },
  { value: "work", label: "Work" },
  { value: "home", label: "Home" },
];

const props = defineProps<{
  books: ContactBook[];
  compact?: boolean;
}>();
const emit = defineEmits<{ saved: [editedId: string | null] }>();

const accountsStore = useAccountsStore();

const showForm = ref(false);
const formFirstName = ref("");
const formMiddleName = ref("");
const formLastName = ref("");
const formEmails = ref<{ email: string; label: string }[]>([{ email: "", label: "work" }]);
const formPhones = ref<{ number: string; label: string }[]>([]);
const formOrg = ref("");
const formTitle = ref("");
const formNotes = ref("");
const formBookId = ref("");
const editingContactId = ref<string | null>(null);
/// Snapshot of the contact being edited — the update call spreads it
/// so identity fields survive.
let editingContact: Contact | null = null;
const saving = ref(false);
const error = ref<string | null>(null);

function getAccountName(accountId: string): string {
  return accountsStore.accounts.find((a) => a.id === accountId)?.display_name ?? "";
}

function splitDisplayName(name: string): { first: string; middle: string; last: string } {
  const parts = name.trim().split(/\s+/);
  if (parts.length === 1) return { first: parts[0], middle: "", last: "" };
  if (parts.length === 2) return { first: parts[0], middle: "", last: parts[1] };
  return { first: parts[0], middle: parts.slice(1, -1).join(" "), last: parts[parts.length - 1] };
}

function buildDisplayName(): string {
  const parts = [formFirstName.value.trim(), formMiddleName.value.trim(), formLastName.value.trim()].filter(Boolean);
  return parts.join(" ");
}

function openNew(
  defaultBookId: string,
  prefill?: { firstName?: string; middleName?: string; lastName?: string; email?: string },
) {
  editingContactId.value = null;
  editingContact = null;
  formFirstName.value = prefill?.firstName ?? "";
  formMiddleName.value = prefill?.middleName ?? "";
  formLastName.value = prefill?.lastName ?? "";
  formEmails.value = [{ email: prefill?.email ?? "", label: "work" }];
  formPhones.value = [];
  formOrg.value = "";
  formTitle.value = "";
  formNotes.value = "";
  formBookId.value = defaultBookId;
  error.value = null;
  showForm.value = true;
}

function openEdit(contact: Contact) {
  editingContactId.value = contact.id;
  editingContact = contact;
  const nameParts = splitDisplayName(contact.display_name);
  formFirstName.value = nameParts.first;
  formMiddleName.value = nameParts.middle;
  formLastName.value = nameParts.last;
  formEmails.value = parseEmails(contact.emails_json);
  if (formEmails.value.length === 0) formEmails.value = [{ email: "", label: "work" }];
  formPhones.value = parsePhones(contact.phones_json);
  formOrg.value = contact.organization ?? "";
  formTitle.value = contact.title ?? "";
  formNotes.value = contact.notes ?? "";
  formBookId.value = contact.book_id;
  error.value = null;
  showForm.value = true;
}

defineExpose({ openNew, openEdit });

function addEmailField() { formEmails.value.push({ email: "", label: "work" }); }
function removeEmailField(idx: number) { formEmails.value.splice(idx, 1); }
function addPhoneField() { formPhones.value.push({ number: "", label: "mobile" }); }
function removePhoneField(idx: number) { formPhones.value.splice(idx, 1); }

async function saveContact() {
  if (!formFirstName.value.trim()) { error.value = "First name is required"; return; }
  if (!formLastName.value.trim()) { error.value = "Last name is required"; return; }
  saving.value = true;
  error.value = null;

  const displayName = buildDisplayName();
  const emailsFiltered = formEmails.value.filter((e) => e.email.trim());
  const phonesFiltered = formPhones.value.filter((p) => p.number.trim());

  try {
    if (editingContactId.value && editingContact) {
      await api.updateContact({
        ...editingContact,
        display_name: displayName,
        emails_json: JSON.stringify(emailsFiltered),
        phones_json: JSON.stringify(phonesFiltered),
        organization: formOrg.value || null,
        title: formTitle.value || null,
        notes: formNotes.value || null,
        book_id: formBookId.value,
      });
    } else {
      await api.createContact({
        book_id: formBookId.value,
        display_name: displayName,
        emails_json: JSON.stringify(emailsFiltered),
        phones_json: JSON.stringify(phonesFiltered),
        addresses_json: "[]",
        organization: formOrg.value || null,
        title: formTitle.value || null,
        notes: formNotes.value || null,
      });
    }
    showForm.value = false;
    emit("saved", editingContactId.value);
  } catch (e) {
    error.value = String(e);
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <Teleport to="body">
    <div v-if="showForm" class="modal-overlay" @click.self="showForm = false">
      <div class="modal">
        <div class="modal-header">
          <h3>{{ editingContactId ? "Edit Contact" : "New Contact" }}</h3>
          <button class="modal-close" @click="showForm = false">
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" /></svg>
          </button>
        </div>
        <div class="modal-body">
          <div v-if="error" class="form-error">{{ error }}</div>

          <div class="form-group">
            <label>Contact Book</label>
            <Select
              v-model="formBookId"
              :options="props.books.map(b => ({ value: b.id, label: `${b.name} (${getAccountName(b.account_id)})` }))"
            />
          </div>

          <div class="name-row">
            <div class="form-group">
              <label>First Name *</label>
              <input v-model="formFirstName" type="text" placeholder="First" autofocus />
            </div>
            <div v-if="!compact" class="form-group">
              <label>Middle Name</label>
              <input v-model="formMiddleName" type="text" placeholder="Middle" />
            </div>
            <div class="form-group">
              <label>Last Name *</label>
              <input v-model="formLastName" type="text" placeholder="Last" />
            </div>
          </div>

          <div class="form-group">
            <label>Email</label>
            <div v-for="(em, idx) in formEmails" :key="idx" class="multi-row">
              <input v-model="em.email" type="email" placeholder="email@example.com" />
              <Select v-if="!compact" v-model="em.label" :options="EMAIL_LABEL_OPTIONS" class="label-select" />
              <button v-if="formEmails.length > 1" class="rm-btn" @click="removeEmailField(idx)">&times;</button>
            </div>
            <button class="add-btn" @click="addEmailField">+ Add email</button>
          </div>

          <div class="form-group">
            <label>Phone</label>
            <div v-for="(ph, idx) in formPhones" :key="idx" class="multi-row">
              <input v-model="ph.number" type="tel" placeholder="+1 (555) 123-4567" />
              <Select v-if="!compact" v-model="ph.label" :options="PHONE_LABEL_OPTIONS" class="label-select" />
              <button class="rm-btn" @click="removePhoneField(idx)">&times;</button>
            </div>
            <button class="add-btn" @click="addPhoneField">+ Add phone</button>
          </div>

          <div class="form-group">
            <label>Organization</label>
            <input v-model="formOrg" type="text" placeholder="Company name" />
          </div>

          <div v-if="!compact" class="form-group">
            <label>Job Title</label>
            <input v-model="formTitle" type="text" placeholder="Job title" />
          </div>

          <div v-if="!compact" class="form-group">
            <label>Notes</label>
            <textarea v-model="formNotes" rows="3" placeholder="Notes"></textarea>
          </div>
        </div>
        <div class="modal-footer">
          <button class="btn-cancel" @click="showForm = false">Cancel</button>
          <button class="btn-save" :disabled="saving" data-testid="contact-save-btn" @click="saveContact">
            {{ saving ? "Saving..." : editingContactId ? "Save" : "Add Contact" }}
          </button>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
/* Contacts-flavored modal chrome, carried per-component so the look
   stays byte-identical to the pre-split view (it differs from the
   settings ModalShell chrome). */
.modal-overlay {
  position: fixed; top: 0; left: 0; right: 0; bottom: 0;
  background: rgba(0, 0, 0, 0.6);
  display: flex; align-items: center; justify-content: center;
  z-index: 1000;
}

.modal {
  background: var(--color-bg-secondary);
  border-radius: 10px;
  width: 540px;
  max-height: 85vh;
  overflow-y: auto;
  box-shadow: 0 20px 25px -5px rgba(0,0,0,0.1), 0 8px 10px -6px rgba(0,0,0,0.1);
}

.modal-header {
  display: flex; justify-content: space-between; align-items: center;
  padding: 16px 20px;
  border-bottom: 0.8px solid var(--color-border);
}
.modal-header h3 { font-size: 18px; font-weight: 600; }

.modal-close {
  width: 32px; height: 32px; border-radius: 4px;
  display: flex; align-items: center; justify-content: center;
  color: var(--color-text-muted);
}
.modal-close:hover { background: var(--color-bg-hover); }

.modal-body { padding: 20px; }

.modal-footer {
  display: flex; justify-content: flex-end; gap: 8px;
  padding: 12px 20px;
  border-top: 0.8px solid var(--color-border);
}

.form-error {
  padding: 8px 12px; background: rgba(251,44,54,0.06);
  color: var(--color-danger-text); border-radius: 6px; margin-bottom: 16px; font-size: 12px;
}

.form-group { margin-bottom: 16px; }
.form-group label { display: block; margin-bottom: 4px; font-size: 14px; font-weight: 500; color: var(--color-text-secondary); }
.form-group {
  --input-height: 36px;
  --input-padding: 0 12px;
  --input-border: 0.8px solid var(--color-border);
  --input-bg: var(--color-bg-secondary);
  --input-font-size: 16px;
}

.form-group input, .form-group textarea {
  width: 100%; height: 36px; padding: 0 12px;
  border: 0.8px solid var(--color-border); border-radius: 4px;
  background: var(--color-bg-secondary); font-size: 16px;
}
.form-group textarea { height: 96px; padding: 8px 12px; resize: vertical; line-height: 1.5; }
.form-group input:focus, .form-group textarea:focus {
  outline: none; border-color: var(--color-accent);
}

.name-row { display: flex; gap: 8px; margin-bottom: 16px; }
.name-row .form-group { flex: 1; margin-bottom: 0; }

.multi-row { display: flex; gap: 6px; margin-bottom: 6px; }
.multi-row input { flex: 1; }
.multi-row .label-select { width: 100px; flex-shrink: 0; }

.rm-btn {
  width: 36px; height: 36px; border-radius: 4px; font-size: 18px;
  color: var(--color-text-muted); display: flex; align-items: center; justify-content: center;
}
.rm-btn:hover { background: rgba(251,44,54,0.08); color: var(--color-danger-text); }

.add-btn { font-size: 13px; font-weight: 500; color: var(--color-accent); padding: 4px 0; }

.btn-cancel {
  height: 32px; padding: 0 20px; background: var(--color-bg-tertiary);
  border-radius: 4px; font-size: 16px; font-weight: 500; color: var(--color-text);
}
.btn-save {
  height: 32px; padding: 0 20px; background: var(--color-accent);
  border-radius: 4px; font-size: 16px; font-weight: 500; color: white;
}
.btn-save:disabled { opacity: 0.5; }
</style>
