<template>
  <Teleport to="body">
    <div v-if="visible" class="dpm-mask" @click.self="$emit('close')" @keydown.esc="$emit('close')">
      <div class="dpm">
        <div class="dpm-head">
          <h2>自定义时间范围</h2>
          <button class="dpm-close" @click="$emit('close')">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M18 6 6 18M6 6l12 12"/></svg>
          </button>
        </div>

        <!-- 模式切换 -->
        <div class="dpm-tabs">
          <button
            v-for="t in tabs"
            :key="t.v"
            class="dpm-tab"
            :class="{ active: mode === t.v }"
            @click="switchMode(t.v)"
          >{{ t.label }}</button>
        </div>

        <!-- 日历 -->
        <div class="dpm-calendar">
          <CalendarMonth
            :year="viewYear"
            :month="viewMonth"
            :mode="mode"
            :dateStart="dateStart"
            :dateEnd="dateEnd"
            @select="onDateSelect"
            @prev-month="prevMonth"
            @next-month="nextMonth"
          />
        </div>

        <!-- 时间选择器（周模式隐藏） -->
        <div v-if="mode !== 'week'" class="dpm-time">
          <TimePicker
            v-model:hour="timeHour"
            v-model:minute="timeMinute"
          />
        </div>

        <!-- 选中范围提示 -->
        <div v-if="dateStart" class="dpm-range-hint">
          {{ rangeHint }}
        </div>

        <!-- 底部按钮 -->
        <div class="dpm-footer">
          <button class="dpm-btn" @click="$emit('close')">取消</button>
          <button class="dpm-btn dpm-btn--primary" :disabled="!canConfirm" @click="onConfirm">确认</button>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<script setup>
import { ref, computed, watch } from 'vue'
import CalendarMonth from './CalendarMonth.vue'
import TimePicker from './TimePicker.vue'

const props = defineProps({
  visible: { type: Boolean, default: false },
})

const emit = defineEmits(['close', 'confirm'])

const tabs = [
  { v: 'date', label: '日期' },
  { v: 'week', label: '周' },
  { v: 'range', label: '日期范围' },
]

const mode = ref('date')
const viewYear = ref(new Date().getFullYear())
const viewMonth = ref(new Date().getMonth())
const timeHour = ref(0)
const timeMinute = ref(0)
const dateStart = ref(null)
const dateEnd = ref(null)
const rangeClickCount = ref(0)

// 打开时重置状态
watch(() => props.visible, (v) => {
  if (v) {
    const now = new Date()
    viewYear.value = now.getFullYear()
    viewMonth.value = now.getMonth()
    timeHour.value = now.getHours()
    timeMinute.value = Math.floor(now.getMinutes() / 5) * 5
    dateStart.value = null
    dateEnd.value = null
    rangeClickCount.value = 0
    mode.value = 'date'
  }
})

function switchMode(v) {
  mode.value = v
  dateStart.value = null
  dateEnd.value = null
  rangeClickCount.value = 0
}

function prevMonth() {
  if (viewMonth.value === 0) {
    viewMonth.value = 11
    viewYear.value--
  } else {
    viewMonth.value--
  }
}

function nextMonth() {
  if (viewMonth.value === 11) {
    viewMonth.value = 0
    viewYear.value++
  } else {
    viewMonth.value++
  }
}

function onDateSelect(date) {
  if (mode.value === 'range') {
    if (rangeClickCount.value === 0) {
      dateStart.value = date
      dateEnd.value = null
      rangeClickCount.value = 1
    } else {
      // 第二次点击
      if (date.getTime() < dateStart.value.getTime()) {
        dateEnd.value = dateStart.value
        dateStart.value = date
      } else {
        dateEnd.value = date
      }
      rangeClickCount.value = 2
    }
  } else {
    dateStart.value = date
    dateEnd.value = null
    rangeClickCount.value = 1
    // 周模式自动计算整周范围
    if (mode.value === 'week') {
      const mon = getMonday(date)
      const sun = new Date(mon)
      sun.setDate(sun.getDate() + 6)
      dateStart.value = mon
      dateEnd.value = sun
    }
  }
}

function getMonday(date) {
  const d = new Date(date)
  const day = (d.getDay() + 6) % 7
  d.setDate(d.getDate() - day)
  d.setHours(0, 0, 0, 0)
  return d
}

const canConfirm = computed(() => {
  if (!dateStart.value) return false
  if (mode.value === 'range') return !!dateEnd.value
  return true
})

