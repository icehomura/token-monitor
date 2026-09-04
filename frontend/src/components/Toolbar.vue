<template>
  <header class="toolbar" ref="toolbarRef" @mousedown="tryDrag" @dblclick="onDblClick">
    <span class="endpoint" :title="endpointTitle">{{ endpointText }}</span>
    <IconButton class="copy-btn" title="复制 API 地址" @click="copyEndpoint">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <rect x="9" y="9" width="13" height="13" rx="2" />
        <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />
      </svg>
    </IconButton>
    <BaseButton class="test-btn" @click="sendTest">发送测试请求</BaseButton>
    <span class="test-status">{{ testStatus }}</span>
    <div class="toolbar-right">
      <span
        v-if="activeRangeText"
        class="active-range"
        title="点击编辑自定义范围"
        @click="openedFromDropdown = false; showDatePicker = true"
      >{{ activeRangeText }}</span>
      <DdSelect
        :options="rangeOptions"
        v-model="selectedRange"
      />
    </div>
    <DatePickerModal
      :visible="showDatePicker"
      @close="onDatePickerClose"
      @confirm="onDatePickerConfirm"
    />
    <CopyApiModal
      :visible="showCopyModal"
      :port="serverPort"
      @close="showCopyModal = false"
    />
  </header>
</template>

<script setup>
import { ref, watch, computed, onMounted, onBeforeUnmount } from 'vue'
import DdSelect from './DdSelect.vue'
import DatePickerModal from './DatePickerModal.vue'
import CopyApiModal from './CopyApiModal.vue'
import IconButton from './base/IconButton.vue'
import BaseButton from './base/BaseButton.vue'
import { useTauri } from '../composables/useTauri'

const { invoke } = useTauri()

const emit = defineEmits(['range-change'])

const rangeOptions = [
  { v: '5', label: '近 5 分钟' },
  { v: '10', label: '近 10 分钟' },
  { v: '30', label: '近 30 分钟' },
  { v: '60', label: '近 1 小时' },
  { v: '300', label: '近 5 小时' },
  { v: '0', label: '今日' },
  { v: '-1', label: '本周' },
  { v: 'custom', label: '自定义时间范围' },
]

const selectedRange = ref(localStorage.getItem('tm_range') || '10')
const showDatePicker = ref(false)
const prevRange = ref(selectedRange.value)
const confirmedRange = ref(null) // { startMs, endMs }
const openedFromDropdown = ref(false) // 标记是否从下拉框打开弹窗
const testStatus = ref('')
const endpointText = ref('')
const endpointTitle = ref('')
const toolbarRef = ref(null)
const showCopyModal = ref(false)
const serverPort = ref(8188)

const activeRangeText = computed(() => {
  if (selectedRange.value !== 'custom' || !confirmedRange.value) return ''
  const { startMs, endMs } = confirmedRange.value
  const fmt = (ms) => {
    const d = new Date(ms)
    const M = String(d.getMonth() + 1).padStart(2, '0')
    const D = String(d.getDate()).padStart(2, '0')
    const h = String(d.getHours()).padStart(2, '0')
    const m = String(d.getMinutes()).padStart(2, '0')
    return `${M}-${D} ${h}:${m}`
  }
  return `${fmt(startMs)} ~ ${fmt(endMs)}`
})

// 当选择 "custom" 时打开弹窗，不立即 emit
watch(selectedRange, (v) => {
  if (v === 'custom') {
    openedFromDropdown.value = true
    showDatePicker.value = true
    return
  }
  prevRange.value = v
  localStorage.setItem('tm_range', v)
  emit('range-change', v)
})

function onDatePickerClose() {
  showDatePicker.value = false
  // 仅从下拉框打开时恢复，从范围文本打开时保持 'custom'
  if (openedFromDropdown.value) {
    selectedRange.value = prevRange.value
  }
  openedFromDropdown.value = false
}

function onDatePickerConfirm(rangeObj) {
  showDatePicker.value = false
  openedFromDropdown.value = false
  localStorage.setItem('tm_range', 'custom')
  confirmedRange.value = { startMs: rangeObj.startMs, endMs: rangeObj.endMs }
  emit('range-change', rangeObj)
}

async function initInfo() {
  try {
    const info = await invoke('get_server_info')
    endpointText.value = info.endpoint + '/*' + (info.model_override ? `（强制模型 ${info.model_override}）` : '')
    if (Array.isArray(info.endpoints)) {
      endpointTitle.value = '支持的接口：' + info.endpoints.join('  ·  ')
    }
    serverPort.value = info.port || 8188
  } catch {}
}

function copyEndpoint() {
  showCopyModal.value = true
}

async function sendTest() {
  testStatus.value = '请求中…'
  try {
    const info = await invoke('get_server_info')
    const r = await fetch(info.endpoint + '/chat/completions', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        model: info.model_override || 'x-preview-f',
        stream: false,
        max_tokens: 32,
        messages: [{ role: 'user', content: 'hi' }],
      }),
    })
    testStatus.value = r.ok ? '✓ 成功' : `✗ HTTP ${r.status}`
  } catch {
    testStatus.value = '✗ 失败'
  }
}

const { getCurrentWindow } = useTauri()
const win = getCurrentWindow()
function isInteractive(el) { return el?.closest('button, select, input, a, .dd') }
function tryDrag(e) {
  if (e.button !== 0 || isInteractive(e.target)) return
  e.preventDefault()
  try { win?.startDragging() } catch {}
}
function onDblClick(e) {
  if (isInteractive(e.target)) return
  try { win?.toggleMaximize() } catch {}
}

let unlistenInfo = null

onMounted(async () => {
  initInfo()
  // 监听后端切换 Profile / 模型后发出的事件，刷新地址栏
  const { listen } = useTauri()
  unlistenInfo = await listen('server-info-changed', () => initInfo())
})

onBeforeUnmount(() => {
  unlistenInfo?.()
})

defineExpose({ initInfo, selectedRange })
</script>

<style scoped>
.toolbar {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 10px 20px;
  border-bottom: 1px solid var(--border);
  background: var(--panel);
  flex-shrink: 0;
}
.endpoint {
  color: var(--muted); font-size: 12px; white-space: nowrap;
  min-width: 0; overflow: hidden; text-overflow: clip;
}
.copy-btn { width: 34px !important; height: 34px !important; flex-shrink: 0; border-radius: 6px; }
.copy-btn :deep(svg) { width: 15px; height: 15px; }
.test-btn { height: 34px; flex-shrink: 0; }
.test-status { color: var(--muted); font-size: 12px; min-width: 60px; }
.toolbar-right { margin-left: auto; display: flex; align-items: center; gap: 10px; flex-shrink: 0; }
.active-range {
  font-size: 12px;
  color: var(--blue);
  background: rgba(79, 140, 255, 0.08);
  border: 1px solid rgba(79, 140, 255, 0.2);
  border-radius: 6px;
  padding: 4px 10px;
  white-space: nowrap;
  cursor: pointer;
  transition: background .12s;
}
.active-range:hover {
  background: rgba(79, 140, 255, 0.15);
}
</style>
