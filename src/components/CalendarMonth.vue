<template>
  <div class="calendar-month">
    <div class="cal-header">
      <button class="cal-nav" @click="$emit('prev-month')">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="m15 18-6-6 6-6"/></svg>
      </button>
      <span class="cal-title">{{ year }} 年 {{ month + 1 }} 月</span>
      <button class="cal-nav" @click="$emit('next-month')">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="m9 18 6-6-6-6"/></svg>
      </button>
    </div>

    <div class="cal-weekdays">
      <span v-for="d in weekdays" :key="d" class="cal-wd">{{ d }}</span>
    </div>

    <div class="cal-grid">
      <div
        v-for="(cell, i) in grid"
        :key="i"
        class="cal-cell"
        :class="cellClass(cell)"
        @click="cell.date && $emit('select', cell.date)"
      >
        <span v-if="cell.day" class="cal-day">{{ cell.day }}</span>
      </div>
    </div>
  </div>
</template>

<script setup>
import { computed } from 'vue'

const props = defineProps({
  year: { type: Number, required: true },
  month: { type: Number, required: true }, // 0-indexed
  mode: { type: String, default: 'date' }, // 'date' | 'week' | 'range'
  dateStart: { type: Date, default: null },
  dateEnd: { type: Date, default: null },
})

defineEmits(['select', 'prev-month', 'next-month'])

const weekdays = ['日', '一', '二', '三', '四', '五', '六']

const grid = computed(() => {
  const firstDay = new Date(props.year, props.month, 1)
  const daysInMonth = new Date(props.year, props.month + 1, 0).getDate()
  const startWeekday = firstDay.getDay() // 0=Sun
  const cells = []
  for (let i = 0; i < startWeekday; i++) cells.push({ day: 0, date: null })
  for (let d = 1; d <= daysInMonth; d++) cells.push({ day: d, date: new Date(props.year, props.month, d) })
  while (cells.length < 42) cells.push({ day: 0, date: null })
  return cells
})

function sameDay(a, b) {
  if (!a || !b) return false
  return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate()
}

function isToday(date) {
  return sameDay(date, new Date())
}

function getMonday(date) {
  const d = new Date(date)
  const day = (d.getDay() + 6) % 7 // Mon=0..Sun=6
  d.setDate(d.getDate() - day)
  d.setHours(0, 0, 0, 0)
  return d
}

function isInRange(date) {
  if (!props.dateStart || !date) return false
  if (props.mode === 'week') {
    const mon = getMonday(date)
    const sun = new Date(mon)
    sun.setDate(sun.getDate() + 6)
    const selMon = getMonday(props.dateStart)
    return mon.getTime() === selMon.getTime()
  }
  if (props.mode === 'range') {
    const start = props.dateStart.getTime()
    const end = props.dateEnd ? props.dateEnd.getTime() : start
    const t = date.getTime()
    const lo = Math.min(start, end)
    const hi = Math.max(start, end)
    return t >= lo && t <= hi
  }
  return false
}

function cellClass(cell) {
  if (!cell.date) return {}
  const date = cell.date
  const today = isToday(date)
  const selected = sameDay(date, props.dateStart) || sameDay(date, props.dateEnd)
  const inRange = isInRange(date)

  return {
    'cal-cell--empty': !cell.day,
    'cal-cell--today': today,
    'cal-cell--selected': selected,
    'cal-cell--in-range': inRange && !selected,
    'cal-cell--range-start': props.mode === 'range' && sameDay(date, props.dateStart),
    'cal-cell--range-end': props.mode === 'range' && props.dateEnd && sameDay(date, props.dateEnd),
  }
}
</script>

<style scoped>
.calendar-month {
  user-select: none;
}
.cal-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 10px;
}
.cal-nav {
  display: flex; align-items: center; justify-content: center;
  width: 28px; height: 28px;
  border: 1px solid var(--border); border-radius: 6px;
  background: var(--bg); color: var(--muted);
  cursor: pointer; transition: background .12s, color .12s;
}
.cal-nav:hover { background: var(--border); color: var(--text); }
.cal-title {
  font-size: 14px; font-weight: 600; color: var(--text);
}
.cal-weekdays {
  display: grid;
  grid-template-columns: repeat(7, 1fr);
  margin-bottom: 4px;
}
.cal-wd {
  text-align: center;
  font-size: 11px;
  color: var(--muted);
  padding: 4px 0;
}
.cal-grid {
  display: grid;
  grid-template-columns: repeat(7, 1fr);
  gap: 2px;
}
.cal-cell {
  display: flex; align-items: center; justify-content: center;
  height: 32px;
  border-radius: 6px;
  cursor: default;
  position: relative;
}
.cal-cell:not(.cal-cell--empty) { cursor: pointer; }
.cal-day {
  font-size: 13px;
  color: var(--text);
  width: 28px; height: 28px;
  display: flex; align-items: center; justify-content: center;
  border-radius: 6px;
  transition: background .12s, color .12s;
}
.cal-cell:not(.cal-cell--empty):hover .cal-day {
  background: var(--border);
}
/* 今日标记 */
.cal-cell--today .cal-day {
  box-shadow: inset 0 0 0 1px var(--blue);
}
/* 选中 */
.cal-cell--selected .cal-day {
  background: var(--blue) !important;
  color: #fff !important;
  font-weight: 600;
}
/* 范围中间 */
.cal-cell--in-range .cal-day {
  background: rgba(79, 140, 255, 0.1);
  border-radius: 0;
}
/* 范围起止圆角 */
.cal-cell--range-start .cal-day { border-radius: 6px 0 0 6px; }
.cal-cell--range-end .cal-day { border-radius: 0 6px 6px 0; }
.cal-cell--range-start.cal-cell--range-end .cal-day { border-radius: 6px; }
</style>
