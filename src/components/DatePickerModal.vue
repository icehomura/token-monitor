<template>
  <Teleport to="body">
    <transition name="dpm-fade" appear>
      <div v-if="visible" class="dpm-mask" @click.self="$emit('close')" @keydown.escape="$emit('close')">
        <div class="dpm" @click.stop>
          <!-- 模式切换 -->
          <div class="dpm-tabs">
            <button v-for="t in modes" :key="t.k" :class="['dpm-tab', { active: mode === t.k }]" @click="switchMode(t.k)">
              {{ t.l }}
            </button>
          </div>

          <!-- 结果预览 -->
          <div class="dpm-range">
            <template v-if="isRange">
              <span :class="['dpm-date', 'active']" @click="activeField = 'start'">{{ range.start }}</span>
              <span class="dpm-sep">→</span>
              <span :class="['dpm-date', { active: activeField === 'end', [selectedDateClass]: endMatch }]">{{ range.end }}</span>
            </template>
            <template v-else>
              <span>{{ range.start }}</span>
              <span class="dpm-sep">{{ mode === 'month' ? '整月' : '～' }}</span>
              <span>{{ range.end }}</span>
            </template>
          </div>

          <!-- 日历 / 月份 -->
          <div class="dpm-cal-wrap">
            <div class="dpm-cal-header">
              <button class="dpm-arrow" @click="prev">&lt;</button>
              <span class="dpm-cal-title">{{ calTitle }}</span>
              <button class="dpm-arrow" @click="next">&gt;</button>
            </div>

            <div v-if="showMonthGrid" class="dpm-month-grid">
              <button v-for="m in 12" :key="m" :class="['dpm-month-item', { active: viewMonth === m }]" @click="viewMonth = m">
                {{ m }}月
              </button>
            </div>

            <div v-else class="dpm-day-grid">
              <div v-for="w in dayHeaders" :key="w" class="dpm-weekday">{{ w }}</div>
              <button
                v-for="(d, i) in calendarDays"
                :key="i"
                :class="dayClass(d)"
                :disabled="d.disabled"
                @click="selectDate(d)"
              >
                {{ d.day }}
              </button>
            </div>
          </div>

          <!-- 时间（仅范围内模式） -->
          <div v-if="isRange" class="dpm-time-row">
            <div class="dpm-time-field">
              <span class="dpm-time-label" :class="{ active: activeField === 'start' }" @click="activeField = 'start'">开始时间</span>
              <div class="dpm-time-select">
                <select :value="startH" @change="startH = +$event.target.value" class="dpm-time-sel">
                  <option v-for="h in 24" :key="h-1" :value="h-1">{{ String(h-1).padStart(2,'0') }}</option>
                </select>
                <span class="dpm-time-colon">:</span>
                <select :value="startM" @change="startM = +$event.target.value" class="dpm-time-sel">
                  <option v-for="m in [0,5,10,15,20,25,30,35,40,45,50,55]" :key="m" :value="m">{{ String(m).padStart(2,'0') }}</option>
                </select>
              </div>
            </div>
            <div class="dpm-time-field">
              <span class="dpm-time-label" :class="{ active: activeField === 'end' }" @click="activeField = 'end'">结束时间</span>
              <div class="dpm-time-select">
                <select :value="endH" @change="endH = +$event.target.value" class="dpm-time-sel">
                  <option v-for="h in 24" :key="h-1" :value="h-1">{{ String(h-1).padStart(2,'0') }}</option>
                </select>
                <span class="dpm-time-colon">:</span>
                <select :value="endM" @change="endM = +$event.target.value" class="dpm-time-sel">
                  <option v-for="m in [0,5,10,15,20,25,30,35,40,45,50,55]" :key="m" :value="m">{{ String(m).padStart(2,'0') }}</option>
                </select>
              </div>
            </div>
          </div>

          <!-- 页脚 -->
          <div class="dpm-footer">
            <button class="dpm-btn dpm-btn-cancel" @click="$emit('close')">取消</button>
            <button class="dpm-btn dpm-btn-ok" @click="onConfirm">确认</button>
          </div>
        </div>
      </div>
    </transition>
  </Teleport>
</template>

<script setup>
import { ref, computed, watch } from 'vue'

const props = defineProps({ visible: Boolean })
const emit = defineEmits(['close', 'confirm'])

const modes = [
  { k: 'day',  l: '日期' },
  { k: 'week', l: '周' },
  { k: 'month', l: '月份' },
  { k: 'range', l: '指定范围' },
]

const mode     = ref('day')
const year     = ref(0)
const month    = ref(0)
const dateStart = ref(null)
const dateEnd   = ref(null)
const startH = ref(0), startM = ref(0)
const endH   = ref(0), endM   = ref(0)
const activeField = ref('start')

