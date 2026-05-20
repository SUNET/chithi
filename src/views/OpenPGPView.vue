<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from "vue";
import { storeToRefs } from "pinia";
import * as api from "@/lib/tauri";
import { usePgpStore } from "@/stores/pgp";
import type { PgpCardDetails } from "@/lib/types";

const pgpStore = usePgpStore();
const {
  cards,
  filteredKeys,
  selectedKey,
  selectedFingerprint,
  loadingKeys,
  loadingCards,
  lastError,
  searchQuery,
} = storeToRefs(pgpStore);

// Local UI state. Modal flags live in the view rather than the store so
// closing the view tears down all in-flight prompts.
const showImportModal = ref(false);
const showWkdModal = ref(false);
const armoredInput = ref("");
const wkdEmail = ref("");
const importBusy = ref(false);
const wkdBusy = ref(false);
const importError = ref<string | null>(null);
const wkdError = ref<string | null>(null);
const lastSyncSummary = ref<string | null>(null);

onMounted(async () => {
  await pgpStore.ensureListener();
  // Keys + cards in parallel — the card list is best-effort.
  await Promise.all([pgpStore.fetchKeys(), pgpStore.fetchCards()]);
});

onUnmounted(() => {
  pgpStore.disposeListener();
});

function selectKey(fp: string) {
  // Keys and cards share the right pane — selecting one clears the
  // other so the user always sees a single detail view that matches
  // the highlighted row on the left.
  selectedCardIdent.value = null;
  cardDetails.value = null;
  cardDetailsError.value = null;
  pgpStore.selectKey(fp);
}

// Card selection lives in the view (not the store) because it's
// purely a UI focus state — the underlying card data is already
// in `cards` from `pgpStore.fetchCards()`, and the per-card detail
// is fetched on demand because each call hits the card reader.
const selectedCardIdent = ref<string | null>(null);
const cardDetails = ref<PgpCardDetails | null>(null);
const cardDetailsLoading = ref(false);
const cardDetailsError = ref<string | null>(null);

async function selectCard(ident: string) {
  pgpStore.selectKey(null);
  selectedCardIdent.value = ident;
  cardDetails.value = null;
  cardDetailsError.value = null;
  cardDetailsLoading.value = true;
  try {
    cardDetails.value = await api.pgpCardDetails(ident);
  } catch (e) {
    cardDetailsError.value = e instanceof Error ? e.message : String(e);
  } finally {
    cardDetailsLoading.value = false;
  }
}

function formatRetryCounter(n: number): string {
  // libtumpa surfaces 0 when the card is locked-out on that PIN.
  return n === 0 ? "0 (locked)" : String(n);
}

function formatFingerprint(fp: string): string {
  // Full 40-char SHA-1 (v4) / 64-char SHA-256 (v6) fingerprint, grouped
  // 4-by-4 as gpg / sequoia conventionally display them.
  return fp.toUpperCase().replace(/(.{4})/g, "$1 ").trim();
}

function formatDate(iso: string | null): string {
  if (!iso) return "—";
  try {
    return new Date(iso).toLocaleDateString();
  } catch {
    return iso;
  }
}

function primaryDisplay(key: { primaryUid: string | null; fingerprint: string }) {
  return key.primaryUid?.trim() || `(no UID) ${formatFingerprint(key.fingerprint)}`;
}

// "Secret key" status is more nuanced than the libtumpa `isSecret` flag
// alone — for card-resident keys the keystore DB only stores the public
// material (`isSecret` = false) while the secret bytes live on the card
// and are usable for sign/decrypt. So we have three useful states:
//   - secret in keystore: software key, anyone with the passphrase can use
//   - on smartcard: secret on a linked card, usable when card connected
//   - public only: no software secret and no card linkage
function secretKeyStatus(key: {
  isSecret: boolean;
  cardIdents: string[];
}): string {
  if (key.isSecret && key.cardIdents.length > 0) {
    return `in keystore + smartcard (${key.cardIdents.join(", ")})`;
  }
  if (key.isSecret) return "in keystore";
  if (key.cardIdents.length > 0) {
    return `on smartcard (${key.cardIdents.join(", ")})`;
  }
  return "absent (public only)";
}

