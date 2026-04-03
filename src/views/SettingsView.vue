<script setup lang="ts">
import { ref } from "vue";
import { useRouter } from "vue-router";
import { useAccountsStore } from "@/stores/accounts";
import type { AccountConfig } from "@/lib/types";

const router = useRouter();
const accountsStore = useAccountsStore();
const showForm = ref(false);
const saving = ref(false);
const error = ref<string | null>(null);

const form = ref<AccountConfig>({
  display_name: "",
  email: "",
  provider: "gmail",
  imap_host: "imap.gmail.com",
  imap_port: 993,
  smtp_host: "smtp.gmail.com",
  smtp_port: 587,
  username: "",
  password: "",
  use_tls: true,
});

function applyGmailPreset() {
  form.value.provider = "gmail";
  form.value.imap_host = "imap.gmail.com";
  form.value.imap_port = 993;
  form.value.smtp_host = "smtp.gmail.com";
  form.value.smtp_port = 587;
  form.value.use_tls = true;
}

async function saveAccount() {
  saving.value = true;
  error.value = null;
  try {
    await accountsStore.addAccount(form.value);
    showForm.value = false;
    // Navigate to mail view to see sync progress
    router.push("/");
  } catch (e) {
    error.value = String(e);
  } finally {
    saving.value = false;
  }
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
          <span>{{ account.display_name }} ({{ account.email }})</span>
          <button class="btn-danger" @click="accountsStore.deleteAccount(account.id)">Remove</button>
        </div>
        <div v-if="accountsStore.accounts.length === 0" class="empty">
          No accounts configured
        </div>
      </div>

      <button class="btn-primary" @click="showForm = true; applyGmailPreset()">
        Add Account
      </button>

      <div v-if="showForm" class="account-form">
        <h3>Add Email Account</h3>
        <div v-if="error" class="error">{{ error }}</div>
        <div class="form-group">
          <label>Display Name</label>
          <input v-model="form.display_name" type="text" placeholder="My Gmail" />
        </div>
        <div class="form-group">
          <label>Email</label>
          <input v-model="form.email" type="email" placeholder="you@gmail.com" />
        </div>
        <div class="form-group">
          <label>Username (usually same as email)</label>
          <input v-model="form.username" type="text" placeholder="you@gmail.com" />
        </div>
        <div class="form-group">
          <label>App Password</label>
          <input v-model="form.password" type="password" placeholder="Gmail app password" />
        </div>
        <div class="form-group">
          <label>IMAP Host</label>
          <input v-model="form.imap_host" type="text" />
        </div>
        <div class="form-row">
          <div class="form-group">
            <label>IMAP Port</label>
            <input v-model.number="form.imap_port" type="number" />
          </div>
          <div class="form-group">
            <label>SMTP Host</label>
            <input v-model="form.smtp_host" type="text" />
          </div>
          <div class="form-group">
            <label>SMTP Port</label>
            <input v-model.number="form.smtp_port" type="number" />
          </div>
        </div>
        <div class="form-actions">
          <button class="btn-primary" :disabled="saving" @click="saveAccount">
            {{ saving ? "Saving..." : "Save" }}
          </button>
          <button class="btn-secondary" @click="showForm = false">Cancel</button>
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

.account-form {
  margin-top: 16px;
  padding: 16px;
  border: 1px solid var(--color-border);
  border-radius: 8px;
  background: var(--color-bg-secondary);
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
</style>
