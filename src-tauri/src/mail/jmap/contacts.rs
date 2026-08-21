//! JMAP Contacts transport (RFC 9610) using JSContact cards (RFC 9553).

use std::collections::HashSet;

use crate::error::{Error, Result};

use super::{JmapConfig, JmapConnection, CONTACTS_CAPABILITY, CORE_CAPABILITY};

const ADDRESS_BOOK_CALL_ID: &str = "ab1";
const QUERY_CALL_ID: &str = "q1";
const RECHECK_CALL_ID: &str = "qr1";
const CREATE_CALL_ID: &str = "s1";
const UPDATE_CALL_ID: &str = "u1";
const DELETE_CALL_ID: &str = "d1";
const CREATE_ID: &str = "new1";
const MAX_FETCH_ATTEMPTS: usize = 3;
const CONTACT_PAGE_CAP: usize = 500;
const ADDRESS_BOOK_PROPERTIES: [&str; 2] = ["id", "name"];
const CONTACT_CARD_PROPERTIES: [&str; 9] = [
    "id",
    "uid",
    "addressBookIds",
    "name",
    "emails",
    "phones",
    "organizations",
    "titles",
    "notes",
];

impl JmapConnection {
    fn contacts_account_id(&self) -> Result<&str> {
        self.contacts_account_id
            .as_deref()
            .ok_or_else(|| Error::Other("JMAP session has no contacts-capable account".into()))
    }

    fn contacts_read_session(&self) -> Result<(&str, usize)> {
        let account_id = self.contacts_account_id()?;
        if self.max_objects_in_get == 0 {
            return Err(Error::Other(
                "JMAP session advertises maxObjectsInGet=0 for contact reads".into(),
            ));
        }
        Ok((account_id, self.max_objects_in_get.min(CONTACT_PAGE_CAP)))
    }

    /// Return a complete, validated list of address books.
    pub(crate) async fn list_address_books(
        &self,
        config: &JmapConfig,
    ) -> Result<Vec<JmapAddressBook>> {
        let (account_id, _) = self.contacts_read_session()?;
        let request = serde_json::json!({
            "using": [CORE_CAPABILITY, CONTACTS_CAPABILITY],
            "methodCalls": [["AddressBook/get", {
                "accountId": account_id,
                "properties": ADDRESS_BOOK_PROPERTIES
            }, ADDRESS_BOOK_CALL_ID]]
        });

        let response = self.api_request(&request, config).await?;
        let books = parse_address_books_response(&response, account_id)?;
        log::info!("JMAP fetched {} address books", books.len());
        Ok(books)
    }

    /// Return one complete account-wide ContactCard snapshot. A changing
    /// query is retried from scratch; partial attempts are never returned.
    pub(crate) async fn fetch_contacts(&self, config: &JmapConfig) -> Result<Vec<JmapContact>> {
        let (account_id, max_objects_in_get) = self.contacts_read_session()?;

        for attempt in 1..=MAX_FETCH_ATTEMPTS {
            match self
                .fetch_contacts_attempt(config, account_id, max_objects_in_get)
                .await?
            {
                FetchAttempt::Complete(contacts) => {
                    log::info!("JMAP fetched {} contacts", contacts.len());
                    return Ok(contacts);
                }
                FetchAttempt::Changed if attempt < MAX_FETCH_ATTEMPTS => {
                    log::info!(
                        "JMAP contacts changed during fetch; retrying ({}/{})",
                        attempt,
                        MAX_FETCH_ATTEMPTS
                    );
                }
                FetchAttempt::Changed => {
                    return Err(Error::Other(
                        "JMAP contacts kept changing during complete fetch".into(),
                    ));
                }
            }
        }

        Err(Error::Other("JMAP contact fetch retry exhausted".into()))
    }

    async fn fetch_contacts_attempt(
        &self,
        config: &JmapConfig,
        account_id: &str,
        max_objects_in_get: usize,
    ) -> Result<FetchAttempt> {
        let mut ids = Vec::new();
        let mut seen_query_ids = HashSet::new();
        let mut position = 0usize;
        let mut query_state: Option<String> = None;
        let mut total: Option<usize> = None;

        loop {
            let request =
                contact_query_request(account_id, position, max_objects_in_get, QUERY_CALL_ID);
            let response = self.api_request(&request, config).await?;
            let page = parse_query_response(&response, QUERY_CALL_ID, account_id)?;

            if page.position != position || page.ids.len() > max_objects_in_get {
                return Err(Error::Other(
                    "Malformed JMAP ContactCard/query page bounds".into(),
                ));
            }
            if query_state
                .as_deref()
                .is_some_and(|state| state != page.query_state)
            {
                return Ok(FetchAttempt::Changed);
            }
            if total.is_some_and(|expected| expected != page.total) {
                return Err(Error::Other(
                    "JMAP ContactCard/query total changed without a new state".into(),
                ));
            }
            query_state.get_or_insert_with(|| page.query_state.clone());
            total.get_or_insert(page.total);

            if position
                .checked_add(page.ids.len())
                .is_none_or(|end| end > page.total)
            {
                return Err(Error::Other(
                    "Malformed JMAP ContactCard/query result count".into(),
                ));
            }
            for id in page.ids {
                if !seen_query_ids.insert(id.clone()) {
                    return Err(Error::Other(
                        "JMAP ContactCard/query returned a duplicate id".into(),
                    ));
                }
                ids.push(id);
            }

            position = ids.len();
            if position == page.total {
                break;
            }
            if position == page.position {
                return Err(Error::Other(
                    "JMAP ContactCard/query stopped before the reported total".into(),
                ));
            }
        }

        let query_state = query_state
            .ok_or_else(|| Error::Other("JMAP ContactCard/query omitted its query state".into()))?;
        let total =
            total.ok_or_else(|| Error::Other("JMAP ContactCard/query omitted its total".into()))?;
        if ids.len() != total {
            return Err(Error::Other(
                "JMAP ContactCard/query returned an incomplete id set".into(),
            ));
        }

        let mut contacts = Vec::with_capacity(ids.len());
        let mut seen_card_ids = HashSet::new();
        let mut seen_uids = HashSet::new();
        let mut get_state: Option<String> = None;

        for (chunk_index, chunk) in ids.chunks(max_objects_in_get).enumerate() {
            let call_id = format!("g{}", chunk_index + 1);
            let request = contact_get_request(account_id, chunk, &call_id);
            let response = self.api_request(&request, config).await?;
            let mut chunk_card_ids = HashSet::new();
            let mut chunk_uids = HashSet::new();
            let get_result = parse_contact_get_response(
                &response,
                &call_id,
                account_id,
                chunk,
                &mut chunk_card_ids,
                &mut chunk_uids,
            )?;
            let ContactGetResult::Complete {
                state,
                contacts: mut chunk_contacts,
            } = get_result
            else {
                return Ok(FetchAttempt::Changed);
            };
            if get_state
                .as_deref()
                .is_some_and(|expected| expected != state)
            {
                return Ok(FetchAttempt::Changed);
            }
            if !seen_card_ids.is_disjoint(&chunk_card_ids) {
                return Err(Error::Other(
                    "JMAP ContactCard/get returned a duplicate id".into(),
                ));
            }
            if !seen_uids.is_disjoint(&chunk_uids) {
                return Err(Error::Other(
                    "JMAP ContactCard/get returned a duplicate uid".into(),
                ));
            }
            seen_card_ids.extend(chunk_card_ids);
            seen_uids.extend(chunk_uids);
            get_state.get_or_insert(state);
            contacts.append(&mut chunk_contacts);
        }

        let request = contact_query_request(account_id, 0, 0, RECHECK_CALL_ID);
        let response = self.api_request(&request, config).await?;
        let recheck = parse_query_response(&response, RECHECK_CALL_ID, account_id)?;
        if recheck.position != 0 || !recheck.ids.is_empty() {
            return Err(Error::Other(
                "Malformed JMAP ContactCard/query state recheck".into(),
            ));
        }
        if recheck.query_state != query_state || recheck.total != total {
            return Ok(FetchAttempt::Changed);
        }

        contacts.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(FetchAttempt::Complete(contacts))
    }

    /// Create a JSContact card and return its server-assigned JMAP id.
    pub(crate) async fn create_contact_card(
        &self,
        config: &JmapConfig,
        address_book_id: &str,
        uid: &str,
        display_name: &str,
        emails_json: &str,
        phones_json: &str,
        organization: Option<&str>,
        title: Option<&str>,
        notes: Option<&str>,
    ) -> Result<String> {
        let card = build_create_card(
            address_book_id,
            uid,
            display_name,
            emails_json,
            phones_json,
            organization,
            title,
            notes,
        )?;
        let account_id = self.contacts_account_id()?;
        let mut create = serde_json::Map::new();
        create.insert(CREATE_ID.into(), card);
        let request = serde_json::json!({
            "using": [CORE_CAPABILITY, CONTACTS_CAPABILITY],
            "methodCalls": [["ContactCard/set", {
                "accountId": account_id,
                "create": create
            }, CREATE_CALL_ID]]
        });

        let response = self.api_request(&request, config).await?;
        let remote_id = validate_create_response(&response, account_id)?;
        log::info!("JMAP created contact id={}", remote_id);
        Ok(remote_id)
    }

    /// Replace every locally owned mutable field via a JMAP PatchObject.
    pub(crate) async fn update_contact_card(
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
        require_id(remote_id, "contact")?;
        let patch = build_update_patch(
            display_name,
            emails_json,
            phones_json,
            organization,
            title,
            notes,
        )?;
        let account_id = self.contacts_account_id()?;
        let mut update = serde_json::Map::new();
        update.insert(remote_id.into(), patch);
        let request = serde_json::json!({
            "using": [CORE_CAPABILITY, CONTACTS_CAPABILITY],
            "methodCalls": [["ContactCard/set", {
                "accountId": account_id,
                "update": update
            }, UPDATE_CALL_ID]]
        });

        let response = self.api_request(&request, config).await?;
        validate_update_response(&response, account_id, remote_id)?;
        log::info!("JMAP updated contact id={}", remote_id);
        Ok(())
    }

