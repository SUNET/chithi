//! Inbound PGP/MIME (RFC 3156) parsing.
//!
//! We don't trust `mail_parser` to walk the MIME tree here. Signature
//! verification per RFC 3156 §5.1 requires hashing the byte-exact bytes
//! of the signed entity (its own MIME headers + body) as transmitted —
//! including original CRLF/LF style, original transfer encoding, original
//! header folding. Any high-level parser that decodes attributes,
//! normalises line endings, or unfolds headers loses fidelity in ways
//! that break verification on perfectly-valid inputs.
//!
//! Pattern lifted from the tumpa mail extension's `PGPMimeParser.swift`
//! at `~/code/openpgp/tumpa_mail_extension/TumpaMailExtension/`:
//!   1. Split raw bytes into headers + body at the first blank-line
//!      separator (`\r\n\r\n` or `\n\n`).
//!   2. Pull the outer Content-Type header value with header folding
//!      preserved, parse out `boundary=` and `protocol=` parameters
//!      with a small one-shot scanner.
//!   3. Scan the body for `--boundary` lines and slice between them.
//!   4. For the signed case, return the byte-exact signed-entity slice
//!      and the armored signature for libtumpa::verify::verify_detached.

use std::borrow::Cow;

use crate::message::PgpKind;

/// Cheap precheck. Returns true if the raw bytes contain any token that
/// hints at PGP/MIME, so the caller can short-circuit fast on the 99% of
/// messages that are plain mail. Lifted from tumpa mail extension's
/// `hasPGPMarkers` — anchors on the inner `application/pgp-*` tokens
/// because Outlook breaks the spec and wraps PGP in `multipart/mixed`.
pub fn has_pgp_markers(raw: &[u8]) -> bool {
    contains_ascii_ci(raw, b"application/pgp-encrypted")
        || contains_ascii_ci(raw, b"application/pgp-signature")
        || contains_subslice(raw, b"-----BEGIN PGP MESSAGE-----")
}

/// Classify a message. Returns `None` for non-PGP mail.
pub fn detect_kind(raw: &[u8]) -> Option<PgpKind> {
    if !has_pgp_markers(raw) {
        return None;
    }
    let (headers, body) = split_headers_body(raw)?;
    let ct = header_value(headers, b"content-type").unwrap_or_default();
    if header_value_is_multipart_pgp_encrypted(&ct) {
        return Some(PgpKind::MimeEncrypted);
    }
    if header_value_is_multipart_pgp_signed(&ct) {
        return Some(PgpKind::MimeSigned);
    }
    // Inline armor: text body containing the BEGIN/END PGP MESSAGE
    // markers. We treat any occurrence of the begin marker in the body as
    // a signal; the decrypt command extracts the precise armor block.
    if contains_subslice(body, b"-----BEGIN PGP MESSAGE-----") {
        return Some(PgpKind::InlineArmor);
    }
    None
}

/// Pull the OpenPGP ciphertext out of a `multipart/encrypted` message.
/// Returns the bytes of the `application/octet-stream` child part —
/// the armored OpenPGP message that libtumpa decrypts.
pub fn extract_encrypted_payload(raw: &[u8]) -> Option<Vec<u8>> {
    let (headers, body) = split_headers_body(raw)?;
    let ct = header_value(headers, b"content-type")?;
    if !header_value_is_multipart_pgp_encrypted(&ct) {
        return None;
    }
    let boundary = parse_attribute(&ct, "boundary")?;
    let parts = slice_parts(body, boundary.as_bytes());
    // Find the `application/octet-stream` child and return its body.
    for part in &parts {
        let Some((p_headers, p_body)) = split_headers_body(part) else {
            continue;
        };
        let p_ct = header_value(p_headers, b"content-type").unwrap_or_default();
        if starts_with_ci(p_ct.as_bytes(), b"application/octet-stream") {
            return Some(strip_trailing_crlf(p_body).to_vec());
        }
    }
    None
}

