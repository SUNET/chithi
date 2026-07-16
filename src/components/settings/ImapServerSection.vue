<script setup lang="ts">
/// IMAP tab server configuration: manual IMAP/SMTP host+port rows plus
/// Thunderbird-style autodiscovery. Owns the discovery state — it only
/// touches `form` and its own progress refs.
///
/// `form` is the modal's shared AccountConfig draft, passed by
/// reference — inputs and discovery results write straight into it.
import { ref } from "vue";
import type { AccountConfig } from "@/lib/types";
import * as api from "@/lib/tauri";

const props = defineProps<{
  form: AccountConfig;
  editing: boolean;
}>();

const discoveringMail = ref(false);
const discoveryNote = ref<string | null>(null);

/// Thunderbird-style mail-server autodiscovery for the IMAP tab.
/// Applies any discovered IMAP/SMTP host+port+TLS settings to the
/// form. CalDAV / CardDAV are intentionally not probed here: an
/// IMAP account is mail-only by design now, and pretending
/// otherwise turned out to silently glue mail and DAV bindings
/// onto the same row, then collide with the dedicated CalDAV /
/// CardDAV account types and produce duplicate calendars on sync.
///
/// Discovery never overwrites a value the user has already typed.
/// MX-derived hosts in particular are an inbound-routing hint that
/// frequently differs from the actual submission/IMAP servers
/// (relay providers, hosted spam filters), so trusting them over
/// user input would silently break the account; the same principle
/// applies to higher-quality sources too — if the user typed a
/// value, they have context the autoconfig database doesn't.
async function discoverMailServers() {
  const form = props.form;
  discoveringMail.value = true;
  discoveryNote.value = null;
  try {
    const result = await api.discoverMailServers(
      form.email,
      form.imap_host,
      form.smtp_host,
    );

    const filled: string[] = [];
    const skipped: string[] = [];

    // Each field (host, port) is checked independently so a user
    // who has the host typed but cleared the port can fill in just
    // the port via discovery, and vice versa. Port `0` from the
    // form (cleared <input type="number">) counts as empty.
    const imapAvailable = !!result.imap_host;
    const smtpAvailable = !!result.smtp_host;
    const imapHostEmpty = !form.imap_host;
    const imapPortEmpty = !form.imap_port;
    const smtpHostEmpty = !form.smtp_host;
    const smtpPortEmpty = !form.smtp_port;

    let filledImapHost = false;
    if (imapAvailable && imapHostEmpty) {
      form.imap_host = result.imap_host;
      filledImapHost = true;
    }
    let filledImapPort = false;
    if (imapPortEmpty && result.imap_port) {
      form.imap_port = result.imap_port;
      filledImapPort = true;
    }
    if (filledImapHost || filledImapPort) {
      filled.push("IMAP");
    } else if (imapAvailable) {
      skipped.push("IMAP");
    }

    let filledSmtpHost = false;
    if (smtpAvailable && smtpHostEmpty) {
      form.smtp_host = result.smtp_host;
      filledSmtpHost = true;
    }
    let filledSmtpPort = false;
    if (smtpPortEmpty && result.smtp_port) {
      form.smtp_port = result.smtp_port;
      filledSmtpPort = true;
    }
    if (filledSmtpHost || filledSmtpPort) {
      filled.push("SMTP");
    } else if (smtpAvailable) {
      skipped.push("SMTP");
    }

    // Apply use_tls only when we filled the matching *host* — the
    // TLS setting belongs to the host, not the port, so adjusting
    // it after only filling a port could silently flip a user's
    // intent on a host they typed manually. If both hosts were
    // filled and disagree, prefer the more secure setting rather
    // than silently downgrade.
    if (filledImapHost && filledSmtpHost) {
      if (result.imap_use_tls === result.smtp_use_tls) {
        form.use_tls = result.imap_use_tls;
      } else {
        console.warn(
          "autoconfig: imap_use_tls / smtp_use_tls disagree; keeping TLS on",
        );
        form.use_tls = true;
      }
    } else if (filledImapHost) {
      form.use_tls = result.imap_use_tls;
    } else if (filledSmtpHost) {
      form.use_tls = result.smtp_use_tls;
    }

    const sourceLabel = result.source ? ` (via ${result.source})` : "";
    if (filled.length === 0 && skipped.length === 0) {
      discoveryNote.value = "No autoconfig data found for this domain.";
    } else if (filled.length === 0) {
      discoveryNote.value =
        `Kept your existing ${skipped.join(" + ")} settings; autoconfig${sourceLabel} also returned values but did not overwrite.`;
    } else if (skipped.length === 0) {
      discoveryNote.value = `Filled ${filled.join(" + ")}${sourceLabel}.`;
    } else {
      discoveryNote.value =
        `Filled ${filled.join(" + ")}${sourceLabel}. Kept your existing ${skipped.join(" + ")} settings.`;
    }
  } catch (e) {
    // Match the rest of the UI: unwrap Error.message instead of
    // template-stringifying the raw value, which can render
    // "[object Object]" when the backend returns a structured error.
    const msg = e instanceof Error ? e.message : String(e);
    discoveryNote.value = `Discovery failed: ${msg}`;
  } finally {
    discoveringMail.value = false;
  }
}
</script>

<template>
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

  <div class="form-group">
    <label>Mail server auto-discovery</label>
    <div class="mail-discovery-row">
      <button
        type="button"
        class="btn-secondary"
        data-testid="mail-discover-btn"
        :disabled="discoveringMail || !form.email"
        @click="discoverMailServers"
      >
        {{ discoveringMail ? 'Searching...' : 'Auto-discover IMAP / SMTP' }}
      </button>
      <span v-if="!form.email" class="field-hint">
        Enter your email address first.
      </span>
    </div>
    <span v-if="discoveryNote" class="field-hint" data-testid="mail-discovery-note">
      {{ discoveryNote }}
    </span>
    <span class="field-hint">
      Looks up the IMAP / SMTP host, port and TLS settings for your domain via Thunderbird-style autoconfig. Calendars and contacts are added as separate accounts on the CalDAV / CardDAV tabs.
    </span>
    <div v-if="editing && form.caldav_url" class="dav-link-cleanup-row">
      <span class="field-hint dav-link-hint" data-testid="dav-link-hint">
        This account is also linked to {{ form.caldav_url }}.
      </span>
      <button
        type="button"
        class="btn-secondary"
        data-testid="dav-unlink-btn"
        @click="form.caldav_url = ''"
      >
        Unlink calendar / contacts
      </button>
    </div>
  </div>
</template>

<style scoped>
.mail-discovery-row {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}

.dav-link-hint {
  word-break: break-all;
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
</style>
