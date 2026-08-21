//! Injectable credentials and OAuth token-endpoint dependencies for providers.

use std::sync::Arc;

use async_trait::async_trait;

use crate::account::MailAccountConfig;
use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::mail::jmap::JmapConfig;
use crate::oauth::{OAuthProvider, OAuthTokens};

const PROVIDER_HTTP_TIMEOUT_SECS: u64 = 30;
const USER_AGENT: &str = concat!("Chithi/", env!("CARGO_PKG_VERSION"));

/// Microsoft Graph token families have deliberately different consent policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphTokenPurpose {
    Baseline,
    Rooms,
}

#[derive(Clone, Eq, PartialEq)]
pub struct MailCredentials {
    pub secret: String,
    pub use_xoauth2: bool,
}

/// Persistence boundary for backend-only OAuth and provider credentials.
pub trait OAuthTokenStore: Send + Sync {
    fn load(&self, account_id: &str) -> Result<Option<OAuthTokens>>;
    fn store(&self, account_id: &str, tokens: &OAuthTokens) -> Result<()>;
    fn delete(&self, account_id: &str) -> Result<()>;
}

pub struct SystemOAuthTokenStore;

impl OAuthTokenStore for SystemOAuthTokenStore {
    fn load(&self, account_id: &str) -> Result<Option<OAuthTokens>> {
        crate::oauth::load_tokens(account_id)
    }

    fn store(&self, account_id: &str, tokens: &OAuthTokens) -> Result<()> {
        crate::oauth::store_tokens(account_id, tokens)
    }

    fn delete(&self, account_id: &str) -> Result<()> {
        crate::oauth::delete_tokens(account_id)
    }
}

/// HTTP boundary for authorization-code exchange and token refresh requests.
#[async_trait]
pub trait TokenEndpointClient: Send + Sync {
    async fn exchange_code(
        &self,
        provider: &OAuthProvider,
        code: &str,
        port: u16,
        code_verifier: Option<&str>,
    ) -> Result<OAuthTokens>;

    async fn refresh(&self, provider: &OAuthProvider, refresh_token: &str) -> Result<OAuthTokens>;

    async fn refresh_scoped(
        &self,
        provider: &OAuthProvider,
        refresh_token: &str,
        scopes: &str,
    ) -> Result<OAuthTokens>;

    async fn refresh_dynamic(
        &self,
        token_url: &str,
        refresh_token: &str,
        client_id: &str,
    ) -> Result<OAuthTokens>;
}

/// Production token endpoint transport. Static OAuth and dynamic OIDC retain
/// their existing distinct timeout policies.
pub struct ReqwestTokenEndpointClient {
    oauth_http: reqwest::Client,
    oidc_http: reqwest::Client,
}

impl ReqwestTokenEndpointClient {
    pub fn new(oauth_http: reqwest::Client, oidc_http: reqwest::Client) -> Self {
        Self {
            oauth_http,
            oidc_http,
        }
    }

    pub fn production() -> Result<Self> {
        let oidc_http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| Error::Other(format!("HTTP client error: {}", error)))?;
        Ok(Self::new(reqwest::Client::new(), oidc_http))
    }
}

#[async_trait]
impl TokenEndpointClient for ReqwestTokenEndpointClient {
    async fn exchange_code(
        &self,
        provider: &OAuthProvider,
        code: &str,
        port: u16,
        code_verifier: Option<&str>,
    ) -> Result<OAuthTokens> {
        crate::oauth::exchange_code_with_client(
            provider,
            code,
            port,
            code_verifier,
            &self.oauth_http,
        )
        .await
    }

    async fn refresh(&self, provider: &OAuthProvider, refresh_token: &str) -> Result<OAuthTokens> {
        crate::oauth::refresh_access_token_with_client(provider, refresh_token, &self.oauth_http)
            .await
    }

    async fn refresh_scoped(
        &self,
        provider: &OAuthProvider,
        refresh_token: &str,
        scopes: &str,
    ) -> Result<OAuthTokens> {
        crate::oauth::refresh_with_scopes_with_client(
            provider,
            refresh_token,
            scopes,
            &self.oauth_http,
        )
        .await
    }

    async fn refresh_dynamic(
        &self,
        token_url: &str,
        refresh_token: &str,
        client_id: &str,
    ) -> Result<OAuthTokens> {
        crate::oauth::refresh_token_dynamic_with_client(
            token_url,
            refresh_token,
            client_id,
            &self.oidc_http,
        )
        .await
    }
}

/// Provider-facing credential policy. Callers choose a named purpose rather
/// than supplying arbitrary OAuth scopes.
#[async_trait]
pub trait ProviderCredentials: Send + Sync {
    /// Coordinator used for per-account token refresh, when the implementation
    /// participates in token lifecycle serialization.
    fn account_coordinator(&self) -> Option<Arc<ProviderAccountCoordinator>> {
        None
    }

    async fn google_access_token(&self, account_id: &str) -> Result<String>;
    async fn graph_access_token(
        &self,
        account_id: &str,
        purpose: GraphTokenPurpose,
    ) -> Result<String>;
    async fn mail_credentials_for(&self, account: &MailAccountConfig) -> Result<MailCredentials>;
    async fn jmap_config_for(&self, account: &MailAccountConfig) -> Result<JmapConfig>;

    /// Compatibility wrapper for service paths not yet migrated to focused
    /// account views.
    async fn mail_credentials(&self, account: &AccountFull) -> Result<MailCredentials> {
        let config = account.mail_config();
        self.mail_credentials_for(&config).await
    }

    /// Compatibility wrapper for shared JMAP calendar/contact paths.
    async fn jmap_config(&self, account: &AccountFull) -> Result<JmapConfig> {
        let config = account.mail_config();
        self.jmap_config_for(&config).await
    }
    async fn jmap_push_access_token(
        &self,
        account_id: &str,
        token_endpoint: &str,
        client_id: &str,
    ) -> Result<Option<String>>;
    async fn zoom_access_token(&self, account_id: &str) -> Result<String>;
    async fn matrix_access_token(&self, account_id: &str) -> Result<String>;
    async fn talk_app_password(&self, account_id: &str) -> Result<String>;
}

pub struct ProviderCredentialService {
    tokens: Arc<dyn OAuthTokenStore>,
    endpoint: Arc<dyn TokenEndpointClient>,
    account_locks: Arc<ProviderAccountCoordinator>,
}

#[derive(Default)]
pub struct ProviderAccountCoordinator {
    locks: std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ProviderAccountCoordinator {
    pub async fn lock_account(&self, account_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let account_lock = self
            .locks
            .lock()
            .unwrap()
            .entry(account_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        account_lock.lock_owned().await
    }
}

struct OAuthTokenLifecycle {
    tokens: Arc<dyn OAuthTokenStore>,
    account_locks: Arc<ProviderAccountCoordinator>,
}

impl OAuthTokenLifecycle {
    fn store_zoom_and_commit<F>(
        &self,
        account_id: &str,
        tokens: &OAuthTokens,
        commit: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        if let Err(error) = self.tokens.store(account_id, tokens) {
            return match self.tokens.delete(account_id) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(compensation_error(
                    "storing Zoom tokens",
                    error,
                    "removing a partial token write",
                    cleanup_error,
                )),
            };
        }

        if let Err(error) = commit() {
            return match self.tokens.delete(account_id) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(compensation_error(
                    "committing the Zoom account",
                    error,
                    "removing its tokens",
                    cleanup_error,
                )),
            };
        }
        crate::oauth::clear_reauth_required(account_id);
        Ok(())
    }

