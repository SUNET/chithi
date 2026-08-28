<script setup lang="ts">
import { ref, computed, onMounted, watch } from "vue";
import { useRoute } from "vue-router";
import { storeToRefs } from "pinia";
import { useAccountsStore } from "@/stores/accounts";
import { usePlatformStore } from "@/stores/platform";
import { useUiStore } from "@/stores/ui";
import { acctColor } from "@/lib/account-colors";
import * as api from "@/lib/tauri";
import MobileAppBar from "@/components/mobile/MobileAppBar.vue";
import AccountTypePicker from "@/components/settings/AccountTypePicker.vue";
import AccountFormModal from "@/components/settings/AccountFormModal.vue";
import {
  accountSecondaryLabel,
  accountTypeLabel,
  type AccountType,
} from "@/lib/account-types";

const route = useRoute();
const accountsStore = useAccountsStore();
const platformStore = usePlatformStore();
const uiStore = useUiStore();
const { isMobile } = storeToRefs(platformStore);
const viewMounted = ref(false);
onMounted(() => { viewMounted.value = true; });

// Mobile toggles — persist to localStorage so they survive reloads.
const blockRemoteImages = ref(localStorage.getItem("chithi-block-remote-images") !== "false");
function setBlockRemoteImages(v: boolean) {
  blockRemoteImages.value = v;
  localStorage.setItem("chithi-block-remote-images", String(v));
}

const themeLabel = computed(() =>
  uiStore.theme === "dark" ? "Dark" : "Light",
);

// The add/edit form lives in AccountFormModal (#166); the view only
// drives it through the exposed openNew / openEdit handles.
const formModal = ref<InstanceType<typeof AccountFormModal> | null>(null);

// First step of "Add Account": pick a type. Replaces the cramped
// in-modal tab row with a dialog that lists every supported
// account type as cards, and on pick opens the account form
// pre-set to that type. Edit-existing skips this step. (#148 cleanup)
const showPicker = ref(false);
const showDeleteConfirm = ref(false);
const deletingAccountId = ref<string | null>(null);
const abandonZoomAcknowledged = ref(false);
const deletingZoomAccount = ref(false);
const deletingVisioAccount = ref(false);
const deleteProviderLoading = ref(false);
const deleteProviderLookupFailed = ref(false);

function getInitials(name: string): string {
  const words = name.split(/\s+/);
  if (words.length >= 2) return (words[0][0] + words[1][0]).toUpperCase();
  return name.slice(0, 2).toUpperCase();
}

function openNewForm() {
  showPicker.value = true;
}

/// Step 2 of the Add-account flow: type is chosen, hand off to the
/// form modal. Closes the picker.
function pickAccountType(type: AccountType) {
  showPicker.value = false;
  formModal.value?.openNew(type);
}

function cancelPicker() {
  showPicker.value = false;
}

function openEditForm(id: string) {
  formModal.value?.openEdit(id);
}

async function confirmDelete(id: string) {
  abandonZoomAcknowledged.value = false;
  deletingAccountId.value = id;
  deletingZoomAccount.value = accountsStore.accounts.find(
    (account) => account.id === id,
  )?.meet_protocol === "zoom";
  deletingVisioAccount.value = accountsStore.accounts.find(
    (account) => account.id === id,
  )?.meet_protocol === "visio";
  deleteProviderLoading.value = true;
  deleteProviderLookupFailed.value = false;
  showDeleteConfirm.value = true;

  try {
    const config = await api.getAccountConfig(id);
    if (showDeleteConfirm.value && deletingAccountId.value === id) {
      deletingZoomAccount.value = config.meet_protocol === "zoom";
      deletingVisioAccount.value = config.meet_protocol === "visio";
    }
  } catch {
    // The account summary remains the fallback if config loading fails.
    if (showDeleteConfirm.value && deletingAccountId.value === id) {
      deleteProviderLookupFailed.value =
        !deletingZoomAccount.value && !deletingVisioAccount.value;
    }
  } finally {
    if (showDeleteConfirm.value && deletingAccountId.value === id) {
      deleteProviderLoading.value = false;
    }
  }
}

