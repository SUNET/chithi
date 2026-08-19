use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::db::service_bindings::{
    DavBindingConfig, ImapBindingConfig, JmapBindingConfig, ServiceBinding,
};
use crate::error::Result;

/// Strict Fastmail JMAP endpoint check. Returns `true` only when
/// the URL parses, uses https, and its host is *exactly*
/// `api.fastmail.com` (case-insensitive). A plain
/// `starts_with("https://api.fastmail.com")` would also match
/// lookalike hosts such as `https://api.fastmail.com.attacker.example`
/// and tag them as Fastmail. Used by both the per-account
/// `populate_legacy_from_bindings` and the list-view `list_accounts`
/// query so the two recovery paths stay in lock-step.
fn is_fastmail_jmap_url(url_str: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url_str) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    parsed
        .host_str()
        .map(|h| h.eq_ignore_ascii_case("api.fastmail.com"))
        .unwrap_or(false)
}

#[cfg(test)]
mod fastmail_url_tests {
    use super::is_fastmail_jmap_url;

    #[test]
    fn accepts_canonical_fastmail() {
        assert!(is_fastmail_jmap_url("https://api.fastmail.com"));
        assert!(is_fastmail_jmap_url("https://api.fastmail.com/jmap"));
        assert!(is_fastmail_jmap_url(
            "https://api.fastmail.com/.well-known/jmap"
        ));
    }

    #[test]
    fn case_insensitive_host() {
        assert!(is_fastmail_jmap_url("https://API.Fastmail.COM/jmap"));
    }

    #[test]
    fn rejects_lookalike_subdomains() {
        // The whole reason this helper exists: a startsWith check
        // would have approved these.
        assert!(!is_fastmail_jmap_url(
            "https://api.fastmail.com.attacker.example/jmap"
        ));
        assert!(!is_fastmail_jmap_url(
            "https://api.fastmail.com.evil.com/jmap"
        ));
        assert!(!is_fastmail_jmap_url("https://api.fastmail.computer/jmap"));
    }

