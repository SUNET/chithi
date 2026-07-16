<script setup lang="ts">
/// Standalone CalDAV / CardDAV URL field — the URL is the entire
/// reason the account exists, so it stays a manual input. (The DAV
/// username field lives in the modal's shared identity block: the
/// password field renders between username and URL, so pulling the
/// username in here would visibly reorder the form.)
///
/// `form` is the modal's shared AccountConfig draft, passed by
/// reference — the URL input writes into it.
import type { AccountConfig } from "@/lib/types";

defineProps<{
  form: AccountConfig;
  accountType: "caldav" | "carddav";
}>();
</script>

<template>
  <div class="form-group">
    <label>{{ accountType === 'carddav' ? 'CardDAV URL' : 'CalDAV URL' }}</label>
    <input
      v-model="form.caldav_url"
      type="url"
      :placeholder="accountType === 'carddav'
        ? 'https://contacts.example.com/dav'
        : 'https://mail.example.com/dav/cal'"
      :data-testid="`${accountType}-url`"
    />
  </div>
</template>
