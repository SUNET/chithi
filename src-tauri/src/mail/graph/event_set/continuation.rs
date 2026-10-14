//! Validate collection identity without rewriting Graph's opaque paging URL.
//! OData keyed segments and slash-separated segments address the same resource.

use super::{invalid, required_string, GraphClient, Method};
use crate::error::{Error, Result};

#[derive(Debug, PartialEq, Eq)]
enum Principal {
    Me,
    User(String),
}

#[derive(Debug, PartialEq, Eq)]
enum Calendar {
    Default,
    Id(String),
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Events,
    Instances,
    Exceptions,
}

#[derive(Debug, PartialEq, Eq)]
struct Collection {
    principal: Principal,
    calendar: Option<Calendar>,
    event: Option<String>,
    kind: Kind,
}

fn decoded(value: &str) -> Result<String> {
    // Decode one segment exactly once. Encoded slashes remain part of an
    // opaque ID, never additional path segments; `+` is not form decoding.
    let bytes = value.as_bytes();
    for (at, byte) in bytes.iter().enumerate() {
        if *byte == b'%'
            && !bytes
                .get(at + 1..at + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit))
        {
            return Err(invalid("invalid collection encoding"));
        }
    }
    let value = urlencoding::decode(value).map_err(|_| invalid("invalid collection encoding"))?;
    if value.is_empty()
        || matches!(value.as_ref(), "." | "..")
        || value.chars().any(char::is_control)
    {
        return Err(invalid("invalid collection segment"));
    }
    Ok(value.into_owned())
}

fn entity(parts: &[String], at: &mut usize, name: &str) -> Result<String> {
    let part = parts
        .get(*at)
        .ok_or_else(|| invalid("missing collection entity"))?;
    if part == name {
        let key = parts
            .get(*at + 1)
            .ok_or_else(|| invalid("missing collection key"))?
            .clone();
        *at += 2;
        return Ok(key);
    }
    let key = part
        .strip_prefix(&format!("{name}('"))
        .and_then(|key| key.strip_suffix("')"))
        .ok_or_else(|| invalid("unsupported collection entity"))?;
    // OData string literals escape apostrophes by doubling them.
    let mut result = String::new();
    let mut chars = key.chars();
    while let Some(ch) = chars.next() {
        if ch == '\'' && chars.next() != Some('\'') {
            return Err(invalid("invalid OData collection key"));
        }
        result.push(ch);
    }
    if result.is_empty() || matches!(result.as_str(), "." | "..") {
        return Err(invalid("invalid OData collection key"));
    }
    *at += 1;
    Ok(result)
}

fn parts(client: &GraphClient, url: &reqwest::Url) -> Result<Vec<String>> {
    let root = reqwest::Url::parse(&client.endpoints.v1_api_root).map_err(invalid)?;
    let prefix = format!("{}/", root.path().trim_end_matches('/'));
    let path = url
        .path()
        .strip_prefix(&prefix)
        .ok_or_else(|| invalid("collection left API root"))?;
    path.split('/').map(decoded).collect()
}

fn parse(parts: &[String]) -> Result<Collection> {
    let mut at = 0;
    let principal = if parts.first().is_some_and(|part| part == "me") {
        at += 1;
        Principal::Me
    } else {
        Principal::User(entity(parts, &mut at, "users")?)
    };
    let calendar = match parts.get(at).map(String::as_str) {
        Some("calendar") => {
            at += 1;
            Some(Calendar::Default)
        }
        Some(part) if part == "calendars" || part.starts_with("calendars('") => {
            Some(Calendar::Id(entity(parts, &mut at, "calendars")?))
        }
        _ => None,
    };
    if parts.get(at).is_some_and(|part| part == "events") && at + 1 == parts.len() {
        return Ok(Collection {
            principal,
            calendar: Some(calendar.unwrap_or(Calendar::Default)),
            event: None,
            kind: Kind::Events,
        });
    }
    let event = entity(parts, &mut at, "events")?;
    let kind = match parts.get(at).map(String::as_str) {
        Some("instances") => Kind::Instances,
        Some("exceptionOccurrences") => Kind::Exceptions,
        _ => return Err(invalid("unsupported calendar collection")),
    };
    if at + 1 != parts.len() {
        return Err(invalid("unexpected collection suffix"));
    }
    Ok(Collection {
        principal,
        calendar,
        event: Some(event),
        kind,
    })
}

