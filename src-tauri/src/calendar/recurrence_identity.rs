use chrono::{DateTime, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecurrenceObjectKind {
    Master,
    Occurrence,
    Exception,
    Exclusion,
}

impl RecurrenceObjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Master => "master",
            Self::Occurrence => "occurrence",
            Self::Exception => "exception",
            Self::Exclusion => "exclusion",
        }
    }

    pub fn from_stored(value: &str) -> Result<Self> {
        match value {
            "master" => Ok(Self::Master),
            "occurrence" => Ok(Self::Occurrence),
            "exception" => Ok(Self::Exception),
            "exclusion" => Ok(Self::Exclusion),
            _ => Err(Error::Other(format!(
                "Unknown recurrence object kind: {value:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecurrenceValueType {
    Date,
    DateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecurrenceMutationScope {
    ThisOccurrence,
    EntireSeries,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecurrenceMutationPlan {
    pub scope: RecurrenceMutationScope,
    pub recurrence_object_id: String,
    pub event_id: String,
    pub account_id: String,
    pub calendar_id: String,
    pub object_kind: RecurrenceObjectKind,
    pub local_series_event_id: Option<String>,
    pub provider_calendar_id: Option<String>,
    pub provider_series_id: Option<String>,
    pub provider_occurrence_id: Option<String>,
    pub recurrence_id: Option<String>,
    pub recurrence_timezone: Option<String>,
    pub recurrence_value_type: Option<RecurrenceValueType>,
    pub occurrence: OccurrenceFields,
    pub expected_provider_revision: Option<String>,
    pub expected_local_revision: i64,
    pub backend_protocol: String,
    pub remote_target_id: String,
}

/// Renderer-safe recurrence identity and effective occurrence projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecurrenceObjectSummary {
    pub object_id: String,
    pub event_id: String,
    pub account_id: String,
    pub calendar_id: String,
    pub kind: RecurrenceObjectKind,
    pub local_series_event_id: Option<String>,
    pub provider_calendar_id: Option<String>,
    pub provider_series_id: Option<String>,
    pub provider_occurrence_id: Option<String>,
    pub recurrence_id: Option<String>,
    pub recurrence_timezone: Option<String>,
    pub recurrence_value_type: Option<RecurrenceValueType>,
    pub occurrence: OccurrenceFields,
    pub provider_revision: Option<String>,
}

impl RecurrenceObjectSummary {
    pub fn from_identity(identity: RecurrenceIdentity, calendar_id: &str) -> Self {
        Self {
            object_id: identity.object_id,
            event_id: identity.event_id,
            account_id: identity.account_id,
            calendar_id: calendar_id.to_owned(),
            kind: identity.kind,
            local_series_event_id: identity.local_series_event_id,
            provider_calendar_id: identity.provider_calendar_id,
            provider_series_id: identity.provider_series_id,
            provider_occurrence_id: identity.provider_occurrence_id,
            recurrence_id: identity.recurrence_id,
            recurrence_timezone: identity.recurrence_timezone,
            recurrence_value_type: identity.recurrence_value_type,
            occurrence: identity.occurrence,
            provider_revision: identity.provider_revision,
        }
    }
}

/// Fields that THIS-OCCURRENCE may change. `None` means unchanged; an empty
/// description or location clears that field, matching ordinary event edits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateOccurrenceInput {
    pub title: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub all_day: Option<bool>,
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OccurrenceFields {
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_time: String,
    pub end_time: String,
    pub all_day: bool,
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdatedOccurrence {
    pub event_id: String,
    pub recurrence_object_id: String,
    #[serde(flatten)]
    pub fields: OccurrenceFields,
    pub local_revision: i64,
    pub kind: RecurrenceObjectKind,
    pub provider_revision: Option<String>,
}

impl OccurrenceFields {
    pub fn validate(&self) -> Result<()> {
        if self.title.trim().is_empty() {
            return Err(Error::Other("occurrence title must be non-empty".into()));
        }
        if self
            .timezone
            .as_deref()
            .is_some_and(|value| value.trim().is_empty() || value.chars().any(char::is_control))
        {
            return Err(Error::Other(
                "timezone must be non-empty and contain no control characters".into(),
            ));
        }
        if self.all_day {
            let start = NaiveDate::parse_from_str(&self.start_time, "%Y-%m-%d").map_err(|_| {
                Error::Other("all-day occurrences require ISO date start and end values".into())
            })?;
            let end = NaiveDate::parse_from_str(&self.end_time, "%Y-%m-%d").map_err(|_| {
                Error::Other("all-day occurrences require ISO date start and end values".into())
            })?;
            validate_positive_range(start, end)
        } else {
            let start = DateTime::parse_from_rfc3339(&self.start_time).map_err(|_| {
                Error::Other("timed occurrences require RFC 3339 start and end values".into())
            })?;
            let end = DateTime::parse_from_rfc3339(&self.end_time).map_err(|_| {
                Error::Other("timed occurrences require RFC 3339 start and end values".into())
            })?;
            validate_positive_range(start, end)
        }
    }
}

impl RecurrenceValueType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Date => "date",
            Self::DateTime => "date-time",
        }
    }

    pub fn from_stored(value: &str) -> Result<Self> {
        match value {
            "date" => Ok(Self::Date),
            "date-time" => Ok(Self::DateTime),
            _ => Err(Error::Other(format!(
                "Unknown recurrence value type: {value:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecurrenceIdentity {
    pub object_id: String,
    pub account_id: String,
    pub event_id: String,
    pub local_series_event_id: Option<String>,
    pub provider_calendar_id: Option<String>,
    pub provider_series_id: Option<String>,
    pub provider_occurrence_id: Option<String>,
    pub recurrence_id: Option<String>,
    pub recurrence_timezone: Option<String>,
    pub recurrence_value_type: Option<RecurrenceValueType>,
    pub occurrence: OccurrenceFields,
    #[serde(skip_serializing)]
    pub provider_native_data: Option<String>,
    pub provider_revision: Option<String>,
    pub kind: RecurrenceObjectKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecurrenceIdentitySeed {
    pub local_series_event_id: Option<String>,
    pub provider_calendar_id: Option<String>,
    pub provider_series_id: Option<String>,
    pub provider_occurrence_id: Option<String>,
    pub recurrence_id: Option<String>,
    pub recurrence_timezone: Option<String>,
    pub recurrence_value_type: Option<RecurrenceValueType>,
    pub occurrence: OccurrenceFields,
    #[serde(skip_serializing)]
    pub provider_native_data: Option<String>,
    pub provider_revision: Option<String>,
    pub kind: RecurrenceObjectKind,
}

impl RecurrenceIdentity {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("object_id", Some(self.object_id.as_str())),
            ("account_id", Some(self.account_id.as_str())),
            ("event_id", Some(self.event_id.as_str())),
        ] {
            if let Some(value) = value {
                validate_identity(name, value)?;
            }
        }

        self.as_seed().validate()
    }

    fn as_seed(&self) -> RecurrenceIdentitySeed {
        RecurrenceIdentitySeed {
            local_series_event_id: self.local_series_event_id.clone(),
            provider_calendar_id: self.provider_calendar_id.clone(),
            provider_series_id: self.provider_series_id.clone(),
            provider_occurrence_id: self.provider_occurrence_id.clone(),
            recurrence_id: self.recurrence_id.clone(),
            recurrence_timezone: self.recurrence_timezone.clone(),
            recurrence_value_type: self.recurrence_value_type,
            occurrence: self.occurrence.clone(),
            provider_native_data: self.provider_native_data.clone(),
            provider_revision: self.provider_revision.clone(),
            kind: self.kind,
        }
    }
}

impl RecurrenceIdentitySeed {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            (
                "local_series_event_id",
                self.local_series_event_id.as_deref(),
            ),
            ("provider_calendar_id", self.provider_calendar_id.as_deref()),
            ("provider_series_id", self.provider_series_id.as_deref()),
            (
                "provider_occurrence_id",
                self.provider_occurrence_id.as_deref(),
            ),
            ("recurrence_id", self.recurrence_id.as_deref()),
        ] {
            if let Some(value) = value {
                validate_identity(name, value)?;
            }
        }

        let has_provider_identity =
            self.provider_series_id.is_some() || self.provider_occurrence_id.is_some();
        if has_provider_identity != self.provider_calendar_id.is_some() {
            return Err(Error::Other(
                "provider recurrence identity and provider_calendar_id must be supplied together"
                    .into(),
            ));
        }

        if self
            .recurrence_timezone
            .as_deref()
            .is_some_and(|value| value.trim().is_empty() || value.chars().any(char::is_control))
        {
            return Err(Error::Other(
                "recurrence_timezone must be non-empty and contain no control characters".into(),
            ));
        }

        match self.kind {
            RecurrenceObjectKind::Master => {
                if self.recurrence_id.is_some() || self.recurrence_value_type.is_some() {
                    return Err(Error::Other(
                        "master recurrence objects cannot have a recurrence ID or value type"
                            .into(),
                    ));
                }
            }
            _ => {
                if self.recurrence_id.is_none() || self.recurrence_value_type.is_none() {
                    return Err(Error::Other(
                        "non-master recurrence objects require a recurrence ID and value type"
                            .into(),
                    ));
                }
                if self.local_series_event_id.is_none() && self.provider_series_id.is_none() {
                    return Err(Error::Other(
                        "non-master recurrence objects require a local or provider series identity"
                            .into(),
                    ));
                }
            }
        }

        self.occurrence.validate()
    }

    pub fn bind(
        &self,
        account_id: impl Into<String>,
        event_id: impl Into<String>,
        object_id: impl Into<String>,
    ) -> Result<RecurrenceIdentity> {
        self.validate()?;
        let identity = RecurrenceIdentity {
            object_id: object_id.into(),
            account_id: account_id.into(),
            event_id: event_id.into(),
            local_series_event_id: self.local_series_event_id.clone(),
            provider_calendar_id: self.provider_calendar_id.clone(),
            provider_series_id: self.provider_series_id.clone(),
            provider_occurrence_id: self.provider_occurrence_id.clone(),
            recurrence_id: self.recurrence_id.clone(),
            recurrence_timezone: self.recurrence_timezone.clone(),
            recurrence_value_type: self.recurrence_value_type,
            occurrence: self.occurrence.clone(),
            provider_native_data: self.provider_native_data.clone(),
            provider_revision: self.provider_revision.clone(),
            kind: self.kind,
        };
        identity.validate()?;
        Ok(identity)
    }
}

fn validate_identity(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(Error::Other(format!(
            "{name} must be non-empty and contain no control characters"
        )));
    }
    Ok(())
}

fn validate_positive_range<T: PartialOrd>(start: T, end: T) -> Result<()> {
    if end <= start {
        return Err(Error::Other(
            "occurrence end must be after occurrence start".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(kind: RecurrenceObjectKind) -> RecurrenceIdentity {
        let non_master = kind != RecurrenceObjectKind::Master;
        RecurrenceIdentity {
            object_id: "object".into(),
            account_id: "account".into(),
            event_id: "event".into(),
            local_series_event_id: non_master.then(|| "master-event".into()),
            provider_calendar_id: None,
            provider_series_id: None,
            provider_occurrence_id: None,
            recurrence_id: non_master.then(|| "2026-09-15T10:00:00Z".into()),
            recurrence_timezone: Some("Europe/Stockholm".into()),
            recurrence_value_type: non_master.then_some(RecurrenceValueType::DateTime),
            occurrence: OccurrenceFields {
                title: "Occurrence".into(),
                description: Some("Description".into()),
                location: Some("Room 2".into()),
                start_time: "2026-09-15T10:00:00Z".into(),
                end_time: "2026-09-15T11:00:00Z".into(),
                all_day: false,
                timezone: Some("Europe/Helsinki".into()),
            },
            provider_native_data: None,
            provider_revision: None,
            kind,
        }
    }

    fn seed(kind: RecurrenceObjectKind) -> RecurrenceIdentitySeed {
        identity(kind).as_seed()
    }

    #[test]
    fn all_kinds_and_value_types_have_stable_serde_values() {
        for (kind, stored) in [
            (RecurrenceObjectKind::Master, "master"),
            (RecurrenceObjectKind::Occurrence, "occurrence"),
            (RecurrenceObjectKind::Exception, "exception"),
            (RecurrenceObjectKind::Exclusion, "exclusion"),
        ] {
            assert_eq!(kind.as_str(), stored);
            assert_eq!(RecurrenceObjectKind::from_stored(stored).unwrap(), kind);
            assert_eq!(serde_json::to_value(kind).unwrap(), stored);
            assert_eq!(
                serde_json::from_str::<RecurrenceObjectKind>(&format!("\"{stored}\"")).unwrap(),
                kind
            );
        }
        for (value_type, stored) in [
            (RecurrenceValueType::Date, "date"),
            (RecurrenceValueType::DateTime, "date-time"),
        ] {
            assert_eq!(value_type.as_str(), stored);
            assert_eq!(
                RecurrenceValueType::from_stored(stored).unwrap(),
                value_type
            );
            assert_eq!(serde_json::to_value(value_type).unwrap(), stored);
        }
        assert!(RecurrenceObjectKind::from_stored("future").is_err());
        assert!(RecurrenceValueType::from_stored("instant").is_err());

        for (scope, wire) in [
            (RecurrenceMutationScope::ThisOccurrence, "this-occurrence"),
            (RecurrenceMutationScope::EntireSeries, "entire-series"),
        ] {
            assert_eq!(serde_json::to_value(scope).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<RecurrenceMutationScope>(&format!("\"{wire}\"")).unwrap(),
                scope
            );
        }
    }

    #[test]
    fn mutation_plan_never_serializes_provider_native_data() {
        let plan = RecurrenceMutationPlan {
            scope: RecurrenceMutationScope::ThisOccurrence,
            recurrence_object_id: "object".into(),
            event_id: "event".into(),
            account_id: "account".into(),
            calendar_id: "calendar".into(),
            object_kind: RecurrenceObjectKind::Occurrence,
            local_series_event_id: Some("master".into()),
            provider_calendar_id: Some("provider-calendar".into()),
            provider_series_id: Some("remote-master".into()),
            provider_occurrence_id: Some("remote-occurrence".into()),
            recurrence_id: Some("2026-09-15".into()),
            recurrence_timezone: None,
            recurrence_value_type: Some(RecurrenceValueType::Date),
            occurrence: OccurrenceFields {
                title: "Occurrence".into(),
                description: None,
                location: None,
                start_time: "2026-09-15".into(),
                end_time: "2026-09-16".into(),
                all_day: true,
                timezone: None,
            },
            expected_provider_revision: Some("revision".into()),
            expected_local_revision: 42,
            backend_protocol: "google".into(),
            remote_target_id: "remote-occurrence".into(),
        };
        let serialized = serde_json::to_value(plan).unwrap();
        assert!(serialized.get("provider_native_data").is_none());
        assert_eq!(serialized["occurrence"]["title"], "Occurrence");
        assert!(serialized.get("effective_start").is_none());
    }

    #[test]
    fn recurrence_summary_contains_safe_selection_fields_only() {
        let mut identity = identity(RecurrenceObjectKind::Exception);
        identity.provider_calendar_id = Some("provider-calendar".into());
        identity.provider_series_id = Some("provider-series".into());
        identity.provider_occurrence_id = Some("provider-occurrence".into());
        identity.provider_native_data = Some("secret native payload".into());
        identity.provider_revision = Some("revision".into());

        let value = serde_json::to_value(RecurrenceObjectSummary::from_identity(
            identity,
            "local-calendar",
        ))
        .unwrap();
        assert_eq!(value["calendar_id"], "local-calendar");
        assert_eq!(value["provider_calendar_id"], "provider-calendar");
        assert_eq!(value["occurrence"]["title"], "Occurrence");
        assert_eq!(value["provider_revision"], "revision");
        assert!(value.get("provider_native_data").is_none());
        assert!(!value.to_string().contains("secret native payload"));
    }

    #[test]
    fn validation_enforces_kind_identity_and_effective_range_invariants() {
        for kind in [
            RecurrenceObjectKind::Master,
            RecurrenceObjectKind::Occurrence,
            RecurrenceObjectKind::Exception,
            RecurrenceObjectKind::Exclusion,
        ] {
            identity(kind).validate().unwrap();
        }

        let mut master = identity(RecurrenceObjectKind::Master);
        master.recurrence_id = Some("2026-09-15".into());
        assert!(master.validate().is_err());

        let mut occurrence = identity(RecurrenceObjectKind::Occurrence);
        occurrence.recurrence_id = None;
        assert!(occurrence.validate().is_err());
        occurrence.recurrence_id = Some("2026-09-15".into());
        occurrence.recurrence_value_type = None;
        assert!(occurrence.validate().is_err());
        occurrence.recurrence_value_type = Some(RecurrenceValueType::Date);
        occurrence.local_series_event_id = None;
        assert!(occurrence.validate().is_err());

        let mut provider = identity(RecurrenceObjectKind::Occurrence);
        provider.provider_series_id = Some("provider-series".into());
        assert!(provider.validate().is_err());
        provider.provider_calendar_id = Some("provider-calendar".into());
        provider.validate().unwrap();
        provider.provider_series_id = None;
        provider.provider_occurrence_id = Some("provider-occurrence".into());
        provider.validate().unwrap();
        for invalid in ["", "   ", "bad\ncalendar"] {
            provider.provider_calendar_id = Some(invalid.into());
            assert!(provider.validate().is_err(), "{invalid:?}");
        }
        let mut local = identity(RecurrenceObjectKind::Occurrence);
        local.provider_calendar_id = Some("provider-calendar".into());
        assert!(local.validate().is_err());

        for invalid in ["", "   ", "bad\nid"] {
            let mut value = identity(RecurrenceObjectKind::Master);
            value.object_id = invalid.into();
            assert!(value.validate().is_err(), "{invalid:?}");
        }

        let mut value = identity(RecurrenceObjectKind::Master);
        value.occurrence.end_time = value.occurrence.start_time.clone();
        assert!(value.validate().is_err());
        value.occurrence.all_day = true;
        value.occurrence.start_time = "2026-09-16".into();
        value.occurrence.end_time = "2026-09-15".into();
        assert!(value.validate().is_err());
        value.occurrence.start_time = "not-a-date".into();
        value.occurrence.end_time = "also-not-a-date".into();
        assert!(value.validate().is_err());
    }

    #[test]
    fn seed_validation_matches_identity_and_binding_adds_local_ids() {
        let mut value = seed(RecurrenceObjectKind::Occurrence);
        value.validate().unwrap();
        let bound = value.bind("account", "event", "object").unwrap();
        assert_eq!(bound.account_id, "account");
        assert_eq!(bound.event_id, "event");
        assert_eq!(bound.object_id, "object");

        value.provider_series_id = None;
        value.local_series_event_id = None;
        assert!(value.validate().is_err());
        assert!(value.bind("account", "event", "object").is_err());
        assert!(seed(RecurrenceObjectKind::Master)
            .bind("", "event", "object")
            .is_err());
    }

    #[test]
    fn occurrence_fields_enforce_title_timezone_and_date_shape() {
        let mut fields = OccurrenceFields {
            title: "Occurrence".into(),
            description: Some(String::new()),
            location: None,
            start_time: "2026-09-15T10:00:00Z".into(),
            end_time: "2026-09-15T11:00:00Z".into(),
            all_day: false,
            timezone: Some("Europe/Stockholm".into()),
        };
        fields.validate().unwrap();
        fields.title = " ".into();
        assert!(fields.validate().is_err());
        fields.title = "Occurrence".into();
        fields.timezone = Some("bad\ntimezone".into());
        assert!(fields.validate().is_err());
        fields.timezone = None;
        fields.all_day = true;
        assert!(fields.validate().is_err());
        fields.start_time = "2026-09-15".into();
        fields.end_time = "2026-09-16".into();
        fields.validate().unwrap();
        fields.all_day = false;
        assert!(fields.validate().is_err());
    }

    #[test]
    fn updated_occurrence_is_flat_and_cannot_expose_native_data() {
        let result = UpdatedOccurrence {
            event_id: "event".into(),
            recurrence_object_id: "object".into(),
            fields: OccurrenceFields {
                title: "Occurrence".into(),
                description: None,
                location: None,
                start_time: "2026-09-15".into(),
                end_time: "2026-09-16".into(),
                all_day: true,
                timezone: None,
            },
            local_revision: 2,
            kind: RecurrenceObjectKind::Exception,
            provider_revision: Some("revision".into()),
        };
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["title"], "Occurrence");
        assert!(value.get("fields").is_none());
        assert!(value.get("provider_native_data").is_none());
    }

    #[test]
    fn identities_and_seeds_never_serialize_provider_native_data() {
        let mut identity = identity(RecurrenceObjectKind::Occurrence);
        identity.provider_native_data = Some("secret identity payload".into());
        let identity_json = serde_json::to_value(&identity).unwrap();
        assert!(identity_json.get("provider_native_data").is_none());

        let mut seed = identity.as_seed();
        seed.provider_native_data = Some("secret seed payload".into());
        let seed_json = serde_json::to_value(&seed).unwrap();
        assert!(seed_json.get("provider_native_data").is_none());

        let deserialized: RecurrenceIdentitySeed = serde_json::from_value(serde_json::json!({
            "local_series_event_id": "master-event",
            "provider_calendar_id": null,
            "provider_series_id": null,
            "provider_occurrence_id": null,
            "recurrence_id": "2026-09-15T10:00:00Z",
            "recurrence_timezone": "Europe/Stockholm",
            "recurrence_value_type": "date-time",
            "occurrence": seed.occurrence,
            "provider_native_data": "trusted database payload",
            "provider_revision": null,
            "kind": "occurrence"
        }))
        .unwrap();
        assert_eq!(
            deserialized.provider_native_data.as_deref(),
            Some("trusted database payload")
        );
    }
}
