<script setup lang="ts">
import { ref } from "vue";
import { useRouter } from "vue-router";
import { useAccountsStore } from "@/stores/accounts";
import type { AccountConfig } from "@/lib/types";
import * as api from "@/lib/tauri";

const router = useRouter();
const accountsStore = useAccountsStore();
const showForm = ref(false);
const showDeleteConfirm = ref(false);
const deletingAccountId = ref<string | null>(null);
const saving = ref(false);
const error = ref<string | null>(null);
const editingAccountId = ref<string | null>(null);

const avatarColors = ["#3366cc", "#2e7d32", "#9c27b0", "#e65100", "#00838f"];

function getAvatarColor(index: number): string {
  return avatarColors[index % avatarColors.length];
}

function getInitials(name: string): string {
  const words = name.split(/\s+/);
  if (words.length >= 2) return (words[0][0] + words[1][0]).toUpperCase();
  return name.slice(0, 2).toUpperCase();
}

const defaultForm = (): AccountConfig => ({
  display_name: "",
  email: "",
  provider: "generic",
  mail_protocol: "imap",
  imap_host: "",
  imap_port: 993,
  smtp_host: "",
  smtp_port: 587,
  jmap_url: "",
  caldav_url: "",
  username: "",
  password: "",
  use_tls: true,
});

const form = ref<AccountConfig>(defaultForm());

type AccountType = "gmail" | "imap" | "jmap" | "caldav";
const accountType = ref<AccountType>("gmail");

function selectAccountType(type: AccountType) {
  accountType.value = type;
  const f = form.value;
  switch (type) {
    case "gmail":
      f.provider = "gmail";
      f.mail_protocol = "imap";
      if (!editingAccountId.value) {
        f.imap_host = "imap.gmail.com";
        f.imap_port = 993;
        f.smtp_host = "smtp.gmail.com";
        f.smtp_port = 587;
      }
      f.jmap_url = "";
      f.use_tls = true;
      break;
    case "imap":
      f.provider = "generic";
      f.mail_protocol = "imap";
      f.jmap_url = "";
      f.use_tls = true;
      break;
    case "jmap":
      f.provider = "generic";
      f.mail_protocol = "jmap";
      f.use_tls = true;
      break;
    case "caldav":
      f.provider = "generic";
      f.mail_protocol = "imap";
      f.imap_host = "";
      f.imap_port = 0;
      f.smtp_host = "";
      f.smtp_port = 0;
      f.jmap_url = "";
      f.use_tls = true;
      break;
  }
}

function openNewForm() {
  editingAccountId.value = null;
  form.value = defaultForm();
  accountType.value = "gmail";
  selectAccountType("gmail");
  showForm.value = true;
  error.value = null;
}

async function openEditForm(id: string) {
  editingAccountId.value = id;
  error.value = null;
  try {
    const config = await api.getAccountConfig(id);
    form.value = config;
    if (config.provider === "gmail") {
      accountType.value = "gmail";
    } else if (config.mail_protocol === "jmap") {
      accountType.value = "jmap";
    } else if (config.caldav_url && !config.imap_host) {
      accountType.value = "caldav";
    } else {
      accountType.value = "imap";
    }
    showForm.value = true;
  } catch (e) {
    error.value = String(e);
  }
}

async function saveAccount() {
  saving.value = true;
  error.value = null;
  try {
    if (editingAccountId.value) {
      await api.updateAccount(editingAccountId.value, form.value);
      await accountsStore.fetchAccounts();
    } else {
      await accountsStore.addAccount(form.value);
      router.push("/");
    }
    showForm.value = false;
    editingAccountId.value = null;
  } catch (e) {
    error.value = String(e);
  } finally {
    saving.value = false;
  }
}

function cancelForm() {
  showForm.value = false;
  editingAccountId.value = null;
  error.value = null;
}

function confirmDelete(id: string) {
  deletingAccountId.value = id;
  showDeleteConfirm.value = true;
}

