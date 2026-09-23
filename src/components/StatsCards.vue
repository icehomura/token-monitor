<template>
  <section class="cards">
    <div class="card">
      <div class="card-title">当前分钟</div>
      <div class="card-body">
        <div class="tok-grid">
          <div class="tok-cell tok-input">
            <span class="tok-label">输入 TPM</span>
            <span class="tok-num">{{ fmtTokens(Math.max(0, currentInputTpm - currentCachedTpm), convertUnits) }}</span>
          </div>
          <div class="tok-cell tok-output">
            <span class="tok-label">输出 TPM</span>
            <span class="tok-num">{{ fmtTokens(currentOutputTpm, convertUnits) }}</span>
          </div>
          <div class="tok-cell tok-cached">
            <span class="tok-label">缓存 TPM</span>
            <span class="tok-num">{{ fmtTokens(currentCachedTpm, convertUnits) }}</span>
          </div>
          <div class="tok-cell tok-total">
            <span class="tok-label">合计 TPM</span>
            <span class="tok-num">{{ fmtTokens(currentTotalTpm, convertUnits) }}</span>
          </div>
        </div>
      </div>
    </div>

    <div class="card">
      <div class="card-title">词元数明细</div>
      <div class="card-body">
        <div class="tok-grid">
          <div class="tok-cell tok-input">
            <span class="tok-label">输入</span>
            <span class="tok-num">{{ fmtTokens(uncachedInputTokens, convertUnits) }}</span>
          </div>
          <div class="tok-cell tok-output">
            <span class="tok-label">输出</span>
            <span class="tok-num">{{ fmtTokens(stats.outputTokens, convertUnits) }}</span>
          </div>
          <div class="tok-cell tok-cached">
            <span class="tok-label">缓存</span>
            <span class="tok-num">{{ fmtTokens(stats.cachedTokens, convertUnits) }}</span>
          </div>
          <div class="tok-cell tok-total">
            <span class="tok-label">合计</span>
            <span class="tok-num">{{ fmtTokens(consumedTokens, convertUnits) }}</span>
          </div>
        </div>
      </div>
    </div>

    <div class="card">
      <div class="card-title">时间窗口内</div>
      <div class="card-body">
        <div class="tok-grid tok-grid-3">
          <div class="tok-cell tok-input">
            <span class="tok-label">总请求数</span>
            <span class="tok-num">{{ fmtTokens(stats.sumReq, convertUnits) }}</span>
          </div>
          <div class="tok-cell tok-output">
            <span class="tok-label">总输出词元数</span>
            <span class="tok-num">{{ fmtTokens(stats.sumTok, convertUnits) }}</span>
          </div>
          <div class="tok-cell tok-cached">
            <span class="tok-label">空闲时间</span>
            <span class="tok-num">{{ idleTimeDisplay }}</span>
          </div>
          <div class="tok-cell tok-input">
            <span class="tok-label">平均 RPM</span>
            <span class="tok-num">{{ avgRpmDisplay }}</span>
          </div>
          <div class="tok-cell tok-output">
            <span class="tok-label">平均输出词元数/分</span>
            <span class="tok-num">{{ avgTpmDisplay }}</span>
          </div>
          <div class="tok-cell tok-total">
            <span class="tok-label">时间利用率</span>
            <span class="tok-num">{{ utilizationDisplay }}</span>
          </div>
        </div>
      </div>
    </div>
  </section>
</template>

<script setup>
import { computed } from 'vue'
import { fmtTokens } from '../utils/format'

const props = defineProps({
  stats: { type: Object, required: true },
  convertUnits: { type: Boolean, default: false },
})

const currentRpm = computed(() => {
  const rpms = props.stats.rpms || []
  return rpms[rpms.length - 1] || 0
})

const currentTpm = computed(() => {
  const tpms = props.stats.tpms || []
  return tpms[tpms.length - 1] || 0
})

const currentInputTpm = computed(() => {
  const arr = props.stats.inputTokensPerMin || []
  return arr[arr.length - 1] || 0
})

