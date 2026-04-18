/**
 * Per-account color picker.
 *
 * Colors are derived from a stable hash of the account UID rather than
 * the provider type, so two accounts on the same provider get distinct
 * colors and the assignment is consistent across sessions / users.
 *
 * Returns CSS custom-property references (e.g. `var(--acct-3)`) that
 * resolve through the active theme.
 */

const PALETTE_SIZE = 7;

export interface AcctColor {
  fill: string; // strong color — ring, badge, label
  soft: string; // tinted background
}

function hashStr(s: string): number {
  // FNV-1a 32-bit
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

export function acctColor(accountId: string | null | undefined): AcctColor {
  const id = accountId ?? "";
  const idx = (hashStr(id) % PALETTE_SIZE) + 1;
  return {
    fill: `var(--acct-${idx})`,
    soft: `var(--acct-${idx}-soft)`,
  };
}