async function openFilePicker() {
  importError.value = null;
  importBusy.value = true;
  try {
    const result = await api.pgpPickAndImportKey();
    if (!result) return; // user cancelled
    // pgpPickAndImportKey doesn't go through the store, so refresh
    // ourselves and select the newly-imported key.
    await pgpStore.fetchKeys();
    pgpStore.selectKey(result.fingerprint);
    showImportModal.value = false;
    armoredInput.value = "";
  } catch (e) {
    importError.value = e instanceof Error ? e.message : String(e);
  } finally {
    importBusy.value = false;
  }
}

async function importArmored() {
  importError.value = null;
  const trimmed = armoredInput.value.trim();
  if (!trimmed) {
    importError.value = "Paste an armored PGP key first.";
    return;
  }
  importBusy.value = true;
  try {
    await pgpStore.importArmored(trimmed);
    showImportModal.value = false;
    armoredInput.value = "";
  } catch (e) {
    importError.value = e instanceof Error ? e.message : String(e);
  } finally {
    importBusy.value = false;
  }
}

async function fetchViaWkd() {
  wkdError.value = null;
  const email = wkdEmail.value.trim();
  if (!email) {
    wkdError.value = "Enter an email address to look up.";
    return;
  }
  wkdBusy.value = true;
  try {
    await pgpStore.fetchViaWkd(email);
    showWkdModal.value = false;
    wkdEmail.value = "";
  } catch (e) {
    wkdError.value = e instanceof Error ? e.message : String(e);
  } finally {
    wkdBusy.value = false;
  }
}

async function syncCards() {
  lastSyncSummary.value = null;
  try {
    const detections = await pgpStore.autoLinkCards();
    if (detections.length === 0) {
      lastSyncSummary.value =
        "No new links. Either no cards are connected or all on-card keys are already linked.";
    } else {
      const linkedKeys = new Set(detections.map((d) => d.keyFingerprint)).size;
      const linkedCards = new Set(detections.map((d) => d.cardIdent)).size;
      lastSyncSummary.value = `Linked ${linkedKeys} key(s) across ${linkedCards} card(s) (${detections.length} slot${detections.length === 1 ? "" : "s"}).`;
    }
  } catch (e) {
    lastSyncSummary.value = e instanceof Error ? e.message : String(e);
  }
}

async function exportSelected() {
  if (!selectedKey.value) return;
  try {
    const armored = await pgpStore.exportPublic(selectedKey.value.fingerprint);
    await navigator.clipboard.writeText(armored);
    lastSyncSummary.value = "Public key copied to clipboard.";
  } catch (e) {
    lastSyncSummary.value = e instanceof Error ? e.message : String(e);
  }
}

const deleteConfirmFp = ref<string | null>(null);
async function confirmDelete() {
  if (!deleteConfirmFp.value) return;
  try {
    await pgpStore.deleteKey(deleteConfirmFp.value);
  } catch (e) {
    lastSyncSummary.value = e instanceof Error ? e.message : String(e);
  } finally {
    deleteConfirmFp.value = null;
  }
}

const keystoreEmptyHint = computed(() =>
  !loadingKeys.value && filteredKeys.value.length === 0 && searchQuery.value === "",
);
</script>

