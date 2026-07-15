//! JMAP identity domain: `Identity/*` methods.

use crate::error::{Error, Result};

use super::{JmapConfig, JmapConnection};

impl JmapConnection {
    /// Find the identity ID for email submission.
    pub(super) async fn find_identity_id(&self, config: &JmapConfig) -> Result<String> {
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:submission"],
            "methodCalls": [
                ["Identity/get", {
                    "accountId": self.account_id
                }, "id1"]
            ]
        });
        let resp = self.api_request(&request, config).await?;
        if let Some(identities) = resp["methodResponses"][0][1]["list"].as_array() {
            if let Some(first) = identities.first() {
                if let Some(id) = first["id"].as_str() {
                    return Ok(id.to_string());
                }
            }
        }
        Err(Error::Other("No JMAP identity found for submission".into()))
    }
}
