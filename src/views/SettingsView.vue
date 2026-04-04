<script setup lang="ts">
import { ref } from "vue";
import { useRouter } from "vue-router";
import { useAccountsStore } from "@/stores/accounts";
import type { AccountConfig } from "@/lib/types";
import * as api from "@/lib/tauri";

const router = useRouter();
const accountsStore = useAccountsStore();
const showForm = ref(false);
const saving = ref(false);
const error = ref<string | null>(null);
const editingAccountId = ref<string | null>(null);

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
      f.mail_protocol = "imap"; // CalDAV-only, no email
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
    // Determine account type from config
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
</script>

<template>
  <div class="settings-view">
    <div class="settings-content">
      <h2>Accounts</h2>
      <div class="account-list">
        <div
          v-for="account in accountsStore.accounts"
          :key="account.id"
          class="account-item"
        >
          <div class="account-info">
            <span class="account-name">{{ account.display_name }}</span>
            <span class="account-email">{{ account.email }}</span>
            <span class="account-protocol">{{ account.mail_protocol.toUpperCase() }}</span>
          </div>
          <div class="account-actions">
            <button class="btn-edit" @click="openEditForm(account.id)">Edit</button>
            <button class="btn-danger" @click="accountsStore.deleteAccount(account.id)">Remove</button>
          </div>
        </div>
        <div v-if="accountsStore.accounts.length === 0" class="empty">
          No accounts configured
        </div>
      </div>

      <button class="btn-primary" @click="openNewForm">Add Account</button>

      <div v-if="showForm" class="account-form">
        <h3>{{ editingAccountId ? 'Edit Account' : 'Add Email Account' }}</h3>
        <div v-if="error" class="error">{{ error }}</div>

        <!-- Account type selector (disabled when editing) -->
        <div class="type-selector">
          <button
            class="type-btn"
            :class="{ active: accountType === 'gmail' }"
            :disabled="!!editingAccountId"
            @click="selectAccountType('gmail')"
          >Gmail</button>
          <button
            class="type-btn"
            :class="{ active: accountType === 'imap' }"
            :disabled="!!editingAccountId"
            @click="selectAccountType('imap')"
          >IMAP</button>
          <button
            class="type-btn"
            :class="{ active: accountType === 'jmap' }"
            :disabled="!!editingAccountId"
            @click="selectAccountType('jmap')"
          >JMAP</button>
          <button
            class="type-btn"
            :class="{ active: accountType === 'caldav' }"
            :disabled="!!editingAccountId"
            @click="selectAccountType('caldav')"
          >CalDAV</button>
        </div>

        <!-- Common fields -->
        <div class="form-group">
          <label>Display Name</label>
          <input v-model="form.display_name" type="text" :placeholder="accountType === 'caldav' ? 'My Calendar' : 'My Email'" />
        </div>
        <div v-if="accountType !== 'caldav'" class="form-group">
          <label>Email Address</label>
          <input v-model="form.email" type="email" placeholder="you@example.com" />
        </div>
        <div class="form-group">
          <label>Username</label>
          <input v-model="form.username" type="text" placeholder="you@example.com" />
        </div>
        <div class="form-group">
          <label>{{ accountType === 'gmail' ? 'App Password' : 'Password' }}</label>
          <input v-model="form.password" type="password" :placeholder="accountType === 'gmail' ? 'Gmail app password' : 'Password'" />
        </div>

        <!-- IMAP/SMTP fields (not for CalDAV-only or JMAP) -->
        <template v-if="accountType === 'imap' || accountType === 'gmail'">
          <div class="form-row">
            <div class="form-group">
              <label>IMAP Host</label>
              <input v-model="form.imap_host" type="text" placeholder="imap.example.com" :disabled="accountType === 'gmail'" />
            </div>
            <div class="form-group">
              <label>IMAP Port</label>
              <input v-model.number="form.imap_port" type="number" :disabled="accountType === 'gmail'" />
            </div>
          </div>
          <div class="form-row">
            <div class="form-group">
              <label>SMTP Host</label>
              <input v-model="form.smtp_host" type="text" placeholder="smtp.example.com" :disabled="accountType === 'gmail'" />
            </div>
            <div class="form-group">
              <label>SMTP Port</label>
              <input v-model.number="form.smtp_port" type="number" :disabled="accountType === 'gmail'" />
            </div>
          </div>
        </template>

        <!-- JMAP fields -->
        <template v-if="accountType === 'jmap'">
          <div class="form-group">
            <label>JMAP URL (leave blank for auto-discovery from email domain)</label>
            <input v-model="form.jmap_url" type="url" placeholder="https://mail.example.com" />
          </div>
          <p class="hint">If left blank, the app will try to discover the JMAP endpoint from your email domain via <code>.well-known/jmap</code>.</p>
        </template>

        <!-- CalDAV URL for IMAP accounts (optional) and CalDAV-only accounts (required) -->
        <template v-if="accountType === 'imap' || accountType === 'caldav'">
          <div class="form-group">
            <label>CalDAV URL {{ accountType === 'caldav' ? '(required)' : '(optional — for calendar sync)' }}</label>
            <input v-model="form.caldav_url" type="url" placeholder="https://mail.example.com/dav/cal" />
          </div>
          <p v-if="accountType === 'caldav'" class="hint">CalDAV-only account — only calendar data will be synced, no email.</p>
          <p v-else class="hint">If set, calendars will be synced via CalDAV. Leave blank for email-only.</p>
        </template>

        <!-- CalDAV-only: hide email field, make it optional -->

        <!-- Gmail hint -->
        <template v-if="accountType === 'gmail' && !editingAccountId">
          <p class="hint">Gmail uses IMAP (imap.gmail.com:993) and SMTP (smtp.gmail.com:587). You need a <a href="https://myaccount.google.com/apppasswords" class="link">Gmail App Password</a>.</p>
        </template>

        <div class="form-actions">
          <button class="btn-primary" :disabled="saving" @click="saveAccount">
            {{ saving ? "Saving..." : (editingAccountId ? "Update" : "Save") }}
          </button>
          <button class="btn-secondary" @click="cancelForm">Cancel</button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.settings-view {
  height: 100%;
  overflow-y: auto;
  padding: 24px;
}

