use regex::RegexBuilder;
use serde::{Deserialize, Serialize};

use super::rules::{Condition, ConditionField, ConditionOp, FilterAction, FilterRule, MatchType};

/// Lightweight struct holding the message fields needed for filter matching.
/// Constructed from DB row data before running the filter engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageData {
    pub id: String,
    pub uid: u32,
    pub folder_path: String,
    pub from_name: Option<String>,
    pub from_email: String,
    pub to_addresses: Vec<AddressEntry>,
    pub cc_addresses: Vec<AddressEntry>,
    pub subject: Option<String>,
    pub size: u64,
    pub has_attachments: bool,
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressEntry {
    pub name: Option<String>,
    pub email: String,
}

/// Check whether a single filter rule matches the given message.
pub fn matches_message(rule: &FilterRule, msg: &MessageData) -> bool {
    if rule.conditions.is_empty() {
        return false;
    }

    match rule.match_type {
        MatchType::All => rule.conditions.iter().all(|c| eval_condition(c, msg)),
        MatchType::Any => rule.conditions.iter().any(|c| eval_condition(c, msg)),
    }
}

/// Run through all rules sorted by priority (highest first), collect actions
/// from matching rules. If a matching rule has `stop_processing` set, stop
/// evaluating further rules.
pub fn apply_filters(rules: &[FilterRule], msg: &MessageData) -> Vec<FilterAction> {
    let mut sorted: Vec<&FilterRule> = rules.iter().filter(|r| r.enabled).collect();
    sorted.sort_by(|a, b| b.priority.cmp(&a.priority));

    let mut collected_actions: Vec<FilterAction> = Vec::new();

    for rule in sorted {
        if matches_message(rule, msg) {
            log::info!(
                "Filter '{}' (id={}) matched message id={} subject={:?}",
                rule.name,
                rule.id,
                msg.id,
                msg.subject
            );
            collected_actions.extend(rule.actions.clone());

            if rule.stop_processing {
                log::debug!(
                    "Filter '{}' has stop_processing=true, halting further evaluation",
                    rule.name
                );
                break;
            }
        }
    }

    collected_actions
}

/// Evaluate a single condition against a message.
fn eval_condition(cond: &Condition, msg: &MessageData) -> bool {
    match cond.field {
        ConditionField::From => {
            // Match against both from_name and from_email
            let from_combined = format!(
                "{} {}",
                msg.from_name.as_deref().unwrap_or(""),
                msg.from_email
            );
            eval_string_op(&cond.op, &from_combined, &cond.value)
        }
        ConditionField::To => {
            // Match if any To address matches
            eval_address_list_op(&cond.op, &msg.to_addresses, &cond.value)
        }
        ConditionField::Cc => {
            // Match if any Cc address matches
            eval_address_list_op(&cond.op, &msg.cc_addresses, &cond.value)
        }
        ConditionField::Subject => {
            let subject = msg.subject.as_deref().unwrap_or("");
            eval_string_op(&cond.op, subject, &cond.value)
        }
        ConditionField::Size => eval_numeric_op(&cond.op, msg.size, &cond.value),
        ConditionField::HasAttachment => {
            let expected = cond.value.to_lowercase() == "true";
            msg.has_attachments == expected
        }
    }
}

/// Evaluate a string operation (case-insensitive).
fn eval_string_op(op: &ConditionOp, haystack: &str, needle: &str) -> bool {
    let h = haystack.to_lowercase();
    let n = needle.to_lowercase();

    match op {
        ConditionOp::Contains => h.contains(&n),
        ConditionOp::NotContains => !h.contains(&n),
        ConditionOp::Equals => h == n,
        ConditionOp::NotEquals => h != n,
        ConditionOp::MatchesRegex => {
            match RegexBuilder::new(needle).size_limit(1_000_000).build() {
                Ok(re) => re.is_match(haystack),
                Err(e) => {
                    log::warn!("Invalid or too complex regex '{}': {}", needle, e);
                    false
                }
            }
        }
        ConditionOp::GreaterThan | ConditionOp::LessThan => {
            // Numeric ops don't apply to strings; always false
            false
        }
    }
}

