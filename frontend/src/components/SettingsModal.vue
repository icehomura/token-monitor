<template>
  <Teleport to="body">
    <div v-if="visible" class="modal-mask" @click.self="$emit('close')">
      <div class="modal">
        <div class="modal-head">
          <div>
            <h2>设置</h2>
            <p class="modal-sub">管理 API 配置文件与系统设置</p>
          </div>
          <IconButton class="modal-close" title="关闭" @click="$emit('close')">
            <span style="font-size:15px">✕</span>
          </IconButton>
        </div>

        <!-- Tab 条：用 v-show 切换，保证各卡片内部状态与 watch 不被销毁 -->
        <div class="tabs">
          <button
            v-for="tab in tabs"
            :key="tab.id"
            class="tab-btn"
            :class="{ active: activeTab === tab.id }"
            @click="activeTab = tab.id"
          >
            {{ tab.label }}
          </button>
        </div>

        <div class="modal-body">
        <div class="tab-content">

        <!-- ── Tab：AI 服务 ── -->
        <div v-show="activeTab === 'ai'" class="tab-pane">
        <!-- 配置文件管理 -->
        <SettingsCard title="API 配置文件" description="管理多组 API 地址、模型和密钥配置，点击切换激活" auto>
          <template #actions>
            <BaseButton variant="primary" @click="addNewProfile">
              <span style="margin-right:4px">+</span> 新建配置文件
            </BaseButton>
          </template>
          <div class="profile-list">
            <div
              v-for="p in profiles"
              :key="p.id"
              class="profile-item"
              :class="{ active: p.id === activeProfileId }"
              @click="selectProfile(p)"
            >
              <div class="profile-info">
                <span class="profile-name">{{ p.name }}</span>
                <span class="profile-detail">{{ p.upstream_url || '默认地址' }} · {{ p.model_override || '原始模型' }} · 并发{{ p.max_concurrency }}</span>
              </div>
              <div class="profile-actions">
                <span v-if="p.id === activeProfileId" class="profile-active-badge">激活</span>
                <IconButton class="profile-edit-btn" title="编辑" @click.stop="editProfile(p)">
                  <span style="font-size:12px">✎</span>
                </IconButton>
                <IconButton class="profile-del-btn" title="删除" @click.stop="deleteProfile(p)">
                  <span style="font-size:12px">✕</span>
                </IconButton>
              </div>
            </div>
          </div>

          <template #hint>
            <small :class="['ff-hint', profileMsgType]">{{ profileMsg }}</small>
          </template>
        </SettingsCard>



        <!-- 第一行：当前状态 + 端口 -->
        <div class="grid-2">
          <SettingsCard title="当前状态">
            <div class="status-row">
              <span class="status-icon">✓</span>
              <span class="status-text">{{ settingsInfo }}</span>
            </div>
          </SettingsCard>
          <SettingsCard title="服务端口" description="保存后自动重启监听">
            <div class="port-row">
              <BaseInput v-model.number="port" type="number" :min="1" :max="65535" spinner />
              <BaseButton variant="primary" @click="savePort">
                <span style="margin-right:4px">⟳</span> 保存并重启
              </BaseButton>
            </div>
            <template #hint>
              <small :class="['ff-hint', portMsgType]">{{ portMsg }}</small>
            </template>
          </SettingsCard>
        </div>

        <!-- 探针设置 -->
        <SettingsCard title="探针设置" description="定期探测上游服务可用性">
          <div class="settings-row">
            <span class="settings-label">探针开关</span>
            <BaseToggle v-model="probeEnabled" />
          </div>
          <div class="settings-row">
            <span class="settings-label">探测间隔（秒）</span>
            <BaseInput v-model.number="probeInterval" type="number" spinner :min="1" :max="1800" />
          </div>
          <div class="settings-actions">
            <BaseButton variant="primary" @click="saveProbe">保存探针设置</BaseButton>
          </div>
          <template #hint>
            <small :class="['ff-hint', probeMsgType]">{{ probeMsg }}</small>
          </template>
        </SettingsCard>

        </div><!-- /Tab AI 服务 -->

        <!-- ── Tab：余额 ── -->
        <div v-show="activeTab === 'balance'" class="tab-pane">
        <!-- 余额设置 -->
        <SettingsCard title="余额设置" description="定期查询上游账户余额">
          <div class="settings-row">
            <span class="settings-label">余额查询开关</span>
            <BaseToggle v-model="balanceEnabled" />
          </div>
          <div class="settings-row">
            <span class="settings-label">货币单位</span>
            <DdSelect :options="currencyOptions" v-model="balanceCurrency" />
          </div>
          <div class="settings-row">
            <span class="settings-label">查询间隔（秒）</span>
            <BaseInput v-model.number="balanceInterval" type="number" spinner :min="1" :max="1800" />
          </div>
          <div class="settings-row">
            <span class="settings-label">当前余额</span>
            <span :class="['ff-hint', balanceInfoType]">{{ balanceInfo }}</span>
          </div>
          <div class="settings-actions">
            <BaseButton variant="primary" @click="saveBalance">保存余额设置</BaseButton>
          </div>
          <template #hint>
            <small :class="['ff-hint', balanceMsgType]">{{ balanceMsg }}</small>
          </template>
        </SettingsCard>

        </div><!-- /Tab 余额 -->

        <!-- ── Tab：CodeG ── -->
        <div v-show="activeTab === 'codeg'" class="tab-pane">
        <!-- CodeG 服务器 -->
        <SettingsCard title="CodeG 服务器" description="连接 Codeg server，在 Token Monitor 底部显示并监控活跃会话" stretch>
          <div class="codeg-grid">
            <div class="codeg-field">
              <span class="codeg-label">服务器地址</span>
              <BaseInput v-model="codegUrl" placeholder="http://127.0.0.1:3080" />
            </div>
            <div class="codeg-field">
              <span class="codeg-label">服务器 Token</span>
              <div class="codeg-token-row">
                <BaseInput v-model="codegToken" :type="showToken ? 'text' : 'password'" placeholder="Codeg server CODEG_TOKEN" />
                <IconButton class="codeg-token-eye" title="显示/隐藏 Token" @click="showToken = !showToken">
                  <span>{{ showToken ? '隐藏' : '显示' }}</span>
                </IconButton>
              </div>
            </div>
            <div class="codeg-field codeg-toggle">
              <span class="codeg-label">底部 CodeG 行</span>
              <BaseToggle v-model="codegEnabled" labelOn="已开启" labelOff="已关闭" />
            </div>
            <div class="codeg-field codeg-toggle">
              <span class="codeg-label">高级自动恢复</span>
              <BaseToggle v-model="codegAutoRecovery" labelOn="已开启" labelOff="已关闭" />
              <small class="codeg-advanced-hint">会话超过阈值无输出，或进入错误状态时自动停止并重试</small>
            </div>
            <div class="codeg-field codeg-timeout">
              <span class="codeg-label">不活跃阈值（秒）</span>
              <BaseInput v-model.number="codegTimeout" type="number" :min="10" spinner />
            </div>
            <div class="codeg-field codeg-actions">
              <BaseButton variant="primary" @click="saveCodeg">保存 CodeG 配置</BaseButton>
            </div>
          </div>
          <template #hint>
            <small :class="['ff-hint', codegMsgType]">{{ codegMsg }}</small>
          </template>
        </SettingsCard>

        </div><!-- /Tab CodeG -->

        <!-- ── Tab：界面与系统 ── -->
        <div v-show="activeTab === 'system'" class="tab-pane">
        <!-- 两列：单位转换 + 并发数 -->
        <div class="grid-2">
          <SettingsCard title="词元数量单位转换" description="开启后超过 1000 显示为 K / M / B，保留 1 位小数" modifier="unit">
            <div class="toggle-row">
              <BaseToggle v-model="unitToggle" labelOn="已开启" labelOff="已关闭" />
            </div>
            <template #hint>
              <small :class="['ff-hint', unitMsgType]">{{ unitMsg }}</small>
            </template>
          </SettingsCard>
          <SettingsCard title="并发数限制" :description="`当前激活配置的上限`">
            <div class="status-row">
              <span class="status-icon" style="background:var(--blue)">⚡</span>
              <span class="status-text">当前并发 {{ stats.concurrency }} / {{ currentMaxConcurrency }}</span>
            </div>
          </SettingsCard>
        </div>

        <!-- 两列：关闭行为 + 开机自启 -->
        <div class="grid-2">
          <SettingsCard title="关闭按钮行为" description="点击关闭按钮时的默认操作">
            <DdSelect
              :options="closeActionOptions"
              v-model="closeAction"
            />
            <template #hint>
              <small :class="['ff-hint', closeActionMsgType]">{{ closeActionMsg }}</small>
            </template>
          </SettingsCard>
          <SettingsCard title="开机自启动" description="系统启动时自动运行程序">
            <div class="toggle-row">
              <BaseToggle v-model="autostart" labelOn="已开启" labelOff="已关闭" />
            </div>
            <template #hint>
              <small :class="['ff-hint', autostartMsgType]">{{ autostartMsg }}</small>
            </template>
          </SettingsCard>
        </div>

        <!-- 主题 -->
        <SettingsCard title="界面主题">
          <div class="theme-row">
            <div
              v-for="t in themeOptions"
              :key="t.v"
              class="theme-card"
              :class="{ active: themeName === t.v }"
              @click="setTheme(t.v)"
            >
              <span class="theme-icon">
                <ThemeIcon :name="t.icon" />
              </span>
              <div class="theme-info">
                <span class="theme-name">{{ t.label }}</span>
                <span class="theme-desc">{{ t.desc }}</span>
              </div>
              <svg v-if="themeName === t.v" class="theme-check" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg>
            </div>
          </div>
        </SettingsCard>

        </div><!-- /Tab 界面与系统 -->
        </div><!-- /tab-content -->
        </div><!-- /modal-body -->

        <!-- 底部 -->
        <div class="modal-footer">
          <span class="footer-hint">配置文件切换立即生效，无需重启服务</span>
        </div>
      </div>
    </div>
  </Teleport>
  <ProfileEditModal
    :visible="!!editingProfile"
    :profile="editingProfile"
    @cancel="editingProfile = null"
    @save="onProfileSave"
  />
