<script setup lang="ts">
/// Add/Edit Account modal (shared by mobile + desktop), extracted from
/// SettingsView (#166). Owns the whole form lifecycle: the shared
/// `form: AccountConfig` draft that every per-type section binds into,
/// the per-type OAuth / OIDC / meet sign-in flows, IMAP autodiscovery,
/// default-book selection and the save/cancel paths. The parent opens
/// it imperatively via the exposed `openNew(type)` / `openEdit(id)` —
/// edit-open is async (config + book loads) and the modal owns its own
/// visibility, so an imperative handle beats a v-model here. Store
/// refresh and post-save navigation happen inside, exactly as they did
/// in the view, so no emits are needed.
import { ref, computed } from "vue";
import { useRouter } from "vue-router";
import { useAccountsStore } from "@/stores/accounts";
import { usePlatformStore } from "@/stores/platform";
import type { AccountConfig } from "@/lib/types";
import * as api from "@/lib/tauri";
import { openUrl } from "@tauri-apps/plugin-opener";
import PasswordInput from "@/components/common/PasswordInput.vue";
import ModalShell from "@/components/common/ModalShell.vue";
import MeetAccountSection from "@/components/settings/MeetAccountSection.vue";
import OauthSignInSection from "@/components/settings/OauthSignInSection.vue";
import ImapServerSection from "@/components/settings/ImapServerSection.vue";
import JmapSection from "@/components/settings/JmapSection.vue";
import DavSection from "@/components/settings/DavSection.vue";
import SyncBindingsSection from "@/components/settings/SyncBindingsSection.vue";
import PgpAdvancedSection from "@/components/settings/PgpAdvancedSection.vue";
import {
  accountTypeLabelLong,
  isFastmailJmapUrl,
  type AccountType,
  type BookOption,
} from "@/lib/account-types";

const router = useRouter();
const accountsStore = useAccountsStore();
const platformStore = usePlatformStore();

const showForm = ref(false);
const saving = ref(false);
const error = ref<string | null>(null);
const editingAccountId = ref<string | null>(null);
const oauthStatus = ref<string | null>(null);
const oauthInProgress = ref(false);
const meetAuthStatus = ref<string | null>(null);
let visioOperationId = 0;
const activeVisioLogin = ref<{
  operationId: number;
  sessionId: string | null;
} | null>(null);

function invalidateVisioLogin() {
  visioOperationId += 1;
  if (activeVisioLogin.value) meetSigningIn.value = false;
  const sessionId = activeVisioLogin.value?.sessionId;
  activeVisioLogin.value = null;
  if (sessionId) {
    void api.meetVisioLoginCancel(sessionId).catch((e) => {
      // Cancellation can lose the backend's atomic claim once persistence has
      // begun. The stale operation is still prevented from mutating this form.
      console.warn("invalidateVisioLogin: backend cancellation failed", e);
    });
  }
}

// Default contact book per binding (#137). Stored separately from
// `form` because it lives in service_bindings.config_json, not in
// AccountConfig. Loaded when the edit modal opens; persisted in
// saveAccount alongside the account update. `null` = not set, falls
// back to the auto-pick on the backend.
const defaultMailBookId = ref<string | null>(null);
const defaultCalendarBookId = ref<string | null>(null);

// Cross-account list of contact books shown in the dropdowns.
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
    || accountType.value === "zoom"
    || accountType.value === "visio",
);

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
  pgp_attach_pubkey_on_sign: true,
  pgp_autocrypt_header: true,
  pgp_encrypt_subject: true,
  pgp_encrypt_drafts: true,
});

