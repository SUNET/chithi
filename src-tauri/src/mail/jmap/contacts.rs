//! JMAP contacts domain: `AddressBook/*` and `ContactCard/*` methods
//! (RFC 9553 JSContact).

use crate::error::{Error, Result};

use super::{JmapConfig, JmapConnection};

impl JmapConnection {
    /// List address books from the JMAP server.
    pub async fn list_address_books(&self, config: &JmapConfig) -> Result<Vec<JmapAddressBook>> {
        log::info!("JMAP listing address books");
        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"],
            "methodCalls": [
                ["AddressBook/get", {
                    "accountId": self.account_id,
                }, "ab1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;
        let mut books = Vec::new();

        if let Some(list) = resp["methodResponses"][0][1]["list"].as_array() {
            for ab in list {
                let id = ab["id"].as_str().unwrap_or_default().to_string();
                let name = ab["name"].as_str().unwrap_or("Contacts").to_string();
                books.push(JmapAddressBook { id, name });
            }
        }

        log::info!("JMAP found {} address books", books.len());
        Ok(books)
    }

    /// Fetch contacts from a JMAP address book.
    pub async fn fetch_contacts(
        &self,
        config: &JmapConfig,
        address_book_id: Option<&str>,
    ) -> Result<Vec<JmapContact>> {
        log::debug!("JMAP fetching contacts (addressBook={:?})", address_book_id);

        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"],
            "methodCalls": [
                ["ContactCard/get", {
                    "accountId": self.account_id,
                }, "c1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;
        log::debug!(
            "JMAP ContactCard response: {}",
            serde_json::to_string(&resp).unwrap_or_default()
        );

        let mut contacts = Vec::new();

        if let Some(list) = resp["methodResponses"][0][1]["list"].as_array() {
            for card in list {
                let id = card["id"].as_str().unwrap_or_default().to_string();
                let uid = card["uid"].as_str().map(|s| s.to_string());

                // Parse name — handles both JSContact formats:
                // 1. {full: "...", given: "...", surname: "..."} (simple)
                // 2. {components: [{kind:"given",value:"..."}, {kind:"surname",value:"..."}]} (Stalwart)
                let display_name = if let Some(name_obj) = card["name"].as_object() {
                    // Try "full" first
                    if let Some(full) = name_obj.get("full").and_then(|f| f.as_str()) {
                        full.to_string()
                    }
                    // Try "components" array (Stalwart JSContact format)
                    else if let Some(components) =
                        name_obj.get("components").and_then(|c| c.as_array())
                    {
                        let mut given = String::new();
                        let mut middle = String::new();
                        let mut surname = String::new();
                        for comp in components {
                            let kind = comp["kind"].as_str().unwrap_or("");
                            let value = comp["value"].as_str().unwrap_or("");
                            match kind {
                                "given" => given = value.to_string(),
                                "given2" | "middle" => middle = value.to_string(),
                                "surname" => surname = value.to_string(),
                                _ => {}
                            }
                        }
                        let parts: Vec<&str> = [given.as_str(), middle.as_str(), surname.as_str()]
                            .into_iter()
                            .filter(|s| !s.is_empty())
                            .collect();
                        if parts.is_empty() {
                            "(No name)".to_string()
                        } else {
                            parts.join(" ")
                        }
                    }
                    // Try direct given/surname
                    else {
                        let given = name_obj.get("given").and_then(|g| g.as_str()).unwrap_or("");
                        let surname = name_obj
                            .get("surname")
                            .and_then(|s| s.as_str())
                            .unwrap_or("");
                        let name = format!("{} {}", given, surname).trim().to_string();
                        if name.is_empty() {
                            "(No name)".to_string()
                        } else {
                            name
                        }
                    }
                } else {
                    "(No name)".to_string()
                };

                // Parse emails
                let mut emails = Vec::new();
                if let Some(emails_obj) = card["emails"].as_object() {
                    for (_key, em) in emails_obj {
                        let addr = em["address"].as_str().unwrap_or_default().to_string();
                        // Try label, then contexts keys, then default to "work"
                        let label = em["label"]
                            .as_str()
                            .map(|s| s.to_string())
                            .or_else(|| {
                                em["contexts"]
                                    .as_object()
                                    .and_then(|c| c.keys().next().cloned())
                            })
                            .unwrap_or_else(|| "work".to_string());
                        if !addr.is_empty() {
                            emails.push(serde_json::json!({"email": addr, "label": label}));
                        }
                    }
                }

                // Parse phones
                let mut phones = Vec::new();
                if let Some(phones_obj) = card["phones"].as_object() {
                    for (_key, ph) in phones_obj {
                        let number = ph["number"].as_str().unwrap_or_default().to_string();
                        let label = ph["label"].as_str().unwrap_or("mobile").to_string();
                        if !number.is_empty() {
                            phones.push(serde_json::json!({"number": number, "label": label}));
                        }
                    }
                }

                // Parse organization
                let organization = card["organizations"]
                    .as_object()
                    .and_then(|orgs| orgs.values().next())
                    .and_then(|org| org["name"].as_str())
                    .map(|s| s.to_string());

                let title = card["titles"]
                    .as_object()
                    .and_then(|titles| titles.values().next())
                    .and_then(|t| t["name"].as_str())
                    .map(|s| s.to_string());

                let notes = card["notes"]
                    .as_object()
                    .and_then(|n| n.values().next())
                    .and_then(|note| note["note"].as_str())
                    .map(|s| s.to_string());

                // Determine which address book this belongs to
                let ab_id = card["addressBookIds"]
                    .as_object()
                    .and_then(|abs| abs.keys().next())
                    .map(|s| s.to_string());

                // Filter by address book if specified
                if let Some(ref target_ab) = address_book_id {
                    if let Some(ref contact_ab) = ab_id {
                        if contact_ab != target_ab {
                            continue;
                        }
                    }
                }

                contacts.push(JmapContact {
                    id,
                    uid,
                    display_name,
                    emails_json: serde_json::to_string(&emails)
                        .unwrap_or_else(|_| "[]".to_string()),
                    phones_json: serde_json::to_string(&phones)
                        .unwrap_or_else(|_| "[]".to_string()),
                    organization,
                    title,
                    notes,
                });
            }
        }

        log::info!("JMAP fetched {} contacts", contacts.len());
        Ok(contacts)
    }

    /// Create a contact on the JMAP server. Returns the server-assigned ID.
    pub async fn create_contact_card(
        &self,
        config: &JmapConfig,
        address_book_id: &str,
        display_name: &str,
        emails_json: &str,
        phones_json: &str,
        organization: Option<&str>,
        title: Option<&str>,
        notes: Option<&str>,
    ) -> Result<String> {
        log::info!("JMAP creating contact: '{}'", display_name);

        // Build name components from display_name
        let name_parts: Vec<&str> = display_name.split_whitespace().collect();
        let mut components = Vec::new();
        if let Some(first) = name_parts.first() {
            components.push(serde_json::json!({"kind": "given", "value": first}));
        }
        if name_parts.len() > 2 {
            let middle = name_parts[1..name_parts.len() - 1].join(" ");
            components.push(serde_json::json!({"kind": "given2", "value": middle}));
        }
        if name_parts.len() >= 2 {
            components
                .push(serde_json::json!({"kind": "surname", "value": name_parts.last().unwrap()}));
        }

        let mut card = serde_json::json!({
            "@type": "Card",
            "version": "1.0",
            "name": {
                "components": components,
                "isOrdered": true,
            },
            "addressBookIds": { address_book_id: true },
        });

        // Add emails
        if let Ok(emails) = serde_json::from_str::<Vec<serde_json::Value>>(emails_json) {
            let mut emails_map = serde_json::Map::new();
            for (i, em) in emails.iter().enumerate() {
                let addr = em["email"].as_str().unwrap_or_default();
                if !addr.is_empty() {
                    emails_map.insert(format!("e{}", i), serde_json::json!({"address": addr}));
                }
            }
            if !emails_map.is_empty() {
                card["emails"] = serde_json::Value::Object(emails_map);
            }
        }

        // Add phones
        if let Ok(phones) = serde_json::from_str::<Vec<serde_json::Value>>(phones_json) {
            let mut phones_map = serde_json::Map::new();
            for (i, ph) in phones.iter().enumerate() {
                let number = ph["number"].as_str().unwrap_or_default();
                if !number.is_empty() {
                    phones_map.insert(format!("p{}", i), serde_json::json!({"number": number}));
                }
            }
            if !phones_map.is_empty() {
                card["phones"] = serde_json::Value::Object(phones_map);
            }
        }

        // Add organization
        if let Some(org) = organization {
            if !org.is_empty() {
                card["organizations"] = serde_json::json!({"o0": {"name": org}});
            }
        }

        // Add title
        if let Some(t) = title {
            if !t.is_empty() {
                card["titles"] = serde_json::json!({"t0": {"name": t}});
            }
        }

        // Add notes
        if let Some(n) = notes {
            if !n.is_empty() {
                card["notes"] = serde_json::json!({"n0": {"note": n}});
            }
        }

        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"],
            "methodCalls": [
                ["ContactCard/set", {
                    "accountId": self.account_id,
                    "create": {
                        "new1": card,
                    }
                }, "s1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notCreated"]["new1"].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP create contact failed: {}",
                desc
            )));
        }

        let remote_id = resp["methodResponses"][0][1]["created"]["new1"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        log::info!("JMAP created contact '{}' id={}", display_name, remote_id);
        Ok(remote_id)
    }

    /// Update a contact on the JMAP server via ContactCard/set.
    pub async fn update_contact_card(
        &self,
        config: &JmapConfig,
        remote_id: &str,
        display_name: &str,
        emails_json: &str,
        phones_json: &str,
        organization: Option<&str>,
        title: Option<&str>,
        notes: Option<&str>,
    ) -> Result<()> {
        log::info!("JMAP updating contact: '{}' ({})", display_name, remote_id);

        // Build name components from display_name
        let name_parts: Vec<&str> = display_name.split_whitespace().collect();
        let mut components = Vec::new();
        if let Some(first) = name_parts.first() {
            components.push(serde_json::json!({"kind": "given", "value": first}));
        }
        if name_parts.len() > 2 {
            let middle = name_parts[1..name_parts.len() - 1].join(" ");
            components.push(serde_json::json!({"kind": "given2", "value": middle}));
        }
        if name_parts.len() >= 2 {
            components
                .push(serde_json::json!({"kind": "surname", "value": name_parts.last().unwrap()}));
        }

        let mut updates = serde_json::json!({
            "name": {
                "components": components,
                "isOrdered": true,
            },
        });

        // Emails
        if let Ok(emails) = serde_json::from_str::<Vec<serde_json::Value>>(emails_json) {
            let mut emails_map = serde_json::Map::new();
            for (i, em) in emails.iter().enumerate() {
                let addr = em["email"].as_str().unwrap_or_default();
                if !addr.is_empty() {
                    emails_map.insert(format!("e{}", i), serde_json::json!({"address": addr}));
                }
            }
            updates["emails"] = serde_json::Value::Object(emails_map);
        }

        // Phones
        if let Ok(phones) = serde_json::from_str::<Vec<serde_json::Value>>(phones_json) {
            let mut phones_map = serde_json::Map::new();
            for (i, ph) in phones.iter().enumerate() {
                let number = ph["number"].as_str().unwrap_or_default();
                if !number.is_empty() {
                    phones_map.insert(format!("p{}", i), serde_json::json!({"number": number}));
                }
            }
            updates["phones"] = serde_json::Value::Object(phones_map);
        }

        // Organization
        if let Some(org) = organization.filter(|s| !s.is_empty()) {
            updates["organizations"] = serde_json::json!({"o0": {"name": org}});
        }

        // Title
        if let Some(t) = title.filter(|s| !s.is_empty()) {
            updates["titles"] = serde_json::json!({"t0": {"name": t}});
        }

        // Notes
        if let Some(n) = notes.filter(|s| !s.is_empty()) {
            updates["notes"] = serde_json::json!({"n0": {"note": n}});
        }

        let mut update_map = serde_json::Map::new();
        update_map.insert(remote_id.to_string(), updates);

        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"],
            "methodCalls": [
                ["ContactCard/set", {
                    "accountId": self.account_id,
                    "update": update_map,
                }, "u1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notUpdated"][remote_id].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP update contact failed: {}",
                desc
            )));
        }

        log::info!("JMAP updated contact '{}'", remote_id);
        Ok(())
    }

    /// Delete a contact on the JMAP server via ContactCard/set destroy.
    pub async fn delete_contact_card(&self, config: &JmapConfig, remote_id: &str) -> Result<()> {
        log::info!("JMAP deleting contact: {}", remote_id);

        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"],
            "methodCalls": [
                ["ContactCard/set", {
                    "accountId": self.account_id,
                    "destroy": [remote_id]
                }, "d1"]
            ]
        });

        self.api_request(&request, config).await?;
        log::info!("JMAP deleted contact '{}'", remote_id);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct JmapAddressBook {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct JmapContact {
    pub id: String,
    pub uid: Option<String>,
    pub display_name: String,
    pub emails_json: String,
    pub phones_json: String,
    pub organization: Option<String>,
    pub title: Option<String>,
    pub notes: Option<String>,
}