const currentCachedTpm = computed(() => {
  const arr = props.stats.cachedTokensPerMin || []
  return arr[arr.length - 1] || 0
})

const currentOutputTpm = computed(() => {
  const arr = props.stats.outputTokensPerMin || []
  return arr[arr.length - 1] || 0
})

const currentTotalTpm = computed(() => {
  // 合计 = 原始输入（含缓存）+ 输出
  return currentInputTpm.value + currentOutputTpm.value
})

const uncachedInputTokens = computed(() => {
  const input = props.stats.inputTokens || 0
  const cached = props.stats.cachedTokens || 0
  return Math.max(0, input - cached)
})

const consumedTokens = computed(() => {
  const output = props.stats.outputTokens || 0
  return uncachedInputTokens.value + output
})

// 窗口内平均请求数/分钟（四舍五入取整）
const avgRpmDisplay = computed(() => {
  const w = props.stats.windowMinutes || 0
  if (w === 0) return '0'
  return Math.round(props.stats.sumReq / w).toString()
})

// 窗口内平均输出词元数/分钟
const avgTpmDisplay = computed(() => {
  const w = props.stats.windowMinutes || 0
  if (w === 0) return '0'
  const avg = props.stats.sumTok / w
  return fmtTokens(Math.round(avg), props.convertUnits)
})

// 活跃分钟数：tpm > 0 的分钟数
const activeMinutes = computed(() => {
  return (props.stats.tpms || []).filter(t => t > 0).length
})

// 空闲分钟数
const idleMinutes = computed(() => {
  const total = props.stats.windowMinutes || 0
  return Math.max(0, total - activeMinutes.value)
})

// 空闲时间显示：>=60分钟显示小时（1位小数），否则显示分钟
const idleTimeDisplay = computed(() => {
  const m = idleMinutes.value
  if (m === 0) return '0 分钟'
  if (m >= 60) {
    return (m / 60).toFixed(1) + ' 小时'
  }
  return m + ' 分钟'
})

// 利用率：活跃分钟 / 总窗口分钟 * 100%
const utilizationDisplay = computed(() => {
  const total = props.stats.windowMinutes || 0
  if (total === 0) return '0%'
  const pct = (activeMinutes.value / total * 100)
  return pct >= 10 ? Math.round(pct) + '%' : pct.toFixed(1) + '%'
})
</script>

<style scoped>
.cards {
  display: grid;
  grid-template-columns: 2fr 2fr 3fr;
  gap: 16px;
  flex-shrink: 0;
}

.card {
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 10px;
  padding: 14px 18px;
  display: flex;
  flex-direction: column;
}

/* 标题固定左上角 */
.card-title {
  color: var(--muted);
  font-size: 12px;
  margin-bottom: 10px;
  line-height: 1;
}

/* 内容区居中 */
.card-body {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
}
.card-footer {
  display: flex;
  justify-content: flex-end;
  padding-top: 10px;
}

/* 第三张卡片：3列2行（必须在 .tok-grid 之后以覆盖 grid-template-columns） */
.tok-grid-3 {
  grid-template-columns: repeat(3, 1fr) !important;
}

/* 左右卡片：RPM/TPM 或 总请求数/总词元数 */
.rate-item {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 2px;
}
.rate-label { font-size: 11px; color: var(--muted); }
.rate-value { font-size: 28px; font-weight: 700; line-height: 1.2; }
.rate-value.rpm { color: var(--blue); }
.rate-value.tpm { color: var(--green); }
.rate-divider {
  width: 1px; height: 36px;
  background: var(--border);
  margin: 0 8px;
  flex-shrink: 0;
}

/* 中间词元明细：2行2列，文字在上数字在下，居中 */
.tok-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 12px 20px;
  width: 100%;
}
.tok-cell {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 2px;
}
.tok-label { font-size: 11px; color: var(--muted); }
.tok-num { font-size: 20px; font-weight: 700; line-height: 1.2; }
.tok-input .tok-num { color: var(--blue); }
.tok-output .tok-num { color: var(--green); }
.tok-cached .tok-num { color: var(--muted); }
.tok-total .tok-num { color: var(--text); }
</style>
