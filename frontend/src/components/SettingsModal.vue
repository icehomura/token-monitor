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

        <div class="modal-body">
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

        </div>

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

// ──────── 打开时加载数据 ────────
// 防止打开 modal 时 watcher 触发多余的保存
const _loadingSettings = ref(true)

watch(() => props.visible, async (v) => {
  if (!v) return
  _loadingSettings.value = true
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

.modal-body > .settings-card,
.modal-body > .grid-2 { margin-bottom: 10px; }
.modal-body > .grid-2:last-child { margin-bottom: 0; }
.grid-2 { align-items: stretch; }

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