const dayHeaders = ['一','二','三','四','五','六','日']

// ---- Watch: 打开重置 ----
watch(() => props.visible, v => {
  if (!v) return
  const d = new Date()
  const hh = d.getHours(), mm = d.getMinutes()
  const sm = Math.floor(mm / 5) * 5
  const em = Math.min(sm + 5, 55)

  mode.value = 'day'
  year.value = d.getFullYear()
  month.value = d.getMonth() + 1
  dateStart.value = toDate(d)
  dateEnd.value = null
  startH.value = hh; startM.value = sm
  endH.value = hh; endM.value = em
  activeField.value = 'start'
})

function switchMode(v) {
  mode.value = v
  if (v === 'month') dateStart.value = dateEnd.value = null
  else if (v === 'range') { dateStart.value = dateEnd.value = null }
  else { dateStart.value = toDate(new Date()); dateEnd.value = null }
}

// ---- 导航 ----
function prev() {
  if (showMonthGrid.value) { month.value--; if (month.value < 1) { month.value = 12; year.value-- } return }
  month.value--; if (month.value < 1) { month.value = 12; year.value-- }
}
function next() {
  if (showMonthGrid.value) { month.value++; if (month.value > 12) { month.value = 1; year.value++ } return }
  month.value++; if (month.value > 12) { month.value = 1; year.value++ }
}

const showMonthGrid = computed(() => mode.value === 'month')
const isRange = computed(() => mode.value === 'range')
const selectedDateClass = computed(() => mode.value === 'range' ? 'hl-mid' : 'hl-start')
const endMatch = computed(() => {
  if (!dateEnd.value) return false
  if (mode.value === 'range') return dateEnd.value === toDate(dateEnd.value)
  if (mode.value === 'week') return true
  return dateStart.value === dateEnd.value
})
const calTitle = computed(() => showMonthGrid.value ? `${year.value}年` : `${year.value}年${month.value}月`)

// ---- 日历计算 ----
const calendarDays = computed(() => {
  const y = year.value, m = month.value
  const firstDow = new Date(y, m - 1, 1).getDay()
  const startIdx = firstDow === 0 ? 6 : firstDow - 1
  const daysInMonth = new Date(y, m, 0).getDate()

  const days = []
  for (let i = startIdx; i > 0; i--) {
    const d = new Date(y, m - 1, 1 - i)
    days.push({ day: d.getDate(), date: toDate(d), disabled: true })
  }
  for (let d = 1; d <= daysInMonth; d++) {
    days.push({ day: d, date: toDate(new Date(y, m - 1, d)), disabled: false })
  }
  // 补至 42 格
  while (days.length % 7 !== 0) {
    days.push({ day: days.length - startIdx - daysInMonth + 1, date: '', disabled: true })
  }
  if (days.length <= 35) for (let i = 1; i <= 7; i++) days.push({ day: i, date: '', disabled: true })
  return days
})

function dayClass(d) {
  if (!d.date) return 'dpm-day disabled'
  const cls = ['dpm-day']
  if (d.date === dateStart.value) { cls.push('hl-start'); return cls.join(' ') }

  if (mode.value === 'week' && dateStart.value) {
    const ms = toMonday(dateStart.value)
    const me = addDays(ms, 6)
    if (d.date >= ms && d.date <= me) { cls.push(d.date === ms ? 'hl-start' : d.date === me ? 'hl-end' : 'hl-mid'); return cls.join(' ') }
  } else if (mode.value === 'range') {
    if (dateStart.value && dateEnd.value) {
      const ms = Math.min(dateStart.value, dateEnd.value)
      const me = Math.max(dateStart.value, dateEnd.value)
      if (d.date >= ms && d.date <= me) { cls.push(d.date === ms ? 'hl-start' : d.date === me ? 'hl-end' : 'hl-mid'); return cls.join(' ') }
    } else if (d.date === dateStart.value) { cls.push('hl-start'); return cls.join(' ') }
  } else {
    if (d.date === dateStart.value) cls.push('hl-start')
  }
  return cls.join(' ')
}

function selectDate(d) {
  if (d.disabled || !d.date) return
  // 月份模式
  if (mode.value === 'month') {
    month.value = parseInt(d.date.slice(5, 7), 10)
    return
  }
  // 日期 / 周
  if (mode.value !== 'range') {
    dateStart.value = d.date
    dateEnd.value = null
    return
  }
  // 范围模式
  if (activeField.value === 'start') {
    dateStart.value = d.date
    if (!dateEnd.value || dateEnd.value < dateStart.value) dateEnd.value = dateStart.value
    activeField.value = 'end'
  } else {
    dateEnd.value = d.date
    if (dateEnd.value < dateStart.value) { const tmp = dateStart.value; dateStart.value = dateEnd.value; dateEnd.value = tmp }
    activeField.value = 'start'
  }
}