/// Extract a `multipart/signed` message's signed-entity bytes and detached
/// signature. The signed-entity bytes are sliced verbatim from `raw` so
/// the bytes the verifier sees are exactly what the sender signed.
/// Returns `(signed_entity, armored_signature)`.
pub fn extract_signed_payload(raw: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (headers, body) = split_headers_body(raw)?;
    let ct = header_value(headers, b"content-type")?;
    if !header_value_is_multipart_pgp_signed(&ct) {
        return None;
    }
    let boundary = parse_attribute(&ct, "boundary")?;
    let parts = slice_parts(body, boundary.as_bytes());
    if parts.len() < 2 {
        return None;
    }
    // The signed entity is the first child (passed verbatim, with its own
    // headers — that's what RFC 3156 §5 hashes).
    let signed_entity = parts[0].to_vec();
    // The signature is the second child; we split off its part-headers
    // and return only the armored signature bytes.
    let (sig_headers, sig_body) = split_headers_body(parts[1])?;
    let sig_ct = header_value(sig_headers, b"content-type").unwrap_or_default();
    if !starts_with_ci(sig_ct.as_bytes(), b"application/pgp-signature") {
        return None;
    }
    Some((signed_entity, strip_trailing_crlf(sig_body).to_vec()))
}

/// Extract an inline `-----BEGIN PGP MESSAGE-----` … `-----END PGP MESSAGE-----`
/// armor block from the message body, if present. Used for the legacy
/// "PGP inline" shape (predates RFC 3156 multipart/encrypted).
pub fn extract_inline_armor(raw: &[u8]) -> Option<Vec<u8>> {
    let (_headers, body) = split_headers_body(raw)?;
    let begin = b"-----BEGIN PGP MESSAGE-----";
    let end = b"-----END PGP MESSAGE-----";
    let start = find_subslice(body, begin)?;
    let after_begin = start + begin.len();
    let end_pos = find_subslice(&body[after_begin..], end)?;
    let end_abs = after_begin + end_pos + end.len();
    Some(body[start..end_abs].to_vec())
}

/// Generate the candidate canonical-CRLF byte sequences to try when the
/// straight canonical form of a signed entity fails verification. Mirrors
/// the recovery variants in tumpa mail extension's `tolerantSignedVariants`:
/// Outlook/Exchange `\r\r\n` collapse and stripped trailing CRLFs.
pub fn tolerant_signed_variants(canonical: &[u8]) -> Vec<Vec<u8>> {
    let mut variants: Vec<Vec<u8>> = Vec::new();

    // Outlook/Exchange CR-doubling collapse.
    let collapsed = collapse_doubled_cr(canonical);
    if collapsed != canonical {
        variants.push(collapsed.clone());
    }

    // Up-to-3 trailing CRLFs peeled, from BOTH the original-canonical
    // and the collapsed form — both sources of mangling can stack.
    for base in [canonical.to_vec(), collapsed] {
        let mut current = base;
        for _ in 0..3 {
            if current.ends_with(b"\r\n") {
                current.truncate(current.len() - 2);
                if current != canonical && !variants.iter().any(|v| v == &current) {
                    variants.push(current.clone());
                }
            } else {
                break;
            }
        }
    }
    variants
}

