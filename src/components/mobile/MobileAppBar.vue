<script setup lang="ts">
defineProps<{
  title?: string;
  subtitle?: string;
  large?: boolean;
}>();
</script>

<template>
  <header class="mobile-app-bar" :class="{ large }">
    <div class="row">
      <div class="slot leading">
        <slot name="leading" />
      </div>
      <div v-if="!large" class="title">{{ title }}</div>
      <div v-if="large" class="spacer" />
      <div class="slot trailing">
        <slot name="trailing" />
      </div>
    </div>
    <div v-if="large" class="large-title">
      <h1>{{ title }}</h1>
      <div v-if="subtitle" class="subtitle">{{ subtitle }}</div>
    </div>
  </header>
</template>

<style scoped>
.mobile-app-bar {
  flex-shrink: 0;
  background: var(--color-bg);
  border-bottom: 1px solid var(--color-divider, #e9e0cd);
  padding: 10px 8px;
  padding-top: max(10px, env(safe-area-inset-top));
}

.mobile-app-bar.large {
  padding-bottom: 6px;
}

.row {
  min-height: var(--touch-min);
  display: flex;
  align-items: center;
  gap: 4px;
}

.title {
  flex: 1;
  text-align: center;
  font-size: 17px;
  font-weight: 600;
  letter-spacing: -0.3px;
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.spacer {
  flex: 1;
}

.slot {
  display: flex;
  align-items: center;
  min-width: var(--touch-min);
}

.slot.trailing {
  justify-content: flex-end;
  gap: 2px;
}

.large-title {
  padding: 4px 12px 8px;
}

.large-title h1 {
  margin: 0;
  font-size: 34px;
  font-weight: 700;
  letter-spacing: -0.8px;
  line-height: 1.05;
  color: var(--color-text);
}

.large-title .subtitle {
  margin-top: 2px;
  font-size: 13px;
  color: var(--color-text-muted);
}

[data-platform="android"] .title {
  text-align: left;
  padding-left: 4px;
  font-size: 20px;
  font-weight: 500;
}

[data-platform="android"] .large-title h1 {
  font-size: 28px;
  font-weight: 500;
  letter-spacing: 0;
}
</style>