<template>
  <div class="openpgp-view">
    <!-- Left pane: search + key list + cards section -->
    <aside class="left-pane">
      <div class="toolbar">
        <input
          v-model="searchQuery"
          class="search"
          type="search"
          placeholder="Search UID or fingerprint…"
          data-testid="pgp-search"
        />
        <div class="toolbar-actions">
          <button class="btn" @click="showImportModal = true" data-testid="pgp-import-btn">
            Import
          </button>
          <button class="btn" @click="showWkdModal = true" data-testid="pgp-wkd-btn">
            Fetch via WKD
          </button>
          <button class="btn" @click="syncCards" data-testid="pgp-sync-cards-btn">
            Sync cards
          </button>
        </div>
        <p v-if="lastSyncSummary" class="hint">{{ lastSyncSummary }}</p>
        <p v-if="lastError" class="error">{{ lastError }}</p>
      </div>

      <ul v-if="!keystoreEmptyHint" class="key-list" data-testid="pgp-key-list">
        <li
          v-for="key in filteredKeys"
          :key="key.fingerprint"
          class="key-item"
          :class="{ active: key.fingerprint === selectedFingerprint }"
          :data-testid="`pgp-key-${key.fingerprint}`"
          @click="selectKey(key.fingerprint)"
        >
          <div class="key-row">
            <span class="primary">{{ primaryDisplay(key) }}</span>
            <span class="badges">
              <span v-if="key.isSecret" class="badge badge-secret" title="Secret key present">S</span>
              <span v-if="key.cardIdents.length" class="badge badge-card" :title="`On card: ${key.cardIdents.join(', ')}`">C</span>
              <span v-if="key.isRevoked" class="badge badge-revoked" title="Revoked">R</span>
            </span>
          </div>
          <div class="fp">{{ formatFingerprint(key.fingerprint) }}</div>
        </li>
      </ul>
      <div v-else class="empty">
        <p>No keys yet. Use Import, Fetch via WKD, or generate one in tumpa-cli / tumpa desktop — they share this same keystore at <code>~/.tumpa/keys.db</code>.</p>
      </div>

      <section class="cards-section" v-if="!loadingCards">
        <h3>Smartcards <span class="muted" v-if="cards.length === 0">(none connected)</span></h3>
        <ul v-if="cards.length" class="card-list" data-testid="pgp-card-list">
          <li
            v-for="c in cards"
            :key="c.ident"
            class="card-item"
            :class="{ active: c.ident === selectedCardIdent }"
            :data-testid="`pgp-card-${c.ident}`"
            role="button"
            tabindex="0"
            :aria-selected="c.ident === selectedCardIdent"
            @click="selectCard(c.ident)"
            @keydown.enter.prevent="selectCard(c.ident)"
            @keydown.space.prevent="selectCard(c.ident)"
          >
            <div class="card-name">{{ c.manufacturerName }} <span class="muted">#{{ c.serialNumber }}</span></div>
            <div class="card-meta">
              <span v-if="c.cardholderName">{{ c.cardholderName }}</span>
              <span class="muted">{{ c.ident }}</span>
            </div>
          </li>
        </ul>
      </section>
    </aside>

    <!-- Right pane: key or card detail -->
    <main class="right-pane">
      <!-- Card detail takes precedence when a card is selected. -->
      <div v-if="selectedCardIdent" class="detail" data-testid="pgp-card-detail">
        <header class="detail-header">
          <h2>
            {{ cardDetails?.manufacturerName ?? "Smartcard" }}
            <span v-if="cardDetails?.serialNumber" class="muted">
              #{{ cardDetails.serialNumber }}
            </span>
          </h2>
          <code class="fp-full">{{ selectedCardIdent }}</code>
        </header>

        <p v-if="cardDetailsLoading" class="hint">Reading card…</p>
        <p v-if="cardDetailsError" class="error">{{ cardDetailsError }}</p>

        <template v-if="cardDetails && !cardDetailsError">
          <section>
            <h3>Card</h3>
            <dl class="meta">
              <dt>Cardholder</dt>
              <dd>{{ cardDetails.cardholderName ?? "—" }}</dd>
              <dt>Manufacturer</dt>
              <dd>{{ cardDetails.manufacturerName ?? "—" }}</dd>
              <dt>Serial</dt>
              <dd><code>{{ cardDetails.serialNumber }}</code></dd>
              <dt>Public key URL</dt>
              <dd>
                <a
                  v-if="cardDetails.publicKeyUrl"
                  :href="cardDetails.publicKeyUrl"
                  target="_blank"
                  rel="noopener"
                >{{ cardDetails.publicKeyUrl }}</a>
                <span v-else>—</span>
              </dd>
            </dl>
          </section>

          <section>
            <h3>On-card key slots</h3>
            <table class="subkey-table">
              <thead>
                <tr><th>Slot</th><th>Fingerprint</th></tr>
              </thead>
              <tbody>
                <tr>
                  <td>Signature</td>
                  <td>
                    <code v-if="cardDetails.signatureFingerprint">
                      {{ formatFingerprint(cardDetails.signatureFingerprint) }}
                    </code>
                    <span v-else class="muted">empty</span>
                  </td>
                </tr>
                <tr>
                  <td>Encryption</td>
                  <td>
                    <code v-if="cardDetails.encryptionFingerprint">
                      {{ formatFingerprint(cardDetails.encryptionFingerprint) }}
                    </code>
                    <span v-else class="muted">empty</span>
                  </td>
                </tr>
                <tr>
                  <td>Authentication</td>
                  <td>
                    <code v-if="cardDetails.authenticationFingerprint">
                      {{ formatFingerprint(cardDetails.authenticationFingerprint) }}
                    </code>
                    <span v-else class="muted">empty</span>
                  </td>
                </tr>
              </tbody>
            </table>
          </section>

          <section>
            <h3>Counters</h3>
            <dl class="meta">
              <dt>Signatures made</dt>
              <dd>{{ cardDetails.signatureCounter }}</dd>
              <dt>User PIN retries left</dt>
              <dd>{{ formatRetryCounter(cardDetails.pinRetryCounter) }}</dd>
              <dt>Reset code retries left</dt>
              <dd>{{ formatRetryCounter(cardDetails.resetCodeRetryCounter) }}</dd>
              <dt>Admin PIN retries left</dt>
              <dd>{{ formatRetryCounter(cardDetails.adminPinRetryCounter) }}</dd>
            </dl>
          </section>
        </template>
      </div>

      <div v-else-if="!selectedKey" class="placeholder">
        Select a key or smartcard on the left to see its details.
      </div>
      <div v-else class="detail" data-testid="pgp-key-detail">
        <header class="detail-header">
          <h2>{{ primaryDisplay(selectedKey) }}</h2>
          <code class="fp-full">{{ selectedKey.fingerprint.toUpperCase() }}</code>
        </header>

        <div class="detail-actions">
          <button class="btn" @click="exportSelected" data-testid="pgp-export-btn">
            Copy public key
          </button>
          <button
            class="btn btn-danger"
            @click="deleteConfirmFp = selectedKey.fingerprint"
            data-testid="pgp-delete-btn"
          >
            Delete
          </button>
        </div>

        <section>
          <h3>User IDs</h3>
          <ul class="uid-list">
            <li v-for="(u, i) in selectedKey.userIds" :key="i">
              <span>{{ u.uid }}</span>
              <span v-if="u.email" class="muted">({{ u.email }})</span>
            </li>
          </ul>
        </section>

        <section>
          <h3>Subkeys</h3>
          <table class="subkey-table">
            <thead>
              <tr>
                <th>Fingerprint</th>
                <th>Type</th>
                <th>Algorithm</th>
                <th>Bits</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="sk in selectedKey.subkeys" :key="sk.fingerprint">
                <td><code>{{ formatFingerprint(sk.fingerprint) }}</code></td>
                <td>{{ sk.keyType }}</td>
                <td>{{ sk.algorithm ?? "—" }}</td>
                <td>{{ sk.bitLength ?? "—" }}</td>
              </tr>
            </tbody>
          </table>
        </section>

        <section v-if="selectedKey.cardIdents.length">
          <h3>Smartcard links</h3>
          <ul>
            <li v-for="ident in selectedKey.cardIdents" :key="ident"><code>{{ ident }}</code></li>
          </ul>
        </section>

        <section>
          <h3>Status</h3>
          <dl class="meta">
            <dt>Created</dt><dd>{{ formatDate(selectedKey.creationTime) }}</dd>
            <dt>Expires</dt><dd>{{ formatDate(selectedKey.expirationTime) }}</dd>
            <dt>Secret key</dt><dd>{{ secretKeyStatus(selectedKey) }}</dd>
            <dt v-if="selectedKey.isRevoked">Revoked</dt>
            <dd v-if="selectedKey.isRevoked">{{ formatDate(selectedKey.revocationTime) }}</dd>
          </dl>
        </section>
      </div>
    </main>

    <!-- Import modal -->
    <div v-if="showImportModal" class="modal-overlay" @click.self="showImportModal = false">
      <div class="modal" role="dialog" aria-label="Import key">
        <h3>Import OpenPGP key</h3>
        <p>Paste an armored key (begins with <code>-----BEGIN PGP</code>) or pick a file.</p>
        <textarea
          v-model="armoredInput"
          rows="8"
          placeholder="-----BEGIN PGP PUBLIC KEY BLOCK-----"
          data-testid="pgp-import-textarea"
        ></textarea>
        <p v-if="importError" class="error">{{ importError }}</p>
        <div class="modal-actions">
          <button class="btn" @click="openFilePicker" :disabled="importBusy">
            Pick file…
          </button>
          <span class="spacer"></span>
          <button class="btn" @click="showImportModal = false" :disabled="importBusy">
            Cancel
          </button>
          <button
            class="btn btn-primary"
            @click="importArmored"
            :disabled="importBusy"
            data-testid="pgp-import-submit"
          >
            Import
          </button>
        </div>
      </div>
    </div>

    <!-- WKD modal -->
    <div v-if="showWkdModal" class="modal-overlay" @click.self="showWkdModal = false">
      <div class="modal" role="dialog" aria-label="Fetch via WKD">
        <h3>Fetch public key via WKD</h3>
        <p>Looks up the key on the address's domain (Web Key Directory).</p>
        <input
          v-model="wkdEmail"
          type="email"
          placeholder="alice@example.com"
          data-testid="pgp-wkd-input"
        />
        <p v-if="wkdError" class="error">{{ wkdError }}</p>
        <div class="modal-actions">
          <span class="spacer"></span>
          <button class="btn" @click="showWkdModal = false" :disabled="wkdBusy">Cancel</button>
          <button
            class="btn btn-primary"
            @click="fetchViaWkd"
            :disabled="wkdBusy"
            data-testid="pgp-wkd-submit"
          >
            Fetch
          </button>
        </div>
      </div>
    </div>

    <!-- Delete confirmation -->
    <div v-if="deleteConfirmFp" class="modal-overlay" @click.self="deleteConfirmFp = null">
      <div class="modal" role="dialog" aria-label="Confirm delete">
        <h3>Delete key?</h3>
        <p>
          This removes the key from the shared keystore. If the secret key is
          only stored here (not on a smartcard or backed up), it will be lost.
        </p>
        <code>{{ deleteConfirmFp.toUpperCase() }}</code>
        <div class="modal-actions">
          <span class="spacer"></span>
          <button class="btn" @click="deleteConfirmFp = null">Cancel</button>
          <button class="btn btn-danger" @click="confirmDelete" data-testid="pgp-delete-confirm">
            Delete
          </button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.openpgp-view {
  display: flex;
  width: 100%;
  height: 100%;
  background: var(--color-bg);
}