</template>

<script setup>
import { ref, watch, computed } from 'vue'
import IconButton from './base/IconButton.vue'
import BaseButton from './base/BaseButton.vue'
import BaseInput from './base/BaseInput.vue'
import BaseToggle from './base/BaseToggle.vue'
import SettingsCard from './SettingsCard.vue'
import DdSelect from './DdSelect.vue'
import ThemeIcon from './ThemeIcon.vue'
import ProfileEditModal from './ProfileEditModal.vue'
import { useTauri } from '../composables/useTauri'

const { invoke } = useTauri()

const props = defineProps({
  visible: { type: Boolean, default: false },
  themeName: { type: String, default: 'dark' },
  convertUnits: { type: Boolean, default: false },
  stats: { type: Object, default: () => ({ concurrency: 0 }) },
})

const emit = defineEmits(['close', 'update:themeName', 'update:convertUnits'])

// ──────── Tab 分组 ────────
const DEFAULT_TAB = 'ai'
const activeTab = ref(DEFAULT_TAB)
const tabs = [
  { id: 'ai', label: 'AI 服务' },
  { id: 'balance', label: '余额' },
  { id: 'codeg', label: 'CodeG' },
  { id: 'system', label: '界面与系统' },
]

const themeOptions = [
  { v: 'dark', label: '深色', desc: '暗色护眼', icon: 'dark' },
  { v: 'light', label: '浅色', desc: '明亮简洁', icon: 'light' },
  { v: 'system', label: '跟随系统', desc: '自动切换', icon: 'system' },
]

