//! Shared types for server-side mail search across IMAP / JMAP / Graph.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct SearchQuery {
    pub text: String,
    #[serde(default)]
    pub fields: SearchFields,
    #[serde(default)]
    pub has_attachment: Option<bool>,
    #[serde(default)]
    pub since_days: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchFields {
    pub subject: bool,
    pub from: bool,
    pub to: bool,
    pub body: bool,
}

impl Default for SearchFields {
    fn default() -> Self {
        Self {
            subject: true,
            from: true,
            to: true,
            body: true,
        }
    }
}

impl SearchFields {
    pub fn all_enabled(&self) -> bool {
        self.subject && self.from && self.to && self.body
    }

    pub fn any_enabled(&self) -> bool {
        self.subject || self.from || self.to || self.body
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub account_id: String,
    pub folder_path: String,
    pub uid: Option<u32>,
    pub message_id: Option<String>,
    pub backend_id: String,
    pub subject: String,
    pub from_name: Option<String>,
    pub from_email: Option<String>,
    pub date: i64,
    pub snippet: Option<String>,
}

/// Build an IMAP `UID SEARCH` argument from a query.
///
/// Inside an IMAP quoted string only `"` and `\` are special and are escaped
/// with a leading backslash (RFC 3501 §4.3). Control characters (CR, LF, NUL)
/// are forbidden in quoted strings and would break command framing if a user
/// query contained them, so they are stripped before quoting.
/// Returns `None` if no fields are enabled or the text is empty.
pub fn build_imap_search(query: &SearchQuery) -> Option<String> {
    let text = query.text.trim();
    if text.is_empty() || !query.fields.any_enabled() {
        return None;
    }
    let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
    if cleaned.is_empty() {
        return None;
    }
    let escaped = cleaned.replace('\\', "\\\\").replace('"', "\\\"");

    let mut keys: Vec<String> = Vec::new();
    if query.fields.subject {
        keys.push(format!("SUBJECT \"{}\"", escaped));
    }
    if query.fields.from {
        keys.push(format!("FROM \"{}\"", escaped));
    }
    if query.fields.to {
        keys.push(format!("TO \"{}\"", escaped));
    }
    if query.fields.body {
        keys.push(format!("BODY \"{}\"", escaped));
    }

    let combined = match keys.len() {
        1 => keys.remove(0),
        n => {
            let mut acc = keys.pop().unwrap();
            for _ in 1..n {
                let next = keys.pop().unwrap();
                acc = format!("OR {} {}", next, acc);
            }
            acc
        }
    };

    let mut full = format!("CHARSET UTF-8 {}", combined);
    if let Some(days) = query.since_days {
        if days > 0 {
            full.push_str(&format!(" SINCE {}", imap_since_date(days)));
        }
    }
    Some(full)
}

fn imap_since_date(days_ago: u32) -> String {
    let now = chrono::Utc::now();
    let dt = now - chrono::Duration::days(days_ago as i64);
    dt.format("%d-%b-%Y").to_string()
}

/// Build a JMAP `Email/query` FilterCondition for a search query.
///
/// When all fields are enabled, uses the single `text` filter (RFC 8621
/// defines it to cover headers + body). Otherwise OR-combines individual
/// per-field filters.
pub fn build_jmap_filter(query: &SearchQuery) -> Option<serde_json::Value> {
    let text = query.text.trim();
    if text.is_empty() {
        return None;
    }
    let mut conditions: Vec<serde_json::Value> = Vec::new();

    if query.fields.all_enabled() {
        conditions.push(serde_json::json!({ "text": text }));
    } else {
        let mut field_conds: Vec<serde_json::Value> = Vec::new();
        if query.fields.subject {
            field_conds.push(serde_json::json!({ "subject": text }));
        }
        if query.fields.from {
            field_conds.push(serde_json::json!({ "from": text }));
        }
        if query.fields.to {
            field_conds.push(serde_json::json!({ "to": text }));
        }
        if query.fields.body {
            field_conds.push(serde_json::json!({ "body": text }));
        }
        if field_conds.is_empty() {
            return None;
        }
        if field_conds.len() == 1 {
            conditions.push(field_conds.remove(0));
        } else {
            conditions.push(serde_json::json!({
                "operator": "OR",
                "conditions": field_conds,
            }));
        }
    }

    if let Some(true) = query.has_attachment {
        conditions.push(serde_json::json!({ "hasAttachment": true }));
    }
    if let Some(days) = query.since_days {
        if days > 0 {
            let after = chrono::Utc::now() - chrono::Duration::days(days as i64);
            conditions.push(serde_json::json!({ "after": after.to_rfc3339() }));
        }
    }

    if conditions.len() == 1 {
        Some(conditions.remove(0))
    } else {
        Some(serde_json::json!({
            "operator": "AND",
            "conditions": conditions,
        }))
    }
}

/// Build a Microsoft Graph `$search` KQL expression for a search query.
///
/// Returns the bare KQL — the caller must wrap it as `$search="..."` exactly
/// once. Quotes inside the user's text are stripped because KQL field-value
/// quoting cannot be reliably escaped through a URL query parameter.
///
/// When all fields are enabled, omits field prefixes (Graph's default search
/// covers subject + body + from). For multi-word values restricted to a
/// specific field, wraps the value in parens so each term stays scoped to
/// that field (e.g. `subject:(tax 2024)` matches when both `tax` and `2024`
/// appear in the subject).
pub fn build_graph_kql(query: &SearchQuery) -> Option<String> {
    let text = query.text.trim();
    if text.is_empty() || !query.fields.any_enabled() {
        return None;
    }
    let safe = text.replace('"', "");
    if safe.is_empty() {
        return None;
    }

    let core = if query.fields.all_enabled() {
        safe.clone()
    } else {
        let value = if safe.contains(' ') {
            format!("({})", safe)
        } else {
            safe.clone()
        };
        let mut parts: Vec<String> = Vec::new();
        if query.fields.subject {
            parts.push(format!("subject:{}", value));
        }
        if query.fields.from {
            parts.push(format!("from:{}", value));
        }
        if query.fields.to {
            parts.push(format!("to:{}", value));
        }
        if query.fields.body {
            parts.push(format!("body:{}", value));
        }
        if parts.is_empty() {
            return None;
        }
        parts.join(" OR ")
    };

    let mut kql = core;
    if let Some(true) = query.has_attachment {
        kql = format!("({}) AND hasAttachments:true", kql);
    }
    Some(kql)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str) -> SearchQuery {
        SearchQuery {
            text: text.into(),
            fields: SearchFields::default(),
            has_attachment: None,
            since_days: None,
        }
    }

