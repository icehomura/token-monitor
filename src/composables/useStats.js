import { reactive } from 'vue'
import { useTauri } from './useTauri'

const { invoke } = useTauri()

let pending = null
let seq = 0

export function useStats() {
  const stats = reactive({
    labels: [],
    rpms: [],
    tpms: [],
    sumReq: 0,
    sumTok: 0,
    concurrency: 0,
    loading: false,
    windowMinutes: 0,
    // 词元明细（总量）
    inputTokens: 0,
    outputTokens: 0,
    cachedTokens: 0,
    // 每分钟词元明细（用于计算当前分钟各项TPM）
    inputTokensPerMin: [],
    outputTokensPerMin: [],
    cachedTokensPerMin: [],
  })

  async function refresh(windowMinutes) {
    const current = ++seq
    stats.loading = true
    try {
      let resp
      // 自定义时间范围模式：传入 startMs / endMs
      if (windowMinutes && typeof windowMinutes === 'object' && windowMinutes.startMs) {
        resp = await invoke('get_stats', {
          windowMinutes: 0,
          startMs: windowMinutes.startMs,
          endMs: windowMinutes.endMs,
        })
      } else {
        const range = Number(windowMinutes)
        resp = await invoke('get_stats', { windowMinutes: range })
      }
      if (current !== seq) return // 新的切换已经开始，丢弃过期返回
      const buckets = resp.buckets || []
      stats.labels = buckets.map(b => b.minute)
      stats.rpms = buckets.map(b => b.rpm)
      stats.tpms = buckets.map(b => b.output_tokens || 0)
      stats.sumReq = stats.rpms.reduce((a, b) => a + b, 0)
      stats.sumTok = stats.tpms.reduce((a, b) => a + b, 0)
      stats.concurrency = resp.concurrency || 0
      stats.windowMinutes = resp.window_minutes || 0
      stats.inputTokens = buckets.reduce((a, b) => a + (b.input_tokens || 0), 0)
      stats.outputTokens = buckets.reduce((a, b) => a + (b.output_tokens || 0), 0)
      stats.cachedTokens = buckets.reduce((a, b) => a + (b.cached_tokens || 0), 0)
      stats.inputTokensPerMin = buckets.map(b => b.input_tokens || 0)
      stats.outputTokensPerMin = buckets.map(b => b.output_tokens || 0)
      stats.cachedTokensPerMin = buckets.map(b => b.cached_tokens || 0)
    } catch (e) {
      console.error('get_stats failed:', e)
    } finally {
      if (current === seq) stats.loading = false
    }
  }

  return { stats, refresh }
}
