<template>
  <section v-if="visible" class="ai-bar">
    <!-- 最新探测状态块：14x14 固定尺寸，永不改变 -->
    <div class="status-block" :style="{ background: latestColor }" :title="statusTooltip"></div>

    <span class="status-label" :class="{ offline: !status.ok }">{{ status.ok ? '在线' : '离线' }}</span>

    <span class="bar-sep">|</span>

    <span class="bar-item item-time">
      <span class="bar-lbl">上次</span>
      <span class="bar-val">{{ lastTimeText }}</span>
    </span>

    <span class="bar-sep">|</span>

    <span class="bar-item item-latency">
      <span class="bar-lbl">延迟</span>
      <span class="bar-val">{{ latencyText }}</span>
    </span>

    <span class="bar-sep">|</span>

    <span class="bar-item item-uptime">
      <span class="bar-lbl">成功率</span>
      <span class="bar-val">{{ uptimeText }}</span>
    </span>

    <span class="bar-sep">|</span>

    <span class="bar-item item-checks">
      <span class="bar-lbl">检测</span>
      <span class="bar-val">{{ totalChecksText }}</span>
      <span class="bar-lbl">次</span>
    </span>

    <span class="bar-sep">|</span>

    <span class="bar-item item-fails" :class="{ 'bar-fail': failCount > 0 }">
      <span class="bar-lbl">失败</span>
      <span class="bar-val">{{ failCountText }}</span>
    </span>

    <!-- 右侧探测块：宽度由剩余空间动态决定 -->
    <div class="probe-strip" ref="stripRef">
      <div
        v-for="(b, i) in visibleBlocks"
        :key="i"
        class="probe-block"
        :style="{ background: b.color }"
        :title="b.title"
      ></div>
    </div>
  </section>
</template>

<script setup>
import { ref, computed, watch, nextTick, onMounted, onBeforeUnmount } from 'vue'
import { useTauri } from '../composables/useTauri'

const { invoke } = useTauri()

// 整行显隐需通知父组件：App.vue 的高度公式要扣掉这 36px
const emit = defineEmits(['visible-change'])

// 组件自持状态：不依赖父组件传参，自己 invoke 轮询
const visible = ref(false)
const stripRef = ref(null) // 探测条容器 ref，用于 ResizeObserver 测宽

watch(visible, (v) => emit('visible-change', !!v), { immediate: true })
const status = ref({ ok: false, latencyMs: 0, lastCheck: 0, error: null })
const history = ref([]) // 循环队列：始终最多 MAX_CAPACITY 条，最旧的自动丢弃
const stats = ref({ uptime: 0, avgLatency: 0, totalChecks: 0, failCount: 0 })

let timer = null
let intervalMs = 0 // 当前定时器对应的间隔（毫秒），为 0 表示未启动
let stopped = false // 组件已卸载标记，避免卸载后继续轮询

// ── 动态探测条：根据剩余宽度实时计算可见色块数 ──
const BLOCK_W = 3   // 单个色块宽度（px）
const BLOCK_GAP = 1 // 色块之间的 gap（px）
const MAX_CAPACITY = 60 // 历史队列最大容量，窗口变窄时不缩减数据，只减少显示数

const stripWidth = ref(0)   // strip 容器的实际像素宽度
let resizeObs = null        // ResizeObserver 实例

// 整行由 v-if 控制，mount 时元素尚未渲染（stripRef 为 null）。
// 因此必须等 visible 变 true、DOM 渲染完成后再挂 ResizeObserver，
// 否则宽度永远是 0，色块一个都不会显示。
let stripReady = false
async function setupStrip() {
  if (stripReady) return
  await nextTick()
  if (!stripRef.value) return
  stripReady = true
  const measure = () => {
    if (stripRef.value) stripWidth.value = Math.floor(stripRef.value.getBoundingClientRect().width)
  }
  measure()
  resizeObs = new ResizeObserver((entries) => {
    for (const entry of entries) {
      stripWidth.value = Math.floor(entry.contentRect.width)
    }
  })
  resizeObs.observe(stripRef.value)
}

watch(visible, (v) => { if (v) setupStrip() }, { immediate: true })

// 根据可用宽度计算当前能显示多少个色块（上限 = MAX_CAPACITY）
const visibleCount = computed(() => {
  if (stripWidth.value <= 0) return 0
  const count = Math.floor(stripWidth.value / (BLOCK_W + BLOCK_GAP))
  return Math.min(count, MAX_CAPACITY)
})

// 从历史队列尾部取最近 visibleCount 条，色块按「旧→新」排列（左侧最旧）
const visibleBlocks = computed(() => {
  const r = range.value
  const count = visibleCount.value
  // 历史数据按时间正序（旧→新），取最近 count 条
  const samples = history.value.slice(0, count).reverse()
  const list = []

  // 数据不足时左侧补占位块，保持视觉宽度稳定
  for (let i = samples.length; i < count; i++) {
    list.push({ color: C_EMPTY, title: '无数据' })
  }

  for (const h of samples) {
    if (!h || !h.ok) {
      list.push({
        color: GRADIENT[4],
        title: `${fmtTime(h?.timestamp)} 探测失败`,
      })
      continue
    }
    const ms = Number(h.latencyMs) || 0
    list.push({
      color: colorForLatency(ms, r),
      title: `${fmtTime(h.timestamp)} 成功 ${ms}ms`,
    })
  }

  return list
})