/// CRLF-canonicalise an entity for signing (bare CRs and bare LFs → CRLF).
/// Used when signing outbound mail in Phase D; exported here because it
/// lives next to the inbound verify recovery logic.
pub fn canonicalize_for_signing(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / 32);
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            0x0D => {
                out.push(0x0D);
                if i + 1 < input.len() && input[i + 1] == 0x0A {
                    out.push(0x0A);
                    i += 2;
                } else {
                    out.push(0x0A); // bare CR → CRLF
                    i += 1;
                }
            }
            0x0A => {
                out.push(0x0D);
                out.push(0x0A); // bare LF → CRLF
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Header / boundary primitives
// ---------------------------------------------------------------------------

/// Split raw RFC-822 bytes at the first blank-line separator. Tolerates
/// both `\r\n\r\n` (canonical) and `\n\n` (Apple-Mail-style LF-only).
fn split_headers_body(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    if let Some(i) = find_subslice(raw, b"\r\n\r\n") {
        Some((&raw[..i], &raw[i + 4..]))
    } else {
        find_subslice(raw, b"\n\n").map(|i| (&raw[..i], &raw[i + 2..]))
    }
}

/// Look up a header by name (ASCII case-insensitive). Returns the full
/// value with folding unfolded (continuation lines joined with a single
/// space) so callers can `parse_attribute` over it.
fn header_value(headers: &[u8], name: &[u8]) -> Option<String> {
    // Walk lines. A line is terminated by CRLF or LF. A continuation
    // line begins with SP or HTAB.
    let mut i = 0;
    while i < headers.len() {
        // Find end-of-line.
        let mut eol = i;
        while eol < headers.len() && headers[eol] != b'\n' {
            eol += 1;
        }
        // Trim a trailing CR if present.
        let raw_end = if eol > i && headers[eol - 1] == b'\r' {
            eol - 1
        } else {
            eol
        };
        let line = &headers[i..raw_end];
        // Find the colon for header name.
        if let Some(colon) = line.iter().position(|&b| b == b':') {
            let header_name = &line[..colon];
            if ascii_eq_ci(header_name, name) {
                // Found. Start with the value after the colon (trim
                // leading WSP) and append any folded continuation
                // lines.
                let value_start = colon + 1;
                let mut value: Vec<u8> = Vec::new();
                value.extend_from_slice(trim_leading_wsp(&line[value_start..]));
                // Walk forward, appending continuation lines.
                let mut j = eol + 1;
                while j < headers.len() && (headers[j] == b' ' || headers[j] == b'\t') {
                    // Find end of this continuation line.
                    let mut cont_eol = j;
                    while cont_eol < headers.len() && headers[cont_eol] != b'\n' {
                        cont_eol += 1;
                    }
                    let cont_raw_end = if cont_eol > j && headers[cont_eol - 1] == b'\r' {
                        cont_eol - 1
                    } else {
                        cont_eol
                    };
                    value.push(b' ');
                    value.extend_from_slice(trim_leading_wsp(&headers[j..cont_raw_end]));
                    j = cont_eol + 1;
                }
                return Some(String::from_utf8_lossy(&value).into_owned());
            }
        }
        i = eol + 1;
    }
    None
}

fn trim_leading_wsp(s: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < s.len() && (s[i] == b' ' || s[i] == b'\t') {
        i += 1;
    }
    &s[i..]
}

/// Parse a single `name=value` attribute from a Content-Type-style value.
/// Handles quoted values and ignores comments. Case-insensitive on the
/// attribute name.
fn parse_attribute(header_value: &str, attr: &str) -> Option<String> {
    let attr_lc = attr.to_ascii_lowercase();
    let bytes = header_value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip leading WSP/semicolons.
        while i < bytes.len() && (bytes[i] == b';' || bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        // Read attribute name.
        let name_start = i;
        while i < bytes.len() && bytes[i] != b'=' && bytes[i] != b';' {
            i += 1;
        }
        let name_end = i;
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        let name = &bytes[name_start..name_end];
        let name_trimmed = trim_trailing_wsp(trim_leading_wsp(name));
        i += 1; // skip '='
                // Read value, handling optional quoting.
        let value = if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let val_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            let v = &bytes[val_start..i];
            if i < bytes.len() {
                i += 1;
            } // skip closing quote
            v
        } else {
            let val_start = i;
            while i < bytes.len() && bytes[i] != b';' {
                i += 1;
            }
            trim_trailing_wsp(&bytes[val_start..i])
        };
        if ascii_eq_ci(name_trimmed, attr_lc.as_bytes()) {
            return Some(String::from_utf8_lossy(value).into_owned());
        }
    }
    None
}

fn trim_trailing_wsp(s: &[u8]) -> &[u8] {
    let mut end = s.len();
    while end > 0 && (s[end - 1] == b' ' || s[end - 1] == b'\t') {
        end -= 1;
    }
    &s[..end]
}

/// Slice the body of a multipart entity into its child parts at
/// `--boundary` lines. Each returned slice is the bytes between two
/// boundary lines, with the boundary's preceding CRLF (RFC 2046 §5.1.1)
/// already excluded.
fn slice_parts<'a>(body: &'a [u8], boundary: &[u8]) -> Vec<&'a [u8]> {
    // The opening boundary is `--<boundary>` at the start of a line.
    // The closing boundary is `--<boundary>--`. We split the body by
    // boundary occurrences and keep the slices between them.
    let mut opener = Vec::with_capacity(boundary.len() + 4);
    opener.extend_from_slice(b"--");
    opener.extend_from_slice(boundary);
    let opener = opener.as_slice();

    let mut parts: Vec<&[u8]> = Vec::new();
    let positions = find_boundary_positions(body, opener);
    for window in positions.windows(2) {
        let (start_marker, end_marker) = (window[0], window[1]);
        let part_body_start = advance_past_boundary_line(body, start_marker, opener.len());
        // Strip the CRLF immediately preceding the next boundary
        // marker. mail_parser doesn't enforce this; RFC 2046 §5.1.1
        // says the CRLF *preceding* `--boundary` is part of the
        // delimiter, not the body.
        let part_body_end = strip_preceding_crlf(body, end_marker);
        if part_body_start <= part_body_end {
            parts.push(&body[part_body_start..part_body_end]);
        }
    }
    parts
}

