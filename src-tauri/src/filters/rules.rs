use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterRule {
    pub id: String,
    pub account_id: Option<String>,
    pub name: String,
    pub enabled: bool,
    pub priority: i32,
    pub match_type: MatchType,
    pub conditions: Vec<Condition>,
    pub actions: Vec<FilterAction>,
    pub stop_processing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchType {
    All,
    Any,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Condition {
    pub field: ConditionField,
    pub op: ConditionOp,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionField {
    From,
    To,
    Cc,
    Subject,
    Size,
    HasAttachment,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOp {
    Contains,
    NotContains,
    Equals,
    NotEquals,
    MatchesRegex,
    GreaterThan,
    LessThan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action")]
#[serde(rename_all = "snake_case")]
pub enum FilterAction {
    Move { target: String },
    Copy { target: String },
    Delete,
    Flag { value: String },
    Unflag { value: String },
    MarkRead,
    MarkUnread,
    Stop,
}
