<script setup lang="ts">
defineOptions({ inheritAttrs: false });
const props = defineProps<{
  modelValue?: number;
  modelModifiers?: Record<string, boolean>;
  value?: number;
  min: number | string;
  max: number | string;
  step?: number | string;
}>();
const emit = defineEmits<{
  "update:modelValue": [value: number];
  input: [event: Event];
}>();
const current = computed(
  () => props.modelValue ?? props.value ?? Number(props.min),
);
const progress = computed(() => {
  const min = Number(props.min);
  const max = Number(props.max);
  return max > min
    ? Math.max(0, Math.min(100, ((current.value - min) / (max - min)) * 100))
    : 0;
});
function onInput(event: Event) {
  emit("update:modelValue", Number((event.target as HTMLInputElement).value));
  emit("input", event);
}
</script>

<template>
  <input
    v-bind="$attrs"
    type="range"
    :min="min"
    :max="max"
    :step="step"
    :value="current"
    :style="{ '--range-progress': `${progress}%` }"
    @input="onInput"
  />
</template>