    fn delete_zoom_and_commit<F>(&self, account_id: &str, commit: F) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        let restore_reauth = crate::oauth::is_reauth_required(account_id);
        let previous = self.tokens.load(account_id)?;
        if let Err(error) = self.tokens.delete(account_id) {
            let result = match previous {
                Some(tokens) => match self.tokens.store(account_id, &tokens) {
                    Ok(()) => Err(error),
                    Err(restore_error) => Err(compensation_error(
                        "deleting Zoom tokens",
                        error,
                        "restoring the token snapshot",
                        restore_error,
                    )),
                },
                None => Err(error),
            };
            if restore_reauth {
                crate::oauth::mark_reauth_required(account_id);
            }
            return result;
        }

        if let Err(error) = commit() {
            let result = match previous {
                Some(tokens) => match self.tokens.store(account_id, &tokens) {
                    Ok(()) => Err(error),
                    Err(restore_error) => Err(compensation_error(
                        "committing Zoom account deletion",
                        error,
                        "restoring its tokens",
                        restore_error,
                    )),
                },
                None => Err(error),
            };
            if restore_reauth {
                crate::oauth::mark_reauth_required(account_id);
            }
            return result;
        }
        crate::oauth::clear_reauth_required(account_id);
        Ok(())
    }

    fn replace_zoom_and_commit<F>(
        &self,
        account_id: &str,
        tokens: &OAuthTokens,
        commit: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        let restore_reauth = crate::oauth::is_reauth_required(account_id);
        let previous = self.tokens.load(account_id)?;
        if let Err(error) = self.tokens.store(account_id, tokens) {
            let compensation = match previous.as_ref() {
                Some(previous) => self.tokens.store(account_id, previous),
                None => self.tokens.delete(account_id),
            };
            if restore_reauth {
                crate::oauth::mark_reauth_required(account_id);
            }
            return match compensation {
                Ok(()) => Err(error),
                Err(restore_error) => Err(compensation_error(
                    "replacing Zoom tokens",
                    error,
                    "restoring the token snapshot",
                    restore_error,
                )),
            };
        }
        if let Err(error) = commit() {
            let compensation = match previous.as_ref() {
                Some(previous) => self.tokens.store(account_id, previous),
                None => self.tokens.delete(account_id),
            };
            if restore_reauth {
                crate::oauth::mark_reauth_required(account_id);
            }
            return match compensation {
                Ok(()) => Err(error),
                Err(restore_error) => Err(compensation_error(
                    "committing Zoom reauthentication",
                    error,
                    "restoring the token snapshot",
                    restore_error,
                )),
            };
        }
        crate::oauth::clear_reauth_required(account_id);
        Ok(())
    }
}

pub struct ZoomTokenLifecycleGuard<'a> {
    lifecycle: &'a OAuthTokenLifecycle,
    account_id: &'a str,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl ZoomTokenLifecycleGuard<'_> {
    pub fn store_and_commit<F>(&self, tokens: &OAuthTokens, commit: F) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        self.lifecycle
            .store_zoom_and_commit(self.account_id, tokens, commit)
    }

    pub fn delete_and_commit<F>(&self, commit: F) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        self.lifecycle
            .delete_zoom_and_commit(self.account_id, commit)
    }

    pub fn replace_and_commit<F>(&self, tokens: &OAuthTokens, commit: F) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        self.lifecycle
            .replace_zoom_and_commit(self.account_id, tokens, commit)
    }
}

fn compensation_error(
    operation: &str,
    error: Error,
    compensation: &str,
    compensation_error: Error,
) -> Error {
    Error::Other(format!(
        "failed while {operation}: {error}; also failed while {compensation}: {compensation_error}"
    ))
}

/// Shared HTTP transports and test-overridable provider API endpoints.
/// Cloning a `reqwest::Client` retains its connection pool.
pub struct ProviderTransports {
    pub graph_http: reqwest::Client,
    pub graph_endpoints: crate::mail::graph::GraphEndpoints,
    pub google_http: reqwest::Client,
    pub google_endpoints: crate::mail::google::GoogleEndpoints,
    pub jmap_discovery_http: reqwest::Client,
    pub jmap_api_http: reqwest::Client,
    pub jmap_submission_http: reqwest::Client,
    pub jmap_sse_http: reqwest::Client,
    pub oidc_http: reqwest::Client,
    pub oidc_poll_http: reqwest::Client,
    pub dav_http: reqwest::Client,
    pub zoom_http: reqwest::Client,
    pub zoom_api_root: String,
    pub matrix_http: reqwest::Client,
    pub talk_http: reqwest::Client,
}

impl ProviderTransports {
    pub fn production() -> Result<Self> {
        let jmap_discovery_http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|error| Error::Other(error.to_string()))?;
        let jmap_api_http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| Error::Other(error.to_string()))?;
        let jmap_submission_http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| Error::Other(error.to_string()))?;

        Ok(Self {
            graph_http: reqwest::Client::new(),
            graph_endpoints: crate::mail::graph::GraphEndpoints::default(),
            google_http: reqwest::Client::new(),
            google_endpoints: crate::mail::google::GoogleEndpoints::default(),
            jmap_discovery_http,
            jmap_api_http,
            jmap_submission_http,
            // No overall timeout: EventSource responses are long-lived and
            // enforce a per-chunk read timeout in the push loop.
            jmap_sse_http: reqwest::Client::builder()
                .build()
                .map_err(|error| Error::Other(error.to_string()))?,
            oidc_http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|error| Error::Other(format!("HTTP client build error: {}", error)))?,
            oidc_poll_http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|error| Error::Other(format!("HTTP client build error: {}", error)))?,
            dav_http: crate::mail::dav_http::build_dav_client()?,
            zoom_http: build_meet_http_client("zoom")?,
            zoom_api_root: "https://api.zoom.us/v2".into(),
            matrix_http: build_meet_http_client("matrix")?,
            talk_http: build_meet_http_client("talk")?,
        })
    }
}

fn build_meet_http_client(provider: &str) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(PROVIDER_HTTP_TIMEOUT_SECS))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|error| Error::Other(format!("{} http client: {}", provider, error)))
}

/// Focused provider dependencies shared by backend and meet contexts.
pub struct ProviderServices {
    credentials: Arc<dyn ProviderCredentials>,
    token_store: Arc<dyn OAuthTokenStore>,
    token_endpoint: Arc<dyn TokenEndpointClient>,
    token_lifecycle: OAuthTokenLifecycle,
    pub transports: ProviderTransports,
}

impl ProviderServices {
    pub fn new(
        credentials: Arc<dyn ProviderCredentials>,
        token_store: Arc<dyn OAuthTokenStore>,
        token_endpoint: Arc<dyn TokenEndpointClient>,
        transports: ProviderTransports,
    ) -> Self {
        let account_locks = credentials
            .account_coordinator()
            .unwrap_or_else(|| Arc::new(ProviderAccountCoordinator::default()));
        Self {
            credentials,
            token_store: token_store.clone(),
            token_endpoint,
            token_lifecycle: OAuthTokenLifecycle {
                tokens: token_store,
                account_locks,
            },
            transports,
        }
    }