.left-pane {
  width: 320px;
  flex-shrink: 0;
  border-right: 0.8px solid var(--color-border);
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

.toolbar {
  padding: 12px;
  border-bottom: 0.8px solid var(--color-border);
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.search {
  width: 100%;
  padding: 6px 8px;
  border-radius: var(--radius);
  border: 0.8px solid var(--color-border);
  background: var(--color-bg);
  color: var(--color-text);
  font-size: 13px;
}

.toolbar-actions {
  display: flex;
  gap: 6px;
  flex-wrap: wrap;
}

.btn {
  padding: 4px 10px;
  border-radius: var(--radius);
  border: 0.8px solid var(--color-border);
  background: var(--color-bg);
  color: var(--color-text);
  font-size: 12px;
  cursor: pointer;
}

.btn:hover:not(:disabled) {
  background: var(--color-bg-hover);
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.btn-primary {
  background: var(--color-accent);
  color: #fff;
  border-color: var(--color-accent);
}

.btn-danger {
  color: var(--color-danger, #fb2c36);
  border-color: var(--color-danger, #fb2c36);
}

.btn-danger:hover:not(:disabled) {
  background: var(--color-danger, #fb2c36);
  color: #fff;
}

.hint {
  font-size: 12px;
  color: var(--color-text-muted);
  margin: 0;
}

.error {
  font-size: 12px;
  color: var(--color-danger, #fb2c36);
  margin: 0;
}

.key-list {
  flex: 1;
  overflow-y: auto;
  margin: 0;
  padding: 0;
  list-style: none;
}

.key-item {
  padding: 10px 12px;
  border-bottom: 0.8px solid var(--color-border);
  cursor: pointer;
}

.key-item:hover {
  background: var(--color-bg-hover);
}

.key-item.active {
  background: var(--color-bg-selected, var(--color-bg-hover));
}

.key-row {
  display: flex;
  justify-content: space-between;
  gap: 8px;
  align-items: center;
}

.primary {
  font-size: 13px;
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.badges {
  display: flex;
  gap: 4px;
  flex-shrink: 0;
}

.badge {
  display: inline-block;
  font-size: 10px;
  font-weight: 600;
  padding: 1px 5px;
  border-radius: 4px;
  background: var(--color-bg-tertiary);
  color: var(--color-text-muted);
}

.badge-secret {
  background: var(--color-accent);
  color: #fff;
}

.badge-card {
  background: #6b7280;
  color: #fff;
}

.badge-revoked {
  background: var(--color-danger, #fb2c36);
  color: #fff;
}

.fp {
  font-family: var(--font-mono, monospace);
  font-size: 11px;
  color: var(--color-text-muted);
  margin-top: 2px;
  word-break: break-all;
}

.empty {
  padding: 16px;
  color: var(--color-text-muted);
  font-size: 13px;
}

.cards-section {
  border-top: 0.8px solid var(--color-border);
  padding: 12px;
  flex-shrink: 0;
}

.cards-section h3 {
  font-size: 12px;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--color-text-muted);
  margin: 0 0 8px 0;
}

.card-list {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.card-item {
  padding: 6px 8px;
  margin: 0 -8px;
  font-size: 12px;
  cursor: pointer;
  border-radius: 4px;
}

.card-item:hover {
  background: var(--color-bg-hover);
}

.card-item.active {
  background: var(--color-bg-selected, var(--color-bg-hover));
}

.card-item:focus-visible {
  outline: 2px solid var(--color-accent, currentColor);
  outline-offset: -2px;
}

.card-name {
  color: var(--color-text);
}

.card-meta {
  color: var(--color-text-muted);
  display: flex;
  gap: 8px;
  font-size: 11px;
  margin-top: 2px;
}

.muted {
  color: var(--color-text-muted);
}

.right-pane {
  flex: 1;
  overflow-y: auto;
  padding: 24px;
}

.placeholder {
  color: var(--color-text-muted);
  font-size: 14px;
  text-align: center;
  margin-top: 80px;
}

.detail-header h2 {
  margin: 0;
  font-size: 18px;
}

.fp-full {
  display: block;
  font-family: var(--font-mono, monospace);
  font-size: 12px;
  color: var(--color-text-muted);
  margin-top: 6px;
  word-break: break-all;
}

.detail-actions {
  margin: 16px 0;
  display: flex;
  gap: 8px;
}

.detail section {
  margin-top: 20px;
}

.detail section h3 {
  font-size: 12px;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--color-text-muted);
  margin: 0 0 8px 0;
}

.uid-list {
  list-style: none;
  margin: 0;
  padding: 0;
  font-size: 13px;
}

.uid-list li {
  padding: 4px 0;
  display: flex;
  gap: 6px;
  flex-wrap: wrap;
}

.subkey-table {
  width: 100%;
  border-collapse: collapse;
  font-size: 12px;
}

.subkey-table th,
.subkey-table td {
  text-align: left;
  padding: 4px 8px;
  border-bottom: 0.8px solid var(--color-border);
}

.subkey-table th {
  color: var(--color-text-muted);
  font-weight: 500;
}

.meta {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: 4px 16px;
  font-size: 13px;
  margin: 0;
}

.meta dt {
  color: var(--color-text-muted);
}

.meta dd {
  margin: 0;
}

.modal-overlay {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.4);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 1000;
}

.modal {
  background: var(--color-bg);
  padding: 20px;
  border-radius: var(--radius);
  width: min(520px, 90vw);
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.modal h3 {
  margin: 0;
}

.modal textarea,
.modal input {
  width: 100%;
  padding: 8px;
  border-radius: var(--radius);
  border: 0.8px solid var(--color-border);
  background: var(--color-bg-secondary, var(--color-bg));
  color: var(--color-text);
  font-family: var(--font-mono, monospace);
  font-size: 12px;
  resize: vertical;
}

.modal-actions {
  display: flex;
  gap: 8px;
  align-items: center;
}

.spacer {
  flex: 1;
}
</style>
