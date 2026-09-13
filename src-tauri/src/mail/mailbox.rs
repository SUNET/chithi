//! Outbound single-mailbox parsing: RFC 5322 §§3.2/3.4, RFC 5321 §4.1.2,
//! and the UTF-8 extensions in RFCs 6531/6532. See ADR 0009.
//!
//! Header comments and display names are not envelope data. Parse them here,
//! then validate the semantic local-part and domain for SMTP/JMAP submission.
//! This is not the inbound, obsolete-syntax/recovery parser. Groups and lists
//! are valid header constructs but are not single compose mailbox items.

use lettre::{message::Mailbox, Address};

use crate::error::{Error, Result};

pub(crate) struct ParsedMailbox {
    pub mailbox: Mailbox,
    pub local_part: String,
    pub domain_key: String,
    /// Retain the syntactic distinction needed by JMAP Identity wildcards.
    pub quoted_local_part: bool,
}

pub(crate) fn parse_mailbox(value: &str) -> Result<Mailbox> {
    parse(value).map(|parsed| parsed.mailbox)
}

/// Parse the entire item, keeping semantic comparison data separate from the
/// minimally quoted wire spelling. Caller-owned input is never rewritten.
pub(crate) fn parse(value: &str) -> Result<ParsedMailbox> {
    let mut parser = Parser::new(value);
    parser.cfws().ok_or_else(invalid)?;

    let mut bare = parser.clone();
    if let Some(parsed) = bare.addr_spec() {
        if bare.cfws().is_some() && bare.peek().is_none() {
            return Ok(parsed);
        }
    }

    let name = if parser.peek() == Some('<') {
        None
    } else {
        Some(parser.phrase().ok_or_else(invalid)?)
    };
    parser.expect('<').ok_or_else(invalid)?;
    let mut parsed = parser.addr_spec().ok_or_else(invalid)?;
    parser.expect('>').ok_or_else(invalid)?;
    parser.cfws().ok_or_else(invalid)?;
    if parser.peek().is_some() {
        return Err(invalid());
    }
    parsed.mailbox.name = name;
    Ok(parsed)
}

fn invalid() -> Error {
    Error::Other("Expected one valid outbound mailbox (groups and lists are unsupported)".into())
}

#[derive(Clone)]
struct Parser<'a> {
    chars: std::str::Chars<'a>,
}

