# AGENTS.md

## Project Overview

Token Monitor 是一个基于 Tauri 2 的本地 AI 网关，提供多渠道代理转发 + RPM/TPM 实时统计。前端 Vue 3 + Vite，后端 Rust + Axum + SQLite (WAL)。

## Tech Stack

- **Runtime**: Tauri 2 (Rust backend + WebView frontend)
- **Backend**: Rust, Axum, tokio, reqwest (rustls-tls), rusqlite (bundled)
- **Frontend**: Vue 3 (Composition API, `<script setup>`), Vite, ECharts
- **Package Manager**: bun (前端), cargo (后端)
- **IPC**: Tauri invoke / events (Rust <-> Vue)

## Project Structure

```
token-monitor/              # 项目根目录（前端根）
├── package.json            # 前端依赖与脚本
├── vite.config.js          # Vite 配置
├── index.html              # 入口 HTML
├── src/                    # Vue 前端源码
│   ├── main.js             # Vue 应用入口
│   ├── App.vue             # 根组件
│   ├── components/         # UI 组件
│   ├── composables/        # Vue 组合式函数 (useTauri, useStats, useTheme)
│   └── utils/              # 工具函数 (format.js)
├── src-tauri/              # Rust 后端
│   ├── Cargo.toml          # Rust 依赖
│   ├── tauri.conf.json     # Tauri 配置
│   ├── build.rs            # Tauri 构建脚本
│   ├── capabilities/       # Tauri 权限声明
│   └── src/                # Rust 源码
│       ├── main.rs         # Tauri 命令注册、配置管理
│       ├── proxy.rs        # HTTP 代理核心（路由、格式转换、AIMD 调度）
│       ├── scheduler.rs    # 多渠道调度器（加权随机 + AIMD 自适应并发）
│       ├── balance.rs      # DeepSeek 余额查询
│       ├── probe.rs        # AI 服务探针
│       └── stats.rs        # SQLite 统计存储、并发槽位管理
├── icons/                  # 应用图标
└── docs/                   # 文档与截图
```

## Build & Run

```bash
# 安装前端依赖
bun install

# 前端开发（仅 Vite 热更新）
bun run dev

# 完整 Tauri 开发（前端 + Rust 后端）
bun run tauri dev

# 构建发布版本
bun run tauri build

# 后端测试（需在 src-tauri 目录下）
cd src-tauri && cargo test
```

## Key Architecture Decisions

### 代理路由 (proxy.rs)
- 三个入口：`/v1/chat/completions`、`/v1/responses`、`/v1/messages`
- 每个入口支持三种上游格式自动转换（Chat <-> Responses <-> Anthropic）
- 渠道选择通过 `wait_for_slot()` 获取 `Lease`（含 profile_id + 并发守卫）
- 流式与非流式共用 `Lease::Drop` 归还槽位，防泄漏

### 调度器 (scheduler.rs)
- 加权随机选择渠道，每个渠道独立并发 / RPM / TPM 限制
- AIMD（加性增/乘性减）自适应并发控制
- 429 响应触发乘性减，2xx 响应触发加性增

### IPC 通信模式
- Rust -> Vue: `app.emit("event-name", payload)` + Vue `listen("event-name", cb)`
- Vue -> Rust: `invoke("command_name", { args })` + `#[tauri::command]`
- 常用事件：`stats-updated`、`balance-query-triggered`、`close-requested`

### 余额查询 (balance.rs)
- 仅 DeepSeek 官方上游 (`api.deepseek.com`) 支持余额查询
- `get_balance`: 走 acquire_lease 闸门，占用并发槽位
- `get_channel_balance(profile_id)`: 按指定渠道直接查询，不占并发槽位
- 金额以字符串原样透传，不做浮点转换

## Code Conventions

### Rust
- 命名：snake_case 函数/变量，PascalCase 类型
- 错误处理：Tauri 命令返回 `Result<Value, String>`，错误信息用中文
- 并发：`OnceLock<RwLock<T>>` 做全局状态，`tokio::spawn` 做异步任务
- 注释语言：中文，详细解释设计决策和边界情况

### Vue
- 组件：`<script setup>` + Composition API，单文件组件
- 状态管理：组件自持 ref，不依赖全局 store
- IPC 封装：通过 `useTauri()` composable 获取 `invoke` / `listen`
- 样式：`<style scoped>` + CSS 变量 (`var(--panel)`, `var(--border)` 等)
- 组件命名：PascalCase (TitleBar, StatsCards, RpmChart)

### 关键路径文件热度
- `src/proxy.rs` (~440x 访问) — 代理核心，改动需谨慎
- `src/main.rs` (~171x) — 命令注册与配置管理
- `frontend/src/components/SettingsModal.vue` (~85x) — 设置界面

## Testing

- 后端：`cd src-tauri && cargo test` (68 个单元/集成测试)
- 重点测试模块：balance (余额解析)、proxy (路由/格式转换)、scheduler (并发调度)
- 前端无自动化测试，通过 `bun run dev` + Tauri 手动验证
