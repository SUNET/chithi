//! JMAP identity domain: `Identity/*` methods.

use crate::error::{Error, Result};

use super::mail::{parse_rfc5321_addr_spec, ParsedMailbox};
use super::{JmapConfig, JmapConnection, JmapSubmissionEnvelope};

impl JmapConnection {
    /// Find the first exact sender identity, falling back to the first
    /// RFC 8621 `*@same-domain` identity only when no exact match exists.
    pub(super) async fn find_identity_id(
        &self,
        config: &JmapConfig,
        envelope: &JmapSubmissionEnvelope,
    ) -> Result<String> {
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:submission"],
            "methodCalls": [
                ["Identity/get", {
                    "accountId": self.account_id,
                    "properties": ["id", "email"]
                }, "id1"]
            ]
        });
        let resp = self.api_request(&request, config).await?;
        let (method, body) = identity_method_response(&resp)?;
        if method == "error" {
            return Err(Error::Other(format!(
                "JMAP Identity/get failed (type={})",
                super::safe_jmap_error_type(body)
            )));
        }
        if method != "Identity/get" {
            return Err(Error::Other(
                "JMAP Identity/get returned an unexpected method".into(),
            ));
        }
        let identities = body
            .get("list")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| Error::Other("Malformed JMAP Identity/get response".into()))?;
        select_identity_id(identities, envelope.mail_from_mailbox())
    }
}

fn identity_method_response(response: &serde_json::Value) -> Result<(&str, &serde_json::Value)> {
    let responses = response
        .get("methodResponses")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::Other("Malformed JMAP Identity/get response".into()))?;
    if responses.len() != 1 {
        return Err(Error::Other(
            "Malformed JMAP Identity/get response count".into(),
        ));
    }
    let tuple = responses[0]
        .as_array()
        .filter(|tuple| tuple.len() == 3)
        .ok_or_else(|| Error::Other("Malformed JMAP Identity/get response tuple".into()))?;
    if tuple[2].as_str() != Some("id1") {
        return Err(Error::Other(
            "Mismatched JMAP Identity/get response call id".into(),
        ));
    }
    let method = tuple[0]
        .as_str()
        .ok_or_else(|| Error::Other("Malformed JMAP Identity/get method".into()))?;
    Ok((method, &tuple[1]))
}

fn select_identity_id(
    identities: &[serde_json::Value],
    mail_from: &ParsedMailbox,
) -> Result<String> {
    let mut wildcard_id = None;
    for identity in identities {
        let Some(id) = identity
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let Some(email) = identity
            .get("email")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_rfc5321_addr_spec)
        else {
            continue;
        };
        if email.matches(mail_from) {
            return Ok(id.to_string());
        }
        if wildcard_id.is_none() && email.is_wildcard_for(mail_from) {
            wildcard_id = Some(id.to_string());
        }
    }
    wildcard_id.ok_or_else(|| {
        Error::Other("No JMAP identity permits the submission mail-from address".into())
    })
}

#[cfg(test)]
mod tests {
    use super::{identity_method_response, select_identity_id};
    use crate::mail::jmap::JmapSubmissionEnvelope;

    fn sender(mail_from: &str) -> JmapSubmissionEnvelope {
        JmapSubmissionEnvelope::new(mail_from, &["recipient@example.test".into()], &[], &[])
            .unwrap()
    }

    #[test]
    fn exact_identity_outranks_wildcard_and_preserves_first_exact() {
        let envelope = sender("Alice@Example.test");
        let identities = serde_json::json!([
            { "id": "wildcard", "email": "*@example.test" },
            { "id": "exact-first", "email": "\"Ali\\ce\"@EXAMPLE.test" },
            { "id": "exact-second", "email": "Alice@example.test" }
        ]);

        assert_eq!(
            select_identity_id(identities.as_array().unwrap(), envelope.mail_from_mailbox())
                .unwrap(),
            "exact-first"
        );
    }

    #[test]
    fn same_domain_wildcard_is_accepted_without_exact_match() {
        let envelope = sender("alias@Example.test");
        let identities = serde_json::json!([
            { "id": "wrong", "email": "*@other.test" },
            { "id": "wildcard", "email": "*@EXAMPLE.test" }
        ]);

        assert_eq!(
            select_identity_id(identities.as_array().unwrap(), envelope.mail_from_mailbox())
                .unwrap(),
            "wildcard"
        );
    }

    #[test]
    fn wrong_domain_wildcard_is_rejected() {
        let envelope = sender("alias@example.test");
        let identities = serde_json::json!([
            { "id": "wrong", "email": "*@other.test" },
            { "id": "also-wrong", "email": "alias@other.test" }
        ]);

        assert!(
            select_identity_id(identities.as_array().unwrap(), envelope.mail_from_mailbox())
                .is_err()
        );
    }

    #[test]
    fn local_part_case_must_match_exactly() {
        let envelope = sender("Alice@example.test");
        let identities = serde_json::json!([
            { "id": "wrong-case", "email": "alice@example.test" }
        ]);

        assert!(
            select_identity_id(identities.as_array().unwrap(), envelope.mail_from_mailbox())
                .is_err()
        );
    }

    #[test]
    fn identity_response_requires_one_correctly_correlated_tuple() {
        for response in [
            serde_json::json!({ "methodResponses": [] }),
            serde_json::json!({
                "methodResponses": [["Identity/get", { "list": [] }, "wrong"]]
            }),
            serde_json::json!({
                "methodResponses": [
                    ["Identity/get", { "list": [] }, "id1"],
                    ["Identity/get", { "list": [] }, "extra"]
                ]
            }),
        ] {
            assert!(identity_method_response(&response).is_err());
        }
    }
}
