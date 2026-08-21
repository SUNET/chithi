//! JMAP mailbox domain: `Mailbox/*` methods.

use crate::error::{Error, Result};

use super::{JmapConfig, JmapConnection};

impl JmapConnection {
    pub async fn list_folders(
        &self,
        config: &JmapConfig,
    ) -> Result<Vec<(String, String, Option<&'static str>, Option<String>)>> {
        log::debug!("JMAP listing mailboxes");
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Mailbox/get", {
                    "accountId": self.account_id,
                    "properties": ["id", "name", "role", "totalEmails", "unreadEmails", "parentId"]
                }, "m1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        let mailboxes = resp["methodResponses"][0][1]["list"]
            .as_array()
            .ok_or_else(|| Error::Other("Invalid Mailbox/get response".into()))?;

        let mut folders = Vec::new();
        for mb in mailboxes {
            let id = mb["id"].as_str().unwrap_or("").to_string();
            let name = mb["name"].as_str().unwrap_or("Unknown").to_string();
            let role = mb["role"].as_str();
            let folder_type = match role {
                Some("inbox") => Some("inbox"),
                Some("drafts") => Some("drafts"),
                Some("sent") => Some("sent"),
                Some("trash") => Some("trash"),
                Some("junk") => Some("junk"),
                Some("archive") => Some("archive"),
                _ => None,
            };
            let parent_id = mb["parentId"].as_str().map(|s| s.to_string());
            log::debug!(
                "  mailbox: {} ({}) role={:?} parentId={:?}",
                name,
                id,
                role,
                parent_id
            );
            folders.push((name, id, folder_type, parent_id));
        }
        log::info!("JMAP found {} mailboxes", folders.len());
        Ok(folders)
    }

    /// Find a mailbox by its JMAP role (inbox, sent, drafts, trash, junk).
    pub(super) async fn find_mailbox_by_role(
        &self,
        config: &JmapConfig,
        role: &str,
    ) -> Result<Option<String>> {
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Mailbox/get", {
                    "accountId": self.account_id,
                    "properties": ["id", "role"]
                }, "r1"]
            ]
        });
        let resp = self.api_request(&request, config).await?;
        mailbox_id_by_role(&resp, role)
    }

    /// Create a new mailbox on the JMAP server.
    pub async fn create_mailbox(
        &self,
        config: &JmapConfig,
        name: &str,
        parent_id: Option<&str>,
    ) -> Result<String> {
        log::info!("JMAP creating mailbox: {} (parent={:?})", name, parent_id);
        let create_id = "new-folder";
        let mut mailbox = serde_json::json!({ "name": name });
        if let Some(pid) = parent_id {
            mailbox["parentId"] = serde_json::json!(pid);
        }
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Mailbox/set", {
                    "accountId": self.account_id,
                    "create": {
                        create_id: mailbox
                    }
                }, "c1"]
            ]
        });
        let resp = self.api_request(&request, config).await?;
        let created_id = resp["methodResponses"][0][1]["created"][create_id]["id"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if created_id.is_empty() {
            let err = resp["methodResponses"][0][1]["notCreated"][create_id]["description"]
                .as_str()
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP Mailbox/set create failed: {}",
                err
            )));
        }
        log::info!("JMAP mailbox created: id={}", created_id);
        Ok(created_id)
    }

    pub async fn destroy_mailbox(
        &self,
        config: &JmapConfig,
        mailbox_id: &str,
        remove_messages: bool,
    ) -> Result<()> {
        log::info!(
            "JMAP destroying mailbox: {} (remove_messages={})",
            mailbox_id,
            remove_messages
        );
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [
                ["Mailbox/set", {
                    "accountId": self.account_id,
                    "onDestroyRemoveEmails": remove_messages,
                    "destroy": [mailbox_id]
                }, "d1"]
            ]
        });
        let resp = self.api_request(&request, config).await?;
        let method_name = resp["methodResponses"][0][0]
            .as_str()
            .unwrap_or("<unknown>");
        if method_name != "Mailbox/set" {
            log::error!("Unexpected JMAP response to mailbox destroy: {}", resp);
            return Err(Error::Other(format!(
                "Unexpected JMAP response to mailbox destroy: {}",
                method_name,
            )));
        }

        let destroyed = resp["methodResponses"][0][1]["destroyed"]
            .as_array()
            .map(|a| a.iter().any(|v| v.as_str() == Some(mailbox_id)))
            .unwrap_or(false);
        if !destroyed {
            let err = resp["methodResponses"][0][1]["notDestroyed"][mailbox_id]["description"]
                .as_str()
                .unwrap_or("Unknown error");
            log::error!("JMAP mailbox destroy failed for {}: {}", mailbox_id, err);
            return Err(Error::Other(format!(
                "JMAP Mailbox/set destroy failed: {}",
                err
            )));
        }
        log::info!("JMAP mailbox destroyed: {}", mailbox_id);
        Ok(())
    }
}

