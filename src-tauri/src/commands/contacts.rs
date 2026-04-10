use serde::Deserialize;
use tauri::State;

use crate::db;
use crate::db::contacts::{CollectedContact, Contact, ContactBook};
use crate::error::Result;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Contact Books
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_contact_books(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Vec<ContactBook>> {
    let conn = state.db.lock().await;
    db::contacts::list_contact_books(&conn, &account_id)
}

// ---------------------------------------------------------------------------
// Contacts CRUD
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_contacts(
    state: State<'_, AppState>,
    book_id: String,
) -> Result<Vec<Contact>> {
    let conn = state.db.lock().await;
    db::contacts::list_contacts(&conn, &book_id)
}

#[tauri::command]
pub async fn get_contact(
    state: State<'_, AppState>,
    contact_id: String,
) -> Result<Contact> {
    let conn = state.db.lock().await;
    db::contacts::get_contact(&conn, &contact_id)
}

#[derive(Debug, Deserialize)]
pub struct NewContactInput {
    pub book_id: String,
    pub display_name: String,
    pub emails_json: String,
    pub phones_json: String,
    pub addresses_json: String,
    pub organization: Option<String>,
    pub title: Option<String>,
    pub notes: Option<String>,
}

#[tauri::command]
pub async fn create_contact(
    state: State<'_, AppState>,
    contact: NewContactInput,
) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let c = Contact {
        id: id.clone(),
        book_id: contact.book_id,
        uid: Some(format!("{}@chithi", uuid::Uuid::new_v4())),
        display_name: contact.display_name,
        emails_json: contact.emails_json,
        phones_json: contact.phones_json,
        addresses_json: contact.addresses_json,
        organization: contact.organization,
        title: contact.title,
        notes: contact.notes,
        vcard_data: None,
        remote_id: None,
        etag: None,
    };
    let conn = state.db.lock().await;
    db::contacts::insert_contact(&conn, &c)?;
    log::info!("Created contact {} '{}'", id, c.display_name);

    // Push to Google People API if applicable
    let book = conn.query_row(
        "SELECT cb.sync_type, cb.account_id FROM contact_books cb WHERE cb.id = ?1",
        rusqlite::params![c.book_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ).ok();
    drop(conn);

    if let Some((sync_type, account_id)) = book {
        if sync_type == "google" {
            if let Ok(token) = get_google_token(&account_id).await {
                let http = reqwest::Client::new();
                let mut person = serde_json::json!({
                    "names": [{"givenName": c.display_name}],
                });
                if let Ok(emails) = serde_json::from_str::<Vec<serde_json::Value>>(&c.emails_json) {
                    let ge: Vec<_> = emails.iter()
                        .filter_map(|e| e["email"].as_str().map(|addr| serde_json::json!({"value": addr})))
                        .collect();
                    if !ge.is_empty() { person["emailAddresses"] = serde_json::json!(ge); }
                }
                if let Ok(phones) = serde_json::from_str::<Vec<serde_json::Value>>(&c.phones_json) {
                    let gp: Vec<_> = phones.iter()
                        .filter_map(|p| p["number"].as_str().map(|n| serde_json::json!({"value": n})))
                        .collect();
                    if !gp.is_empty() { person["phoneNumbers"] = serde_json::json!(gp); }
                }
                match http.post("https://people.googleapis.com/v1/people:createContact")
                    .bearer_auth(&token)
                    .json(&person)
                    .send().await
                {
                    Ok(resp) if resp.status().is_success() => {
                        if let Ok(data) = resp.json::<serde_json::Value>().await {
                            if let Some(rn) = data["resourceName"].as_str() {
                                let conn = state.db.lock().await;
                                conn.execute(
                                    "UPDATE contacts SET remote_id = ?1 WHERE id = ?2",
                                    rusqlite::params![rn, id],
                                ).ok();
                                log::info!("Created contact on Google: {}", rn);
                            }
                        }
                    }
                    Ok(resp) => { let b = resp.text().await.unwrap_or_default(); log::error!("Google create contact failed: {}", b); }
                    Err(e) => log::error!("Google create contact request failed: {}", e),
                }
            }
        } else if sync_type == "o365" {
            if let Ok(token) = crate::mail::graph::get_graph_token(&account_id).await {
                let graph = crate::mail::graph::GraphClient::new(&token);
                let mut gc = serde_json::json!({
                    "displayName": c.display_name,
                });
                if let Ok(emails) = serde_json::from_str::<Vec<serde_json::Value>>(&c.emails_json) {
                    let ge: Vec<_> = emails.iter()
                        .filter_map(|e| e["email"].as_str().map(|addr| serde_json::json!({"address": addr, "name": ""})))
                        .collect();
                    if !ge.is_empty() { gc["emailAddresses"] = serde_json::json!(ge); }
                }
                if let Ok(phones) = serde_json::from_str::<Vec<serde_json::Value>>(&c.phones_json) {
                    let mobile = phones.iter().find(|p| p["label"].as_str() == Some("mobile"));
                    if let Some(m) = mobile.and_then(|p| p["number"].as_str()) {
                        gc["mobilePhone"] = serde_json::json!(m);
                    }
                    let biz: Vec<&str> = phones.iter()
                        .filter(|p| p["label"].as_str() == Some("work"))
                        .filter_map(|p| p["number"].as_str())
                        .collect();
                    if !biz.is_empty() { gc["businessPhones"] = serde_json::json!(biz); }
                }
                if let Some(ref org) = c.organization { gc["companyName"] = serde_json::json!(org); }
                if let Some(ref t) = c.title { gc["jobTitle"] = serde_json::json!(t); }
                match graph.create_contact(&gc).await {
                    Ok(remote_id) => {
                        let conn = state.db.lock().await;
                        conn.execute(
                            "UPDATE contacts SET remote_id = ?1 WHERE id = ?2",
                            rusqlite::params![remote_id, id],
                        ).ok();
                        log::info!("Created contact on Graph: {}", remote_id);
                    }
                    Err(e) => log::error!("Graph create contact failed: {}", e),
                }
            }
        } else if sync_type == "carddav" {
            let book_href = {
                let conn = state.db.lock().await;
                conn.query_row(
                    "SELECT remote_id FROM contact_books WHERE id = ?1",
                    rusqlite::params![c.book_id],
                    |row| row.get::<_, Option<String>>(0),
                ).ok().flatten()
            };
            if let Some(href) = book_href {
                let account = {
                    let conn = state.db.lock().await;
                    db::accounts::get_account_full(&conn, &account_id)?
                };
                match crate::mail::carddav::CardDavClient::connect(
                    &account.caldav_url, &account.username, &account.password, &account.email,
                ).await {
                    Ok(client) => {
                        let uid = c.uid.as_deref().unwrap_or(&id);
                        let emails: Vec<crate::mail::carddav::VCardEmail> = serde_json::from_str::<Vec<serde_json::Value>>(&c.emails_json)
                            .unwrap_or_default().iter()
                            .filter_map(|e| Some(crate::mail::carddav::VCardEmail { email: e["email"].as_str()?.to_string(), label: e["label"].as_str().unwrap_or("work").to_string() }))
                            .collect();
                        let phones: Vec<crate::mail::carddav::VCardPhone> = serde_json::from_str::<Vec<serde_json::Value>>(&c.phones_json)
                            .unwrap_or_default().iter()
                            .filter_map(|p| Some(crate::mail::carddav::VCardPhone { number: p["number"].as_str()?.to_string(), label: p["label"].as_str().unwrap_or("work").to_string() }))
                            .collect();
                        let vcard = crate::mail::carddav::generate_vcard(uid, &c.display_name, &emails, &phones, c.organization.as_deref(), c.title.as_deref(), c.notes.as_deref());
                        match client.put_contact(&href, uid, &vcard).await {
                            Ok(etag) => {
                                let remote_id = format!("{}/{}.vcf", href.trim_end_matches('/'), uid);
                                let conn = state.db.lock().await;
                                conn.execute("UPDATE contacts SET remote_id = ?1, etag = ?2, vcard_data = ?3 WHERE id = ?4", rusqlite::params![remote_id, etag, vcard, id]).ok();
                                log::info!("Created contact on CardDAV: {}", remote_id);
                            }
                            Err(e) => log::error!("CardDAV create contact failed: {}", e),
                        }
                    }
                    Err(e) => log::error!("CardDAV connect failed: {}", e),
                }
            }
        }
    }

    Ok(id)
}

#[tauri::command]
pub async fn update_contact(
    state: State<'_, AppState>,
    contact: Contact,
) -> Result<()> {
    let conn = state.db.lock().await;
    db::contacts::update_contact(&conn, &contact)?;
    log::info!("Updated contact {}", contact.id);

    // Push to Google People API if applicable
    if let Some(ref remote_id) = contact.remote_id {
        if !remote_id.is_empty() {
            let book_info = conn.query_row(
                "SELECT cb.sync_type, cb.account_id FROM contact_books cb WHERE cb.id = ?1",
                rusqlite::params![contact.book_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            ).ok();
            drop(conn);

            if let Some((sync_type, account_id)) = book_info {
                if sync_type == "google" {
                    if let Ok(token) = get_google_token(&account_id).await {
                        let http = reqwest::Client::new();
                        let mut person = serde_json::json!({
                            "names": [{"givenName": contact.display_name}],
                        });
                        if let Ok(emails) = serde_json::from_str::<Vec<serde_json::Value>>(&contact.emails_json) {
                            let ge: Vec<_> = emails.iter()
                                .filter_map(|e| e["email"].as_str().map(|a| serde_json::json!({"value": a})))
                                .collect();
                            if !ge.is_empty() { person["emailAddresses"] = serde_json::json!(ge); }
                        }
                        if let Ok(phones) = serde_json::from_str::<Vec<serde_json::Value>>(&contact.phones_json) {
                            let gp: Vec<_> = phones.iter()
                                .filter_map(|p| p["number"].as_str().map(|n| serde_json::json!({"value": n})))
                                .collect();
                            if !gp.is_empty() { person["phoneNumbers"] = serde_json::json!(gp); }
                        }
                        let url = format!(
                            "https://people.googleapis.com/v1/{}:updateContact?updatePersonFields=names,emailAddresses,phoneNumbers",
                            remote_id
                        );
                        match http.patch(&url).bearer_auth(&token).json(&person).send().await {
                            Ok(r) if r.status().is_success() => log::info!("Updated contact on Google: {}", remote_id),
                            Ok(r) => { let b = r.text().await.unwrap_or_default(); log::warn!("Google update contact failed: {}", b); }
                            Err(e) => log::warn!("Google update contact request failed: {}", e),
                        }
                    }
                } else if sync_type == "o365" {
                    if let Ok(token) = crate::mail::graph::get_graph_token(&account_id).await {
                        let graph = crate::mail::graph::GraphClient::new(&token);
                        let mut gc = serde_json::json!({
                            "displayName": contact.display_name,
                        });
                        if let Ok(emails) = serde_json::from_str::<Vec<serde_json::Value>>(&contact.emails_json) {
                            let ge: Vec<_> = emails.iter()
                                .filter_map(|e| e["email"].as_str().map(|addr| serde_json::json!({"address": addr, "name": ""})))
                                .collect();
                            if !ge.is_empty() { gc["emailAddresses"] = serde_json::json!(ge); }
                        }
                        if let Ok(phones) = serde_json::from_str::<Vec<serde_json::Value>>(&contact.phones_json) {
                            let mobile = phones.iter().find(|p| p["label"].as_str() == Some("mobile"));
                            if let Some(m) = mobile.and_then(|p| p["number"].as_str()) {
                                gc["mobilePhone"] = serde_json::json!(m);
                            }
                            let biz: Vec<&str> = phones.iter()
                                .filter(|p| p["label"].as_str() == Some("work"))
                                .filter_map(|p| p["number"].as_str())
                                .collect();
                            if !biz.is_empty() { gc["businessPhones"] = serde_json::json!(biz); }
                        }
                        if let Some(ref org) = contact.organization { gc["companyName"] = serde_json::json!(org); }
                        if let Some(ref t) = contact.title { gc["jobTitle"] = serde_json::json!(t); }
                        match graph.update_contact(remote_id, &gc).await {
                            Ok(()) => log::info!("Updated contact on Graph: {}", remote_id),
                            Err(e) => log::warn!("Graph update contact failed: {}", e),
                        }
                    }
                } else if sync_type == "jmap" {
                    let account = {
                        let conn = state.db.lock().await;
                        db::accounts::get_account_full(&conn, &account_id)?
                    };
                    let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;
                    match crate::mail::jmap::JmapConnection::connect(&jmap_config).await {
                        Ok(conn_jmap) => {
                            match conn_jmap.update_contact_card(
                                &jmap_config,
                                remote_id,
                                &contact.display_name,
                                &contact.emails_json,
                                &contact.phones_json,
                                contact.organization.as_deref(),
                                contact.title.as_deref(),
                                contact.notes.as_deref(),
                            ).await {
                                Ok(()) => log::info!("Updated contact on JMAP: {}", remote_id),
                                Err(e) => log::warn!("JMAP update contact failed: {}", e),
                            }
                        }
                        Err(e) => log::warn!("JMAP connect failed for contact update: {}", e),
                    }
                } else if sync_type == "carddav" {
                    let account = {
                        let conn = state.db.lock().await;
                        db::accounts::get_account_full(&conn, &account_id)?
                    };
                    match crate::mail::carddav::CardDavClient::connect(
                        &account.caldav_url, &account.username, &account.password, &account.email,
                    ).await {
                        Ok(client) => {
                            let uid = contact.uid.as_deref().unwrap_or(&contact.id);
                            let book_href = {
                                let conn = state.db.lock().await;
                                conn.query_row("SELECT remote_id FROM contact_books WHERE id = ?1", rusqlite::params![contact.book_id], |row| row.get::<_, Option<String>>(0)).ok().flatten().unwrap_or_default()
                            };
                            let emails: Vec<crate::mail::carddav::VCardEmail> = serde_json::from_str::<Vec<serde_json::Value>>(&contact.emails_json)
                                .unwrap_or_default().iter()
                                .filter_map(|e| Some(crate::mail::carddav::VCardEmail { email: e["email"].as_str()?.to_string(), label: e["label"].as_str().unwrap_or("work").to_string() }))
                                .collect();
                            let phones: Vec<crate::mail::carddav::VCardPhone> = serde_json::from_str::<Vec<serde_json::Value>>(&contact.phones_json)
                                .unwrap_or_default().iter()
                                .filter_map(|p| Some(crate::mail::carddav::VCardPhone { number: p["number"].as_str()?.to_string(), label: p["label"].as_str().unwrap_or("work").to_string() }))
                                .collect();
                            let vcard = crate::mail::carddav::generate_vcard(uid, &contact.display_name, &emails, &phones, contact.organization.as_deref(), contact.title.as_deref(), contact.notes.as_deref());
                            match client.put_contact(&book_href, uid, &vcard).await {
                                Ok(etag) => {
                                    let conn = state.db.lock().await;
                                    conn.execute("UPDATE contacts SET etag = ?1, vcard_data = ?2 WHERE id = ?3", rusqlite::params![etag, vcard, contact.id]).ok();
                                    log::info!("Updated contact on CardDAV: {}", remote_id);
                                }
                                Err(e) => log::warn!("CardDAV update contact failed: {}", e),
                            }
                        }
                        Err(e) => log::warn!("CardDAV connect failed for contact update: {}", e),
                    }
                }
            }
            return Ok(());
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn delete_contact(
    state: State<'_, AppState>,
    contact_id: String,
) -> Result<()> {
    // Check if this contact has a Google remote_id before deleting
    let conn = state.db.lock().await;
    let remote_info = conn.query_row(
        "SELECT c.remote_id, cb.sync_type, cb.account_id FROM contacts c JOIN contact_books cb ON c.book_id = cb.id WHERE c.id = ?1",
        rusqlite::params![contact_id],
        |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
    ).ok();
    db::contacts::delete_contact(&conn, &contact_id)?;
    log::info!("Deleted contact {}", contact_id);
    drop(conn);

    // Delete from Google People API if applicable
    if let Some((Some(remote_id), sync_type, account_id)) = remote_info {
        if sync_type == "google" && !remote_id.is_empty() {
            if let Ok(token) = get_google_token(&account_id).await {
                let http = reqwest::Client::new();
                let url = format!("https://people.googleapis.com/v1/{}:deleteContact", remote_id);
                match http.delete(&url).bearer_auth(&token).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        log::info!("Deleted contact from Google: {}", remote_id);
                    }
                    Ok(resp) => { let b = resp.text().await.unwrap_or_default(); log::warn!("Google delete contact failed: {}", b); }
                    Err(e) => log::warn!("Google delete contact request failed: {}", e),
                }
            }
        } else if sync_type == "o365" && !remote_id.is_empty() {
            if let Ok(token) = crate::mail::graph::get_graph_token(&account_id).await {
                let graph = crate::mail::graph::GraphClient::new(&token);
                match graph.delete_contact(&remote_id).await {
                    Ok(()) => log::info!("Deleted contact from Graph: {}", remote_id),
                    Err(e) => log::warn!("Graph delete contact failed: {}", e),
                }
            }
        } else if sync_type == "jmap" && !remote_id.is_empty() {
            let account = {
                let conn = state.db.lock().await;
                db::accounts::get_account_full(&conn, &account_id)?
            };
            let jmap_config = crate::commands::sync_cmd::build_jmap_config(&account).await?;
            match crate::mail::jmap::JmapConnection::connect(&jmap_config).await {
                Ok(conn_jmap) => {
                    match conn_jmap.delete_contact_card(&jmap_config, &remote_id).await {
                        Ok(()) => log::info!("Deleted contact from JMAP: {}", remote_id),
                        Err(e) => log::warn!("JMAP delete contact failed: {}", e),
                    }
                }
                Err(e) => log::warn!("JMAP connect failed for contact delete: {}", e),
            }
        } else if sync_type == "carddav" && !remote_id.is_empty() {
            let account = {
                let conn = state.db.lock().await;
                db::accounts::get_account_full(&conn, &account_id)?
            };
            match crate::mail::carddav::CardDavClient::connect(
                &account.caldav_url, &account.username, &account.password, &account.email,
            ).await {
                Ok(client) => {
                    match client.delete_contact(&remote_id).await {
                        Ok(()) => log::info!("Deleted contact from CardDAV: {}", remote_id),
                        Err(e) => log::warn!("CardDAV delete contact failed: {}", e),
                    }
                }
                Err(e) => log::warn!("CardDAV connect failed for contact delete: {}", e),
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn sync_contacts(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<()> {
    log::info!("sync_contacts: account={}", account_id);

    let account = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)?
    };

    if account.mail_protocol == "jmap" {
        sync_contacts_jmap(&state, &account_id, &account).await?;
    } else if account.provider == "gmail" {
        match sync_contacts_google(&state, &account_id, &account).await {
            Ok(()) => {}
            Err(e) => {
                log::warn!("sync_contacts: Gmail CardDAV failed (OAuth may not be set up): {}", e);
            }
        }
    } else if account.provider == "o365" {
        sync_contacts_graph(&state, &account_id).await?;
    } else if account.mail_protocol == "imap" {
        // Generic IMAP account — try CardDAV sync
        match sync_contacts_carddav(&state, &account_id, &account).await {
            Ok(()) => {}
            Err(e) => {
                log::warn!("sync_contacts: CardDAV failed for {}: {}", account_id, e);
            }
        }
    } else {
        log::debug!("sync_contacts: skipping account {} (no supported sync)", account_id);
    }

    log::info!("sync_contacts: completed for account {}", account_id);
    Ok(())
}

async fn get_google_token(account_id: &str) -> Result<String> {
    let tokens = crate::oauth::load_tokens(account_id)?
        .ok_or_else(|| crate::error::Error::Other(
            "No Google OAuth tokens. Please sign in with Google in Settings.".into(),
        ))?;

    if !tokens.is_expired() {
        return Ok(tokens.access_token);
    }

    let refresh_token = tokens.refresh_token
        .ok_or_else(|| crate::error::Error::Other("No refresh token".into()))?;
    let new_tokens = crate::oauth::refresh_access_token(&crate::oauth::GOOGLE, &refresh_token).await?;
    crate::oauth::store_tokens(account_id, &new_tokens)?;
    Ok(new_tokens.access_token)
}

async fn sync_contacts_google(
    state: &State<'_, AppState>,
    account_id: &str,
    _account: &db::accounts::AccountFull,
) -> Result<()> {
    // Get a valid OAuth2 access token
    let access_token = get_google_token(account_id).await?;

    let conn = state.db.lock().await;
    let book_id = {
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM contact_books WHERE account_id = ?1 AND sync_type = 'google'",
                rusqlite::params![account_id],
                |row| row.get(0),
            )
            .ok();

        if let Some(id) = existing {
            id
        } else {
            let id = uuid::Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO contact_books (id, account_id, name, sync_type) VALUES (?1, ?2, 'Google Contacts', 'google')",
                rusqlite::params![id, account_id],
            )?;
            id
        }
    };
    drop(conn);

    // Fetch contacts using Google People API (more reliable than CardDAV for Google)
    let http = reqwest::Client::new();
    let resp = http
        .get("https://people.googleapis.com/v1/people/me/connections")
        .bearer_auth(&access_token)
        .query(&[
            ("personFields", "names,emailAddresses,phoneNumbers,organizations"),
            ("pageSize", "1000"),
        ])
        .send()
        .await
        .map_err(|e| crate::error::Error::Other(format!("Google People API failed: {}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(crate::error::Error::Other(format!("Google People API error {}: {}", status, body)));
    }

    let data: serde_json::Value = resp.json().await
        .map_err(|e| crate::error::Error::Other(format!("Google People API parse error: {}", e)))?;

    let connections = data["connections"].as_array();
    let count = connections.map(|c| c.len()).unwrap_or(0);
    log::info!("sync_contacts_google: fetched {} contacts", count);

    let conn = state.db.lock().await;

    if let Some(people) = connections {
        for person in people {
            let resource_name = person["resourceName"].as_str().unwrap_or_default();

            // Parse name
            let display_name = person["names"]
                .as_array()
                .and_then(|names| names.first())
                .and_then(|n| n["displayName"].as_str())
                .unwrap_or("(No name)")
                .to_string();

            // Parse emails
            let mut emails = Vec::new();
            if let Some(email_list) = person["emailAddresses"].as_array() {
                for em in email_list {
                    let addr = em["value"].as_str().unwrap_or_default();
                    let label = em["type"].as_str().unwrap_or("other");
                    if !addr.is_empty() {
                        emails.push(serde_json::json!({"email": addr, "label": label}));
                    }
                }
            }

            // Parse phones
            let mut phones = Vec::new();
            if let Some(phone_list) = person["phoneNumbers"].as_array() {
                for ph in phone_list {
                    let number = ph["value"].as_str().unwrap_or_default();
                    let label = ph["type"].as_str().unwrap_or("mobile");
                    if !number.is_empty() {
                        phones.push(serde_json::json!({"number": number, "label": label}));
                    }
                }
            }

            // Parse organization
            let organization = person["organizations"]
                .as_array()
                .and_then(|orgs| orgs.first())
                .and_then(|o| o["name"].as_str())
                .map(|s| s.to_string());

            let title = person["organizations"]
                .as_array()
                .and_then(|orgs| orgs.first())
                .and_then(|o| o["title"].as_str())
                .map(|s| s.to_string());

            // Upsert by remote_id
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM contacts WHERE book_id = ?1 AND remote_id = ?2",
                    rusqlite::params![book_id, resource_name],
                    |row| row.get(0),
                )
                .ok();

            let emails_json = serde_json::to_string(&emails).unwrap_or_else(|_| "[]".to_string());
            let phones_json = serde_json::to_string(&phones).unwrap_or_else(|_| "[]".to_string());

            if let Some(id) = existing {
                conn.execute(
                    "UPDATE contacts SET display_name=?1, emails_json=?2, phones_json=?3, organization=?4, title=?5, updated_at=CURRENT_TIMESTAMP WHERE id=?6",
                    rusqlite::params![display_name, emails_json, phones_json, organization, title, id],
                )?;
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT INTO contacts (id, book_id, display_name, emails_json, phones_json, addresses_json, organization, title, remote_id) VALUES (?1, ?2, ?3, ?4, ?5, '[]', ?6, ?7, ?8)",
                    rusqlite::params![id, book_id, display_name, emails_json, phones_json, organization, title, resource_name],
                )?;
            }
        }
    }

    log::info!("sync_contacts_google: completed for account {}", account_id);
    Ok(())
}

async fn sync_contacts_jmap(
    state: &State<'_, AppState>,
    account_id: &str,
    account: &db::accounts::AccountFull,
) -> Result<()> {
    use crate::mail::jmap::JmapConnection;

    let jmap_config = crate::commands::sync_cmd::build_jmap_config(account).await?;

    let jmap_conn = JmapConnection::connect(&jmap_config).await?;

    // Step 1: Fetch address books
    let address_books = jmap_conn.list_address_books(&jmap_config).await?;
    log::info!("sync_contacts: fetched {} address books from JMAP", address_books.len());

    let mut remote_to_local: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    {
        let conn = state.db.lock().await;
        for ab in &address_books {
            // Upsert contact book
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM contact_books WHERE account_id = ?1 AND remote_id = ?2",
                    rusqlite::params![account_id, ab.id],
                    |row| row.get(0),
                )
                .ok();

            let local_id = if let Some(id) = existing {
                conn.execute(
                    "UPDATE contact_books SET name = ?1 WHERE id = ?2",
                    rusqlite::params![ab.name, id],
                )?;
                id
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT INTO contact_books (id, account_id, name, remote_id, sync_type) VALUES (?1, ?2, ?3, ?4, 'jmap')",
                    rusqlite::params![id, account_id, ab.name, ab.id],
                )?;
                id
            };
            remote_to_local.insert(ab.id.clone(), local_id);
        }
    }

    // Step 2: Fetch contacts for each address book
    for ab in &address_books {
        let jmap_contacts = match jmap_conn.fetch_contacts(&jmap_config, Some(&ab.id)).await {
            Ok(c) => c,
            Err(e) => {
                log::error!("sync_contacts: failed to fetch contacts for '{}': {}", ab.name, e);
                continue;
            }
        };

        log::info!("sync_contacts: fetched {} contacts for '{}'", jmap_contacts.len(), ab.name);

        let local_book_id = remote_to_local.get(&ab.id).cloned().unwrap_or_default();
        let conn = state.db.lock().await;

        for jc in &jmap_contacts {
            // Upsert by remote_id
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM contacts WHERE book_id = ?1 AND remote_id = ?2",
                    rusqlite::params![local_book_id, jc.id],
                    |row| row.get(0),
                )
                .ok();

            if let Some(id) = existing {
                conn.execute(
                    "UPDATE contacts SET display_name=?1, emails_json=?2, phones_json=?3, organization=?4, title=?5, notes=?6, uid=?7, updated_at=CURRENT_TIMESTAMP WHERE id=?8",
                    rusqlite::params![jc.display_name, jc.emails_json, jc.phones_json, jc.organization, jc.title, jc.notes, jc.uid, id],
                )?;
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT INTO contacts (id, book_id, uid, display_name, emails_json, phones_json, addresses_json, organization, title, notes, remote_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, '[]', ?7, ?8, ?9, ?10)",
                    rusqlite::params![id, local_book_id, jc.uid, jc.display_name, jc.emails_json, jc.phones_json, jc.organization, jc.title, jc.notes, jc.id],
                )?;
            }
        }

        // Remove contacts deleted on server
        let server_ids: std::collections::HashSet<String> =
            jmap_contacts.iter().map(|c| c.id.clone()).collect();
        let local_synced: Vec<(String, String)> = conn
            .prepare(
                "SELECT id, remote_id FROM contacts WHERE book_id = ?1 AND remote_id IS NOT NULL AND remote_id != ''",
            )
            .and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![local_book_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();

        let mut deleted = 0u32;
        for (local_id, remote_id) in &local_synced {
            if !server_ids.contains(remote_id) {
                conn.execute("DELETE FROM contacts WHERE id = ?1", rusqlite::params![local_id]).ok();
                deleted += 1;
            }
        }
        if deleted > 0 {
            log::info!("sync_contacts: removed {} server-deleted contacts from '{}'", deleted, ab.name);
        }

        // Push local contacts (no remote_id) to server
        type UnpushedRow = (String, String, String, String, Option<String>, Option<String>, Option<String>);
        let unpushed: Vec<UnpushedRow> = conn
            .prepare(
                "SELECT id, display_name, emails_json, phones_json, organization, title, notes
                 FROM contacts WHERE book_id = ?1 AND (remote_id IS NULL OR remote_id = '')",
            )
            .and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![local_book_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();

        if !unpushed.is_empty() {
            log::info!("sync_contacts: pushing {} local contacts to JMAP for '{}'", unpushed.len(), ab.name);
            drop(conn); // Release lock for async calls

            for (local_id, name, emails, phones, org, title, notes) in &unpushed {
                match jmap_conn.create_contact_card(
                    &jmap_config,
                    &ab.id,
                    name,
                    emails,
                    phones,
                    org.as_deref(),
                    title.as_deref(),
                    notes.as_deref(),
                ).await {
                    Ok(remote_id) => {
                        log::info!("sync_contacts: pushed contact '{}' to JMAP, remote_id={}", name, remote_id);
                        let conn = state.db.lock().await;
                        conn.execute(
                            "UPDATE contacts SET remote_id = ?1 WHERE id = ?2",
                            rusqlite::params![remote_id, local_id],
                        ).ok();
                    }
                    Err(e) => {
                        log::error!("sync_contacts: failed to push contact '{}': {}", name, e);
                    }
                }
            }
        }
    }

    Ok(())
}

async fn sync_contacts_carddav(
    state: &State<'_, AppState>,
    account_id: &str,
    account: &db::accounts::AccountFull,
) -> Result<()> {
    use crate::mail::carddav::{CardDavClient, parse_vcard};

    log::info!("sync_contacts_carddav: starting for account {}", account_id);

    // Use caldav_url for CardDAV too (same server usually hosts both).
    // If empty, auto-discovery will try .well-known/carddav.
    let client = CardDavClient::connect(
        &account.caldav_url,
        &account.username,
        &account.password,
        &account.email,
    )
    .await?;

    let address_books = client.list_addressbooks().await?;
    log::info!(
        "sync_contacts_carddav: found {} address books",
        address_books.len()
    );

    for ab in &address_books {
        // Upsert contact book in DB
        let book_id = {
            let conn = state.db.lock().await;
            let book = db::contacts::ContactBook {
                id: uuid::Uuid::new_v4().to_string(),
                account_id: account_id.to_string(),
                name: ab.name.clone(),
                remote_id: Some(ab.href.clone()),
                sync_type: "carddav".to_string(),
            };

            // Check if book already exists by remote_id
            let existing = db::contacts::list_contact_books(&conn, account_id)?;
            let found = existing
                .iter()
                .find(|b| b.remote_id.as_deref() == Some(&ab.href));
            if let Some(existing_book) = found {
                existing_book.id.clone()
            } else {
                db::contacts::insert_contact_book(&conn, &book)?;
                book.id
            }
        };

        // Fetch contacts from server
        let server_contacts = match client.fetch_contacts(&ab.href).await {
            Ok(c) => c,
            Err(e) => {
                log::error!(
                    "sync_contacts_carddav: failed to fetch contacts from '{}': {}",
                    ab.name,
                    e
                );
                continue;
            }
        };

        log::info!(
            "sync_contacts_carddav: fetched {} contacts from '{}'",
            server_contacts.len(),
            ab.name
        );

        let conn = state.db.lock().await;

        // Get existing local contacts for this book
        let local_contacts = db::contacts::list_contacts(&conn, &book_id)?;
        let mut local_by_uid: std::collections::HashMap<String, db::contacts::Contact> =
            local_contacts
                .into_iter()
                .filter_map(|c| c.uid.clone().map(|uid| (uid, c)))
                .collect();

        // Upsert server contacts
        for sc in &server_contacts {
            let parsed = parse_vcard(&sc.vcard_data);

            let emails_json = serde_json::to_string(&parsed.emails).unwrap_or_else(|_| "[]".to_string());
            let phones_json = serde_json::to_string(&parsed.phones).unwrap_or_else(|_| "[]".to_string());

            if let Some(existing) = local_by_uid.remove(&sc.uid) {
                // Update if etag changed
                if existing.etag.as_deref() != Some(&sc.etag) {
                    let updated = db::contacts::Contact {
                        display_name: parsed.display_name,
                        emails_json,
                        phones_json,
                        organization: parsed.organization,
                        title: parsed.title,
                        notes: parsed.note,
                        vcard_data: Some(sc.vcard_data.clone()),
                        etag: Some(sc.etag.clone()),
                        remote_id: Some(sc.href.clone()),
                        ..existing
                    };
                    db::contacts::update_contact(&conn, &updated)?;
                }
            } else {
                // New contact from server
                let contact = db::contacts::Contact {
                    id: uuid::Uuid::new_v4().to_string(),
                    book_id: book_id.clone(),
                    uid: Some(sc.uid.clone()),
                    display_name: parsed.display_name,
                    emails_json,
                    phones_json,
                    addresses_json: "[]".to_string(),
                    organization: parsed.organization,
                    title: parsed.title,
                    notes: parsed.note,
                    vcard_data: Some(sc.vcard_data.clone()),
                    remote_id: Some(sc.href.clone()),
                    etag: Some(sc.etag.clone()),
                };
                db::contacts::insert_contact(&conn, &contact)?;
            }
        }

        // Remove contacts deleted on server
        let deleted: usize = local_by_uid.len();
        for orphan in local_by_uid.values() {
            // Only delete if it had a remote_id (was synced from server)
            if orphan.remote_id.is_some() {
                db::contacts::delete_contact(&conn, &orphan.id)?;
            }
        }
        if deleted > 0 {
            log::info!(
                "sync_contacts_carddav: removed {} server-deleted contacts from '{}'",
                deleted,
                ab.name
            );
        }
    }

    log::info!("sync_contacts_carddav: completed for account {}", account_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Search (for compose autocomplete)
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn search_contacts(
    state: State<'_, AppState>,
    query: String,
) -> Result<Vec<Contact>> {
    let conn = state.db.lock().await;
    db::contacts::search_all_contacts(&conn, &query)
}

#[tauri::command]
pub async fn search_collected_contacts(
    state: State<'_, AppState>,
    query: String,
) -> Result<Vec<CollectedContact>> {
    let conn = state.db.lock().await;
    db::contacts::search_collected_contacts(&conn, &query)
}

// ---------------------------------------------------------------------------
// Microsoft Graph contacts sync
// ---------------------------------------------------------------------------

async fn sync_contacts_graph(
    state: &State<'_, AppState>,
    account_id: &str,
) -> Result<()> {
    log::info!("sync_contacts_graph: starting for account {}", account_id);

    let token = match crate::mail::graph::get_graph_token(account_id).await {
        Ok(t) => t,
        Err(e) => {
            log::error!("sync_contacts_graph: failed to get token: {}", e);
            return Err(e);
        }
    };
    let client = crate::mail::graph::GraphClient::new(&token);

    // 1. Ensure contact book exists
    let book_id = {
        let conn = state.db.lock().await;
        let existing: Option<String> = conn.query_row(
            "SELECT id FROM contact_books WHERE account_id = ?1 AND sync_type = 'o365'",
            rusqlite::params![account_id],
            |row| row.get(0),
        ).ok();

        match existing {
            Some(id) => id,
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT INTO contact_books (id, account_id, name, sync_type) VALUES (?1, ?2, 'Outlook Contacts', 'o365')",
                    rusqlite::params![id, account_id],
                )?;
                log::info!("sync_contacts_graph: created contact book 'Outlook Contacts'");
                id
            }
        }
    };

    // 2. Fetch contacts from Graph
    let graph_contacts = match client.list_contacts().await {
        Ok(c) => c,
        Err(e) => {
            log::error!("sync_contacts_graph: list_contacts failed: {}", e);
            return Err(e);
        }
    };
    log::info!("sync_contacts_graph: fetched {} contacts", graph_contacts.len());

    let conn = state.db.lock().await;

    // Build set of server IDs for reconciliation
    let server_ids: std::collections::HashSet<String> =
        graph_contacts.iter().map(|c| c.id.clone()).collect();

    // 3. Upsert contacts
    for gc in &graph_contacts {
        let existing: Option<String> = conn.query_row(
            "SELECT id FROM contacts WHERE book_id = ?1 AND remote_id = ?2",
            rusqlite::params![book_id, gc.id],
            |row| row.get(0),
        ).ok();

        match existing {
            Some(local_id) => {
                // Update existing contact
                conn.execute(
                    "UPDATE contacts SET display_name = ?1, emails_json = ?2, phones_json = ?3,
                     organization = ?4, title = ?5, updated_at = CURRENT_TIMESTAMP
                     WHERE id = ?6",
                    rusqlite::params![
                        gc.display_name,
                        gc.emails_json,
                        gc.phones_json,
                        gc.organization,
                        gc.title,
                        local_id,
                    ],
                ).ok();
            }
            None => {
                // Insert new contact
                let id = uuid::Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT INTO contacts (id, book_id, display_name, emails_json, phones_json, organization, title, remote_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![
                        id,
                        book_id,
                        gc.display_name,
                        gc.emails_json,
                        gc.phones_json,
                        gc.organization,
                        gc.title,
                        gc.id,
                    ],
                )?;
            }
        }
    }

    // 4. Remove contacts deleted on server
    let local_contacts: Vec<(String, String)> = conn
        .prepare(
            "SELECT id, remote_id FROM contacts WHERE book_id = ?1 AND remote_id IS NOT NULL AND remote_id != ''",
        )?
        .query_map(rusqlite::params![book_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut deleted = 0;
    for (local_id, remote_id) in &local_contacts {
        if !server_ids.contains(remote_id) {
            conn.execute("DELETE FROM contacts WHERE id = ?1", rusqlite::params![local_id]).ok();
            deleted += 1;
        }
    }
    if deleted > 0 {
        log::info!("sync_contacts_graph: removed {} server-deleted contacts", deleted);
    }

    log::info!("sync_contacts_graph: completed for account {}", account_id);
    Ok(())
}
