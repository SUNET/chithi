use serde::{Deserialize, Serialize};

pub(crate) mod reconcile;

/// Provider-neutral contact shared by persistence and backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub id: String,
    pub book_id: String,
    pub uid: Option<String>,
    pub display_name: String,
    pub emails_json: String,
    pub phones_json: String,
    pub addresses_json: String,
    pub organization: Option<String>,
    pub title: Option<String>,
    pub notes: Option<String>,
    pub vcard_data: Option<String>,
    pub remote_id: Option<String>,
    pub etag: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::Contact;

    #[test]
    fn contact_json_contract_is_stable() {
        let contact = Contact {
            id: "contact-1".into(),
            book_id: "book-1".into(),
            uid: Some("uid-1".into()),
            display_name: "Ada Lovelace".into(),
            emails_json: r#"[{"email":"ada@example.com","label":"work"}]"#.into(),
            phones_json: r#"[{"number":"+46701234567","label":"mobile"}]"#.into(),
            addresses_json: r#"[{"city":"London","country":"UK"}]"#.into(),
            organization: Some("Analytical Engines".into()),
            title: Some("Programmer".into()),
            notes: Some("First algorithm".into()),
            vcard_data: Some("BEGIN:VCARD".into()),
            remote_id: Some("remote-1".into()),
            etag: Some("etag-1".into()),
        };
        let expected = serde_json::json!({
            "id": "contact-1",
            "book_id": "book-1",
            "uid": "uid-1",
            "display_name": "Ada Lovelace",
            "emails_json": r#"[{"email":"ada@example.com","label":"work"}]"#,
            "phones_json": r#"[{"number":"+46701234567","label":"mobile"}]"#,
            "addresses_json": r#"[{"city":"London","country":"UK"}]"#,
            "organization": "Analytical Engines",
            "title": "Programmer",
            "notes": "First algorithm",
            "vcard_data": "BEGIN:VCARD",
            "remote_id": "remote-1",
            "etag": "etag-1",
        });

        assert_eq!(serde_json::to_value(&contact).unwrap(), expected);
        let decoded: Contact = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);

        let minimal: Contact = serde_json::from_value(serde_json::json!({
            "id": "contact-2",
            "book_id": "book-1",
            "display_name": "Minimal",
            "emails_json": "[]",
            "phones_json": "[]",
            "addresses_json": "[]",
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(minimal).unwrap(),
            serde_json::json!({
                "id": "contact-2",
                "book_id": "book-1",
                "uid": null,
                "display_name": "Minimal",
                "emails_json": "[]",
                "phones_json": "[]",
                "addresses_json": "[]",
                "organization": null,
                "title": null,
                "notes": null,
                "vcard_data": null,
                "remote_id": null,
                "etag": null,
            })
        );
    }
}