const form = ref<AccountConfig>(defaultForm());

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
      // Also reset the JMAP-specific fields so a previous click on
      // the Fastmail tab (which hardcodes jmap_url to
      // api.fastmail.com and jmap_auth_method to "bearer") doesn't
      // leak through into the generic JMAP form — the JMAP UI only
      // offers Basic / OIDC, so a stuck "bearer" would be
      // unreachable from the user's perspective and the saved URL
      // would auto-pick the Fastmail edit-load branch.
      if (!editingAccountId.value) {
        f.imap_host = "";
        f.imap_port = 0;
        f.smtp_host = "";
        f.smtp_port = 0;
        f.jmap_url = "";
        f.jmap_auth_method = "basic";
      }
      f.use_tls = true;
      break;
    case "fastmail":
      // Fastmail-specific JMAP. Hardcoded URL + bearer auth, no
      // user-visible toggles — Fastmail's api.fastmail.com endpoint
      // requires `Authorization: Bearer <api-token>` and rejects
      // HTTP Basic. The Fastmail form asks only for email + API
      // token. `provider = "fastmail"` is set so the list view shows
      // a FASTMAIL chip, but openEdit re-detects by URL since
      // populate_legacy_from_bindings rewrites provider on read-back.
      f.provider = "fastmail";
      f.mail_protocol = "jmap";
      f.jmap_url = "https://api.fastmail.com";
      f.jmap_auth_method = "bearer";
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
    case "visio":
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

/// Open the form for a new account of the picked type. The reset order
/// is load-bearing: `editingAccountId` must be null and `form` must be
/// a fresh `defaultForm()` BEFORE `selectAccountType` runs — its
/// `!editingAccountId` guards decide whether host prefills apply, and
/// a stale form would leak the previous type's fields (e.g. Fastmail's
/// bearer jmap_auth_method into a generic JMAP form).
function openNew(type: AccountType) {
  if (type === "visio" && platformStore.kind !== "desktop") return;
  invalidateVisioLogin();
  editingAccountId.value = null;
  resetDefaultBookState();
  error.value = null;
  meetAuthStatus.value = null;
  form.value = defaultForm();
  selectAccountType(type);
  // Pre-load the cross-account book list so the create-flow dropdowns
  // are populated for users who already have an account with synced
  // books and want to point a new account at one of them.
  loadAvailableBooks();
  showForm.value = true;
}

async function openEdit(id: string) {
  invalidateVisioLogin();
  editingAccountId.value = id;
  error.value = null;
  meetAuthStatus.value = null;
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
      console.warn("openEdit: load default contact books failed", e);
      defaultMailBookId.value = null;
      defaultCalendarBookId.value = null;
    }
    await loadAvailableBooks();
    // The edit path deliberately never calls selectAccountType — the
    // per-type switch would clobber the loaded hosts/flags. It only
    // assigns accountType from what the loaded config looks like.
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
    } else if (
      config.mail_protocol === "jmap"
      && (config.provider === "fastmail" || isFastmailJmapUrl(config.jmap_url))
    ) {
      // Detect Fastmail accounts on edit-load: either provider was
      // set by the Fastmail-tab save path, or the URL points at
      // Fastmail's JMAP endpoint. populate_legacy_from_bindings
      // rewrites provider to "generic" on read-back, so URL-based
      // detection is the durable signal. The host match is strict
      // (full hostname equality, not startsWith) so a lookalike
      // host like `api.fastmail.com.attacker.example` does not
      // silently load the Fastmail tab.
      accountType.value = "fastmail";
      oauthStatus.value = null;
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
      || config.meet_protocol === "visio"
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
    // Pre-existing quirk kept as-is: when getAccountConfig throws the
    // error lands here but the modal never opens, so the message is
    // not visible anywhere. Preserved verbatim from the view.
    error.value = String(e);
  }
}

defineExpose({ openNew, openEdit });