    /// Destroy a ContactCard globally. This does not remove just one
    /// address-book membership.
    pub(crate) async fn delete_contact_card(
        &self,
        config: &JmapConfig,
        remote_id: &str,
    ) -> Result<()> {
        require_id(remote_id, "contact")?;
        let account_id = self.contacts_account_id()?;
        let request = serde_json::json!({
            "using": [CORE_CAPABILITY, CONTACTS_CAPABILITY],
            "methodCalls": [["ContactCard/set", {
                "accountId": account_id,
                "destroy": [remote_id]
            }, DELETE_CALL_ID]]
        });

        let response = self.api_request(&request, config).await?;
        validate_delete_response(&response, account_id, remote_id)?;
        log::info!("JMAP deleted contact id={}", remote_id);
        Ok(())
    }
}

enum FetchAttempt {
    Complete(Vec<JmapContact>),
    Changed,
}

enum ContactGetResult {
    Complete {
        state: String,
        contacts: Vec<JmapContact>,
    },
    Changed,
}

#[derive(Debug)]
struct QueryPage {
    query_state: String,
    position: usize,
    total: usize,
    ids: Vec<String>,
}

fn contact_query_request(
    account_id: &str,
    position: usize,
    limit: usize,
    call_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "using": [CORE_CAPABILITY, CONTACTS_CAPABILITY],
        "methodCalls": [["ContactCard/query", {
            "accountId": account_id,
            "position": position,
            "limit": limit,
            "calculateTotal": true
        }, call_id]]
    })
}

fn contact_get_request(account_id: &str, ids: &[String], call_id: &str) -> serde_json::Value {
    serde_json::json!({
        "using": [CORE_CAPABILITY, CONTACTS_CAPABILITY],
        "methodCalls": [["ContactCard/get", {
            "accountId": account_id,
            "ids": ids,
            "properties": CONTACT_CARD_PROPERTIES
        }, call_id]]
    })
}

fn single_method_body<'a>(
    response: &'a serde_json::Value,
    expected_method: &str,
    expected_call_id: &str,
    expected_account_id: &str,
) -> Result<&'a serde_json::Map<String, serde_json::Value>> {
    response
        .get("sessionState")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::Other("Malformed JMAP response sessionState".into()))?;
    let responses = response
        .get("methodResponses")
        .and_then(serde_json::Value::as_array)
        .filter(|responses| responses.len() == 1)
        .ok_or_else(|| Error::Other(format!("Malformed JMAP {expected_method} response count")))?;
    let tuple = responses[0]
        .as_array()
        .filter(|tuple| tuple.len() == 3)
        .ok_or_else(|| Error::Other(format!("Malformed JMAP {expected_method} response tuple")))?;
    if tuple[2].as_str() != Some(expected_call_id) {
        return Err(Error::Other(format!(
            "JMAP {expected_method} returned an unexpected call id"
        )));
    }
    let method = tuple[0]
        .as_str()
        .ok_or_else(|| Error::Other(format!("Malformed JMAP {expected_method} method")))?;
    if method == "error" {
        return Err(Error::Other(format!(
            "JMAP {expected_method} failed (type={})",
            super::safe_jmap_error_type(&tuple[1])
        )));
    }
    if method != expected_method {
        return Err(Error::Other(format!(
            "JMAP {expected_method} returned an unexpected method"
        )));
    }
    let body = tuple[1]
        .as_object()
        .ok_or_else(|| Error::Other(format!("Malformed JMAP {expected_method} body")))?;
    if body.get("accountId").and_then(serde_json::Value::as_str) != Some(expected_account_id) {
        return Err(Error::Other(format!(
            "JMAP {expected_method} returned an unexpected account id"
        )));
    }
    Ok(body)
}

fn parse_address_books_response(
    response: &serde_json::Value,
    account_id: &str,
) -> Result<Vec<JmapAddressBook>> {
    let body = single_method_body(
        response,
        "AddressBook/get",
        ADDRESS_BOOK_CALL_ID,
        account_id,
    )?;
    require_string(body, "state", "AddressBook/get")?;
    require_empty_not_found(body, "AddressBook/get")?;
    let list = body
        .get("list")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::Other("Malformed JMAP AddressBook/get list".into()))?;

    let mut seen = HashSet::new();
    let mut books = Vec::with_capacity(list.len());
    for value in list {
        let book = value
            .as_object()
            .ok_or_else(|| Error::Other("Malformed JMAP address book".into()))?;
        let id = book
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::Other("JMAP address book omitted its id".into()))?;
        require_id(id, "address book")?;
        if !seen.insert(id) {
            return Err(Error::Other(
                "JMAP AddressBook/get returned a duplicate id".into(),
            ));
        }
        let name = book
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| Error::Other("JMAP address book has an invalid name".into()))?;
        books.push(JmapAddressBook {
            id: id.into(),
            name: name.into(),
        });
    }
    books.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(books)
}

fn parse_query_response(
    response: &serde_json::Value,
    call_id: &str,
    account_id: &str,
) -> Result<QueryPage> {
    let body = single_method_body(response, "ContactCard/query", call_id, account_id)?;
    let query_state = require_string(body, "queryState", "ContactCard/query")?.to_string();
    body.get("canCalculateChanges")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| {
            Error::Other("Malformed JMAP ContactCard/query canCalculateChanges".into())
        })?;
    let position = require_unsigned(body, "position", "ContactCard/query")?;
    let total = require_unsigned(body, "total", "ContactCard/query")?;
    let values = body
        .get("ids")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::Other("Malformed JMAP ContactCard/query ids".into()))?;
    let mut page_ids = HashSet::new();
    let mut ids = Vec::with_capacity(values.len());
    for value in values {
        let id = value
            .as_str()
            .ok_or_else(|| Error::Other("Malformed JMAP ContactCard/query id".into()))?;
        require_id(id, "contact")?;
        if !page_ids.insert(id) {
            return Err(Error::Other(
                "JMAP ContactCard/query returned a duplicate id".into(),
            ));
        }
        ids.push(id.into());
    }
    Ok(QueryPage {
        query_state,
        position,
        total,
        ids,
    })
}

fn parse_contact_get_response(
    response: &serde_json::Value,
    call_id: &str,
    account_id: &str,
    requested_ids: &[String],
    seen_card_ids: &mut HashSet<String>,
    seen_uids: &mut HashSet<String>,
) -> Result<ContactGetResult> {
    let body = single_method_body(response, "ContactCard/get", call_id, account_id)?;
    let state = require_string(body, "state", "ContactCard/get")?.to_string();
    let list = body
        .get("list")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::Other("Malformed JMAP ContactCard/get list".into()))?;

    let requested: HashSet<&str> = requested_ids.iter().map(String::as_str).collect();
    if requested.len() != requested_ids.len() {
        return Err(Error::Other(
            "JMAP ContactCard/get was given duplicate ids".into(),
        ));
    }
    let not_found_values = body
        .get("notFound")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::Other("Malformed JMAP ContactCard/get notFound".into()))?;
    let mut not_found = HashSet::with_capacity(not_found_values.len());
    for value in not_found_values {
        let id = value
            .as_str()
            .ok_or_else(|| Error::Other("Malformed JMAP ContactCard/get notFound id".into()))?;
        require_id(id, "not-found contact")?;
        if !requested.contains(id) {
            return Err(Error::Other(
                "JMAP ContactCard/get returned an unrequested notFound id".into(),
            ));
        }
        if !not_found.insert(id) {
            return Err(Error::Other(
                "JMAP ContactCard/get returned a duplicate notFound id".into(),
            ));
        }
    }
    let mut returned = HashSet::with_capacity(list.len());
    let mut returned_uids = HashSet::with_capacity(list.len());
    let mut contacts = Vec::with_capacity(list.len());
    for value in list {
        let contact = parse_contact_card(value)?;
        if !requested.contains(contact.id.as_str()) {
            return Err(Error::Other(
                "JMAP ContactCard/get returned an unrequested id".into(),
            ));
        }
        if not_found.contains(contact.id.as_str()) {
            return Err(Error::Other(
                "JMAP ContactCard/get returned an id in list and notFound".into(),
            ));
        }
        if !returned.insert(contact.id.clone()) || seen_card_ids.contains(&contact.id) {
            return Err(Error::Other(
                "JMAP ContactCard/get returned a duplicate id".into(),
            ));
        }
        if !returned_uids.insert(contact.uid.clone()) || seen_uids.contains(&contact.uid) {
            return Err(Error::Other(
                "JMAP ContactCard/get returned a duplicate uid".into(),
            ));
        }
        contacts.push(contact);
    }
    if returned.len() + not_found.len() != requested.len()
        || requested
            .iter()
            .any(|id| !returned.contains(*id) && !not_found.contains(*id))
    {
        return Err(Error::Other(
            "JMAP ContactCard/get returned an incomplete id set".into(),
        ));
    }
    if !not_found.is_empty() {
        return Ok(ContactGetResult::Changed);
    }
    seen_card_ids.extend(returned);
    seen_uids.extend(returned_uids);
    Ok(ContactGetResult::Complete { state, contacts })
}

fn parse_contact_card(value: &serde_json::Value) -> Result<JmapContact> {
    let card = value
        .as_object()
        .ok_or_else(|| Error::Other("Malformed JMAP ContactCard".into()))?;
    let id = card
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::Other("JMAP ContactCard omitted its id".into()))?;
    require_id(id, "contact")?;
    let uid = card
        .get("uid")
        .and_then(serde_json::Value::as_str)
        .filter(|uid| !uid.trim().is_empty())
        .ok_or_else(|| Error::Other("JMAP ContactCard has an invalid uid".into()))?;
    let address_book_ids = parse_address_book_ids(card)?;
    let display_name = parse_name(card.get("name"))?;
    let emails_json = parse_contact_points(card.get("emails"), ContactPointKind::Email)?;
    let phones_json = parse_contact_points(card.get("phones"), ContactPointKind::Phone)?;
    let organization = parse_organization(card.get("organizations"))?;
    let title = parse_first_named(card.get("titles"), "titles", "name")?;
    let notes = parse_first_named(card.get("notes"), "notes", "note")?;

    Ok(JmapContact {
        id: id.into(),
        uid: uid.into(),
        address_book_ids,
        display_name,
        emails_json,
        phones_json,
        organization,
        title,
        notes,
    })
}

