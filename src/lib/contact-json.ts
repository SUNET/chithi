/** Invalid or non-array contact JSON is empty so it cannot break rendering. */
function parseJsonList<T>(encoded: string): T[] {
  try {
    const parsed: unknown = JSON.parse(encoded);
    return Array.isArray(parsed) ? (parsed as T[]) : [];
  } catch {
    return [];
  }
}

export function parseEmails(
  encoded: string,
): { email: string; label: string }[] {
  return parseJsonList(encoded);
}

export function parsePhones(
  encoded: string,
): { number: string; label: string }[] {
  return parseJsonList(encoded);
}

export function parseFirstEmail(encoded: string): string {
  const email = parseJsonList<{ email?: unknown }>(encoded)[0]?.email;
  return typeof email === "string" ? email : "";
}