fn mailbox_id_by_role(response: &serde_json::Value, role: &str) -> Result<Option<String>> {
    let responses = response
        .get("methodResponses")
        .and_then(serde_json::Value::as_array)
        .filter(|responses| responses.len() == 1)
        .ok_or_else(|| Error::Other("Malformed JMAP Mailbox/get response count".into()))?;
    let tuple = responses[0]
        .as_array()
        .filter(|tuple| tuple.len() == 3)
        .ok_or_else(|| Error::Other("Malformed JMAP Mailbox/get response tuple".into()))?;
    if tuple[2].as_str() != Some("r1") {
        return Err(Error::Other(
            "JMAP Mailbox/get returned an unexpected call id".into(),
        ));
    }

    let method = tuple[0]
        .as_str()
        .ok_or_else(|| Error::Other("Malformed JMAP Mailbox/get method".into()))?;
    if method == "error" {
        return Err(Error::Other(format!(
            "JMAP Mailbox/get failed (type={})",
            super::safe_jmap_error_type(&tuple[1])
        )));
    }
    if method != "Mailbox/get" {
        return Err(Error::Other(
            "JMAP Mailbox/get returned an unexpected method".into(),
        ));
    }

    let mailboxes = tuple[1]
        .get("list")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::Other("JMAP Mailbox/get returned no mailbox list".into()))?;
    for mailbox in mailboxes {
        let mailbox = mailbox
            .as_object()
            .ok_or_else(|| Error::Other("JMAP Mailbox/get returned a malformed mailbox".into()))?;
        let id = mailbox
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                Error::Other("JMAP Mailbox/get returned a mailbox without an id".into())
            })?;
        let mailbox_role = mailbox
            .get("role")
            .ok_or_else(|| Error::Other("JMAP Mailbox/get omitted a requested role".into()))?;
        if !mailbox_role.is_null() && !mailbox_role.is_string() {
            return Err(Error::Other(
                "JMAP Mailbox/get returned a malformed role".into(),
            ));
        }
        if mailbox_role.as_str() == Some(role) {
            return Ok(Some(id.to_string()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::mailbox_id_by_role;

    #[test]
    fn role_lookup_requires_a_correlated_mailbox_get_result() {
        let valid = serde_json::json!({
            "methodResponses": [["Mailbox/get", {
                "list": [
                    { "id": "archive-id", "role": null },
                    { "id": "sent-id", "role": "sent" }
                ]
            }, "r1"]]
        });
        assert_eq!(
            mailbox_id_by_role(&valid, "sent").unwrap().as_deref(),
            Some("sent-id")
        );

        for invalid in [
            serde_json::json!({
                "methodResponses": [["error", { "type": "serverFail" }, "r1"]]
            }),
            serde_json::json!({
                "methodResponses": [["Mailbox/get", { "list": [] }, "wrong"]]
            }),
            serde_json::json!({
                "methodResponses": [["Mailbox/get", {}, "r1"]]
            }),
            serde_json::json!({
                "methodResponses": [["Mailbox/get", {
                    "list": [{ "id": "sent-id" }]
                }, "r1"]]
            }),
        ] {
            assert!(mailbox_id_by_role(&invalid, "sent").is_err());
        }
    }

    #[test]
    fn role_lookup_redacts_method_error_descriptions() {
        let secret = "private mailbox detail";
        let response = serde_json::json!({
            "methodResponses": [["error", {
                "type": "serverFail",
                "description": secret
            }, "r1"]]
        });

        let error = mailbox_id_by_role(&response, "sent")
            .unwrap_err()
            .to_string();
        assert!(error.contains("serverFail"));
        assert!(!error.contains(secret));
    }
}
