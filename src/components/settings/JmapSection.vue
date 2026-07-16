<script setup lang="ts">
/// Generic JMAP tab: Basic/OIDC auth method selector, JMAP URL, and
/// the OIDC device-code sign-in block. The device flow itself lives in
/// the modal (it owns the pending-account id and the form's oauth2:
/// marker); this section only emits.
///
/// `form` is the modal's shared AccountConfig draft, passed by
/// reference — the auth-method buttons and URL input write into it.
/// Switching back to Basic also emits `reauth` so the modal clears
/// its signed-in status, matching the pre-split inline handler.
import type { AccountConfig } from "@/lib/types";

defineProps<{
  form: AccountConfig;
  editing: boolean;
  oauthStatus: string | null;
  oidcUserCode: string | null;
  oauthInProgress: boolean;
}>();
const emit = defineEmits<{ oidcSignIn: []; reauth: [] }>();
</script>

<template>
  <div class="form-group">
    <label>Authentication</label>
    <div class="type-selector">
      <button
        class="type-btn"
        :class="{ active: form.jmap_auth_method === 'basic' }"
        :disabled="editing"
        @click="form.jmap_auth_method = 'basic'; emit('reauth')"
      >Password</button>
      <button
        class="type-btn"
        :class="{ active: form.jmap_auth_method === 'oidc' }"
        :disabled="editing"
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
        <button class="btn-reauth" @click="emit('reauth')">Sign in again</button>
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
        @click="emit('oidcSignIn')"
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

<style scoped>
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