async function saveAccount() {
  saving.value = true;
  error.value = null;
  try {
    // Fastmail save-time guard: the form's only secret is the API
    // token (the "Password" input is relabelled to "API token" for
    // this tab). On a new account it must be a non-empty token, since
    // bearer-mode JmapConfig builds will otherwise fail fast at
    // connect time with a generic error and the account is unusable.
    // We check trim() too — whitespace-only input would have been
    // silently dropped by the keyring write guard, leaving the field
    // visually "set" but the keyring empty.
    // When editing, blank means "leave the existing token alone", so
    // we only reject a token that the user explicitly typed but that
    // consists entirely of whitespace.
    if (accountType.value === "fastmail") {
      const trimmedToken = form.value.password.trim();
      if (!editingAccountId.value && trimmedToken === "") {
        throw new Error(
          "Fastmail accounts require an API token. Generate one at " +
            "Fastmail Settings → Privacy & Security → Manage API tokens.",
        );
      }
      if (editingAccountId.value && form.value.password !== "" && trimmedToken === "") {
        throw new Error(
          "API token cannot be only whitespace. Leave the field blank " +
            "to keep the existing token, or paste a valid token.",
        );
      }
    }
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
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    saving.value = false;
  }
}

function cancelForm() {
  invalidateVisioLogin();
  meetSigningIn.value = false;
  showForm.value = false;
  editingAccountId.value = null;
  error.value = null;
  meetAuthStatus.value = null;
  resetDefaultBookState();
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

// --- Video conferencing (#148) -------------------------------------------
//
// All meet providers (Talk, Matrix, Zoom, …) use a browser-assisted
// login flow. The two-step pattern matches what we already do for
// Gmail / O365 OAuth: start returns a URL, we open it via the
// shell-opener, then a second call drives the flow to completion
// and persists the account. Each provider has its own pair of
// signInWith* functions because the shapes differ (Talk polls,
// Matrix waits for an SSO redirect, Zoom is OAuth+PKCE).
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
      start.session_id,
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
    const start = await api.meetZoomLoginStart(
      editingAccountId.value || undefined,
    );
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

async function signInWithVisio() {
  if (meetSigningIn.value) return;
  if (!form.value.meet_url.trim()) {
    error.value = "Enter your La Suite Visio instance URL first";
    return;
  }
  meetSigningIn.value = true;
  error.value = null;
  meetAuthStatus.value = null;
  const operationId = ++visioOperationId;
  const editingId = editingAccountId.value;
  const serverUrl = form.value.meet_url.trim();
  const displayName = form.value.display_name || undefined;
  activeVisioLogin.value = { operationId, sessionId: null };
  try {
    const start = await api.meetVisioLoginStart(
      serverUrl,
      editingId || undefined,
    );
    if (activeVisioLogin.value?.operationId !== operationId) {
      await api.meetVisioLoginCancel(start.session_id);
      return;
    }
    activeVisioLogin.value.sessionId = start.session_id;
    const accountId = await api.meetVisioLoginComplete(
      start.session_id,
      displayName,
    );
    if (activeVisioLogin.value?.operationId !== operationId) return;
    activeVisioLogin.value = null;
    await accountsStore.fetchAccounts();
    if (visioOperationId !== operationId) return;
    if (editingId) {
      meetAuthStatus.value = "Signed in again. Select Save to keep account name changes.";
      return;
    }
    showForm.value = false;
    editingAccountId.value = null;
    resetDefaultBookState();
    router.push("/");
    void accountId;
  } catch (e) {
    if (visioOperationId === operationId) {
      const msg = e instanceof Error ? e.message : String(e);
      error.value = `La Suite Visio sign-in failed: ${msg}`;
    }
  } finally {
    if (activeVisioLogin.value?.operationId === operationId) {
      activeVisioLogin.value = null;
    }
    if (visioOperationId === operationId) meetSigningIn.value = false;
  }
}
</script>

<template>
  <ModalShell
    :open="showForm"
    :title="editingAccountId ? 'Edit Account' : 'Add Account'"
    @close="cancelForm"
  >
    <!-- Wrapper element so shared form primitives can be styled once
         here and reach the per-type section components via :deep()
         (scoped styles otherwise stop at child roots). -->
    <div class="account-form">
    <div v-if="error" class="form-error">{{ error }}</div>

    <!-- Account type is picked via the picker dialog before this
         modal opens (#148 cleanup). Show a read-only label here so
         the user knows what they picked / which kind of account
         they're editing. -->
    <div class="form-group">
      <label>Account Type</label>
      <div class="type-readonly" data-testid="account-type-readonly">
        {{ accountTypeLabelLong(accountType) }}
      </div>
    </div>

    <div class="form-group">
      <label>Account Name</label>
      <input
        v-model="form.display_name"
        type="text"
        :disabled="isMeetTab && meetSigningIn"
        :placeholder="accountType === 'caldav' ? 'My Calendar' : 'e.g., Personal, Work'"
      />
    </div>

    <!-- Video-conferencing tabs (#148). One URL field + a
         browser-assisted sign-in button replaces the rest
         of the form, since neither account type has any
         mail / calendar / contacts surface to configure
         here. -->
    <MeetAccountSection
      v-if="accountType === 'talk' || accountType === 'matrix' || accountType === 'zoom' || accountType === 'visio'"
      :form="form"
      :account-type="accountType"
      :editing="!!editingAccountId"
      :signing-in="meetSigningIn"
      :auth-status="meetAuthStatus"
      :authentication-supported="accountType !== 'visio' || platformStore.kind === 'desktop'"
      @sign-in="
        accountType === 'talk'
          ? signInWithTalk()
          : accountType === 'matrix'
            ? signInWithMatrix()
            : accountType === 'zoom'
              ? signInWithZoom()
              : signInWithVisio()
      "
    />

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
      <label>{{ accountType === 'fastmail' ? 'API token' : (accountType === 'gmail' ? 'App Password' : 'Password') }}</label>
      <PasswordInput
        v-model="form.password"
        :placeholder="editingAccountId ? (accountType === 'fastmail' ? 'Leave empty to keep current token' : 'Leave empty to keep current password') : (accountType === 'fastmail' ? 'Paste your Fastmail API token' : (accountType === 'gmail' ? 'Gmail app password (for IMAP/SMTP)' : '••••••••'))"
      />
      <span v-if="accountType === 'fastmail'" class="field-hint">Generate at Fastmail Settings → Privacy &amp; Security → Manage API tokens. Stored in your OS keyring.</span>
      <span v-else class="field-hint">Passwords are stored securely in your OS keyring</span>
    </div>

    <OauthSignInSection
      v-if="accountType === 'gmail'"
      provider="google"
      :status="oauthStatus"
      :in-progress="oauthInProgress"
      @sign-in="startGoogleOAuth"
      @reauth="oauthStatus = null"
    />

    <OauthSignInSection
      v-if="accountType === 'o365'"
      provider="microsoft"
      :status="oauthStatus"
      :in-progress="oauthInProgress"
      @sign-in="startMicrosoftOAuth"
      @reauth="oauthStatus = null"
    />

    <!-- Server rows + autodiscovery. Order-safe merge: for the IMAP
         tab nothing renders between the old server-rows and
         discovery positions. -->
    <ImapServerSection
      v-if="accountType === 'imap'"
      :form="form"
      :editing="!!editingAccountId"
    />

    <JmapSection
      v-if="accountType === 'jmap'"
      :form="form"
      :editing="!!editingAccountId"
      :oauth-status="oauthStatus"
      :oidc-user-code="oidcUserCode"
      :oauth-in-progress="oauthInProgress"
      @oidc-sign-in="startJmapOidc"
      @reauth="oauthStatus = null"
    />

    <!-- Fastmail tab: hardcoded JMAP URL + bearer auth, so the
         form only needs an info row. The API-token field is
         rendered above by the shared password block (which
         relabels itself when accountType === 'fastmail'). -->
    <template v-if="accountType === 'fastmail'">
      <div class="form-group">
        <label>JMAP endpoint</label>
        <div class="type-readonly">https://api.fastmail.com</div>
        <span class="field-hint">Fastmail's JMAP API. Authentication uses Authorization: Bearer with the API token above.</span>
      </div>
    </template>

    <!-- For standalone CalDAV / CardDAV the URL is the entire
         reason the account exists, so it stays as a manual
         input. IMAP accounts go through auto-discovery instead
         (inside ImapServerSection); the discovered URL drives
         whether the calendar / contacts toggles appear in the
         per-service section. -->
    <DavSection
      v-if="accountType === 'caldav' || accountType === 'carddav'"
      :form="form"
      :account-type="accountType"
    />

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
         CardDAV tabs hide the irrelevant rows. -->
    <SyncBindingsSection
      v-if="accountType !== 'caldav' && accountType !== 'carddav' && !isMeetTab"
      v-model:mail-book-id="defaultMailBookId"
      v-model:calendar-book-id="defaultCalendarBookId"
      :form="form"
      :has-calendar-binding="hasCalendarBinding"
      :has-contacts-binding="hasContactsBinding"
      :available-books="availableBooks"
    />

    <PgpAdvancedSection v-if="!isMeetTab && form.mail_protocol" :form="form" />

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

    <template #footer>
      <button class="btn-secondary" @click="cancelForm">Cancel</button>
      <!-- New meet accounts persist through the Sign-in
           button above; no separate Save step. Editing
           keeps Save so the user can rename the account. -->
      <button
        v-if="!isMeetTab || editingAccountId"
        class="btn-primary"
        :disabled="saving || meetSigningIn"
        @click="saveAccount"
      >
        {{ saving ? "Saving..." : (editingAccountId ? "Save" : "Add Account") }}
      </button>
    </template>
  </ModalShell>
</template>

<style scoped>
/* Shared form primitives, styled once and reaching the per-type
   section components through :deep() — scoped styles otherwise stop
   at child component roots. Rules the modal alone uses (form-error,
   type-readonly, signature, info-box, footer buttons) stay plain;
   section-specific rules live in their section components. */

.account-form :deep(.form-group) {
  margin-bottom: 14px;
}

.account-form :deep(.form-group label) {
  display: block;
  margin-bottom: 4px;
  font-size: 12px;
  font-weight: 500;
  color: var(--color-text-secondary);
}

.account-form :deep(.form-group input) {
  width: 100%;
  height: 40px;
  padding: 0 12px;
  border: 0.8px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg-secondary);
  font-size: 16px;
}

.account-form :deep(.form-group input:focus) {
  outline: none;
  border-color: var(--color-accent);
  box-shadow: 0 0 0 2px var(--color-accent-light);
}

.account-form :deep(.form-group input:disabled) {
  opacity: 0.5;
}

.account-form :deep(.field-hint) {
  display: block;
  font-size: 11px;
  color: var(--color-text-muted);
  margin-top: 4px;
}

.account-form :deep(.form-row) {
  display: flex;
  gap: 12px;
}

.account-form :deep(.form-row .form-group) {
  flex: 1;
}

.account-form :deep(.form-row .form-group.port) {
  flex: 1;
}

.account-form :deep(.form-group-checkbox .checkbox-label) {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 13px;
  font-weight: 500;
  color: var(--color-text);
  margin-bottom: 4px;
}

.account-form :deep(.form-group-checkbox .checkbox-label input[type="checkbox"]) {
  width: auto;
  height: auto;
  margin: 0;
}

.account-form :deep(.form-group-checkbox .form-help) {
  margin: 0 0 0 24px;
  font-size: 12px;
  color: var(--color-text-muted);
  line-height: 1.4;
}

.account-form :deep(.btn-oauth) {
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

.account-form :deep(.btn-oauth:hover) {
  background: var(--color-bg-secondary);
  border-color: var(--color-text-muted);
}

.account-form :deep(.btn-oauth:disabled) {
  opacity: 0.6;
}

.account-form :deep(.oauth-row) {
  display: flex;
  align-items: center;
  gap: 8px;
}

.account-form :deep(.oauth-status) {
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

.account-form :deep(.btn-reauth) {
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

.account-form :deep(.btn-reauth:hover) {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

/* Modal-own rules below. */

.form-error {
  padding: 8px 12px;
  background: rgba(220, 53, 69, 0.06);
  color: var(--color-danger);
  border-radius: 6px;
  margin-bottom: 16px;
  font-size: 12px;
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
</style>
