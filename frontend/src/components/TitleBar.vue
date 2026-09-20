<template>
  <div class="titlebar" :class="{ 'titlebar--mac': isMac }" ref="titlebarRef" @mousedown="tryDrag" @dblclick="onDblClick">
    <div class="titlebar-brand">
      <span class="dot"></span>
      <strong>Token Monitor</strong>
      <span v-if="concurrency > 0" class="concurrency-badge" title="当前并发数">
        并发 {{ concurrency }}
      </span>
    </div>
    <div class="titlebar-actions">
      <!-- DeepSeek 余额：只读展示，不是按钮/输入框，因此天然作为拖拽把手 -->
      <span
        v-if="balanceVisible"
        class="balance-badge"
        :class="{ ok: balanceOk }"
        :title="balanceTitle"
      >{{ balanceText }}</span>
      <IconButton title="设置" @click="$emit('open-settings')">
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <circle cx="12" cy="12" r="3" />
          <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z" />
        </svg>
      </IconButton>
      <IconButton title="窗口置顶" :active="pinned" @click="togglePin">
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12 17v5" />
          <path d="M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7h1a2 2 0 0 0 0-4H8a2 2 0 0 0 0 4h1z" />
        </svg>
      </IconButton>
      <IconButton title="最小化" @click="win?.minimize()" v-if="!isMac">
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
          <path d="M5 12h14" />
        </svg>
      </IconButton>
      <IconButton title="最大化" @click="toggleMaximize" v-if="!isMac">
        <svg v-show="!isMaximized" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round">
          <rect x="5" y="5" width="14" height="14" rx="1.5" />
        </svg>
        <svg v-show="isMaximized" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round">
          <rect x="8" y="8" width="11" height="11" rx="1.5" />
          <path d="M5 16V6.5A1.5 1.5 0 0 1 6.5 5H16" />
        </svg>
      </IconButton>
      <IconButton title="隐藏到托盘" class="ibtn--close" @click="win?.close()" v-if="!isMac">
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
          <path d="M18 6 6 18M6 6l12 12" />
        </svg>
      </IconButton>
    </div>
  </div>
</template>

<script setup>
import { ref, computed, onMounted, onBeforeUnmount } from 'vue'
import { useTauri } from '../composables/useTauri'
import IconButton from './base/IconButton.vue'

defineProps({ concurrency: { type: Number, default: 0 } })
defineEmits(['open-settings'])

const { invoke, getCurrentWindow } = useTauri()
const win = getCurrentWindow()
const pinned = ref(false)
const isMaximized = ref(false)
const titlebarRef = ref(null)
const isMac = /mac|darwin/i.test(navigator.userAgent)

// ---- DeepSeek 余额（组件自持状态，不依赖父组件传参） ----
const balanceVisible = ref(false)
const balance = ref(null)

let balanceTimer = null
let balanceIntervalMs = 0 // 当前定时器对应的间隔（毫秒），为 0 表示未启动
let balanceStopped = false // 组件已卸载标记，避免卸载后继续轮询

// 三种展示状态：不支持（灰，非错误）/ 可用（绿色，币种符号 + 余额）/ 不可用（灰，原因）
const balanceOk = computed(() => {
  const b = balance.value
  return !!b && b.supported !== false && b.available === true
})

const balanceText = computed(() => {
  const b = balance.value
  if (!b) return ''
  if (b.supported === false) return b.reason || '余额不可用'
  if (b.available === true) {
    const symbol = b.currency === 'USD' ? '$' : '¥'
    // 余额是后端返回的字符串，直接拼接，不做浮点转换
    return `${symbol}${b.total || '--'}`
  }
  return b.reason || '余额获取失败'
})

const balanceTitle = computed(() => {
  const b = balance.value
  if (!b) return 'DeepSeek 余额'
  if (b.supported === false) return `余额查询不可用：${b.reason || '当前上游不支持'}`
  if (b.available === true) return `DeepSeek 余额 ${b.currency || 'CNY'} ${b.total || ''}`
  return `余额获取失败：${b.reason || '未知原因'}`
})

async function readBalanceSettings() {
  try {
    return (await invoke('get_balance_settings')) || null
  } catch {
    return null
  }
}