/// Only structural labels are exposed. Never include a query/skip token,
/// mailbox identifier, calendar identifier, or event identifier in diagnostics.
fn shape(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| match part.as_str() {
            "me"
            | "users"
            | "calendar"
            | "calendars"
            | "events"
            | "instances"
            | "exceptionOccurrences" => part.clone(),
            _ if part.starts_with("users('") => "users(<id>)".into(),
            _ if part.starts_with("calendars('") => "calendars(<id>)".into(),
            _ if part.starts_with("events('") => "events(<id>)".into(),
            _ => "<id>".into(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub(super) struct CollectionGuard {
    expected: Collection,
    expected_shape: String,
    principal: Option<(String, Option<String>)>,
    default_calendar: Option<String>,
}

impl CollectionGuard {
    pub(super) fn new(client: &GraphClient, scope: &str) -> Result<Self> {
        let url = client.calendar_set_url(scope)?;
        let parts = parts(client, &url)?;
        let expected = parse(&parts)?;
        if expected.principal != Principal::Me {
            return Err(invalid("expected signed-in-user collection"));
        }
        Ok(Self {
            expected,
            expected_shape: shape(&parts),
            principal: None,
            default_calendar: None,
        })
    }

    pub(super) async fn validate(
        &mut self,
        client: &GraphClient,
        url: &reqwest::Url,
    ) -> Result<()> {
        let parts = parts(client, url)?;
        let actual_shape = shape(&parts);
        let reject = |reason| {
            let message = format!("continuation left selected calendar collection: {reason}; expected {}, received {actual_shape}", self.expected_shape);
            log::warn!("Graph pagination rejected: {message}");
            Error::CalendarReadIncomplete(message)
        };
        let actual = parse(&parts).map_err(|_| reject("unsupported path shape"))?;
        if actual.kind != self.expected.kind || actual.event != self.expected.event {
            return Err(reject("collection or series identity mismatch"));
        }
        if let Principal::User(user) = &actual.principal {
            if self.principal.is_none() {
                let profile = client
                    .calendar_set_request(
                        Method::GET,
                        "/me",
                        &[("$select", "id,userPrincipalName")],
                        None,
                        None,
                    )
                    .await?;
                self.principal = Some((
                    required_string(&profile, "id")?.to_owned(),
                    profile["userPrincipalName"].as_str().map(str::to_owned),
                ));
            }
            let (id, upn) = self
                .principal
                .as_ref()
                .ok_or_else(|| reject("missing signed-in-user identity"))?;
            let same_guid = uuid::Uuid::parse_str(id)
                .ok()
                .zip(uuid::Uuid::parse_str(user).ok())
                .is_some_and(|(a, b)| a == b);
            if user != id
                && !same_guid
                && !upn
                    .as_ref()
                    .is_some_and(|upn| user.contains('@') && user.eq_ignore_ascii_case(upn))
            {
                return Err(reject("signed-in-user identity mismatch"));
            }
        }
        match (&self.expected.calendar, &actual.calendar) {
            (Some(Calendar::Id(expected)), Some(Calendar::Id(actual))) if expected != actual => {
                return Err(reject("calendar identity mismatch"));
            }
            (Some(Calendar::Id(expected)), Some(Calendar::Default))
            | (Some(Calendar::Default), Some(Calendar::Id(expected))) => {
                if self.default_calendar.is_none() {
                    let calendar = client
                        .calendar_set_request(
                            Method::GET,
                            "/me/calendar",
                            &[("$select", "id")],
                            None,
                            None,
                        )
                        .await?;
                    self.default_calendar = Some(required_string(&calendar, "id")?.to_owned());
                }
                if self.default_calendar.as_ref() != Some(expected) {
                    return Err(reject("default calendar identity mismatch"));
                }
            }
            // Immutable Graph event IDs are account-wide. Exception/instance
            // navigation may omit the calendar only for the exact same series.
            (Some(_), None) if self.expected.event.is_some() => {}
            (None, Some(_)) | (Some(_), None) => return Err(reject("missing calendar identity")),
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_and_odata_key_forms_have_identical_collection_identity() {
        let slash = ["me", "calendars", "cal/key+'quote", "events"].map(str::to_owned);
        let keyed = ["me", "calendars('cal/key+''quote')", "events"].map(str::to_owned);
        assert_eq!(parse(&slash).unwrap(), parse(&keyed).unwrap());
        assert_eq!(decoded("cal%2fkey%2B%27quote").unwrap(), slash[2]);
        assert_eq!(decoded("cal%252fkey").unwrap(), "cal%2fkey");
    }

    #[test]
    fn malformed_paths_are_not_accepted_as_equivalent_collections() {
        for path in [
            "me/calendars('')/events",
            "me/calendars('bad'quote')/events",
            "me/calendars('a')/events/extra",
            "groups/a/calendars/b/events",
            "me/events/a/attachments",
            "me/calendars/a/events/a/instances/extra",
        ] {
            let parts = path.split('/').map(str::to_owned).collect::<Vec<_>>();
            assert!(parse(&parts).is_err(), "{path}");
        }
        for segment in ["bad%", "bad%0", "bad%GG", "%FF", "%00", "%2e%2e", ""] {
            assert!(decoded(segment).is_err(), "{segment}");
        }
    }

    #[test]
    fn diagnostics_redact_all_opaque_identifiers() {
        let parts = [
            "users('private@example.test')",
            "calendars('private-calendar')",
            "events",
            "private-event",
            "exceptionOccurrences",
        ]
        .map(str::to_owned);
        assert_eq!(
            shape(&parts),
            "users(<id>)/calendars(<id>)/events/<id>/exceptionOccurrences"
        );
    }
}
