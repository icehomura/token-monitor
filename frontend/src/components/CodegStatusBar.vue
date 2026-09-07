<template>
  <section v-if="show" class="codeg-dock" :class="{ error: !!status.error }">
    <div v-if="status.error" class="codeg-error-banner">
      <span class="dot"></span>
      <span>CodeG 未连接</span>
      <span class="codeg-err-text">{{ status.error }}</span>
      <button class="codeg-refresh" @click="refresh">刷新</button>
    </div>

    <template v-else>
      <div class="codeg-col codeg-col-stats">
        <div class="codeg-card-head">
          <strong>系统资源</strong>
          <span class="badge">刷新 {{ loading ? '中' : '5s' }}</span>
        </div>
        <div class="codeg-stat-grid">
          <div class="stat-item">
            <span class="stat-label">CPU</span>
            <span class="stat-value">{{ fmtNum(status.system?.cpu_percent) }}%</span>
          </div>
          <div class="stat-item">
            <span class="stat-label">内存</span>
            <span class="stat-value">{{ fmtBytes(status.system?.memory_used_gb) }} / {{ fmtBytes(status.system?.memory_total_gb) }} ({{ fmtNum(status.system?.memory_percent) }}%)</span>
          </div>
          <div class="stat-item">
            <span class="stat-label">GPU</span>
            <span class="stat-value" v-if="status.system?.gpu_total_gb">{{ fmtBytes(status.system?.gpu_used_gb) }} / {{ fmtBytes(status.system?.gpu_total_gb) }} ({{ fmtNum(status.system?.gpu_percent) }}%)</span>
            <span class="stat-value" v-else>—</span>
          </div>
          <div class="stat-item">
            <span class="stat-label">C 盘</span>
            <span class="stat-value">{{ fmtBytes(status.system?.c_drive_used_gb) }} / {{ fmtBytes(status.system?.c_drive_total_gb) }} ({{ fmtNum(status.system?.c_drive_percent) }}%)</span>
          </div>
          <div class="stat-item">
            <span class="stat-label">进程</span>
            <span class="stat-value">Node {{ status.system?.node_processes || 0 }} · Agent {{ status.system?.agent_processes || 0 }}</span>
          </div>
        </div>
      </div>

      <div class="codeg-col codeg-col-active">
        <div class="codeg-card-head">
          <strong>运行中会话</strong>
          <span class="badge">运行 {{ status.running_count || 0 }}</span>
        </div>
        <div class="codeg-session-list">
          <div v-for="s in activeSessions" :key="s.connection_id" class="session-row">
            <div class="session-main">
              <div class="session-name" :title="sessionTitle(s)">{{ sessionTitle(s) }}</div>
              <div v-if="s.latest_reply" class="session-preview" :title="s.latest_reply">{{ s.latest_reply }}</div>
            </div>
            <span class="session-state" :class="'session-state--' + statusClass(s)">{{ statusLabel(s) }}</span>
          </div>
          <div v-if="!activeSessions.length" class="empty">暂无运行中会话</div>
        </div>
      </div>

      <div class="codeg-col codeg-col-request">
        <div class="codeg-card-head">
          <strong>等待输入</strong>
          <span class="badge">等待 {{ status.waiting_input_count || 0 }}</span>
        </div>
        <div class="codeg-session-list">
          <div v-for="s in waitingSessions" :key="s.connection_id" class="session-row">
            <div class="session-main">
              <div class="session-name" :title="sessionTitle(s)">{{ sessionTitle(s) }}</div>
              <div v-if="s.latest_reply" class="session-preview" :title="s.latest_reply">{{ s.latest_reply }}</div>
            </div>
            <span class="session-state" :class="'session-state--' + statusClass(s)">{{ statusLabel(s) }}</span>
          </div>
          <div v-if="!waitingSessions.length" class="empty">暂无等待输入的会话</div>
        </div>
      </div>

      <div class="codeg-col codeg-col-error">
        <div class="codeg-card-head">
          <strong>错误 / 已停止</strong>
          <span class="badge">错误 {{ status.error_count || 0 }} · 停止 {{ status.stopped_count || 0 }}</span>
        </div>
        <div class="codeg-session-list">
          <div v-for="s in errorSessions" :key="s.connection_id" class="session-row">
            <div class="session-main">
              <div class="session-name" :title="sessionTitle(s)">{{ sessionTitle(s) }}</div>
              <div v-if="s.latest_reply" class="session-preview" :title="s.latest_reply">{{ s.latest_reply }}</div>
            </div>
            <span class="session-state" :class="'session-state--' + statusClass(s)">{{ statusLabel(s) }}</span>
          </div>
          <div v-if="!errorSessions.length" class="empty">暂无错误或已停止会话</div>
        </div>
      </div>
    </template>
  </section>
</template>

<script setup>
import { ref, watch, computed, onMounted, onBeforeUnmount } from 'vue'
import { useTauri } from '../composables/useTauri'

const { invoke } = useTauri()

const props = defineProps({
  show: { type: Boolean, default: false },
})

const status = ref({ connected: false, error: null, sessions: [], system: {} })
const loading = ref(false)
let timer = null

const activeSessions = computed(() => (status.value.sessions || []).filter(s => ['prompting', 'connecting'].includes(s.status)))
const waitingSessions = computed(() => (status.value.sessions || []).filter(s => s.waiting_for))
const errorSessions = computed(() => (status.value.sessions || []).filter(s => ['disconnected', 'error'].includes(s.status) && !s.waiting_for))