const closeActionOptions = [
  { v: 'ask', label: '询问' },
  { v: 'minimize', label: '最小化到托盘' },
  { v: 'quit', label: '关闭程序' },
]

function setTheme(v) { emit('update:themeName', v) }

const unitToggle = computed({
  get: () => props.convertUnits,
  set: (v) => emit('update:convertUnits', v),
})

// ──────── Profile 状态 ────────
const profiles = ref([])
const activeProfileId = ref('')
const editingProfile = ref(null)
const profileMsg = ref('')
const profileMsgType = ref('')

const currentMaxConcurrency = computed(() => {
  const p = profiles.value.find(p => p.id === activeProfileId.value)
  return p ? p.max_concurrency : 20
})

// ──────── 其他设置状态 ────────
const port = ref(8188)
const portMsg = ref('')
const portMsgType = ref('')
const settingsInfo = ref('')
const unitMsg = ref('')
const unitMsgType = ref('')
const closeAction = ref('ask')
const closeActionMsg = ref('')
const closeActionMsgType = ref('')
const autostart = ref(false)
const autostartMsg = ref('')
const autostartMsgType = ref('')

// ──────── 探针设置状态 ────────
const probeEnabled = ref(false)
const probeInterval = ref(15)
const probeMsg = ref('')
const probeMsgType = ref('')

