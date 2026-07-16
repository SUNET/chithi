/// Parsers for the JSON-encoded email/phone lists stored on contact
/// rows (`emails_json` / `phones_json`). Malformed JSON degrades to an
/// empty list — rows written by older versions or foreign servers must
/// never break rendering.

export function parseEmails(json: string): { email: string; label: string }[] {
  try { return JSON.parse(json); } catch { return []; }
}

export function parsePhones(json: string): { number: string; label: string }[] {
  try { return JSON.parse(json); } catch { return []; }
}

export function parseFirstEmail(json: string): string {
  try {
    const arr = JSON.parse(json) as Array<{ email?: string }>;
    return arr[0]?.email ?? "";
  } catch {
    return "";
  }
}
