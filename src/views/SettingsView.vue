<script setup lang="ts">
import { ref, computed, onMounted } from "vue";
import { useRouter, useRoute } from "vue-router";
import { storeToRefs } from "pinia";
import { useAccountsStore } from "@/stores/accounts";
import { usePlatformStore } from "@/stores/platform";
import { useUiStore } from "@/stores/ui";
import type { AccountConfig } from "@/lib/types";
import * as api from "@/lib/tauri";
import { openUrl } from "@tauri-apps/plugin-opener";
import PasswordInput from "@/components/common/PasswordInput.vue";
import { acctColor } from "@/lib/account-colors";
import MobileAppBar from "@/components/mobile/MobileAppBar.vue";

const router = useRouter();
const route = useRoute();
const accountsStore = useAccountsStore();
const platformStore = usePlatformStore();
const uiStore = useUiStore();
const { isMobile } = storeToRefs(platformStore);

/// Long-form label for the type-selector buttons in the modal.
/// Mostly the same as `accountTypeLabel` for the listing, but the
/// modal is wider and benefits from "Nextcloud Talk" and "Matrix"
/// spelled out instead of an upper-case acronym.
function accountTypeLabelLong(t: AccountType): string {
  switch (t) {
    case "gmail":
      return "Gmail";
    case "o365":
      return "Microsoft 365";
    case "talk":
      return "Nextcloud Talk";
    case "matrix":
      return "Matrix";
    case "zoom":
      return "Zoom";
    default:
      return t.toUpperCase();
  }
}

function accountTypeLabel(acc: {
  provider?: string;
  mail_protocol?: string;
  has_calendar_binding?: boolean;
  has_contacts_binding?: boolean;
  meet_protocol?: string;
}): string {
  if (acc.provider === "gmail") return "GMAIL";
  if (acc.provider === "o365") return "MICROSOFT 365";
  if (acc.mail_protocol) return acc.mail_protocol.toUpperCase();
  // Standalone DAV accounts: name them by the user-visible service
  // they provide rather than the protocol acronym. "Calendar" and
  // "Contacts" mean something to a user; "CALDAV" and "CARDDAV"
  // mean something to a protocol nerd.
  const hasCal = !!acc.has_calendar_binding;
  const hasCon = !!acc.has_contacts_binding;
  if (hasCal && hasCon) return "Calendar and Contacts";
  if (hasCal) return "Calendar";
  if (hasCon) return "Contacts";
  // Meet-only accounts (#148). Same user-visible naming approach.
  switch (acc.meet_protocol) {
    case "talk":
      return "Nextcloud Talk";
    case "matrix":
      return "Matrix";
    case "zoom":
      return "Zoom";
    default:
      return "";
  }
}

/// Secondary line for the settings account list. Standalone CalDAV /
/// CardDAV accounts created before the email back-fill landed have
/// `email = ""`; falling back to `username` means they still show
/// something identifying instead of a blank gap.
function accountSecondaryLabel(acc: { email: string; username: string }): string {
  return acc.email || acc.username || "";
}

// Mobile toggles — persist to localStorage so they survive reloads.
const blockRemoteImages = ref(localStorage.getItem("chithi-block-remote-images") !== "false");
function setBlockRemoteImages(v: boolean) {
  blockRemoteImages.value = v;
  localStorage.setItem("chithi-block-remote-images", String(v));
}

const themeLabel = computed(() =>
  uiStore.theme === "dark" ? "Dark" : "Light",
);
const showForm = ref(false);
// First step of "Add Account": pick a type. Replaces the cramped
// in-modal tab row with a dialog that lists the eight account
// types as cards grouped by service area, and on pick opens the
// existing edit form pre-set to that type. Edit-existing skips
// this step. (#148 cleanup)
const showPicker = ref(false);
const showDeleteConfirm = ref(false);
const deletingAccountId = ref<string | null>(null);
const saving = ref(false);
const error = ref<string | null>(null);
const editingAccountId = ref<string | null>(null);
const oauthStatus = ref<string | null>(null);
const oauthInProgress = ref(false);
const discoveringMail = ref(false);
const discoveryNote = ref<string | null>(null);