function closeDeleteConfirm() {
  showDeleteConfirm.value = false;
  deletingAccountId.value = null;
  abandonZoomAcknowledged.value = false;
  deletingZoomAccount.value = false;
  deletingVisioAccount.value = false;
  deleteProviderLoading.value = false;
  deleteProviderLookupFailed.value = false;
}

async function doDelete() {
  if (deleteProviderLoading.value) return;
  if (deletingAccountId.value) {
    if (deletingZoomAccount.value && abandonZoomAcknowledged.value) {
      await accountsStore.abandonZoomAccount(
        deletingAccountId.value,
        "ABANDON REMOTE ZOOM MEETINGS",
      );
    } else {
      await accountsStore.deleteAccount(deletingAccountId.value);
    }
  }
  closeDeleteConfirm();
}

// Onboarding hands off via ?addAccount=<provider>. Wait for native platform
// detection before honoring desktop-only providers; `kind` intentionally
// defaults to desktop while detection is in flight.
watch([
  viewMounted,
  () => platformStore.platformReady,
  () => route.query.addAccount,
], ([mounted, ready, want]) => {
  if (!mounted || !ready) return;
  if (typeof want !== "string") return;
  const mapped: Record<string, AccountType> = {
    jmap: "jmap",
    microsoft365: "o365",
    o365: "o365",
    gmail: "gmail",
    imap: "imap",
    caldav: "caldav",
    carddav: "carddav",
    talk: "talk",
    matrix: "matrix",
    zoom: "zoom",
    visio: "visio",
  };
  const type = mapped[want];
  if (!type) return;
  if (type === "visio" && platformStore.kind !== "desktop") return;
  // Deep-link path skips the picker and lands directly on the
  // form for the requested type — onboarding has already chosen.
  pickAccountType(type);
}, { immediate: true });
</script>

