<script setup lang="ts">
/// Per-binding sync controls: the mail/calendar/contacts sync toggles
/// with their interval fields, plus the default-contact-book pickers
/// (#137). Only meaningful for accounts that have multiple bindings;
/// the standalone CalDAV / CardDAV tabs never render this section.
///
/// `form` is the modal's shared AccountConfig draft, passed by
/// reference — toggles and intervals write into it. The book picks
/// live OUTSIDE the form (service_bindings.config_json, not
/// AccountConfig), so they come in as two v-models the modal persists
/// in saveAccount.
import { computed } from "vue";
import type { AccountConfig } from "@/lib/types";
import type { BookOption } from "@/lib/account-types";

const props = defineProps<{
  form: AccountConfig;
  hasCalendarBinding: boolean;
  hasContactsBinding: boolean;
  availableBooks: BookOption[];
}>();

const mailBookId = defineModel<string | null>("mailBookId", { required: true });
const calendarBookId = defineModel<string | null>("calendarBookId", { required: true });

// Wire form-side number inputs in minutes; convert to/from seconds when
// reading and writing AccountConfig so the wire format keeps the
// Tauri-friendly seconds unit.
function makeMinutesField(key: "calendar_sync_interval_seconds" | "contacts_sync_interval_seconds" | "mail_sync_interval_seconds") {
  return computed<number | null>({
    get: () => {
      const s = props.form[key];
      return s == null ? null : Math.round(s / 60);
    },
    set: (m) => {
      if (m == null || Number.isNaN(m)) {
        props.form[key] = null;
      } else {
        // Clamp to a minimum of 1 minute. The browser already enforces
        // `min="1"` on the input but a programmatic v-model write (or
        // someone bypassing the input) could otherwise persist
        // sub-minute values into *_sync_interval_seconds.
        const minutes = Math.max(1, Math.round(m));
        props.form[key] = minutes * 60;
      }
    },
  });
}

const calendarIntervalMinutes = makeMinutesField("calendar_sync_interval_seconds");
const contactsIntervalMinutes = makeMinutesField("contacts_sync_interval_seconds");
</script>

<template>
  <div class="form-group bindings-section" data-testid="binding-controls">
    <label class="bindings-section-title">Per-service sync</label>

    <!-- Mail toggle + interval: only show when there's actually
         a mail binding. CalDAV/CardDAV-only accounts hide this. -->
    <div v-if="form.mail_protocol" class="form-group form-group-checkbox">
      <label class="checkbox-label">
        <input
          v-model="form.mail_sync_enabled"
          type="checkbox"
          data-testid="mail-sync-enabled"
        />
        Sync mail
      </label>
      <p class="form-help">
        Turn off to keep using calendars and contacts on this server without fetching mail. Useful for JMAP accounts you only treat as a calendar source.
      </p>
    </div>

    <div v-if="hasCalendarBinding" class="form-group form-group-checkbox binding-row">
      <label class="checkbox-label">
        <input
          v-model="form.calendar_sync_enabled"
          type="checkbox"
          data-testid="calendar-sync-enabled"
        />
        Sync calendar
      </label>
      <div class="interval-row">
        <span>Every</span>
        <input
          v-model="calendarIntervalMinutes"
          type="number"
          min="1"
          max="1440"
          placeholder="5"
          class="interval-input"
          data-testid="calendar-sync-interval"
        />
        <span>minutes</span>
        <span class="field-hint inline-hint">default 5 if blank</span>
      </div>
    </div>

    <div v-if="hasContactsBinding" class="form-group form-group-checkbox binding-row">
      <label class="checkbox-label">
        <input
          v-model="form.contacts_sync_enabled"
          type="checkbox"
          data-testid="contacts-sync-enabled"
        />
        Sync contacts
      </label>
      <div class="interval-row">
        <span>Every</span>
        <input
          v-model="contactsIntervalMinutes"
          type="number"
          min="1"
          max="1440"
          placeholder="30"
          class="interval-input"
          data-testid="contacts-sync-interval"
        />
        <span>minutes</span>
        <span class="field-hint inline-hint">default 30 if blank</span>
      </div>
    </div>

    <p class="form-help bindings-footer">
      When a service is off, the corresponding data is not fetched from the server. Already-synced data remains available offline.
    </p>

    <div v-if="form.mail_protocol" class="form-group binding-row">
      <label>Default address book for compose</label>
      <select
        v-model="mailBookId"
        class="form-control"
        data-testid="default-contact-book-mail"
      >
        <option :value="null">Auto (first synced book on this account)</option>
        <option v-for="b in availableBooks" :key="b.id" :value="b.id">{{ b.label }}</option>
      </select>
      <span class="field-hint">Recipient autocomplete in the composer ranks matches from this book first.</span>
    </div>

    <div v-if="hasCalendarBinding" class="form-group binding-row">
      <label>Default address book for event attendees</label>
      <select
        v-model="calendarBookId"
        class="form-control"
        data-testid="default-contact-book-calendar"
      >
        <option :value="null">Auto (first synced book on this account)</option>
        <option v-for="b in availableBooks" :key="b.id" :value="b.id">{{ b.label }}</option>
      </select>
      <span class="field-hint">Attendee autocomplete in the event editor ranks matches from this book first.</span>
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

.binding-row {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.interval-row {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-left: 24px;
  font-size: 12px;
  color: var(--color-text-muted);
}

.interval-input {
  /* Override the full-width form input style — the timer field is a
     short inline number input, not a text field. */
  width: 64px;
  height: 28px;
  padding: 0 8px;
  font-size: 12px;
}

.inline-hint {
  margin-left: 4px;
  font-style: italic;
}

.bindings-footer {
  margin: 4px 0 0 0;
  font-size: 12px;
  color: var(--color-text-muted);
  line-height: 1.4;
}
</style>