fn parse_address_book_ids(
    card: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<String>> {
    let memberships = card
        .get("addressBookIds")
        .and_then(serde_json::Value::as_object)
        .filter(|memberships| !memberships.is_empty())
        .ok_or_else(|| {
            Error::Other("JMAP ContactCard has invalid address-book memberships".into())
        })?;
    let mut ids = Vec::with_capacity(memberships.len());
    for (id, value) in memberships {
        require_id(id, "address book")?;
        if value.as_bool() != Some(true) {
            return Err(Error::Other(
                "JMAP ContactCard has invalid address-book memberships".into(),
            ));
        }
        ids.push(id.clone());
    }
    ids.sort();
    Ok(ids)
}

fn parse_name(value: Option<&serde_json::Value>) -> Result<String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(String::new());
    };
    let name = value
        .as_object()
        .ok_or_else(|| Error::Other("JMAP ContactCard has a malformed name".into()))?;

    let full = optional_string(name, "full", "name")?;
    let given = optional_string(name, "given", "name")?;
    let surname = optional_string(name, "surname", "name")?;
    let components = match name.get("components") {
        None => None,
        Some(value) => {
            let values = value.as_array().ok_or_else(|| {
                Error::Other("JMAP ContactCard has malformed name components".into())
            })?;
            let mut parts = Vec::new();
            for value in values {
                let component = value.as_object().ok_or_else(|| {
                    Error::Other("JMAP ContactCard has a malformed name component".into())
                })?;
                let kind = component
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    .filter(|kind| !kind.is_empty())
                    .ok_or_else(|| {
                        Error::Other("JMAP ContactCard name component omitted its kind".into())
                    })?;
                let value = component
                    .get("value")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        Error::Other("JMAP ContactCard name component omitted its value".into())
                    })?;
                if kind != "separator" && !value.is_empty() {
                    parts.push(value);
                }
            }
            if parts.is_empty() {
                return Err(Error::Other(
                    "JMAP ContactCard name has no non-separator component".into(),
                ));
            }
            Some(parts.join(" "))
        }
    };
    full.map(str::to_string)
        .or(components)
        .or_else(|| {
            (given.is_some() || surname.is_some()).then(|| {
                [given, surname]
                    .into_iter()
                    .flatten()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        })
        .ok_or_else(|| Error::Other("JMAP ContactCard name omitted usable name fields".into()))
}

#[derive(Clone, Copy)]
enum ContactPointKind {
    Email,
    Phone,
}

impl ContactPointKind {
    fn remote_value_key(self) -> &'static str {
        match self {
            Self::Email => "address",
            Self::Phone => "number",
        }
    }

    fn local_value_key(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Phone => "number",
        }
    }

    fn property(self) -> &'static str {
        match self {
            Self::Email => "emails",
            Self::Phone => "phones",
        }
    }

    fn default_label(self) -> &'static str {
        match self {
            Self::Email => "work",
            Self::Phone => "mobile",
        }
    }
}

fn parse_contact_points(
    value: Option<&serde_json::Value>,
    kind: ContactPointKind,
) -> Result<String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok("[]".into());
    };
    let points = value.as_object().ok_or_else(|| {
        Error::Other(format!(
            "JMAP ContactCard has malformed {}",
            kind.property()
        ))
    })?;
    let mut ids: Vec<&String> = points.keys().collect();
    ids.sort();
    let mut local = Vec::with_capacity(ids.len());
    for id in ids {
        require_id(id, kind.property())?;
        let point = points[id].as_object().ok_or_else(|| {
            Error::Other(format!(
                "JMAP ContactCard has a malformed {} entry",
                kind.property()
            ))
        })?;
        let point_value = point
            .get(kind.remote_value_key())
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                Error::Other(format!(
                    "JMAP ContactCard {} entry omitted its {}",
                    kind.property(),
                    kind.remote_value_key()
                ))
            })?;
        let label = parse_point_label(point, kind.default_label())?;
        local.push(serde_json::json!({
            kind.local_value_key(): point_value,
            "label": label,
            "jmap_id": id
        }));
    }
    serde_json::to_string(&local)
        .map_err(|_| Error::Other("Failed to encode validated JMAP contact points".into()))
}

fn parse_point_label(
    point: &serde_json::Map<String, serde_json::Value>,
    default_label: &str,
) -> Result<String> {
    let label = optional_string(point, "label", "contact point")?;
    let contexts = match point.get("contexts") {
        None => Vec::new(),
        Some(value) => {
            let contexts = value.as_object().ok_or_else(|| {
                Error::Other("JMAP ContactCard has malformed contact-point contexts".into())
            })?;
            let mut names = Vec::with_capacity(contexts.len());
            for (name, enabled) in contexts {
                if name.is_empty() || enabled.as_bool() != Some(true) {
                    return Err(Error::Other(
                        "JMAP ContactCard has invalid contact-point contexts".into(),
                    ));
                }
                names.push(name.as_str());
            }
            names.sort();
            names
        }
    };
    Ok(label
        .map(str::to_string)
        .or_else(|| contexts.first().map(|context| (*context).to_string()))
        .unwrap_or_else(|| default_label.to_string()))
}

fn parse_organization(value: Option<&serde_json::Value>) -> Result<Option<String>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let organizations = value
        .as_object()
        .ok_or_else(|| Error::Other("JMAP ContactCard has malformed organizations".into()))?;
    let mut ids: Vec<&String> = organizations.keys().collect();
    ids.sort();
    let mut first_name = None;
    for id in ids {
        require_id(id, "organizations")?;
        let organization = organizations[id].as_object().ok_or_else(|| {
            Error::Other("JMAP ContactCard has a malformed organizations entry".into())
        })?;
        let name = optional_string(organization, "name", "organization")?;
        if first_name.is_none() {
            first_name = name.map(str::to_string);
        }
    }
    Ok(first_name)
}

fn parse_first_named(
    value: Option<&serde_json::Value>,
    property: &str,
    nested_property: &str,
) -> Result<Option<String>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let entries = value
        .as_object()
        .ok_or_else(|| Error::Other(format!("JMAP ContactCard has malformed {property}")))?;
    let mut ids: Vec<&String> = entries.keys().collect();
    ids.sort();
    let mut first = None;
    for id in ids {
        require_id(id, property)?;
        let entry = entries[id].as_object().ok_or_else(|| {
            Error::Other(format!("JMAP ContactCard has a malformed {property} entry"))
        })?;
        let text = entry
            .get(nested_property)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                Error::Other(format!(
                    "JMAP ContactCard {property} entry omitted {nested_property}"
                ))
            })?;
        if first.is_none() {
            first = Some(text.to_string());
        }
    }
    Ok(first)
}

fn build_create_card(
    address_book_id: &str,
    uid: &str,
    display_name: &str,
    emails_json: &str,
    phones_json: &str,
    organization: Option<&str>,
    title: Option<&str>,
    notes: Option<&str>,
) -> Result<serde_json::Value> {
    require_id(address_book_id, "address book")?;
    if uid.trim().is_empty() {
        return Err(Error::Other(
            "Cannot create a JMAP contact without a uid".into(),
        ));
    }
    let emails = parse_local_contact_points(emails_json, ContactPointKind::Email)?;
    let phones = parse_local_contact_points(phones_json, ContactPointKind::Phone)?;
    let mut memberships = serde_json::Map::new();
    memberships.insert(address_book_id.into(), serde_json::Value::Bool(true));
    let mut card = serde_json::Map::new();
    card.insert("@type".into(), serde_json::Value::String("Card".into()));
    card.insert("version".into(), serde_json::Value::String("1.0".into()));
    card.insert("uid".into(), serde_json::Value::String(uid.into()));
    card.insert(
        "addressBookIds".into(),
        serde_json::Value::Object(memberships),
    );
    if let Some(name) = build_name(display_name) {
        card.insert("name".into(), name);
    }
    insert_nonempty_map(&mut card, "emails", emails);
    insert_nonempty_map(&mut card, "phones", phones);
    insert_optional_named(&mut card, "organizations", "o0", "name", organization);
    insert_optional_named(&mut card, "titles", "t0", "name", title);
    insert_optional_named(&mut card, "notes", "n0", "note", notes);
    Ok(serde_json::Value::Object(card))
}

fn build_update_patch(
    display_name: &str,
    emails_json: &str,
    phones_json: &str,
    organization: Option<&str>,
    title: Option<&str>,
    notes: Option<&str>,
) -> Result<serde_json::Value> {
    let emails = parse_local_contact_points(emails_json, ContactPointKind::Email)?;
    let phones = parse_local_contact_points(phones_json, ContactPointKind::Phone)?;
    let mut patch = serde_json::Map::new();
    patch.insert(
        "name".into(),
        build_name(display_name).unwrap_or(serde_json::Value::Null),
    );
    patch.insert("emails".into(), map_or_null(emails));
    patch.insert("phones".into(), map_or_null(phones));
    patch.insert(
        "organizations".into(),
        named_map_or_null("o0", "name", organization),
    );
    patch.insert("titles".into(), named_map_or_null("t0", "name", title));
    patch.insert("notes".into(), named_map_or_null("n0", "note", notes));
    Ok(serde_json::Value::Object(patch))
}

#[derive(Debug)]
struct LocalContactPoint {
    value: String,
    label: Option<String>,
    jmap_id: Option<String>,
}