// 历史队列中的有效样本（用于颜色归一化），始终基于 MAX_CAPACITY
const allSamples = computed(() => history.value.slice(0, MAX_CAPACITY).reverse())

// 5 档延迟梯度：绿 → 黄绿 → 黄 → 橙 → 红（相对当前窗口归一化后的比例）
// 明度单调递增（51%→53%→60%→63%→68%），保证相邻档在色盲下也能靠明暗区分
const GRADIENT = [
  '#35d0a5', // 1 绿   最快
  '#8bc34a', // 2 黄绿
  '#e6c34a', // 3 黄
  '#f5934a', // 4 橙
  '#ff5b5b', // 5 红   最慢 / 失败
]
const C_EMPTY = 'var(--border)' // 无数据：灰色占位

// 成功样本的延迟区间（用于相对归一化，基于全部历史而非仅可见部分）
const range = computed(() => {
  const latencies = allSamples.value
    .filter((h) => h && h.ok && Number(h.latencyMs) > 0)
    .map((h) => Number(h.latencyMs))
  if (latencies.length === 0) return { min: 0, max: 0, span: 0, count: 0 }
  const min = Math.min(...latencies)
  const max = Math.max(...latencies)
  return { min, max, span: max - min, count: latencies.length }
})

// 相对比例 → 均匀分入 5 档渐变
function colorForLatency(ms, r) {
  // 无成功样本：视为最差
  if (r.count === 0) return GRADIENT[4]
  // 全部相同延迟：统一最快档
  if (r.span <= 0) return GRADIENT[0]
  const ratio = (ms - r.min) / r.span
  const idx = Math.min(4, Math.max(0, Math.floor(ratio * 5)))
  return GRADIENT[idx]
}

// 左侧状态块：取最新一次探测的结果着色
const latestColor = computed(() => {
  if (!status.value.ok) return GRADIENT[4]
  if (range.value.count === 0) return GRADIENT[0]
  return colorForLatency(Number(status.value.latencyMs) || 0, range.value)
})

const statusTooltip = computed(() => {
  if (!status.value.ok) return '离线: ' + (status.value.error || '连接失败')
  return `在线 - 延迟 ${Number(status.value.latencyMs) || 0}ms`
})

// 上次探测时间：精确到秒的 HH:MM:SS，无日期
const lastTimeText = computed(() => {
  const t = status.value.lastCheck
  if (!t) return '--:--:--'
  const d = new Date(t)
  return [d.getHours(), d.getMinutes(), d.getSeconds()]
    .map((n) => String(n).padStart(2, '0'))
    .join(':')
})

function fmtTime(ts) {
  if (!ts) return '--:--:--'
  const d = new Date(ts)
  return [d.getHours(), d.getMinutes(), d.getSeconds()]
    .map((n) => String(n).padStart(2, '0'))
    .join(':')
}

const latencyText = computed(() => `${Math.round(Number(stats.value.avgLatency) || 0)}ms`)
const uptimeText = computed(() => `${(Number(stats.value.uptime) || 0).toFixed(1)}%`)
const totalChecksText = computed(() => String(Number(stats.value.totalChecks) || 0))
const failCountText = computed(() => String(Number(stats.value.failCount) || 0))
const failCount = computed(() => Number(stats.value.failCount) || 0)

// 读取探针设置；读取失败返回 null（视为不可用，保留当前显示状态）
async function readProbeSettings() {
  try {
    return (await invoke('get_probe_settings')) || null
  } catch {
    return null
  }
}

// 重建轮询定时器（interval_secs 单位是秒，需 ×1000）
function reschedule(secs) {
  const next = Math.max(1, Number(secs) || 0) * 1000
  if (timer && intervalMs === next) return
  if (timer) clearInterval(timer)
  intervalMs = next
  timer = setInterval(tick, next)
}

function stopTimer() {
  if (timer) clearInterval(timer)
  timer = null
  intervalMs = 0
}

// 累积一次探测结果（头插 + 只保留最近 MAX_CAPACITY 条，循环队列）
function pushSample(ok, latencyMs) {
  history.value = [{ timestamp: Date.now(), ok, latencyMs }, ...history.value].slice(0, MAX_CAPACITY)
}

// 成功率 = 成功次数 / 总次数；平均延迟取成功样本均值
function recomputeStats() {
  const total = history.value.length
  const okList = history.value.filter((h) => h.ok)
  const latencies = okList.filter((h) => h.latencyMs > 0).map((h) => h.latencyMs)
  stats.value = {
    uptime: total > 0 ? Math.round((okList.length / total) * 1000) / 10 : 0,
    avgLatency: latencies.length > 0 ? Math.round(latencies.reduce((a, b) => a + b, 0) / latencies.length) : 0,
    totalChecks: total,
    failCount: total - okList.length,
  }
}

