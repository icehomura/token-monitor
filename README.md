# Token Monitor

Tauri 2 桌面应用：Trae 兼容 API 代理（Rust 后端）+ RPM/TPM 实时统计。

## 下载安装

从 [GitHub Releases](https://github.com/icehomura/token-monitor/releases/latest) 下载对应平台的安装包：

| 平台 | 文件 | 说明 |
|------|------|------|
| Windows x64 | `*_x64-setup.exe` | NSIS 安装包，双击运行 |
| macOS Apple Silicon | `*_aarch64.dmg` | M1/M2/M3/M4 芯片 |
| macOS Intel | `*_x64.dmg` | Intel 芯片 |
| Linux x86_64 | `*_amd64.AppImage` / `*.deb` | AppImage 直接运行；deb 用 `dpkg -i` 安装 |
| Linux ARM64 | `*_aarch64.AppImage` / `*.deb` | 树莓派等 ARM 设备 |

### macOS 注意事项

macOS 会拦截未签名的应用，首次打开需要在终端执行：

```bash
xattr -cr /Applications/Token\ Monitor.app
```

或者：系统设置 → 隐私与安全性 → 点击「仍要打开」。

## 功能

- **代理服务**：程序启动即自动运行在 `http://127.0.0.1:8188`，支持三种 API 格式输入，统一转发到上游：
  - `POST /v1/chat/completions` — OpenAI 传统 Chat Completions → Responses 参数转换（tools、工具历史、reasoning_effort）
  - `POST /v1/responses` — OpenAI Responses 格式，原样透传
  - `POST /v1/messages` — Anthropic Messages 格式，双向转换（请求转 Responses，响应按 Anthropic SSE/JSON 返回，支持 text 与 tool_use 块）
  - SSE 流式转发、5xx 自动重试 3 次
- **系统托盘**：关闭窗口 = 隐藏到托盘；托盘左键单击切换窗口；右键菜单可退出
- **统计面板**：
  - RPM / TPM 两张 ECharts 图表
  - 按**整分钟**分桶对齐（不是相对时间），空分钟补零
  - 时间维度：近 5 分钟 / 10 分钟（默认）/ 30 分钟 / 1 小时 / 5 小时 / 今日
  - 每次请求后端 emit `stats-updated` 事件，前端立即刷新（另有 5s 兜底轮询）
  - 内置「发送测试请求」按钮，直接打代理验证链路并产生一条统计
- **AI 服务探针**：Toolbar 下方一行，按设定间隔探测上游可用性，显示延迟、成功率与最近 60 次历史
- **DeepSeek 余额**：标题栏显示账户余额（仅当上游为 DeepSeek 官方 `api.deepseek.com` 时可用；中转站上游会跳过并说明原因）

## 配置

配置**只从 `token-monitor.json` 读取**（不再支持环境变量）。

**配置文件位置按平台区分**（数据库 `token-monitor-data.db` 与 `crash.log` 同目录）：

| 平台 | 位置 | 原因 |
|---|---|---|
| Windows | exe 同目录 → 工作目录 | exe 目录是稳定文件夹 |
| macOS | `~/Library/Application Support/com.icehomura.token-monitor/` | exe 位于 `.app` 包内，DMG 升级会整包替换，写在包内必丢 |
| Linux AppImage | `.AppImage` 文件同目录 | AppImage 挂载点是只读临时目录，每次启动路径都变 |
| Linux deb | 二进制同目录 → 工作目录 | 未做特殊处理 |

> **macOS / AppImage 首次使用需手动放置配置**：程序不会自动迁移旧数据，请在上述目录新建 `token-monitor.json`（可参考 `token-monitor.example.json`）。配置也可全部在设置界面里改，程序会写回该目录。

```json
{
  "api_key": "sk-xxx",
  "model_override": "",
  "upstream_url": "https://api.deepseek.com/v1/responses",
  "port": 8188,
  "balance": { "enabled": false, "currency": "CNY", "interval_secs": 15 },
  "probe":   { "enabled": true,  "interval_secs": 15 }
}
```

| 字段 | 说明 |
|---|---|
| `api_key` | 上游密钥，**必填**，为空时代理返回配置错误 |
| `model_override` | 强制覆盖上游模型名，留空则不覆盖 |
| `upstream_url` | 上游 Responses API 全路径 |
| `port` | 服务端口，默认 8188 |
| `balance.enabled` | 是否开启余额查询，默认 `false` |
| `balance.currency` | 显示币种：`CNY` 或 `USD`，默认 `CNY` |
| `balance.interval_secs` | 余额查询间隔，范围 1~1800 秒，默认 15 |
| `probe.enabled` | 是否开启探针，默认 `true` |
| `probe.interval_secs` | 探针间隔，范围 1~1800 秒，默认 15 |

以上均可在设置界面修改，无需手工编辑 JSON。

> 余额查询仅在上游 host 为 `api.deepseek.com` 时发起。上游若是中转站，余额功能会显示「非官方上游不可用」而不是报错——中转站的密钥打官方余额接口没有意义。

## 开发 / 构建

```bash
# 开发调试
cargo run

# release 可执行文件（target/release/token-monitor.exe）
cargo build --release

# 打 NSIS 安装包需要 tauri-cli：
cargo install tauri-cli && cargo tauri build

# 重新生成图标（改 icons/icon.svg 后）
bash scripts/gen_icons.sh
```

## 结构

```
frontend/index.html   前端页面（ECharts 走 CDN）
frontend/main.js      前端逻辑
frontend/style.css    前端样式
src/main.rs           入口：托盘、关窗隐藏、配置加载、启动服务
src/proxy.rs          代理：三种格式转换 + SSE 流式 + 重试 + 统计上报
src/stats.rs          RPM/TPM 整分钟分桶
src/codeg.rs          Codeg 服务器连接与会话聚合
src/balance.rs        余额查询
src/probe.rs          AI 服务探针
icons/icon.svg        1024x1024 矢量原图
scripts/gen_icons.sh  SVG -> PNG(Chrome headless) -> 全套位图(Pillow)
```

## CI/CD

推送到 `main` 不会自动构建。打 tag 后自动构建全平台产物并创建 Release：

```bash
git tag v0.2.0
git push origin v0.2.0
```
