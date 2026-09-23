<template>
  <SettingsCard title="调度状态" description="查看各渠道实时调度情况" auto>
    <div class="scheduler-grid">
      <div v-for="ch in channels" :key="ch.profile_id" class="channel-item" :class="{ disabled: !ch.enabled }">
        <div class="channel-header">
          <span class="channel-name">
            {{ ch.name || ch.profile_id }}
            <span v-if="channelBalances[ch.profile_id]?.text" class="channel-balance" :class="{ ok: channelBalances[ch.profile_id].ok }">{{ channelBalances[ch.profile_id].text }}</span>
          </span>
          <span v-if="!ch.enabled" class="channel-disabled">已禁用</span>
        </div>
        <div class="channel-stats">
          <div class="stat">
            <span class="stat-label">并发</span>
            <!-- 分母用自适应有效上限：上游限流时会低于配置上限，
                 显示配置值会让人误以为还有余量 -->
            <span class="stat-value">
              {{ ch.current_concurrency }}/{{ ch.effective_limit }}
              <span v-if="isThrottled(ch)" class="stat-note">上限 {{ ch.max_concurrency }}</span>
            </span>
          </div>
          <div class="stat">
            <span class="stat-label">RPM</span>
            <span class="stat-value">{{ ch.current_rpm }}/{{ ch.max_rpm || '∞' }}</span>
          </div>
          <div class="stat">
            <span class="stat-label">TPM</span>
            <span class="stat-value">{{ formatToken(ch.current_tpm) }}/{{ ch.max_tpm ? formatToken(ch.max_tpm) : '∞' }}</span>
          </div>
          <div class="stat">
            <span class="stat-label">权重</span>
            <span class="stat-value">{{ ch.weight }}</span>
          </div>
        </div>
      </div>
      <div v-if="channels.length === 0" class="no-channels">
        暂无渠道配置
      </div>
    </div>
  </SettingsCard>
</template>

<script setup>
import { ref, onMounted, onUnmounted } from 'vue'
import { useTauri } from '../composables/useTauri'
import SettingsCard from './SettingsCard.vue'

const { invoke } = useTauri()
const channels = ref([])
const channelBalances = ref({})
let refreshTimer = null
let balanceTimer = null

async function refresh() {
  try {
    const status = await invoke('get_scheduler_status')
    channels.value = status.channels || []
  } catch {}
}

async function refreshBalances() {
  const result = {}
  for (const ch of channels.value) {
    try {
      const b = await invoke('get_channel_balance', { profileId: ch.profile_id }) || {}
      if (b.supported === false || b.available !== true) {
        result[ch.profile_id] = { text: '', ok: false }
      } else {
        const sym = b.currency === 'USD' ? '$' : '¥'
        result[ch.profile_id] = { text: `${sym}${b.total || '--'}`, ok: true }
      }
    } catch {
      result[ch.profile_id] = { text: '', ok: false }
    }
  }
  channelBalances.value = result
}

function formatToken(n) {
  if (n >= 1000000) return (n / 1000000).toFixed(1) + 'M'
  if (n >= 1000) return (n / 1000).toFixed(1) + 'K'
  return String(n)
}

// 自适应上限低于配置上限 → 当前正被上游限流而主动收紧
function isThrottled(ch) {
  return typeof ch.effective_limit === 'number' && ch.effective_limit < ch.max_concurrency
}

onMounted(async () => {
  await refresh()
  await refreshBalances()
  refreshTimer = setInterval(refresh, 2000)
  balanceTimer = setInterval(refreshBalances, 30000)
})

onUnmounted(() => {
  if (refreshTimer) clearInterval(refreshTimer)
  if (balanceTimer) clearInterval(balanceTimer)
})
</script>

<style scoped>
.scheduler-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(280px, 1fr));
  gap: 10px;
}

.channel-item {
  padding: 10px 12px;
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 8px;
}

.channel-item.disabled {
  opacity: 0.5;
}

.channel-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 8px;
}

.channel-name {
  font-size: 13px;
  font-weight: 600;
  color: var(--text);
  display: flex;
  align-items: center;
  gap: 8px;
}

.channel-balance {
  font-size: 11px;
  font-weight: 500;
  color: var(--muted);
  background: var(--border);
  padding: 1px 7px;
  border-radius: 10px;
  line-height: 1.5;
}

.channel-balance.ok {
  color: var(--green);
}

.channel-disabled {
  font-size: 11px;
  color: var(--muted);
}

.channel-stats {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 6px;
}

.stat {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.stat-label {
  font-size: 10px;
  color: var(--muted);
}

.stat-value {
  font-size: 12px;
  color: var(--text);
  font-weight: 500;
}

/* 被限流收紧时的补充说明，弱化显示避免抢主数值 */
.stat-note {
  font-size: 10px;
  color: var(--muted);
  font-weight: 400;
}

.no-channels {
  grid-column: 1 / -1;
  text-align: center;
  padding: 20px;
  color: var(--muted);
  font-size: 13px;
}
</style>