// Wire form-side number inputs in minutes; convert to/from seconds when
// reading and writing AccountConfig so the wire format keeps the
// Tauri-friendly seconds unit.
function makeMinutesField(key: "calendar_sync_interval_seconds" | "contacts_sync_interval_seconds" | "mail_sync_interval_seconds") {
  return computed<number | null>({
    get: () => {
      const s = form.value[key];
      return s == null ? null : Math.round(s / 60);
    },
    set: (m) => {
      if (m == null || Number.isNaN(m)) {
        form.value[key] = null;
      } else {
        // Clamp to a minimum of 1 minute. The browser already enforces
        // `min="1"` on the input but a programmatic v-model write (or
        // someone bypassing the input) could otherwise persist
        // sub-minute values into *_sync_interval_seconds.
        const minutes = Math.max(1, Math.round(m));
        form.value[key] = minutes * 60;
      }
    },
  });
}

const calendarIntervalMinutes = makeMinutesField("calendar_sync_interval_seconds");
const contactsIntervalMinutes = makeMinutesField("contacts_sync_interval_seconds");

// Default contact book per binding (#137). Stored separately from
// `form` because it lives in service_bindings.config_json, not in
// AccountConfig. Loaded when the edit modal opens; persisted in
// saveAccount alongside the account update. `null` = not set, falls
// back to the auto-pick on the backend.
const defaultMailBookId = ref<string | null>(null);
const defaultCalendarBookId = ref<string | null>(null);

// Cross-account list of contact books shown in the dropdowns. Each
// label is "Account / Book" so the same book name on two different
// accounts (e.g. "Personal") stays distinguishable.
type BookOption = { id: string; label: string };
const availableBooks = ref<BookOption[]>([]);

async function loadAvailableBooks() {
  // Fetch all accounts' books in parallel — sequential awaits made the
  // edit modal noticeably slow once you had three or four accounts,
  // because each Tauri invoke serialised on the previous one. Per-
  // account failures still degrade gracefully: a failed listContactBooks
  // for one account leaves its books absent without aborting the rest.
  const perAccount = await Promise.all(
    accountsStore.accounts.map(async (acc) => {
      try {
        const books = await api.listContactBooks(acc.id);
        const accLabel = acc.display_name || acc.email || acc.id;
        return books.map((b) => ({
          id: b.id,
          label: `${accLabel} / ${b.name}`,
        }));
      } catch (e) {
        console.warn("loadAvailableBooks: failed for", acc.id, e);
        return [] as BookOption[];
      }
    }),
  );
  // Stable ordering: keep books grouped by account, then alphabetical
  // by label within each account, so the dropdown renders the same on
  // every open.
  availableBooks.value = perAccount
    .flat()
    .sort((a, b) => a.label.localeCompare(b.label));
}

// Per-binding default-book state lives outside `form` because it
// belongs to service_bindings.config_json, not AccountConfig. Reset
// when the modal closes or opens fresh so a previously edited
// account's selection can't leak into the next "Add account" / next
// edit.
function resetDefaultBookState() {
  defaultMailBookId.value = null;
  defaultCalendarBookId.value = null;
  availableBooks.value = [];
}

// Whether the current form would result in a calendar / contacts
// binding once saved. Mirrors derive_bindings on the backend:
//  - JMAP mail accounts get JMAP calendar + JMAP contacts.
//  - Gmail (auth_method oauth-google) gets Google APIs for both.
//  - O365 gets Graph for both.
//  - Generic IMAP only gets DAV bindings if a caldav_url has been
//    discovered (or manually filled).
const hasCalendarBinding = computed(() =>
  form.value.mail_protocol === "jmap"
  || form.value.provider === "gmail"
  || form.value.provider === "o365"
  || !!form.value.caldav_url,
);
const hasContactsBinding = computed(() => hasCalendarBinding.value);

/// Convenience: this account-type tab has no email identity and
/// no password — auth is browser-assisted, the loginName / MXID
/// goes into `username`. Lets the form hide email / password /
/// signature / per-service-sync sections that are meaningless
/// for these accounts. (#148)
const isMeetTab = computed(
  () =>
    accountType.value === "talk"
    || accountType.value === "matrix"
    || accountType.value === "zoom",
);

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
  meet_url: "",
  meet_protocol: "",
  username: "",
  password: "",
  use_tls: true,
  signature: "",
  jmap_auth_method: "basic",
  oidc_token_endpoint: "",
  oidc_client_id: "",
  calendar_sync_enabled: true,
  mail_sync_enabled: true,
  contacts_sync_enabled: true,
  mail_sync_interval_seconds: null,
  calendar_sync_interval_seconds: null,
  contacts_sync_interval_seconds: null,
  has_calendar_binding: false,
  has_contacts_binding: false,
});