impl<'a> Parser<'a> {
    fn new(value: &'a str) -> Self {
        Self {
            chars: value.chars(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.clone().next()
    }

    fn expect(&mut self, expected: char) -> Option<()> {
        (self.chars.next()? == expected).then_some(())
    }

    fn take_while(&mut self, predicate: impl Fn(char) -> bool) -> &'a str {
        let remaining = self.chars.as_str();
        while self.peek().is_some_and(&predicate) {
            self.chars.next();
        }
        &remaining[..remaining.len() - self.chars.as_str().len()]
    }

    /// RFC 5322 folding removes only CRLF, never the significant WSP after it.
    fn fws(&mut self) -> Option<String> {
        let mut spaces = String::new();
        loop {
            spaces.push_str(self.take_while(is_wsp));
            if self.peek() != Some('\r') {
                break;
            }
            self.expect('\r')?;
            self.expect('\n')?;
            if !self.peek().is_some_and(is_wsp) {
                return None;
            }
        }
        Some(spaces)
    }

    fn cfws(&mut self) -> Option<bool> {
        let mut consumed = false;
        loop {
            consumed |= !self.fws()?.is_empty();
            if self.peek() != Some('(') {
                return Some(consumed);
            }
            consumed = true;
            self.comment()?;
        }
    }

    /// Iterative nesting prevents an input-controlled call-stack depth.
    fn comment(&mut self) -> Option<()> {
        self.expect('(')?;
        let mut depth = 1usize;
        while depth != 0 {
            self.fws()?;
            match self.chars.next()? {
                '(' => depth += 1,
                ')' => depth -= 1,
                '\\' => {
                    self.quoted_pair()?;
                }
                ch if ch.is_ascii_graphic() || !ch.is_ascii() => {}
                _ => return None,
            }
        }
        Some(())
    }

    fn quoted_pair(&mut self) -> Option<char> {
        self.chars
            .next()
            .filter(|ch| ch.is_ascii_graphic() || is_wsp(*ch))
    }

    fn quoted_string(&mut self) -> Option<String> {
        self.expect('"')?;
        let mut value = String::new();
        loop {
            value.push_str(&self.fws()?);
            match self.chars.next()? {
                '"' => return Some(value),
                '\\' => value.push(self.quoted_pair()?),
                ch if ch.is_ascii_graphic() || !ch.is_ascii() => value.push(ch),
                _ => return None,
            }
        }
    }

    fn phrase(&mut self) -> Option<String> {
        let mut name = String::new();
        let mut first = true;
        loop {
            let word = match self.peek()? {
                '"' => self.quoted_string()?,
                // Obsolete display-name periods are accepted as input only;
                // Lettre's header encoder emits a properly quoted phrase.
                '.' if !first => self.take_while(|ch| ch == '.').to_string(),
                _ => {
                    let word = self.take_while(is_atext);
                    if word.is_empty() {
                        return None;
                    }
                    word.to_string()
                }
            };
            first = false;
            name.push_str(&word);
            let separated = self.cfws()?;
            if self.peek() == Some('<') {
                return Some(name);
            }
            if separated {
                name.push(' ');
            }
        }
    }

    fn addr_spec(&mut self) -> Option<ParsedMailbox> {
        self.cfws()?;
        let quoted_local_part = self.peek() == Some('"');
        let local_part = if quoted_local_part {
            self.quoted_string()?
        } else {
            let local = self.take_while(|ch| is_atext(ch) || ch == '.');
            if !is_dot_atom(local) {
                return None;
            }
            local.to_string()
        };
        self.cfws()?;
        self.expect('@')?;
        self.cfws()?;
        let domain = if self.peek() == Some('[') {
            let start = self.chars.as_str();
            self.expect('[')?;
            self.take_while(|ch| ch != ']');
            self.expect(']')?;
            &start[..start.len() - self.chars.as_str().len()]
        } else {
            self.take_while(|ch| is_atext(ch) || ch == '.')
        };
        self.cfws()?;

        // Header quoted-strings may contain HTAB; SMTP quoted-pairSMTP and
        // qtextSMTP may not. RFC 6531 extends qtext with UTF8-non-ascii only.
        if local_part
            .chars()
            .any(|ch| ch.is_ascii() && !(' '..='~').contains(&ch))
        {
            return None;
        }
        let (domain, domain_key) = validated_domain(domain)?;
        let wire_local = minimal_local_part(&local_part);
        // Both parts have been independently validated against the outbound
        // grammar. Lettre's Address parser uses broader header-domain rules
        // and rejects some RFC-valid quoted/UTF-8 forms; it is a carrier here.
        let email = Address::new_dangerous(wire_local, domain);
        Some(ParsedMailbox {
            mailbox: Mailbox::new(None, email),
            local_part,
            domain_key,
            quoted_local_part,
        })
    }
}

fn is_wsp(ch: char) -> bool {
    matches!(ch, ' ' | '\t')
}

fn is_atext(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || "!#$%&'*+-/=?^_`{|}~".contains(ch) || !ch.is_ascii()
}

fn is_dot_atom(value: &str) -> bool {
    value
        .split('.')
        .all(|atom| !atom.is_empty() && atom.chars().all(is_atext))
}

fn minimal_local_part(local: &str) -> String {
    if is_dot_atom(local) {
        return local.to_string();
    }
    let mut quoted = String::from("\"");
    for ch in local.chars() {
        if matches!(ch, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    quoted
}

fn validated_domain(domain: &str) -> Option<(String, String)> {
    if let Some(literal) = domain.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let canonical = if let Some((tag, address)) = literal.split_once(':') {
            if !tag.eq_ignore_ascii_case("IPv6") {
                return None;
            }
            format!("[IPv6:{}]", address.parse::<std::net::Ipv6Addr>().ok()?)
        } else {
            let parts = literal
                .split('.')
                .map(|part| {
                    (part.len() <= 3 && part.bytes().all(|b| b.is_ascii_digit()))
                        .then(|| part.parse::<u8>().ok())
                        .flatten()
                })
                .collect::<Option<Vec<_>>>()?;
            let octets: [u8; 4] = parts.try_into().ok()?;
            format!("[{}]", std::net::Ipv4Addr::from(octets))
        };
        return Some((canonical.clone(), canonical));
    }
    let ascii = ascii_domain(domain)?;
    // Use IDNA, not URL host parsing: numeric DNS names such as 127.1 must not
    // be rewritten as an IPv4 address, and forbidden DNS labels must fail.
    let wire = if domain.is_ascii() {
        domain.to_string()
    } else {
        idna::domain_to_unicode(&ascii).0
    };
    Some((wire, ascii.to_ascii_lowercase()))
}

/// IDNA processing with SMTP's LDH label rules, not URL or domain-registration
/// policy. Interior double hyphens are valid in ordinary ASCII DNS labels.
pub(crate) fn ascii_domain(domain: &str) -> Option<String> {
    use idna::uts46::{AsciiDenyList, DnsLength, Hyphens, Uts46};

    Uts46::new()
        .to_ascii(
            domain.as_bytes(),
            AsciiDenyList::STD3,
            Hyphens::CheckFirstLast,
            DnsLength::Verify,
        )
        .ok()
        .map(|domain| domain.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_5321_local_parts_are_minimally_quoted_without_changing_meaning() {
        for (input, semantic, wire) in [
            ("Alice", "Alice", "Alice"),
            (r#""Ali\ce""#, "Alice", "Alice"),
            (r#""a\>b""#, "a>b", r#""a>b""#),
            (r#""a\"b""#, "a\"b", r#""a\"b""#),
            (r#""a\\b""#, "a\\b", r#""a\\b""#),
            (r#"" a  b ""#, " a  b ", r#"" a  b ""#),
            (r#""a\ b""#, "a b", r#""a b""#),
            (r#""a@b,c;d(e)<f>""#, "a@b,c;d(e)<f>", r#""a@b,c;d(e)<f>""#),
            (r#""a.b""#, "a.b", "a.b"),
            (r#"".a..b.""#, ".a..b.", r#"".a..b.""#),
            (r#""""#, "", r#""""#),
            (r#""δοκιμή""#, "δοκιμή", "δοκιμή"),
            (r#""a🦀b""#, "a🦀b", "a🦀b"),
        ] {
            for value in [
                format!("{input}@Example.COM"),
                format!("Name <{input}@Example.COM>"),
            ] {
                let parsed = parse(&value).unwrap_or_else(|err| panic!("{value:?}: {err}"));
                assert_eq!(parsed.local_part, semantic, "{value:?}");
                assert_eq!(
                    parsed.mailbox.email.to_string(),
                    format!("{wire}@Example.COM")
                );
                let reparsed = parse(&parsed.mailbox.to_string()).unwrap();
                assert_eq!(reparsed.local_part, semantic);
                assert_eq!(reparsed.mailbox, parsed.mailbox);
            }
        }
    }

    #[test]
    fn unicode_whitespace_is_smtputf8_local_data_not_outer_padding() {
        // RFC 6531 §3.3 adds UTF8-non-ascii to atext, not to FWS/CFWS.
        for ch in [
            '\u{85}', '\u{a0}', '\u{1680}', '\u{2003}', '\u{2028}', '\u{2029}', '\u{202f}',
            '\u{205f}', '\u{3000}', '\u{feff}',
        ] {
            let address = format!("{ch}alice{ch}@example.com");
            for input in [
                address.clone(),
                format!(" \t{address} \t"),
                format!("Name <{address}>"),
            ] {
                let parsed = parse(&input).unwrap();
                assert_eq!(parsed.local_part, format!("{ch}alice{ch}"));
                assert_eq!(parsed.mailbox.email.to_string(), address);
            }
        }
    }

    #[test]
    fn malformed_edge_whitespace_is_not_discarded_before_parsing() {
        for padding in [
            "\r", "\n", "\r\n", "\r ", "\n ", " \r", " \n", "\x0b", "\x0c",
        ] {
            for input in [
                format!("{padding}alice@example.com"),
                format!("alice@example.com{padding}"),
            ] {
                assert!(parse(&input).is_err(), "accepted {input:?}");
            }
        }
        for suffix in ['\u{a0}', '\u{2003}', '\u{202f}', '\u{3000}'] {
            assert!(parse(&format!("alice@example.com{suffix}")).is_err());
            assert!(parse(&format!("<alice@example.com>{suffix}")).is_err());
        }
        for input in [
            " \talice@example.com \t",
            "\r\n alice@example.com\r\n \t",
            " \t\r\n (comment) alice@example.com (comment)\r\n \t",
        ] {
            assert_eq!(
                parse(input).unwrap().mailbox.email.to_string(),
                "alice@example.com"
            );
        }
    }

    #[test]
    fn smtp_quoted_pair_and_qtext_cover_the_rfc_ascii_ranges() {
        // RFC 5321 §4.1.2: quoted-pairSMTP is backslash + %d32-126;
        // qtextSMTP excludes only DQUOTE/backslash from that printable range.
        for byte in 0..=127u8 {
            let ch = char::from(byte);
            let escaped = format!("\"a\\{ch}b\"@example.com");
            let unescaped = format!("\"ab{ch}\"@example.com");
            let printable = (32..=126).contains(&byte);
            let parsed = parse(&escaped);
            assert_eq!(parsed.is_ok(), printable, "quoted pair {byte}");
            if let Ok(parsed) = parsed {
                assert_eq!(parsed.local_part, format!("a{ch}b"));
            }
            assert_eq!(
                parse(&unescaped).is_ok(),
                printable && !matches!(ch, '"' | '\\'),
                "qtext {byte}"
            );
        }
        // SMTPUTF8 extends qtext/atext, not the quoted-pair production.
        assert!(parse(r#""a\é"@example.com"#).is_err());
    }

    #[test]
    fn every_printable_ascii_pair_preserves_its_semantic_value() {
        for first in ' '..='~' {
            for second in ' '..='~' {
                let semantic = format!("{first}{second}");
                let mut quoted = String::from("\"");
                for ch in [first, second] {
                    quoted.push('\\');
                    quoted.push(ch);
                }
                quoted.push_str("\"@example.com");
                let parsed = parse(&quoted).unwrap();
                assert_eq!(parsed.local_part, semantic);
                assert_eq!(
                    parse(&parsed.mailbox.to_string()).unwrap().local_part,
                    semantic
                );
            }
        }
    }

    #[test]
    fn header_comments_folding_and_quoted_delimiters_are_context_sensitive() {
        for (value, name, local) in [
            (
                r#""Display >, \"Name\"" <"a>b"@example.com>"#,
                Some("Display >, \"Name\""),
                "a>b",
            ),
            (
                "Name (a > (nested \\) comment)) <alice@example.com> (after)",
                Some("Name"),
                "alice",
            ),
            (
                "First\r\n Last <alice@example.com>",
                Some("First Last"),
                "alice",
            ),
            (
                "(before) alice (local) @ (domain) example.com (after)",
                None,
                "alice",
            ),
            ("<\"a\r\n  b\"@example.com>", None, "a  b"),
            (
                "Dr. Example <alice@example.com>",
                Some("Dr. Example"),
                "alice",
            ),
            (
                "Åsa Österberg <asa@example.com>",
                Some("Åsa Österberg"),
                "asa",
            ),
        ] {
            let parsed = parse(value).unwrap_or_else(|err| panic!("{value:?}: {err}"));
            assert_eq!(parsed.mailbox.name.as_deref(), name);
            assert_eq!(parsed.local_part, local);
        }
        let nested = format!("{}{}alice@example.com", "(".repeat(4096), ")".repeat(4096));
        assert!(parse(&nested).is_ok());
    }

    #[test]
    fn malformed_mailboxes_and_non_mailbox_header_constructs_fail_closed() {
        for value in [
            "",
            "invalid",
            "@example.com",
            "a@",
            ".a@example.com",
            "a..b@example.com",
            "a.@example.com",
            "a b@example.com",
            "alice@example.com,",
            "alice@example.com;",
            "Group: alice@example.com;",
            "Group:;",
            "a@example.com, B <b@example.com>",
            r#""Name", Other <"a>b"@example.com>"#,
            r#"Name <"a>b@example.com>"#,
            r#"Name <"a>b"@example.com"#,
            r#"Name <<"a>b"@example.com>>"#,
            r#"Name <"a>b"@example.com> trailing"#,
            "Name <a@example.com> (unterminated",
            "Name ) <a@example.com>",
            "Name\r\nBcc: secret@example.com <a@example.com>",
            "Name\n <a@example.com>",
            "Name\r <a@example.com>",
            "Name\0 <a@example.com>",
            "a@example.com\r\nBcc: secret@example.com",
            "\"a\tb\"@example.com",
            "\"a\\\tb\"@example.com",
            "\"a\n b\"@example.com",
            "\"a\r\nb\"@example.com",
        ] {
            assert!(parse(value).is_err(), "unexpected acceptance: {value:?}");
        }
    }

    #[test]
    fn domains_use_smtp_literals_and_idna_not_url_host_semantics() {
        for (domain, wire, key) in [
            ("Example.COM", "Example.COM", "example.com"),
            ("ab--cd.example", "ab--cd.example", "ab--cd.example"),
            (
                "ab--cd.bücher.example",
                "ab--cd.bücher.example",
                "ab--cd.xn--bcher-kva.example",
            ),
            ("bücher.example", "bücher.example", "xn--bcher-kva.example"),
            (
                "xn--bcher-kva.example",
                "xn--bcher-kva.example",
                "xn--bcher-kva.example",
            ),
            ("127.1", "127.1", "127.1"),
            ("192.0.2.1", "192.0.2.1", "192.0.2.1"),
            ("[192.000.002.001]", "[192.0.2.1]", "[192.0.2.1]"),
            (
                "[ipv6:2001:DB8:0:0::1]",
                "[IPv6:2001:db8::1]",
                "[IPv6:2001:db8::1]",
            ),
        ] {
            let parsed = parse(&format!("alice@{domain}")).unwrap();
            assert_eq!(parsed.mailbox.email.domain(), wire);
            assert_eq!(parsed.domain_key, key);
        }
        for domain in [
            "[not-an-ip]",
            "[unknown:foo]",
            "[2001:db8::1]",
            "2001:db8::1",
            "[256.0.0.1]",
            "[127.1]",
            "[IPv6:invalid]",
            "[IPv6:]",
            "example..com",
            "example.com.",
            "-example.com",
            "example-.com",
            "exam_ple.com",
            "exam!ple.com",
            "exam%70le.com",
            "[]",
            "[ ]",
            "example.com。",
            "example.com．",
            "example.com｡",
        ] {
            assert!(
                parse(&format!("alice@{domain}")).is_err(),
                "accepted {domain:?}"
            );
        }
    }

    #[test]
    fn empty_quoted_local_is_not_a_null_reverse_path() {
        // Published RFC 5321 uses *QcontentSMTP. Do not silently adopt the
        // unpublished 5321bis change to 1*QcontentSMTP (erratum 5414).
        assert_eq!(parse(r#"""@example.com"#).unwrap().local_part, "");
        assert!(parse("<>").is_err());
    }
}
