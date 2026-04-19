<script setup lang="ts">
import { ref } from "vue";
import { useRouter } from "vue-router";
import { useAccountsStore } from "@/stores/accounts";
import type { AccountConfig } from "@/lib/types";
import * as api from "@/lib/tauri";
import { open as shellOpen } from "@tauri-apps/plugin-shell";
import PasswordInput from "@/components/common/PasswordInput.vue";
import { acctColor } from "@/lib/account-colors";

const router = useRouter();
const accountsStore = useAccountsStore();
const showForm = ref(false);
const showDeleteConfirm = ref(false);
const deletingAccountId = ref<string | null>(null);
const saving = ref(false);
const error = ref<string | null>(null);
const editingAccountId = ref<string | null>(null);
const oauthStatus = ref<string | null>(null);
const oauthInProgress = ref(false);

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
  signature: "",
  jmap_auth_method: "basic",
  oidc_token_endpoint: "",
  oidc_client_id: "",
  calendar_sync_enabled: true,
});

const form = ref<AccountConfig>(defaultForm());

type AccountType = "gmail" | "imap" | "jmap" | "caldav" | "o365";
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
    case "o365":
      f.provider = "o365";
      f.mail_protocol = "graph";
      if (!editingAccountId.value) {
        f.imap_host = "outlook.office365.com";
        f.imap_port = 993;
        f.smtp_host = "smtp.office365.com";
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
    if (config.provider === "o365") {
      accountType.value = "o365";
      try {
        const hasTokens = await api.oauthHasTokens(id);
        if (hasTokens) {
          oauthStatus.value = "Signed in with Microsoft";
        } else {
          oauthStatus.value = null;
        }
      } catch { oauthStatus.value = null; }
    } else if (config.provider === "gmail") {
      accountType.value = "gmail";
      try {
        const hasTokens = await api.oauthHasTokens(id);
        if (hasTokens) {
          oauthStatus.value = "Signed in with Google";
        } else {
          oauthStatus.value = null;
        }
      } catch { oauthStatus.value = null; }
    } else if (config.mail_protocol === "jmap") {
      accountType.value = "jmap";
      if (config.jmap_auth_method === "oidc") {
        try {
          const hasTokens = await api.oauthHasTokens(id);
          if (hasTokens) {
            oauthStatus.value = "Signed in via OIDC";
          } else {
            oauthStatus.value = null;
          }
        } catch { oauthStatus.value = null; }
      } else {
        oauthStatus.value = null;
      }
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
    // Default username to email if not set (Gmail and most IMAP servers use email as username)
    if (!form.value.username.trim()) {
      form.value.username = form.value.email;
    }
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

async function startGoogleOAuth() {
  oauthInProgress.value = true;
  oauthStatus.value = null;
  error.value = null;

  try {
    // Generate a temporary account ID if creating new
    const tempAccountId = editingAccountId.value ?? `gmail-pending-${Date.now()}`;

    // Start OAuth flow — get auth URL
    const { url, port } = await api.oauthStart("google");

    // Open browser
    await shellOpen(url);

    // Wait for callback (this blocks until user completes in browser)
    await api.oauthComplete("google", port, tempAccountId);

    // Store the temp ID so saveAccount can use it
    form.value.password = `oauth2:${tempAccountId}`;
    oauthStatus.value = "Signed in with Google";
  } catch (e) {
    error.value = `Google sign-in failed: ${e}`;
  } finally {
    oauthInProgress.value = false;
  }
}

async function startMicrosoftOAuth() {
  oauthInProgress.value = true;
  oauthStatus.value = null;
  error.value = null;

  try {
    const tempAccountId = editingAccountId.value ?? `o365-pending-${Date.now()}`;

    const { url, port } = await api.oauthStart("microsoft");
    await shellOpen(url);
    await api.oauthComplete("microsoft", port, tempAccountId);

    // Auto-fill display name and email from Microsoft Graph /me
    try {
      const profile = await api.oauthGetMsProfile(tempAccountId) as { display_name: string; email: string; login_email: string };
      if (profile.display_name) form.value.display_name = profile.display_name;
      if (profile.email) form.value.email = profile.email;
      // Set username to the Microsoft login identity (needed for IMAP XOAUTH2)
      if (profile.login_email) form.value.username = profile.login_email;
    } catch (e) {
      console.error("Failed to fetch Microsoft profile:", e);
    }

    form.value.password = `oauth2:${tempAccountId}`;
    oauthStatus.value = "Signed in with Microsoft";
  } catch (e) {
    error.value = `Microsoft sign-in failed: ${e}`;
  } finally {
    oauthInProgress.value = false;
  }
}

const oidcUserCode = ref<string | null>(null);

async function startJmapOidc() {
  oauthInProgress.value = true;
  oauthStatus.value = null;
  oidcUserCode.value = null;
  error.value = null;

  try {
    const tempAccountId = editingAccountId.value ?? `jmap-oidc-pending-${Date.now()}`;

    // Start device flow — passes existing client_id (empty for first-time setup)
    const result = await api.jmapOidcStart(
      form.value.jmap_url,
      form.value.email,
      form.value.oidc_client_id,
    );

    // Save token endpoint and client_id for account creation
    form.value.oidc_token_endpoint = result.token_endpoint;
    form.value.oidc_client_id = result.client_id;

    // Show the user code and open browser to verification URL
    oidcUserCode.value = result.user_code;
    const openUrl = result.verification_uri_complete ?? result.verification_uri;
    if (!openUrl.startsWith("https://") && !openUrl.startsWith("http://")) {
      throw new Error(`Unexpected verification URL scheme: ${openUrl}`);
    }
    await shellOpen(openUrl);

    // Poll until user completes authorization (this blocks)
    await api.jmapOidcComplete(
      result.device_code,
      result.token_endpoint,
      result.interval,
      result.expires_in,
      tempAccountId,
      result.client_id,
    );

    // Only set oauth2: marker for new accounts (triggers token migration in add_account).
    // On re-auth of existing accounts, keep password empty so save doesn't overwrite keyring.
    if (!editingAccountId.value) {
      form.value.password = `oauth2:${tempAccountId}`;
    }
    form.value.jmap_auth_method = "oidc";
    oidcUserCode.value = null;
    oauthStatus.value = "Signed in via OIDC";
  } catch (e) {
    error.value = `OIDC sign-in failed: ${e}`;
    oidcUserCode.value = null;
  } finally {
    oauthInProgress.value = false;
  }
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
          v-for="account in accountsStore.accounts"
          :key="account.id"
          class="account-card"
          :style="{ '--acct-color': acctColor(account.id).fill } as Record<string, string>"
        >
          <div class="account-card-left">
            <span class="account-avatar" :style="{ background: acctColor(account.id).fill }">
              {{ getInitials(account.display_name) }}
            </span>
            <div class="account-card-info">
              <span class="account-card-name">{{ account.display_name }}</span>
              <span class="account-card-email">{{ account.email }}</span>
              <span class="account-card-type" :style="{ color: acctColor(account.id).fill }">{{ account.provider === 'gmail' ? 'Gmail' : account.provider === 'o365' ? 'Microsoft 365' : account.mail_protocol.toUpperCase() }}</span>
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
                  v-for="t in (['gmail', 'o365', 'imap', 'jmap', 'caldav'] as AccountType[])"
                  :key="t"
                  class="type-btn"
                  :class="{ active: accountType === t }"
                  :disabled="!!editingAccountId"
                  @click="selectAccountType(t)"
                >{{ t === 'gmail' ? 'Gmail' : t === 'o365' ? 'Microsoft 365' : t.toUpperCase() }}</button>
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
            <div v-if="accountType !== 'o365' && !(accountType === 'jmap' && form.jmap_auth_method === 'oidc')" class="form-group">
              <label>{{ accountType === 'gmail' ? 'App Password' : 'Password' }}</label>
              <PasswordInput
                v-model="form.password"
                :placeholder="editingAccountId ? 'Leave empty to keep current password' : (accountType === 'gmail' ? 'Gmail app password (for IMAP/SMTP)' : '••••••••')"
              />
              <span class="field-hint">Passwords are stored securely in your OS keyring</span>
            </div>

            <template v-if="accountType === 'gmail'">
              <div class="form-group">
                <label>Calendar &amp; Contacts Sync</label>
                <div v-if="oauthStatus" class="oauth-row">
                  <div class="oauth-status">
                    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#00a63e" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12" /></svg>
                    {{ oauthStatus }}
                  </div>
                  <button class="btn-reauth" @click="oauthStatus = null">Sign in again</button>
                </div>
                <button
                  v-else
                  class="btn-oauth"
                  :disabled="oauthInProgress"
                  @click="startGoogleOAuth"
                >
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none">
                    <path d="M22.56 12.25c0-.78-.07-1.53-.2-2.25H12v4.26h5.92a5.06 5.06 0 0 1-2.2 3.32v2.77h3.57c2.08-1.92 3.28-4.74 3.28-8.1z" fill="#4285F4"/>
                    <path d="M12 23c2.97 0 5.46-.98 7.28-2.66l-3.57-2.77c-.98.66-2.23 1.06-3.71 1.06-2.86 0-5.29-1.93-6.16-4.53H2.18v2.84C3.99 20.53 7.7 23 12 23z" fill="#34A853"/>
                    <path d="M5.84 14.09c-.22-.66-.35-1.36-.35-2.09s.13-1.43.35-2.09V7.07H2.18A10.96 10.96 0 0 0 1 12c0 1.77.42 3.45 1.18 4.93l3.66-2.84z" fill="#FBBC05"/>
                    <path d="M12 5.38c1.62 0 3.06.56 4.21 1.64l3.15-3.15C17.45 2.09 14.97 1 12 1 7.7 1 3.99 3.47 2.18 7.07l3.66 2.84c.87-2.6 3.3-4.53 6.16-4.53z" fill="#EA4335"/>
                  </svg>
                  {{ oauthInProgress ? "Waiting for browser..." : "Sign in with Google" }}
                </button>
                <span class="field-hint">Sign in to sync Google Calendar and Contacts. IMAP/SMTP uses app password above.</span>
              </div>
            </template>

            <template v-if="accountType === 'o365'">
              <div class="form-group">
                <label>Microsoft 365 Sign In</label>
                <div v-if="oauthStatus" class="oauth-row">
                  <div class="oauth-status">
                    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#00a63e" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12" /></svg>
                    {{ oauthStatus }}
                  </div>
                  <button class="btn-reauth" @click="oauthStatus = null">Sign in again</button>
                </div>
                <button
                  v-else
                  class="btn-oauth"
                  :disabled="oauthInProgress"
                  @click="startMicrosoftOAuth"
                >
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none">
                    <rect x="1" y="1" width="10" height="10" fill="#F25022"/>
                    <rect x="13" y="1" width="10" height="10" fill="#7FBA00"/>
                    <rect x="1" y="13" width="10" height="10" fill="#00A4EF"/>
                    <rect x="13" y="13" width="10" height="10" fill="#FFB900"/>
                  </svg>
                  {{ oauthInProgress ? "Waiting for browser..." : "Sign in with Microsoft" }}
                </button>
                <span class="field-hint">Sign in to access mail, calendar, and contacts via Microsoft Graph API.</span>
              </div>
            </template>

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
                <label>Authentication</label>
                <div class="type-selector">
                  <button
                    class="type-btn"
                    :class="{ active: form.jmap_auth_method === 'basic' }"
                    :disabled="!!editingAccountId"
                    @click="form.jmap_auth_method = 'basic'; oauthStatus = null"
                  >Password</button>
                  <button
                    class="type-btn"
                    :class="{ active: form.jmap_auth_method === 'oidc' }"
                    :disabled="!!editingAccountId"
                    @click="form.jmap_auth_method = 'oidc'"
                  >OIDC</button>
                </div>
              </div>
              <div class="form-group">
                <label>JMAP URL</label>
                <input v-model="form.jmap_url" type="url" placeholder="https://mail.example.com" />
                <span class="field-hint">Leave blank for auto-discovery via .well-known/jmap</span>
              </div>
              <template v-if="form.jmap_auth_method === 'oidc'">
                <div class="form-group">
                  <label>OIDC Sign In</label>
                  <div v-if="oauthStatus" class="oauth-row">
                    <div class="oauth-status">
                      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#00a63e" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12" /></svg>
                      {{ oauthStatus }}
                    </div>
                    <button class="btn-reauth" @click="oauthStatus = null">Sign in again</button>
                  </div>
                  <div v-else-if="oidcUserCode" class="oidc-device-code">
                    <p class="device-code-label">Enter this code in your browser:</p>
                    <p class="device-code-value">{{ oidcUserCode }}</p>
                    <p class="device-code-hint">Waiting for authorization...</p>
                  </div>
                  <button
                    v-else
                    class="btn-oauth"
                    :disabled="oauthInProgress || !form.email"
                    @click="startJmapOidc"
                  >
                    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                      <rect x="3" y="11" width="18" height="11" rx="2" ry="2" /><path d="M7 11V7a5 5 0 0 1 10 0v4" />
                    </svg>
                    {{ oauthInProgress ? "Starting..." : "Sign in with OIDC" }}
                  </button>
                  <span class="field-hint">Opens your browser to authenticate with your identity provider.</span>
                </div>
              </template>
            </template>

            <template v-if="accountType === 'imap' || accountType === 'caldav'">
              <div class="form-group">
                <label>CalDAV URL {{ accountType === 'caldav' ? '' : '(optional)' }}</label>
                <input v-model="form.caldav_url" type="url" placeholder="https://mail.example.com/dav/cal" />
              </div>
            </template>

            <template v-if="accountType === 'gmail' && !editingAccountId">
              <div class="info-box">Gmail uses IMAP (imap.gmail.com:993) and SMTP (smtp.gmail.com:587). Sign in with Google above to authorize access.</div>
            </template>

            <div class="form-group">
              <label>Email Signature</label>
              <textarea
                v-model="form.signature"
                class="signature-textarea"
                rows="4"
                placeholder="-- &#10;Your Name&#10;Your Title"
              ></textarea>
            </div>

            <div class="form-group form-group-checkbox">
              <label class="checkbox-label">
                <input
                  v-model="form.calendar_sync_enabled"
                  type="checkbox"
                  data-testid="calendar-sync-enabled"
                />
                Enable calendar sync for this account
              </label>
              <p class="form-help">
                When off, calendars and events are not fetched from the server. Existing calendar data remains available offline.
              </p>
            </div>
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
  padding: 14px 16px;
  border: 1px solid var(--color-border);
  border-left: 4px solid var(--acct-color, var(--color-accent));
  border-radius: var(--radius);
  background: var(--color-bg-secondary);
  box-shadow: var(--shadow-sm);
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
  color: #c2410c; /* warm red per PATCHES §9, not raw danger */
}

.icon-btn-sm.danger:hover {
  background: rgba(194, 65, 12, 0.08);
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
  background: var(--color-accent-light);
  border-color: var(--color-accent);
  color: var(--color-accent);
}

.type-btn:disabled {
  opacity: 0.5;
  cursor: default;
}

.signature-textarea {
  width: 100%;
  padding: 8px 10px;
  font-size: 13px;
  font-family: 'Liberation Mono', monospace;
  border: 1px solid var(--color-border);
  border-radius: 6px;
  background: var(--color-bg);
  color: var(--color-text);
  resize: vertical;
}

.signature-textarea:focus {
  outline: none;
  border-color: var(--color-accent);
}

.info-box {
  padding: 10px 12px;
  background: var(--color-bg-secondary);
  border: 1px solid var(--color-border);
  border-radius: 6px;
  font-size: 12px;
  color: var(--color-text-muted);
}

.form-group-checkbox .checkbox-label {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 13px;
  font-weight: 500;
  color: var(--color-text);
  margin-bottom: 4px;
}

.form-group-checkbox .checkbox-label input[type="checkbox"] {
  width: auto;
  height: auto;
  margin: 0;
}

.form-group-checkbox .form-help {
  margin: 0 0 0 24px;
  font-size: 12px;
  color: var(--color-text-muted);
  line-height: 1.4;
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

.btn-oauth {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 40px;
  padding: 0 20px;
  background: var(--color-bg-secondary);
  border: 0.8px solid var(--color-border);
  border-radius: 4px;
  font-size: 14px;
  font-weight: 500;
  color: var(--color-text);
  transition: all 0.12s;
  width: 100%;
  justify-content: center;
}

.btn-oauth:hover {
  background: var(--color-bg-secondary);
  border-color: var(--color-text-muted);
}

.btn-oauth:disabled {
  opacity: 0.6;
}

.oauth-row {
  display: flex;
  align-items: center;
  gap: 8px;
}

.oauth-status {
  display: flex;
  align-items: center;
  gap: 6px;
  height: 40px;
  padding: 0 12px;
  background: rgba(0, 166, 62, 0.06);
  border-radius: 4px;
  font-size: 14px;
  font-weight: 500;
  color: #00a63e;
  flex: 1;
}

.btn-reauth {
  height: 40px;
  padding: 0 12px;
  border: 0.8px solid var(--color-border);
  border-radius: 4px;
  font-size: 13px;
  font-weight: 500;
  color: var(--color-text-secondary);
  white-space: nowrap;
  transition: all 0.12s;
}

.btn-reauth:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
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

.oidc-device-code {
  text-align: center;
  padding: 16px;
  border: 1px solid var(--color-border);
  border-radius: 8px;
  background: var(--color-bg-secondary);
}

.device-code-label {
  font-size: 13px;
  color: var(--color-text-secondary);
  margin-bottom: 8px;
}

.device-code-value {
  font-size: 28px;
  font-weight: 700;
  font-family: 'Liberation Mono', monospace;
  letter-spacing: 4px;
  color: var(--color-accent);
  margin-bottom: 8px;
}

.device-code-hint {
  font-size: 12px;
  color: var(--color-text-muted);
}
</style>