    pub fn production() -> Result<Self> {
        let token_endpoint: Arc<dyn TokenEndpointClient> =
            Arc::new(ReqwestTokenEndpointClient::production()?);
        let token_store: Arc<dyn OAuthTokenStore> = Arc::new(SystemOAuthTokenStore);
        let credentials = Arc::new(ProviderCredentialService::new(
            token_store.clone(),
            token_endpoint.clone(),
        ));
        Ok(Self::new(
            credentials,
            token_store,
            token_endpoint,
            ProviderTransports::production()?,
        ))
    }

    pub fn credentials(&self) -> &dyn ProviderCredentials {
        self.credentials.as_ref()
    }

    pub fn token_endpoint(&self) -> &dyn TokenEndpointClient {
        self.token_endpoint.as_ref()
    }

    pub fn token_store(&self) -> &dyn OAuthTokenStore {
        self.token_store.as_ref()
    }

    pub async fn lock_zoom_tokens<'a>(
        &'a self,
        account_id: &'a str,
    ) -> ZoomTokenLifecycleGuard<'a> {
        ZoomTokenLifecycleGuard {
            lifecycle: &self.token_lifecycle,
            account_id,
            _guard: self
                .token_lifecycle
                .account_locks
                .lock_account(account_id)
                .await,
        }
    }

    pub async fn graph_client(
        &self,
        account_id: &str,
        purpose: GraphTokenPurpose,
    ) -> Result<crate::mail::graph::GraphClient> {
        let token = self
            .credentials
            .graph_access_token(account_id, purpose)
            .await?;
        Ok(crate::mail::graph::GraphClient::with_client(
            self.transports.graph_http.clone(),
            &token,
            self.transports.graph_endpoints.clone(),
        ))
    }

    pub async fn google_client(
        &self,
        account_id: &str,
    ) -> Result<crate::mail::google::GoogleClient> {
        let token = self.credentials.google_access_token(account_id).await?;
        Ok(crate::mail::google::GoogleClient::with_client(
            self.transports.google_http.clone(),
            &token,
            self.transports.google_endpoints.clone(),
        ))
    }

    pub async fn jmap_client(
        &self,
        account: &AccountFull,
    ) -> Result<(JmapConfig, crate::mail::jmap::JmapConnection)> {
        let config = account.mail_config();
        self.jmap_client_for(&config).await
    }

    pub async fn jmap_client_for(
        &self,
        account: &MailAccountConfig,
    ) -> Result<(JmapConfig, crate::mail::jmap::JmapConnection)> {
        let config = self.credentials.jmap_config_for(account).await?;
        let connection = crate::mail::jmap::JmapConnection::connect_with_clients(
            &config,
            self.transports.jmap_discovery_http.clone(),
            self.transports.jmap_api_http.clone(),
            self.transports.jmap_submission_http.clone(),
        )
        .await?;
        Ok((config, connection))
    }

    pub async fn caldav_client(
        &self,
        config: &crate::mail::caldav::CalDavConfig,
    ) -> Result<crate::mail::caldav::CalDavClient> {
        crate::mail::caldav::CalDavClient::connect_with_client(
            config,
            self.transports.dav_http.clone(),
        )
        .await
    }

    pub async fn carddav_client(
        &self,
        carddav_url: &str,
        username: &str,
        password: &str,
        email: &str,
    ) -> Result<crate::mail::carddav::CardDavClient> {
        crate::mail::carddav::CardDavClient::connect_with_client(
            carddav_url,
            username,
            password,
            email,
            self.transports.dav_http.clone(),
        )
        .await
    }
}

impl ProviderCredentialService {
    pub fn new(tokens: Arc<dyn OAuthTokenStore>, endpoint: Arc<dyn TokenEndpointClient>) -> Self {
        Self {
            tokens,
            endpoint,
            account_locks: Arc::new(ProviderAccountCoordinator::default()),
        }
    }

    fn required_tokens(&self, account_id: &str, missing_message: &str) -> Result<OAuthTokens> {
        self.tokens
            .load(account_id)?
            .ok_or_else(|| Error::Other(missing_message.into()))
    }

    async fn jmap_oidc_access_token(&self, account: &MailAccountConfig) -> Result<Option<String>> {
        if account.jmap_auth_method != "oidc" {
            return Ok(None);
        }

        let _guard = self.account_locks.lock_account(&account.id).await;
        let tokens =
            self.required_tokens(&account.id, "No OIDC tokens found. Please sign in again.")?;
        if !tokens.is_expired() {
            return Ok(Some(tokens.access_token));
        }

        let refresh_token = tokens
            .refresh_token
            .ok_or_else(|| Error::Other("No refresh token. Please sign in again.".into()))?;
        if account.oidc_token_endpoint.is_empty() {
            return Err(Error::Other(
                "OIDC token endpoint not configured. Please sign in again.".into(),
            ));
        }
        if account.oidc_client_id.is_empty() {
            return Err(Error::Other(
                "OIDC client_id not configured. Please sign in again.".into(),
            ));
        }

        let refreshed = self
            .endpoint
            .refresh_dynamic(
                &account.oidc_token_endpoint,
                &refresh_token,
                &account.oidc_client_id,
            )
            .await?;
        self.tokens.store(&account.id, &refreshed)?;
        Ok(Some(refreshed.access_token))
    }
}

#[async_trait]
impl ProviderCredentials for ProviderCredentialService {
    fn account_coordinator(&self) -> Option<Arc<ProviderAccountCoordinator>> {
        Some(self.account_locks.clone())
    }

    async fn google_access_token(&self, account_id: &str) -> Result<String> {
        let _guard = self.account_locks.lock_account(account_id).await;
        let tokens = self.required_tokens(
            account_id,
            "No Google OAuth tokens. Please sign in with Google in Settings.",
        )?;
        if !tokens.is_expired() {
            return Ok(tokens.access_token);
        }

        let refresh_token = tokens
            .refresh_token
            .ok_or_else(|| Error::Other("No refresh token".into()))?;
        let refreshed = self
            .endpoint
            .refresh(&crate::oauth::GOOGLE, &refresh_token)
            .await?;
        self.tokens.store(account_id, &refreshed)?;
        Ok(refreshed.access_token)
    }

