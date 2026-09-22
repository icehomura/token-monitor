<template>
  <Teleport to="body">
    <div v-if="visible" class="modal-mask" @click.self="$emit('cancel')">
      <div class="modal">
        <div class="modal-head">
          <h2>{{ isNew ? '新建配置文件' : '编辑配置文件' }}</h2>
          <IconButton class="modal-close" title="关闭" @click="$emit('cancel')">
            <span style="font-size:15px">✕</span>
          </IconButton>
        </div>
        <div class="modal-body">
          <div class="profile-form">
            <div class="form-row">
              <label class="form-label">名称</label>
              <BaseInput v-model="form.name" placeholder="配置名称" />
            </div>
            <div class="form-row">
              <label class="form-label">转发地址</label>
              <BaseInput v-model="form.upstream_url" placeholder="https://…/v1/responses" />
            </div>
            <div class="form-row">
              <label class="form-label">模型名</label>
              <BaseInput v-model="form.model_override" placeholder="留空 = 使用原始模型" />
            </div>
            <div class="form-row">
              <label class="form-label">API Key</label>
              <BaseInput v-model="form.api_key" type="password" placeholder="sk-..." />
            </div>
            <div class="form-row">
              <label class="form-label">上游格式</label>
              <select v-model="form.upstream_format" class="select">
                <option value="responses">Responses API</option>
                <option value="chat_completions">Chat Completions</option>
                <option value="anthropic">Anthropic Messages</option>
              </select>
            </div>
            <div class="form-row">
              <label class="form-label">最大并发数</label>
              <BaseInput v-model.number="form.max_concurrency" type="number" :min="1" :max="999" spinner />
            </div>
          </div>
        </div>
        <div class="modal-footer">
          <BaseButton @click="$emit('cancel')">取消</BaseButton>
          <BaseButton variant="primary" @click="handleSave">
            <span style="margin-right:4px">✓</span> 保存配置
          </BaseButton>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<script setup>
import { ref, watch, computed } from 'vue'
import IconButton from './base/IconButton.vue'
import BaseButton from './base/BaseButton.vue'
import BaseInput from './base/BaseInput.vue'

const props = defineProps({
  visible: { type: Boolean, default: false },
  profile: { type: Object, default: null },
})

const emit = defineEmits(['cancel', 'save'])

const isNew = computed(() => !props.profile?.id)

const form = ref({
  id: '',
  name: '',
  upstream_url: '',
  upstream_format: 'responses',
  api_key: '',
  model_override: '',
  max_concurrency: 20,
})

watch(() => props.visible, (v) => {
  if (v && props.profile) {
    form.value = { ...props.profile }
  }
})

function handleSave() {
  emit('save', { ...form.value })
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
  width: 480px; max-width: calc(100vw - 48px);
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
</style>
