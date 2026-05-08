/**
 * Contact-merge helpers (#129).
 *
 * Pure functions extracted from ContactsView.vue so the merge rules
 * can be unit-tested without mounting the component. The view imports
 * `mergeContacts` and uses its result as the input to
 * `api.updateContact` for the merge primary, then deletes the
 * secondary.
 *
 * Merge policy: "primary wins" on atomic fields, with empty-string
 * fallthrough to the secondary so the loser's value still surfaces
 * when the primary has nothing. Lists (emails, phones, addresses)
 * are unioned with case-folded dedup. Notes concatenate when both
 * sides differ.
 */

import type { Contact } from "./types";

interface EmailEntry { email?: string; label?: string }
interface PhoneEntry { number?: string; label?: string }

/// Which side a field-by-field merge should pull a value from.
/// "keeper" = the contact whose id / remote_id / etag survive the
/// merge. "loser" = the one being deleted. "both" only applies to
/// notes — concatenate the two with a separator.
export type AtomicSide = "keeper" | "loser";
export type NotesSide = "keeper" | "loser" | "both";

/// One entry in the unioned list shown in the picker. Each item
/// carries the source side (so the UI can show a label) and an
/// `include` flag for the checkbox.
export interface MergeListItem {
  source: AtomicSide;
  include: boolean;
  /// Raw item from the original JSON (email entry, phone entry, …).
  item: Record<string, unknown>;
}

/// Full picker state. The dialog populates this from the two
/// contacts; `applyMergeChoices` consumes it to produce the
/// surviving record.
export interface MergeChoices {
  display_name: AtomicSide;
  organization: AtomicSide;
  title: AtomicSide;
  notes: NotesSide;
  emails: MergeListItem[];
  phones: MergeListItem[];
  addresses: MergeListItem[];
}

function trimOrNull(s: string | null | undefined): string | null {
  const t = (s ?? "").trim();
  return t.length > 0 ? t : null;
}

function side(
  choice: AtomicSide,
  keeper: string | null | undefined,
  loser: string | null | undefined,
): string | null {
  // If the picked side is empty, fall through to the other so the
  // user never ends up with a blank field as a side effect of the
  // dialog defaulting to one side.
  const picked = choice === "keeper" ? keeper : loser;
  const fallback = choice === "keeper" ? loser : keeper;
  return trimOrNull(picked) ?? trimOrNull(fallback);
}

function mergeNotes(
  choice: NotesSide,
  keeper: string | null | undefined,
  loser: string | null | undefined,
): string | null {
  const k = trimOrNull(keeper);
  const l = trimOrNull(loser);
  switch (choice) {
    case "keeper":
      return k ?? l;
    case "loser":
      return l ?? k;
    case "both": {
      if (!k) return l;
      if (!l || k === l) return k;
      return `${k}\n---\n${l}`;
    }
  }
}

function parseList<T = Record<string, unknown>>(json: string): T[] {
  try {
    const parsed = JSON.parse(json);
    return Array.isArray(parsed) ? (parsed as T[]) : [];
  } catch {
    return [];
  }
}

/// Build a default `MergeListItem[]` for a list field (emails /
/// phones / addresses): union of keeper + loser, deduped by the
/// supplied canonical key, with all entries pre-checked. Keeper's
/// items come first so the picker UI shows them on top.
export function buildListItems(
  keeperJson: string,
  loserJson: string,
  key: (item: Record<string, unknown>) => string,
): MergeListItem[] {
  const items: MergeListItem[] = [];
  const seen = new Set<string>();
  const push = (raw: Record<string, unknown>, source: AtomicSide) => {
    const k = key(raw).trim().toLowerCase();
    if (!k || seen.has(k)) return;
    seen.add(k);
    items.push({ source, include: true, item: raw });
  };
  for (const it of parseList(keeperJson)) push(it, "keeper");
  for (const it of parseList(loserJson)) push(it, "loser");
  return items;
}

/// Turn an opinion-laden `MergeChoices` into the final Contact
/// record that gets pushed via `update_contact`. Identity fields
/// (id, book_id, uid, remote_id, etag, vcard_data) always come
/// from `keeper` so the merged record points at the existing
/// remote object.
export function applyMergeChoices(
  keeper: Contact,
  loser: Contact,
  choices: MergeChoices,
): Contact {
  return {
    ...keeper,
    display_name:
      side(choices.display_name, keeper.display_name, loser.display_name)
      ?? keeper.display_name,
    organization: side(choices.organization, keeper.organization, loser.organization),
    title: side(choices.title, keeper.title, loser.title),
    notes: mergeNotes(choices.notes, keeper.notes, loser.notes),
    emails_json: JSON.stringify(
      choices.emails.filter((it) => it.include).map((it) => it.item),
    ),
    phones_json: JSON.stringify(
      choices.phones.filter((it) => it.include).map((it) => it.item),
    ),
    addresses_json: JSON.stringify(
      choices.addresses.filter((it) => it.include).map((it) => it.item),
    ),
  };
}

/// Default choices for two contacts: keeper-wins on each atomic
/// field unless the keeper's value is empty (then default to the
/// loser); `notes` defaults to `both` only when both sides have
/// non-empty differing notes, otherwise keeper. List items default
/// to all included.
export function defaultChoices(keeper: Contact, loser: Contact): MergeChoices {
  const pick = (a: string | null | undefined, b: string | null | undefined): AtomicSide =>
    trimOrNull(a) ? "keeper" : trimOrNull(b) ? "loser" : "keeper";

  const k = trimOrNull(keeper.notes);
  const l = trimOrNull(loser.notes);
  const notes: NotesSide = !k ? "loser" : !l || k === l ? "keeper" : "both";

  return {
    display_name: pick(keeper.display_name, loser.display_name),
    organization: pick(keeper.organization, loser.organization),
    title: pick(keeper.title, loser.title),
    notes,
    emails: buildListItems(
      keeper.emails_json,
      loser.emails_json,
      (it) => String((it as EmailEntry).email ?? ""),
    ),
    phones: buildListItems(
      keeper.phones_json,
      loser.phones_json,
      (it) => String((it as PhoneEntry).number ?? ""),
    ),
    addresses: buildListItems(
      keeper.addresses_json,
      loser.addresses_json,
      (it) => JSON.stringify(it),
    ),
  };
}

/// Convenience wrapper: legacy API that auto-resolves to keeper-wins
/// defaults. Kept so the existing call site can stay compact in the
/// non-interactive code path; callers that want field-by-field
/// control should use `defaultChoices` + `applyMergeChoices`.
export function mergeContacts(keeper: Contact, loser: Contact): Contact {
  return applyMergeChoices(keeper, loser, defaultChoices(keeper, loser));
}
