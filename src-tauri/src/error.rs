use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("IMAP error: {0}")]
    Imap(String),

    #[error("Mail parse error: {0}")]
    MailParse(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Account not found: {0}")]
    AccountNotFound(String),

    #[error("Message not found: {0}")]
    MessageNotFound(String),

    #[error("Sync error: {0}")]
    Sync(String),

    #[error("Keyring error: {0}")]
    Keyring(String),

    /// The account's OAuth refresh token was rejected (`invalid_grant`,
    /// e.g. AADSTS70043 conditional-access lifetime limits). The user must
    /// sign in again; automated retries are pointless until then.
    #[error("Re-authentication required: {0}")]
    AuthRequired(String),

    #[error("OAuth2 callback missing required state parameter")]
    OAuthStateMissing,

    #[error("OAuth2 state mismatch (possible CSRF)")]
    OAuthStateMismatch,

    #[error("{protocol} does not support {capability}")]
    UnsupportedCapability {
        protocol: &'static str,
        capability: &'static str,
    },

    /// A delivery attempt ended without a definitive rejection or success.
    /// Keep this variant payload-free so user-facing formatting cannot leak
    /// envelope recipients or transport internals.
    #[error("Delivery outcome is unknown")]
    IndeterminateDelivery,

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn is_indeterminate_delivery(&self) -> bool {
        matches!(self, Self::IndeterminateDelivery)
    }
}

impl Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn indeterminate_delivery_has_safe_typed_classification() {
        let error = Error::IndeterminateDelivery;

        assert!(error.is_indeterminate_delivery());
        assert_eq!(error.to_string(), "Delivery outcome is unknown");
        assert_eq!(
            serde_json::to_string(&error).unwrap(),
            r#""Delivery outcome is unknown""#
        );
        assert!(!Error::Other("ordinary failure".into()).is_indeterminate_delivery());
    }
}