<template>
  <!-- Mobile: section-card layout with uppercase muted labels -->
  <div v-if="isMobile" class="settings-view mobile">
    <MobileAppBar large title="Settings" />

    <div class="mobile-scroll">
      <!-- Accounts -->
      <div class="section">
        <div class="section-label">Accounts</div>
        <div class="section-card">
          <div
            v-for="account in accountsStore.accounts"
            :key="account.id"
            class="mobile-account-item"
            :style="{ ['--acct-color']: acctColor(account.id).fill }"
          >
            <button
              type="button"
              class="mobile-account-row"
              :aria-label="`Edit ${account.display_name}`"
              data-testid="mobile-account-edit"
              @click="openEditForm(account.id)"
            >
              <span class="mobile-account-avatar" :style="{ background: acctColor(account.id).fill }">
                {{ getInitials(account.display_name) }}
              </span>
              <span class="mobile-account-info">
                <span class="mobile-account-name">{{ account.display_name }}</span>
                <span class="mobile-account-email">{{ accountSecondaryLabel(account) }}</span>
                <span class="mobile-account-type" :style="{ color: acctColor(account.id).fill }">
                  {{ accountTypeLabel(account) }}
                </span>
              </span>
              <svg class="mobile-row-chevron" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
                <polyline points="9 18 15 12 9 6" />
              </svg>
            </button>
            <button
              type="button"
              class="mobile-account-delete"
              :aria-label="`Delete ${account.display_name}`"
              data-testid="mobile-account-delete"
              @click="confirmDelete(account.id)"
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
                <polyline points="3 6 5 6 21 6" />
                <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
              </svg>
              <span>Delete</span>
            </button>
          </div>
          <button class="mobile-add-account" @click="openNewForm">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
              <path d="M16 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" />
              <circle cx="8.5" cy="7" r="4" />
              <line x1="20" y1="8" x2="20" y2="14" />
              <line x1="23" y1="11" x2="17" y2="11" />
            </svg>
            <span>Add account</span>
          </button>
        </div>
      </div>

      <!-- General -->
      <div class="section">
        <div class="section-label">General</div>
        <div class="section-card">
          <button class="mobile-setting-row" @click="uiStore.setTheme(uiStore.theme === 'dark' ? 'light' : 'dark')">
            <span class="mobile-setting-label">Appearance</span>
            <span class="mobile-setting-value">{{ themeLabel }}</span>
            <svg class="mobile-row-chevron" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
              <polyline points="9 18 15 12 9 6" />
            </svg>
          </button>
          <div class="mobile-setting-row static">
            <span class="mobile-setting-label">Time format</span>
            <span class="mobile-setting-value">
              {{ uiStore.timeFormat === "auto" ? "Auto" : uiStore.timeFormat === "12" ? "12-hour" : "24-hour" }}
            </span>
          </div>
          <div class="mobile-setting-row static">
            <span class="mobile-setting-label">Default account</span>
            <span class="mobile-setting-value">
              {{ accountsStore.activeAccount()?.email ?? "—" }}
            </span>
          </div>
        </div>
      </div>

      <!-- Privacy & storage -->
      <div class="section">
        <div class="section-label">Privacy &amp; storage</div>
        <div class="section-card">
          <label class="mobile-setting-row toggle">
            <span class="mobile-setting-label">Block remote images</span>
            <input
              type="checkbox"
              class="toggle-input"
              :checked="blockRemoteImages"
              @change="setBlockRemoteImages(($event.target as HTMLInputElement).checked)"
            />
            <span class="toggle-pill" :class="{ on: blockRemoteImages }">
              <span class="toggle-thumb"></span>
            </span>
          </label>
          <div class="mobile-setting-row static">
            <span class="mobile-setting-label">Cache size</span>
            <span class="mobile-setting-value">—</span>
          </div>
        </div>
      </div>

      <!-- About -->
      <div class="section">
        <div class="section-label">About</div>
        <div class="section-card">
          <div class="mobile-setting-row static">
            <span class="mobile-setting-label">Version</span>
            <span class="mobile-setting-value">0.1.0</span>
          </div>
        </div>
      </div>
    </div>

  </div>

  <!-- Desktop -->
  <div v-else class="settings-view">
    <div class="settings-content">
      <h1 class="settings-title">Settings</h1>

      <div class="section-header">
        <h2 class="section-title">Accounts</h2>
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
              <span class="account-card-email">{{ accountSecondaryLabel(account) }}</span>
              <span class="account-card-type" :style="{ color: acctColor(account.id).fill }">{{ accountTypeLabel(account) }}</span>
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
  </div>

  <!-- Step 1 of Add Account: pick a type. (#148 cleanup) -->
  <AccountTypePicker
    :open="showPicker"
    :allow-visio="platformStore.kind === 'desktop'"
    @pick="pickAccountType"
    @cancel="cancelPicker"
  />

  <!-- Add/Edit Account Modal (shared by mobile + desktop) -->
  <AccountFormModal ref="formModal" />


    <!-- Delete Confirmation Modal -->
    <Teleport to="body">
      <div v-if="showDeleteConfirm" class="modal-overlay" @click.self="closeDeleteConfirm">
        <div class="modal modal-sm">
          <div class="modal-body">
            <h3 class="confirm-title">Delete Account</h3>
            <p class="confirm-text">Are you sure you want to delete this account? This action cannot be undone.</p>
            <div
              v-if="deletingZoomAccount"
              class="zoom-abandon-warning"
              data-testid="zoom-abandon-warning"
            >
              <strong>Remote Zoom meetings may remain.</strong>
              <p>
                If normal deletion cannot clean up Zoom, you can remove only
                the local account. Meetings already created in Zoom may remain
                active and must be removed from Zoom separately.
              </p>
              <label class="zoom-abandon-acknowledgement">
                <input
                  v-model="abandonZoomAcknowledged"
                  type="checkbox"
                  data-testid="zoom-abandon-checkbox"
                />
                I understand that remote Zoom meetings may remain
              </label>
            </div>
            <div
              v-if="deleteProviderLookupFailed"
              class="zoom-abandon-warning"
              data-testid="delete-provider-lookup-warning"
            >
              <strong>Remote resource status could not be checked.</strong>
              <p>
                The account configuration could not be loaded. Deleting the
                local account may leave remote meetings or rooms behind.
              </p>
            </div>
            <div
              v-if="deletingVisioAccount"
              class="zoom-abandon-warning"
              data-testid="visio-delete-warning"
            >
              <strong>Remote Visio rooms will remain.</strong>
              <p>
                Deleting this local account does not delete rooms already
                created in La Suite Visio. Remove them from Visio separately
                if they should no longer be available.
              </p>
            </div>
          </div>
          <div class="modal-footer">
            <button class="btn-secondary" @click="closeDeleteConfirm">Cancel</button>
            <button
              class="btn-danger"
              data-testid="delete-account-confirm"
              :disabled="deleteProviderLoading"
              @click="doDelete"
            >
              {{ deleteProviderLoading ? "Checking…" : deletingZoomAccount && abandonZoomAcknowledged ? "Delete locally" : "Delete" }}
            </button>
          </div>
        </div>
      </div>
    </Teleport>
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

.zoom-abandon-warning {
  margin-top: 16px;
  padding: 12px;
  border: 1px solid var(--color-danger);
  border-radius: 6px;
  background: rgba(194, 65, 12, 0.08);
  color: var(--color-text);
  font-size: 13px;
  line-height: 1.45;
}

.zoom-abandon-warning p {
  margin: 6px 0 10px;
  color: var(--color-text-secondary);
}

.zoom-abandon-acknowledgement {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  font-weight: 500;
}

.zoom-abandon-acknowledgement input {
  margin-top: 2px;
}

/* ============================================================
   Mobile layout
   ============================================================ */
.settings-view.mobile {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-height: 0;
  padding: 0;
  background: var(--color-bg-secondary);
  overflow: hidden;
}

.mobile-scroll {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 4px 14px 40px;
}

.section {
  margin-top: 18px;
}

.section:first-child {
  margin-top: 4px;
}

.section-label {
  padding: 0 4px 6px;
  font-size: 11px;
  font-weight: 700;
  letter-spacing: 0.6px;
  text-transform: uppercase;
  color: var(--color-text-muted);
}

.section-card {
  background: #fff;
  border: 1px solid var(--color-border);
  border-radius: 12px;
  overflow: hidden;
}

.mobile-account-item {
  display: flex;
  align-items: stretch;
  border-bottom: 1px solid var(--color-border);
  border-left: 4px solid var(--acct-color, var(--color-accent));
}

.mobile-account-item:last-of-type {
  border-bottom: 0;
}

.mobile-account-row {
  flex: 1;
  min-width: 0;
  width: 100%;
  display: flex;
  align-items: center;
  gap: 12px;
  min-height: 68px;
  padding: 10px 14px;
  border: 0;
  background: transparent;
  text-align: left;
  cursor: pointer;
}

.mobile-account-delete {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 3px;
  width: 70px;
  border: 0;
  border-left: 1px solid var(--color-border);
  background: transparent;
  color: var(--color-danger);
  font-family: inherit;
  font-size: 11px;
  font-weight: 600;
  cursor: pointer;
}

.mobile-account-delete svg {
  width: 17px;
  height: 17px;
  stroke-width: 1.7;
}

.mobile-account-delete:focus-visible,
.mobile-account-row:focus-visible {
  outline: 2px solid var(--color-accent);
  outline-offset: -2px;
}

.mobile-account-avatar {
  width: 40px;
  height: 40px;
  border-radius: 50%;
  color: #fff;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 14px;
  font-weight: 600;
  flex-shrink: 0;
}

.mobile-account-info {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 1px;
}

.mobile-account-name {
  font-size: 15px;
  font-weight: 600;
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.mobile-account-email {
  font-size: 12px;
  color: var(--color-text-muted);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.mobile-account-type {
  font-size: 10px;
  font-weight: 700;
  letter-spacing: 0.5px;
}

.mobile-row-chevron {
  width: 16px;
  height: 16px;
  flex-shrink: 0;
  stroke-width: 1.8;
  color: var(--color-text-muted);
}

.mobile-add-account {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 8px;
  width: 100%;
  height: 44px;
  background: var(--color-accent-light);
  border: 1px dashed var(--color-accent);
  color: var(--color-accent);
  font-family: inherit;
  font-size: 14px;
  font-weight: 600;
  cursor: pointer;
}

.mobile-add-account svg {
  width: 16px;
  height: 16px;
  stroke-width: 1.8;
}

.mobile-setting-row {
  display: flex;
  align-items: center;
  width: 100%;
  min-height: 44px;
  padding: 0 14px;
  background: transparent;
  border: 0;
  border-bottom: 1px solid var(--color-border-soft, var(--color-border));
  text-align: left;
  cursor: pointer;
  font-family: inherit;
  font-size: 14px;
  color: var(--color-text);
}

.mobile-setting-row:last-child {
  border-bottom: 0;
}

.mobile-setting-row.static {
  cursor: default;
}

.mobile-setting-row.toggle {
  position: relative;
  cursor: pointer;
}

.mobile-setting-label {
  flex: 1;
  min-width: 0;
}

.mobile-setting-value {
  flex-shrink: 0;
  color: var(--color-text-muted);
  font-size: 13px;
}

.toggle-input {
  position: absolute;
  opacity: 0;
  pointer-events: none;
}

.toggle-pill {
  position: relative;
  width: 46px;
  height: 28px;
  border-radius: 999px;
  background: var(--color-border);
  transition: background 0.18s;
  flex-shrink: 0;
}

.toggle-pill.on {
  background: var(--color-accent);
}

.toggle-thumb {
  position: absolute;
  top: 2px;
  left: 2px;
  width: 24px;
  height: 24px;
  border-radius: 50%;
  background: #fff;
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.2);
  transition: transform 0.18s cubic-bezier(.2,.8,.2,1);
}

.toggle-pill.on .toggle-thumb {
  transform: translateX(18px);
}

/* ============================================================
   Edit-account modal: sheet presentation on mobile (§13)
   ============================================================ */
@media (max-width: 720px) {
  .modal-overlay {
    align-items: flex-end;
    background: rgba(20, 14, 6, 0.4);
  }

  .modal {
    width: 100%;
    max-width: 100%;
    height: calc(100vh - 48px);
    max-height: calc(100vh - 48px);
    border-bottom-left-radius: 0;
    border-bottom-right-radius: 0;
    border-top-left-radius: var(--radius-sheet, 16px);
    border-top-right-radius: var(--radius-sheet, 16px);
    box-shadow: var(--shadow-sheet, 0 -12px 30px rgba(30, 20, 10, 0.18));
    position: relative;
  }

  /* Grabber at the top of the sheet. */
  .modal::before {
    content: "";
    display: block;
    width: 38px;
    height: 5px;
    border-radius: 100px;
    background: var(--color-border);
    margin: 8px auto 4px;
    flex-shrink: 0;
  }

  .modal-header {
    justify-content: center;
    padding: 4px 16px 10px;
  }

  .modal-header h3 {
    font-size: 15px;
    font-weight: 600;
    flex: 1;
    text-align: center;
  }

  .modal-close {
    position: absolute;
    top: 16px;
    right: 12px;
  }

  /* Dedicated "Remove account" action mobile pattern (§13 footer). */
  .btn-danger {
    background: #fff;
    color: #8a3a24;
    border: 1px solid #d4a89a;
  }
}
</style>
