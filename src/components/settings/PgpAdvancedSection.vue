<script setup lang="ts">
/// Per-account OpenPGP "Advanced settings". Only rendered for accounts
/// that actually have a mail binding (mail_protocol non-empty) —
/// CalDAV-only / CardDAV-only / Meet accounts don't send mail so these
/// toggles would be meaningless. All four ship default-on; the user
/// opts out by unticking. Backend reads these on the compose / draft
/// commands.
///
/// `form` is the modal's shared AccountConfig draft, passed by
/// reference — the checkboxes write into it.
import type { AccountConfig } from "@/lib/types";

defineProps<{ form: AccountConfig }>();
</script>

<template>
  <div class="form-group bindings-section" data-testid="pgp-advanced-settings">
    <label class="bindings-section-title">Advanced settings</label>

    <div class="form-group form-group-checkbox">
      <label class="checkbox-label">
        <input
          v-model="form.pgp_attach_pubkey_on_sign"
          type="checkbox"
          data-testid="pgp-attach-pubkey-on-sign"
        />
        Attach my public key when adding an OpenPGP digital signature
      </label>
    </div>

    <div class="form-group form-group-checkbox">
      <label class="checkbox-label">
        <input
          v-model="form.pgp_autocrypt_header"
          type="checkbox"
          data-testid="pgp-autocrypt-header"
        />
        Send OpenPGP public key(s) in the email headers for compatibility with Autocrypt
      </label>
    </div>

    <div class="form-group form-group-checkbox">
      <label class="checkbox-label">
        <input
          v-model="form.pgp_encrypt_subject"
          type="checkbox"
          data-testid="pgp-encrypt-subject"
        />
        Encrypt the subject of OpenPGP messages
      </label>
    </div>

    <div class="form-group form-group-checkbox">
      <label class="checkbox-label">
        <input
          v-model="form.pgp_encrypt_drafts"
          type="checkbox"
          data-testid="pgp-encrypt-drafts"
        />
        Store draft messages in encrypted format
      </label>
    </div>
  </div>
</template>

<style scoped>
.bindings-section {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.bindings-section-title {
  /* Mirror .form-group label so this section reads like a labelled field
     (no border, no fieldset chrome). */
  font-size: 13px;
  font-weight: 500;
  color: var(--color-text);
}
</style>
