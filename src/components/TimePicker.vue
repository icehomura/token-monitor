<template>
  <div class="time-picker">
    <div class="tp-col">
      <div class="tp-label">时</div>
      <div class="tp-scroll" ref="hourScroll">
        <div class="tp-pad" />
        <div
          v-for="h in 24"
          :key="h"
          class="tp-cell"
          :class="{ selected: h - 1 === hour }"
          @click="$emit('update:hour', h - 1)"
        >{{ String(h - 1).padStart(2, '0') }}</div>
        <div class="tp-pad" />
      </div>
    </div>
    <span class="tp-sep">:</span>
    <div class="tp-col">
      <div class="tp-label">分</div>
      <div class="tp-scroll" ref="minuteScroll">
        <div class="tp-pad" />
        <div
          v-for="m in minutes"
          :key="m"
          class="tp-cell"
          :class="{ selected: m === minute }"
          @click="$emit('update:minute', m)"
        >{{ String(m).padStart(2, '0') }}</div>
        <div class="tp-pad" />
      </div>
    </div>
  </div>
</template>

<script setup>
import { ref, watch, nextTick, onMounted } from 'vue'

const props = defineProps({
  hour: { type: Number, default: 0 },
  minute: { type: Number, default: 0 },
})

defineEmits(['update:hour', 'update:minute'])

const minutes = [0, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55]

const hourScroll = ref(null)
const minuteScroll = ref(null)

function scrollToSelected() {
  nextTick(() => {
    const cellH = 28
    if (hourScroll.value) {
      const idx = props.hour
      hourScroll.value.scrollTop = idx * cellH
    }
    if (minuteScroll.value) {
      const idx = minutes.indexOf(props.minute)
      if (idx >= 0) minuteScroll.value.scrollTop = idx * cellH
    }
  })
}

onMounted(scrollToSelected)
watch(() => props.hour, scrollToSelected)
watch(() => props.minute, scrollToSelected)
</script>

<style scoped>
.time-picker {
  display: flex;
  align-items: flex-start;
  gap: 4px;
  justify-content: center;
}
.tp-col {
  display: flex;
  flex-direction: column;
  align-items: center;
}
.tp-label {
  font-size: 11px;
  color: var(--muted);
  margin-bottom: 4px;
}
.tp-scroll {
  width: 56px;
  height: 140px;
  overflow-y: auto;
  scrollbar-width: none;
}
.tp-scroll::-webkit-scrollbar { display: none; }
.tp-pad {
  height: 56px; /* (140 - 28) / 2 = 56px padding for centering */
  flex-shrink: 0;
}
.tp-cell {
  height: 28px;
  line-height: 28px;
  text-align: center;
  font-size: 13px;
  color: var(--muted);
  border-radius: 4px;
  cursor: pointer;
  transition: background .12s, color .12s;
  flex-shrink: 0;
}
.tp-cell:hover {
  background: var(--border);
  color: var(--text);
}
.tp-cell.selected {
  background: var(--blue);
  color: #fff;
  font-weight: 600;
}
.tp-sep {
  font-size: 18px;
  font-weight: 600;
  color: var(--muted);
  line-height: 140px;
  padding: 0 2px;
}
</style>
