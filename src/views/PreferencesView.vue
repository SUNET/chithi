<script setup lang="ts">
import { computed, nextTick, ref } from "vue";
import { useRouter } from "vue-router";
import { useUiStore, type Theme, type TimeFormat } from "@/stores/ui";

type Section = "general" | "date-time";

const router = useRouter();
const uiStore = useUiStore();
const activeSection = ref<Section>("general");

const sections: ReadonlyArray<{ id: Section; label: string }> = [
  { id: "general", label: "General" },
  { id: "date-time", label: "Date and Time" },
];

function close() {
  router.back();
}

// --- General ----------------------------------------------------------------

const themeOptions: ReadonlyArray<{ value: Theme; label: string }> = [
  { value: "system", label: "System" },
  { value: "light", label: "Light" },
  { value: "dark", label: "Dark" },
];

// --- Date and Time ----------------------------------------------------------

const weekStartOptions: ReadonlyArray<{ day: number; label: string }> = [
  { day: 0, label: "Sunday" },
  { day: 1, label: "Monday" },
  { day: 6, label: "Saturday" },
];

const timeFormatOptions: ReadonlyArray<{ value: TimeFormat; label: string }> = [
  { value: "auto", label: "Auto" },
  { value: "12", label: "12h" },
  { value: "24", label: "24h" },
];

// Timezone picker (combobox)
const tzSearch = ref("");
const tzDropdownOpen = ref(false);
const tzHighlightIndex = ref(-1);
const tzDropdownRef = ref<HTMLElement | null>(null);

const filteredTimezones = computed(() => {
  const query = tzSearch.value.toLowerCase();
  if (!query) return uiStore.timezoneList;
  return uiStore.timezoneList.filter((tz: string) => tz.toLowerCase().includes(query));
});

function selectTimezone(tz: string) {
  uiStore.setDisplayTimezone(tz);
  tzSearch.value = "";
  tzDropdownOpen.value = false;
  tzHighlightIndex.value = -1;
}

function onTzInput(e: Event) {
  tzSearch.value = (e.target as HTMLInputElement).value;
  tzHighlightIndex.value = 0;
}

function onTzInputFocus() {
  tzDropdownOpen.value = true;
  tzSearch.value = "";
  tzHighlightIndex.value = -1;
}

function onTzInputBlur() {
  setTimeout(() => {
    tzDropdownOpen.value = false;
    tzSearch.value = "";
    tzHighlightIndex.value = -1;
  }, 200);
}

function onTzKeydown(e: KeyboardEvent) {
  if (!tzDropdownOpen.value) return;
  const list = filteredTimezones.value;
  if (e.key === "ArrowDown") {
    e.preventDefault();
    tzHighlightIndex.value = Math.min(tzHighlightIndex.value + 1, list.length - 1);
    scrollHighlightedIntoView();
  } else if (e.key === "ArrowUp") {
    e.preventDefault();
    tzHighlightIndex.value = Math.max(tzHighlightIndex.value - 1, 0);
    scrollHighlightedIntoView();
  } else if (e.key === "Enter") {
    e.preventDefault();
    if (tzHighlightIndex.value >= 0 && tzHighlightIndex.value < list.length) {
      selectTimezone(list[tzHighlightIndex.value]);
      (e.target as HTMLInputElement)?.blur();
    }
  } else if (e.key === "Escape") {
    tzDropdownOpen.value = false;
    (e.target as HTMLInputElement)?.blur();
  }
}

function scrollHighlightedIntoView() {
  nextTick(() => {
    const el = tzDropdownRef.value?.querySelector(".tz-option.highlighted");
    if (el) el.scrollIntoView({ block: "nearest" });
  });
}
</script>