// ──────── 余额设置状态 ────────
const balanceEnabled = ref(false)
const balanceCurrency = ref('CNY')
const balanceInterval = ref(15)
const balanceMsg = ref('')
const balanceMsgType = ref('')
const balanceInfo = ref('—')       // 只读的当前余额展示
const balanceInfoType = ref('')    // '' 为灰色提示，'ok' 为绿色

const currencyOptions = [
  { v: 'CNY', label: '人民币 (CNY)' },
  { v: 'USD', label: '美元 (USD)' },
]

// 间隔统一裁剪到 1~1800 秒，非法值回落到默认 15
function clampInterval(v) {
  const n = Number(v)
  if (!Number.isFinite(n) || n <= 0) return 15
  return Math.min(1800, Math.max(1, Math.round(n)))
}

// ──────── CodeG 服务器状态 ────────
const codegEnabled = ref(false)
const codegUrl = ref('http://127.0.0.1:3080')
const codegToken = ref('')
const codegAutoRecovery = ref(true)
const codegTimeout = ref(120)
const showToken = ref(false)
const codegMsg = ref('')
const codegMsgType = ref('')

// ──────── 打开时加载数据 ────────
// 防止打开 modal 时 watcher 触发多余的保存
const _loadingSettings = ref(true)

watch(() => props.visible, async (v) => {
  if (!v) return
  _loadingSettings.value = true
  // 每次打开都回到默认 Tab
  activeTab.value = DEFAULT_TAB
  // 加载 profiles
  try {
    const r = await invoke('get_profiles')
    profiles.value = r.profiles || []
    activeProfileId.value = r.active_profile_id || ''
  } catch {}
  // 加载通用设置
  try {
    const s = await invoke('get_settings')
    port.value = s.port
    settingsInfo.value = `API Key：${s.has_api_key ? '已配置' : '未配置'}` +
      (s.model_override ? ` · 强制模型 ${s.model_override}` : ' · 未强制模型') +
      ` · 并发上限 ${s.max_concurrency}`
  } catch {}
  try {
    const ca = await invoke('get_close_action')
    closeAction.value = ca.action || 'ask'
  } catch {}
  try {
    autostart.value = await invoke('get_autostart')
  } catch {}
  // 加载 CodeG 配置
  try {
    const c = await invoke('get_codeg_settings')
    const cfg = c.config || {}
    codegEnabled.value = !!cfg.enabled
    codegUrl.value = cfg.server_url || 'http://127.0.0.1:3080'
    codegToken.value = cfg.token || ''
    codegAutoRecovery.value = cfg.auto_recovery !== false
    codegTimeout.value = cfg.inactivity_timeout_secs || 120
  } catch {}
  // 加载探针设置
  try {
    const p = await invoke('get_probe_settings') || {}
    const cfg = p.config || p
    probeEnabled.value = !!cfg.enabled
    probeInterval.value = clampInterval(cfg.interval_secs)
  } catch {}
  // 加载余额设置
  try {
    const b = await invoke('get_balance_settings') || {}
    const cfg = b.config || b
    balanceEnabled.value = !!cfg.enabled
    balanceCurrency.value = cfg.currency || 'CNY'
    balanceInterval.value = clampInterval(cfg.interval_secs)
  } catch {}
  // 加载当前余额（只读展示）
  await refreshBalance()
  _loadingSettings.value = false
})

watch(unitToggle, (v) => {
  if (_loadingSettings.value) return
  unitMsg.value = v ? '✓ 已开启单位转换' : '✓ 已关闭单位转换'
  unitMsgType.value = 'ok'
})

watch(closeAction, async (v) => {
  if (_loadingSettings.value) return
  closeActionMsg.value = '保存中…'; closeActionMsgType.value = ''
  try {
    await invoke('set_close_action', { action: v })
    const labels = { ask: '询问', minimize: '最小化到托盘', quit: '关闭程序' }
    closeActionMsg.value = `✓ 已设为"${labels[v] || v}"`
    closeActionMsgType.value = 'ok'
  } catch (e) {
    closeActionMsg.value = String(e); closeActionMsgType.value = 'err'
  }
})