function fmtNum(v) {
  const n = Number(v)
  if (!Number.isFinite(n)) return '0'
  return n >= 10 ? Math.round(n).toString() : n.toFixed(1)
}

function fmtBytes(gb) {
  const n = Number(gb)
  if (!Number.isFinite(n)) return '0 GB'
  const mb = n * 1024
  if (mb < 1) return Math.round(n * 1024 * 1024) + ' KB'
  if (mb < 1024) return (mb >= 10 ? Math.round(mb) : mb.toFixed(1)) + ' MB'
  return (n >= 10 ? Math.round(n) : n.toFixed(1)) + ' GB'
}

function sessionTitle(s) {
  if (s.session_title || s.title) return s.session_title || s.title
  return s.session_name || s.latest_reply || s.agent_type || '会话'
}

function statusMeta(s) {
  if (s.waiting_for) {
    const map = { question: '等待问题输入', permission: '等待权限确认', plan_approval: '等待计划确认' }
    return { label: map[s.waiting_for] || '等待输入', cls: 'waiting' }
  }
  switch (s.status) {
    case 'connecting': return { label: '连接中', cls: 'connecting' }
    case 'prompting': return { label: '运行中', cls: 'running' }
    case 'connected': return { label: '空闲', cls: 'idle' }
    case 'disconnected': return { label: '已停止', cls: 'stopped' }
    case 'error': return { label: '错误', cls: 'error' }
    default: return { label: s.status || '未知', cls: 'unknown' }
  }
}

function statusLabel(s) { return statusMeta(s).label }
function statusClass(s) { return statusMeta(s).cls }

async function refresh() {
  if (loading.value) return
  if (!props.show) return
  loading.value = true
  try {
    const r = await invoke('get_codeg_status')
    status.value = r || {}
    if (!r?.connected) {
      status.value.error = status.value.error || 'CodeG 服务器未连接'
    }
  } catch (e) {
    status.value = { ...status.value, connected: false, error: String(e) }
  } finally {
    loading.value = false
  }
}

watch(() => props.show, (v) => {
  if (v) refresh()
  else if (timer) { clearInterval(timer); timer = null }
})

onMounted(() => {
  if (props.show) refresh()
  timer = setInterval(refresh, 5000)
})

onBeforeUnmount(() => {
  if (timer) clearInterval(timer)
})
</script>

<style scoped>
.codeg-dock {
  display: grid;
  grid-template-columns: 280px 1fr 1fr 1fr;
  gap: 12px;
  min-height: var(--codeg-bar-height, 210px);
  padding: 12px 16px;
  border-top: 1px solid var(--border);
  background: var(--panel);
  flex-shrink: 0;
  overflow: hidden;
}
.codeg-error-banner {
  grid-column: 1 / -1;
  display: flex; align-items: center; gap: 10px;
  color: #ff6b6b; font-size: 13px;
}
.codeg-err-text { color: var(--muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.codeg-col {
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 10px 12px;
  min-width: 0;
  overflow: hidden;
  display: flex;
  flex-direction: column;
  background: var(--bg);
}
.codeg-card-head {
  display: flex; align-items: center; justify-content: space-between;
  font-size: 12px; margin-bottom: 8px;
}
.codeg-card-head strong { color: var(--text); font-weight: 600; }
.badge {
  font-size: 11px; color: var(--muted);
  background: var(--border); border-radius: 10px; padding: 2px 8px;
}
.codeg-stat-grid {
  display: grid; grid-template-columns: 1fr 1fr; gap: 4px 12px;
  overflow: hidden;
}
.stat-item { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.stat-label { font-size: 10px; color: var(--muted); }
.stat-value { font-size: 12px; color: var(--text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.codeg-session-list {
  overflow-y: auto; min-height: 0;
  display: flex; flex-direction: column; gap: 6px;
}
.session-row {
  display: flex; align-items: center; justify-content: space-between; gap: 8px;
  font-size: 12px; padding: 5px 8px;
  background: var(--panel); border: 1px solid var(--border); border-radius: 6px;
  min-width: 0;
}
.session-main {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
  flex: 1;
}
.session-name {
  color: var(--text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 12px;
}
.session-preview {
  color: var(--muted);
  font-size: 11px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.session-state {
  flex-shrink: 0; font-size: 10px; padding: 2px 6px; border-radius: 4px;
  white-space: nowrap;
}
.session-state--running { color: var(--green); background: rgba(53, 208, 165, .12); }
.session-state--connecting { color: #58a6ff; background: rgba(88, 166, 255, .12); }
.session-state--idle { color: #8b97b0; background: rgba(139, 151, 176, .12); }
.session-state--waiting { color: #ffb454; background: rgba(255, 180, 84, .12); }
.session-state--stopped { color: var(--muted); background: var(--border); }
.session-state--error { color: #ff6b6b; background: rgba(255, 107, 107, .12); }
.session-state--unknown { color: var(--muted); background: var(--border); }
.empty { color: var(--muted); font-size: 12px; padding: 8px 0; }
.codeg-refresh {
  margin-left: auto;
  background: var(--border); color: var(--text);
  border: 1px solid transparent; border-radius: 6px;
  padding: 4px 10px; font-size: 12px; cursor: pointer;
}
.codeg-refresh:hover { filter: brightness(1.2); }
.dot {
  width: 8px; height: 8px; border-radius: 50%;
  background: #ff6b6b; flex-shrink: 0;
}
</style>