async function doDelete() {
  if (deletingAccountId.value) {
    await accountsStore.deleteAccount(deletingAccountId.value);
  }
  showDeleteConfirm.value = false;
  deletingAccountId.value = null;
}
</script>

<template>
  <div class="settings-view">
    <div class="settings-content">
      <h1 class="settings-title">Settings</h1>

      <div class="section-header">
        <h2 class="section-title">Email Accounts</h2>
        <button class="btn-add" @click="openNewForm">
          + Add Account
        </button>
      </div>

      <div class="account-list">
        <div
          v-for="(account, idx) in accountsStore.accounts"
          :key="account.id"
          class="account-card"
        >
          <div class="account-card-left">
            <span class="account-avatar" :style="{ background: getAvatarColor(idx) }">
              {{ getInitials(account.display_name) }}
            </span>
            <div class="account-card-info">
              <span class="account-card-name">{{ account.display_name }}</span>
              <span class="account-card-email">{{ account.email }}</span>
              <span class="account-card-type">{{ account.provider === 'gmail' ? 'Gmail' : account.mail_protocol.toUpperCase() }}</span>
            </div>
          </div>
          <div class="account-card-actions">
            <button class="icon-btn-sm" title="Edit" @click="openEditForm(account.id)">
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                <path d="M12 20h9M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z" />
              </svg>
            </button>
            <button class="icon-btn-sm danger" title="Delete" @click="confirmDelete(account.id)">
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                <polyline points="3 6 5 6 21 6" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
              </svg>
            </button>
          </div>
        </div>
      </div>
    </div>

    <!-- Add/Edit Account Modal -->
    <Teleport to="body">
      <div v-if="showForm" class="modal-overlay" @click.self="cancelForm">
        <div class="modal">
          <div class="modal-header">
            <h3>{{ editingAccountId ? 'Edit Account' : 'Add Account' }}</h3>
            <button class="modal-close" @click="cancelForm">&times;</button>
          </div>
          <div class="modal-body">
            <div v-if="error" class="form-error">{{ error }}</div>

            <div class="form-group">
              <label>Account Type</label>
              <div class="type-selector">
                <button
                  v-for="t in (['gmail', 'imap', 'jmap', 'caldav'] as AccountType[])"
                  :key="t"
                  class="type-btn"
                  :class="{ active: accountType === t }"
                  :disabled="!!editingAccountId"
                  @click="selectAccountType(t)"
                >{{ t === 'gmail' ? 'Gmail' : t.toUpperCase() }}</button>
              </div>
            </div>

            <div class="form-group">
              <label>Account Name</label>
              <input v-model="form.display_name" type="text" :placeholder="accountType === 'caldav' ? 'My Calendar' : 'e.g., Personal, Work'" />
            </div>
            <div v-if="accountType !== 'caldav'" class="form-group">
              <label>Email Address</label>
              <input v-model="form.email" type="email" placeholder="user@example.com" />
            </div>
            <div class="form-group">
              <label>Password</label>
              <input v-model="form.password" type="password" placeholder="••••••••" />
              <span class="field-hint">Passwords are stored securely in your OS keyring</span>
            </div>

            <template v-if="accountType === 'imap'">
              <div class="form-row">
                <div class="form-group">
                  <label>IMAP Server</label>
                  <input v-model="form.imap_host" type="text" placeholder="imap.example.com" />
                </div>
                <div class="form-group port">
                  <label>Port</label>
                  <input v-model.number="form.imap_port" type="number" />
                </div>
              </div>
              <div class="form-row">
                <div class="form-group">
                  <label>SMTP Server</label>
                  <input v-model="form.smtp_host" type="text" placeholder="smtp.example.com" />
                </div>
                <div class="form-group port">
                  <label>Port</label>
                  <input v-model.number="form.smtp_port" type="number" />
                </div>
              </div>
            </template>

            <template v-if="accountType === 'jmap'">
              <div class="form-group">
                <label>JMAP URL</label>
                <input v-model="form.jmap_url" type="url" placeholder="https://mail.example.com" />
                <span class="field-hint">Leave blank for auto-discovery via .well-known/jmap</span>
              </div>
            </template>

            <template v-if="accountType === 'imap' || accountType === 'caldav'">
              <div class="form-group">
                <label>CalDAV URL {{ accountType === 'caldav' ? '' : '(optional)' }}</label>
                <input v-model="form.caldav_url" type="url" placeholder="https://mail.example.com/dav/cal" />
              </div>
            </template>

            <template v-if="accountType === 'gmail' && !editingAccountId">
              <div class="info-box">Gmail settings will be configured automatically</div>
            </template>
          </div>
          <div class="modal-footer">
            <button class="btn-secondary" @click="cancelForm">Cancel</button>
            <button class="btn-primary" :disabled="saving" @click="saveAccount">
              {{ saving ? "Saving..." : (editingAccountId ? "Save" : "Add Account") }}
            </button>
          </div>
        </div>
      </div>
    </Teleport>

    <!-- Delete Confirmation Modal -->
    <Teleport to="body">
      <div v-if="showDeleteConfirm" class="modal-overlay" @click.self="showDeleteConfirm = false">
        <div class="modal modal-sm">
          <div class="modal-body">
            <h3 class="confirm-title">Delete Account</h3>
            <p class="confirm-text">Are you sure you want to delete this account? This action cannot be undone.</p>
          </div>
          <div class="modal-footer">
            <button class="btn-secondary" @click="showDeleteConfirm = false">Cancel</button>
            <button class="btn-danger" @click="doDelete">Delete</button>
          </div>
        </div>
      </div>
    </Teleport>
  </div>