watch(autostart, async (v) => {
  if (_loadingSettings.value) return
  autostartMsg.value = '设置中…'; autostartMsgType.value = ''
  try {
    await invoke('set_autostart', { enabled: v })
    autostartMsg.value = v ? '✓ 已开启开机自启' : '✓ 已关闭开机自启'
    autostartMsgType.value = 'ok'
  } catch (e) {
    autostartMsg.value = String(e); autostartMsgType.value = 'err'
  }
})

// 余额开关变化时同步一次只读状态行（保存仍由按钮触发）
watch(balanceEnabled, async (v) => {
  if (_loadingSettings.value) return
  if (!v) { balanceInfo.value = '—'; balanceInfoType.value = ''; return }
  await refreshBalance()
})

// ──────── Profile 操作 ────────
function selectProfile(p) {
  if (p.id === activeProfileId.value) return
  invoke('set_active_profile', { id: p.id }).then(r => {
    activeProfileId.value = r.active_profile_id
    profileMsg.value = `✓ 已切换到「${p.name}」`
    profileMsgType.value = 'ok'
  }).catch(e => {
    profileMsg.value = String(e); profileMsgType.value = 'err'
  })
}

function addNewProfile() {
  editingProfile.value = {
    id: '',
    name: `配置 ${profiles.value.length + 1}`,
    upstream_url: '',
    api_key: '',
    model_override: '',
    max_concurrency: 20,
  }
}

function editProfile(p) {
  editingProfile.value = { ...p }
}

function deleteProfile(p) {
  invoke('delete_profile', { id: p.id }).then(r => {
    profiles.value = r.profiles || []
    activeProfileId.value = r.active_profile_id || ''
    profileMsg.value = `✓ 已删除「${p.name}」`
    profileMsgType.value = 'ok'
  }).catch(e => {
    profileMsg.value = String(e); profileMsgType.value = 'err'
  })
}

function onProfileSave(profile) {
  // 新建时生成 id
  if (!profile.id) profile.id = Date.now().toString(36) + Math.random().toString(36).slice(2, 6)
  invoke('save_profile', { profile }).then(r => {
    profiles.value = r.profiles || []
    editingProfile.value = null
    profileMsg.value = `✓ 已保存「${profile.name}」`
    profileMsgType.value = 'ok'
  }).catch(e => {
    profileMsg.value = String(e); profileMsgType.value = 'err'
  })
}

// ──────── CodeG ────────
async function saveCodeg() {
  codegMsg.value = '保存中…'; codegMsgType.value = ''
  const cfg = {
    enabled: codegEnabled.value,
    server_url: codegUrl.value.trim(),
    token: codegToken.value,
    auto_recovery: codegAutoRecovery.value,
    inactivity_timeout_secs: Math.max(10, Number(codegTimeout.value) || 120),
  }
  try {
    const r = await invoke('set_codeg_settings', { config: cfg })
    codegMsg.value = `✓ 已保存${r.config.enabled ? '，CodeG 行已开启' : '，CodeG 行已关闭'}`
    codegMsgType.value = 'ok'
  } catch (e) {
    codegMsg.value = String(e); codegMsgType.value = 'err'
  }
}

// ──────── 探针 ────────
async function saveProbe() {
  probeMsg.value = '保存中…'; probeMsgType.value = ''
  const cfg = {
    enabled: probeEnabled.value,
    interval_secs: clampInterval(probeInterval.value),
  }
  try {
    const r = await invoke('set_probe_settings', { config: cfg }) || {}
    // 用后端返回的配置回填
    const saved = r.config || r
    probeEnabled.value = !!saved.enabled
    probeInterval.value = clampInterval(saved.interval_secs)
    probeMsg.value = `✓ 已保存${probeEnabled.value ? '，探针已开启' : '，探针已关闭'}`
    probeMsgType.value = 'ok'
  } catch (e) {
    probeMsg.value = String(e); probeMsgType.value = 'err'
  }
}

