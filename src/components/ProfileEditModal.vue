<template>
  <Teleport to="body">
    <div v-if="visible" class="modal-mask" @click.self="$emit('cancel')">
      <div class="modal">
        <div class="modal-head">
          <h2>{{ isNew ? '新建渠道' : '编辑渠道' }}</h2>
        </div>
        <div class="modal-body">
          <div class="profile-form">
            <div class="form-row">
              <label class="form-label">名称</label>
              <BaseInput v-model="form.name" placeholder="配置名称" @blur="debouncedSave" />
            </div>
            <div class="form-row">
              <label class="form-label">参与调度</label>
              <BaseToggle v-model="form.enabled" labelOn="已开启" labelOff="已关闭" @blur="debouncedSave" />
            </div>
            <div class="form-row">
              <label class="form-label">转发地址</label>
              <BaseInput v-model="form.upstream_url" placeholder="https://…/v1/responses" @blur="debouncedSave" />
            </div>
            <div class="form-row">
              <label class="form-label">模型名</label>
              <BaseInput v-model="form.model_override" placeholder="留空 = 使用原始模型" @blur="debouncedSave" />
            </div>
            <div class="form-row">
              <label class="form-label">API Key</label>
              <BaseInput v-model="form.api_key" type="password" placeholder="sk-..." @blur="debouncedSave" />
            </div>
            <div class="form-row">
              <label class="form-label">上游格式</label>
              <select v-model="form.upstream_format" class="select" @blur="debouncedSave">
                <option value="responses">Responses API</option>
                <option value="chat_completions">Chat Completions</option>
                <option value="anthropic">Anthropic Messages</option>
              </select>
            </div>
            <div class="form-row">
              <label class="form-label">最大并发数</label>
              <BaseInput v-model.number="form.max_concurrency" type="number" :min="1" :max="999" spinner @blur="debouncedSave" />
            </div>
            <div class="form-row-rate">
              <div class="form-rate-item">
                <label class="form-label">RPM 限制</label>
                <BaseInput v-model.number="form.max_rpm" type="number" :min="0" placeholder="0 = 不限制" @blur="debouncedSave" />
              </div>
              <div class="form-rate-item">
                <label class="form-label">TPM 限制</label>
                <BaseInput v-model.number="form.max_tpm" type="number" :min="0" placeholder="0 = 不限制" @blur="debouncedSave" />
              </div>
              <div class="form-rate-item">
                <label class="form-label">调度权重</label>
                <BaseInput v-model.number="form.weight" type="number" :min="1" :max="1000" spinner @blur="debouncedSave" />
              </div>
            </div>
          </div>
        </div>
        <div class="modal-footer">
          <span v-if="saved" class="save-status ok">✓ 已保存</span>
          <span v-else-if="saving" class="save-status">保存中...</span>
          <BaseButton variant="primary" @click="debouncedSave(); $emit('cancel')">保存并关闭</BaseButton>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<script setup>
import { ref, watch, computed, onUnmounted } from 'vue'
import IconButton from './base/IconButton.vue'
import BaseButton from './base/BaseButton.vue'
import BaseInput from './base/BaseInput.vue'
import BaseToggle from './base/BaseToggle.vue'

const props = defineProps({
  visible: { type: Boolean, default: false },
  profile: { type: Object, default: null },
})

const emit = defineEmits(['cancel', 'save'])

const isNew = computed(() => !props.profile?.id)

const form = ref({
  id: '',
  name: '',
  enabled: true,
  upstream_url: '',
  upstream_format: 'responses',
  api_key: '',
  model_override: '',
  max_concurrency: 20,
  max_rpm: 0,
  max_tpm: 0,
  weight: 100,
})

watch(() => props.visible, (v) => {
  if (v && props.profile) {
    form.value = { ...props.profile }
  }
})

const saving = ref(false)
const saved = ref(false)

// 防抖保存定时器
let saveTimer = null

function debouncedSave() {
  if (saveTimer) clearTimeout(saveTimer)
  saveTimer = setTimeout(() => {
    handleSave()
  }, 500)
}

onUnmounted(() => {
  if (saveTimer) clearTimeout(saveTimer)
})

async function handleSave() {
  if (saving.value) return
  saving.value = true
  try {
    emit('save', { ...form.value })
    saved.value = true
    setTimeout(() => { saved.value = false }, 2000)
  } finally {
    saving.value = false
  }
}
</script>

<style scoped>
.modal-mask {
  position: fixed; inset: 0;
  background: rgba(5, 10, 20, .65);
  backdrop-filter: blur(4px);
  display: flex; align-items: center; justify-content: center;
  z-index: 200;
}
.modal {
  width: 560px; max-width: calc(100vw - 48px);
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 12px;
  padding: 16px 24px 14px;
  box-shadow: 0 16px 48px rgba(0,0,0,.45);
  display: flex;
  flex-direction: column;
}
.modal-head {
  display: flex; align-items: center; justify-content: space-between;
  margin-bottom: 16px;
  flex-shrink: 0;
}
.modal-head h2 { font-size: 15px; font-weight: 600; margin: 0; }
.modal-close {
  width: 34px !important;
  height: 34px !important;
  flex-shrink: 0;
  border-radius: 6px;
}
.modal-body {
  flex: 1;
  min-height: 0;
}
.profile-form { display: flex; flex-direction: column; gap: 12px; width: 100%; }
.form-row { display: flex; align-items: center; gap: 10px; }
.form-label { font-size: 12px; color: var(--muted); width: 80px; flex-shrink: 0; text-align: right; }
.form-row :deep(.input-wrap) { flex: 1; }
.select {
  background: var(--bg); color: var(--text);
  border: 1px solid var(--border); border-radius: 6px;
  padding: 7px 10px; font-size: 13px; outline: none;
  width: 100%; min-width: 0; flex: 1;
  cursor: pointer;
}
.select:focus { border-color: var(--blue); }
.form-row-rate {
  display: grid;
  grid-template-columns: 1fr 1fr 1fr;
  gap: 10px;
}
.form-rate-item { display: flex; flex-direction: column; gap: 4px; }
.form-rate-item .form-label { width: auto; text-align: left; }
.form-rate-item :deep(.input-wrap) { width: 100%; }
.modal-footer {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 10px;
  padding-top: 16px;
  margin-top: 16px;
  border-top: 1px solid var(--border);
  flex-shrink: 0;
}
.save-status {
  font-size: 12px;
  color: var(--muted);
  margin-right: auto;
}
.save-status.ok {
  color: var(--green);
}
</style>
