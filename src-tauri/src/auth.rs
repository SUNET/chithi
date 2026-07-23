//! Credential resolution: turn a stored account into a ready-to-use
//! credential (bearer token, XOAUTH2 password, JMAP config).
//!
//! One function per provider/service. OAuth *flows* (sign-in, consent,
//! device authorization) stay in `commands/oauth_cmd.rs` and the
//! per-provider modules; this module only loads/refreshes what a flow
//! already stored. Keeping it out of `commands/` lets `mail/` and
//! `ops/` resolve credentials without depending on the command layer.
//!
//! - JMAP: [`build_jmap_config`] (Basic / bearer / OIDC overlay),
//!   [`refresh_jmap_oidc_token`] for the push loop's reconnects
//! - Google: [`get_google_token`] (Calendar v3 + People v1 scope set)
//! - O365 over IMAP/SMTP: [`get_imap_credentials`] (IMAP-scoped
//!   XOAUTH2 refresh; Graph tokens live in `mail::graph`)

use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};

/// Get a valid OIDC access token for a JMAP account, refreshing if needed.
/// Returns `None` if the account doesn't use OIDC.
pub async fn get_jmap_oidc_token(account: &AccountFull) -> Result<Option<String>> {
    if account.jmap_auth_method != "oidc" {
        return Ok(None);
    }

    log::info!(
        "OIDC[get_jmap_oidc_token]: account={} token_endpoint={} client_id={}",
        account.id,
        account.oidc_token_endpoint,
        account.oidc_client_id,
    );

    let tokens = crate::oauth::load_tokens(&account.id)?.ok_or_else(|| {
        log::error!(
            "OIDC[get_jmap_oidc_token]: no tokens in keyring for account {}",
            account.id
        );
        Error::Other("No OIDC tokens found. Please sign in again.".into())
    })?;

    let now = chrono::Utc::now().timestamp();
    let expired = tokens.is_expired();
    // Don't log access / refresh tokens (even truncated); the expiry +
    // presence flag is enough to diagnose refresh behavior.
    log::info!(
        "OIDC[get_jmap_oidc_token]: account={} loaded has_refresh={} expires_at={:?} now={} expired={}",
        account.id,
        tokens.refresh_token.is_some(),
        tokens.expires_at,
        now,
        expired,
    );

    if !expired {
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

    let new_tokens = crate::oauth::refresh_token_dynamic(
        &account.oidc_token_endpoint,
        &refresh_token,
        &account.oidc_client_id,
    )
    .await?;
    crate::oauth::store_tokens(&account.id, &new_tokens)?;
    log::info!(
        "OIDC[get_jmap_oidc_token]: account={} new tokens stored",
        account.id
    );

    Ok(Some(new_tokens.access_token))
}

/// Refresh an OIDC access token using the account_id and OIDC metadata.
/// Used by the push loop to refresh tokens on reconnect without DB access.
pub async fn refresh_jmap_oidc_token(
    account_id: &str,
    oidc_token_endpoint: &str,
    oidc_client_id: &str,
) -> Result<Option<String>> {
    log::info!(
        "OIDC[refresh_jmap_oidc_token]: account={} token_endpoint={} client_id={}",
        account_id,
        oidc_token_endpoint,
        oidc_client_id
    );
    let tokens = match crate::oauth::load_tokens(account_id)? {
        Some(tokens) => tokens,
        None => {
            log::warn!(
                "OIDC[refresh_jmap_oidc_token]: no tokens for account {}",
                account_id
            );
            return Ok(None);
        }
    };

    let now = chrono::Utc::now().timestamp();
    let expired = tokens.is_expired();
    // Don't log access / refresh tokens (even truncated).
    log::info!(
        "OIDC[refresh_jmap_oidc_token]: account={} has_refresh={} expires_at={:?} now={} expired={}",
        account_id,
        tokens.refresh_token.is_some(),
        tokens.expires_at,
        now,
        expired,
    );

    if !expired {
        return Ok(Some(tokens.access_token));
    }

    let refresh_token = match tokens.refresh_token {
        Some(refresh_token) => refresh_token,
        None => return Ok(Some(tokens.access_token)),
    };

    if oidc_token_endpoint.is_empty() || oidc_client_id.is_empty() {
        return Ok(Some(tokens.access_token));
    }

    let new_tokens =
        crate::oauth::refresh_token_dynamic(oidc_token_endpoint, &refresh_token, oidc_client_id)
            .await?;
    crate::oauth::store_tokens(account_id, &new_tokens)?;
    log::info!(
        "OIDC[refresh_jmap_oidc_token]: account={} new tokens stored",
        account_id
    );

    Ok(Some(new_tokens.access_token))
}

/// Build a JmapConfig from an account, including OIDC token if applicable.
///
/// Delegates to `JmapConfig::from_account` so the bearer-mode promotion
/// (password → access_token for Fastmail-style API tokens) is applied
/// uniformly. For OIDC accounts we overlay the refreshed access token
/// from the keyring afterwards; from_account leaves `access_token: None`
/// for OIDC because it has no async refresh path.
pub async fn build_jmap_config(account: &AccountFull) -> Result<crate::mail::jmap::JmapConfig> {
    let mut config = crate::mail::jmap::JmapConfig::from_account(account);
    if let Some(token) = get_jmap_oidc_token(account).await? {
        config.access_token = Some(token);
    }
    Ok(config)
}

/// Get a valid Google OAuth2 access token, refreshing if expired.
pub async fn get_google_token(account_id: &str) -> Result<String> {
    let tokens = crate::oauth::load_tokens(account_id)?.ok_or_else(|| {
        Error::Other("No Google OAuth tokens. Please sign in with Google in Settings.".into())
    })?;

    if !tokens.is_expired() {
        return Ok(tokens.access_token);
    }

    let refresh_token = tokens
        .refresh_token
        .ok_or_else(|| Error::Other("No refresh token".into()))?;
    match crate::oauth::refresh_access_token(&crate::oauth::GOOGLE, &refresh_token).await {
        Ok(new_tokens) => {
            crate::oauth::store_tokens(account_id, &new_tokens)?;
            Ok(new_tokens.access_token)
        }
        Err(e) => Err(e),
    }
}

/// Resolve the IMAP/SMTP password for an account. Returns
/// `(password, use_xoauth2)`.
///
/// For O365 (`auth_method == "oauth-microsoft"`) this refreshes an
/// IMAP-scoped OAuth token — Microsoft issues resource-scoped access
/// tokens, so the Graph token cannot be reused for IMAP/SMTP — and
/// persists the new token set so a rotated refresh token is not lost.
/// Every other auth method uses the stored account password.
pub async fn get_imap_credentials(account: &AccountFull) -> Result<(String, bool)> {
    if account.auth_method != "oauth-microsoft" {
        return Ok((account.password.clone(), false));
    }

    // Dead refresh token (invalid_grant)? Fail fast without a network call.
    crate::oauth::ensure_not_reauth_required(&account.id)?;

    let tokens = crate::oauth::load_tokens(&account.id)?.ok_or_else(|| {
        Error::Other("No O365 OAuth tokens. Please sign in with Microsoft.".into())
    })?;
    let refresh_token = tokens.refresh_token.ok_or_else(|| {
        Error::Other("No O365 refresh token. Please sign in with Microsoft.".into())
    })?;
    let imap_tokens = crate::oauth::refresh_with_scopes(
        &crate::oauth::MICROSOFT,
        &refresh_token,
        crate::oauth::MICROSOFT_IMAP_SCOPES,
    )
    .await
    .map_err(|e| crate::oauth::auth_required_on_invalid_grant(&account.id, e))?;
    // Save the potentially rotated refresh token
    crate::oauth::store_tokens(&account.id, &imap_tokens)?;
    Ok((imap_tokens.access_token, true))
}
