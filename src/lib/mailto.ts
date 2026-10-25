import type { ComposeParams } from "./compose-window";

const MAILTO_SCHEME = "mailto:";

/** Encodes an address while preserving safe addr-spec punctuation. */
export function encodeMailtoAddress(email: string): string {
  return encodeURIComponent(email.trim())
    .replace(/%40/g, "@")
    .replace(/%2E/gi, ".")
    .replace(/%2B/gi, "+");
}

/** Keeps only characters understood by OS dialers. */
export function sanitizeTel(number: string): string {
  return number.replace(/[^0-9+*#\-.]/g, "");
}

/** Parses an RFC 6068 URI into compose parameters. */
export function parseMailto(href: string): ComposeParams | null {
  if (
    href.slice(0, MAILTO_SCHEME.length).toLowerCase() !== MAILTO_SCHEME
  ) {
    return null;
  }

  const afterScheme = href.slice(MAILTO_SCHEME.length);
  const questionMarkIndex = afterScheme.indexOf("?");
  const path =
    questionMarkIndex === -1
      ? afterScheme
      : afterScheme.slice(0, questionMarkIndex);
  const queryString =
    questionMarkIndex === -1 ? "" : afterScheme.slice(questionMarkIndex + 1);
  const pathAddresses = path
    .split(",")
    .map((address) => safeDecode(address.trim()))
    .filter(Boolean);
  const params = new URLSearchParams(queryString);
  const result: ComposeParams = {};

  const to = mergeAddressLists(pathAddresses, params.getAll("to"));
  if (to) result.to = to;

  for (const field of ["cc", "bcc"] as const) {
    const addresses = mergeAddressLists(params.getAll(field));
    if (addresses) result[field] = addresses;
  }

  for (const field of ["subject", "body"] as const) {
    const value = params.get(field);
    if (value) result[field] = value;
  }

  return result;
}

function safeDecode(value: string): string {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

/** Flattens repeated and comma-separated address lists without duplicates. */
function mergeAddressLists(...lists: string[][]): string {
  const seen = new Set<string>();
  const addresses: string[] = [];

  for (const list of lists) {
    for (const value of list) {
      for (const piece of value.split(",")) {
        const address = piece.trim();
        if (!address || !address.includes("@") || seen.has(address)) continue;
        seen.add(address);
        addresses.push(address);
      }
    }
  }

  return addresses.join(", ");
}