    async fn graph_access_token(
        &self,
        account_id: &str,
        purpose: GraphTokenPurpose,
    ) -> Result<String> {
        let _guard = self.account_locks.lock_account(account_id).await;
        crate::oauth::ensure_not_reauth_required(account_id)?;
        let tokens = self.required_tokens(
            account_id,
            "No O365 OAuth tokens. Please sign in with Microsoft.",
        )?;
        let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
            Error::Other("No refresh token for O365. Please sign in again.".into())
        })?;
        let (scopes, latch_reauth) = match purpose {
            GraphTokenPurpose::Baseline => (crate::oauth::MICROSOFT_GRAPH_SCOPES, true),
            GraphTokenPurpose::Rooms => (crate::oauth::MICROSOFT_GRAPH_ROOM_SCOPES, false),
        };
        let refreshed = self
            .endpoint
            .refresh_scoped(&crate::oauth::MICROSOFT, &refresh_token, scopes)
            .await
            .map_err(|error| {
                if latch_reauth {
                    crate::oauth::auth_required_on_invalid_grant(account_id, error)
                } else {
                    error
                }
            })?;

        if refreshed.refresh_token.is_some() {
            self.tokens.store(
                account_id,
                &OAuthTokens {
                    access_token: tokens.access_token,
                    refresh_token: refreshed.refresh_token.clone(),
                    expires_at: tokens.expires_at,
                },
            )?;
        }
        Ok(refreshed.access_token)
    }

    async fn mail_credentials_for(&self, account: &MailAccountConfig) -> Result<MailCredentials> {
        if account.auth_method != "oauth-microsoft" {
            return Ok(MailCredentials {
                secret: account.password.clone(),
                use_xoauth2: false,
            });
        }

        let _guard = self.account_locks.lock_account(&account.id).await;
        crate::oauth::ensure_not_reauth_required(&account.id)?;
        let tokens = self.required_tokens(
            &account.id,
            "No O365 OAuth tokens. Please sign in with Microsoft.",
        )?;
        let refresh_token = tokens.refresh_token.ok_or_else(|| {
            Error::Other("No O365 refresh token. Please sign in with Microsoft.".into())
        })?;
        let refreshed = self
            .endpoint
            .refresh_scoped(
                &crate::oauth::MICROSOFT,
                &refresh_token,
                crate::oauth::MICROSOFT_IMAP_SCOPES,
            )
            .await
            .map_err(|error| crate::oauth::auth_required_on_invalid_grant(&account.id, error))?;
        self.tokens.store(&account.id, &refreshed)?;
        Ok(MailCredentials {
            secret: refreshed.access_token,
            use_xoauth2: true,
        })
    }

    async fn jmap_config_for(&self, account: &MailAccountConfig) -> Result<JmapConfig> {
        let mut config = JmapConfig::from_mail_account(account);
        if let Some(token) = self.jmap_oidc_access_token(account).await? {
            config.access_token = Some(token);
        }
        Ok(config)
    }

    async fn jmap_push_access_token(
        &self,
        account_id: &str,
        token_endpoint: &str,
        client_id: &str,
    ) -> Result<Option<String>> {
        let _guard = self.account_locks.lock_account(account_id).await;
        let Some(tokens) = self.tokens.load(account_id)? else {
            return Ok(None);
        };
        if !tokens.is_expired() {
            return Ok(Some(tokens.access_token));
        }
        let Some(refresh_token) = tokens.refresh_token else {
            return Ok(Some(tokens.access_token));
        };
        if token_endpoint.is_empty() || client_id.is_empty() {
            return Ok(Some(tokens.access_token));
        }
        let refreshed = self
            .endpoint
            .refresh_dynamic(token_endpoint, &refresh_token, client_id)
            .await?;
        self.tokens.store(account_id, &refreshed)?;
        Ok(Some(refreshed.access_token))
    }

    async fn zoom_access_token(&self, account_id: &str) -> Result<String> {
        let _guard = self.account_locks.lock_account(account_id).await;
        crate::oauth::ensure_not_reauth_required(account_id)?;
        let tokens =
            self.required_tokens(account_id, "Zoom: no tokens in keyring; sign in again")?;
        if !tokens.is_expired() {
            return Ok(tokens.access_token);
        }
        let refresh_token = tokens.refresh_token.ok_or_else(|| {
            Error::Other("Zoom: access token expired and no refresh token; sign in again".into())
        })?;
        let refreshed = self
            .endpoint
            .refresh(&crate::oauth::ZOOM, &refresh_token)
            .await
            .map_err(|error| crate::oauth::auth_required_on_invalid_grant(account_id, error))?;
        self.tokens.store(account_id, &refreshed)?;
        // Injected stores do not necessarily delegate to oauth::store_tokens,
        // so clear only after their persistence boundary reports success.
        crate::oauth::clear_reauth_required(account_id);
        Ok(refreshed.access_token)
    }

    async fn matrix_access_token(&self, account_id: &str) -> Result<String> {
        Ok(self
            .required_tokens(
                account_id,
                "Matrix: no access token in keyring; sign in again",
            )?
            .access_token)
    }

    async fn talk_app_password(&self, account_id: &str) -> Result<String> {
        Ok(self
            .required_tokens(
                account_id,
                "Nextcloud Talk: no app password in keyring; sign in again",
            )?
            .access_token)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct MemoryTokenStore {
        tokens: Mutex<HashMap<String, OAuthTokens>>,
    }

    impl OAuthTokenStore for MemoryTokenStore {
        fn load(&self, account_id: &str) -> Result<Option<OAuthTokens>> {
            Ok(self.tokens.lock().unwrap().get(account_id).cloned())
        }

        fn store(&self, account_id: &str, tokens: &OAuthTokens) -> Result<()> {
            self.tokens
                .lock()
                .unwrap()
                .insert(account_id.to_string(), tokens.clone());
            Ok(())
        }

        fn delete(&self, account_id: &str) -> Result<()> {
            self.tokens.lock().unwrap().remove(account_id);
            Ok(())
        }
    }

    #[derive(Default)]
    struct LatchAwareMemoryTokenStore {
        inner: MemoryTokenStore,
    }

    impl OAuthTokenStore for LatchAwareMemoryTokenStore {
        fn load(&self, account_id: &str) -> Result<Option<OAuthTokens>> {
            self.inner.load(account_id)
        }

        fn store(&self, account_id: &str, tokens: &OAuthTokens) -> Result<()> {
            self.inner.store(account_id, tokens)?;
            crate::oauth::clear_reauth_required(account_id);
            Ok(())
        }

        fn delete(&self, account_id: &str) -> Result<()> {
            self.inner.delete(account_id)?;
            crate::oauth::clear_reauth_required(account_id);
            Ok(())
        }
    }

    struct DeleteFailingTokenStore {
        inner: MemoryTokenStore,
    }

    impl OAuthTokenStore for DeleteFailingTokenStore {
        fn load(&self, account_id: &str) -> Result<Option<OAuthTokens>> {
            self.inner.load(account_id)
        }

        fn store(&self, account_id: &str, tokens: &OAuthTokens) -> Result<()> {
            self.inner.store(account_id, tokens)?;
            crate::oauth::clear_reauth_required(account_id);
            Ok(())
        }

        fn delete(&self, account_id: &str) -> Result<()> {
            // Simulate a store that removes the credential and clears its
            // latch before reporting a later backend failure.
            self.inner.delete(account_id)?;
            crate::oauth::clear_reauth_required(account_id);
            Err(Error::Other("injected delete failure".into()))
        }
    }

    struct FailOnceAfterWriteTokenStore {
        inner: MemoryTokenStore,
        fail_next_store: AtomicBool,
    }

    impl OAuthTokenStore for FailOnceAfterWriteTokenStore {
        fn load(&self, account_id: &str) -> Result<Option<OAuthTokens>> {
            self.inner.load(account_id)
        }

        fn store(&self, account_id: &str, tokens: &OAuthTokens) -> Result<()> {
            self.inner.store(account_id, tokens)?;
            if self.fail_next_store.swap(false, Ordering::SeqCst) {
                return Err(Error::Other("injected failure after token write".into()));
            }
            Ok(())
        }

        fn delete(&self, account_id: &str) -> Result<()> {
            self.inner.delete(account_id)
        }
    }

    struct FakeTokenEndpoint {
        refreshed: OAuthTokens,
        scopes: Mutex<Vec<String>>,
    }

    struct ScriptedTokenEndpoint {
        responses: Mutex<VecDeque<Result<OAuthTokens>>>,
        refresh_calls: AtomicUsize,
        block_first: bool,
        first_entered: tokio::sync::Notify,
        release_first: tokio::sync::Notify,
    }

    impl ScriptedTokenEndpoint {
        fn new(responses: Vec<Result<OAuthTokens>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                refresh_calls: AtomicUsize::new(0),
                block_first: false,
                first_entered: tokio::sync::Notify::new(),
                release_first: tokio::sync::Notify::new(),
            }
        }

        fn blocking(responses: Vec<Result<OAuthTokens>>) -> Self {
            Self {
                block_first: true,
                ..Self::new(responses)
            }
        }

        fn refresh_calls(&self) -> usize {
            self.refresh_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl TokenEndpointClient for ScriptedTokenEndpoint {
        async fn exchange_code(
            &self,
            _provider: &OAuthProvider,
            _code: &str,
            _port: u16,
            _code_verifier: Option<&str>,
        ) -> Result<OAuthTokens> {
            unreachable!()
        }

        async fn refresh(
            &self,
            _provider: &OAuthProvider,
            _refresh_token: &str,
        ) -> Result<OAuthTokens> {
            let call = self.refresh_calls.fetch_add(1, Ordering::SeqCst);
            if self.block_first && call == 0 {
                self.first_entered.notify_one();
                self.release_first.notified().await;
            }
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected token refresh")
        }

        async fn refresh_scoped(
            &self,
            _provider: &OAuthProvider,
            _refresh_token: &str,
            _scopes: &str,
        ) -> Result<OAuthTokens> {
            unreachable!()
        }

        async fn refresh_dynamic(
            &self,
            _token_url: &str,
            _refresh_token: &str,
            _client_id: &str,
        ) -> Result<OAuthTokens> {
            unreachable!()
        }
    }

    struct CoordinatedBlockingCredentials {
        coordinator: Arc<ProviderAccountCoordinator>,
        tokens: Arc<MemoryTokenStore>,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait]
    impl ProviderCredentials for CoordinatedBlockingCredentials {
        fn account_coordinator(&self) -> Option<Arc<ProviderAccountCoordinator>> {
            Some(self.coordinator.clone())
        }

        async fn google_access_token(&self, _account_id: &str) -> Result<String> {
            unreachable!()
        }

        async fn graph_access_token(
            &self,
            _account_id: &str,
            _purpose: GraphTokenPurpose,
        ) -> Result<String> {
            unreachable!()
        }

        async fn mail_credentials_for(
            &self,
            _account: &MailAccountConfig,
        ) -> Result<MailCredentials> {
            unreachable!()
        }

        async fn jmap_config_for(&self, _account: &MailAccountConfig) -> Result<JmapConfig> {
            unreachable!()
        }

        async fn jmap_push_access_token(
            &self,
            _account_id: &str,
            _token_endpoint: &str,
            _client_id: &str,
        ) -> Result<Option<String>> {
            unreachable!()
        }

        async fn zoom_access_token(&self, account_id: &str) -> Result<String> {
            let _guard = self.coordinator.lock_account(account_id).await;
            self.entered.notify_one();
            self.release.notified().await;
            let refreshed = OAuthTokens {
                access_token: "custom-refreshed-access".into(),
                refresh_token: Some("custom-refreshed-refresh".into()),
                expires_at: Some(i64::MAX),
            };
            self.tokens.store(account_id, &refreshed)?;
            Ok(refreshed.access_token)
        }

        async fn matrix_access_token(&self, _account_id: &str) -> Result<String> {
            unreachable!()
        }

        async fn talk_app_password(&self, _account_id: &str) -> Result<String> {
            unreachable!()
        }
    }

    #[derive(Default)]
    struct BlockingRotatingEndpoint {
        refresh_tokens: Mutex<Vec<String>>,
        first_entered: tokio::sync::Notify,
        release_first: tokio::sync::Notify,
    }

    impl BlockingRotatingEndpoint {
        fn refreshed_tokens(refresh_token: &str) -> OAuthTokens {
            match refresh_token {
                "stored-refresh" => OAuthTokens {
                    access_token: "first-access".into(),
                    refresh_token: Some("rotated-refresh".into()),
                    expires_at: Some(0),
                },
                "rotated-refresh" => OAuthTokens {
                    access_token: "second-access".into(),
                    refresh_token: Some("rotated-refresh-again".into()),
                    expires_at: Some(i64::MAX),
                },
                "other-refresh" => OAuthTokens {
                    access_token: "other-access".into(),
                    refresh_token: Some("other-rotated-refresh".into()),
                    expires_at: Some(i64::MAX),
                },
                other => panic!("unexpected refresh token: {other}"),
            }
        }
    }

    #[async_trait]
    impl TokenEndpointClient for BlockingRotatingEndpoint {
        async fn exchange_code(
            &self,
            _provider: &OAuthProvider,
            _code: &str,
            _port: u16,
            _code_verifier: Option<&str>,
        ) -> Result<OAuthTokens> {
            unreachable!()
        }

        async fn refresh(
            &self,
            _provider: &OAuthProvider,
            refresh_token: &str,
        ) -> Result<OAuthTokens> {
            let is_first = {
                let mut refresh_tokens = self.refresh_tokens.lock().unwrap();
                refresh_tokens.push(refresh_token.to_string());
                refresh_tokens.len() == 1
            };
            if is_first {
                self.first_entered.notify_one();
                self.release_first.notified().await;
            }
            Ok(Self::refreshed_tokens(refresh_token))
        }

        async fn refresh_scoped(
            &self,
            _provider: &OAuthProvider,
            _refresh_token: &str,
            _scopes: &str,
        ) -> Result<OAuthTokens> {
            unreachable!()
        }

        async fn refresh_dynamic(
            &self,
            _token_url: &str,
            _refresh_token: &str,
            _client_id: &str,
        ) -> Result<OAuthTokens> {
            unreachable!()
        }
    }

    #[async_trait]
    impl TokenEndpointClient for FakeTokenEndpoint {
        async fn exchange_code(
            &self,
            _provider: &OAuthProvider,
            _code: &str,
            _port: u16,
            _code_verifier: Option<&str>,
        ) -> Result<OAuthTokens> {
            Ok(self.refreshed.clone())
        }

        async fn refresh(
            &self,
            _provider: &OAuthProvider,
            _refresh_token: &str,
        ) -> Result<OAuthTokens> {
            Ok(self.refreshed.clone())
        }

        async fn refresh_scoped(
            &self,
            _provider: &OAuthProvider,
            _refresh_token: &str,
            scopes: &str,
        ) -> Result<OAuthTokens> {
            self.scopes.lock().unwrap().push(scopes.to_string());
            Ok(self.refreshed.clone())
        }

        async fn refresh_dynamic(
            &self,
            _token_url: &str,
            _refresh_token: &str,
            _client_id: &str,
        ) -> Result<OAuthTokens> {
            Ok(self.refreshed.clone())
        }
    }

    fn expired_tokens() -> OAuthTokens {
        OAuthTokens {
            access_token: "stored-access".into(),
            refresh_token: Some("stored-refresh".into()),
            expires_at: Some(0),
        }
    }

    fn refreshed_tokens() -> OAuthTokens {
        OAuthTokens {
            access_token: "fresh-access".into(),
            refresh_token: Some("rotated-refresh".into()),
            expires_at: Some(i64::MAX),
        }
    }

    fn mail_account(auth_method: &str, password: &str) -> MailAccountConfig {
        MailAccountConfig {
            id: "mail-focused".into(),
            display_name: "Focused Mail".into(),
            email: "user@example.com".into(),
            protocol: "imap".into(),
            username: "user@example.com".into(),
            password: password.into(),
            auth_method: auth_method.into(),
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            use_tls: true,
            jmap_url: String::new(),
            jmap_auth_method: "basic".into(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
        }
    }

    #[tokio::test]
    async fn focused_password_mail_credentials_preserve_keyring_secret() {
        let service = ProviderCredentialService::new(
            Arc::new(MemoryTokenStore::default()),
            Arc::new(FakeTokenEndpoint {
                refreshed: refreshed_tokens(),
                scopes: Mutex::new(Vec::new()),
            }),
        );

        let credentials = service
            .mail_credentials_for(&mail_account("password", "keyring-secret"))
            .await
            .unwrap();

        assert_eq!(credentials.secret, "keyring-secret");
        assert!(!credentials.use_xoauth2);
    }

    #[tokio::test]
    async fn focused_microsoft_mail_credentials_use_imap_scope_and_rotate() {
        let store = Arc::new(MemoryTokenStore::default());
        store.store("mail-focused", &expired_tokens()).unwrap();
        let endpoint = Arc::new(FakeTokenEndpoint {
            refreshed: refreshed_tokens(),
            scopes: Mutex::new(Vec::new()),
        });
        let service = ProviderCredentialService::new(store.clone(), endpoint.clone());

        let credentials = service
            .mail_credentials_for(&mail_account("oauth-microsoft", "ignored"))
            .await
            .unwrap();

        assert_eq!(credentials.secret, "fresh-access");
        assert!(credentials.use_xoauth2);
        assert_eq!(
            endpoint.scopes.lock().unwrap().as_slice(),
            [crate::oauth::MICROSOFT_IMAP_SCOPES]
        );
        assert_eq!(
            store.load("mail-focused").unwrap().unwrap().refresh_token,
            Some("rotated-refresh".into())
        );
    }

    #[tokio::test]
    async fn graph_purposes_select_distinct_scope_sets() {
        let store = Arc::new(MemoryTokenStore::default());
        store.store("account", &expired_tokens()).unwrap();
        let endpoint = Arc::new(FakeTokenEndpoint {
            refreshed: refreshed_tokens(),
            scopes: Mutex::new(Vec::new()),
        });
        let credentials = ProviderCredentialService::new(store, endpoint.clone());

        credentials
            .graph_access_token("account", GraphTokenPurpose::Baseline)
            .await
            .unwrap();
        credentials
            .graph_access_token("account", GraphTokenPurpose::Rooms)
            .await
            .unwrap();

        assert_eq!(
            *endpoint.scopes.lock().unwrap(),
            vec![
                crate::oauth::MICROSOFT_GRAPH_SCOPES,
                crate::oauth::MICROSOFT_GRAPH_ROOM_SCOPES,
            ]
        );
    }

    #[tokio::test]
    async fn graph_refresh_rotates_only_the_stored_refresh_token() {
        let store = Arc::new(MemoryTokenStore::default());
        store.store("account", &expired_tokens()).unwrap();
        let endpoint = Arc::new(FakeTokenEndpoint {
            refreshed: refreshed_tokens(),
            scopes: Mutex::new(Vec::new()),
        });
        let credentials = ProviderCredentialService::new(store.clone(), endpoint);

        let access = credentials
            .graph_access_token("account", GraphTokenPurpose::Baseline)
            .await
            .unwrap();

        assert_eq!(access, "fresh-access");
        let stored = store.load("account").unwrap().unwrap();
        assert_eq!(stored.access_token, "stored-access");
        assert_eq!(stored.refresh_token.as_deref(), Some("rotated-refresh"));
        assert_eq!(stored.expires_at, Some(0));
    }

    #[tokio::test]
    async fn same_account_refreshes_serialize_and_observe_rotation() {
        let store = Arc::new(MemoryTokenStore::default());
        store.store("account", &expired_tokens()).unwrap();
        let endpoint = Arc::new(BlockingRotatingEndpoint::default());
        let credentials = Arc::new(ProviderCredentialService::new(
            store.clone(),
            endpoint.clone(),
        ));

        let first_credentials = credentials.clone();
        let first =
            tokio::spawn(async move { first_credentials.google_access_token("account").await });
        endpoint.first_entered.notified().await;

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let second_credentials = credentials.clone();
        let second = tokio::spawn(async move {
            started_tx.send(()).unwrap();
            second_credentials.google_access_token("account").await
        });
        started_rx.await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(
            *endpoint.refresh_tokens.lock().unwrap(),
            vec!["stored-refresh"]
        );

        endpoint.release_first.notify_one();
        assert_eq!(first.await.unwrap().unwrap(), "first-access");
        assert_eq!(second.await.unwrap().unwrap(), "second-access");
        assert_eq!(
            *endpoint.refresh_tokens.lock().unwrap(),
            vec!["stored-refresh", "rotated-refresh"]
        );
        assert_eq!(
            store
                .load("account")
                .unwrap()
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("rotated-refresh-again")
        );
    }

    #[tokio::test]
    async fn different_account_refresh_locks_are_independent() {
        let store = Arc::new(MemoryTokenStore::default());
        store.store("blocked", &expired_tokens()).unwrap();
        store
            .store(
                "other",
                &OAuthTokens {
                    access_token: "other-stored-access".into(),
                    refresh_token: Some("other-refresh".into()),
                    expires_at: Some(0),
                },
            )
            .unwrap();
        let endpoint = Arc::new(BlockingRotatingEndpoint::default());
        let credentials = Arc::new(ProviderCredentialService::new(store, endpoint.clone()));

        let blocked_credentials = credentials.clone();
        let blocked =
            tokio::spawn(async move { blocked_credentials.google_access_token("blocked").await });
        endpoint.first_entered.notified().await;

        let other = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            credentials.google_access_token("other"),
        )
        .await;
        endpoint.release_first.notify_one();

        assert_eq!(other.unwrap().unwrap(), "other-access");
        assert_eq!(blocked.await.unwrap().unwrap(), "first-access");
    }

    #[tokio::test]
    async fn zoom_invalid_grant_latches_concurrent_and_repeated_refreshes() {
        let account_id = "zoom-invalid-grant-concurrent";
        crate::oauth::clear_reauth_required(account_id);
        let store = Arc::new(MemoryTokenStore::default());
        store.store(account_id, &expired_tokens()).unwrap();
        let endpoint = Arc::new(ScriptedTokenEndpoint::blocking(vec![Err(Error::Other(
            "Token refresh error: {\"error\":\"invalid_grant\",\
             \"reason\":\"Invalid authorization grant\"}"
                .into(),
        ))]));
        let credentials = Arc::new(ProviderCredentialService::new(store, endpoint.clone()));

        let first_credentials = credentials.clone();
        let first =
            tokio::spawn(async move { first_credentials.zoom_access_token(account_id).await });
        endpoint.first_entered.notified().await;
        let (second_started_tx, second_started_rx) = tokio::sync::oneshot::channel();
        let second_credentials = credentials.clone();
        let second = tokio::spawn(async move {
            second_started_tx.send(()).unwrap();
            second_credentials.zoom_access_token(account_id).await
        });
        second_started_rx.await.unwrap();
        assert_eq!(endpoint.refresh_calls(), 1);
        assert!(!second.is_finished());

        endpoint.release_first.notify_one();

        assert!(matches!(first.await.unwrap(), Err(Error::AuthRequired(_))));
        assert!(matches!(second.await.unwrap(), Err(Error::AuthRequired(_))));
        assert!(matches!(
            credentials.zoom_access_token(account_id).await,
            Err(Error::AuthRequired(_))
        ));
        assert_eq!(endpoint.refresh_calls(), 1);
        crate::oauth::clear_reauth_required(account_id);
    }

    #[tokio::test]
    async fn zoom_unrelated_refresh_error_allows_retry_and_rotation() {
        let account_id = "zoom-retry-after-503";
        crate::oauth::clear_reauth_required(account_id);
        let store = Arc::new(MemoryTokenStore::default());
        store.store(account_id, &expired_tokens()).unwrap();
        let endpoint = Arc::new(ScriptedTokenEndpoint::new(vec![
            Err(Error::Other("Token refresh failed: 503".into())),
            Ok(refreshed_tokens()),
        ]));
        let credentials = ProviderCredentialService::new(store.clone(), endpoint.clone());

        let first = credentials.zoom_access_token(account_id).await;
        assert!(matches!(first, Err(Error::Other(_))));
        crate::oauth::ensure_not_reauth_required(account_id).unwrap();

        assert_eq!(
            credentials.zoom_access_token(account_id).await.unwrap(),
            "fresh-access"
        );
        assert_eq!(endpoint.refresh_calls(), 2);
        let stored = store.load(account_id).unwrap().unwrap();
        assert_eq!(stored.refresh_token.as_deref(), Some("rotated-refresh"));
    }

    #[tokio::test]
    async fn zoom_creation_commit_failure_removes_stored_tokens() {
        let store = Arc::new(MemoryTokenStore::default());
        let lifecycle = OAuthTokenLifecycle {
            tokens: store.clone(),
            account_locks: Arc::new(ProviderAccountCoordinator::default()),
        };
        let _guard = lifecycle.account_locks.lock_account("zoom").await;

        let result = lifecycle.store_zoom_and_commit("zoom", &refreshed_tokens(), || {
            Err(Error::Other("injected commit failure".into()))
        });

        assert!(result.is_err());
        assert!(store.load("zoom").unwrap().is_none());
    }

    #[tokio::test]
    async fn successful_zoom_creation_clears_reauth_with_injected_store() {
        let account_id = "zoom-creation-clears-reauth";
        crate::oauth::mark_reauth_required(account_id);
        let store = Arc::new(MemoryTokenStore::default());
        let lifecycle = OAuthTokenLifecycle {
            tokens: store.clone(),
            account_locks: Arc::new(ProviderAccountCoordinator::default()),
        };
        let _guard = lifecycle.account_locks.lock_account(account_id).await;

        lifecycle
            .store_zoom_and_commit(account_id, &refreshed_tokens(), || Ok(()))
            .unwrap();

        crate::oauth::ensure_not_reauth_required(account_id).unwrap();
        assert_eq!(
            store.load(account_id).unwrap().unwrap().access_token,
            "fresh-access"
        );
    }

    #[tokio::test]
    async fn successful_zoom_reauth_replaces_tokens_and_clears_latch() {
        let account_id = "zoom-reauth-success";
        let store = Arc::new(MemoryTokenStore::default());
        store
            .store(
                account_id,
                &OAuthTokens {
                    access_token: "old-access".into(),
                    refresh_token: Some("old-refresh".into()),
                    expires_at: Some(0),
                },
            )
            .unwrap();
        crate::oauth::mark_reauth_required(account_id);
        let lifecycle = OAuthTokenLifecycle {
            tokens: store.clone(),
            account_locks: Arc::new(ProviderAccountCoordinator::default()),
        };
        let _guard = lifecycle.account_locks.lock_account(account_id).await;

        lifecycle
            .replace_zoom_and_commit(account_id, &refreshed_tokens(), || Ok(()))
            .unwrap();

        assert_eq!(
            store.load(account_id).unwrap().unwrap().access_token,
            "fresh-access"
        );
        crate::oauth::ensure_not_reauth_required(account_id).unwrap();
    }

    #[tokio::test]
    async fn failed_zoom_reauth_restores_tokens_and_latch() {
        let account_id = "zoom-reauth-rollback";
        let store = Arc::new(FailOnceAfterWriteTokenStore {
            inner: MemoryTokenStore::default(),
            fail_next_store: AtomicBool::new(false),
        });
        let previous = OAuthTokens {
            access_token: "old-access".into(),
            refresh_token: Some("old-refresh".into()),
            expires_at: Some(0),
        };
        store.store(account_id, &previous).unwrap();
        store.fail_next_store.store(true, Ordering::SeqCst);
        crate::oauth::mark_reauth_required(account_id);
        let lifecycle = OAuthTokenLifecycle {
            tokens: store.clone(),
            account_locks: Arc::new(ProviderAccountCoordinator::default()),
        };
        let _guard = lifecycle.account_locks.lock_account(account_id).await;

        assert!(lifecycle
            .replace_zoom_and_commit(account_id, &refreshed_tokens(), || Ok(()))
            .is_err());

        let restored = store.load(account_id).unwrap().unwrap();
        assert_eq!(restored.access_token, previous.access_token);
        assert_eq!(restored.refresh_token, previous.refresh_token);
        assert_eq!(restored.expires_at, previous.expires_at);
        assert!(crate::oauth::ensure_not_reauth_required(account_id).is_err());
        crate::oauth::clear_reauth_required(account_id);
    }

    #[tokio::test]
    async fn zoom_reauth_commit_failure_restores_tokens_and_latch() {
        let account_id = "zoom-reauth-commit-rollback";
        let store = Arc::new(MemoryTokenStore::default());
        let previous = OAuthTokens {
            access_token: "old-access".into(),
            refresh_token: Some("old-refresh".into()),
            expires_at: Some(0),
        };
        store.store(account_id, &previous).unwrap();
        crate::oauth::mark_reauth_required(account_id);
        let lifecycle = OAuthTokenLifecycle {
            tokens: store.clone(),
            account_locks: Arc::new(ProviderAccountCoordinator::default()),
        };
        let _guard = lifecycle.account_locks.lock_account(account_id).await;

        assert!(lifecycle
            .replace_zoom_and_commit(account_id, &refreshed_tokens(), || {
                Err(Error::Other("injected identity commit failure".into()))
            })
            .is_err());

        let restored = store.load(account_id).unwrap().unwrap();
        assert_eq!(restored.access_token, previous.access_token);
        assert_eq!(restored.refresh_token, previous.refresh_token);
        assert_eq!(restored.expires_at, previous.expires_at);
        assert!(crate::oauth::ensure_not_reauth_required(account_id).is_err());
        crate::oauth::clear_reauth_required(account_id);
    }

    #[tokio::test]
    async fn zoom_deletion_commit_failure_restores_previous_tokens() {
        let account_id = "zoom-delete-compensation-reauth";
        crate::oauth::clear_reauth_required(account_id);
        let store = Arc::new(LatchAwareMemoryTokenStore::default());
        store.store(account_id, &refreshed_tokens()).unwrap();
        crate::oauth::mark_reauth_required(account_id);
        let lifecycle = OAuthTokenLifecycle {
            tokens: store.clone(),
            account_locks: Arc::new(ProviderAccountCoordinator::default()),
        };
        let _guard = lifecycle.account_locks.lock_account(account_id).await;

        let result = lifecycle.delete_zoom_and_commit(account_id, || {
            Err(Error::Other("injected commit failure".into()))
        });

        assert!(result.is_err());
        let restored = store.load(account_id).unwrap().unwrap();
        assert_eq!(restored.access_token, "fresh-access");
        assert_eq!(restored.refresh_token.as_deref(), Some("rotated-refresh"));
        assert_eq!(restored.expires_at, Some(i64::MAX));
        assert!(crate::oauth::ensure_not_reauth_required(account_id).is_err());
        crate::oauth::clear_reauth_required(account_id);
    }

    #[tokio::test]
    async fn successful_zoom_deletion_clears_reauth_without_stored_tokens() {
        let account_id = "zoom-delete-no-entry-clears-reauth";
        crate::oauth::mark_reauth_required(account_id);
        let store = Arc::new(MemoryTokenStore::default());
        let lifecycle = OAuthTokenLifecycle {
            tokens: store,
            account_locks: Arc::new(ProviderAccountCoordinator::default()),
        };
        let _guard = lifecycle.account_locks.lock_account(account_id).await;

        lifecycle
            .delete_zoom_and_commit(account_id, || Ok(()))
            .unwrap();

        crate::oauth::ensure_not_reauth_required(account_id).unwrap();
    }

    #[tokio::test]
    async fn failed_zoom_token_deletion_keeps_reauth_latched() {
        let account_id = "zoom-delete-failure-keeps-reauth";
        let store = Arc::new(DeleteFailingTokenStore {
            inner: MemoryTokenStore::default(),
        });
        store.store(account_id, &refreshed_tokens()).unwrap();
        crate::oauth::mark_reauth_required(account_id);
        let lifecycle = OAuthTokenLifecycle {
            tokens: store.clone(),
            account_locks: Arc::new(ProviderAccountCoordinator::default()),
        };
        let _guard = lifecycle.account_locks.lock_account(account_id).await;

        assert!(lifecycle
            .delete_zoom_and_commit(account_id, || Ok(()))
            .is_err());
        assert!(crate::oauth::ensure_not_reauth_required(account_id).is_err());
        assert_eq!(
            store
                .load(account_id)
                .unwrap()
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("rotated-refresh")
        );
        crate::oauth::clear_reauth_required(account_id);
    }

    #[tokio::test]
    async fn zoom_deletion_waits_for_refresh_then_removes_refreshed_tokens() {
        let store = Arc::new(MemoryTokenStore::default());
        store.store("zoom", &expired_tokens()).unwrap();
        let endpoint = Arc::new(BlockingRotatingEndpoint::default());
        let token_endpoint: Arc<dyn TokenEndpointClient> = endpoint.clone();
        let credentials = Arc::new(ProviderCredentialService::new(
            store.clone(),
            token_endpoint.clone(),
        ));
        let providers = Arc::new(ProviderServices::new(
            credentials,
            store.clone(),
            token_endpoint,
            ProviderTransports::production().unwrap(),
        ));

        let refresh_providers = providers.clone();
        let refresh = tokio::spawn(async move {
            refresh_providers
                .credentials()
                .zoom_access_token("zoom")
                .await
        });
        endpoint.first_entered.notified().await;

        let delete_providers = providers.clone();
        let delete = tokio::spawn(async move {
            let token_guard = delete_providers.lock_zoom_tokens("zoom").await;
            token_guard.delete_and_commit(|| Ok(()))
        });
        tokio::task::yield_now().await;
        assert!(!delete.is_finished());

        endpoint.release_first.notify_one();
        assert_eq!(refresh.await.unwrap().unwrap(), "first-access");
        delete.await.unwrap().unwrap();
        assert!(store.load("zoom").unwrap().is_none());
    }

    #[tokio::test]
    async fn injected_coordinated_credentials_serialize_with_lifecycle_deletion() {
        let store = Arc::new(MemoryTokenStore::default());
        store.store("zoom", &expired_tokens()).unwrap();
        let credentials = Arc::new(CoordinatedBlockingCredentials {
            coordinator: Arc::new(ProviderAccountCoordinator::default()),
            tokens: store.clone(),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let endpoint: Arc<dyn TokenEndpointClient> = Arc::new(FakeTokenEndpoint {
            refreshed: refreshed_tokens(),
            scopes: Mutex::new(Vec::new()),
        });
        let providers = Arc::new(ProviderServices::new(
            credentials.clone(),
            store.clone(),
            endpoint,
            ProviderTransports::production().unwrap(),
        ));

        let refresh_providers = providers.clone();
        let refresh = tokio::spawn(async move {
            refresh_providers
                .credentials()
                .zoom_access_token("zoom")
                .await
        });
        credentials.entered.notified().await;

        let delete_providers = providers.clone();
        let delete = tokio::spawn(async move {
            let token_guard = delete_providers.lock_zoom_tokens("zoom").await;
            token_guard.delete_and_commit(|| Ok(()))
        });
        tokio::task::yield_now().await;
        assert!(!delete.is_finished());

        credentials.release.notify_one();
        assert_eq!(refresh.await.unwrap().unwrap(), "custom-refreshed-access");
        delete.await.unwrap().unwrap();
        assert!(store.load("zoom").unwrap().is_none());
    }

    #[tokio::test]
    async fn meet_credentials_keep_provider_specific_storage_semantics() {
        let store = Arc::new(MemoryTokenStore::default());
        store
            .store(
                "meet",
                &OAuthTokens {
                    access_token: "long-lived-secret".into(),
                    refresh_token: None,
                    expires_at: None,
                },
            )
            .unwrap();
        let endpoint = Arc::new(FakeTokenEndpoint {
            refreshed: refreshed_tokens(),
            scopes: Mutex::new(Vec::new()),
        });
        let credentials = ProviderCredentialService::new(store, endpoint);

        assert_eq!(
            credentials.matrix_access_token("meet").await.unwrap(),
            "long-lived-secret"
        );
        assert_eq!(
            credentials.talk_app_password("meet").await.unwrap(),
            "long-lived-secret"
        );
    }
}
