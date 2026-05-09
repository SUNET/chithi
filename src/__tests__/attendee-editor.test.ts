/**
 * Component tests for AttendeeEditor — the type-ahead attendee field
 * in the calendar event form. Covers debounce / search invocation,
 * arrow-key + Enter selection, comma-or-Enter to add a raw email,
 * and stale debounce / out-of-order response handling so a regression
 * in any of those paths surfaces in CI.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import AttendeeEditor from "@/components/calendar/AttendeeEditor.vue";
import * as api from "@/lib/tauri";

vi.mock("@/lib/tauri", () => ({
  searchContacts: vi.fn(),
  searchContactsForAccount: vi.fn(),
}));

function contact(name: string, email: string) {
  return {
    id: `c-${email}`,
    book_id: "b1",
    uid: `uid-${email}`,
    display_name: name,
    emails_json: JSON.stringify([{ email, label: "" }]),
    phones_json: "[]",
    addresses_json: "[]",
    organization: null,
    title: null,
    notes: null,
    vcard_data: null,
    remote_id: null,
    etag: null,
  };
}

async function typeAndAdvance(wrapper: ReturnType<typeof mount>, text: string) {
  const input = wrapper.get('[data-testid="attendee-input"]');
  await input.setValue(text);
  // The component debounces 150ms before firing the search; advance
  // a hair past that so the timer fires inside fake-timer space.
  vi.advanceTimersByTime(160);
  await flushPromises();
}

describe("AttendeeEditor", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.mocked(api.searchContacts).mockReset();
    vi.mocked(api.searchContactsForAccount).mockReset();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("debounces and runs a search for 2+ char queries", async () => {
    vi.mocked(api.searchContacts).mockResolvedValue([
      contact("Alice", "alice@example.com"),
    ]);

    const wrapper = mount(AttendeeEditor, {
      attachTo: document.body,
      props: { modelValue: [] },
    });

    await typeAndAdvance(wrapper, "ali");
    expect(api.searchContacts).toHaveBeenCalledOnce();
    expect(api.searchContacts).toHaveBeenCalledWith("ali");

    const items = wrapper.findAll('[data-testid^="attendee-suggestion-"]');
    expect(items).toHaveLength(1);
    expect(items[0].text()).toContain("alice@example.com");
  });

  it("uses searchContactsForAccount with calendar service when accountId is set", async () => {
    vi.mocked(api.searchContactsForAccount).mockResolvedValue([
      contact("Alice", "alice@example.com"),
    ]);

    const wrapper = mount(AttendeeEditor, {
      attachTo: document.body,
      props: { modelValue: [], accountId: "acc-1" },
    });

    await typeAndAdvance(wrapper, "ali");
    expect(api.searchContactsForAccount).toHaveBeenCalledOnce();
    expect(api.searchContactsForAccount).toHaveBeenCalledWith(
      "ali",
      "acc-1",
      "calendar",
    );
    expect(api.searchContacts).not.toHaveBeenCalled();
  });

  it("does not search for queries shorter than 2 chars", async () => {
    vi.mocked(api.searchContacts).mockResolvedValue([]);

    const wrapper = mount(AttendeeEditor, {
      attachTo: document.body,
      props: { modelValue: [] },
    });

    await typeAndAdvance(wrapper, "a");
    expect(api.searchContacts).not.toHaveBeenCalled();
  });

  it("ArrowDown + Enter picks the highlighted suggestion", async () => {
    vi.mocked(api.searchContacts).mockResolvedValue([
      contact("Alice", "alice@example.com"),
      contact("Alex", "alex@example.com"),
    ]);

    const updates: string[][] = [];
    const wrapper = mount(AttendeeEditor, {
      attachTo: document.body,
      props: {
        modelValue: [],
        "onUpdate:modelValue": (v: string[]) => updates.push(v),
      },
    });

    await typeAndAdvance(wrapper, "al");
    const input = wrapper.get('[data-testid="attendee-input"]');
    await input.trigger("keydown", { key: "ArrowDown" });
    await input.trigger("keydown", { key: "ArrowDown" });
    await input.trigger("keydown", { key: "Enter" });

    expect(updates).toHaveLength(1);
    expect(updates[0]).toEqual(["alex@example.com"]);
  });

  it("comma adds the raw typed email when no suggestion is active", async () => {
    vi.mocked(api.searchContacts).mockResolvedValue([]);
    const updates: string[][] = [];
    const wrapper = mount(AttendeeEditor, {
      attachTo: document.body,
      props: {
        modelValue: [],
        "onUpdate:modelValue": (v: string[]) => updates.push(v),
      },
    });

    const input = wrapper.get('[data-testid="attendee-input"]');
    await input.setValue("guest@example.com");
    await input.trigger("keydown", { key: "," });
    expect(updates[updates.length - 1]).toEqual(["guest@example.com"]);
  });

  it("Enter without an active suggestion adds the raw typed email", async () => {
    vi.mocked(api.searchContacts).mockResolvedValue([]);
    const updates: string[][] = [];
    const wrapper = mount(AttendeeEditor, {
      attachTo: document.body,
      props: {
        modelValue: [],
        "onUpdate:modelValue": (v: string[]) => updates.push(v),
      },
    });

    const input = wrapper.get('[data-testid="attendee-input"]');
    await input.setValue("guest@example.com");
    await input.trigger("keydown", { key: "Enter" });
    expect(updates[updates.length - 1]).toEqual(["guest@example.com"]);
  });

  it("backspacing below the min length cancels the pending search", async () => {
    // Schedule a search for "ali", then backspace to "a" before it
    // fires. The mock should never be called — without the
    // clearTimeout in the short-query branch, the stale timer would
    // run and resolve with stale results.
    vi.mocked(api.searchContacts).mockResolvedValue([
      contact("Alice", "alice@example.com"),
    ]);
    const wrapper = mount(AttendeeEditor, {
      attachTo: document.body,
      props: { modelValue: [] },
    });

    const input = wrapper.get('[data-testid="attendee-input"]');
    await input.setValue("ali");
    // Advance only partway through the debounce so the timer is still
    // pending when we shrink the query.
    vi.advanceTimersByTime(80);
    await input.setValue("a");
    vi.advanceTimersByTime(200);
    await flushPromises();

    expect(api.searchContacts).not.toHaveBeenCalled();
    expect(wrapper.findAll('[data-testid^="attendee-suggestion-"]')).toHaveLength(0);
  });

  it("out-of-order responses don't overwrite a fresher search", async () => {
    // First search resolves slowly, second resolves fast. With the
    // request-id guard, the slow first response is discarded.
    let resolveFirst!: (v: ReturnType<typeof contact>[]) => void;
    const firstPromise = new Promise<ReturnType<typeof contact>[]>((resolve) => {
      resolveFirst = resolve;
    });
    vi.mocked(api.searchContacts)
      .mockReturnValueOnce(firstPromise)
      .mockResolvedValueOnce([contact("Bob", "bob@example.com")]);

    const wrapper = mount(AttendeeEditor, {
      attachTo: document.body,
      props: { modelValue: [] },
    });

    const input = wrapper.get('[data-testid="attendee-input"]');
    await input.setValue("al");
    vi.advanceTimersByTime(160);
    await flushPromises();
    // Second keystroke fires a second search, which resolves
    // immediately.
    await input.setValue("bo");
    vi.advanceTimersByTime(160);
    await flushPromises();
    // Now the first request finally completes — but its result must
    // not clobber the second's.
    resolveFirst([contact("Alice", "alice@example.com")]);
    await flushPromises();

    const items = wrapper.findAll('[data-testid^="attendee-suggestion-"]');
    expect(items).toHaveLength(1);
    expect(items[0].text()).toContain("bob@example.com");
  });
});
