use super::recurrence_identity::OccurrenceFields;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedOccurrence {
    /// UTC RFC 3339 with `Z`, or an ISO date for an all-day position.
    pub original_start: String,
    pub fields: OccurrenceFields,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    pub occurrences: Vec<GeneratedOccurrence>,
    /// True only when another overlapping position was actually found.
    pub has_more: bool,
}