fn parse_local_contact_points(
    json: &str,
    kind: ContactPointKind,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|_| Error::Other(format!("Invalid local {} JSON", kind.property())))?;
    let values = value
        .as_array()
        .ok_or_else(|| Error::Other(format!("Local {} JSON must be an array", kind.property())))?;
    let mut points = Vec::with_capacity(values.len());
    let mut reserved_ids = HashSet::new();

    for value in values {
        let object = value.as_object().ok_or_else(|| {
            Error::Other(format!("Local {} entry must be an object", kind.property()))
        })?;
        let point_value = object
            .get(kind.local_value_key())
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                Error::Other(format!(
                    "Local {} entry has an invalid {}",
                    kind.property(),
                    kind.local_value_key()
                ))
            })?;
        let label = match object.get("label") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| {
                        Error::Other(format!(
                            "Local {} entry has an invalid label",
                            kind.property()
                        ))
                    })?
                    .to_string(),
            ),
        };
        let jmap_id = match object.get("jmap_id") {
            None => None,
            Some(value) => {
                let id = value.as_str().ok_or_else(|| {
                    Error::Other(format!(
                        "Local {} entry has an invalid jmap_id",
                        kind.property()
                    ))
                })?;
                require_id(id, "contact-point map")?;
                reserved_ids.insert(id.to_string());
                Some(id.to_string())
            }
        };
        points.push(LocalContactPoint {
            value: point_value.to_string(),
            label,
            jmap_id,
        });
    }

    let prefix = match kind {
        ContactPointKind::Email => 'e',
        ContactPointKind::Phone => 'p',
    };
    let mut next_id = 0usize;
    let mut claimed_explicit_ids = HashSet::new();
    let mut allocated_ids = reserved_ids;
    let mut result = serde_json::Map::new();
    for point in points {
        let id = if let Some(id) = point
            .jmap_id
            .filter(|id| claimed_explicit_ids.insert(id.clone()))
        {
            id
        } else {
            loop {
                let candidate = format!("{prefix}{next_id}");
                next_id += 1;
                if allocated_ids.insert(candidate.clone()) {
                    break candidate;
                }
            }
        };
        let mut remote = serde_json::Map::new();
        remote.insert(
            kind.remote_value_key().into(),
            serde_json::Value::String(point.value),
        );
        if let Some(label) = point.label {
            remote.insert("label".into(), serde_json::Value::String(label));
        }
        result.insert(id, serde_json::Value::Object(remote));
    }
    Ok(result)
}

fn build_name(display_name: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = display_name.split_whitespace().collect();
    let first = parts.first()?;
    let mut components = Vec::new();
    components.push(serde_json::json!({ "kind": "given", "value": first }));
    if parts.len() > 2 {
        components.push(serde_json::json!({
            "kind": "given2",
            "value": parts[1..parts.len() - 1].join(" ")
        }));
    }
    if parts.len() >= 2 {
        components.push(serde_json::json!({
            "kind": "surname",
            "value": parts[parts.len() - 1]
        }));
    }
    Some(serde_json::json!({
        "components": components,
        "isOrdered": true
    }))
}

fn insert_nonempty_map(
    object: &mut serde_json::Map<String, serde_json::Value>,
    property: &str,
    map: serde_json::Map<String, serde_json::Value>,
) {
    if !map.is_empty() {
        object.insert(property.into(), serde_json::Value::Object(map));
    }
}

fn insert_optional_named(
    object: &mut serde_json::Map<String, serde_json::Value>,
    property: &str,
    id: &str,
    nested_property: &str,
    value: Option<&str>,
) {
    let value = named_map_or_null(id, nested_property, value);
    if !value.is_null() {
        object.insert(property.into(), value);
    }
}

fn named_map_or_null(id: &str, nested_property: &str, value: Option<&str>) -> serde_json::Value {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return serde_json::Value::Null;
    };
    let mut nested = serde_json::Map::new();
    nested.insert(
        nested_property.into(),
        serde_json::Value::String(value.into()),
    );
    let mut map = serde_json::Map::new();
    map.insert(id.into(), serde_json::Value::Object(nested));
    serde_json::Value::Object(map)
}

fn map_or_null(map: serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    if map.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::Object(map)
    }
}

fn validate_create_response(response: &serde_json::Value, account_id: &str) -> Result<String> {
    let body = contact_set_body(response, CREATE_CALL_ID, account_id)?;
    let outcomes = validate_set_outcomes(body)?;
    outcomes.require_irrelevant_empty(SetOperation::Create)?;
    if outcomes.created.len() + outcomes.not_created.len() != 1
        || outcomes
            .created
            .keys()
            .chain(outcomes.not_created.keys())
            .any(|id| id != CREATE_ID)
    {
        return Err(Error::Other(
            "JMAP ContactCard/set create returned an invalid outcome".into(),
        ));
    }
    if let Some(error) = outcomes.not_created.get(CREATE_ID) {
        return Err(Error::Other(format!(
            "JMAP ContactCard/set create failed (type={})",
            super::safe_jmap_error_type(error)
        )));
    }
    let id = outcomes.created[CREATE_ID]
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::Other("JMAP ContactCard/set create omitted the created id".into()))?;
    require_id(id, "created contact")?;
    Ok(id.into())
}

fn validate_update_response(
    response: &serde_json::Value,
    account_id: &str,
    remote_id: &str,
) -> Result<()> {
    let body = contact_set_body(response, UPDATE_CALL_ID, account_id)?;
    let outcomes = validate_set_outcomes(body)?;
    outcomes.require_irrelevant_empty(SetOperation::Update)?;
    if outcomes.updated.len() + outcomes.not_updated.len() != 1
        || outcomes
            .updated
            .keys()
            .chain(outcomes.not_updated.keys())
            .any(|id| id != remote_id)
    {
        return Err(Error::Other(
            "JMAP ContactCard/set update returned an invalid outcome".into(),
        ));
    }
    if let Some(error) = outcomes.not_updated.get(remote_id) {
        return Err(Error::Other(format!(
            "JMAP ContactCard/set update failed (type={})",
            super::safe_jmap_error_type(error)
        )));
    }
    if !outcomes.updated.contains_key(remote_id) {
        return Err(Error::Other(
            "JMAP ContactCard/set update omitted the requested id".into(),
        ));
    }
    Ok(())
}

fn validate_delete_response(
    response: &serde_json::Value,
    account_id: &str,
    remote_id: &str,
) -> Result<()> {
    let body = contact_set_body(response, DELETE_CALL_ID, account_id)?;
    let outcomes = validate_set_outcomes(body)?;
    outcomes.require_irrelevant_empty(SetOperation::Delete)?;
    if outcomes.destroyed.len() + outcomes.not_destroyed.len() != 1
        || outcomes.destroyed.iter().any(|id| *id != remote_id)
        || outcomes.not_destroyed.keys().any(|id| id != remote_id)
    {
        return Err(Error::Other(
            "JMAP ContactCard/set destroy returned an invalid outcome".into(),
        ));
    }
    if outcomes
        .destroyed
        .first()
        .is_some_and(|id| *id == remote_id)
    {
        return Ok(());
    }
    let error = outcomes.not_destroyed.get(remote_id).ok_or_else(|| {
        Error::Other("JMAP ContactCard/set destroy omitted the requested id".into())
    })?;
    let error_type = super::bounded_jmap_error_type(error).unwrap_or("unknown");
    if error_type == "notFound" {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "JMAP ContactCard/set destroy failed (type={error_type})"
        )))
    }
}

fn contact_set_body<'a>(
    response: &'a serde_json::Value,
    call_id: &str,
    account_id: &str,
) -> Result<&'a serde_json::Map<String, serde_json::Value>> {
    let body = single_method_body(response, "ContactCard/set", call_id, account_id)?;
    optional_string(body, "oldState", "ContactCard/set")?;
    require_string(body, "newState", "ContactCard/set")?;
    Ok(body)
}

struct SetOutcomes<'a> {
    created: &'a serde_json::Map<String, serde_json::Value>,
    updated: &'a serde_json::Map<String, serde_json::Value>,
    destroyed: Vec<&'a str>,
    not_created: &'a serde_json::Map<String, serde_json::Value>,
    not_updated: &'a serde_json::Map<String, serde_json::Value>,
    not_destroyed: &'a serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Copy)]
enum SetOperation {
    Create,
    Update,
    Delete,
}

impl SetOutcomes<'_> {
    fn require_irrelevant_empty(&self, operation: SetOperation) -> Result<()> {
        let invalid = match operation {
            SetOperation::Create => {
                !self.updated.is_empty()
                    || !self.destroyed.is_empty()
                    || !self.not_updated.is_empty()
                    || !self.not_destroyed.is_empty()
            }
            SetOperation::Update => {
                !self.created.is_empty()
                    || !self.destroyed.is_empty()
                    || !self.not_created.is_empty()
                    || !self.not_destroyed.is_empty()
            }
            SetOperation::Delete => {
                !self.created.is_empty()
                    || !self.updated.is_empty()
                    || !self.not_created.is_empty()
                    || !self.not_updated.is_empty()
            }
        };
        if invalid {
            Err(Error::Other(
                "JMAP ContactCard/set returned an extraneous outcome".into(),
            ))
        } else {
            Ok(())
        }
    }
}

fn validate_set_outcomes(
    body: &serde_json::Map<String, serde_json::Value>,
) -> Result<SetOutcomes<'_>> {
    let created = optional_map(body, "created", "ContactCard/set")?;
    let updated = optional_map(body, "updated", "ContactCard/set")?;
    let destroyed_values = optional_array(body, "destroyed", "ContactCard/set")?;
    let not_created = optional_map(body, "notCreated", "ContactCard/set")?;
    let not_updated = optional_map(body, "notUpdated", "ContactCard/set")?;
    let not_destroyed = optional_map(body, "notDestroyed", "ContactCard/set")?;

    validate_result_map(created, CreatedValueKind::Created)?;
    validate_result_map(updated, CreatedValueKind::Updated)?;
    validate_error_map(not_created)?;
    validate_error_map(not_updated)?;
    validate_error_map(not_destroyed)?;
    let mut destroyed_seen = HashSet::new();
    let mut destroyed = Vec::with_capacity(destroyed_values.len());
    for value in destroyed_values {
        let id = value
            .as_str()
            .ok_or_else(|| Error::Other("Malformed JMAP ContactCard/set destroyed id".into()))?;
        require_id(id, "destroyed contact")?;
        if !destroyed_seen.insert(id) {
            return Err(Error::Other(
                "JMAP ContactCard/set returned duplicate destroyed ids".into(),
            ));
        }
        destroyed.push(id);
    }
    Ok(SetOutcomes {
        created,
        updated,
        destroyed,
        not_created,
        not_updated,
        not_destroyed,
    })
}