// ---- 范围预览 ----
const range = computed(() => {
  const fmt = v => v ? `${v.slice(0,4)}-${v.slice(5,7)}-${v.slice(8,10)}` : '—'
  const fmtT = (v, h, m) => v ? `${fmt(v)} ${String(h).padStart(2,'0')}:${String(m).padStart(2,'0')}` : '—'

  if (mode.value === 'day')   return { start: fmt(dateStart.value), end: fmt(addDays(dateStart.value, 1)) }
  if (mode.value === 'week') {
    if (!dateStart.value) return { start: '—', end: '—' }
    const m = toMonday(dateStart.value)
    return { start: fmt(m), end: fmt(addDays(m, 7)) }
  }
  if (mode.value === 'month') {
    if (!dateStart.value) return { start: `${year.value}-${String(month.value).padStart(2,'0')}-01`, end: fmt(addDays(new Date(year.value, month.value, 0).toISOString().slice(0,10), 1)) }
    return { start: fmt(dateStart.value), end: fmt(addDays(dateStart.value, 1)) }
  }
  return { start: fmtT(dateStart.value, startH.value, startM.value), end: fmtT(dateEnd.value, endH.value, endM.value) }
})

// ---- 确认 ----
function onConfirm() {
  if (mode.value === 'range' && (!dateStart.value || !dateEnd.value)) return

  const day = dateStart.value || toDate(new Date())
  let startMs, endMs

  if (mode.value === 'day') {
    startMs = new Date(day + 'T00:00:00').getTime()
    endMs   = new Date(day + 'T00:00:00').getTime() + 86400000
  } else if (mode.value === 'week') {
    const m = toMonday(day)
    startMs = new Date(m + 'T00:00:00').getTime()
    endMs   = new Date(m + 'T00:00:00').getTime() + 604800000
  } else if (mode.value === 'month') {
    const m = month.value
    startMs = new Date(year.value, m - 1, 1).getTime()
    endMs   = new Date(year.value, m, 1).getTime()
  } else {
    startMs = new Date(dateStart.value + 'T' + String(startH.value).padStart(2,'0') + ':' + String(startM.value).padStart(2,'0') + ':00').getTime()
    endMs   = new Date(dateEnd.value   + 'T' + String(endH.value).padStart(2,'0')   + ':' + String(endM.value).padStart(2,'0')   + ':00').getTime()
    if (endMs <= startMs) endMs = startMs + 3600000
  }
  emit('confirm', { startMs, endMs })
}

// ---- 工具函数 ----
function toDate(d) {
  if (typeof d === 'string') return d
  return d.getFullYear() + '-' + String(d.getMonth()+1).padStart(2,'0') + '-' + String(d.getDate()).padStart(2,'0')
}
function addDays(s, n) {
  if (!s) return ''
  const d = new Date(s + 'T00:00:00')
  d.setDate(d.getDate() + n)
  return toDate(d)
}
function toMonday(date) {
  const d = new Date(date + 'T00:00:00')
  const dow = d.getDay()
  const diff = (dow === 0 ? -6 : 1 - dow)
  d.setDate(d.getDate() + diff)
  return toDate(d)
}
</script>

<style scoped>
/* ── 遮罩 + 面板 ── */
.dpm-mask {
  position: fixed; inset: 0; z-index: 1000;
  display: flex; align-items: center; justify-content: center;
  background: rgba(0,0,0,0.45); backdrop-filter: blur(4px);
}
.dpm {
  background: var(--panel); border: 1px solid var(--border);
  border-radius: 12px; padding: 18px;
  width: 360px; max-height: 80vh; overflow-y: auto;
  box-shadow: 0 8px 24px rgba(0,0,0,0.35);
  animation: dpmSlideUp .22s ease-out;
}
@keyframes dpmSlideUp {
  from { opacity: 0; transform: translateY(12px) scale(0.97); }
  to   { opacity: 1; transform: translateY(0) scale(1); }
}

/* ── 模式选项卡 ── */
.dpm-tabs {
  display: flex; gap: 4px;
  background: var(--bg); border-radius: 8px; padding: 3px;
  margin-bottom: 14px;
}
.dpm-tab {
  flex: 1; padding: 5px 0; border: none; border-radius: 6px;
  background: transparent; font-size: 13px; font-weight: 500;
  color: var(--muted); cursor: pointer; transition: all .15s;
}
.dpm-tab.active {
  background: var(--blue); color: #fff; box-shadow: 0 1px 4px rgba(0,0,0,0.25);
}

