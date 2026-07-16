<script setup lang="ts">
/// Branded OAuth sign-in row shared by the Gmail and Microsoft 365
/// tabs: a green "signed in" status with a re-auth button once tokens
/// exist, or the provider's sign-in button. The actual flow lives in
/// the modal (it owns the form and the pending-account id); this
/// section only emits.
defineProps<{
  provider: "google" | "microsoft";
  status: string | null;
  inProgress: boolean;
}>();
const emit = defineEmits<{ signIn: []; reauth: [] }>();
</script>

<template>
  <div class="form-group">
    <label>{{ provider === 'google' ? 'Calendar & Contacts Sync' : 'Microsoft 365 Sign In' }}</label>
    <div v-if="status" class="oauth-row">
      <div class="oauth-status">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#00a63e" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12" /></svg>
        {{ status }}
      </div>
      <button class="btn-reauth" @click="emit('reauth')">Sign in again</button>
    </div>
    <button
      v-else
      class="btn-oauth"
      :disabled="inProgress"
      @click="emit('signIn')"
    >
      <svg v-if="provider === 'google'" width="16" height="16" viewBox="0 0 24 24" fill="none">
        <path d="M22.56 12.25c0-.78-.07-1.53-.2-2.25H12v4.26h5.92a5.06 5.06 0 0 1-2.2 3.32v2.77h3.57c2.08-1.92 3.28-4.74 3.28-8.1z" fill="#4285F4"/>
        <path d="M12 23c2.97 0 5.46-.98 7.28-2.66l-3.57-2.77c-.98.66-2.23 1.06-3.71 1.06-2.86 0-5.29-1.93-6.16-4.53H2.18v2.84C3.99 20.53 7.7 23 12 23z" fill="#34A853"/>
        <path d="M5.84 14.09c-.22-.66-.35-1.36-.35-2.09s.13-1.43.35-2.09V7.07H2.18A10.96 10.96 0 0 0 1 12c0 1.77.42 3.45 1.18 4.93l3.66-2.84z" fill="#FBBC05"/>
        <path d="M12 5.38c1.62 0 3.06.56 4.21 1.64l3.15-3.15C17.45 2.09 14.97 1 12 1 7.7 1 3.99 3.47 2.18 7.07l3.66 2.84c.87-2.6 3.3-4.53 6.16-4.53z" fill="#EA4335"/>
      </svg>
      <svg v-else width="16" height="16" viewBox="0 0 24 24" fill="none">
        <rect x="1" y="1" width="10" height="10" fill="#F25022"/>
        <rect x="13" y="1" width="10" height="10" fill="#7FBA00"/>
        <rect x="1" y="13" width="10" height="10" fill="#00A4EF"/>
        <rect x="13" y="13" width="10" height="10" fill="#FFB900"/>
      </svg>
      {{ inProgress ? "Waiting for browser..." : (provider === 'google' ? "Sign in with Google" : "Sign in with Microsoft") }}
    </button>
    <span v-if="provider === 'google'" class="field-hint">Sign in to sync Google Calendar and Contacts. IMAP/SMTP uses app password above.</span>
    <span v-else class="field-hint">Sign in to access mail, calendar, and contacts via Microsoft Graph API.</span>
  </div>
</template>
