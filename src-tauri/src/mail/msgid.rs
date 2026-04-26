/// Canonicalize an RFC 5322 Message-ID-shaped string for storage and lookup.
///
/// Many IMAP servers and parser code paths feed us slightly different shapes
/// for the same id: with or without surrounding angle brackets, with leading
/// whitespace from the wire, with stray internal spaces in folded headers.
/// All of those break the exact-match SQL we use to stitch threads
/// (`WHERE message_id = ?`). Normalize once at every entry point so the
/// stored form is always `<core>` with no whitespace anywhere.
///
/// Returns `None` for empty input or anything that contains nothing but
/// whitespace and brackets (`""`, `"<>"`, `"   "`).
pub fn normalize_message_id(s: &str) -> Option<String> {
    let core: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '<' && *c != '>')
        .collect();
    if core.is_empty() {
        return None;
    }
    Some(format!("<{}>", core))
}

#[cfg(test)]
mod tests {
    use super::normalize_message_id;

    #[test]
    fn wraps_unwrapped_id() {
        assert_eq!(normalize_message_id("abc@host"), Some("<abc@host>".into()));
    }

    #[test]
    fn keeps_already_wrapped_id() {
        assert_eq!(
            normalize_message_id("<abc@host>"),
            Some("<abc@host>".into())
        );
    }

    #[test]
    fn strips_leading_whitespace() {
        assert_eq!(
            normalize_message_id(" <abc@host>"),
            Some("<abc@host>".into())
        );
    }

    #[test]
    fn strips_trailing_whitespace() {
        assert_eq!(
            normalize_message_id("<abc@host> "),
            Some("<abc@host>".into())
        );
    }

    #[test]
    fn strips_internal_whitespace_from_folded_headers() {
        assert_eq!(
            normalize_message_id("<abc@\n host>"),
            Some("<abc@host>".into())
        );
    }

    #[test]
    fn returns_none_for_empty() {
        assert_eq!(normalize_message_id(""), None);
    }

    #[test]
    fn returns_none_for_whitespace_only() {
        assert_eq!(normalize_message_id("   "), None);
    }

    #[test]
    fn returns_none_for_empty_brackets() {
        assert_eq!(normalize_message_id("<>"), None);
    }
}