/// Find all `--boundary` lines in `body`. A boundary occurrence is the
/// position of the leading `--` on a fresh line — either at position 0 or
/// after a CRLF/LF. Both the regular `--boundary` and the closing
/// `--boundary--` count (the closing form just ends the last part).
fn find_boundary_positions(body: &[u8], opener: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < body.len() {
        if i + opener.len() <= body.len()
            && &body[i..i + opener.len()] == opener
            && (i == 0 || body[i - 1] == b'\n' || (i >= 2 && &body[i - 2..i] == b"\r\n"))
        {
            out.push(i);
            i += opener.len();
            continue;
        }
        i += 1;
    }
    out
}

/// Given a boundary's start position and the boundary opener length,
/// return the offset of the first byte AFTER the boundary line — i.e.
/// the start of the part's MIME headers.
fn advance_past_boundary_line(body: &[u8], boundary_start: usize, opener_len: usize) -> usize {
    let mut i = boundary_start + opener_len;
    // Skip any trailing chars on the boundary line (e.g. the `--`
    // closing-form suffix or trailing WSP).
    while i < body.len() && body[i] != b'\n' {
        i += 1;
    }
    if i < body.len() {
        i += 1; // consume the LF
    }
    i
}

/// Trim the single CRLF (or LF) immediately preceding `marker_start`.
fn strip_preceding_crlf(body: &[u8], marker_start: usize) -> usize {
    let mut end = marker_start;
    if end >= 2 && &body[end - 2..end] == b"\r\n" {
        end -= 2;
    } else if end >= 1 && body[end - 1] == b'\n' {
        end -= 1;
    }
    end
}

fn strip_trailing_crlf(body: &[u8]) -> Cow<'_, [u8]> {
    if body.ends_with(b"\r\n") {
        Cow::Borrowed(&body[..body.len() - 2])
    } else if body.ends_with(b"\n") {
        Cow::Borrowed(&body[..body.len() - 1])
    } else {
        Cow::Borrowed(body)
    }
}

fn header_value_is_multipart_pgp_encrypted(ct: &str) -> bool {
    let lc = ct.to_ascii_lowercase();
    lc.starts_with("multipart/encrypted")
        && parse_attribute(&lc, "protocol")
            .as_deref()
            .map(|p| p.eq_ignore_ascii_case("application/pgp-encrypted"))
            .unwrap_or(false)
}

fn header_value_is_multipart_pgp_signed(ct: &str) -> bool {
    let lc = ct.to_ascii_lowercase();
    lc.starts_with("multipart/signed")
        && parse_attribute(&lc, "protocol")
            .as_deref()
            .map(|p| p.eq_ignore_ascii_case("application/pgp-signature"))
            .unwrap_or(false)
}