.settings-content {
  max-width: 600px;
}

h2 {
  margin-bottom: 16px;
}

h3 {
  margin-bottom: 12px;
}

.account-list {
  margin-bottom: 16px;
}

.account-item {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 8px 12px;
  border: 1px solid var(--color-border);
  border-radius: 6px;
  margin-bottom: 8px;
}

.account-info {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.account-name {
  font-weight: 600;
}

.account-email {
  font-size: 12px;
  color: var(--color-text-secondary);
}

.account-protocol {
  font-size: 10px;
  color: var(--color-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.account-actions {
  display: flex;
  gap: 6px;
}

.account-form {
  margin-top: 16px;
  padding: 16px;
  border: 1px solid var(--color-border);
  border-radius: 8px;
  background: var(--color-bg-secondary);
}

.type-selector {
  display: flex;
  gap: 0;
  margin-bottom: 16px;
  border: 1px solid var(--color-border);
  border-radius: 6px;
  overflow: hidden;
}

.type-btn {
  flex: 1;
  padding: 8px 16px;
  font-size: 13px;
  font-weight: 500;
  color: var(--color-text-secondary);
  background: var(--color-bg);
  border-right: 1px solid var(--color-border);
}

.type-btn:last-child {
  border-right: none;
}

.type-btn:hover:not(:disabled) {
  background: var(--color-bg-hover);
}

.type-btn.active {
  background: var(--color-accent);
  color: var(--color-bg);
  font-weight: 600;
}

.type-btn:disabled {
  opacity: 0.6;
  cursor: default;
}

.form-group {
  margin-bottom: 12px;
}

.form-group label {
  display: block;
  margin-bottom: 4px;
  font-size: 12px;
  color: var(--color-text-secondary);
}

.form-group input {
  width: 100%;
  padding: 6px 10px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
}

.form-group input:disabled {
  opacity: 0.5;
}

.form-row {
  display: flex;
  gap: 12px;
}

.form-row .form-group {
  flex: 1;
}

.form-actions {
  display: flex;
  gap: 8px;
  margin-top: 16px;
}

.btn-primary {
  padding: 6px 16px;
  background: var(--color-accent);
  color: var(--color-bg);
  border-radius: 6px;
  font-weight: 600;
}

.btn-primary:disabled {
  opacity: 0.5;
}

.btn-secondary {
  padding: 6px 16px;
  border: 1px solid var(--color-border);
  border-radius: 6px;
}

.btn-edit {
  padding: 4px 10px;
  font-size: 12px;
  color: var(--color-accent);
  border: 1px solid var(--color-border);
  border-radius: 4px;
}

.btn-edit:hover {
  background: var(--color-bg-hover);
}

.btn-danger {
  padding: 4px 10px;
  color: var(--color-danger);
  font-size: 12px;
}

.btn-danger:hover {
  background: rgba(243, 139, 168, 0.1);
  border-radius: 4px;
}

.error {
  padding: 8px 12px;
  background: rgba(243, 139, 168, 0.1);
  color: var(--color-danger);
  border-radius: 6px;
  margin-bottom: 12px;
}

.empty {
  color: var(--color-text-muted);
  padding: 12px;
}

.hint {
  font-size: 12px;
  color: var(--color-text-muted);
  margin-bottom: 12px;
  line-height: 1.4;
}

.hint code {
  font-family: var(--font-mono);
  background: var(--color-bg-tertiary);
  padding: 1px 4px;
  border-radius: 3px;
}

.link {
  color: var(--color-accent);
}
</style>
