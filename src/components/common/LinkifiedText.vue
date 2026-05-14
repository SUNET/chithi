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
const TRAILING_PUNCT = /[.,!?;:)\]}>'"]+$/;
const URL_RE = /https?:\/\/[^\s<>"']+/g;

// Exchange/Outlook calendar descriptions sometimes arrive as a full HTML
// document instead of plain text. Decode them to text so the user sees the
// content rather than raw markup, preserving line breaks from <br> and
// common block elements. <a href> values are appended to the text output
// so links that only existed as anchors (no visible URL) still get
// linkified by the regex pass below.
function htmlToPlain(input: string): string {
  if (!/<\s*(html|body|head|div|p|br|a|span)\b/i.test(input)) return input;
  const doc = new DOMParser().parseFromString(input, "text/html");
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
    let url = match[0];
    let trailing = "";
    const m = url.match(TRAILING_PUNCT);
    if (m) {
      trailing = m[0];
      url = url.slice(0, url.length - trailing.length);
    }
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