function fmtDate(d) {
  if (!d) return ''
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`
}

function fmtTime(h, m) {
  return `${String(h).padStart(2, '0')}:${String(m).padStart(2, '0')}`
}

const rangeHint = computed(() => {
  if (!dateStart.value) return ''
  if (mode.value === 'week') {
    return `已选择：${fmtDate(dateStart.value)} ~ ${fmtDate(dateEnd.value)}`
  }
  if (mode.value === 'range' && dateEnd.value) {
    return `已选择：${fmtDate(dateStart.value)} ~ ${fmtDate(dateEnd.value)}`
  }
  return `已选择：${fmtDate(dateStart.value)} ${fmtTime(timeHour.value, timeMinute.value)}`
})

function onConfirm() {
  if (!canConfirm.value) return
  let startMs, endMs, range

  if (mode.value === 'date') {
    startMs = dateStart.value.getTime()
    startMs = startMs - (startMs % 86400000) // 零点
    startMs += timeHour.value * 3600000 + timeMinute.value * 60000
    endMs = startMs + 86400000
    range = 'custom-date'
  } else if (mode.value === 'week') {
    startMs = dateStart.value.getTime() // 已经是周一零点
    endMs = dateEnd.value.getTime() + 86400000 // 周日 + 1 天
    range = 'custom-week'
  } else {
    startMs = dateStart.value.getTime()
    startMs = startMs - (startMs % 86400000)
    startMs += timeHour.value * 3600000 + timeMinute.value * 60000
    endMs = dateEnd.value.getTime() + 86400000
    range = 'custom-range'
  }

  if (endMs > startMs) {
    emit('confirm', { range, startMs, endMs })
  }
}
</script>

<style scoped>
.dpm-mask {
  position: fixed; inset: 0;
  background: rgba(5, 10, 20, .6);
  backdrop-filter: blur(4px);
  display: flex; align-items: center; justify-content: center;
  z-index: 200;
  animation: dpmFadeIn .15s ease;
}
@keyframes dpmFadeIn { from { opacity: 0; } to { opacity: 1; } }

.dpm {
  width: 400px; max-width: calc(100vw - 48px);
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 12px;
  padding: 20px 24px 16px;
  box-shadow: 0 16px 48px rgba(0,0,0,.45);
  animation: dpmSlideUp .2s ease;
}
@keyframes dpmSlideUp { from { transform: translateY(12px); opacity: 0; } to { transform: translateY(0); opacity: 1; } }

.dpm-head {
  display: flex; align-items: center; justify-content: space-between;
  margin-bottom: 14px;
}
.dpm-head h2 { font-size: 15px; font-weight: 600; color: var(--text); margin: 0; }
.dpm-close {
  display: flex; align-items: center; justify-content: center;
  width: 28px; height: 28px; border-radius: 6px;
  border: none; background: var(--border); color: var(--muted);
  cursor: pointer; transition: background .12s, color .12s;
}
.dpm-close:hover { background: var(--border); color: var(--text); filter: brightness(1.2); }

/* 模式切换 */
.dpm-tabs {
  display: flex; gap: 4px;
  background: var(--bg);
  border-radius: 8px;
  padding: 3px;
  margin-bottom: 14px;
}
.dpm-tab {
  flex: 1; height: 30px;
  border: none; border-radius: 6px;
  background: transparent; color: var(--muted);
  font-size: 12px; font-weight: 500; cursor: pointer;
  transition: background .15s, color .15s;
}
.dpm-tab:hover { color: var(--text); }
.dpm-tab.active {
  background: var(--blue); color: #fff; font-weight: 600;
}

/* 日历容器 */
.dpm-calendar {
  margin-bottom: 10px;
}

/* 时间选择器 */
.dpm-time {
  display: flex; justify-content: center;
  padding: 8px 0;
  border-top: 1px solid var(--border);
  margin-top: 10px;
}

/* 范围提示 */
.dpm-range-hint {
  text-align: center;
  font-size: 12px; color: var(--muted);
  padding: 6px 0 0;
}

/* 底部按钮 */
.dpm-footer {
  display: flex; justify-content: flex-end; gap: 8px;
  padding-top: 12px;
  margin-top: 10px;
  border-top: 1px solid var(--border);
}
.dpm-btn {
  padding: 6px 18px; border-radius: 6px;
  font-size: 13px; cursor: pointer;
  border: 1px solid transparent;
  background: var(--border); color: var(--text);
  transition: filter .1s;
}
.dpm-btn:hover { filter: brightness(1.2); }
.dpm-btn:disabled { opacity: .4; cursor: not-allowed; }
.dpm-btn--primary { background: var(--blue); color: #fff; }
.dpm-btn--primary:hover { filter: brightness(1.15); }
</style>