const form = ref<AccountConfig>(defaultForm());

type AccountType =
  | "gmail"
  | "imap"
  | "jmap"
  | "caldav"
  | "carddav"
  | "o365"
  | "talk"
  | "matrix"
  | "zoom";
const accountType = ref<AccountType>("gmail");

function selectAccountType(type: AccountType) {
  accountType.value = type;
  const f = form.value;

  // Reset per-service flags up front so switching tabs doesn't carry
  // disabled-state from a previous selection (e.g. picking CardDAV-only
  // turns calendar_sync_enabled off, then switching back to IMAP must
  // turn it on again, otherwise the new account would silently skip
  // calendar sync). Each branch below overrides only what it needs.
  f.calendar_sync_enabled = true;
  f.contacts_sync_enabled = true;
  f.mail_sync_enabled = true;

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
      // Switching from Gmail / O365 leaves their pre-filled hosts
      // in the form; clear them so the user starts on an empty
      // server for a generic IMAP account they're meant to fill in
      // manually (or via auto-discover).
      if (!editingAccountId.value) {
        f.imap_host = "";
        f.imap_port = 993;
        f.smtp_host = "";
        f.smtp_port = 587;
      }
      f.jmap_url = "";
      f.use_tls = true;
      break;
    case "jmap":
      f.provider = "generic";
      f.mail_protocol = "jmap";
      // Same logic: any IMAP host pre-filled by Gmail / O365 / a
      // previous IMAP click is irrelevant for JMAP, clear it.
      if (!editingAccountId.value) {
        f.imap_host = "";
        f.imap_port = 0;
        f.smtp_host = "";
        f.smtp_port = 0;
      }
      f.use_tls = true;
      break;
    case "caldav":
      // Standalone CalDAV calendar (#43). No mail backend; the bindings
      // layer skips creating a mail binding when mail_protocol is empty.
      f.provider = "generic";
      f.mail_protocol = "";
      f.imap_host = "";
      f.imap_port = 0;
      f.smtp_host = "";
      f.smtp_port = 0;
      f.jmap_url = "";
      f.use_tls = true;
      // CalDAV-only accounts shouldn't also create a CardDAV contacts
      // binding by default — disable it explicitly. The mail toggle
      // doesn't matter (no mail binding will be derived) but keep it
      // consistent.
      f.contacts_sync_enabled = false;
      break;
    case "carddav":
      // Standalone CardDAV address book (#43). Same shape as CalDAV but
      // we toggle the inverse flags so derive_bindings creates only the
      // contacts binding.
      f.provider = "generic";
      f.mail_protocol = "";
      f.imap_host = "";
      f.imap_port = 0;
      f.smtp_host = "";
      f.smtp_port = 0;
      f.jmap_url = "";
      f.use_tls = true;
      f.calendar_sync_enabled = false;
      break;
    case "talk":
    case "matrix":
    case "zoom":
      // Video-conferencing accounts (#148). No mail / calendar /
      // contacts bindings — only meet. The actual creation goes
      // through a browser-assisted login flow rather than the
      // shared modal save path, so the form data here is only
      // used to seed defaults for the URL input. Zoom in
      // particular has no per-user server URL (Zoom hosts it),
      // so the URL input doesn't render for that tab.
      f.provider = "generic";
      f.mail_protocol = "";
      f.imap_host = "";
      f.imap_port = 0;
      f.smtp_host = "";
      f.smtp_port = 0;
      f.jmap_url = "";
      f.caldav_url = "";
      f.use_tls = true;
      f.calendar_sync_enabled = false;
      f.contacts_sync_enabled = false;
      f.mail_sync_enabled = false;
      f.meet_url = "";
      f.meet_protocol = type;
      break;
  }
}

function openNewForm() {
  editingAccountId.value = null;
  resetDefaultBookState();
  error.value = null;
  showPicker.value = true;
}

