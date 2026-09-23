<template>
  <Teleport to="body">
    <div v-if="visible" class="modal-mask" @click.self="$emit('close')">
      <div class="modal">
        <div class="modal-head">
          <div>
            <h2>复制 API 地址</h2>
            <p class="modal-sub">选择要复制的 API 类型</p>
          </div>
          <IconButton class="modal-close" title="关闭" @click="$emit('close')">
            <span style="font-size:15px">✕</span>
          </IconButton>
        </div>

        <div class="modal-body">
          <div class="api-list">
            <div
              v-for="api in apiOptions"
              :key="api.path"
              class="api-item"
              @click="copyApi(api)"
            >
              <div class="api-info">
                <span class="api-name">{{ api.name }}</span>
                <span class="api-desc">{{ api.desc }}</span>
              </div>
              <div class="api-path">
                <code>{{ api.fullPath }}</code>
                <IconButton class="copy-icon" title="复制">
                  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                    <rect x="9" y="9" width="13" height="13" rx="2" />
                    <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />
                  </svg>
                </IconButton>
              </div>
            </div>
          </div>
        </div>

        <div class="modal-footer">
          <span class="footer-hint">{{ copyMsg }}</span>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<script setup>
import { ref, computed } from 'vue'
import IconButton from './base/IconButton.vue'

const props = defineProps({
  visible: { type: Boolean, default: false },
  port: { type: Number, default: 8188 },
})

const emit = defineEmits(['close'])

const copyMsg = ref('')

const baseUrl = computed(() => `http://127.0.0.1:${props.port}`)

const apiOptions = computed(() => [
  {
    name: 'Anthropic Messages',
    desc: 'Anthropic 原生格式，兼容 Claude SDK',
    path: '',
    fullPath: baseUrl.value,
  },
  {
    name: 'OpenAI Chat Completions',
    desc: 'Chat Completions 格式，兼容 OpenAI SDK',
    path: '/v1/chat/completions',
    fullPath: `${baseUrl.value}/v1/chat/completions`,
  },
  {
    name: 'OpenAI Responses',
    desc: 'Responses 格式，原生透传',
    path: '/v1/responses',
    fullPath: `${baseUrl.value}/v1/responses`,
  },
])

async function copyApi(api) {
  try {
    await navigator.clipboard.writeText(api.fullPath)
    copyMsg.value = `已复制「${api.name}」 ✓`
  } catch {
    copyMsg.value = '复制失败'
  }
  setTimeout(() => { copyMsg.value = '' }, 2000)
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
  width: 480px; max-width: calc(100vw - 48px);
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 12px;
  padding: 16px 24px 14px;
  box-shadow: 0 12px 40px rgba(0,0,0,.35);
  display: flex;
  flex-direction: column;
}
.modal-head {
  display: flex; align-items: flex-start; justify-content: space-between;
  margin-bottom: 16px;
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

.modal-body {
  min-height: 0;
}

.api-list { display: flex; flex-direction: column; gap: 8px; }
.api-item {
  display: flex; align-items: center; justify-content: space-between;
  padding: 12px 14px;
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 8px;
  cursor: pointer;
  transition: border-color .15s, background .15s;
}
.api-item:hover { border-color: var(--blue); background: rgba(79,140,255,.06); }
.api-info { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.api-name { font-size: 13px; font-weight: 600; color: var(--text); }
.api-desc { font-size: 11px; color: var(--muted); }
.api-path { display: flex; align-items: center; gap: 8px; flex-shrink: 0; }
.api-path code {
  font-size: 11px;
  color: var(--muted);
  background: var(--border);
  padding: 3px 8px;
  border-radius: 4px;
}
.copy-icon {
  width: 28px !important; height: 28px !important; border-radius: 4px;
}

.modal-footer {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  padding-top: 12px;
  margin-top: 12px;
  flex-shrink: 0;
}
.footer-hint { font-size: 12px; color: var(--muted); min-height: 16px; }
</style>