// ──────── 余额 ────────
async function saveBalance() {
  balanceMsg.value = '保存中…'; balanceMsgType.value = ''
  const cfg = {
    enabled: balanceEnabled.value,
    currency: balanceCurrency.value,
    interval_secs: clampInterval(balanceInterval.value),
  }
  try {
    const r = await invoke('set_balance_settings', { config: cfg }) || {}
    // 用后端返回的配置回填
    const saved = r.config || r
    balanceEnabled.value = !!saved.enabled
    balanceCurrency.value = saved.currency || 'CNY'
    balanceInterval.value = clampInterval(saved.interval_secs)
    balanceMsg.value = `✓ 已保存${balanceEnabled.value ? '，余额查询已开启' : '，余额查询已关闭'}`
    balanceMsgType.value = 'ok'
    await refreshBalance()
  } catch (e) {
    balanceMsg.value = String(e); balanceMsgType.value = 'err'
  }
}

// 读取当前余额，填充只读状态行（不可用属于正常状态，用灰色而非红色）
async function refreshBalance() {
  try {
    const b = await invoke('get_balance') || {}
    if (b.supported === false) {
      balanceInfo.value = b.reason || '当前上游不支持余额查询'
      balanceInfoType.value = ''
    } else if (b.available === true) {
      const sym = (b.currency || balanceCurrency.value) === 'USD' ? '$' : '¥'
      balanceInfo.value = `余额 ${sym}${b.total != null ? b.total : '--'}`
      balanceInfoType.value = 'ok'
    } else {
      balanceInfo.value = b.reason || '余额暂不可用'
      balanceInfoType.value = ''
    }
  } catch (e) {
    balanceInfo.value = String(e)
    balanceInfoType.value = ''
  }
}

// ──────── 端口 ────────
async function savePort() {
  portMsg.value = '重启中…'; portMsgType.value = ''
  try {
    const r = await invoke('set_port', { port: port.value })
    portMsg.value = `✓ 已在端口 ${r.port} 重新监听`; portMsgType.value = 'ok'
  } catch (e) {
    portMsg.value = String(e); portMsgType.value = 'err'
  }
}
</script>

<style scoped>
.modal-mask {
  position: fixed; inset: 0;
  background: rgba(5, 10, 20, .55);
  backdrop-filter: blur(3px);
  display: flex; align-items: center; justify-content: center;
  z-index: 100;
}
.modal {
  width: 720px; max-width: calc(100vw - 48px);
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 12px;
  padding: 16px 24px 14px;
  box-shadow: 0 12px 40px rgba(0,0,0,.35);
  max-height: calc(100vh - 64px);
  display: flex;
  flex-direction: column;
}
.modal-body {
  overflow-y: auto;
  min-height: 0;
  margin: 0 -4px;
  padding: 0 4px;
  scrollbar-gutter: stable;
}
.modal-head {
  display: flex; align-items: flex-start; justify-content: space-between;
  margin-bottom: 12px;
  flex-shrink: 0;
}
.modal-head h2 { font-size: 16px; font-weight: 600; margin: 0; }
.modal-sub { font-size: 12px; color: var(--muted); margin: 2px 0 0; }
.modal-close {
  width: 34px !important;
  height: 34px !important;
  flex-shrink: 0;
  border-radius: 6px;
}

/* Tab 条 */
.tabs {
  display: flex;
  gap: 2px;
  margin-bottom: 12px;
  background: var(--bg);
  border-radius: 8px;
  padding: 3px;
  flex-shrink: 0;
}
.tab-btn {
  flex: 1;
  padding: 7px 12px;
  font-size: 12px;
  font-weight: 500;
  color: var(--muted);
  background: transparent;
  border: none;
  border-radius: 6px;
  cursor: pointer;
  transition: all .15s;
}
.tab-btn:hover { color: var(--text); }
.tab-btn.active {
  background: var(--panel);
  color: var(--text);
  font-weight: 600;
}

/* 卡片间距：卡片直接位于各自 Tab 面板内 */
.tab-pane > .settings-card,
.tab-pane > .grid-2 { margin-bottom: 10px; }
.tab-pane > :last-child { margin-bottom: 0; }
.grid-2 { align-items: stretch; }

/* 通用「标签 + 控件」行（探针 / 余额） */
.settings-row {
  display: flex; align-items: center; justify-content: space-between;
  gap: 12px; width: 100%; min-width: 0;
}
.settings-label { font-size: 12px; color: var(--muted); flex-shrink: 0; }
.settings-row .input-wrap { flex: 0 0 150px; min-width: 0; }
.settings-row .ff-hint { margin-top: 0; }
.settings-actions { display: flex; justify-content: flex-end; width: 100%; }