#[derive(Clone, Copy)]
enum CreatedValueKind {
    Created,
    Updated,
}

fn validate_result_map(
    map: &serde_json::Map<String, serde_json::Value>,
    kind: CreatedValueKind,
) -> Result<()> {
    for (id, value) in map {
        require_id(id, "set outcome")?;
        let valid = match kind {
            CreatedValueKind::Created => value.is_object(),
            CreatedValueKind::Updated => value.is_object() || value.is_null(),
        };
        if !valid {
            return Err(Error::Other(
                "Malformed JMAP ContactCard/set result map".into(),
            ));
        }
    }
    Ok(())
}

fn validate_error_map(map: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
    for (id, value) in map {
        require_id(id, "set error outcome")?;
        if !value.is_object() || super::bounded_jmap_error_type(value).is_none() {
            return Err(Error::Other(
                "Malformed JMAP ContactCard/set error map".into(),
            ));
        }
    }
    Ok(())
}

fn require_empty_not_found(
    body: &serde_json::Map<String, serde_json::Value>,
    method: &str,
) -> Result<()> {
    let not_found = body
        .get("notFound")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::Other(format!("Malformed JMAP {method} notFound")))?;
    if !not_found.is_empty() {
        return Err(Error::Other(format!(
            "JMAP {method} returned nonempty notFound"
        )));
    }
    Ok(())
}

fn require_string<'a>(
    body: &'a serde_json::Map<String, serde_json::Value>,
    property: &str,
    method: &str,
) -> Result<&'a str> {
    body.get(property)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::Other(format!("Malformed JMAP {method} {property}")))
}

fn optional_string<'a>(
    body: &'a serde_json::Map<String, serde_json::Value>,
    property: &str,
    context: &str,
) -> Result<Option<&'a str>> {
    match body.get(property) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| Error::Other(format!("Malformed JMAP {context} {property}"))),
    }
}

fn require_unsigned(
    body: &serde_json::Map<String, serde_json::Value>,
    property: &str,
    method: &str,
) -> Result<usize> {
    let value = body
        .get(property)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| Error::Other(format!("Malformed JMAP {method} {property}")))?;
    usize::try_from(value)
        .map_err(|_| Error::Other(format!("JMAP {method} {property} is too large")))
}

fn optional_map<'a>(
    body: &'a serde_json::Map<String, serde_json::Value>,
    property: &str,
    method: &str,
) -> Result<&'a serde_json::Map<String, serde_json::Value>> {
    static EMPTY: std::sync::OnceLock<serde_json::Map<String, serde_json::Value>> =
        std::sync::OnceLock::new();
    match body.get(property) {
        None | Some(serde_json::Value::Null) => Ok(EMPTY.get_or_init(serde_json::Map::new)),
        Some(serde_json::Value::Object(map)) => Ok(map),
        Some(_) => Err(Error::Other(format!("Malformed JMAP {method} {property}"))),
    }
}

fn optional_array<'a>(
    body: &'a serde_json::Map<String, serde_json::Value>,
    property: &str,
    method: &str,
) -> Result<&'a [serde_json::Value]> {
    match body.get(property) {
        None | Some(serde_json::Value::Null) => Ok(&[]),
        Some(serde_json::Value::Array(values)) => Ok(values),
        Some(_) => Err(Error::Other(format!("Malformed JMAP {method} {property}"))),
    }
}

fn require_id(value: &str, context: &str) -> Result<()> {
    if is_valid_id(value) {
        Ok(())
    } else {
        Err(Error::Other(format!("Invalid JMAP {context} id")))
    }
}

