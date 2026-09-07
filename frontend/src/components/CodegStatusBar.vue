<template>
  <footer v-if="show" class="codeg-bar" :class="{ connected: status.connected, error: !!status.error }">
    <template v-if="status.error">
      <span class="codeg-item codeg-status">CodeG 未连接</span>
      <span class="codeg-item codeg-err">{{ status.error }}</span>
    </template>
    <template v-else>
      <span class="codeg-item codeg-status">
        <span class="dot"></span>
        CodeG
      </span>
      <span class="codeg-item">会话 {{ status.session_count || 0 }}</span>
      <span class="codeg-item codeg-running">运行 {{ status.running_count || 0 }}</span>
      <span class="codeg-item codeg-stopped">停止 {{ status.stopped_count || 0 }}</span>
      <span class="codeg-item" :class="{ warn: status.waiting_input_count > 0 }">
        等待输入 {{ status.waiting_input_count || 0 }}
      </span>
      <span class="codeg-item codeg-session" :title="status.active_session_name || ''">
        当前 {{ status.active_session_name || '(无活跃会话)' }}
      </span>
      <span class="codeg-item codeg-proc">Node.js {{ status.system?.node_processes || 0 }}</span>
      <span class="codeg-item codeg-proc">智能体 {{ status.system?.agent_processes || 0 }}</span>
      <span class="codeg-item codeg-res">{{ fmtNum(status.system?.cpu_percent) }}% CPU</span>
      <span class="codeg-item codeg-res">内存 {{ fmtNum(status.system?.memory_used_gb) }}/{{ fmtNum(status.system?.memory_total_gb) }}G</span>
      <span v-if="status.system?.gpu_total_gb" class="codeg-item codeg-res">
        GPU {{ fmtNum(status.system?.gpu_used_gb) }}/{{ fmtNum(status.system?.gpu_total_gb) }}G
      </span>
      <span class="codeg-item codeg-res">C盘 {{ fmtNum(status.system?.c_drive_percent) }}%</span>
      <button v-if="!loading" class="codeg-refresh" title="刷新 CodeG 状态" @click="refresh">刷新</button>
      <span v-if="loading" class="codeg-item codeg-loading">刷新中…</span>
    </template>
  </footer>
</template>

<script setup>
import { ref, watch, onMounted, onBeforeUnmount, computed } from 'vue'
import { useTauri } from '../composables/useTauri'

const { invoke } = useTauri()

const props = defineProps({
  show: { type: Boolean, default: false },
})

const status = ref({ connected: false, error: null, sessions: [], system: {} })
const loading = ref(false)
let timer = null

function fmtNum(v) {
  const n = Number(v)
  if (!Number.isFinite(n)) return '0'
  return n >= 10 ? Math.round(n).toString() : n.toFixed(1)
}

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
.codeg-bar {
  display: flex;
  align-items: center;
  gap: 14px;
  height: 42px;
  min-height: 42px;
  padding: 5px 20px;
  border-top: 1px solid var(--border);
  background: var(--panel);
  overflow-x: auto;
  white-space: nowrap;
  flex-shrink: 0;
  font-size: 12px;
  color: var(--muted);
}
.codeg-item { flex-shrink: 0; display: inline-flex; align-items: center; font-size: 12px; }
.codeg-status { font-weight: 600; color: var(--text); }
.codeg-running { color: var(--green); }
.codeg-stopped { color: var(--muted); }
.codeg-session { color: var(--text); max-width: 260px; overflow: hidden; text-overflow: ellipsis; }
.codeg-err { color: #ff6b6b; max-width: 46vw; overflow: hidden; text-overflow: ellipsis; }
.warn { color: #ffb454; }
.dot {
  width: 7px; height: 7px; border-radius: 50%;
  background: var(--green); box-shadow: 0 0 6px var(--green);
  margin-right: 6px;
}
.codeg-bar.error .dot { background: #ff6b6b; box-shadow: none; }
.codeg-refresh {
  margin-left: auto;
  flex-shrink: 0;
  background: var(--border);
  color: var(--text);
  border: 1px solid transparent;
  border-radius: 6px;
  padding: 4px 10px;
  font-size: 12px;
  cursor: pointer;
}
.codeg-refresh:hover { filter: brightness(1.2); }
.codeg-loading { margin-left: auto; color: var(--muted); }
</style>