</template>

<style scoped>
.settings-view {
  height: 100%;
  overflow-y: auto;
  padding: 32px;
  background: var(--color-bg);
}

.settings-content {
  max-width: 640px;
  margin: 0 auto;
}

.settings-title {
  font-size: 24px;
  font-weight: 600;
  margin-bottom: 24px;
}

.section-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin-bottom: 16px;
}

.section-title {
  font-size: 18px;
  font-weight: 500;
  color: var(--color-text);
}

.btn-add {
  display: flex;
  align-items: center;
  gap: 4px;
  height: 36px;
  padding: 0 16px;
  background: var(--color-accent);
  color: white;
  border-radius: 999px;
  font-size: 14px;
  font-weight: 500;
  transition: background 0.12s;
}

.btn-add:hover {
  background: var(--color-accent-hover);
}

.account-list {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.account-card {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 16px;
  border: 0.8px solid var(--color-border);
  border-radius: 10px;
  background: white;
  min-height: 100px;
}

.account-card-left {
  display: flex;
  align-items: center;
  gap: 12px;
}

.account-avatar {
  width: 48px;
  height: 48px;
  border-radius: 50%;
  color: white;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 16px;
  font-weight: 500;
  flex-shrink: 0;
}

.account-card-info {
  display: flex;
  flex-direction: column;
}

.account-card-name {
  font-size: 18px;
  font-weight: 500;
}

.account-card-email {
  font-size: 12px;
  color: var(--color-text-muted);
}

.account-card-type {
  font-size: 10px;
  color: var(--color-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.5px;
  margin-top: 1px;
}

.account-card-actions {
  display: flex;
  gap: 8px;
}

.icon-btn-sm {
  width: 32px;
  height: 32px;
  border-radius: 6px;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--color-text-muted);
  transition: all 0.12s;
}

.icon-btn-sm:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.icon-btn-sm.danger {
  color: var(--color-danger);
}

.icon-btn-sm.danger:hover {
  background: rgba(220, 53, 69, 0.08);
}

/* Modal */
.modal-overlay {
  position: fixed;
  top: 0; left: 0; right: 0; bottom: 0;
  background: rgba(0, 0, 0, 0.2);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 1000;
}

.modal {
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: 12px;
  width: 480px;
  max-height: 85vh;
  overflow-y: auto;
  box-shadow: 0 8px 32px rgba(0, 0, 0, 0.12);
}

.modal-sm {
  width: 400px;
}

.modal-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 16px 20px;
  border-bottom: 1px solid var(--color-border);
}