/// Step 2 of the Add-account flow: type is chosen, set up the
/// form and open it. Closes the picker.
function pickAccountType(type: AccountType) {
  form.value = defaultForm();
  accountType.value = type;
  selectAccountType(type);
  // Pre-load the cross-account book list so the create-flow dropdowns
  // are populated for users who already have an account with synced
  // books and want to point a new account at one of them.
  loadAvailableBooks();
  showPicker.value = false;
  showForm.value = true;
}

function cancelPicker() {
  showPicker.value = false;
}

/// Brief subtitle shown under each card in the picker dialog.
/// Kept terse — the card's title already says what the type is.
function accountTypeDescription(t: AccountType): string {
  switch (t) {
    case "gmail":
      return "Mail, calendar and contacts via Google";
    case "o365":
      return "Mail, calendar and contacts via Microsoft 365";
    case "imap":
      return "Generic IMAP / SMTP mail account";
    case "jmap":
      return "JMAP mail (e.g. Fastmail, Stalwart)";
    case "caldav":
      return "Standalone calendar via CalDAV";
    case "carddav":
      return "Standalone contacts via CardDAV";
    case "talk":
      return "Video conferencing on a Nextcloud server";
    case "matrix":
      return "Video conferencing via Matrix / Element Call";
    case "zoom":
      return "Video conferencing via Zoom";
  }
}

