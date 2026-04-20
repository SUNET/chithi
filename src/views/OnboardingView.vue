<script setup lang="ts">
import { useRouter } from "vue-router";

const router = useRouter();

interface Provider {
  id: string;
  name: string;
  sub: string;
  color: string;
  tint: string;
  initial: string;
}

const providers: Provider[] = [
  {
    id: "jmap",
    name: "Stalwart / JMAP",
    sub: "Self-hosted · recommended",
    color: "#b54708",
    tint: "#fef3e2",
    initial: "J",
  },
  {
    id: "microsoft365",
    name: "Microsoft 365",
    sub: "Work or school account",
    color: "#6d8a3a",
    tint: "#eef2dc",
    initial: "M",
  },
  {
    id: "gmail",
    name: "Google / Gmail",
    sub: "OAuth",
    color: "#b8404d",
    tint: "#fbe0e3",
    initial: "G",
  },
  {
    id: "imap",
    name: "Other (IMAP / SMTP)",
    sub: "Manual setup",
    color: "#6b4226",
    tint: "#f3ead7",
    initial: "O",
  },
];

function pickProvider(p: Provider) {
  // Hand off to the main Settings flow — the full provider wizards live
  // there. This keeps onboarding the first-run router and the settings
  // view the canonical "add account" surface.
  router.push({ path: "/settings", query: { addAccount: p.id } });
}

function skipForNow() {
  router.push("/");
}
</script>

<template>
  <div class="onboarding-view">
    <div class="brand-pill">
      <span class="brand-tile">C</span>
      <span class="brand-name">Chithi</span>
    </div>
    <h1 class="hero">Your mail, your calendar, on your device.</h1>
    <p class="lede">
      Pick a provider to sign in. Chithi stores everything locally — nothing
      leaves your machine unless you send it.
    </p>

    <ul class="provider-list">
      <li v-for="p in providers" :key="p.id">
        <button
          class="provider-card"
          :style="{ borderLeftColor: p.color }"
          @click="pickProvider(p)"
        >
          <span
            class="provider-avatar"
            :style="{ background: p.tint, color: p.color }"
          >
            {{ p.initial }}
          </span>
          <span class="provider-text">
            <span class="provider-name">{{ p.name }}</span>
            <span class="provider-sub">{{ p.sub }}</span>
          </span>
          <svg
            class="chevron"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linecap="round"
            stroke-linejoin="round"
          >
            <polyline points="9 6 15 12 9 18" />
          </svg>
        </button>
      </li>
    </ul>

    <button class="skip-link" @click="skipForNow">
      Skip and add an account later
    </button>
  </div>
</template>

<style scoped>
.onboarding-view {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  background: var(--color-bg);
  padding: 32px 20px 40px;
  padding-top: max(32px, env(safe-area-inset-top));
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.brand-pill {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  align-self: flex-start;
  padding: 6px 10px 6px 6px;
  border-radius: 100px;
  background: var(--color-accent-light);
  color: var(--color-accent);
}

.brand-tile {
  width: 24px;
  height: 24px;
  border-radius: 6px;
  background: var(--color-accent);
  color: #fff;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-weight: 700;
  font-size: 14px;
}

.brand-name {
  font-weight: 600;
  font-size: 13px;
  letter-spacing: -0.1px;
}

.hero {
  font-size: 36px;
  line-height: 1.08;
  font-weight: 700;
  letter-spacing: -0.8px;
  color: var(--color-text);
  margin: 4px 0 0;
}

.lede {
  font-size: 15px;
  line-height: 1.5;
  color: var(--color-text-muted);
  margin: 0 0 8px;
}

.provider-list {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.provider-card {
  width: 100%;
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 14px 12px;
  background: var(--color-bg-secondary);
  border: 1px solid var(--color-divider, #e9e0cd);
  border-left: 4px solid var(--color-accent);
  border-radius: var(--radius-card-mobile);
  text-align: left;
  font-family: inherit;
  color: var(--color-text);
  cursor: pointer;
  min-height: 60px;
}

.provider-avatar {
  width: 38px;
  height: 38px;
  border-radius: 8px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-weight: 700;
  font-size: 16px;
  flex-shrink: 0;
}

.provider-text {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.provider-name {
  font-size: 15px;
  font-weight: 600;
  color: var(--color-text);
}

.provider-sub {
  font-size: 12px;
  color: var(--color-text-muted);
}

.chevron {
  width: 18px;
  height: 18px;
  color: var(--color-text-muted);
  flex-shrink: 0;
}

.skip-link {
  align-self: center;
  margin-top: 8px;
  padding: 10px 14px;
  border: 0;
  background: transparent;
  color: var(--color-accent);
  font-weight: 600;
  font-size: 14px;
  cursor: pointer;
}
</style>
