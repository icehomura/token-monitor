<template>
  <div class="app-shell" :style="{ '--codeg-bar-height': codegBarVisible ? '220px' : '0px', '--probe-bar-height': probeBarVisible ? '36px' : '0px' }">
  <TitleBar
    :concurrency="stats.concurrency"
    @open-settings="showSettings = true"
  />
  <Toolbar
    ref="toolbarRef"
    @range-change="onRangeChange"
  />
  <!-- 第三行：AI 服务探针 -->
  <AiProbeBar @visible-change="onProbeVisible" />
  <section class="main-area">
    <StatsCards
       :stats="stats"
       :convertUnits="convertUnits"
    />
    <RpmChart
      :labels="stats.labels"
      :rpms="stats.rpms"
      :tpms="stats.tpms"
      :inputTpms="stats.inputTokensPerMin"
      :convertUnits="convertUnits"
    />
  </section>
  <CodegStatusBar :show="codegBarVisible" />
  <SettingsModal
    :visible="showSettings"
    :themeName="themeName"
    :convertUnits="convertUnits"
    :stats="stats"
    @close="showSettings = false"
    @update:themeName="setTheme"
    @update:convertUnits="v => { convertUnits = v; setConvertUnits(v); refresh() }"
  />
  <CloseDialog
    :visible="showCloseDialog"
    @close="showCloseDialog = false"
    @choice="onCloseChoice"
  />
  </div>
</template>

<script setup>
import { ref, onMounted, onBeforeUnmount } from 'vue'
import TitleBar from './components/TitleBar.vue'
import Toolbar from './components/Toolbar.vue'
import AiProbeBar from './components/AiProbeBar.vue'
import StatsCards from './components/StatsCards.vue'
import RpmChart from './components/RpmChart.vue'
import CodegStatusBar from './components/CodegStatusBar.vue'
import SettingsModal from './components/SettingsModal.vue'
import CloseDialog from './components/CloseDialog.vue'
import { useStats } from './composables/useStats'
import { useTheme, themeColors } from './composables/useTheme'
import { getConvertUnits, setConvertUnits } from './utils/format'
import { useTauri } from './composables/useTauri'

const { listen, invoke, getCurrentWindow } = useTauri()
const { stats, refresh } = useStats()
const { themeName, setTheme } = useTheme()

const showSettings = ref(false)
const showCloseDialog = ref(false)
const convertUnits = ref(getConvertUnits())
const toolbarRef = ref(null)
const codegBarVisible = ref(false)
// 探针行的实际显隐（由 AiProbeBar 自持，通过 visible-change 上报），用于高度计算
const probeBarVisible = ref(false)
function onProbeVisible(v) { probeBarVisible.value = !!v }
const BASE_WINDOW_HEIGHT = 640
const CODEG_DOCK_HEIGHT = 240
let baseWindowHeight = BASE_WINDOW_HEIGHT

async function rememberWindowHeight() {
  const win = getCurrentWindow()
  if (!win) return
  try {
    const size = await win?.outerSize?.()
    if (size?.height && size.height >= BASE_WINDOW_HEIGHT) {
      baseWindowHeight = size.height
    }
  } catch {}
}

async function applyWindowHeight(showCodeg) {
  const win = getCurrentWindow()
  if (!win) return
  try {
    const width = (await win?.outerSize?.())?.width || 1100
    const height = showCodeg ? baseWindowHeight + CODEG_DOCK_HEIGHT : baseWindowHeight
    await win?.setSize?.({ width, height })
  } catch {}
}

async function updateCodegBar(visible) {
  codegBarVisible.value = visible
  applyWindowHeight(visible)
}

// 当前范围状态：可能是字符串（预设）或 { range, startMs, endMs }（自定义）
const currentRangeState = ref(toolbarRef.value?.selectedRange ?? (localStorage.getItem('tm_range') || '10'))

function currentRange() { return currentRangeState.value }

function onRangeChange(range) {
  currentRangeState.value = range
  refresh(range || currentRange())
}

let unlisten = null
let unlistenClose = null
let unlistenCodeg = null
let refreshTimer = null
let pollTimer = null

function onCloseChoice({ choice, remember }) {
  showCloseDialog.value = false
  // 发送选择到 Rust 后端
  try { window.__TAURI__.event?.emit('close-choice', { choice, remember }) } catch {}
}

function debouncedRefresh() {
  if (refreshTimer) return
  refreshTimer = setTimeout(() => {
    refreshTimer = null
    refresh(currentRange())
  }, 1000)
}

onMounted(async () => {
  // 等一帧让 toolbarRef 就绪
  await new Promise(r => setTimeout(r, 0))
  currentRangeState.value = toolbarRef.value?.selectedRange ?? '10'
  refresh(currentRange())
  pollTimer = setInterval(() => refresh(currentRange()), 5000)
  unlisten = await listen('stats-updated', debouncedRefresh)
  unlistenClose = await listen('close-requested', () => { showCloseDialog.value = true })
  unlistenCodeg = await listen('codeg-settings-changed', async () => {
    try {
      const r = await invoke('get_codeg_settings')
      updateCodegBar(!!(r.config?.enabled))
    } catch {}
  })
  // 加载 CodeG 显示开关；未配置时保持隐藏
  await rememberWindowHeight()
  try {
    const r = await invoke('get_codeg_settings')
    updateCodegBar(!!(r.config?.enabled))
  } catch {}
})

onBeforeUnmount(() => {
  unlisten?.()
  unlistenClose?.()
  unlistenCodeg?.()
  clearInterval(pollTimer)
  clearTimeout(refreshTimer)
})
</script>

<style>
/*
  高度计算（box-sizing: border-box）：
  100vh
  - titlebar    40px
  - toolbar     42px
  - main pad    32px (16 top + 16 bottom)
  = 内容区 = calc(100vh - 114px)
  - cards       ~100px
  - gap          16px
  - chart-box   剩余 ≈ calc(100vh - 230px)
*/
/*
  100vh - titlebar(40) - toolbar(46) = main-area 总高
  内容 = 总高 - padding(32)
  chart = 内容 - cards(~100) - gap(16)
*/
.main-area {
  padding: 16px;
  display: grid;
  grid-template-rows: auto 1fr;
  gap: 16px;
  /* 需扣除标题栏 40 + toolbar 46 + 探针行 36（开启时）+ CodeG 行 */
  height: calc(100vh - 40px - 46px - var(--probe-bar-height, 0px) - var(--codeg-bar-height, 0px));
  overflow: hidden;
}
</style>