.ff-hint {
  display: block;
  margin-top: 8px;
  margin-left: auto;
  text-align: right;
  color: var(--muted);
  font-size: 11px;
  min-height: 14px;
  max-width: 100%;
}
.ff-hint.ok { color: var(--green); }
.ff-hint.err { color: #ff6b6b; }

/* Profile 列表 */
.profile-list { display: flex; flex-direction: column; gap: 6px; width: 100%; }
.profile-item {
  display: flex; align-items: center; justify-content: space-between;
  padding: 8px 12px;
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 8px;
  cursor: pointer;
  transition: border-color .15s, background .15s;
}
.profile-item:hover { border-color: var(--muted); }
.profile-item.active { border-color: var(--blue); background: rgba(79,140,255,.08); }
.profile-info { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.profile-name { font-size: 13px; font-weight: 600; color: var(--text); }
.profile-detail { font-size: 11px; color: var(--muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.profile-actions { display: flex; align-items: center; gap: 6px; flex-shrink: 0; }
.profile-active-badge {
  font-size: 11px; color: var(--blue); background: rgba(79,140,255,.12);
  padding: 2px 8px; border-radius: 4px; font-weight: 600;
}
.profile-edit-btn, .profile-del-btn {
  width: 26px !important; height: 26px !important; border-radius: 4px;
}

/* Profile 编辑表单 */
.profile-form { display: flex; flex-direction: column; gap: 8px; width: 100%; }
.form-row { display: flex; align-items: center; gap: 10px; }
.form-label { font-size: 12px; color: var(--muted); width: 80px; flex-shrink: 0; text-align: right; }
.form-row .input-wrap { flex: 1; }
.form-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 6px; }

/* 端口行 */
.port-row { display: flex; align-items: center; gap: 10px; width: 100%; min-width: 0; }
.port-row .input-wrap { flex: 0 0 150px; min-width: 0; }

/* CodeG 服务器 */
.codeg-grid {
  display: grid;
  grid-template-columns: 1fr 1fr 1fr;
  gap: 10px 14px;
  width: 100%;
}
.codeg-field {
  display: flex;
  flex-direction: column;
  gap: 5px;
  min-width: 0;
}
.codeg-label { font-size: 11px; color: var(--muted); }
.codeg-token-row { display: flex; align-items: center; gap: 6px; min-width: 0; }
.codeg-token-row .input-wrap { flex: 1; min-width: 0; }
.codeg-token-eye {
  width: 34px !important; height: 34px !important;
  flex-shrink: 0; border-radius: 6px; font-size: 12px;
}
.codeg-toggle { justify-content: center; }
.codeg-advanced-hint {
  font-size: 10px;
  color: var(--muted);
  max-width: 150px;
  line-height: 1.35;
}
.codeg-timeout .input-wrap { width: 120px; }
.codeg-actions { justify-content: flex-end; }

/* 主题 */
.theme-row { display: grid; grid-template-columns: repeat(3, 1fr); gap: 10px; }
.theme-card {
  display: flex; align-items: center; gap: 10px;
  padding: 12px 14px;
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 8px;
  cursor: pointer;
  transition: border-color .15s, background .15s;
  position: relative;
}
.theme-card:hover { border-color: var(--muted); }
.theme-card.active { border-color: var(--blue); background: rgba(79,140,255,.08); }
.theme-icon { font-size: 22px; flex-shrink: 0; }
.theme-info { display: flex; flex-direction: column; gap: 1px; }
.theme-name { font-size: 13px; font-weight: 600; color: var(--text); }
.theme-desc { font-size: 11px; color: var(--muted); }
.theme-check { position: absolute; top: 8px; right: 8px; color: var(--blue); }

.grid-2 {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 12px;
}

.modal-footer {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 16px;
  padding-top: 14px;
  margin-top: 12px;
  border-top: 1px solid var(--border);
  flex-shrink: 0;
}
.footer-hint { font-size: 12px; color: var(--muted); }

.status-row { display: flex; align-items: center; gap: 6px; justify-content: center; width: 100%; }
.status-icon {
  display: inline-flex; align-items: center; justify-content: center;
  width: 20px; height: 20px; border-radius: 50%;
  background: var(--green); color: #fff;
  font-size: 11px; font-weight: 700; flex-shrink: 0;
}
.status-text { font-size: 12px; color: var(--text); }
</style>
