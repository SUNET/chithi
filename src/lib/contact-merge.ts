import type { Contact } from "./types";

type AtomicSide = "keeper" | "loser";
type NotesSide = AtomicSide | "both";

interface MergeListItem {
  source: AtomicSide;
  include: boolean;
  item: Record<string, unknown>;
}

export interface MergeChoices {
  display_name: AtomicSide;
  organization: AtomicSide;
  title: AtomicSide;
  notes: NotesSide;
  emails: MergeListItem[];
  phones: MergeListItem[];
  addresses: MergeListItem[];
}

function trimOrNull(value: string | null | undefined): string | null {
  const trimmed = (value ?? "").trim();
  return trimmed || null;
}

function selectAtomicValue(
  choice: AtomicSide,
  keeper: string | null | undefined,
  loser: string | null | undefined,
): string | null {
  const selected = choice === "keeper" ? keeper : loser;
  const fallback = choice === "keeper" ? loser : keeper;
  return trimOrNull(selected) ?? trimOrNull(fallback);
}

function mergeNotes(
  choice: NotesSide,
  keeper: string | null | undefined,
  loser: string | null | undefined,
): string | null {
  const keeperNotes = trimOrNull(keeper);
  const loserNotes = trimOrNull(loser);
  switch (choice) {
    case "keeper":
      return keeperNotes ?? loserNotes;
    case "loser":
      return loserNotes ?? keeperNotes;
    case "both":
      if (!keeperNotes) return loserNotes;
      if (!loserNotes || keeperNotes === loserNotes) return keeperNotes;
      return `${keeperNotes}\n---\n${loserNotes}`;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function parseList(encoded: string): Record<string, unknown>[] {
  try {
    const parsed: unknown = JSON.parse(encoded);
    return Array.isArray(parsed) ? parsed.filter(isRecord) : [];
  } catch {
    return [];
  }
}

function buildListItems(
  keeperJson: string,
  loserJson: string,
  key: (item: Record<string, unknown>) => string,
): MergeListItem[] {
  const items: MergeListItem[] = [];
  const seen = new Set<string>();

  for (const [encoded, source] of [
    [keeperJson, "keeper"],
    [loserJson, "loser"],
  ] as const) {
    for (const item of parseList(encoded)) {
      const canonicalKey = key(item).trim().toLowerCase();
      if (!canonicalKey || seen.has(canonicalKey)) continue;
      seen.add(canonicalKey);
      items.push({ source, include: true, item });
    }
  }

  return items;
}

function serializeIncludedItems(items: MergeListItem[]): string {
  return JSON.stringify(
    items.filter((item) => item.include).map((item) => item.item),
  );
}

/** Applies merge choices while preserving the keeper's identity fields. */
export function applyMergeChoices(
  keeper: Contact,
  loser: Contact,
  choices: MergeChoices,
): Contact {
  return {
    ...keeper,
    display_name:
      selectAtomicValue(
        choices.display_name,
        keeper.display_name,
        loser.display_name,
      ) ?? keeper.display_name,
    organization: selectAtomicValue(
      choices.organization,
      keeper.organization,
      loser.organization,
    ),
    title: selectAtomicValue(choices.title, keeper.title, loser.title),
    notes: mergeNotes(choices.notes, keeper.notes, loser.notes),
    emails_json: serializeIncludedItems(choices.emails),
    phones_json: serializeIncludedItems(choices.phones),
    addresses_json: serializeIncludedItems(choices.addresses),
  };
}

function defaultAtomicSide(
  keeper: string | null | undefined,
  loser: string | null | undefined,
): AtomicSide {
  if (trimOrNull(keeper)) return "keeper";
  return trimOrNull(loser) ? "loser" : "keeper";
}

function defaultNotesSide(
  keeper: string | null | undefined,
  loser: string | null | undefined,
): NotesSide {
  const keeperNotes = trimOrNull(keeper);
  const loserNotes = trimOrNull(loser);
  if (!keeperNotes) return "loser";
  if (!loserNotes || keeperNotes === loserNotes) return "keeper";
  return "both";
}

export function defaultChoices(keeper: Contact, loser: Contact): MergeChoices {
  return {
    display_name: defaultAtomicSide(keeper.display_name, loser.display_name),
    organization: defaultAtomicSide(keeper.organization, loser.organization),
    title: defaultAtomicSide(keeper.title, loser.title),
    notes: defaultNotesSide(keeper.notes, loser.notes),
    emails: buildListItems(
      keeper.emails_json,
      loser.emails_json,
      (item) => String(item.email ?? ""),
    ),
    phones: buildListItems(
      keeper.phones_json,
      loser.phones_json,
      (item) => String(item.number ?? ""),
    ),
    addresses: buildListItems(
      keeper.addresses_json,
      loser.addresses_json,
      (item) => JSON.stringify(item),
    ),
  };
}
