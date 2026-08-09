//! Injectable credentials and OAuth token-endpoint dependencies for providers.

use std::sync::Arc;

use async_trait::async_trait;

use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::mail::jmap::JmapConfig;
use crate::oauth::{OAuthProvider, OAuthTokens};

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
    async fn google_access_token(&self, account_id: &str) -> Result<String>;
    async fn graph_access_token(
        &self,
        account_id: &str,
        purpose: GraphTokenPurpose,
    ) -> Result<String>;
    async fn mail_credentials(&self, account: &AccountFull) -> Result<MailCredentials>;
    async fn jmap_config(&self, account: &AccountFull) -> Result<JmapConfig>;
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
}

impl ProviderCredentialService {
    pub fn new(tokens: Arc<dyn OAuthTokenStore>, endpoint: Arc<dyn TokenEndpointClient>) -> Self {
        Self { tokens, endpoint }
    }

    pub fn production() -> Result<Self> {
        Ok(Self::new(
            Arc::new(SystemOAuthTokenStore),
            Arc::new(ReqwestTokenEndpointClient::production()?),
        ))
    }

    fn required_tokens(&self, account_id: &str, missing_message: &str) -> Result<OAuthTokens> {
        self.tokens
            .load(account_id)?
            .ok_or_else(|| Error::Other(missing_message.into()))
    }

    async fn jmap_oidc_access_token(&self, account: &AccountFull) -> Result<Option<String>> {
        if account.jmap_auth_method != "oidc" {
            return Ok(None);
        }

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
    async fn google_access_token(&self, account_id: &str) -> Result<String> {
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

    async fn mail_credentials(&self, account: &AccountFull) -> Result<MailCredentials> {
        if account.auth_method != "oauth-microsoft" {
            return Ok(MailCredentials {
                secret: account.password.clone(),
                use_xoauth2: false,
            });
        }

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

    async fn jmap_config(&self, account: &AccountFull) -> Result<JmapConfig> {
        let mut config = JmapConfig::from_account(account);
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
            .await?;
        self.tokens.store(account_id, &refreshed)?;
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
    use std::collections::HashMap;
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

    struct FakeTokenEndpoint {
        refreshed: OAuthTokens,
        scopes: Mutex<Vec<String>>,
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