<template>
  <div class="preferences-view" data-testid="preferences-view">
    <header class="prefs-header">
      <h1>Preferences</h1>
      <button class="prefs-close" data-testid="prefs-close" @click="close">&times;</button>
    </header>

    <div class="prefs-body">
      <nav class="prefs-nav" aria-label="Preferences sections">
        <button
          v-for="s in sections"
          :key="s.id"
          class="prefs-nav-item"
          :class="{ active: activeSection === s.id }"
          :data-testid="`prefs-nav-${s.id}`"
          @click="activeSection = s.id"
        >
          {{ s.label }}
        </button>
      </nav>

      <main class="prefs-detail">
        <!-- General -->
        <section v-if="activeSection === 'general'" data-testid="prefs-section-general">
          <h2>General</h2>
          <div class="prefs-row">
            <label class="prefs-label">Theme</label>
            <div class="prefs-radio-group">
              <button
                v-for="opt in themeOptions"
                :key="opt.value"
                class="prefs-radio"
                :class="{ active: uiStore.theme === opt.value }"
                :data-testid="`prefs-theme-${opt.value}`"
                @click="uiStore.setTheme(opt.value)"
              >{{ opt.label }}</button>
            </div>
          </div>
          <div class="prefs-row">
            <label class="prefs-label">Hide title bar</label>
            <div class="prefs-toggle">
              <input
                type="checkbox"
                :checked="!uiStore.decorationsEnabled"
                data-testid="prefs-hide-title-bar"
                @change="(e) => uiStore.setDecorations(!(e.target as HTMLInputElement).checked)"
              />
            </div>
          </div>
        </section>

        <!-- Date and Time -->
        <section v-if="activeSection === 'date-time'" data-testid="prefs-section-date-time">
          <h2>Date and Time</h2>
          <div class="prefs-row">
            <label class="prefs-label">Week starts on</label>
            <div class="prefs-radio-group">
              <button
                v-for="opt in weekStartOptions"
                :key="opt.day"
                class="prefs-radio"
                :class="{ active: uiStore.weekStartDay === opt.day }"
                :data-testid="`prefs-week-start-${opt.day}`"
                @click="uiStore.setWeekStartDay(opt.day)"
              >{{ opt.label }}</button>
            </div>
          </div>
          <div class="prefs-row">
            <label class="prefs-label">Time format</label>
            <div class="prefs-radio-group">
              <button
                v-for="opt in timeFormatOptions"
                :key="opt.value"
                class="prefs-radio"
                :class="{ active: uiStore.timeFormat === opt.value }"
                :data-testid="`prefs-time-format-${opt.value}`"
                @click="uiStore.setTimeFormat(opt.value)"
              >{{ opt.label }}</button>
            </div>
          </div>
          <div class="prefs-row">
            <label class="prefs-label" for="prefs-tz">Display timezone</label>
            <div class="tz-picker">
              <input
                id="prefs-tz"
                type="text"
                class="tz-search-input"
                :placeholder="uiStore.displayTimezone"
                :value="tzDropdownOpen ? tzSearch : ''"
                @input="onTzInput($event)"
                @focus="onTzInputFocus"
                @blur="onTzInputBlur"
                @keydown="onTzKeydown"
                aria-label="Display timezone"
                role="combobox"
                :aria-expanded="tzDropdownOpen"
                aria-controls="prefs-tz-listbox"
                aria-autocomplete="list"
                :aria-activedescendant="tzHighlightIndex >= 0 ? `prefs-tz-opt-${tzHighlightIndex}` : undefined"
                data-testid="prefs-timezone-search"
              />
              <div
                v-if="tzDropdownOpen"
                ref="tzDropdownRef"
                id="prefs-tz-listbox"
                role="listbox"
                aria-label="Timezones"
                class="tz-dropdown"
                data-testid="prefs-timezone-dropdown"
              >
                <button
                  v-for="(tz, idx) in filteredTimezones"
                  :key="tz"
                  :id="`prefs-tz-opt-${idx}`"
                  role="option"
                  :aria-selected="tz === uiStore.displayTimezone"
                  class="tz-option"
                  :class="{ active: tz === uiStore.displayTimezone, highlighted: idx === tzHighlightIndex }"
                  @mousedown.prevent="selectTimezone(tz)"
                  @mouseenter="tzHighlightIndex = idx"
                  :data-testid="`prefs-timezone-option-${tz}`"
                >
                  {{ tz }}
                </button>
                <div v-if="filteredTimezones.length === 0" class="tz-empty">
                  No matching timezones
                </div>
              </div>
            </div>
          </div>
        </section>
      </main>
    </div>
  </div>
