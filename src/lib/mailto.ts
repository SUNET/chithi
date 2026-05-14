import type { ComposeParams } from "./compose-window";

/// Parse a `mailto:` URI into the compose-window's parameter shape per
/// RFC 6068. Returns null when the input doesn't look like mailto: so
/// callers can fall back to their generic link handling.
///
/// Supported:
///   mailto:foo@example.com
///   mailto:foo,bar@example.com
///   mailto:?to=foo&cc=bar&bcc=baz&subject=Hi&body=Hello
///   mailto:foo?subject=Hi%20there&body=Line%201%0ALine%202
///
/// `to` addresses from the path and from the query are merged. Newlines
/// in the body arrive as %0A and survive decodeURIComponent intact.
export function parseMailto(href: string): ComposeParams | null {
  if (!href.slice(0, 7).toLowerCase().startsWith("mailto:")) return null;

  const afterScheme = href.slice(7);
  const qIndex = afterScheme.indexOf("?");
  const path = qIndex === -1 ? afterScheme : afterScheme.slice(0, qIndex);
  const queryStr = qIndex === -1 ? "" : afterScheme.slice(qIndex + 1);

  const toFromPath = path
    .split(",")
    .map((s) => safeDecode(s.trim()))
    .filter(Boolean);

  const params = new URLSearchParams(queryStr);
  const merged = mergeCommaList(toFromPath, params.getAll("to"));

  const result: ComposeParams = {};
  if (merged) result.to = merged;
  const cc = mergeCommaList([], params.getAll("cc"));
  if (cc) result.cc = cc;
  const bcc = mergeCommaList([], params.getAll("bcc"));
  if (bcc) result.bcc = bcc;
  const subject = params.get("subject");
  if (subject) result.subject = subject;
  const body = params.get("body");
  if (body) result.body = body;
  return result;
}

function safeDecode(s: string): string {
  try {
    return decodeURIComponent(s);
  } catch {
    return s;
  }
}

// Multiple ?to= / ?cc= / ?bcc= parameters are allowed, and each may itself
// contain a comma-separated list. Flatten into a single comma-separated
// string with no duplicates so the compose form gets a clean prefill.
function mergeCommaList(initial: string[], extra: string[]): string {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const piece of [...initial, ...extra.flatMap((e) => e.split(","))]) {
    const v = piece.trim();
    if (!v || seen.has(v)) continue;
    seen.add(v);
    out.push(v);
  }
  return out.join(", ");
}