    fn q_fields(text: &str, fields: SearchFields) -> SearchQuery {
        SearchQuery {
            text: text.into(),
            fields,
            has_attachment: None,
            since_days: None,
        }
    }

    #[test]
    fn imap_all_fields_or_combines() {
        let s = build_imap_search(&q("foo")).unwrap();
        assert!(s.starts_with("CHARSET UTF-8 OR "));
        assert!(s.contains("SUBJECT \"foo\""));
        assert!(s.contains("FROM \"foo\""));
        assert!(s.contains("TO \"foo\""));
        assert!(s.contains("BODY \"foo\""));
    }

    #[test]
    fn imap_single_field_no_or() {
        let s = build_imap_search(&q_fields(
            "foo",
            SearchFields {
                subject: true,
                from: false,
                to: false,
                body: false,
            },
        ))
        .unwrap();
        assert_eq!(s, "CHARSET UTF-8 SUBJECT \"foo\"");
    }

    #[test]
    fn imap_quote_escape() {
        let s = build_imap_search(&q("a\"b")).unwrap();
        assert!(s.contains("\\\""));
    }

    #[test]
    fn imap_empty_returns_none() {
        assert!(build_imap_search(&q("")).is_none());
        assert!(build_imap_search(&q("   ")).is_none());
    }

    #[test]
    fn imap_strips_control_chars() {
        let s = build_imap_search(&q("foo\r\nA001 LOGOUT")).unwrap();
        assert!(!s.contains('\r'));
        assert!(!s.contains('\n'));
        assert!(s.contains("fooA001 LOGOUT"));
    }

    #[test]
    fn imap_only_control_chars_returns_none() {
        assert!(build_imap_search(&q("\r\n\t")).is_none());
    }

    #[test]
    fn imap_no_fields_returns_none() {
        let none_fields = SearchFields {
            subject: false,
            from: false,
            to: false,
            body: false,
        };
        assert!(build_imap_search(&q_fields("foo", none_fields)).is_none());
    }

    #[test]
    fn jmap_all_fields_uses_text_shortcut() {
        let f = build_jmap_filter(&q("foo")).unwrap();
        assert_eq!(f, serde_json::json!({ "text": "foo" }));
    }

    #[test]
    fn jmap_partial_uses_or_operator() {
        let f = build_jmap_filter(&q_fields(
            "foo",
            SearchFields {
                subject: true,
                from: true,
                to: false,
                body: false,
            },
        ))
        .unwrap();
        assert_eq!(f["operator"], "OR");
        assert_eq!(f["conditions"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn jmap_attachment_wraps_in_and() {
        let mut query = q("foo");
        query.has_attachment = Some(true);
        let f = build_jmap_filter(&query).unwrap();
        assert_eq!(f["operator"], "AND");
    }

    #[test]
    fn graph_all_fields_bare() {
        let s = build_graph_kql(&q("foo")).unwrap();
        assert_eq!(s, "foo");
    }

    #[test]
    fn graph_partial_uses_prefixed_or() {
        let s = build_graph_kql(&q_fields(
            "foo",
            SearchFields {
                subject: true,
                body: true,
                from: false,
                to: false,
            },
        ))
        .unwrap();
        assert_eq!(s, "subject:foo OR body:foo");
    }

    #[test]
    fn graph_partial_multi_word_uses_parens() {
        let s = build_graph_kql(&q_fields(
            "tax 2024",
            SearchFields {
                subject: true,
                body: false,
                from: false,
                to: false,
            },
        ))
        .unwrap();
        assert_eq!(s, "subject:(tax 2024)");
    }

    #[test]
    fn graph_strips_quotes() {
        let s = build_graph_kql(&q("a\"b")).unwrap();
        assert_eq!(s, "ab");
    }

    #[test]
    fn graph_attachment_wraps_in_and() {
        let mut query = q("foo");
        query.has_attachment = Some(true);
        let s = build_graph_kql(&query).unwrap();
        assert_eq!(s, "(foo) AND hasAttachments:true");
    }
}
