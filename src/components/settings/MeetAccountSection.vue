<script setup lang="ts">
/// Video-conferencing tabs (#148). One URL field + a browser-assisted
/// sign-in button replaces the rest of the form, since neither account
/// type has any mail / calendar / contacts surface to configure here.
///
/// `form` is the modal's shared AccountConfig draft, passed by
/// reference — inputs here bind straight into it (deliberate nested
/// prop mutation; all cross-type mutation stays in the modal).
import type { AccountConfig } from "@/lib/types";

defineProps<{
  form: AccountConfig;
  accountType: "talk" | "matrix" | "zoom";
  editing: boolean;
  signingIn: boolean;
}>();
const emit = defineEmits<{ signIn: [] }>();
</script>

<template>
  <!-- URL input hidden on Zoom because Zoom is hosted —
       there's no per-user server to type. Talk and
       Matrix both need the user's instance URL. -->
  <div v-if="accountType !== 'zoom'" class="form-group">
    <label>{{ accountType === 'matrix' ? 'Homeserver URL' : 'Nextcloud URL' }}</label>
    <input
      v-model="form.meet_url"
      type="url"
      :placeholder="accountType === 'matrix'
        ? 'https://matrix.example.org'
        : 'https://cloud.example.org'"
      :data-testid="`${accountType}-url`"
    />
    <span class="field-hint">
      {{ accountType === 'matrix'
        ? 'Base URL of your Matrix homeserver. SSO will open in your browser.'
        : 'Base URL of your Nextcloud server. Login Flow v2 will open in your browser.' }}
    </span>
  </div>
  <!-- Talk and Matrix still create a new account through their login
       flows, so their edit forms keep the delete-and-add guidance.
       Zoom can update an existing account's OAuth credentials. -->
  <div v-if="!editing || accountType === 'zoom'" class="form-group">
    <button
      type="button"
      class="btn-oauth"
      :disabled="signingIn || (accountType !== 'zoom' && !form.meet_url)"
      :data-testid="`${accountType}-signin-btn`"
      @click="emit('signIn')"
    >
      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
        <rect x="3" y="11" width="18" height="11" rx="2" ry="2" /><path d="M7 11V7a5 5 0 0 1 10 0v4" />
      </svg>
      {{
        signingIn
          ? 'Waiting for browser…'
          : editing && accountType === 'zoom'
            ? 'Sign in again with Zoom'
            : accountType === 'matrix'
            ? 'Sign in with Matrix'
            : accountType === 'zoom'
              ? 'Sign in with Zoom'
              : 'Sign in with Nextcloud'
      }}
    </button>
    <span class="field-hint">
      Opens your browser to authenticate. {{
        accountType === 'zoom'
          ? 'Chithi receives an OAuth token tied to your Zoom account.'
          : 'Your real password never reaches Chithi — we keep a long-lived app token tied to this device.'
      }}
    </span>
  </div>
  <div v-else class="form-group">
    <span class="field-hint">
      To re-authenticate, delete this account and add it again. The session token is stored once and stays valid until you sign out from the {{ accountType === 'matrix' ? 'Matrix' : 'Nextcloud' }} server.
    </span>
  </div>
</template>