/* ── 范围预览 ── */
.dpm-range {
  display: flex; align-items: center; justify-content: center;
  gap: 10px; margin-bottom: 14px; font-size: 14px; font-weight: 600;
}
.dpm-date {
  padding: 3px 8px; border-radius: 6px; cursor: pointer; transition: background .12s;
}
.dpm-date.active,
.dpm-date.active { background: var(--blue); color: #fff; }
.dpm-sep { color: var(--muted); font-size: 13px; font-weight: 400; }

/* ── 日历区域 ── */
.dpm-cal-header {
  display: flex; align-items: center; justify-content: space-between;
  margin-bottom: 8px;
}
.dpm-cal-title { font-size: 14px; font-weight: 600; }
.dpm-arrow {
  width: 28px; height: 28px; border: none; border-radius: 6px;
  background: transparent; font-size: 16px; cursor: pointer;
  display: flex; align-items: center; justify-content: center;
  color: var(--muted); transition: background .12s;
}
.dpm-arrow:hover { background: rgba(127,127,127,0.12); }

/* ── 月份网格 ── */
.dpm-month-grid {
  display: grid; grid-template-columns: repeat(4, 1fr); gap: 6px; margin-bottom: 12px;
}
.dpm-month-item {
  padding: 8px 0; border: none; border-radius: 8px;
  background: var(--bg); font-size: 13px; font-weight: 500;
  color: var(--muted); cursor: pointer; transition: all .12s;
}
.dpm-month-item.active {
  background: var(--blue); color: #fff; box-shadow: 0 1px 4px rgba(0,0,0,0.25);
}
.dpm-month-item:hover:not(.active) { background: rgba(127,127,127,0.12); }

/* ── 日历日期 ── */
.dpm-day-grid {
  display: grid; grid-template-columns: repeat(7, 1fr); gap: 2px;
  margin-bottom: 12px;
}
.dpm-weekday {
  text-align: center; font-size: 11px; font-weight: 500;
  color: var(--muted); padding: 4px 0;
}
.dpm-day {
  aspect-ratio: 1; display: flex; align-items: center; justify-content: center;
  border: none; border-radius: 8px; font-size: 13px; font-weight: 500;
  background: transparent; color: var(--text); cursor: pointer; transition: all .12s;
}
.dpm-day:not(.disabled):hover { background: rgba(127,127,127,0.10); }
.dpm-day.disabled { color: var(--muted); opacity: 0.35; cursor: default; }
/* 选中/区间高亮 */
.dpm-day.hl-start { background: var(--blue); color: #fff; border-radius: 8px 0 0 8px; }
.dpm-day.hl-mid   { background: rgba(0,122,255,0.18); color: var(--text); border-radius: 0; }
.dpm-day.hl-end   { background: var(--blue); color: #fff; border-radius: 0 8px 8px 0; }
.dpm-day.hl-start.hl-end { border-radius: 8px; }

/* ── 时间选择 ── */
.dpm-time-row {
  display: flex; gap: 16px; margin-bottom: 14px;
}
.dpm-time-field { display: flex; flex-direction: column; gap: 4px; flex: 1; }
.dpm-time-label {
  font-size: 11px; font-weight: 500; color: var(--muted); padding-left: 2px;
  cursor: pointer; transition: color .12s;
}
.dpm-time-label.active { color: var(--blue); }
.dpm-time-select { display: flex; align-items: center; gap: 3px; }
.dpm-time-sel {
  width: 52px; padding: 4px 0; border: 1px solid var(--border); border-radius: 6px;
  background: var(--bg); font-size: 13px; text-align: center;
  color: var(--text); cursor: pointer;
}
.dpm-time-colon { font-size: 14px; font-weight: 600; color: var(--muted); }

/* ── 页脚 ── */
.dpm-footer { display: flex; justify-content: flex-end; gap: 8px; margin-top: 14px; }
.dpm-btn {
  padding: 7px 22px; border: none; border-radius: 8px;
  font-size: 13px; font-weight: 500; cursor: pointer; transition: opacity .15s;
}
.dpm-btn-cancel { background: var(--bg); color: var(--muted); }
.dpm-btn-cancel:hover { opacity: 0.75; }
.dpm-btn-ok { background: var(--blue); color: #fff; }
.dpm-btn-ok:hover { opacity: 0.88; }

/* ── 过渡 ── */
.dpm-fade-enter-active { transition: opacity .18s; }
.dpm-fade-leave-active { transition: opacity .12s; }
.dpm-fade-enter-from, .dpm-fade-leave-to { opacity: 0; }
</style>