</template>

<style scoped>
.preferences-view {
  display: flex;
  flex-direction: column;
  height: 100%;
  background: var(--color-bg);
  color: var(--color-text);
}

.prefs-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 20px;
  border-bottom: 1px solid var(--color-border);
}

.prefs-header h1 {
  margin: 0;
  font-size: 16px;
  font-weight: 600;
}

.prefs-close {
  background: none;
  border: none;
  color: var(--color-text-muted);
  font-size: 22px;
  cursor: pointer;
  padding: 0 6px;
}

.prefs-close:hover {
  color: var(--color-text);
}

.prefs-body {
  flex: 1;
  display: grid;
  grid-template-columns: 200px 1fr;
  min-height: 0;
}

.prefs-nav {
  display: flex;
  flex-direction: column;
  border-right: 1px solid var(--color-border);
  padding: 12px 0;
  background: var(--color-bg-secondary);
}

.prefs-nav-item {
  background: none;
  border: none;
  text-align: left;
  padding: 8px 20px;
  font-size: 13px;
  color: var(--color-text-secondary);
  cursor: pointer;
  border-left: 3px solid transparent;
}

.prefs-nav-item:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.prefs-nav-item.active {
  color: var(--color-text);
  border-left-color: var(--color-accent);
  background: var(--color-bg-hover);
}

.prefs-detail {
  padding: 20px 28px;
  overflow-y: auto;
}

.prefs-detail h2 {
  margin: 0 0 16px;
  font-size: 14px;
  font-weight: 600;
}

.prefs-row {
  display: grid;
  grid-template-columns: 180px 1fr;
  align-items: center;
  gap: 16px;
  padding: 10px 0;
}

.prefs-label {
  font-size: 13px;
  color: var(--color-text-secondary);
}

.prefs-radio-group {
  display: flex;
  gap: 6px;
  flex-wrap: wrap;
}

.prefs-radio {
  padding: 5px 14px;
  font-size: 12px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  color: var(--color-text-secondary);
  cursor: pointer;
}

.prefs-radio:hover {
  background: var(--color-bg-hover);
}

.prefs-radio.active {
  border-color: var(--color-accent);
  color: var(--color-accent);
  font-weight: 600;
}

.prefs-toggle input {
  width: 16px;
  height: 16px;
  cursor: pointer;
}

/* Timezone picker (mirrors the calendar-sidebar styling, kept local) */
.tz-picker {
  position: relative;
  max-width: 320px;
  width: 100%;
}

.tz-search-input {
  width: 100%;
  padding: 5px 10px;
  font-size: 12px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  color: var(--color-text);
  outline: none;
  box-sizing: border-box;
}

.tz-search-input:focus {
  border-color: var(--color-accent);
}

.tz-search-input::placeholder {
  color: var(--color-text-secondary);
}

.tz-dropdown {
  position: absolute;
  top: 100%;
  left: 0;
  right: 0;
  max-height: 220px;
  overflow-y: auto;
  background: var(--color-bg-secondary);
  border: 1px solid var(--color-border);
  border-radius: 4px;
  margin-top: 2px;
  z-index: 50;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.15);
}

.tz-option {
  display: block;
  width: 100%;
  padding: 5px 10px;
  text-align: left;
  font-size: 12px;
  color: var(--color-text-secondary);
  background: none;
  border: none;
  cursor: pointer;
}

.tz-option:hover,
.tz-option.highlighted {
  background: var(--color-bg-hover);
}

.tz-option.active {
  color: var(--color-accent);
  font-weight: 600;
}

.tz-empty {
  padding: 8px;
  font-size: 12px;
  color: var(--color-text-muted);
  text-align: center;
}
</style>
