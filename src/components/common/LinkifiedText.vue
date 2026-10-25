<script setup lang="ts">
import { computed } from "vue";
import { useUiStore } from "@/stores/ui";

const props = defineProps<{
  text: string | null | undefined;
}>();

const uiStore = useUiStore();

// Strip sentence punctuation while preserving balanced URL brackets.
const TRAILING_PUNCT_ALWAYS = /[.,!?;:>'"]+$/;
const URL_RE = /https?:\/\/[^\s<>"']+/g;
const BRACKET_PAIRS: Record<string, string> = { ")": "(", "]": "[", "}": "{" };

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

// Parse only rooted or balanced HTML so prose with stray tags remains intact.
function looksLikeHtml(input: string): boolean {
  if (/<!doctype html\b|<html\b|<body\b/i.test(input)) return true;
  const match = input.match(/<([a-z][a-z0-9]*)\b[^>]*>/i);
  if (!match) return false;
  const tag = match[1].toLowerCase();
  return new RegExp(`</${tag}\\s*>`, "i").test(input);
}

function htmlToPlain(input: string): string {
  if (!looksLikeHtml(input)) return input;
  const doc = new DOMParser().parseFromString(input, "text/html");
  // Exclude embedded CSS and scripts from visible text.
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
  return (doc.body?.textContent ?? input)
    .replace(/[ \t]+\n/g, "\n")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
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

function onLinkClick(url: string, event: MouseEvent) {
  event.preventDefault();
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
