<script setup lang="ts">
import { computed } from "vue";
import { useUiStore } from "@/stores/ui";

const props = defineProps<{
  text: string | null | undefined;
}>();

const uiStore = useUiStore();

// Trailing punctuation that a user almost never means as part of a URL but
// which the regex would otherwise capture. Stripped at split-time and
// returned as plain text so it stays visible in the rendered output.
// Closing brackets get a balanced-pair check below (Wikipedia URLs like
// .../Foo_(disambiguation) keep the trailing ')').
const TRAILING_PUNCT_ALWAYS = /[.,!?;:>'"]+$/;
const URL_RE = /https?:\/\/[^\s<>"']+/g;
const BRACKET_PAIRS: Record<string, string> = { ")": "(", "]": "[", "}": "{" };

// Walk trailing punctuation off the end of a matched URL. Symmetric
// brackets are only stripped when the URL has more of them at the end
// than openers inside it; otherwise we leave them so URLs containing
// balanced parens (Wikipedia, MDN, ...) survive unscathed.
function trimTrailingPunct(url: string): { url: string; trailing: string } {
  let trailing = "";
  while (url.length > 0) {
    const last = url[url.length - 1];
    if (last in BRACKET_PAIRS) {
      const opener = BRACKET_PAIRS[last];
      const opens = (url.match(new RegExp("\\" + opener, "g")) ?? []).length;
      const closes = (url.match(new RegExp("\\" + last, "g")) ?? []).length;
      if (closes <= opens) break;
      trailing = last + trailing;
      url = url.slice(0, -1);
      continue;
    }
    const m = url.match(TRAILING_PUNCT_ALWAYS);
    if (!m) break;
    trailing = m[0] + trailing;
    url = url.slice(0, url.length - m[0].length);
  }
  return { url, trailing };
}

// Exchange/Outlook calendar descriptions sometimes arrive as a full HTML
// document instead of plain text. Decode them to text so the user sees the
// content rather than raw markup, preserving line breaks from <br> and
// common block elements. <a href> values are appended to the text output
// so links that only existed as anchors (no visible URL) still get
// linkified by the regex pass below.
//
// The trigger requires a doctype, <html>, or any explicit closing tag.
// Plain prose containing comparisons like "a < b" or "span > 0" therefore
// does not get fed to DOMParser (which would silently rewrite whitespace
// and drop anything outside <body>).
function looksLikeHtml(input: string): boolean {
  return /<!doctype html|<html\b|<\/[a-z][a-z0-9]*\s*>/i.test(input);
}

function htmlToPlain(input: string): string {
  if (!looksLikeHtml(input)) return input;
  const doc = new DOMParser().parseFromString(input, "text/html");
  // Outlook/Word HTML exports embed large <style> blocks ("p.MsoNormal {...}")
  // and the occasional <script>. textContent would otherwise dump that
  // CSS/JS into the user-visible description.
  doc.querySelectorAll("style, script, head, noscript").forEach((el) => el.remove());
  doc.querySelectorAll("br").forEach((br) => br.replaceWith("\n"));
  doc
    .querySelectorAll("p, div, li, h1, h2, h3, h4, h5, h6, tr")
    .forEach((el) => el.append("\n"));
  doc.querySelectorAll("a[href]").forEach((a) => {
    const href = (a as HTMLAnchorElement).getAttribute("href") ?? "";
    if (!href.startsWith("http://") && !href.startsWith("https://")) return;
    const text = (a.textContent ?? "").trim();
    if (!text || !text.includes(href)) a.append(` (${href})`);
  });
  const out = (doc.body?.textContent ?? input)
    .replace(/[ \t]+\n/g, "\n")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
  return out;
}

interface Segment {
  kind: "text" | "link";
  value: string;
}

const segments = computed<Segment[]>(() => {
  const input = htmlToPlain(props.text ?? "");
  if (!input) return [];
  const out: Segment[] = [];
  let lastIndex = 0;
  for (const match of input.matchAll(URL_RE)) {
    const start = match.index ?? 0;
    const { url, trailing } = trimTrailingPunct(match[0]);
    if (start > lastIndex) {
      out.push({ kind: "text", value: input.slice(lastIndex, start) });
    }
    out.push({ kind: "link", value: url });
    if (trailing) out.push({ kind: "text", value: trailing });
    lastIndex = start + match[0].length;
  }
  if (lastIndex < input.length) {
    out.push({ kind: "text", value: input.slice(lastIndex) });
  }
  return out;
});

function onLinkClick(url: string, e: MouseEvent) {
  e.preventDefault();
  uiStore.setHoverUrl(null);
  uiStore.openLinkPopup(url);
}
function onLinkEnter(url: string) {
  uiStore.setHoverUrl(url);
}
function onLinkLeave() {
  uiStore.setHoverUrl(null);
}
</script>

<template>
  <span class="linkified">
    <template v-for="(seg, i) in segments" :key="i">
      <a
        v-if="seg.kind === 'link'"
        :href="seg.value"
        class="linkified-link"
        @click="onLinkClick(seg.value, $event)"
        @mouseenter="onLinkEnter(seg.value)"
        @mouseleave="onLinkLeave"
      >{{ seg.value }}</a>
      <template v-else>{{ seg.value }}</template>
    </template>
  </span>
</template>

<style scoped>
.linkified {
  white-space: pre-wrap;
  word-wrap: break-word;
  overflow-wrap: break-word;
}

.linkified-link {
  color: var(--color-accent);
  text-decoration: underline;
  cursor: pointer;
}

.linkified-link:hover {
  filter: brightness(1.1);
}
</style>
