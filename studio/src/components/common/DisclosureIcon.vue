<script setup lang="ts">
// Disclosure chevron for a collapsible section: points right, turns down when open.
defineProps<{ open: boolean }>()
</script>

<template>
  <svg
    class="disclosure"
    :class="{ 'disclosure--open': open }"
    width="10"
    height="10"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="2.5"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
  >
    <path d="m9 18 6-6-6-6" />
  </svg>
</template>

<style scoped>
.disclosure {
  flex-shrink: 0;
  vertical-align: -1px;
  transition: transform 0.15s;
}
.disclosure--open {
  transform: rotate(90deg);
}
</style>