fn is_valid_id(value: &str) -> bool {
    (1..=255).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JmapAddressBook {
    pub(crate) id: String,
    pub(crate) name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JmapContact {
    pub(crate) id: String,
    pub(crate) uid: String,
    pub(crate) address_book_ids: Vec<String>,
    pub(crate) display_name: String,
    pub(crate) emails_json: String,
    pub(crate) phones_json: String,
    pub(crate) organization: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) notes: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    const ACCOUNT_ID: &str = "contacts-account";

    fn address_book_response(list: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "sessionState": "session-state",
            "methodResponses": [["AddressBook/get", {
                "accountId": ACCOUNT_ID,
                "state": "books-state",
                "list": list,
                "notFound": []
            }, ADDRESS_BOOK_CALL_ID]]
        })
    }

    fn card(id: &str, uid: &str, memberships: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "uid": uid,
            "addressBookIds": memberships
        })
    }

    fn query_response(
        call_id: &str,
        query_state: &str,
        position: usize,
        total: usize,
        ids: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "sessionState": "session-state",
            "methodResponses": [["ContactCard/query", {
                "accountId": ACCOUNT_ID,
                "queryState": query_state,
                "canCalculateChanges": false,
                "position": position,
                "total": total,
                "ids": ids
            }, call_id]]
        })
    }

    fn get_response(call_id: &str, state: &str, list: serde_json::Value) -> serde_json::Value {
        get_response_with_not_found(call_id, state, list, serde_json::json!([]))
    }

    fn get_response_with_not_found(
        call_id: &str,
        state: &str,
        list: serde_json::Value,
        not_found: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "sessionState": "session-state",
            "methodResponses": [["ContactCard/get", {
                "accountId": ACCOUNT_ID,
                "state": state,
                "list": list,
                "notFound": not_found
            }, call_id]]
        })
    }

    fn set_response(call_id: &str, outcomes: serde_json::Value) -> serde_json::Value {
        let mut body = outcomes.as_object().unwrap().clone();
        body.insert("accountId".into(), serde_json::json!(ACCOUNT_ID));
        body.insert("oldState".into(), serde_json::json!("old"));
        body.insert("newState".into(), serde_json::json!("new"));
        serde_json::json!({
            "sessionState": "session-state",
            "methodResponses": [["ContactCard/set", body, call_id]]
        })
    }

    fn set_old_state(response: &mut serde_json::Value, value: Option<serde_json::Value>) {
        let body = response["methodResponses"][0][1].as_object_mut().unwrap();
        if let Some(value) = value {
            body.insert("oldState".into(), value);
        } else {
            body.remove("oldState");
        }
    }

    #[test]
    fn address_book_get_accepts_a_complete_empty_collection() {
        assert_eq!(
            parse_address_books_response(&address_book_response(serde_json::json!([])), ACCOUNT_ID)
                .unwrap(),
            Vec::<JmapAddressBook>::new()
        );
    }

    #[test]
    fn address_book_get_is_strictly_correlated_and_redacts_method_errors() {
        let secret = "private server description";
        let method_error = serde_json::json!({
            "sessionState": "session-state",
            "methodResponses": [["error", {
                "type": "serverFail",
                "description": secret
            }, ADDRESS_BOOK_CALL_ID]]
        });
        let error = parse_address_books_response(&method_error, ACCOUNT_ID)
            .unwrap_err()
            .to_string();
        assert!(error.contains("serverFail"));
        assert!(!error.contains(secret));

        for invalid in [
            serde_json::json!({ "methodResponses": [] }),
            serde_json::json!({
                "sessionState": "session-state",
                "methodResponses": [["AddressBook/get", {
                    "accountId": ACCOUNT_ID,
                    "state": "s",
                    "list": [],
                    "notFound": []
                }, "wrong"]]
            }),
            serde_json::json!({
                "sessionState": "session-state",
                "methodResponses": [["AddressBook/get", {
                    "accountId": "mail-account",
                    "state": "s",
                    "list": [],
                    "notFound": []
                }, ADDRESS_BOOK_CALL_ID]]
            }),
            serde_json::json!({
                "sessionState": "session-state",
                "methodResponses": [
                    ["AddressBook/get", {}, ADDRESS_BOOK_CALL_ID],
                    ["AddressBook/get", {}, "extra"]
                ]
            }),
        ] {
            assert!(parse_address_books_response(&invalid, ACCOUNT_ID).is_err());
        }

        for invalid_session_state in [
            serde_json::json!({
                "methodResponses": [["AddressBook/get", {
                    "accountId": ACCOUNT_ID,
                    "state": "s",
                    "list": [],
                    "notFound": []
                }, ADDRESS_BOOK_CALL_ID]]
            }),
            serde_json::json!({
                "sessionState": null,
                "methodResponses": [["AddressBook/get", {
                    "accountId": ACCOUNT_ID,
                    "state": "s",
                    "list": [],
                    "notFound": []
                }, ADDRESS_BOOK_CALL_ID]]
            }),
        ] {
            assert!(parse_address_books_response(&invalid_session_state, ACCOUNT_ID).is_err());
        }
    }

    #[test]
    fn address_book_get_rejects_partial_or_invalid_objects() {
        for list in [
            serde_json::json!([{"id": "book"}]),
            serde_json::json!([{"id": "book", "name": ""}]),
            serde_json::json!([{"id": "bad/id", "name": "Bad"}]),
            serde_json::json!([
                {"id": "book", "name": "One"},
                {"id": "book", "name": "Two"}
            ]),
            serde_json::json!([null]),
        ] {
            assert!(
                parse_address_books_response(&address_book_response(list), ACCOUNT_ID).is_err()
            );
        }
        let mut nonempty_not_found = address_book_response(serde_json::json!([]));
        nonempty_not_found["methodResponses"][0][1]["notFound"] = serde_json::json!(["missing"]);
        assert!(parse_address_books_response(&nonempty_not_found, ACCOUNT_ID).is_err());
    }

    #[test]
    fn contact_card_parser_preserves_all_memberships_and_stable_map_ids() {
        let parsed = parse_contact_card(&serde_json::json!({
            "id": "card-1",
            "uid": "uid-1",
            "addressBookIds": { "book-b": true, "book-a": true },
            "name": { "components": [
                { "kind": "given", "value": "Ada" },
                { "kind": "given2", "value": "Byron" },
                { "kind": "surname", "value": "Lovelace" }
            ] },
            "emails": {
                "z": { "address": "z@example.test", "label": "other" },
                "a": {
                    "address": "a@example.test",
                    "contexts": { "work": true, "private": true }
                }
            },
            "phones": {
                "p1": { "number": "+46", "contexts": { "private": true } }
            },
            "organizations": { "z": { "name": "Second" }, "a": { "name": "First" } },
            "titles": { "t": { "name": "Programmer" } },
            "notes": { "n": { "note": "A note" } }
        }))
        .unwrap();

        assert_eq!(parsed.address_book_ids, ["book-a", "book-b"]);
        assert_eq!(parsed.display_name, "Ada Byron Lovelace");
        assert_eq!(parsed.organization.as_deref(), Some("First"));
        assert_eq!(parsed.title.as_deref(), Some("Programmer"));
        assert_eq!(parsed.notes.as_deref(), Some("A note"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&parsed.emails_json).unwrap(),
            serde_json::json!([
                {"email": "a@example.test", "label": "private", "jmap_id": "a"},
                {"email": "z@example.test", "label": "other", "jmap_id": "z"}
            ])
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&parsed.phones_json).unwrap(),
            serde_json::json!([
                {"number": "+46", "label": "private", "jmap_id": "p1"}
            ])
        );
    }

    #[test]
    fn contact_card_parser_rejects_invalid_identity_membership_and_consumed_fields() {
        for invalid in [
            card("bad/id", "uid", serde_json::json!({"book": true})),
            card("card", "   ", serde_json::json!({"book": true})),
            card("card", "uid", serde_json::json!({})),
            card("card", "uid", serde_json::json!({"bad/id": true})),
            card("card", "uid", serde_json::json!({"book": false})),
            serde_json::json!({
                "id": "card", "uid": "uid", "addressBookIds": {"book": true},
                "emails": {"e": {}}
            }),
            serde_json::json!({
                "id": "card", "uid": "uid", "addressBookIds": {"book": true},
                "phones": {"p": {"number": 7}}
            }),
            serde_json::json!({
                "id": "card", "uid": "uid", "addressBookIds": {"book": true},
                "notes": {"n": {}}
            }),
            serde_json::json!({
                "id": "card", "uid": "uid", "addressBookIds": {"book": true},
                "name": {"components": [{"kind": "given"}]}
            }),
            serde_json::json!({
                "id": "card", "uid": "uid", "addressBookIds": {"book": true},
                "name": {}
            }),
            serde_json::json!({
                "id": "card", "uid": "uid", "addressBookIds": {"book": true},
                "emails": {"e": {
                    "address": "a@example.test", "contexts": {"work": false}
                }}
            }),
        ] {
            assert!(parse_contact_card(&invalid).is_err());
        }
    }

    #[test]
    fn contact_card_parser_accepts_null_optional_strings_and_legacy_direct_names() {
        let parsed = parse_contact_card(&serde_json::json!({
            "id": "card",
            "uid": "uid",
            "addressBookIds": {"book": true},
            "name": {
                "full": null,
                "given": "Ada",
                "surname": "Lovelace"
            },
            "emails": {
                "empty-label": {
                    "address": "ada@example.test",
                    "label": ""
                },
                "null-label": {
                    "address": "other@example.test",
                    "label": null
                }
            },
            "organizations": {
                "units-only": {"units": [{"name": "Research"}]}
            }
        }))
        .unwrap();

        assert_eq!(parsed.display_name, "Ada Lovelace");
        assert!(parsed.organization.is_none());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&parsed.emails_json).unwrap(),
            serde_json::json!([
                {
                    "email": "ada@example.test",
                    "label": "",
                    "jmap_id": "empty-label"
                },
                {
                    "email": "other@example.test",
                    "label": "work",
                    "jmap_id": "null-label"
                }
            ])
        );

        for invalid in [
            serde_json::json!({
                "id": "card", "uid": "uid", "addressBookIds": {"book": true},
                "name": {"given": 7, "surname": "Lovelace"}
            }),
            serde_json::json!({
                "id": "card", "uid": "uid", "addressBookIds": {"book": true},
                "organizations": {"o": {"name": 7}}
            }),
        ] {
            assert!(parse_contact_card(&invalid).is_err());
        }
    }

    #[test]
    fn contact_get_requires_exact_ids_and_unique_account_wide_uids() {
        let mut seen_ids = HashSet::new();
        let mut seen_uids = HashSet::new();
        let requested = vec!["one".to_string(), "two".to_string()];
        let valid = get_response(
            "g1",
            "state",
            serde_json::json!([
                card("two", "uid-two", serde_json::json!({"book": true})),
                card("one", "uid-one", serde_json::json!({"book": true}))
            ]),
        );
        let result = parse_contact_get_response(
            &valid,
            "g1",
            ACCOUNT_ID,
            &requested,
            &mut seen_ids,
            &mut seen_uids,
        )
        .unwrap();
        let ContactGetResult::Complete { contacts, .. } = result else {
            panic!("complete get was classified as changed");
        };
        assert_eq!(contacts.len(), 2);

        let duplicate_uid = get_response(
            "g2",
            "state",
            serde_json::json!([card("three", "uid-one", serde_json::json!({"book": true}))]),
        );
        assert!(parse_contact_get_response(
            &duplicate_uid,
            "g2",
            ACCOUNT_ID,
            &["three".into()],
            &mut seen_ids,
            &mut seen_uids
        )
        .is_err());

        for list in [
            serde_json::json!([card("one", "uid", serde_json::json!({"book": true}))]),
            serde_json::json!([
                card("one", "uid-1", serde_json::json!({"book": true})),
                card("extra", "uid-2", serde_json::json!({"book": true}))
            ]),
        ] {
            let mut ids = HashSet::new();
            let mut uids = HashSet::new();
            assert!(parse_contact_get_response(
                &get_response("g1", "state", list),
                "g1",
                ACCOUNT_ID,
                &requested,
                &mut ids,
                &mut uids
            )
            .is_err());
        }

        let mut nonempty_not_found = get_response("g1", "state", serde_json::json!([]));
        nonempty_not_found["methodResponses"][0][1]["notFound"] = serde_json::json!(["one"]);
        let mut ids = HashSet::new();
        let mut uids = HashSet::new();
        assert!(matches!(
            parse_contact_get_response(
                &nonempty_not_found,
                "g1",
                ACCOUNT_ID,
                &["one".into()],
                &mut ids,
                &mut uids
            )
            .unwrap(),
            ContactGetResult::Changed
        ));

        for not_found in [
            serde_json::json!(["other"]),
            serde_json::json!(["one", "one"]),
            serde_json::json!(["bad/id"]),
        ] {
            let mut response = get_response("g1", "state", serde_json::json!([]));
            response["methodResponses"][0][1]["notFound"] = not_found;
            let mut ids = HashSet::new();
            let mut uids = HashSet::new();
            assert!(parse_contact_get_response(
                &response,
                "g1",
                ACCOUNT_ID,
                &["one".into()],
                &mut ids,
                &mut uids
            )
            .is_err());
        }

        let mut overlap = get_response(
            "g1",
            "state",
            serde_json::json!([card("one", "uid-one", serde_json::json!({"book": true}))]),
        );
        overlap["methodResponses"][0][1]["notFound"] = serde_json::json!(["one"]);
        let mut ids = HashSet::new();
        let mut uids = HashSet::new();
        assert!(parse_contact_get_response(
            &overlap,
            "g1",
            ACCOUNT_ID,
            &["one".into()],
            &mut ids,
            &mut uids
        )
        .is_err());
    }

    #[test]
    fn create_builder_sends_uid_dynamic_book_key_labels_and_stable_ids() {
        let card = build_create_card(
            "book-7",
            "uid-7@chithi",
            "Ada Lovelace",
            r#"[{"email":"new@example.test","label":"work"},{"email":"old@example.test","label":"private","jmap_id":"e0"}]"#,
            r#"[{"number":"+46","label":"mobile","jmap_id":"phone-key"}]"#,
            Some("Engines"),
            Some("Programmer"),
            Some("Notes"),
        )
        .unwrap();

        assert_eq!(card["@type"], "Card");
        assert_eq!(card["version"], "1.0");
        assert_eq!(card["uid"], "uid-7@chithi");
        assert_eq!(card["addressBookIds"], serde_json::json!({"book-7": true}));
        assert_eq!(card["emails"]["e0"]["address"], "old@example.test");
        assert_eq!(card["emails"]["e1"]["address"], "new@example.test");
        assert_eq!(card["emails"]["e1"]["label"], "work");
        assert_eq!(card["phones"]["phone-key"]["label"], "mobile");
    }

    #[test]
    fn update_builder_explicitly_clears_removed_owned_fields() {
        let patch = build_update_patch("", "[]", "[]", None, Some("  "), None).unwrap();
        assert!(patch["name"].is_null());
        assert!(patch["emails"].is_null());
        assert!(patch["phones"].is_null());
        assert!(patch["organizations"].is_null());
        assert!(patch["titles"].is_null());
        assert!(patch["notes"].is_null());
    }

    #[test]
    fn local_contact_point_builders_reject_malformed_json() {
        for invalid in [
            "not-json",
            "{}",
            "[null]",
            "[{}]",
            r#"[{"email":" "}]"#,
            r#"[{"email":7}]"#,
            r#"[{"email":"a@example.test","jmap_id":"bad/id"}]"#,
            r#"[{"email":"a@example.test","jmap_id":null}]"#,
            r#"[{"email":"a@example.test","label":7}]"#,
            r#"[{"number":"+46"}]"#,
        ] {
            assert!(parse_local_contact_points(invalid, ContactPointKind::Email).is_err());
        }
        assert!(build_create_card("book", " ", "Name", "[]", "[]", None, None, None).is_err());
        let points = parse_local_contact_points(
            r#"[{"email":"a@example.test","label":""},{"email":"b@example.test","label":null}]"#,
            ContactPointKind::Email,
        )
        .unwrap();
        assert_eq!(points["e0"]["label"], "");
        assert!(points["e1"].get("label").is_none());
    }

    #[test]
    fn local_contact_point_builders_remap_duplicate_ids_without_collisions() {
        let emails = parse_local_contact_points(
            r#"[{"email":"a@example.test","jmap_id":"e0"},{"email":"b@example.test","jmap_id":"e0"}]"#,
            ContactPointKind::Email,
        )
        .unwrap();
        assert_eq!(emails.len(), 2);
        assert_eq!(emails["e0"]["address"], "a@example.test");
        assert_eq!(emails["e1"]["address"], "b@example.test");

        let phones = parse_local_contact_points(
            r#"[{"number":"+1"},{"number":"+2","jmap_id":"p0"},{"number":"+3","jmap_id":"p0"}]"#,
            ContactPointKind::Phone,
        )
        .unwrap();
        assert_eq!(phones.len(), 3);
        assert_eq!(phones["p0"]["number"], "+2");
        assert_eq!(phones["p1"]["number"], "+1");
        assert_eq!(phones["p2"]["number"], "+3");
    }

    #[test]
    fn local_contact_point_builders_ignore_provider_metadata() {
        let emails = parse_local_contact_points(
            r#"[{"email":"ada@example.test","name":"Ada","label":"work","provider":{"id":7}}]"#,
            ContactPointKind::Email,
        )
        .unwrap();
        assert_eq!(
            emails,
            serde_json::from_value(serde_json::json!({
                "e0": {"address": "ada@example.test", "label": "work"}
            }))
            .unwrap()
        );
    }

    #[test]
    fn query_response_requires_total_and_can_calculate_changes() {
        assert!(parse_query_response(
            &query_response("q1", "state", 0, 0, serde_json::json!([])),
            "q1",
            ACCOUNT_ID
        )
        .is_ok());

        for property in ["total", "canCalculateChanges"] {
            let mut response = query_response("q1", "state", 0, 0, serde_json::json!([]));
            response["methodResponses"][0][1]
                .as_object_mut()
                .unwrap()
                .remove(property);
            assert!(parse_query_response(&response, "q1", ACCOUNT_ID).is_err());
        }
        let mut wrong_type = query_response("q1", "state", 0, 0, serde_json::json!([]));
        wrong_type["methodResponses"][0][1]["canCalculateChanges"] = serde_json::json!("false");
        assert!(parse_query_response(&wrong_type, "q1", ACCOUNT_ID).is_err());
    }

    #[test]
    fn set_responses_require_one_positive_correlated_outcome() {
        let created = set_response(
            CREATE_CALL_ID,
            serde_json::json!({"created": {"new1": {"id": "card-1"}}}),
        );
        assert_eq!(
            validate_create_response(&created, ACCOUNT_ID).unwrap(),
            "card-1"
        );
        assert!(validate_update_response(
            &set_response(
                UPDATE_CALL_ID,
                serde_json::json!({"updated": {"card-1": null}})
            ),
            ACCOUNT_ID,
            "card-1"
        )
        .is_ok());
        assert!(validate_delete_response(
            &set_response(DELETE_CALL_ID, serde_json::json!({"destroyed": ["card-1"]})),
            ACCOUNT_ID,
            "card-1"
        )
        .is_ok());
        assert!(validate_delete_response(
            &set_response(
                DELETE_CALL_ID,
                serde_json::json!({"notDestroyed": {"card-1": {"type": "notFound"}}})
            ),
            ACCOUNT_ID,
            "card-1"
        )
        .is_ok());

        for invalid in [
            set_response(CREATE_CALL_ID, serde_json::json!({})),
            set_response(
                CREATE_CALL_ID,
                serde_json::json!({
                    "created": {"new1": {"id": "card-1"}},
                    "notCreated": {"new1": {"type": "invalidProperties"}}
                }),
            ),
            set_response(
                CREATE_CALL_ID,
                serde_json::json!({"created": {"other": {"id": "card-1"}}}),
            ),
            set_response(
                CREATE_CALL_ID,
                serde_json::json!({"created": {"new1": {"id": "bad/id"}}}),
            ),
            set_response(
                CREATE_CALL_ID,
                serde_json::json!({
                    "created": {"new1": {"id": "card-1"}},
                    "updated": {"card-1": null}
                }),
            ),
        ] {
            assert!(validate_create_response(&invalid, ACCOUNT_ID).is_err());
        }

        assert!(validate_update_response(
            &set_response(UPDATE_CALL_ID, serde_json::json!({})),
            ACCOUNT_ID,
            "card-1"
        )
        .is_err());
        assert!(validate_update_response(
            &set_response(
                UPDATE_CALL_ID,
                serde_json::json!({
                    "updated": {"card-1": null},
                    "notUpdated": {"card-1": {"type": "serverFail"}}
                })
            ),
            ACCOUNT_ID,
            "card-1"
        )
        .is_err());
        assert!(validate_delete_response(
            &set_response(
                DELETE_CALL_ID,
                serde_json::json!({"notDestroyed": {"card-1": {"type": "forbidden"}}})
            ),
            ACCOUNT_ID,
            "card-1"
        )
        .is_err());
        assert!(validate_delete_response(
            &set_response(DELETE_CALL_ID, serde_json::json!({"destroyed": ["other"]})),
            ACCOUNT_ID,
            "card-1"
        )
        .is_err());
    }

    #[test]
    fn set_responses_allow_missing_or_null_old_state_but_reject_wrong_types() {
        for old_state in [None, Some(serde_json::Value::Null)] {
            let mut create = set_response(
                CREATE_CALL_ID,
                serde_json::json!({"created": {"new1": {"id": "card-1"}}}),
            );
            set_old_state(&mut create, old_state.clone());
            assert!(validate_create_response(&create, ACCOUNT_ID).is_ok());

            let mut update = set_response(
                UPDATE_CALL_ID,
                serde_json::json!({"updated": {"card-1": null}}),
            );
            set_old_state(&mut update, old_state.clone());
            assert!(validate_update_response(&update, ACCOUNT_ID, "card-1").is_ok());

            let mut delete =
                set_response(DELETE_CALL_ID, serde_json::json!({"destroyed": ["card-1"]}));
            set_old_state(&mut delete, old_state);
            assert!(validate_delete_response(&delete, ACCOUNT_ID, "card-1").is_ok());
        }

        for (call_id, outcomes, operation) in [
            (
                CREATE_CALL_ID,
                serde_json::json!({"created": {"new1": {"id": "card-1"}}}),
                SetOperation::Create,
            ),
            (
                UPDATE_CALL_ID,
                serde_json::json!({"updated": {"card-1": null}}),
                SetOperation::Update,
            ),
            (
                DELETE_CALL_ID,
                serde_json::json!({"destroyed": ["card-1"]}),
                SetOperation::Delete,
            ),
        ] {
            let mut response = set_response(call_id, outcomes);
            set_old_state(&mut response, Some(serde_json::json!(7)));
            let result = match operation {
                SetOperation::Create => validate_create_response(&response, ACCOUNT_ID).map(drop),
                SetOperation::Update => validate_update_response(&response, ACCOUNT_ID, "card-1"),
                SetOperation::Delete => validate_delete_response(&response, ACCOUNT_ID, "card-1"),
            };
            assert!(result.is_err());
        }
    }

    #[test]
    fn set_errors_are_bounded_and_never_include_descriptions() {
        let secret = "private contact data from the server";
        let rejected = set_response(
            CREATE_CALL_ID,
            serde_json::json!({
                "notCreated": {"new1": {
                    "type": "invalidProperties",
                    "description": secret
                }}
            }),
        );
        let error = validate_create_response(&rejected, ACCOUNT_ID)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalidProperties"));
        assert!(!error.contains(secret));

        let method_error = serde_json::json!({
            "sessionState": "session-state",
            "methodResponses": [["error", {
                "type": "serverFail",
                "description": secret
            }, UPDATE_CALL_ID]]
        });
        let error = validate_update_response(&method_error, ACCOUNT_ID, "card-1")
            .unwrap_err()
            .to_string();
        assert!(error.contains("serverFail"));
        assert!(!error.contains(secret));
    }

    fn connection(api_url: String, max_objects_in_get: usize) -> JmapConnection {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        JmapConnection {
            http: http.clone(),
            submission_http: http,
            api_url,
            download_url_template: String::new(),
            upload_url_template: String::new(),
            event_source_url_template: None,
            account_id: "mail-account".into(),
            contacts_account_id: Some(ACCOUNT_ID.into()),
            max_objects_in_get,
            max_objects_in_set: 500,
            submission_extensions: std::collections::HashMap::new(),
        }
    }

    fn config() -> JmapConfig {
        JmapConfig {
            jmap_url: String::new(),
            email: "user@example.test".into(),
            username: "user".into(),
            password: "password".into(),
            access_token: None,
            auth_method: "basic".into(),
            oidc_token_endpoint: String::new(),
            oidc_client_id: String::new(),
        }
    }

    async fn response_server(
        responses: Vec<serde_json::Value>,
    ) -> (String, oneshot::Receiver<Vec<serde_json::Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api", listener.local_addr().unwrap());
        let (requests_tx, requests_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut requests = Vec::with_capacity(responses.len());
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                let header_end = loop {
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0, "request ended before its headers");
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(index) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        break index + 4;
                    }
                };
                let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                while request.len() < header_end + content_length {
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0, "request ended before its body");
                    request.extend_from_slice(&chunk[..read]);
                }
                requests.push(
                    serde_json::from_slice(&request[header_end..header_end + content_length])
                        .unwrap(),
                );

                let body = response.to_string();
                let wire = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(wire.as_bytes()).await.unwrap();
            }
            requests_tx.send(requests).unwrap();
        });
        (url, requests_rx)
    }

    #[tokio::test]
    async fn complete_fetch_paginates_queries_and_chunks_gets() {
        let responses = vec![
            query_response("q1", "query-state", 0, 3, serde_json::json!(["a", "b"])),
            query_response("q1", "query-state", 2, 3, serde_json::json!(["c"])),
            get_response(
                "g1",
                "get-state",
                serde_json::json!([
                    card("b", "uid-b", serde_json::json!({"book": true})),
                    card("a", "uid-a", serde_json::json!({"book": true}))
                ]),
            ),
            get_response(
                "g2",
                "get-state",
                serde_json::json!([card("c", "uid-c", serde_json::json!({"book": true}))]),
            ),
            query_response("qr1", "query-state", 0, 3, serde_json::json!([])),
        ];
        let (url, requests_rx) = response_server(responses).await;
        let contacts = connection(url, 2).fetch_contacts(&config()).await.unwrap();
        assert_eq!(
            contacts
                .iter()
                .map(|contact| contact.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );

        let requests = requests_rx.await.unwrap();
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[0]["methodCalls"][0][1]["position"], 0);
        assert_eq!(requests[0]["methodCalls"][0][1]["limit"], 2);
        assert_eq!(requests[1]["methodCalls"][0][1]["position"], 2);
        assert_eq!(requests[1]["methodCalls"][0][1]["limit"], 2);
        assert_eq!(
            requests[2]["methodCalls"][0][1]["ids"],
            serde_json::json!(["a", "b"])
        );
        assert_eq!(
            requests[3]["methodCalls"][0][1]["ids"],
            serde_json::json!(["c"])
        );
        for request_index in [2, 3] {
            assert_eq!(
                requests[request_index]["methodCalls"][0][1]["properties"],
                serde_json::json!([
                    "id",
                    "uid",
                    "addressBookIds",
                    "name",
                    "emails",
                    "phones",
                    "organizations",
                    "titles",
                    "notes"
                ])
            );
        }
        assert_eq!(requests[4]["methodCalls"][0][1]["position"], 0);
        assert_eq!(requests[4]["methodCalls"][0][1]["limit"], 0);
        for request_index in [0, 1, 4] {
            assert_eq!(
                requests[request_index]["methodCalls"][0][1]["calculateTotal"],
                true
            );
        }
    }

    #[tokio::test]
    async fn complete_fetch_caps_a_stable_empty_collection_query() {
        let responses = vec![
            query_response("q1", "empty", 0, 0, serde_json::json!([])),
            query_response("qr1", "empty", 0, 0, serde_json::json!([])),
        ];
        let (url, requests_rx) = response_server(responses).await;
        assert!(connection(url, CONTACT_PAGE_CAP + 1)
            .fetch_contacts(&config())
            .await
            .unwrap()
            .is_empty());
        let requests = requests_rx.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["methodCalls"][0][1]["limit"], CONTACT_PAGE_CAP);
        assert_eq!(requests[1]["methodCalls"][0][1]["limit"], 0);
    }

    #[tokio::test]
    async fn address_book_request_projects_consumed_fields() {
        let responses = vec![address_book_response(serde_json::json!([
            {"id": "book", "name": "Book"}
        ]))];
        let (url, requests_rx) = response_server(responses).await;
        assert_eq!(
            connection(url, 10)
                .list_address_books(&config())
                .await
                .unwrap(),
            [JmapAddressBook {
                id: "book".into(),
                name: "Book".into()
            }]
        );
        assert_eq!(
            requests_rx.await.unwrap()[0],
            serde_json::json!({
                "using": [CORE_CAPABILITY, CONTACTS_CAPABILITY],
                "methodCalls": [["AddressBook/get", {
                    "accountId": ACCOUNT_ID,
                    "properties": ["id", "name"]
                }, ADDRESS_BOOK_CALL_ID]]
            })
        );
    }

    #[tokio::test]
    async fn contact_methods_fail_before_http_without_a_capable_account() {
        let mut connection = connection("http://127.0.0.1:9/api".into(), 10);
        connection.contacts_account_id = None;
        let error = connection
            .list_address_books(&config())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no contacts-capable account"));
    }

    #[tokio::test]
    async fn zero_get_limit_rejects_reads_before_http() {
        let connection = connection("http://127.0.0.1:9/api".into(), 0);
        let address_book_error = connection
            .list_address_books(&config())
            .await
            .unwrap_err()
            .to_string();
        assert!(address_book_error.contains("maxObjectsInGet=0"));

        let contact_error = connection
            .fetch_contacts(&config())
            .await
            .unwrap_err()
            .to_string();
        assert!(contact_error.contains("maxObjectsInGet=0"));
    }

    #[tokio::test]
    async fn requested_not_found_discards_attempt_and_retries() {
        let responses = vec![
            query_response("q1", "state-1", 0, 1, serde_json::json!(["a"])),
            get_response_with_not_found(
                "g1",
                "get-1",
                serde_json::json!([]),
                serde_json::json!(["a"]),
            ),
            query_response("q1", "state-2", 0, 1, serde_json::json!(["b"])),
            get_response(
                "g1",
                "get-2",
                serde_json::json!([card("b", "uid-b", serde_json::json!({"book": true}))]),
            ),
            query_response("qr1", "state-2", 0, 1, serde_json::json!([])),
        ];
        let (url, requests_rx) = response_server(responses).await;
        let contacts = connection(url, 10).fetch_contacts(&config()).await.unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].id, "b");
        assert_eq!(requests_rx.await.unwrap().len(), 5);
    }

    #[tokio::test]
    async fn changed_get_state_precedes_cross_chunk_uid_validation() {
        let responses = vec![
            query_response("q1", "state-1", 0, 2, serde_json::json!(["a"])),
            query_response("q1", "state-1", 1, 2, serde_json::json!(["b"])),
            get_response(
                "g1",
                "get-1",
                serde_json::json!([card("a", "uid-shared", serde_json::json!({"book": true}))]),
            ),
            get_response(
                "g2",
                "get-changed",
                serde_json::json!([card("b", "uid-shared", serde_json::json!({"book": true}))]),
            ),
            query_response("q1", "state-2", 0, 1, serde_json::json!(["c"])),
            get_response(
                "g1",
                "get-2",
                serde_json::json!([card("c", "uid-c", serde_json::json!({"book": true}))]),
            ),
            query_response("qr1", "state-2", 0, 1, serde_json::json!([])),
        ];
        let (url, requests_rx) = response_server(responses).await;
        let contacts = connection(url, 1).fetch_contacts(&config()).await.unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].id, "c");
        assert_eq!(requests_rx.await.unwrap().len(), 7);
    }

    #[tokio::test]
    async fn repeated_requested_not_found_exhausts_the_bounded_retry() {
        let mut responses = Vec::new();
        for attempt in 0..MAX_FETCH_ATTEMPTS {
            let id = format!("card-{attempt}");
            responses.push(query_response(
                "q1",
                &format!("state-{attempt}"),
                0,
                1,
                serde_json::json!([id]),
            ));
            responses.push(get_response_with_not_found(
                "g1",
                &format!("get-{attempt}"),
                serde_json::json!([]),
                serde_json::json!([format!("card-{attempt}")]),
            ));
        }
        let (url, requests_rx) = response_server(responses).await;
        let error = connection(url, 10)
            .fetch_contacts(&config())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("kept changing"));
        assert_eq!(requests_rx.await.unwrap().len(), MAX_FETCH_ATTEMPTS * 2);
    }

    #[tokio::test]
    async fn late_query_state_change_discards_attempt_and_retries() {
        let responses = vec![
            query_response("q1", "state-1", 0, 1, serde_json::json!(["a"])),
            get_response(
                "g1",
                "get-1",
                serde_json::json!([card("a", "uid-a", serde_json::json!({"book": true}))]),
            ),
            query_response("qr1", "state-2", 0, 1, serde_json::json!([])),
            query_response("q1", "state-2", 0, 1, serde_json::json!(["b"])),
            get_response(
                "g1",
                "get-2",
                serde_json::json!([card("b", "uid-b", serde_json::json!({"book": true}))]),
            ),
            query_response("qr1", "state-2", 0, 1, serde_json::json!([])),
        ];
        let (url, requests_rx) = response_server(responses).await;
        let contacts = connection(url, 10).fetch_contacts(&config()).await.unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].id, "b");
        assert_eq!(requests_rx.await.unwrap().len(), 6);
    }

    #[tokio::test]
    async fn repeated_late_state_changes_exhaust_the_bounded_retry() {
        let mut responses = Vec::new();
        for attempt in 0..MAX_FETCH_ATTEMPTS {
            responses.push(query_response(
                "q1",
                &format!("state-{attempt}"),
                0,
                0,
                serde_json::json!([]),
            ));
            responses.push(query_response(
                "qr1",
                &format!("changed-{attempt}"),
                0,
                0,
                serde_json::json!([]),
            ));
        }
        let (url, requests_rx) = response_server(responses).await;
        let error = connection(url, 10)
            .fetch_contacts(&config())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("kept changing"));
        assert_eq!(requests_rx.await.unwrap().len(), MAX_FETCH_ATTEMPTS * 2);
    }

    #[tokio::test]
    async fn set_requests_allow_zero_get_limit_and_use_the_contacts_account() {
        let responses = vec![
            set_response(
                CREATE_CALL_ID,
                serde_json::json!({"created": {"new1": {"id": "card-1"}}}),
            ),
            set_response(
                UPDATE_CALL_ID,
                serde_json::json!({"updated": {"card-1": null}}),
            ),
            set_response(DELETE_CALL_ID, serde_json::json!({"destroyed": ["card-1"]})),
        ];
        let (url, requests_rx) = response_server(responses).await;
        let connection = connection(url, 0);
        let remote_id = connection
            .create_contact_card(
                &config(),
                "book-1",
                "uid-1@chithi",
                "Ada",
                "[]",
                "[]",
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(remote_id, "card-1");
        connection
            .update_contact_card(
                &config(),
                "card-1",
                "Ada Lovelace",
                "[]",
                "[]",
                None,
                None,
                None,
            )
            .await
            .unwrap();
        connection
            .delete_contact_card(&config(), "card-1")
            .await
            .unwrap();
        let requests = requests_rx.await.unwrap();
        assert_eq!(requests.len(), 3);
        let arguments = &requests[0]["methodCalls"][0][1];
        assert_eq!(arguments["accountId"], ACCOUNT_ID);
        let card = &arguments["create"]["new1"];
        assert_eq!(card["uid"], "uid-1@chithi");
        assert_eq!(card["addressBookIds"], serde_json::json!({"book-1": true}));
        assert!(card["addressBookIds"].get("address_book_id").is_none());
        assert_eq!(requests[1]["methodCalls"][0][1]["accountId"], ACCOUNT_ID);
        assert_eq!(requests[2]["methodCalls"][0][1]["accountId"], ACCOUNT_ID);
    }
}