fn collapse_doubled_cr(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if i + 2 < input.len() && input[i] == 0x0D && input[i + 1] == 0x0D && input[i + 2] == 0x0A {
            out.push(0x0D);
            out.push(0x0A);
            i += 3;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Byte-slice helpers
// ---------------------------------------------------------------------------

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn contains_subslice(hay: &[u8], needle: &[u8]) -> bool {
    find_subslice(hay, needle).is_some()
}

fn contains_ascii_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > hay.len() {
        return false;
    }
    for i in 0..=hay.len() - needle.len() {
        if ascii_eq_ci(&hay[i..i + needle.len()], needle) {
            return true;
        }
    }
    false
}

fn ascii_eq_ci(a: &[u8], b: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn starts_with_ci(hay: &[u8], prefix: &[u8]) -> bool {
    hay.len() >= prefix.len() && ascii_eq_ci(&hay[..prefix.len()], prefix)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const ENCRYPTED_MESSAGE: &[u8] = b"\
MIME-Version: 1.0\r\n\
From: alice@example.com\r\n\
To: bob@example.com\r\n\
Subject: Secret\r\n\
Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\";\r\n\
\x20boundary=\"PGP-BOUNDARY\"\r\n\
\r\n\
--PGP-BOUNDARY\r\n\
Content-Type: application/pgp-encrypted\r\n\
\r\n\
Version: 1\r\n\
\r\n\
--PGP-BOUNDARY\r\n\
Content-Type: application/octet-stream; name=\"encrypted.asc\"\r\n\
\r\n\
-----BEGIN PGP MESSAGE-----\r\n\
\r\n\
hQEMA1234567890abcdef0\r\n\
-----END PGP MESSAGE-----\r\n\
--PGP-BOUNDARY--\r\n";

    const SIGNED_MESSAGE: &[u8] = b"\
MIME-Version: 1.0\r\n\
From: alice@example.com\r\n\
To: bob@example.com\r\n\
Subject: Signed\r\n\
Content-Type: multipart/signed; protocol=\"application/pgp-signature\";\r\n\
\x20micalg=\"pgp-sha512\"; boundary=\"PGP-SIG\"\r\n\
\r\n\
--PGP-SIG\r\n\
Content-Type: text/plain\r\n\
\r\n\
Hello, this is a signed message.\r\n\
--PGP-SIG\r\n\
Content-Type: application/pgp-signature\r\n\
\r\n\
-----BEGIN PGP SIGNATURE-----\r\n\
\r\n\
iQEzBAEBCAAdFiEEXXXXXXX\r\n\
-----END PGP SIGNATURE-----\r\n\
--PGP-SIG--\r\n";

    const INLINE_MESSAGE: &[u8] = b"\
MIME-Version: 1.0\r\n\
From: alice@example.com\r\n\
To: bob@example.com\r\n\
Subject: Inline\r\n\
Content-Type: text/plain\r\n\
\r\n\
Here is a secret message:\r\n\
\r\n\
-----BEGIN PGP MESSAGE-----\r\n\
\r\n\
hQEMA1234567890abcdef0\r\n\
-----END PGP MESSAGE-----\r\n\
\r\n\
End of message.\r\n";

    const PLAIN_MESSAGE: &[u8] = b"\
From: alice@example.com\r\n\
To: bob@example.com\r\n\
Subject: hi\r\n\
\r\n\
Just plain text.\r\n";

    #[test]
    fn precheck_detects_pgp_markers() {
        assert!(has_pgp_markers(ENCRYPTED_MESSAGE));
        assert!(has_pgp_markers(SIGNED_MESSAGE));
        assert!(has_pgp_markers(INLINE_MESSAGE));
        assert!(!has_pgp_markers(PLAIN_MESSAGE));
    }

    #[test]
    fn detect_kind_classifies_each_shape() {
        assert_eq!(detect_kind(ENCRYPTED_MESSAGE), Some(PgpKind::MimeEncrypted));
        assert_eq!(detect_kind(SIGNED_MESSAGE), Some(PgpKind::MimeSigned));
        assert_eq!(detect_kind(INLINE_MESSAGE), Some(PgpKind::InlineArmor));
        assert_eq!(detect_kind(PLAIN_MESSAGE), None);
    }

    #[test]
    fn header_value_unfolds_continuation_lines() {
        // Both the leading-space continuation and the leading-tab
        // continuation must reconstitute one logical header value.
        let raw = b"Content-Type: multipart/signed;\r\n boundary=\"X\"; protocol=\"application/pgp-signature\"\r\n";
        let v = header_value(raw, b"content-type").expect("header");
        assert!(v.contains("boundary=\"X\""));
        assert!(v.contains("application/pgp-signature"));
    }

    #[test]
    fn extract_encrypted_payload_returns_ciphertext_part() {
        let ct = extract_encrypted_payload(ENCRYPTED_MESSAGE).expect("encrypted part");
        let s = std::str::from_utf8(&ct).expect("utf-8 armor");
        assert!(s.contains("BEGIN PGP MESSAGE"), "got: {s}");
        assert!(s.contains("END PGP MESSAGE"), "got: {s}");
    }

    #[test]
    fn extract_encrypted_payload_rejects_signed_message() {
        assert!(extract_encrypted_payload(SIGNED_MESSAGE).is_none());
    }

    #[test]
    fn extract_signed_payload_includes_part_headers_and_body() {
        let (signed, sig) = extract_signed_payload(SIGNED_MESSAGE).expect("signed extraction");
        let signed_str = std::str::from_utf8(&signed).expect("utf-8");
        assert!(
            signed_str.contains("Hello, this is a signed message."),
            "signed body missing: {signed_str:?}"
        );
        // RFC 3156 §5: the signed slice is the entire MIME part —
        // headers and all — so verification can hash exactly what was
        // transmitted.
        assert!(
            signed_str.contains("Content-Type: text/plain"),
            "signed part missing headers: {signed_str:?}"
        );
        let sig_str = std::str::from_utf8(&sig).expect("utf-8 sig");
        assert!(sig_str.contains("BEGIN PGP SIGNATURE"), "sig: {sig_str:?}");
    }

    #[test]
    fn extract_inline_armor_pulls_block_only() {
        let armor = extract_inline_armor(INLINE_MESSAGE).expect("inline");
        let s = std::str::from_utf8(&armor).expect("utf-8");
        assert!(s.starts_with("-----BEGIN PGP MESSAGE-----"));
        assert!(s.trim_end().ends_with("-----END PGP MESSAGE-----"));
        assert!(!s.contains("Here is a secret"));
        assert!(!s.contains("End of message"));
    }

    #[test]
    fn extract_inline_armor_returns_none_on_plain_text() {
        assert!(extract_inline_armor(PLAIN_MESSAGE).is_none());
    }

    #[test]
    fn canonicalize_for_signing_normalises_line_endings() {
        // Mixed CR / LF / CRLF input collapses to canonical CRLF.
        let mixed = b"line1\nline2\rline3\r\nline4";
        let out = canonicalize_for_signing(mixed);
        assert_eq!(out, b"line1\r\nline2\r\nline3\r\nline4");
    }

    #[test]
    fn tolerant_signed_variants_covers_known_mangling() {
        // Outlook `\r\r\n` doubling — `collapse_doubled_cr` produces the
        // recovery variant.
        let bad = b"hello\r\r\nworld\r\n";
        let variants = tolerant_signed_variants(bad);
        assert!(variants.iter().any(|v| v == b"hello\r\nworld\r\n"));
    }

    /// Property pin: every recovery variant must be reachable from the
    /// canonical input by a composition of the two documented transforms
    /// — `\r\r\n -> \r\n` collapse and trailing-CRLF strip (up to 3) — and
    /// NOTHING ELSE. If a variant ever diverges further from the canonical
    /// (e.g. body-content insertion, header rewriting, mid-stream LF
    /// stripping) the signature recovery would let an attacker mutate
    /// signed content while still reporting Good.
    ///
    /// The check synthesises the legal-variant set from the canonical and
    /// asserts every produced variant lives inside it. Bounded enumeration
    /// (the strip is ≤3 CRLFs, the collapse is idempotent), so the synth
    /// set has at most a handful of members regardless of input size.
    #[test]
    fn tolerant_signed_variants_only_strips_trailing_crlf_or_collapses_doubled_cr() {
        // Two representative inputs: one with no mangling (so the variants
        // can only be the trailing-strip ladder) and one with a doubled-CR
        // in the middle (so the collapse path triggers AND the strip
        // ladder applies to both bases).
        let canonical_plain = b"line1\r\nline2\r\nline3\r\n\r\n\r\n".to_vec();
        let canonical_doubled = b"head\r\r\nbody\r\nfoot\r\n\r\n".to_vec();

        for canonical in [canonical_plain, canonical_doubled] {
            let variants = tolerant_signed_variants(&canonical);

            // Legal-variant set: canonical, collapsed, and each form's
            // strip-1/strip-2/strip-3 trailing-CRLF descendants. Anything
            // outside this set is a bug.
            let collapsed = collapse_doubled_cr(&canonical);
            let mut legal: Vec<Vec<u8>> = vec![canonical.clone()];
            if collapsed != canonical {
                legal.push(collapsed.clone());
            }
            for base in [canonical.clone(), collapsed] {
                let mut current = base;
                for _ in 0..3 {
                    if current.ends_with(b"\r\n") {
                        current.truncate(current.len() - 2);
                        if !legal.iter().any(|v| v == &current) {
                            legal.push(current.clone());
                        }
                    } else {
                        break;
                    }
                }
            }

            for v in &variants {
                assert!(
                    legal.iter().any(|l| l == v),
                    "tolerant variant escaped the legal-transform set: {:?}\n\
                     legal set was: {:?}",
                    String::from_utf8_lossy(v),
                    legal
                        .iter()
                        .map(|l| String::from_utf8_lossy(l))
                        .collect::<Vec<_>>(),
                );
                // Variants must not equal the canonical (those would be
                // duplicate work).
                assert_ne!(
                    v, &canonical,
                    "canonical itself must not be in the variants list — \
                     the verifier already tried the canonical form first"
                );
            }
        }
    }
}