/// Evaluate a numeric operation (for size comparisons).
fn eval_numeric_op(op: &ConditionOp, actual: u64, value_str: &str) -> bool {
    let threshold = match value_str.parse::<u64>() {
        Ok(v) => v,
        Err(e) => {
            log::warn!(
                "Could not parse numeric filter value '{}': {}",
                value_str,
                e
            );
            return false;
        }
    };

    match op {
        ConditionOp::GreaterThan => actual > threshold,
        ConditionOp::LessThan => actual < threshold,
        ConditionOp::Equals => actual == threshold,
        ConditionOp::NotEquals => actual != threshold,
        _ => false,
    }
}

/// Evaluate an operator against a list of addresses.
/// Returns true if ANY address in the list matches.
/// For NotContains / NotEquals, returns true only if NONE match the positive form.
fn eval_address_list_op(op: &ConditionOp, addrs: &[AddressEntry], value: &str) -> bool {
    if addrs.is_empty() {
        return matches!(op, ConditionOp::NotContains | ConditionOp::NotEquals);
    }

    match op {
        ConditionOp::NotContains => {
            // True if no address contains the value
            !addrs.iter().any(|a| {
                let combined = format!("{} {}", a.name.as_deref().unwrap_or(""), a.email);
                eval_string_op(&ConditionOp::Contains, &combined, value)
            })
        }
        ConditionOp::NotEquals => {
            // True if no address equals the value
            !addrs.iter().any(|a| {
                let combined = format!("{} {}", a.name.as_deref().unwrap_or(""), a.email);
                eval_string_op(&ConditionOp::Equals, &combined, value)
            })
        }
        _ => {
            // For Contains, Equals, MatchesRegex: true if any address matches
            addrs.iter().any(|a| {
                let combined = format!("{} {}", a.name.as_deref().unwrap_or(""), a.email);
                eval_string_op(op, &combined, value)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::rules::*;

    fn make_msg() -> MessageData {
        MessageData {
            id: "test_msg_1".to_string(),
            uid: 100,
            folder_path: "INBOX".to_string(),
            from_name: Some("Alice Smith".to_string()),
            from_email: "alice@example.com".to_string(),
            to_addresses: vec![AddressEntry {
                name: Some("Bob".to_string()),
                email: "bob@example.com".to_string(),
            }],
            cc_addresses: vec![],
            subject: Some("Meeting tomorrow at 3pm".to_string()),
            size: 5000,
            has_attachments: false,
            flags: vec!["seen".to_string()],
        }
    }

    #[test]
    fn test_from_contains() {
        let rule = FilterRule {
            id: "r1".to_string(),
            account_id: None,
            name: "test".to_string(),
            enabled: true,
            priority: 0,
            match_type: MatchType::All,
            conditions: vec![Condition {
                field: ConditionField::From,
                op: ConditionOp::Contains,
                value: "alice".to_string(),
            }],
            actions: vec![],
            stop_processing: false,
        };
        assert!(matches_message(&rule, &make_msg()));
    }

    #[test]
    fn test_subject_not_contains() {
        let rule = FilterRule {
            id: "r2".to_string(),
            account_id: None,
            name: "test".to_string(),
            enabled: true,
            priority: 0,
            match_type: MatchType::All,
            conditions: vec![Condition {
                field: ConditionField::Subject,
                op: ConditionOp::NotContains,
                value: "urgent".to_string(),
            }],
            actions: vec![],
            stop_processing: false,
        };
        assert!(matches_message(&rule, &make_msg()));
    }

    #[test]
    fn test_size_greater_than() {
        let rule = FilterRule {
            id: "r3".to_string(),
            account_id: None,
            name: "test".to_string(),
            enabled: true,
            priority: 0,
            match_type: MatchType::All,
            conditions: vec![Condition {
                field: ConditionField::Size,
                op: ConditionOp::GreaterThan,
                value: "1000".to_string(),
            }],
            actions: vec![],
            stop_processing: false,
        };
        assert!(matches_message(&rule, &make_msg()));
    }

    #[test]
    fn test_match_type_any() {
        let rule = FilterRule {
            id: "r4".to_string(),
            account_id: None,
            name: "test".to_string(),
            enabled: true,
            priority: 0,
            match_type: MatchType::Any,
            conditions: vec![
                Condition {
                    field: ConditionField::From,
                    op: ConditionOp::Contains,
                    value: "nobody@nowhere.com".to_string(),
                },
                Condition {
                    field: ConditionField::Subject,
                    op: ConditionOp::Contains,
                    value: "meeting".to_string(),
                },
            ],
            actions: vec![],
            stop_processing: false,
        };
        assert!(matches_message(&rule, &make_msg()));
    }

    #[test]
    fn test_match_type_all_fails() {
        let rule = FilterRule {
            id: "r5".to_string(),
            account_id: None,
            name: "test".to_string(),
            enabled: true,
            priority: 0,
            match_type: MatchType::All,
            conditions: vec![
                Condition {
                    field: ConditionField::From,
                    op: ConditionOp::Contains,
                    value: "alice".to_string(),
                },
                Condition {
                    field: ConditionField::Subject,
                    op: ConditionOp::Contains,
                    value: "nonexistent".to_string(),
                },
            ],
            actions: vec![],
            stop_processing: false,
        };
        assert!(!matches_message(&rule, &make_msg()));
    }

    #[test]
    fn test_apply_filters_stop_processing() {
        let rules = vec![
            FilterRule {
                id: "r_high".to_string(),
                account_id: None,
                name: "high priority".to_string(),
                enabled: true,
                priority: 10,
                match_type: MatchType::All,
                conditions: vec![Condition {
                    field: ConditionField::From,
                    op: ConditionOp::Contains,
                    value: "alice".to_string(),
                }],
                actions: vec![FilterAction::MarkRead],
                stop_processing: true,
            },
            FilterRule {
                id: "r_low".to_string(),
                account_id: None,
                name: "low priority".to_string(),
                enabled: true,
                priority: 1,
                match_type: MatchType::All,
                conditions: vec![Condition {
                    field: ConditionField::From,
                    op: ConditionOp::Contains,
                    value: "alice".to_string(),
                }],
                actions: vec![FilterAction::Delete],
                stop_processing: false,
            },
        ];

        let actions = apply_filters(&rules, &make_msg());
        // Only the high-priority rule's action should fire due to stop_processing
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], FilterAction::MarkRead));
    }

    #[test]
    fn test_has_attachment() {
        let mut msg = make_msg();
        msg.has_attachments = true;

        let rule = FilterRule {
            id: "r6".to_string(),
            account_id: None,
            name: "test".to_string(),
            enabled: true,
            priority: 0,
            match_type: MatchType::All,
            conditions: vec![Condition {
                field: ConditionField::HasAttachment,
                op: ConditionOp::Equals,
                value: "true".to_string(),
            }],
            actions: vec![],
            stop_processing: false,
        };
        assert!(matches_message(&rule, &msg));
    }

    #[test]
    fn test_regex_match() {
        let rule = FilterRule {
            id: "r7".to_string(),
            account_id: None,
            name: "test".to_string(),
            enabled: true,
            priority: 0,
            match_type: MatchType::All,
            conditions: vec![Condition {
                field: ConditionField::Subject,
                op: ConditionOp::MatchesRegex,
                value: r"[Mm]eeting.*\d+pm".to_string(),
            }],
            actions: vec![],
            stop_processing: false,
        };
        assert!(matches_message(&rule, &make_msg()));
    }
}
