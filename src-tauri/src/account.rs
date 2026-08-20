//! Focused account views consumed by service and provider boundaries.

/// Mail-only account configuration.
///
/// This is an owned snapshot because mail operations commonly cross spawned
/// async and blocking task boundaries. It intentionally excludes calendar,
/// contacts, meeting, UI, and OpenPGP settings. The password is loaded from
/// the keyring through `AccountFull` and must remain backend-only.
#[derive(Clone)]
pub struct MailAccountConfig {
    pub id: String,
    pub display_name: String,
    pub email: String,
    pub protocol: String,
    pub username: String,
    pub password: String,
    pub auth_method: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub use_tls: bool,
    pub jmap_url: String,
    pub jmap_auth_method: String,
    pub oidc_token_endpoint: String,
    pub oidc_client_id: String,
}