async function openEditForm(id: string) {
  editingAccountId.value = id;
  error.value = null;
  try {
    const config = await api.getAccountConfig(id);
    form.value = config;
    // Load current default-book selections (#137). Errors are
    // non-fatal: a missing binding or backend failure leaves the
    // dropdown empty and the user can still pick a value.
    try {
      [defaultMailBookId.value, defaultCalendarBookId.value] = await Promise.all([
        api.getDefaultContactBook(id, "mail").catch(() => null),
        api.getDefaultContactBook(id, "calendar").catch(() => null),
      ]);
    } catch (e) {
      console.warn("openEditForm: load default contact books failed", e);
      defaultMailBookId.value = null;
      defaultCalendarBookId.value = null;
    }
    await loadAvailableBooks();
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
    } else if (
      config.meet_protocol === "talk"
      || config.meet_protocol === "matrix"
      || config.meet_protocol === "zoom"
    ) {
      // Meet-only accounts (Talk / Matrix / Zoom) come through
      // the browser-assisted login and have no mail / calendar /
      // contacts bindings. Detected before the DAV branch
      // because all have `mail_protocol === ""` — without this
      // a meet account would be misclassified as CalDAV and the
      // form would hide the URL + Sign-in row. (#148)
      accountType.value = config.meet_protocol;
    } else if (config.mail_protocol === "") {
      // Standalone DAV account (#43). Pick the tab from the binding
      // shape rather than the sync-enabled flags so toggling "Sync
      // calendar" / "Sync contacts" doesn't reclassify the account
      // back to "imap" and hide the URL field.
      if (config.has_contacts_binding && !config.has_calendar_binding) {
        accountType.value = "carddav";
      } else {
        accountType.value = "caldav";
      }
    } else {
      accountType.value = "imap";
    }
    showForm.value = true;
  } catch (e) {
    error.value = String(e);
  }
}

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
  discoveringMail.value = true;
  discoveryNote.value = null;
  try {
    const result = await api.discoverMailServers(
      form.value.email,
      form.value.imap_host,
      form.value.smtp_host,
    );

    const filled: string[] = [];
    const skipped: string[] = [];

    // Each field (host, port) is checked independently so a user
    // who has the host typed but cleared the port can fill in just
    // the port via discovery, and vice versa. Port `0` from the
    // form (cleared <input type="number">) counts as empty.
    const imapAvailable = !!result.imap_host;
    const smtpAvailable = !!result.smtp_host;
    const imapHostEmpty = !form.value.imap_host;
    const imapPortEmpty = !form.value.imap_port;
    const smtpHostEmpty = !form.value.smtp_host;
    const smtpPortEmpty = !form.value.smtp_port;

    let filledImapHost = false;
    if (imapAvailable && imapHostEmpty) {
      form.value.imap_host = result.imap_host;
      filledImapHost = true;
    }
    let filledImapPort = false;
    if (imapPortEmpty && result.imap_port) {
      form.value.imap_port = result.imap_port;
      filledImapPort = true;
    }
    if (filledImapHost || filledImapPort) {
      filled.push("IMAP");
    } else if (imapAvailable) {
      skipped.push("IMAP");
    }

    let filledSmtpHost = false;
    if (smtpAvailable && smtpHostEmpty) {
      form.value.smtp_host = result.smtp_host;
      filledSmtpHost = true;
    }
    let filledSmtpPort = false;
    if (smtpPortEmpty && result.smtp_port) {
      form.value.smtp_port = result.smtp_port;
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
        form.value.use_tls = result.imap_use_tls;
      } else {
        console.warn(
          "autoconfig: imap_use_tls / smtp_use_tls disagree; keeping TLS on",
        );
        form.value.use_tls = true;
      }
    } else if (filledImapHost) {
      form.value.use_tls = result.imap_use_tls;
    } else if (filledSmtpHost) {
      form.value.use_tls = result.smtp_use_tls;
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

async function saveAccount() {
  saving.value = true;
  error.value = null;
  try {
    // Default username to email if not set (Gmail and most IMAP servers use email as username)
    if (!form.value.username.trim()) {
      form.value.username = form.value.email;
    }
    // Standalone DAV tabs are single-purpose: a CalDAV-tab account
    // is for calendar, a CardDAV-tab account is for contacts. The
    // form hides the other binding's sync toggle, so an existing
    // account with the other flag stuck on `true` can't be cleaned
    // up through the UI. Enforce the constraint at save time so
    // the next round-trip clears it.
    if (accountType.value === "caldav") {
      form.value.contacts_sync_enabled = false;
    } else if (accountType.value === "carddav") {
      form.value.calendar_sync_enabled = false;
    }
    // Mail-having accounts already require an email. The standalone
    // CalDAV / CardDAV tabs hide the email field — but some calendar
    // code paths still touch `account.email` (CalDAV connect uses it
    // for domain extraction during auto-discovery, attendee match in
    // ical.rs uses it as the local identity), so back-fill from
    // username when it looks like one. Falls back to a deterministic
    // local-only string so the field is never blank.
    if (
      (accountType.value === "caldav" || accountType.value === "carddav") &&
      !form.value.email.trim()
    ) {
      const u = form.value.username.trim();
      form.value.email = /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(u)
        ? u
        : `${u || "dav"}@local`;
    }
    let savedId: string | null = null;
    if (editingAccountId.value) {
      await api.updateAccount(editingAccountId.value, form.value);
      savedId = editingAccountId.value;
      await accountsStore.fetchAccounts();
    } else {
      await accountsStore.addAccount(form.value);
      router.push("/");
    }
    // Persist the default-book picks for the account we just saved
    // (#137). Skipped on creation because the backend hasn't
    // synced books yet — the auto-pick on first contacts sync will
    // fill these in. Failures are non-fatal: the rest of the
    // account update has already succeeded.
    if (savedId) {
      try {
        if (form.value.mail_protocol) {
          await api.setDefaultContactBook(savedId, "mail", defaultMailBookId.value);
        }
        if (hasCalendarBinding.value) {
          await api.setDefaultContactBook(
            savedId,
            "calendar",
            defaultCalendarBookId.value,
          );
        }
      } catch (e) {
        console.warn("saveAccount: persist default contact books failed", e);
      }
    }
    showForm.value = false;
    editingAccountId.value = null;
    resetDefaultBookState();
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
  resetDefaultBookState();
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
    await openUrl(url);

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
    await openUrl(url);
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
    const verificationUrl = result.verification_uri_complete ?? result.verification_uri;
    if (!verificationUrl.startsWith("https://") && !verificationUrl.startsWith("http://")) {
      throw new Error(`Unexpected verification URL scheme: ${verificationUrl}`);
    }
    // Android: hop through a Chrome Custom Tab so the app stays foreground.
    // iOS / desktop: the JS plugin-opener path already goes through
    // UIApplication/OS defaults correctly; its Rust free-function equivalent
    // shells out to `uiopen` on iOS which doesn't exist on the simulator.
    if (platformStore.kind === "android") {
      await api.openOauthUrl(verificationUrl);
    } else {
      await openUrl(verificationUrl);
    }

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

// --- Video conferencing (#148) -------------------------------------------
//
// Both providers use a browser-assisted login flow. The two-step
// pattern matches what we already do for Gmail / O365 OAuth: start
// returns a URL, we open it via the shell-opener, then a second
// call drives the flow to completion and persists the account.
const meetSigningIn = ref(false);

async function signInWithTalk() {
  if (meetSigningIn.value) return;
  if (!form.value.meet_url.trim()) {
    error.value = "Enter your Nextcloud server URL first";
    return;
  }
  meetSigningIn.value = true;
  error.value = null;
  try {
    const start = await api.meetTalkLoginStart(form.value.meet_url.trim());
    await openUrl(start.login_url);
    const accountId = await api.meetTalkLoginComplete(
      start.poll_endpoint,
      start.poll_token,
      form.value.display_name || undefined,
    );
    await accountsStore.fetchAccounts();
    showForm.value = false;
    editingAccountId.value = null;
    resetDefaultBookState();
    // Drop the user back on the main app with the new account's
    // calendars / contacts visible (Talk has neither, but the
    // listing still updates).
    router.push("/");
    void accountId;
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    error.value = `Nextcloud Talk sign-in failed: ${msg}`;
  } finally {
    meetSigningIn.value = false;
  }
}

async function signInWithMatrix() {
  if (meetSigningIn.value) return;
  if (!form.value.meet_url.trim()) {
    error.value = "Enter your Matrix homeserver URL first";
    return;
  }
  meetSigningIn.value = true;
  error.value = null;
  try {
    const homeserver = form.value.meet_url.trim();
    const start = await api.meetMatrixLoginStart(homeserver);
    await openUrl(start.login_url);
    const accountId = await api.meetMatrixLoginComplete(
      homeserver,
      start.port,
      form.value.display_name || undefined,
    );
    await accountsStore.fetchAccounts();
    showForm.value = false;
    editingAccountId.value = null;
    resetDefaultBookState();
    router.push("/");
    void accountId;
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    error.value = `Matrix sign-in failed: ${msg}`;
  } finally {
    meetSigningIn.value = false;
  }
}

async function signInWithZoom() {
  // Zoom is hosted by Zoom — there's no per-user URL to type in
  // first. Just kick off the OAuth flow against the Marketplace-
  // registered Chithi app and store the resulting tokens. (#148)
  if (meetSigningIn.value) return;
  meetSigningIn.value = true;
  error.value = null;
  try {
    const start = await api.meetZoomLoginStart();
    await openUrl(start.login_url);
    const accountId = await api.meetZoomLoginComplete(
      start.port,
      form.value.display_name || undefined,
    );
    await accountsStore.fetchAccounts();
    showForm.value = false;
    editingAccountId.value = null;
    resetDefaultBookState();
    router.push("/");
    void accountId;
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    error.value = `Zoom sign-in failed: ${msg}`;
  } finally {
    meetSigningIn.value = false;
  }
}

// Onboarding hands off via ?addAccount=<provider>. Auto-open the new-account
// form with the matching provider preselected.
onMounted(() => {
  const want = route.query.addAccount;
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
  };
  const type = mapped[want];
  if (!type) return;
  // Deep-link path skips the picker and lands directly on the
  // form for the requested type — onboarding has already chosen.
  pickAccountType(type);
});
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
          <button
            v-for="account in accountsStore.accounts"
            :key="account.id"
            class="mobile-account-row"
            :style="{ ['--acct-color']: acctColor(account.id).fill }"
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
  <Teleport to="body">
    <div v-if="showPicker" class="modal-overlay" @click.self="cancelPicker">
      <div class="modal modal-picker" data-testid="account-type-picker">
        <div class="modal-header">
          <h3>Add Account</h3>
          <button class="modal-close" @click="cancelPicker">&times;</button>
        </div>
        <div class="modal-body">
          <p class="picker-help">Pick the kind of account you want to add. You can add more later.</p>
          <div class="picker-grid">
            <button
              v-for="t in (['gmail', 'o365', 'imap', 'jmap', 'caldav', 'carddav', 'talk', 'matrix', 'zoom'] as AccountType[])"
              :key="t"
              class="picker-card"
              :data-testid="`picker-${t}`"
              @click="pickAccountType(t)"
            >
              <span class="picker-card-title">{{ accountTypeLabelLong(t) }}</span>
              <span class="picker-card-desc">{{ accountTypeDescription(t) }}</span>
            </button>
          </div>
        </div>
        <div class="modal-footer">
          <button class="btn-secondary" @click="cancelPicker">Cancel</button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- Add/Edit Account Modal (shared by mobile + desktop) -->
  <Teleport to="body">
      <div v-if="showForm" class="modal-overlay" @click.self="cancelForm">
        <div class="modal">
          <div class="modal-header">
            <h3>{{ editingAccountId ? 'Edit Account' : 'Add Account' }}</h3>
            <button class="modal-close" @click="cancelForm">&times;</button>
          </div>
          <div class="modal-body">
            <div v-if="error" class="form-error">{{ error }}</div>

            <!-- Account type is now picked via the picker dialog
                 before this modal opens (#148 cleanup). Show a
                 read-only label here so the user knows what they
                 picked / which kind of account they're editing. -->
            <div class="form-group">
              <label>Account Type</label>
              <div class="type-readonly" data-testid="account-type-readonly">
                {{ accountTypeLabelLong(accountType) }}
              </div>
            </div>

            <div class="form-group">
              <label>Account Name</label>
              <input v-model="form.display_name" type="text" :placeholder="accountType === 'caldav' ? 'My Calendar' : 'e.g., Personal, Work'" />
            </div>

            <!-- Video-conferencing tabs (#148). One URL field + a
                 browser-assisted sign-in button replaces the rest
                 of the form, since neither account type has any
                 mail / calendar / contacts surface to configure
                 here. -->
            <template v-if="accountType === 'talk' || accountType === 'matrix' || accountType === 'zoom'">
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
              <!-- Sign-in button only shows on the new-account
                   flow. The login flow always inserts a fresh
                   account row (it can't update an existing one),
                   so on edit we hide the button to avoid
                   duplicating the account when the user clicks
                   it. Re-auth of an existing meet account is a
                   delete-and-add operation today. -->
              <div v-if="!editingAccountId" class="form-group">
                <button
                  type="button"
                  class="btn-oauth"
                  :disabled="meetSigningIn || (accountType !== 'zoom' && !form.meet_url)"
                  :data-testid="`${accountType}-signin-btn`"
                  @click="
                    accountType === 'talk'
                      ? signInWithTalk()
                      : accountType === 'matrix'
                        ? signInWithMatrix()
                        : signInWithZoom()
                  "
                >
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                    <rect x="3" y="11" width="18" height="11" rx="2" ry="2" /><path d="M7 11V7a5 5 0 0 1 10 0v4" />
                  </svg>
                  {{
                    meetSigningIn
                      ? 'Waiting for browser…'
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
                  To re-authenticate, delete this account and add it again. The session token is stored once and stays valid until you sign out from the {{ accountType === 'matrix' ? 'Matrix' : accountType === 'zoom' ? 'Zoom' : 'Nextcloud' }} server.
                </span>
              </div>
            </template>

            <!-- DAV-only and meet-only accounts have no mail
                 identity, so they skip the email field. DAV uses an
                 explicit username for Basic auth; meet (Talk /
                 Matrix) gets the loginName / MXID via its
                 browser-assisted login. The mail tabs keep email
                 as the default login (the saveAccount fallback
                 fills username from email when blank). -->
            <div
              v-if="accountType !== 'caldav' && accountType !== 'carddav' && !isMeetTab"
              class="form-group"
            >
              <label>Email Address</label>
              <input
                v-model="form.email"
                type="email"
                placeholder="user@example.com"
                data-testid="account-email"
              />
            </div>
            <div
              v-if="accountType === 'caldav' || accountType === 'carddav'"
              class="form-group"
            >
              <label>Username</label>
              <input
                v-model="form.username"
                type="text"
                placeholder="Login name on the DAV server"
                data-testid="account-username"
              />
              <span class="field-hint">
                Login name for the {{ accountType === 'carddav' ? 'CardDAV' : 'CalDAV' }} server.
              </span>
            </div>
            <div v-if="accountType !== 'o365' && !(accountType === 'jmap' && form.jmap_auth_method === 'oidc') && !isMeetTab" class="form-group">
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

            <!-- For standalone CalDAV / CardDAV the URL is the entire
                 reason the account exists, so it stays as a manual
                 input. IMAP accounts go through auto-discovery instead
                 (button below); the discovered URL drives whether the
                 calendar / contacts toggles appear in the per-service
                 section. -->
            <template v-if="accountType === 'caldav' || accountType === 'carddav'">
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

            <template v-if="accountType === 'imap'">
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
                <div v-if="editingAccountId && form.caldav_url" class="dav-link-cleanup-row">
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

            <template v-if="accountType === 'gmail' && !editingAccountId">
              <div class="info-box">Gmail uses IMAP (imap.gmail.com:993) and SMTP (smtp.gmail.com:587). Sign in with Google above to authorize access.</div>
            </template>

            <div v-if="!isMeetTab" class="form-group">
              <label>Email Signature</label>
              <textarea
                v-model="form.signature"
                class="signature-textarea"
                rows="4"
                placeholder="-- &#10;Your Name&#10;Your Title"
              ></textarea>
            </div>

            <!-- Per-binding sync controls. Only meaningful for accounts
                 that have multiple bindings; the standalone CalDAV /
                 CardDAV tabs hide the irrelevant rows. Visually a
                 form-group that matches the other sections rather than
                 a bordered fieldset. -->
            <div
              v-if="accountType !== 'caldav' && accountType !== 'carddav' && !isMeetTab"
              class="form-group bindings-section"
              data-testid="binding-controls"
            >
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
                  v-model="defaultMailBookId"
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
                  v-model="defaultCalendarBookId"
                  class="form-control"
                  data-testid="default-contact-book-calendar"
                >
                  <option :value="null">Auto (first synced book on this account)</option>
                  <option v-for="b in availableBooks" :key="b.id" :value="b.id">{{ b.label }}</option>
                </select>
                <span class="field-hint">Attendee autocomplete in the event editor ranks matches from this book first.</span>
              </div>
            </div>

            <!-- For standalone CalDAV/CardDAV the only relevant toggle is
                 the calendar/contacts one for the matching service. -->
            <div
              v-if="accountType === 'caldav'"
              class="form-group form-group-checkbox"
            >
              <label class="checkbox-label">
                <input
                  v-model="form.calendar_sync_enabled"
                  type="checkbox"
                  data-testid="calendar-sync-enabled"
                />
                Sync calendar
              </label>
            </div>
            <div
              v-if="accountType === 'carddav'"
              class="form-group form-group-checkbox"
            >
              <label class="checkbox-label">
                <input
                  v-model="form.contacts_sync_enabled"
                  type="checkbox"
                  data-testid="contacts-sync-enabled"
                />
                Sync contacts
              </label>
            </div>
          </div>
          <div class="modal-footer">
            <button class="btn-secondary" @click="cancelForm">Cancel</button>
            <!-- New meet accounts persist through the Sign-in
                 button above; no separate Save step. Editing
                 keeps Save so the user can rename the account. -->
            <button
              v-if="!isMeetTab || editingAccountId"
              class="btn-primary"
              :disabled="saving"
              @click="saveAccount"
            >
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

/* Read-only label that replaces the per-type tab row inside the
   form modal once the picker has chosen the type. (#148) */
.type-readonly {
  padding: 8px 12px;
  border: 1px solid var(--color-border);
  border-radius: 6px;
  background: var(--color-bg-secondary);
  color: var(--color-text);
  font-size: 13px;
  font-weight: 500;
}

/* Account-type picker (#148 cleanup). Two-column card grid. */
.modal-picker {
  max-width: 600px;
}
.picker-help {
  margin: 0 0 12px;
  font-size: 13px;
  color: var(--color-text-muted);
}
.picker-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 8px;
}
.picker-card {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 12px 14px;
  text-align: left;
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: 8px;
  cursor: pointer;
  transition: background 0.1s, border-color 0.1s;
}
.picker-card:hover {
  background: var(--color-bg-hover);
  border-color: var(--color-accent);
}
.picker-card-title {
  font-size: 14px;
  font-weight: 600;
  color: var(--color-text);
}
.picker-card-desc {
  font-size: 12px;
  color: var(--color-text-muted);
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

.mail-discovery-row {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}

.dav-link-hint {
  word-break: break-all;
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

.mobile-account-row {
  width: 100%;
  display: flex;
  align-items: center;
  gap: 12px;
  min-height: 68px;
  padding: 10px 14px;
  border: 0;
  border-left: 4px solid var(--acct-color, var(--color-accent));
  border-bottom: 1px solid var(--color-border);
  background: transparent;
  text-align: left;
  cursor: pointer;
}

.mobile-account-row:last-child {
  border-bottom: 0;
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
