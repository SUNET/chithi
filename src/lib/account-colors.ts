/** Maps an account ID to a stable pair of theme color variables. */

const PALETTE_SIZE = 7;

function hashString(value: string): number {
  // FNV-1a 32-bit
  let hash = 0x811c9dc5;
  for (let index = 0; index < value.length; index++) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  return hash >>> 0;
}

export function acctColor(accountId: string | null | undefined) {
  const paletteIndex = (hashString(accountId ?? "") % PALETTE_SIZE) + 1;
  return {
    fill: `var(--acct-${paletteIndex})`,
    soft: `var(--acct-${paletteIndex}-soft)`,
  };
}
