//! Provider backends (ADR 0050).
//!
//! Each service domain (calendar, contacts, mail) has a trait with one
//! unit-struct implementor per provider, a static [`calendar::registry`]
//! and a `for_account` lookup keyed on the account's enabled service
//! binding — the same shape as `crate::meet::MeetProvider`. Command
//! handlers resolve a backend and make one trait call; they never
//! name-match on protocol strings.
//!
//! Unlike `meet`, backend methods take per-call context (`&AccountFull`,
//! and `&DbPool` for syncs) because provider syncs interleave remote I/O
//! with incremental local upserts and sync-token persistence.

pub mod calendar;
pub mod contacts;
pub mod mail;

#[cfg(test)]
pub(crate) mod testutil {
    use crate::calendar::CalendarEvent;
    use crate::contact::Contact;
    use crate::db::accounts::AccountFull;
    use crate::db::pool::DbPool;
    use crate::db::service_bindings::ServiceBinding;

    /// A minimal account with one enabled `service` binding for
    /// `protocol` and nothing else configured: no URLs, no stored
    /// credentials. The registry tests dispatch on it; the
    /// provider-contract tests rely on the missing credentials to
    /// observe each backend's error semantics without any network I/O.
    pub fn account(service: &str, protocol: &str) -> AccountFull {
        let bindings = if protocol.is_empty() {
            Vec::new()
        } else {
            vec![ServiceBinding {
                id: "b1".into(),
                account_id: "acc1".into(),
                service: service.into(),
                protocol: protocol.into(),
                enabled: true,
                sync_interval_seconds: None,
                config_json: "{}".into(),
            }]
        };
        AccountFull {
            id: "acc1".into(),
            display_name: "Test".into(),
            sender_name: "Test User".into(),
            email: "u@example.com".into(),
            provider: "generic".into(),
            mail_protocol: String::new(),
            imap_host: String::new(),
            imap_port: 0,
            smtp_host: String::new(),
            smtp_port: 0,
            jmap_url: String::new(),
            caldav_url: String::new(),
            meet_url: String::new(),
            meet_protocol: String::new(),
            username: "u@example.com".into(),
            password: String::new(),
            use_tls: true,
            enabled: true,
            signature: String::new(),
            jmap_auth_method: String::new(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
            calendar_sync_enabled: true,
            auth_method: String::new(),
            bindings,
            mail_sync_enabled: true,
            contacts_sync_enabled: true,
            mail_sync_interval_seconds: None,
            calendar_sync_interval_seconds: None,
            contacts_sync_interval_seconds: None,
            pgp_attach_pubkey_on_sign: false,
            pgp_autocrypt_header: false,
            pgp_encrypt_subject: false,
            pgp_encrypt_drafts: false,
        }
    }

    pub fn event() -> CalendarEvent {
        CalendarEvent {
            id: "e1".into(),
            account_id: "acc1".into(),
            calendar_id: "cal1".into(),
            uid: None,
            title: "Standup".into(),
            description: None,
            location: None,
            start_time: "2026-07-16T10:00:00".into(),
            end_time: "2026-07-16T10:30:00".into(),
            all_day: false,
            timezone: None,
            recurrence_rule: None,
            organizer_email: None,
            attendees_json: None,
            my_status: None,
            source_message_id: None,
            ical_data: None,
            remote_id: None,
            etag: None,
        }
    }

    pub fn contact() -> Contact {
        Contact {
            id: "c1".into(),
            book_id: "b1".into(),
            uid: None,
            display_name: "Ada Lovelace".into(),
            emails_json: r#"[{"email":"ada@example.com","label":"work"}]"#.into(),
            phones_json: "[]".into(),
            addresses_json: "[]".into(),
            organization: None,
            title: None,
            notes: None,
            vcard_data: None,
            remote_id: None,
            etag: None,
        }
    }

    /// One-connection pool on a temp file. The contract tests only
    /// need it to satisfy `sync(&DbPool, ...)` — every fixture's
    /// credential failure fires before any DB access.
    pub fn temp_pool() -> (tempfile::TempDir, DbPool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = DbPool::new(&dir.path().join("test.db"), 1).unwrap();
        (dir, pool)
    }
}