// 重建轮询定时器（interval_secs 单位是秒，需 ×1000）
function rescheduleBalance(secs) {
  const next = Math.max(1, Number(secs) || 0) * 1000
  if (balanceTimer && balanceIntervalMs === next) return
  if (balanceTimer) clearInterval(balanceTimer)
  balanceIntervalMs = next
  balanceTimer = setInterval(balanceTick, next)
}

function stopBalanceTimer() {
  if (balanceTimer) clearInterval(balanceTimer)
  balanceTimer = null
  balanceIntervalMs = 0
}

async function fetchBalance() {
  try {
    const r = await invoke('get_balance')
    if (r) balance.value = r
  } catch (e) {
    balance.value = { supported: true, available: false, reason: String(e) }
  }
}

// 每次 tick 重读设置：间隔变了重建定时器，被关闭则停表并隐藏
async function balanceTick() {
  if (balanceStopped) return
  const s = await readBalanceSettings()
  if (!s || s.enabled === false) {
    stopBalanceTimer()
    balanceVisible.value = false
    balance.value = null
    return
  }
  balanceVisible.value = true
  rescheduleBalance(s.interval_secs)
  await fetchBalance()
}

function isInteractive(el) {
  return el?.closest('button, select, input, a, .dd')
}

function tryDrag(e) {
  if (e.button !== 0) return
  if (isInteractive(e.target)) return
  e.preventDefault()
  try { win?.startDragging() } catch {}
}

function onDblClick(e) {
  if (isInteractive(e.target)) return
  toggleMaximize()
}

async function toggleMaximize() {
  try {
    const maximized = await win?.isMaximized?.()
    if (maximized) {
      await win?.unmaximize?.()
    } else {
      await win?.maximize?.()
    }
  } catch {}
  setTimeout(syncMaxIcon, 150)
}

async function togglePin() {
  pinned.value = !pinned.value
  try { await win?.setAlwaysOnTop(pinned.value) } catch {}
}

async function syncMaxIcon() {
  try { isMaximized.value = await win?.isMaximized() } catch {}
}

onMounted(async () => {
  syncMaxIcon()
  win?.onResized?.(() => syncMaxIcon())
  // 余额：读取设置后按 interval_secs 轮询；未开启则不显示余额区域
  const s = await readBalanceSettings()
  if (!s || s.enabled === false) {
    balanceVisible.value = false
    return
  }
  balanceVisible.value = true
  rescheduleBalance(s.interval_secs)
  await fetchBalance() // 首屏立即取一次，避免空白
})

onBeforeUnmount(() => {
  balanceStopped = true
  stopBalanceTimer()
})
</script>

<style scoped>
.titlebar {
  display: flex;
  justify-content: space-between;
  align-items: center;
  height: 40px;
  padding-left: 14px;
  background: var(--panel);
  border-bottom: 1px solid var(--border);
  user-select: none;
  flex-shrink: 0;
}
/* macOS：原生红绿灯占据左侧 ~70px，标题栏内容自动右移 */
.titlebar--mac { padding-left: 80px; }
.titlebar-brand { display: flex; align-items: center; gap: 10px; }
.titlebar-brand strong { font-size: 14px; font-weight: 600; color: var(--text); }
.concurrency-badge {
  font-size: 11px; color: var(--muted); background: var(--border);
  padding: 1px 7px; border-radius: 10px; line-height: 1.5; white-space: nowrap;
}
.concurrency-badge:empty { display: none; }
.dot {
  width: 10px; height: 10px; border-radius: 50%;
  background: var(--green); box-shadow: 0 0 8px var(--green);
  animation: pulse 2s infinite;
}
@keyframes pulse { 50% { opacity: .4; } }
.titlebar-actions { display: flex; height: 100%; }
/* 余额展示：只读文本，沿用徽章风格；不是 button/input，不阻断窗口拖拽 */
.balance-badge {
  align-self: center;
  margin-right: 10px;
  max-width: 180px;
  overflow: hidden;
  text-overflow: ellipsis;
  font-size: 11px; color: var(--muted); background: var(--border);
  padding: 1px 7px; border-radius: 10px; line-height: 1.5; white-space: nowrap;
}
.balance-badge.ok { color: var(--green); }
.balance-badge:empty { display: none; }
</style>