// 单次探测
async function probe() {
  const start = Date.now()
  try {
    const r = await invoke('test_ai_connection')
    if (r && r.success === false) {
      pushSample(false, 0)
      status.value = {
        ok: false,
        latencyMs: 0,
        lastCheck: Date.now(),
        error: r.message || `HTTP ${r.status_code || 0}`,
      }
    } else {
      const latency = Number(r?.latency_ms) || Date.now() - start
      pushSample(true, latency)
      status.value = { ok: true, latencyMs: latency, lastCheck: Date.now(), error: null }
    }
  } catch (e) {
    pushSample(false, 0)
    status.value = { ok: false, latencyMs: 0, lastCheck: Date.now(), error: String(e) }
  }
  recomputeStats()
}

// 每次 tick 重读设置：间隔变了重建定时器，被关闭则停表并隐藏整行
async function tick() {
  if (stopped) return
  const s = await readProbeSettings()
  if (!s || s.enabled === false) {
    stopTimer()
    visible.value = false
    return
  }
  visible.value = true
  reschedule(s.interval_secs)
  await probe()
}

// 探测条宽度由剩余空间动态决定，窗口变宽时多显示几个旧色块（数据不丢失），
// 变窄时少显示几个（数据还在队列里）。上限固定 MAX_CAPACITY，不会随窗口缩放而缩减数据。
onMounted(async () => {
  const s = await readProbeSettings()
  // 未开启探针：整行不渲染
  if (!s || s.enabled === false) {
    visible.value = false
    return
  }
  visible.value = true
  reschedule(s.interval_secs)
  await probe() // 首屏立即探一次，避免空白
})

onBeforeUnmount(() => {
  stopped = true
  stopTimer()
  if (resizeObs) { resizeObs.disconnect(); resizeObs = null }
})
</script>

<style scoped>
/* 固定高度 36px 的单行条：任何内容变化都不会改变高度 */
.ai-bar {
  height: 36px;
  display: flex;
  align-items: center;
  gap: 10px;
  overflow: hidden;
  padding: 0 16px;
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 8px;
  font-size: 12px;
  line-height: 1;
  white-space: nowrap;
  flex-shrink: 0;
  user-select: none;
}

/* 14x14 固定状态块 */
.status-block {
  width: 14px;
  height: 14px;
  flex: 0 0 14px;
  border-radius: 3px;
  transition: background 0.4s;
}

/* 状态文字：固定宽度，避免左右跳动 */
.status-label {
  width: 26px;
  flex: 0 0 26px;
  color: var(--text);
  font-weight: 600;
  font-size: 12px;
}

.status-label.offline {
  color: #ff5b5b;
}

/* 分隔符：自身带固定宽度，负外边距抵消 10px gap 的松散感 */
.bar-sep {
  flex: 0 0 auto;
  margin: 0 -4px;
  color: var(--border);
  font-size: 11px;
  user-select: none;
}

/* 每一项固定宽度，数字使用等宽数字，永不抖动 */
.bar-item {
  display: flex;
  align-items: center;
  gap: 4px;
  flex: 0 0 auto;
  font-variant-numeric: tabular-nums;
}

/* 上次时间 + 统计指标定宽，数字使用等宽数字，永不抖动 */
.item-time {
  width: 68px;
}

.item-latency {
  width: 80px;
}

.item-uptime {
  width: 78px;
}

.item-checks {
  width: 68px;
}

.item-fails {
  width: 52px;
}

.bar-lbl {
  color: var(--muted);
  font-size: 11px;
}

.bar-val {
  color: var(--text);
  font-weight: 600;
  font-size: 12px;
  font-variant-numeric: tabular-nums;
}

.bar-fail .bar-val {
  color: #ff5b5b;
}

/* 右侧 60 个固定尺寸方块：3px 宽，高度由 strip 决定；1px 间隔。
   1px 微圆角用于视觉柔和。经 canvas 像素分析实测：圆角对各颜色削角完全一致
   （绿/红的满宽行数、削窄行数逐字节相同），不会造成几何高度差异。
   深浅色块的观感差异来自与背景的对比强度，与圆角无关。 */
.probe-strip {
  display: flex;
  align-items: stretch;
  justify-content: flex-end;
  gap: 1px;
  /* 占满左侧统计之后的全部剩余空间：宽度由容器决定而非内容，
     避免「宽度为 0 → 色块为 0 → 宽度仍为 0」的自举死锁 */
  flex: 1 1 auto;
  min-width: 0;
  height: 14px;
}

.probe-block {
  width: 3px;
  height: 100%;
  flex: 0 0 3px;
  border-radius: 1px;
  transition: background 0.4s;
}
</style>