    #[test]
    fn rejects_http() {
        // Bearer credentials over plaintext is a hard no; downgrade
        // the provider tag so the saved account stops claiming
        // Fastmail-grade hardening.
        assert!(!is_fastmail_jmap_url("http://api.fastmail.com/jmap"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(!is_fastmail_jmap_url(""));
        assert!(!is_fastmail_jmap_url("api.fastmail.com"));
        assert!(!is_fastmail_jmap_url("not a url at all"));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub display_name: String,
    pub email: String,
    /// Carried in the summary so the settings list can show *something*
    /// for standalone CalDAV / CardDAV accounts whose `email` field
    /// was never set (older accounts created before the DAV-tab
    /// email back-fill landed).
    #[serde(default)]
    pub username: String,
    pub provider: String,
    pub mail_protocol: String,
    pub enabled: bool,
    /// Phase-4 (#43): per-binding sync intervals so the frontend timers
    /// can honor user preferences without an extra get_account_config
    /// round-trip. `None` means "use the service's default cadence".
    #[serde(default)]
    pub mail_sync_interval_seconds: Option<i64>,
    #[serde(default)]
    pub calendar_sync_interval_seconds: Option<i64>,
    #[serde(default)]
    pub contacts_sync_interval_seconds: Option<i64>,
    /// Whether the account has an *enabled* calendar / contacts
    /// binding. The `list_accounts` query is `enabled = 1`-aware so a
    /// CalDAV-tab account (which derives a disabled contacts binding
    /// alongside its enabled calendar binding) reads as Calendar-only
    /// rather than as both. Lets the settings UI label standalone DAV
    /// accounts without a per-row round-trip.
    #[serde(default)]
    pub has_calendar_binding: bool,
    #[serde(default)]
    pub has_contacts_binding: bool,
    /// Protocol of the account's enabled meet binding (`talk` /
    /// `matrix` / `zoom` today; whatever providers are listed in
    /// `meet::registry()` more generally), or `""` when the
    /// account has no meet binding. Lets the calendar event
    /// editor populate its "Add video link" dropdown without an
    /// extra round-trip per row. (#148)
    #[serde(default)]
    pub meet_protocol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub display_name: String,
    pub email: String,
    pub provider: String,
    /// Mail protocol. Empty string means "no mail binding" — used for
    /// standalone CalDAV / CardDAV / JMAP-cal-only accounts.
    pub mail_protocol: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub jmap_url: String,
    pub caldav_url: String,
    /// Server URL the provider keys off. For Talk this is the
    /// Nextcloud root; for Matrix it's the homeserver. For Zoom
    /// this is a marker (`https://zoom.us`) since Zoom is a
    /// hosted service with no per-user URL — `create_url` reads
    /// the OAuth tokens from the keyring and ignores this. Empty
    /// when the account has no meet binding. Pairs with
    /// `meet_protocol`.
    #[serde(default)]
    pub meet_url: String,
    /// Provider discriminator: `talk`, `matrix`, `zoom`, or
    /// whatever else is registered in `meet::registry()`.
    /// Empty when the account has no meet binding.
    #[serde(default)]
    pub meet_protocol: String,
    pub username: String,
    pub password: String,
    pub use_tls: bool,
    #[serde(default)]
    pub signature: String,
    #[serde(default = "default_basic")]
    pub jmap_auth_method: String,
    #[serde(default)]
    pub oidc_token_endpoint: String,
    #[serde(default)]
    pub oidc_client_id: String,
    #[serde(default = "default_true")]
    pub calendar_sync_enabled: bool,
    /// Whether the mail binding is enabled. Default `true`. Set `false` to
    /// keep a JMAP account's calendar/contacts sync running while turning
    /// off mail (the "JMAP cal-only" use case in #43). For a non-mail
    /// account (mail_protocol == "") this field is ignored.
    #[serde(default = "default_true")]
    pub mail_sync_enabled: bool,
    /// Whether the contacts binding is enabled. Default `true`. Lets the
    /// user disable CardDAV / Google People / Graph contacts for an
    /// account that has them otherwise.
    #[serde(default = "default_true")]
    pub contacts_sync_enabled: bool,
    /// Optional per-binding sync interval in seconds. `None` falls back to
    /// the default cadence for that service (see calendar / contacts
    /// stores on the frontend). Each service has its own field so the
    /// wire format stays explicit.
    #[serde(default)]
    pub mail_sync_interval_seconds: Option<i64>,
    #[serde(default)]
    pub calendar_sync_interval_seconds: Option<i64>,
    #[serde(default)]
    pub contacts_sync_interval_seconds: Option<i64>,
    /// Whether a calendar / contacts binding actually exists for this
    /// account, regardless of its enabled state. The Settings UI keys
    /// off these to disambiguate standalone CalDAV-only vs CardDAV-only
    /// accounts even after the user toggles the lone Sync-* flag off.
    /// Read-only on save (the backend rebuilds bindings from the legacy
    /// fields rather than these flags).
    #[serde(default)]
    pub has_calendar_binding: bool,
    #[serde(default)]
    pub has_contacts_binding: bool,
    /// Per-account OpenPGP "Advanced settings" toggles. All default `true`
    /// — fresh-install behavior is fully-enabled and the user opts out by
    /// unticking. The compose / draft backend reads these via a
    /// `SendOptions` IPC payload and only acts when the corresponding
    /// flag is set.
    #[serde(default = "default_true")]
    pub pgp_attach_pubkey_on_sign: bool,
    #[serde(default = "default_true")]
    pub pgp_autocrypt_header: bool,
    #[serde(default = "default_true")]
    pub pgp_encrypt_subject: bool,
    #[serde(default = "default_true")]
    pub pgp_encrypt_drafts: bool,
}

fn default_basic() -> String {
    "basic".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct AccountFull {
    pub id: String,
    pub display_name: String,
    pub email: String,
    pub provider: String,
    pub mail_protocol: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub jmap_url: String,
    pub caldav_url: String,
    /// Server URL + protocol for the meet binding (Talk / Matrix).
    /// Empty when the account has no meet service.
    pub meet_url: String,
    pub meet_protocol: String,
    pub username: String,
    pub password: String,
    pub use_tls: bool,
    pub enabled: bool,
    pub signature: String,
    /// One of "basic" | "bearer" | "oidc". "basic" sends HTTP Basic with
    /// username + password (Stalwart). "bearer" sends Authorization: Bearer
    /// <password> where the password field holds an API token (Fastmail).
    /// "oidc" runs the OAuth/OIDC token flow and uses the resulting access
    /// token as Bearer.
    pub jmap_auth_method: String,
    pub oidc_token_endpoint: String,
    pub oidc_client_id: String,
    pub calendar_sync_enabled: bool,
    /// Phase-2 field: how the user authenticates with this identity.
    /// One of "password" | "oauth-google" | "oauth-microsoft" |
    /// "oauth-jmap-oidc". Populated by the auth_method backfill migration.
    pub auth_method: String,
    /// Phase-2 field: bindings for this account, loaded via
    /// `service_bindings::list_for_account` during `get_account_full`.
    /// Dispatch code uses the helper methods below rather than reading
    /// this field directly.
    pub bindings: Vec<ServiceBinding>,
    /// Phase-4 wire-format mirrors of binding state. Populated from
    /// `bindings` on fetch so the Settings edit form can round-trip the
    /// per-binding `enabled` flags and sync intervals.
    pub mail_sync_enabled: bool,
    pub contacts_sync_enabled: bool,
    pub mail_sync_interval_seconds: Option<i64>,
    pub calendar_sync_interval_seconds: Option<i64>,
    pub contacts_sync_interval_seconds: Option<i64>,
    /// Per-account OpenPGP "Advanced settings" toggles. Mirrors the four
    /// columns on the `accounts` table; loaded by `get_account_full` and
    /// pushed back by `update_account`.
    pub pgp_attach_pubkey_on_sign: bool,
    pub pgp_autocrypt_header: bool,
    pub pgp_encrypt_subject: bool,
    pub pgp_encrypt_drafts: bool,
}

impl AccountFull {
    /// Look up the binding for a given service ("mail" | "calendar" |
    /// "contacts"). Returns `None` if the account has no binding for that
    /// service (e.g. a CalDAV-only account has no mail binding).
    pub fn binding_for(&self, service: &str) -> Option<&ServiceBinding> {
        self.bindings.iter().find(|b| b.service == service)
    }

    pub fn mail_binding(&self) -> Option<&ServiceBinding> {
        self.binding_for("mail")
    }

    pub fn calendar_binding(&self) -> Option<&ServiceBinding> {
        self.binding_for("calendar")
    }

    pub fn contacts_binding(&self) -> Option<&ServiceBinding> {
        self.binding_for("contacts")
    }

    /// Video-conferencing binding (Nextcloud Talk / Matrix / future
    /// providers). #148.
    pub fn meet_binding(&self) -> Option<&ServiceBinding> {
        self.binding_for("meet")
    }

    /// Convenience: the `meet` binding's enabled protocol string,
    /// or `""` when the account has no meet binding or it's been
    /// disabled. Mirrors `mail_protocol_str` / `calendar_protocol_str`.
    pub fn meet_protocol_str(&self) -> &str {
        self.meet_binding()
            .filter(|b| b.enabled)
            .map(|b| b.protocol.as_str())
            .unwrap_or("")
    }

    /// Mail protocol as a string slice. Returns `""` for accounts with no
    /// mail binding (calendar-only / contacts-only) AND for accounts whose
    /// mail binding is explicitly disabled (e.g. a JMAP server used for
    /// calendar/contacts only). Replaces direct reads of
    /// `account.mail_protocol` at dispatch sites — a disabled binding
    /// short-circuits every protocol-specific branch.
    pub fn mail_protocol_str(&self) -> &str {
        self.mail_binding()
            .filter(|b| b.enabled)
            .map(|b| b.protocol.as_str())
            .unwrap_or("")
    }

    pub fn calendar_protocol_str(&self) -> &str {
        self.calendar_binding()
            .filter(|b| b.enabled)
            .map(|b| b.protocol.as_str())
            .unwrap_or("")
    }

    pub fn contacts_protocol_str(&self) -> &str {
        self.contacts_binding()
            .filter(|b| b.enabled)
            .map(|b| b.protocol.as_str())
            .unwrap_or("")
    }

    /// Parsed IMAP/SMTP config from the mail binding, if it's an IMAP binding.
    /// Returns `None` for non-IMAP mail accounts (graph/jmap) so callers can
    /// pattern-match cleanly. Returns `Some(default)` if the binding exists
    /// but the JSON parses with all defaults — that shouldn't happen in
    /// practice but won't panic.
    pub fn mail_imap_config(&self) -> Option<ImapBindingConfig> {
        self.mail_binding()
            .filter(|b| b.protocol == "imap")
            .and_then(|b| b.imap_config().ok())
    }

    pub fn mail_jmap_config(&self) -> Option<JmapBindingConfig> {
        self.mail_binding()
            .filter(|b| b.protocol == "jmap")
            .and_then(|b| b.jmap_config().ok())
    }

    pub fn calendar_caldav_config(&self) -> Option<DavBindingConfig> {
        self.calendar_binding()
            .filter(|b| b.protocol == "caldav")
            .and_then(|b| b.dav_config().ok())
    }

    pub fn contacts_carddav_config(&self) -> Option<DavBindingConfig> {
        self.contacts_binding()
            .filter(|b| b.protocol == "carddav")
            .and_then(|b| b.dav_config().ok())
    }

    /// Calendar URL for any DAV-style calendar (caldav today; future
    /// caldav-over-google-fallback could land here too).
    pub fn calendar_dav_url(&self) -> Option<String> {
        self.calendar_caldav_config().map(|c| c.url)
    }

    pub fn contacts_dav_url(&self) -> Option<String> {
        self.contacts_carddav_config().map(|c| c.url)
    }

    /// Whether the calendar binding is enabled (replaces `calendar_sync_enabled`
    /// on the legacy schema). Returns `false` if the account has no calendar
    /// binding at all.
    pub fn calendar_enabled(&self) -> bool {
        self.calendar_binding().is_some_and(|b| b.enabled)
    }

    /// Populate the legacy per-protocol fields (`provider`, `mail_protocol`,
    /// `imap_host`, ...) from the loaded `bindings` and `auth_method`.
    /// Phase 3 dropped these columns from the database, but the fields are
    /// still part of `AccountFull` so the wire format and the dispatch sites
    /// touched in earlier phases keep working unchanged.
    pub fn populate_legacy_from_bindings(&mut self) {
        self.provider = match self.auth_method.as_str() {
            "oauth-google" => "gmail".to_string(),
            "oauth-microsoft" => "o365".to_string(),
            _ => {
                // Fastmail is fronted by the dedicated "Fastmail" account
                // tab but shares the JMAP wire path. Recover the provider
                // tag from the saved JMAP URL so the list-view chip and
                // the edit-form's type-readonly label both reflect it.
                // Strict hostname match (not startsWith) so a lookalike
                // host like api.fastmail.com.attacker.example does not
                // get tagged as Fastmail.
                let jmap_url = self.mail_jmap_config().map(|c| c.url).unwrap_or_default();
                if is_fastmail_jmap_url(&jmap_url) {
                    "fastmail".to_string()
                } else {
                    "generic".to_string()
                }
            }
        };

        // The Phase-2 `auth_method` column collapses "basic" and "bearer"
        // both to "password" (since auth_method_for only special-cases
        // OIDC). So for non-OIDC accounts we must read the actual JMAP
        // binding to recover the user's choice — otherwise a Fastmail
        // account saved as "bearer" reads back as "basic" and apply_auth
        // sends HTTP Basic, which Fastmail rejects with 401.
        self.jmap_auth_method = if self.auth_method == "oauth-jmap-oidc" {
            "oidc".to_string()
        } else {
            self.mail_jmap_config()
                .map(|c| c.auth_method)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "basic".to_string())
        };

        self.mail_protocol = self
            .mail_binding()
            .map(|b| b.protocol.clone())
            .unwrap_or_default();

        if let Some(c) = self.mail_imap_config() {
            self.imap_host = c.imap_host;
            self.imap_port = c.imap_port;
            self.smtp_host = c.smtp_host;
            self.smtp_port = c.smtp_port;
            self.use_tls = c.use_tls;
        } else if self.mail_protocol == "graph" {
            // O365 (Graph): everything — sync, calendar, contacts — runs
            // over the Graph API, EXCEPT outgoing mail, which still goes
            // over SMTP+XOAUTH2 (see ADR 0025 / commit cb08a43). The
            // `mail/graph` binding carries no IMAP/SMTP host, so the SMTP
            // endpoint is the fixed Microsoft relay rather than a stored
            // config field: `smtp.office365.com:587` (STARTTLS — forced by
            // `smtp::build_transport` on port 587, so `use_tls` is moot).
            //
            // Without this branch a Graph account's `smtp_host` stays
            // empty and every send dials a blank host, failing with
            // "No address associated with hostname". This regressed when
            // dispatch reads moved to service_bindings (phase 2) — the
            // graph binding never stored the SMTP coordinates the legacy
            // `smtp_host` column used to hold.
            self.imap_host = String::new();
            self.imap_port = 993;
            self.smtp_host = "smtp.office365.com".to_string();
            self.smtp_port = 587;
            self.use_tls = true;
        } else {
            // Sensible defaults for non-IMAP accounts.
            self.imap_host = String::new();
            self.imap_port = 993;
            self.smtp_host = String::new();
            self.smtp_port = 587;
            self.use_tls = true;
        }

        self.jmap_url = self.mail_jmap_config().map(|c| c.url).unwrap_or_default();

        // The legacy `caldav_url` column was a single string used for both
        // CalDAV and CardDAV (same server in practice). Phase 3 splits it into
        // independent bindings, so we surface the calendar URL here and fall
        // back to the contacts URL for accounts that have only carddav.
        self.caldav_url = self
            .calendar_dav_url()
            .or_else(|| self.contacts_dav_url())
            .unwrap_or_default();

        // #148: meet binding (Nextcloud Talk / Matrix). Surfaces the
        // server URL + protocol so the Settings edit form can display
        // them; the actual credential lives in the keyring under the
        // account id. Read both up front so the borrow on `self`
        // ends before the writes below.
        let (meet_url, meet_protocol) = match self.meet_binding() {
            Some(b) => (
                b.meet_config().map(|c| c.url).unwrap_or_default(),
                b.protocol.clone(),
            ),
            None => (String::new(), String::new()),
        };
        self.meet_url = meet_url;
        self.meet_protocol = meet_protocol;

        self.calendar_sync_enabled = self.calendar_binding().map(|b| b.enabled).unwrap_or(true);

        // Phase-4: surface per-binding state on the wire format so the
        // Settings edit form sees the toggles' current value.
        self.mail_sync_enabled = self.mail_binding().map(|b| b.enabled).unwrap_or(true);
        self.contacts_sync_enabled = self.contacts_binding().map(|b| b.enabled).unwrap_or(true);
        self.mail_sync_interval_seconds = self.mail_binding().and_then(|b| b.sync_interval_seconds);
        self.calendar_sync_interval_seconds = self
            .calendar_binding()
            .and_then(|b| b.sync_interval_seconds);
        self.contacts_sync_interval_seconds = self
            .contacts_binding()
            .and_then(|b| b.sync_interval_seconds);
    }
}

pub fn list_accounts(conn: &Connection) -> Result<Vec<Account>> {
    // mail_protocol comes from the mail service binding (LEFT JOIN so
    // calendar-only accounts show up with an empty mail_protocol).
    // provider is derived from auth_method on the way out so the wire
    // format stays compatible with the frontend.
    // Pull the mail protocol and per-binding sync intervals via correlated
    // subqueries against service_bindings so the lightweight Account
    // summary doesn't need a separate per-account query for the
    // periodic-sync timers.
    let mut stmt = conn.prepare(
        "SELECT a.id, a.display_name, a.email, a.auth_method, a.enabled, a.username,
                COALESCE(
                    (SELECT b.protocol FROM service_bindings b
                     WHERE b.account_id = a.id
                       AND b.service = 'mail'
                       AND b.enabled = 1
                     LIMIT 1),
                    ''
                ) AS mail_protocol,
                (SELECT b.sync_interval_seconds FROM service_bindings b
                 WHERE b.account_id = a.id AND b.service = 'mail' LIMIT 1)
                    AS mail_sync_interval,
                (SELECT b.sync_interval_seconds FROM service_bindings b
                 WHERE b.account_id = a.id AND b.service = 'calendar' LIMIT 1)
                    AS calendar_sync_interval,
                (SELECT b.sync_interval_seconds FROM service_bindings b
                 WHERE b.account_id = a.id AND b.service = 'contacts' LIMIT 1)
                    AS contacts_sync_interval,
                -- Enabled-aware so a standalone CalDAV-tab account
                -- (which derives a calendar binding enabled=1 plus a
                -- disabled contacts binding) labels as Calendar, not
                -- as both. Same for CardDAV.
                EXISTS (SELECT 1 FROM service_bindings b
                        WHERE b.account_id = a.id
                          AND b.service = 'calendar'
                          AND b.enabled = 1)
                    AS has_calendar_binding,
                EXISTS (SELECT 1 FROM service_bindings b
                        WHERE b.account_id = a.id
                          AND b.service = 'contacts'
                          AND b.enabled = 1)
                    AS has_contacts_binding,
                COALESCE(
                    (SELECT b.protocol FROM service_bindings b
                     WHERE b.account_id = a.id
                       AND b.service = 'meet'
                       AND b.enabled = 1
                     LIMIT 1),
                    ''
                ) AS meet_protocol,
                -- JMAP binding's config_json, used to recover the
                -- Fastmail provider tag from the saved URL. Empty for
                -- non-JMAP accounts. See get_account_full for the
                -- equivalent full-account recovery path.
                COALESCE(
                    (SELECT b.config_json FROM service_bindings b
                     WHERE b.account_id = a.id
                       AND b.service = 'mail'
                       AND b.protocol = 'jmap'
                     LIMIT 1),
                    ''
                ) AS jmap_config_json
         FROM accounts a
         ORDER BY a.display_name",
    )?;
    let accounts = stmt
        .query_map([], |row| {
            let auth_method: String = row.get(3)?;
            let jmap_config_json: String = row.get(13)?;
            let provider = match auth_method.as_str() {
                "oauth-google" => "gmail".to_string(),
                "oauth-microsoft" => "o365".to_string(),
                _ => {
                    // Fastmail accounts save via the dedicated tab but
                    // share the password-based auth_method bucket, so
                    // the provider lives in the JMAP binding's URL at
                    // read time. Parse the binding JSON and validate
                    // the URL's hostname strictly — a substring match
                    // (or even startsWith) on the raw config_json
                    // would mistag a lookalike host like
                    // `api.fastmail.com.attacker.example` or any
                    // other field that happened to contain the
                    // string "api.fastmail.com" as Fastmail.
                    let jmap_url = if jmap_config_json.is_empty() {
                        String::new()
                    } else {
                        serde_json::from_str::<crate::db::service_bindings::JmapBindingConfig>(
                            &jmap_config_json,
                        )
                        .map(|c| c.url)
                        .unwrap_or_default()
                    };
                    if is_fastmail_jmap_url(&jmap_url) {
                        "fastmail".to_string()
                    } else {
                        "generic".to_string()
                    }
                }
            };
            Ok(Account {
                id: row.get(0)?,
                display_name: row.get(1)?,
                email: row.get(2)?,
                username: row.get(5)?,
                provider,
                mail_protocol: row.get(6)?,
                enabled: row.get(4)?,
                mail_sync_interval_seconds: row.get(7)?,
                calendar_sync_interval_seconds: row.get(8)?,
                contacts_sync_interval_seconds: row.get(9)?,
                has_calendar_binding: row.get(10)?,
                has_contacts_binding: row.get(11)?,
                meet_protocol: row.get(12)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(accounts)
}

pub fn get_account_full(conn: &Connection, id: &str) -> Result<AccountFull> {
    let mut account = conn
        .query_row(
            "SELECT id, display_name, email, username, enabled, signature,
                oidc_token_endpoint, oidc_client_id, auth_method,
                pgp_attach_pubkey_on_sign, pgp_autocrypt_header,
                pgp_encrypt_subject, pgp_encrypt_drafts
         FROM accounts WHERE id = ?1",
            params![id],
            |row| {
                Ok(AccountFull {
                    id: row.get(0)?,
                    display_name: row.get(1)?,
                    email: row.get(2)?,
                    username: row.get(3)?,
                    enabled: row.get(4)?,
                    signature: row.get(5)?,
                    oidc_token_endpoint: row.get(6)?,
                    oidc_client_id: row.get(7)?,
                    auth_method: row.get(8)?,
                    pgp_attach_pubkey_on_sign: row.get(9)?,
                    pgp_autocrypt_header: row.get(10)?,
                    pgp_encrypt_subject: row.get(11)?,
                    pgp_encrypt_drafts: row.get(12)?,
                    // Legacy fields populated below from bindings + auth_method.
                    provider: String::new(),
                    mail_protocol: String::new(),
                    jmap_auth_method: String::new(),
                    imap_host: String::new(),
                    imap_port: 993,
                    smtp_host: String::new(),
                    smtp_port: 587,
                    jmap_url: String::new(),
                    caldav_url: String::new(),
                    meet_url: String::new(),
                    meet_protocol: String::new(),
                    password: String::new(),
                    use_tls: true,
                    calendar_sync_enabled: true,
                    bindings: Vec::new(),
                    mail_sync_enabled: true,
                    contacts_sync_enabled: true,
                    mail_sync_interval_seconds: None,
                    calendar_sync_interval_seconds: None,
                    contacts_sync_interval_seconds: None,
                })
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                crate::error::Error::AccountNotFound(id.to_string())
            }
            other => crate::error::Error::Database(other),
        })?;

    // Phase-3: bindings are the source of truth. Load them, then populate
    // the legacy AccountFull fields from the bindings + auth_method so the
    // wire format stays unchanged.
    account.bindings = crate::db::service_bindings::list_for_account(conn, id)?;
    account.populate_legacy_from_bindings();

    // Fetch password from the system keyring. OIDC/OAuth accounts don't
    // store a keyring password here — their tokens live under the
    // `.oauth` service — so a missing entry is expected, not an error.
    match crate::keyring::get_password(&account.id) {
        Ok(Some(pw)) => account.password = pw,
        Ok(None) => {
            log::debug!("No keyring password for account {}", account.id);
        }
        Err(e) => {
            log::warn!(
                "Could not read password from keyring for account {}: {}",
                account.id,
                e
            );
        }
    }

    Ok(account)
}

pub fn insert_account(conn: &Connection, id: &str, config: &AccountConfig) -> Result<()> {
    // Store real passwords in system keyring; skip OIDC accounts and oauth2 migration markers.
    // For bearer mode the value is trimmed (so a paste-with-trailing-newline
    // doesn't poison the Authorization header), AND the post-trim emptiness
    // is checked separately so a whitespace-only field is treated like
    // "leave empty" — otherwise the user could nuke the keyring token by
    // pasting blank lines into the API-token input.
    if !config.password.is_empty()
        && config.jmap_auth_method != "oidc"
        && !config.password.starts_with("oauth2:")
    {
        let secret = if config.jmap_auth_method == "bearer" {
            config.password.trim()
        } else {
            config.password.as_str()
        };
        if !secret.is_empty() {
            crate::keyring::set_password(id, secret)?;
        }
    }

    let auth_method =
        crate::db::service_bindings::auth_method_for(&config.provider, &config.jmap_auth_method);

    conn.execute(
        "INSERT INTO accounts (id, display_name, email, username, signature,
                               oidc_token_endpoint, oidc_client_id, auth_method,
                               pgp_attach_pubkey_on_sign, pgp_autocrypt_header,
                               pgp_encrypt_subject, pgp_encrypt_drafts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            id,
            config.display_name,
            config.email,
            config.username,
            config.signature,
            config.oidc_token_endpoint,
            config.oidc_client_id,
            auth_method,
            config.pgp_attach_pubkey_on_sign,
            config.pgp_autocrypt_header,
            config.pgp_encrypt_subject,
            config.pgp_encrypt_drafts,
        ],
    )?;
    crate::db::service_bindings::rebuild_for_account(
        conn,
        id,
        config_to_legacy_fields(id, true, config),
    )?;
    Ok(())
}

/// Build a `LegacyBindingFields` view over an `AccountConfig`. Used by
/// `insert_account` / `update_account` so writes go through the same
/// `derive_bindings` rules as the Phase-1 populate migration.
fn config_to_legacy_fields<'a>(
    account_id: &'a str,
    enabled: bool,
    config: &'a AccountConfig,
) -> crate::db::service_bindings::LegacyBindingFields<'a> {
    crate::db::service_bindings::LegacyBindingFields {
        account_id,
        enabled,
        provider: &config.provider,
        mail_protocol: &config.mail_protocol,
        imap_host: &config.imap_host,
        imap_port: config.imap_port,
        smtp_host: &config.smtp_host,
        smtp_port: config.smtp_port,
        use_tls: config.use_tls,
        jmap_url: &config.jmap_url,
        jmap_auth_method: &config.jmap_auth_method,
        oidc_token_endpoint: &config.oidc_token_endpoint,
        oidc_client_id: &config.oidc_client_id,
        caldav_url: &config.caldav_url,
        calendar_sync_enabled: config.calendar_sync_enabled,
        meet_url: &config.meet_url,
        meet_protocol: &config.meet_protocol,
        mail_sync_enabled: Some(config.mail_sync_enabled),
        contacts_sync_enabled: Some(config.contacts_sync_enabled),
        mail_sync_interval_seconds: config.mail_sync_interval_seconds,
        calendar_sync_interval_seconds: config.calendar_sync_interval_seconds,
        contacts_sync_interval_seconds: config.contacts_sync_interval_seconds,
    }
}

pub fn update_account(conn: &Connection, id: &str, config: &AccountConfig) -> Result<()> {
    // Only update keyring if a real password was provided; skip OIDC accounts and oauth2 markers.
    // Trim + post-trim empty check for bearer (see insert_account for the
    // full rationale — whitespace-only must round-trip to "no change",
    // not "clobber the existing token").
    if !config.password.is_empty()
        && config.jmap_auth_method != "oidc"
        && !config.password.starts_with("oauth2:")
    {
        let secret = if config.jmap_auth_method == "bearer" {
            config.password.trim()
        } else {
            config.password.as_str()
        };
        if !secret.is_empty() {
            crate::keyring::set_password(id, secret)?;
        }
    }

    let auth_method =
        crate::db::service_bindings::auth_method_for(&config.provider, &config.jmap_auth_method);

    let rows = conn.execute(
        "UPDATE accounts
         SET display_name=?1, email=?2, username=?3, signature=?4,
             oidc_token_endpoint=?5, oidc_client_id=?6, auth_method=?7,
             pgp_attach_pubkey_on_sign=?8, pgp_autocrypt_header=?9,
             pgp_encrypt_subject=?10, pgp_encrypt_drafts=?11,
             updated_at=CURRENT_TIMESTAMP
         WHERE id=?12",
        params![
            config.display_name,
            config.email,
            config.username,
            config.signature,
            config.oidc_token_endpoint,
            config.oidc_client_id,
            auth_method,
            config.pgp_attach_pubkey_on_sign,
            config.pgp_autocrypt_header,
            config.pgp_encrypt_subject,
            config.pgp_encrypt_drafts,
            id,
        ],
    )?;
    if rows == 0 {
        return Err(crate::error::Error::AccountNotFound(id.to_string()));
    }
    // Preserve the existing enabled flag — AccountConfig doesn't carry
    // enabled/disabled, so the previous binding's state would otherwise
    // get clobbered to `true` on every update.
    let enabled: bool = conn
        .query_row(
            "SELECT enabled FROM accounts WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .unwrap_or(true);
    crate::db::service_bindings::rebuild_for_account(
        conn,
        id,
        config_to_legacy_fields(id, enabled, config),
    )?;
    log::info!("Updated account {}", id);
    Ok(())
}

pub fn delete_account(conn: &Connection, id: &str) -> Result<()> {
    if crate::db::meet_meetings::account_has_lifecycle_references(conn, id)? {
        return Err(crate::error::Error::Other(
            "Cannot delete this account while meetings still require it. Delete the related calendar events and wait for meeting cleanup to finish.".into(),
        ));
    }
    // Remove password from keyring (best-effort, don't block deletion)
    if let Err(e) = crate::keyring::delete_password(id) {
        log::warn!("Failed to remove keyring entry for account {}: {}", id, e);
    }
    conn.execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // Phase-3 schema: identity-only columns plus service_bindings.
        conn.execute_batch(
            "
            CREATE TABLE accounts (
                id TEXT PRIMARY KEY,
                display_name TEXT NOT NULL,
                email TEXT NOT NULL,
                username TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                signature TEXT NOT NULL DEFAULT '',
                auth_method TEXT NOT NULL DEFAULT '',
                oidc_token_endpoint TEXT NOT NULL DEFAULT '',
                oidc_client_id TEXT NOT NULL DEFAULT '',
                pgp_attach_pubkey_on_sign INTEGER NOT NULL DEFAULT 1,
                pgp_autocrypt_header INTEGER NOT NULL DEFAULT 1,
                pgp_encrypt_subject INTEGER NOT NULL DEFAULT 1,
                pgp_encrypt_drafts INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE service_bindings (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                service TEXT NOT NULL,
                protocol TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                sync_interval_seconds INTEGER,
                config_json TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(account_id, service, protocol)
            );
            CREATE TABLE meet_meetings (
                event_id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                protocol TEXT NOT NULL,
                meeting_id TEXT NOT NULL,
                join_url TEXT NOT NULL
            );
            CREATE TABLE meet_pending_meetings (
                lifecycle_id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                protocol TEXT NOT NULL,
                meeting_id TEXT NOT NULL,
                join_url TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                cleanup_requested INTEGER NOT NULL DEFAULT 0
            );
            ",
        )
        .unwrap();
        conn
    }

    fn unique_id() -> String {
        format!("test-{}", uuid::Uuid::new_v4())
    }

    fn make_config(email: &str, name: &str) -> AccountConfig {
        AccountConfig {
            display_name: name.to_string(),
            email: email.to_string(),
            provider: "generic".to_string(),
            mail_protocol: "imap".to_string(),
            imap_host: "imap.example.com".to_string(),
            imap_port: 993,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            jmap_url: String::new(),
            caldav_url: String::new(),
            meet_url: String::new(),
            meet_protocol: String::new(),
            username: "user".to_string(),
            password: "secret123".to_string(),
            use_tls: true,
            signature: String::new(),
            jmap_auth_method: "basic".to_string(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
            calendar_sync_enabled: true,
            mail_sync_enabled: true,
            contacts_sync_enabled: true,
            mail_sync_interval_seconds: None,
            calendar_sync_interval_seconds: None,
            contacts_sync_interval_seconds: None,
            has_calendar_binding: false,
            has_contacts_binding: false,
            pgp_attach_pubkey_on_sign: true,
            pgp_autocrypt_header: true,
            pgp_encrypt_subject: true,
            pgp_encrypt_drafts: true,
        }
    }

    #[test]
    fn test_list_accounts_empty() {
        let conn = setup_db();
        let accounts = list_accounts(&conn).unwrap();
        assert!(accounts.is_empty());
    }

    #[test]
    fn test_insert_and_list_accounts() {
        let conn = setup_db();
        let id = unique_id();
        let config = make_config("alice@example.com", "Alice");
        insert_account(&conn, &id, &config).unwrap();

        let accounts = list_accounts(&conn).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].email, "alice@example.com");
        assert_eq!(accounts[0].display_name, "Alice");
        assert!(accounts[0].enabled);
        // Cleanup keyring
        crate::keyring::delete_password(&id).ok();
    }

    #[test]
    fn test_get_account_full_reads_all_fields() {
        let conn = setup_db();
        let id = unique_id();
        let config = make_config("alice@example.com", "Alice");
        insert_account(&conn, &id, &config).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert_eq!(full.email, "alice@example.com");
        assert_eq!(full.imap_host, "imap.example.com");
        assert_eq!(full.imap_port, 993);
        assert_eq!(full.smtp_host, "smtp.example.com");
        assert_eq!(full.smtp_port, 587);
        assert_eq!(full.username, "user");
        assert!(full.use_tls);
        crate::keyring::delete_password(&id).ok();
    }

    /// Regression: O365 (Graph) accounts sync over the Graph API but
    /// still SEND mail over SMTP+XOAUTH2. Their `mail/graph` binding
    /// carries no SMTP host, so `populate_legacy_from_bindings` must
    /// supply the fixed Microsoft relay (`smtp.office365.com:587`).
    /// Without it `smtp_host` is empty and every send dials a blank
    /// host — "No address associated with hostname". This regressed
    /// when dispatch reads moved to service_bindings (phase 2).
    #[test]
    fn test_get_account_full_graph_account_keeps_o365_smtp_host() {
        let conn = setup_db();
        let id = unique_id();
        let mut config = make_config("kushal@outlook.com", "Kushal");
        config.provider = "o365".to_string();
        config.mail_protocol = "graph".to_string();
        // O365 accounts have no IMAP host; the broken binding carried
        // no SMTP host either.
        config.imap_host = String::new();
        config.smtp_host = String::new();
        insert_account(&conn, &id, &config).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert_eq!(full.mail_protocol, "graph");
        assert_eq!(
            full.smtp_host, "smtp.office365.com",
            "Graph accounts must fall back to the O365 SMTP relay for sending"
        );
        assert_eq!(full.smtp_port, 587);
        crate::keyring::delete_password(&id).ok();
    }

    /// Regression: a whitespace-only API token (e.g. user pasted blank
    /// lines into the edit form) must not overwrite the existing
    /// keyring entry. The original guard only checked
    /// `!config.password.is_empty()`, so " \n" got past the gate, then
    /// trim() reduced it to "" and the keyring write clobbered the
    /// real token. apply_auth would then send `Bearer ` (empty value)
    /// on every request. The trimmed-emptiness check inside the gate
    /// makes whitespace-only input behave like "leave empty".
    #[test]
    fn test_update_account_whitespace_only_bearer_token_preserves_keyring() {
        let conn = setup_db();
        let id = unique_id();

        // 1. Create the account with a real token.
        let mut config = make_config("kushal@fastmail.com", "Kushal");
        config.provider = "fastmail".to_string();
        config.mail_protocol = "jmap".to_string();
        config.jmap_url = "https://api.fastmail.com".to_string();
        config.jmap_auth_method = "bearer".to_string();
        config.password = "fmu1-real-token".to_string();
        config.imap_host = String::new();
        config.smtp_host = String::new();
        insert_account(&conn, &id, &config).unwrap();
        assert_eq!(
            crate::keyring::get_password(&id).unwrap().as_deref(),
            Some("fmu1-real-token"),
            "setup precondition: real token must be in keyring",
        );

        // 2. Simulate the user pasting only whitespace into the
        //    "API token" field on the edit form and saving.
        let mut whitespace_update = config.clone();
        whitespace_update.password = "   \n\t  ".to_string();
        update_account(&conn, &id, &whitespace_update).unwrap();

        // 3. Keyring must still hold the original token — the
        //    whitespace-only paste should round-trip as "no change".
        assert_eq!(
            crate::keyring::get_password(&id).unwrap().as_deref(),
            Some("fmu1-real-token"),
            "whitespace-only bearer input must NOT overwrite the keyring entry",
        );
        crate::keyring::delete_password(&id).ok();
    }

    /// Regression: the settings list view reads `Account` rows via
    /// `list_accounts`, which derives `provider` from the Phase-2
    /// `auth_method` column. That column collapses Fastmail (password
    /// auth, no OAuth) to the same bucket as generic JMAP, so before
    /// this query was widened the FASTMAIL chip in SettingsView.vue
    /// would never trigger — Fastmail accounts displayed as
    /// JMAP/GENERIC despite the UI branch existing. The query now
    /// also pulls the JMAP binding's config_json and tags providers
    /// matching `api.fastmail.com` as "fastmail".
    #[test]
    fn test_list_accounts_recovers_fastmail_provider_from_url() {
        let conn = setup_db();
        let id = unique_id();
        let mut config = make_config("kushal@fastmail.com", "Kushal");
        config.provider = "fastmail".to_string();
        config.mail_protocol = "jmap".to_string();
        config.jmap_url = "https://api.fastmail.com".to_string();
        config.jmap_auth_method = "bearer".to_string();
        config.imap_host = String::new();
        config.smtp_host = String::new();
        insert_account(&conn, &id, &config).unwrap();

        let accounts = list_accounts(&conn).unwrap();
        let fm = accounts
            .iter()
            .find(|a| a.id == id)
            .expect("inserted account must appear in list");
        assert_eq!(fm.provider, "fastmail");
        assert_eq!(fm.mail_protocol, "jmap");
        crate::keyring::delete_password(&id).ok();
    }

    /// Regression: the Fastmail account-type tab saves `provider =
    /// "fastmail"`, but the `provider` value is recomputed on read-back
    /// from the Phase-2 `auth_method` column (which only knows "gmail" /
    /// "o365" / "generic"). To keep the list-view chip and edit-form
    /// label saying "FASTMAIL", `populate_legacy_from_bindings` recovers
    /// the tag by inspecting the JMAP URL.
    #[test]
    fn test_get_account_full_recovers_fastmail_provider_from_url() {
        let conn = setup_db();
        let id = unique_id();
        let mut config = make_config("kushal@fastmail.com", "Kushal");
        config.provider = "fastmail".to_string();
        config.mail_protocol = "jmap".to_string();
        config.jmap_url = "https://api.fastmail.com".to_string();
        config.jmap_auth_method = "bearer".to_string();
        config.imap_host = String::new();
        config.smtp_host = String::new();
        insert_account(&conn, &id, &config).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert_eq!(full.provider, "fastmail");
        assert_eq!(full.jmap_auth_method, "bearer");
        crate::keyring::delete_password(&id).ok();
    }

    /// Regression: a JMAP account saved with `jmap_auth_method = "bearer"`
    /// must read back as "bearer", not "basic". The Phase-2 `auth_method`
    /// column collapses both "basic" and "bearer" to "password", so
    /// `populate_legacy_from_bindings` has to recover the real choice from
    /// the JMAP binding's `config_json.auth_method`. Without that,
    /// Fastmail accounts saved via the UI would round-trip as Basic auth
    /// and the push loop would 401 with "Invalid Authorization header,
    /// not bearer".
    #[test]
    fn test_get_account_full_round_trips_bearer_auth_method() {
        let conn = setup_db();
        let id = unique_id();
        let mut config = make_config("kushal@fastmail.com", "Kushal");
        config.mail_protocol = "jmap".to_string();
        config.jmap_url = "https://api.fastmail.com".to_string();
        config.jmap_auth_method = "bearer".to_string();
        config.imap_host = String::new();
        config.smtp_host = String::new();
        insert_account(&conn, &id, &config).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert_eq!(
            full.jmap_auth_method, "bearer",
            "bearer auth must survive insert + read-back so apply_auth \
             routes the token through Authorization: Bearer"
        );
        crate::keyring::delete_password(&id).ok();
    }

    #[test]
    fn test_get_account_full_not_found() {
        let conn = setup_db();
        let result = get_account_full(&conn, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_update_account() {
        let conn = setup_db();
        let id = unique_id();
        let config = make_config("alice@example.com", "Alice");
        insert_account(&conn, &id, &config).unwrap();

        let mut updated = config.clone();
        updated.display_name = "Alice Updated".to_string();
        updated.imap_host = "new-imap.example.com".to_string();
        update_account(&conn, &id, &updated).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert_eq!(full.display_name, "Alice Updated");
        assert_eq!(full.imap_host, "new-imap.example.com");
        crate::keyring::delete_password(&id).ok();
    }

    #[test]
    fn test_update_nonexistent_account() {
        let conn = setup_db();
        let id = unique_id();
        let config = make_config("alice@example.com", "Alice");
        // Store password so keyring call doesn't fail
        crate::keyring::set_password(&id, "test").ok();
        let result = update_account(&conn, &id, &config);
        assert!(result.is_err());
        crate::keyring::delete_password(&id).ok();
    }

    #[test]
    fn test_delete_account() {
        let conn = setup_db();
        let id = unique_id();
        let config = make_config("alice@example.com", "Alice");
        insert_account(&conn, &id, &config).unwrap();

        delete_account(&conn, &id).unwrap();
        let accounts = list_accounts(&conn).unwrap();
        assert!(accounts.is_empty());
    }

    #[test]
    fn test_no_password_column_in_db() {
        let conn = setup_db();
        let has_password = conn
            .prepare("SELECT password FROM accounts LIMIT 0")
            .is_ok();
        assert!(!has_password, "DB should not have a password column");
    }

    #[test]
    fn test_multiple_accounts() {
        let conn = setup_db();
        let id1 = unique_id();
        let id2 = unique_id();
        insert_account(&conn, &id1, &make_config("alice@example.com", "Alice")).unwrap();
        insert_account(&conn, &id2, &make_config("bob@example.com", "Bob")).unwrap();

        let accounts = list_accounts(&conn).unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].display_name, "Alice");
        assert_eq!(accounts[1].display_name, "Bob");
        crate::keyring::delete_password(&id1).ok();
        crate::keyring::delete_password(&id2).ok();
    }

    #[test]
    fn test_signature_persists() {
        let conn = setup_db();
        let id = unique_id();
        let mut config = make_config("alice@example.com", "Alice");
        config.signature = "-- \nAlice Smith\nSenior Engineer".to_string();
        insert_account(&conn, &id, &config).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert_eq!(full.signature, "-- \nAlice Smith\nSenior Engineer");

        // Update signature
        let mut updated = config.clone();
        updated.signature = "-- \nAlice S.".to_string();
        update_account(&conn, &id, &updated).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert_eq!(full.signature, "-- \nAlice S.");
        crate::keyring::delete_password(&id).ok();
    }

    #[test]
    fn test_calendar_sync_enabled_serde_default_true() {
        // Older renderers may send AccountConfig payloads without the new
        // calendar_sync_enabled field; the serde default must yield true so
        // existing accounts keep syncing calendars.
        let json = r#"{
            "display_name": "Alice",
            "email": "a@example.com",
            "provider": "generic",
            "mail_protocol": "imap",
            "imap_host": "imap.example.com",
            "imap_port": 993,
            "smtp_host": "smtp.example.com",
            "smtp_port": 587,
            "jmap_url": "",
            "caldav_url": "",
            "username": "u",
            "password": "p",
            "use_tls": true
        }"#;
        let cfg: AccountConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.calendar_sync_enabled);
    }

    #[test]
    fn test_calendar_sync_enabled_defaults_true_and_persists_toggle() {
        let conn = setup_db();
        let id = unique_id();
        // calendar_sync_enabled now lives on the calendar binding's
        // `enabled` flag. Use an IMAP+CalDAV account so a calendar
        // binding actually gets created and the toggle has somewhere
        // to round-trip through.
        let mut config = make_config("alice@example.com", "Alice");
        config.caldav_url = "https://dav.example.com/cal".into();
        assert!(config.calendar_sync_enabled);
        insert_account(&conn, &id, &config).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert!(full.calendar_sync_enabled);

        let mut updated = config.clone();
        updated.calendar_sync_enabled = false;
        update_account(&conn, &id, &updated).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert!(!full.calendar_sync_enabled);
        crate::keyring::delete_password(&id).ok();
    }

    #[test]
    fn test_jmap_account() {
        let conn = setup_db();
        let id = unique_id();
        let mut config = make_config("kushal@example.com", "JMAP Account");
        config.mail_protocol = "jmap".to_string();
        config.jmap_url = "https://jmap.example.com".to_string();
        insert_account(&conn, &id, &config).unwrap();

        let full = get_account_full(&conn, &id).unwrap();
        assert_eq!(full.mail_protocol, "jmap");
        assert_eq!(full.jmap_url, "https://jmap.example.com");
        crate::keyring::delete_password(&id).ok();
    }
}
