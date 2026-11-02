<script setup lang="ts">
import { Palette, X } from "lucide-vue-next";

const props = defineProps<{ modelValue: number }>();
const emit = defineEmits<{ "update:modelValue": [color: number] }>();
const presets = [
  { color: 0xff69d9b1, name: "мятный" },
  { color: 0xff5ba8ff, name: "синий" },
  { color: 0xff9b87f5, name: "фиолетовый" },
  { color: 0xffff6f91, name: "розовый" },
  { color: 0xffff9f43, name: "оранжевый" },
  { color: 0xffffd166, name: "жёлтый" },
  { color: 0xff4dd0e1, name: "бирюзовый" },
  { color: 0xffa3d977, name: "зелёный" },
];
const channels = [
  { name: "R", shift: 16 },
  { name: "G", shift: 8 },
  { name: "B", shift: 0 },
];
const picker = ref<HTMLElement | null>(null);
const trigger = ref<HTMLButtonElement | null>(null);
const panel = ref<HTMLElement | null>(null);
const expanded = ref(false);
const panelId = useId();
const custom = computed(
  () => !presets.some(({ color }) => color === props.modelValue),
);
const hex = (color: number) =>
  `#${(color & 0xffffff).toString(16).padStart(6, "0")}`;

function close(restoreFocus = false) {
  expanded.value = false;
  if (restoreFocus) trigger.value?.focus();
}
async function toggle() {
  expanded.value = !expanded.value;
  if (expanded.value) {
    await nextTick();
    panel.value
      ?.querySelector<HTMLInputElement>('input[type="number"]')
      ?.focus();
  }
}
function choose(color: number) {
  emit("update:modelValue", color);
  close();
}
function setHex(event: Event) {
  const value = (event.target as HTMLInputElement).value;
  if (/^#[0-9a-f]{6}$/i.test(value))
    emit(
      "update:modelValue",
      (0xff000000 | Number.parseInt(value.slice(1), 16)) >>> 0,
    );
}
function setChannel(shift: number, event: Event) {
  const input = event.target as HTMLInputElement;
  if (input.value === "") return;
  const value = Math.max(0, Math.min(255, Math.round(input.valueAsNumber)));
  if (!Number.isFinite(value)) return;
  input.value = String(value);
  emit(
    "update:modelValue",
    ((props.modelValue & ~(0xff << shift)) | (value << shift) | 0xff000000) >>>
      0,
  );
}
function dismissOutside(event: PointerEvent) {
  if (event.target instanceof Node && !picker.value?.contains(event.target))
    close();
}
function leave(event: FocusEvent) {
  if (
    event.relatedTarget instanceof Node &&
    !picker.value?.contains(event.relatedTarget)
  )
    close();
}
onMounted(() => document.addEventListener("pointerdown", dismissOutside));
onBeforeUnmount(() =>
  document.removeEventListener("pointerdown", dismissOutside),
);
</script>

<template>
  <div class="colors">
    <button
      v-for="preset in presets"
      :key="preset.color"
      class="color"
      :class="{ chosen: modelValue === preset.color }"
      :style="{ background: hex(preset.color) }"
      :aria-label="`Цвет ${preset.name}`"
      :aria-pressed="modelValue === preset.color"
      @click="choose(preset.color)"
    >
      {{ modelValue === preset.color ? "✓" : "" }}
    </button>
    <div
      ref="picker"
      class="custom-picker"
      @focusout="leave"
      @keydown.esc.stop.prevent="close(true)"
    >
      <button
        ref="trigger"
        class="color palette-trigger"
        :class="{ chosen: custom }"
        aria-label="Свой цвет"
        title="Свой цвет"
        aria-haspopup="dialog"
        :aria-expanded="expanded"
        :aria-controls="expanded ? panelId : undefined"
        @click="toggle"
      >
        <Palette :size="22" aria-hidden="true" />
      </button>
      <div
        v-if="expanded"
        :id="panelId"
        ref="panel"
        class="color-popover"
        role="dialog"
        :aria-labelledby="`${panelId}-title`"
      >
        <div class="color-popover-heading">
          <strong :id="`${panelId}-title`">Свой цвет</strong>
          <button
            class="icon"
            aria-label="Закрыть палитру"
            @click="close(true)"
          >
            <X />
          </button>
        </div>
        <div class="color-channels">
          <label class="color-well"
            >Палитра<input
              type="color"
              :value="hex(modelValue)"
              aria-label="Палитра цвета"
              @input="setHex"
          /></label>
          <label
            v-for="channel in channels"
            :key="channel.name"
            class="rgb-field"
          >
            {{ channel.name }}
            <input
              type="number"
              min="0"
              max="255"
              step="1"
              :aria-label="`Цвет ${channel.name}`"
              :value="(modelValue >>> channel.shift) & 255"
              @input="setChannel(channel.shift, $event)"
            />
          </label>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.colors {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 10px;
  margin: 8px 0;
}
.color {
  width: 42px;
  height: 42px;
  flex: none;
  border-radius: 50%;
  border: 3px solid transparent;
  color: #10241d;
  font-size: 20px;
}
.color:hover {
  filter: brightness(1.08);
}
.color.chosen {
  border-color: var(--fg);
}
.custom-picker {
  position: relative;
  margin-left: auto;
}
.palette-trigger {
  display: grid;
  place-items: center;
  background: var(--panel);
  color: var(--accent-ink);
  border: 1px solid var(--line);
}
.palette-trigger.chosen {
  background: var(--accent);
  color: var(--accent-fg);
  border-width: 3px;
}
.palette-trigger[aria-expanded="true"] {
  outline: 2px solid var(--accent-ink);
  outline-offset: 3px;
}
.color-popover {
  position: absolute;
  top: calc(100% + 12px);
  right: 0;
  z-index: 5;
  width: 328px;
  padding: 16px;
  border: 1px solid var(--line);
  border-radius: 12px;
  background: var(--panel);
}
.color-popover-heading {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 12px;
}
.color-popover-heading .icon {
  width: 28px;
  height: 28px;
}
.color-channels {
  display: grid;
  grid-template-columns: 48px repeat(3, minmax(0, 1fr));
  gap: 10px;
}
.color-well,
.rgb-field {
  display: flex;
  flex-direction: column;
  gap: 8px;
  font-size: 12px;
  color: var(--muted);
}
.color-well input {
  width: 48px;
  height: var(--field-height);
  padding: 4px;
  border: 1px solid var(--line);
  border-radius: 8px;
  background: transparent;
  cursor: pointer;
}
.rgb-field input {
  width: 100%;
  padding-inline: 8px;
  font-variant-numeric: tabular-nums;
}
</style>