.modal-header h3 {
  font-size: 16px;
  font-weight: 600;
}

.modal-close {
  font-size: 20px;
  color: var(--color-text-muted);
  width: 28px;
  height: 28px;
  border-radius: 6px;
  display: flex;
  align-items: center;
  justify-content: center;
}

.modal-close:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.modal-body {
  padding: 20px;
}

.modal-footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding: 12px 20px;
  border-top: 1px solid var(--color-border);
}

.form-error {
  padding: 8px 12px;
  background: rgba(220, 53, 69, 0.06);
  color: var(--color-danger);
  border-radius: 6px;
  margin-bottom: 16px;
  font-size: 12px;
}

.form-group {
  margin-bottom: 14px;
}

.form-group label {
  display: block;
  margin-bottom: 4px;
  font-size: 12px;
  font-weight: 500;
  color: var(--color-text-secondary);
}

.form-group input {
  width: 100%;
  height: 40px;
  padding: 0 12px;
  border: 0.8px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg-secondary);
  font-size: 16px;
}

.form-group input:focus {
  outline: none;
  border-color: var(--color-accent);
  box-shadow: 0 0 0 2px var(--color-accent-light);
}

.form-group input:disabled {
  opacity: 0.5;
}

.field-hint {
  display: block;
  font-size: 11px;
  color: var(--color-text-muted);
  margin-top: 4px;
}

.form-row {
  display: flex;
  gap: 12px;
}

.form-row .form-group {
  flex: 1;
}

.form-row .form-group.port {
  flex: 1;
}

.type-selector {
  display: flex;
  gap: 8px;
}

.type-btn {
  flex: 1;
  height: 40px;
  font-size: 16px;
  font-weight: 500;
  color: var(--color-text);
  background: transparent;
  border: 0.8px solid var(--color-border);
  border-radius: 4px;
  transition: all 0.12s;
}

.type-btn:hover:not(:disabled) {
  border-color: var(--color-text-muted);
}

.type-btn.active {
  background: rgba(43, 127, 255, 0.1);
  border-color: #2b7fff;
  color: var(--color-accent);
}

.type-btn:disabled {
  opacity: 0.5;
  cursor: default;
}

.info-box {
  padding: 10px 12px;
  background: var(--color-bg-secondary);
  border: 1px solid var(--color-border);
  border-radius: 6px;
  font-size: 12px;
  color: var(--color-text-muted);
}

.btn-primary {
  height: 40px;
  padding: 0 20px;
  background: var(--color-accent);
  color: white;
  border-radius: 4px;
  font-weight: 500;
  font-size: 16px;
  transition: background 0.12s;
}

.btn-primary:hover {
  background: var(--color-accent-hover);
}

.btn-primary:disabled {
  opacity: 0.5;
}

.btn-secondary {
  height: 40px;
  padding: 0 20px;
  background: var(--color-bg-tertiary);
  border-radius: 4px;
  font-size: 16px;
  font-weight: 500;
  color: var(--color-text);
  transition: background 0.12s;
}

.btn-secondary:hover {
  background: var(--color-border);
}

.btn-danger {
  height: 40px;
  padding: 0 20px;
  background: var(--color-danger);
  color: white;
  border-radius: 4px;
  font-weight: 500;
  font-size: 16px;
}

.confirm-title {
  font-size: 16px;
  font-weight: 600;
  margin-bottom: 8px;
}

.confirm-text {
  font-size: 13px;
  color: var(--color-text-secondary);
  line-height: 1.5;
}
</style>
