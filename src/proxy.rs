//! Trae 兼容代理：Chat Completions / Responses / Anthropic Messages 三种输入，
//! 统一转换为 Responses API 转发到上游；SSE 流式转发、5xx 重试。
use crate::stats;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::OnceLock;
use tauri::Emitter;

const MAX_RETRIES: usize = 3;

/// 上游 API 格式：决定代理如何转换请求和响应
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamFormat {
    /// OpenAI Responses API（默认）
    Responses,
    /// OpenAI Chat Completions
    ChatCompletions,
    /// Anthropic Messages
    Anthropic,
}

impl Default for UpstreamFormat {
    fn default() -> Self { Self::Responses }
}

impl UpstreamFormat {
    pub fn from_str(s: &str) -> Self {
        match s {
            "chat_completions" => Self::ChatCompletions,
            "anthropic" => Self::Anthropic,
            _ => Self::Responses,
        }
    }
}

/// 单次请求体上限。axum 对 Json 提取器默认只放行 2MB，长上下文请求会被本地
/// 直接 413 拦下（报错来自本进程，与上游无关）。这里放宽到 1GiB。
const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024 * 1024;

/// Auto-incrementing request ID counter, starting from 1000
static REQUEST_ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1000);

fn next_request_id() -> u64 {
    REQUEST_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub api_key: String,
    pub model_override: String,
    pub port: u16,
    /// 上游 Responses API 地址（可在设置面板修改）
    pub upstream_url: String,
    /// 最大并发数（可通过配置文件修改）
    pub max_concurrency: usize,
    /// 上游 API 格式：responses / chat_completions / anthropic
    pub upstream_format: UpstreamFormat,
}

static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();
static CONFIG: std::sync::RwLock<Option<ProxyConfig>> = std::sync::RwLock::new(None);
static SCHEDULER: OnceLock<crate::scheduler::Scheduler> = OnceLock::new();

/// 单个调度渠道的上游凭据。
///
/// 动态调度下每个渠道各有自己的地址 / 密钥 / 上游格式 / 模型覆盖，
/// 请求必须按调度结果取用对应渠道的这一组值，不能再用全局 `cfg()`
/// （全局仅保留端口，以及探针 / 余额所需的默认上游）。
#[derive(Debug, Clone)]
pub struct ChannelConfig {
    pub api_key: String,
    pub upstream_url: String,
    pub upstream_format: UpstreamFormat,
    pub model_override: String,
}

/// profile_id -> 渠道凭据，与调度器中的渠道一一对应。
static CHANNEL_CONFIGS: OnceLock<std::sync::RwLock<std::collections::HashMap<String, ChannelConfig>>> =
    OnceLock::new();

fn channel_configs() -> &'static std::sync::RwLock<std::collections::HashMap<String, ChannelConfig>> {
    CHANNEL_CONFIGS.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()))
}

/// 注册或更新渠道凭据（启动时与保存 profile 时调用）
pub fn set_channel_config(profile_id: &str, cfg: ChannelConfig) {
    channel_configs()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(profile_id.to_string(), cfg);
}

/// 移除渠道凭据（删除 profile 时调用）
pub fn remove_channel_config(profile_id: &str) {
    channel_configs()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(profile_id);
}

/// 按 profile_id 取渠道凭据；未注册时返回 None
pub fn channel_config(profile_id: &str) -> Option<ChannelConfig> {
    channel_configs()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(profile_id)
        .cloned()
}

/// 取渠道凭据；profile_id 为空或未注册时降级为全局配置
fn resolve_channel(profile_id: &str) -> ChannelConfig {
    if let Some(c) = channel_config(profile_id) {
        return c;
    }
    let g = cfg();
    ChannelConfig {
        api_key: g.api_key,
        upstream_url: g.upstream_url,
        upstream_format: g.upstream_format,
        model_override: g.model_override,
    }
}

struct ServerHandle {
    shutdown: tokio::sync::watch::Sender<bool>,
    _join: tauri::async_runtime::JoinHandle<()>,
}

static SERVER: std::sync::Mutex<Option<ServerHandle>> = std::sync::Mutex::new(None);

/// 最近一次启动/切换代理服务失败的原因；成功时清空。
///
/// release 构建带 `windows_subsystem = "windows"`，没有控制台，`eprintln!`
/// 无处输出。启动失败（如端口被占用）原本只写 stderr，用户看到的是
/// 「窗口正常打开、托盘正常、统计有曲线」——却没有任何监听，
/// 只能在客户端连不上时才发现。失败必须可观测，因此把原因留在进程内供前端读取。
static LAST_SERVER_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 流式读取的绝对超时：防止上游流一直不停导致并发槽位被永久占用
const STREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// 收尾帧（finish / [DONE]）的发送超时。
/// 客户端停止读取时通道会满，无界的 `send().await` 会让整个任务永久挂起。
const TAIL_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub fn init(handle: tauri::AppHandle, cfg: ProxyConfig) {
    let _ = APP_HANDLE.set(handle);
    *CONFIG.write().unwrap_or_else(|e| e.into_inner()) = Some(cfg);
    init_scheduler();
}

/// 初始化调度器单例（幂等）。
///
/// 与 `init` 分开是为了让调度器不依赖 AppHandle：
/// 渠道由 `main` 通过 `scheduler().upsert_channel(...)` 写入，
/// 请求路径读的是同一个实例。
pub fn init_scheduler() -> &'static crate::scheduler::Scheduler {
    SCHEDULER.get_or_init(crate::scheduler::Scheduler::new)
}

pub fn cfg() -> ProxyConfig {
    CONFIG
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .expect("proxy config not initialized")
}

/// 探针 / 余额使用的上游地址：取第一个已启用渠道。
pub fn upstream_url() -> String {
    probe_channel().upstream_url.trim().to_string()
}

/// 探针 / 余额使用的 API Key，口径同 `upstream_url()`
pub fn default_api_key() -> String {
    probe_channel().api_key
}

/// 探针 / 余额使用的渠道凭据：取首个启用渠道，无启用渠道时回退全局配置。
///
/// 动态调度下不存在单一「当前上游」，用首个启用渠道代表整体可用性；
/// 回退分支同时覆盖调度器尚未初始化的启动早期场景。
pub fn probe_channel() -> ChannelConfig {
    let from_scheduler = SCHEDULER
        .get()
        .and_then(|s| s.first_enabled_id())
        .and_then(|id| channel_config(&id));
    from_scheduler.unwrap_or_else(|| {
        let g = cfg();
        ChannelConfig {
            api_key: g.api_key,
            upstream_url: g.upstream_url,
            upstream_format: g.upstream_format,
            model_override: g.model_override,
        }
    })
}

/// 获取 AppHandle（供 main.rs 调用事件通知）
pub fn app_handle() -> Option<&'static tauri::AppHandle> {
    APP_HANDLE.get()
}

pub(crate) fn scheduler() -> Option<&'static crate::scheduler::Scheduler> {
    SCHEDULER.get()
}

/// 代理服务当前是否在监听
pub fn is_listening() -> bool {
    SERVER.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// 最近一次启动/切换失败的原因；None 表示最近一次是成功的。
/// 启动早期尚未尝试过也返回 None —— 前端据此区分「从未启动」与「启动失败」。
pub fn last_server_error() -> Option<String> {
    LAST_SERVER_ERROR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn set_last_server_error(msg: Option<String>) {
    *LAST_SERVER_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = msg;
}

/// 在新端口重新绑定并监听（停掉旧服务，如有）。
///
/// 顺序是「**先绑定新端口，再停旧服务**」：绑定失败（端口被占用等）时旧服务
/// 仍在正常运行，调用方无需回滚任何状态，配置也不会被改坏。若反过来先停后绑，
/// 一次失败的端口切换会让代理彻底掉线，而坏端口已经落盘——重启也起不来。
///
/// 成败都会记录到 `LAST_SERVER_ERROR`，供前端展示：release 构建没有控制台，
/// 失败若只写 stderr 就等于没有告知用户。
pub async fn restart_server(new_port: u16) -> Result<(), String> {
    let result = restart_server_inner(new_port).await;
    match &result {
        Ok(()) => {
            if last_server_error().is_some() {
                println!("[proxy] 代理服务已恢复监听（端口 {new_port}）");
            }
            set_last_server_error(None);
        }
        Err(e) => {
            eprintln!("[proxy] 代理服务启动失败：{e}");
            set_last_server_error(Some(e.clone()));
        }
    }
    // 主动通知前端：启动失败不会触发任何其它事件，前端无从刷新到这条状态。
    // 复用既有事件名，Toolbar 已经在监听它。
    if let Some(h) = APP_HANDLE.get() {
        let _ = h.emit("server-info-changed", ());
    }
    result
}

async fn restart_server_inner(new_port: u16) -> Result<(), String> {
    // 已经在目标端口上运行 → 直接返回，不做任何切换。
    // 强行重绑会因端口被自己占用而失败（旧监听套接字未必随优雅关闭立即释放），
    // 把一次「值没变」的保存变成一次掉线。
    let already_running = {
        let running = SERVER.lock().unwrap_or_else(|e| e.into_inner()).is_some();
        let current = CONFIG
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|c| c.port);
        running && current == Some(new_port)
    };
    if already_running {
        return Ok(());
    }

    // 1. 先绑定新端口：失败时旧服务不受影响，配置保持原样
    let app = build_router();
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], new_port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("端口 {} 监听失败：{e}", new_port))?;

    // 2. 绑定成功后再停旧实例，等待优雅退出释放端口
    //    先把锁作用域结束，避免 MutexGuard 跨 await 导致 future 非 Send
    let existing = SERVER.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(handle) = existing {
        let _ = handle.shutdown.send(true);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // 3. 更新配置并启动
    {
        let mut c = CONFIG.write().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = c.as_mut() {
            c.port = new_port;
        }
    }
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    let join = tauri::async_runtime::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.changed().await;
            })
            .await
            .ok();
    });
    *SERVER.lock().unwrap_or_else(|e| e.into_inner()) = Some(ServerHandle { shutdown: tx, _join: join });
    println!("proxy server listening on http://{addr}/v1 (chat/completions | responses | messages)");
    Ok(())
}

// ---------- 参数转换：Chat Completions -> Responses API ----------

fn text_part(role: &str, text: &str) -> Value {
    json!({
        "type": if role == "assistant" { "output_text" } else { "input_text" },
        "text": text,
    })
}

/// Chat 格式 {"type":"function","function":{...}} -> Responses 格式 {"type":"function","name":...}
fn convert_tools(tools: Option<&Value>) -> Option<Value> {
    let arr = tools?.as_array()?;
    let out: Vec<Value> = arr
        .iter()
        .filter_map(|t| {
            if t.get("type")?.as_str()? == "function" {
                if let Some(fn_obj) = t.get("function") {
                    return Some(json!({
                        "type": "function",
                        "name": fn_obj.get("name")?,
                        "description": fn_obj.get("description"),
                        "parameters": fn_obj.get("parameters"),
                    }));
                }
            }
            Some(t.clone())
        })
        .filter(|t| t.get("name").map(|n| n.is_string()).unwrap_or(false))
        .collect();
    if out.is_empty() { None } else { Some(Value::Array(out)) }
}

/// 把 chat 消息历史转换为 Responses API 的 input 项，正确处理工具调用与工具结果。
fn convert_messages(messages: &Value) -> Vec<Value> {
    let mut items = Vec::new();
    let Some(arr) = messages.as_array() else { return items };

    for m in arr {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let content = m.get("content");

        // 工具结果 -> function_call_output
        if role == "tool" {
            let output = match content {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                // 与参考实现一致：content 缺失时 json.dumps(None) = "null"
                None => "null".to_string(),
            };
            items.push(json!({
                "type": "function_call_output",
                "call_id": m.get("tool_call_id").and_then(|c| c.as_str()).unwrap_or(""),
                "output": output,
            }));
            continue;
        }

        // assistant 带工具调用 -> 文本消息(可选) + function_call 项
        if role == "assistant" && m.get("tool_calls").is_some() {
            let parts: Vec<Value> = match content {
                Some(Value::String(s)) if !s.is_empty() => vec![text_part(role, s)],
                Some(Value::Array(list)) => list
                    .iter()
                    .filter_map(|p| {
                        let ty = p.get("type").and_then(|t| t.as_str())?;
                        matches!(ty, "text" | "input_text" | "output_text")
                            .then(|| text_part(role, p.get("text").and_then(|t| t.as_str()).unwrap_or("")))
                    })
                    .collect(),
                _ => vec![],
            };
            if !parts.is_empty() {
                items.push(json!({"type": "message", "role": "assistant", "content": parts}));
            }
            for tc in m["tool_calls"].as_array().unwrap_or(&vec![]) {
                let f = tc.get("function").cloned().unwrap_or(json!({}));
                items.push(json!({
                    "type": "function_call",
                    "call_id": tc.get("id").and_then(|i| i.as_str()).unwrap_or(""),
                    "name": f.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                    "arguments": f.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}"),
                }));
            }
            continue;
        }

        // 普通 system/user/assistant 消息
        let converted = match content {
            Some(Value::Array(list)) => {
                let parts: Vec<Value> = list
                    .iter()
                    .filter_map(|p| {
                        let ty = p.get("type").and_then(|t| t.as_str())?;
                        match ty {
                            "text" | "input_text" | "output_text" => Some(text_part(
                                role,
                                p.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                            )),
                            "image_url" => Some(json!({
                                "type": "image_url",
                                "image_url": {"url": p.pointer("/image_url/url").and_then(|u| u.as_str()).unwrap_or("")}
                            })),
                            _ => None,
                        }
                    })
                    .collect();
                json!(parts)
            }
            Some(v) => v.clone(),
            None => Value::Null,
        };
        items.push(json!({"type": "message", "role": role, "content": converted}));
    }
    items
}

fn chat_to_responses_payload(chat_body: &Value, stream: bool, model_override: &str) -> Value {
    let mut payload = json!({
        "input": convert_messages(chat_body.get("messages").unwrap_or(&Value::Null)),
        "stream": stream,
    });
    // 与参考实现“剔除 None 字段”一致：model 缺失时省略而不是发 null
    if let Some(m) = chat_body.get("model") {
        payload["model"] = m.clone();
    }
    if let Some(t) = convert_tools(chat_body.get("tools")) {
        payload["tools"] = t;
    }
    for (from, to) in [("temperature", "temperature"), ("top_p", "top_p")] {
        if let Some(v) = chat_body.get(from) {
            payload[to] = v.clone();
        }
    }
    if let Some(mt) = chat_body
        .get("max_tokens")
        .or_else(|| chat_body.get("max_completion_tokens"))
    {
        payload["max_output_tokens"] = mt.clone();
    }
    // reasoning_effort：Trae 发 none 时映射为 high，其余透传
    match chat_body.get("reasoning_effort").and_then(|r| r.as_str()) {
        Some("none") => payload["reasoning_effort"] = json!("high"),
        Some(other) => payload["reasoning_effort"] = json!(other),
        None => {}
    }
    if !model_override.is_empty() {
        payload["model"] = json!(model_override);
    }
    payload
}

// ---------- 参数转换：Anthropic Messages -> Responses API ----------

/// Anthropic 的 content 兼容字符串与块数组两种形态，统一抽出纯文本
fn anthropic_content_to_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Anthropic 工具定义 {"name","description","input_schema"} -> Responses function 工具
fn anthropic_tools(tools: Option<&Value>) -> Option<Value> {
    let arr = tools?.as_array()?;
    let out: Vec<Value> = arr
        .iter()
        .filter_map(|t| {
            Some(json!({
                "type": "function",
                "name": t.get("name")?,
                "description": t.get("description"),
                "parameters": t.get("input_schema"),
            }))
        })
        .filter(|t| t.get("name").map(|n| n.is_string()).unwrap_or(false))
        .collect();
    if out.is_empty() { None } else { Some(Value::Array(out)) }
}

fn anthropic_message_item(role: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "role": role,
        "content": [text_part(role, text)],
    })
}

/// 把 Anthropic /v1/messages 请求体转换为 Responses API 载荷：
/// - system（字符串或块数组）-> system 消息
/// - text 块 -> input_text / output_text
/// - image 块（base64 源）-> input_image（data URL）
/// - assistant 的 tool_use 块 -> function_call 项
/// - user 的 tool_result 块 -> function_call_output 项
fn anthropic_to_responses_payload(body: &Value, stream: bool, model_override: &str) -> Value {
    let mut items: Vec<Value> = Vec::new();

    if let Some(sys) = body.get("system") {
        let text = anthropic_content_to_text(Some(sys));
        if !text.is_empty() {
            items.push(anthropic_message_item("system", &text));
        }
    }

    if let Some(arr) = body.get("messages").and_then(|m| m.as_array()) {
        for m in arr {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            match m.get("content") {
                // 纯字符串内容
                Some(Value::String(s)) => {
                    if !s.is_empty() {
                        items.push(anthropic_message_item(role, s));
                    }
                }
                // 块数组
                Some(Value::Array(blocks)) => {
                    for b in blocks {
                        match b.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                            "text" => {
                                let text = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                if !text.is_empty() {
                                    items.push(anthropic_message_item(role, text));
                                }
                            }
                            "image" => {
                                let media = b
                                    .pointer("/source/media_type")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("image/png");
                                let data =
                                    b.pointer("/source/data").and_then(|d| d.as_str()).unwrap_or("");
                                if !data.is_empty() {
                                    items.push(json!({
                                        "type": "message",
                                        "role": role,
                                        "content": [{
                                            "type": "input_image",
                                            "image_url": format!("data:{media};base64,{data}"),
                                        }],
                                    }));
                                }
                            }
                            "tool_use" => {
                                items.push(json!({
                                    "type": "function_call",
                                    "call_id": b.get("id").and_then(|i| i.as_str()).unwrap_or(""),
                                    "name": b.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                                    "arguments": serde_json::to_string(
                                        b.get("input").unwrap_or(&json!({})),
                                    )
                                    .unwrap_or_else(|_| "{}".into()),
                                }));
                            }
                            "tool_result" => {
                                let output = match b.get("content") {
                                    Some(Value::String(s)) => s.clone(),
                                    Some(v) => v.to_string(),
                                    None => "null".to_string(),
                                };
                                items.push(json!({
                                    "type": "function_call_output",
                                    "call_id": b.get("tool_use_id").and_then(|i| i.as_str()).unwrap_or(""),
                                    "output": output,
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let mut payload = json!({ "input": items, "stream": stream });
    if let Some(m) = body.get("model") {
        payload["model"] = m.clone();
    }
    if let Some(t) = anthropic_tools(body.get("tools")) {
        payload["tools"] = t;
    }
    for k in ["temperature", "top_p"] {
        if let Some(v) = body.get(k) {
            payload[k] = v.clone();
        }
    }
    if let Some(mt) = body.get("max_tokens") {
        payload["max_output_tokens"] = mt.clone();
    }
    if !model_override.is_empty() {
        payload["model"] = json!(model_override);
    }
    payload
}

// ---------- 参数转换：Responses API -> Chat Completions ----------

/// Responses API input items -> Chat messages 数组
fn responses_items_to_chat_messages(items: &[Value]) -> Vec<Value> {
    let mut messages = Vec::new();
    for item in items {
        let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "message" => {
                let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                let content = item.get("content");
                let text = match content {
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter_map(|p| {
                            let pty = p.get("type").and_then(|t| t.as_str())?;
                            matches!(pty, "input_text" | "output_text" | "text")
                                .then(|| p.get("text").and_then(|t| t.as_str()).unwrap_or(""))
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                    Some(Value::String(s)) => s.clone(),
                    _ => continue,
                };
                messages.push(json!({"role": role, "content": text}));
            }
            "function_call" => {
                messages.push(json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "id": item.get("call_id").and_then(|c| c.as_str()).unwrap_or(""),
                        "type": "function",
                        "function": {
                            "name": item.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                            "arguments": item.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}"),
                        },
                    }],
                }));
            }
            "function_call_output" => {
                let output = item.get("output").and_then(|o| o.as_str()).unwrap_or("");
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": item.get("call_id").and_then(|c| c.as_str()).unwrap_or(""),
                    "content": output,
                }));
            }
            _ => {}
        }
    }
    messages
}

/// Responses tools -> Chat Completions tools
fn responses_tools_to_chat(tools: Option<&Value>) -> Option<Value> {
    let arr = tools?.as_array()?;
    let out: Vec<Value> = arr
        .iter()
        .filter_map(|t| {
            if t.get("type")?.as_str()? == "function" {
                return Some(json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name")?,
                        "description": t.get("description"),
                        "parameters": t.get("parameters"),
                    },
                }));
            }
            None
        })
        .filter(|t| t.pointer("/function/name").and_then(|n| n.as_str()).is_some())
        .collect();
    if out.is_empty() { None } else { Some(Value::Array(out)) }
}

/// Responses API -> Chat Completions payload
fn responses_to_chat_payload(body: &Value, stream: bool, model_override: &str) -> Value {
    let messages = match body.get("input").and_then(|i| i.as_array()) {
        Some(items) => responses_items_to_chat_messages(items),
        None => vec![],
    };
    let mut payload = json!({
        "messages": messages,
        "stream": stream,
    });
    if let Some(m) = body.get("model") {
        payload["model"] = m.clone();
    }
    if let Some(t) = responses_tools_to_chat(body.get("tools")) {
        payload["tools"] = t;
    }
    for (from, to) in [("temperature", "temperature"), ("top_p", "top_p")] {
        if let Some(v) = body.get(from) {
            payload[to] = v.clone();
        }
    }
    if let Some(mt) = body.get("max_output_tokens") {
        payload["max_tokens"] = mt.clone();
    }
    if !model_override.is_empty() {
        payload["model"] = json!(model_override);
    }
    payload
}

// ---------- 参数转换：Chat Completions -> Anthropic Messages ----------

/// Chat tools -> Anthropic tools
fn chat_tools_to_anthropic(tools: Option<&Value>) -> Option<Value> {
    let arr = tools?.as_array()?;
    let out: Vec<Value> = arr
        .iter()
        .filter_map(|t| {
            let f = t.get("function")?;
            Some(json!({
                "name": f.get("name")?,
                "description": f.get("description"),
                "input_schema": f.get("parameters"),
            }))
        })
        .filter(|t| t.get("name").and_then(|n| n.as_str()).is_some())
        .collect();
    if out.is_empty() { None } else { Some(Value::Array(out)) }
}

/// Chat Completions -> Anthropic Messages payload
fn chat_to_anthropic_payload(body: &Value, stream: bool, model_override: &str) -> Value {
    let mut system_text = String::new();
    let mut messages = Vec::new();

    if let Some(arr) = body.get("messages").and_then(|m| m.as_array()) {
        for m in arr {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
            let content = m.get("content");
            // system 消息提取到顶级 system 字段
            if role == "system" {
                let text = match content {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => continue,
                };
                if !system_text.is_empty() { system_text.push('\n'); }
                system_text.push_str(&text);
                continue;
            }
            // assistant 带 tool_calls
            if role == "assistant" && m.get("tool_calls").is_some() {
                let mut blocks = Vec::new();
                // 文本内容
                let text = match content {
                    Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                    _ => None,
                };
                if let Some(t) = text {
                    blocks.push(json!({"type": "text", "text": t}));
                }
                // tool_use blocks
                for tc in m["tool_calls"].as_array().unwrap_or(&vec![]) {
                    let input: Value = tc.pointer("/function/arguments")
                        .and_then(|a| a.as_str())
                        .and_then(|a| serde_json::from_str(a).ok())
                        .unwrap_or(json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.get("id").and_then(|i| i.as_str()).unwrap_or(""),
                        "name": tc.pointer("/function/name").and_then(|n| n.as_str()).unwrap_or(""),
                        "input": input,
                    }));
                }
                if blocks.is_empty() {
                    blocks.push(json!({"type": "text", "text": ""}));
                }
                messages.push(json!({"role": "assistant", "content": blocks}));
                continue;
            }
            // tool role -> user message with tool_result blocks
            if role == "tool" {
                let output = match content {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => "null".to_string(),
                };
                // 合并连续 tool 消息到同一个 user message
                let tool_result_block = json!({
                    "type": "tool_result",
                    "tool_use_id": m.get("tool_call_id").and_then(|c| c.as_str()).unwrap_or(""),
                    "content": output,
                });
                if let Some(last) = messages.last_mut() {
                    if last.get("role").and_then(|r| r.as_str()) == Some("user") {
                        if let Some(Value::Array(blocks)) = last.get_mut("content") {
                            blocks.push(tool_result_block);
                            continue;
                        }
                    }
                }
                messages.push(json!({"role": "user", "content": [tool_result_block]}));
                continue;
            }
            // 普通 user/assistant 消息
            let text = match content {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => continue,
            };
            if !text.is_empty() {
                messages.push(json!({"role": role, "content": text}));
            }
        }
    }

    let mut payload = json!({
        "messages": messages,
        "stream": stream,
    });
    if let Some(m) = body.get("model") {
        payload["model"] = m.clone();
    }
    if !system_text.is_empty() {
        payload["system"] = json!(system_text);
    }
    if let Some(t) = chat_tools_to_anthropic(body.get("tools")) {
        payload["tools"] = t;
    }
    for k in ["temperature", "top_p"] {
        if let Some(v) = body.get(k) {
            payload[k] = v.clone();
        }
    }
    if let Some(mt) = body.get("max_tokens").or_else(|| body.get("max_completion_tokens")) {
        payload["max_tokens"] = mt.clone();
    }
    if !model_override.is_empty() {
        payload["model"] = json!(model_override);
    }
    payload
}

// ---------- 参数转换：Anthropic Messages -> Chat Completions ----------

/// Anthropic tools -> Chat Completions tools
fn anthropic_tools_to_chat(tools: Option<&Value>) -> Option<Value> {
    let arr = tools?.as_array()?;
    let out: Vec<Value> = arr
        .iter()
        .filter_map(|t| {
            Some(json!({
                "type": "function",
                "function": {
                    "name": t.get("name")?,
                    "description": t.get("description"),
                    "parameters": t.get("input_schema"),
                },
            }))
        })
        .filter(|t| t.pointer("/function/name").and_then(|n| n.as_str()).is_some())
        .collect();
    if out.is_empty() { None } else { Some(Value::Array(out)) }
}

/// Anthropic Messages -> Chat Completions payload
fn anthropic_to_chat_payload(body: &Value, stream: bool, model_override: &str) -> Value {
    let mut messages = Vec::new();

    // system 顶级字段 -> system message
    if let Some(sys) = body.get("system") {
        let text = match sys {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    if let Some(arr) = body.get("messages").and_then(|m| m.as_array()) {
        for m in arr {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            match m.get("content") {
                Some(Value::String(s)) => {
                    messages.push(json!({"role": role, "content": s}));
                }
                Some(Value::Array(blocks)) => {
                    let mut text_parts = Vec::new();
                    let mut tool_calls = Vec::new();
                    let mut tool_results = Vec::new();
                    for b in blocks {
                        match b.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                            "text" => {
                                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                    if !t.is_empty() { text_parts.push(t.to_string()); }
                                }
                            }
                            "tool_use" => {
                                tool_calls.push(json!({
                                    "id": b.get("id").and_then(|i| i.as_str()).unwrap_or(""),
                                    "type": "function",
                                    "function": {
                                        "name": b.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                                        "arguments": serde_json::to_string(b.get("input").unwrap_or(&json!({}))).unwrap_or_else(|_| "{}".into()),
                                    },
                                }));
                            }
                            "tool_result" => {
                                let output = match b.get("content") {
                                    Some(Value::String(s)) => s.clone(),
                                    Some(v) => v.to_string(),
                                    None => "null".to_string(),
                                };
                                tool_results.push(json!({
                                    "role": "tool",
                                    "tool_call_id": b.get("tool_use_id").and_then(|i| i.as_str()).unwrap_or(""),
                                    "content": output,
                                }));
                            }
                            _ => {}
                        }
                    }
                    // assistant 消息: text + tool_calls
                    if role == "assistant" {
                        let mut msg = json!({"role": "assistant"});
                        if !tool_calls.is_empty() {
                            let mut content = text_parts.join("\n");
                            if content.is_empty() { content = "".into(); }
                            msg["content"] = json!(content);
                            msg["tool_calls"] = Value::Array(tool_calls);
                        } else {
                            msg["content"] = json!(text_parts.join("\n"));
                        }
                        messages.push(msg);
                    }
                    // tool_results 作为独立的 tool role 消息
                    for tr in tool_results {
                        messages.push(tr);
                    }
                    // user 消息只有文本
                    if role == "user" && !text_parts.is_empty() {
                        messages.push(json!({"role": "user", "content": text_parts.join("\n")}));
                    }
                }
                _ => {}
            }
        }
    }

    let mut payload = json!({
        "messages": messages,
        "stream": stream,
    });
    if let Some(m) = body.get("model") {
        payload["model"] = m.clone();
    }
    if let Some(t) = anthropic_tools_to_chat(body.get("tools")) {
        payload["tools"] = t;
    }
    for k in ["temperature", "top_p"] {
        if let Some(v) = body.get(k) {
            payload[k] = v.clone();
        }
    }
    if let Some(mt) = body.get("max_tokens") {
        payload["max_tokens"] = mt.clone();
    }
    if !model_override.is_empty() {
        payload["model"] = json!(model_override);
    }
    payload
}

// ---------- 参数转换：Responses API -> Anthropic Messages ----------

/// Responses API -> Anthropic Messages payload
fn responses_to_anthropic_payload(body: &Value, stream: bool, model_override: &str) -> Value {
    let mut system_text = String::new();
    let mut messages = Vec::new();

    if let Some(items) = body.get("input").and_then(|i| i.as_array()) {
        for item in items {
            let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match ty {
                "message" => {
                    let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                    let content = item.get("content");
                    let text = match content {
                        Some(Value::Array(parts)) => parts
                            .iter()
                            .filter_map(|p| {
                                let pty = p.get("type").and_then(|t| t.as_str())?;
                                matches!(pty, "input_text" | "output_text" | "text")
                                    .then(|| p.get("text").and_then(|t| t.as_str()).unwrap_or(""))
                            })
                            .collect::<Vec<_>>()
                            .join(""),
                        Some(Value::String(s)) => s.clone(),
                        _ => continue,
                    };
                    if role == "system" {
                        if !system_text.is_empty() { system_text.push('\n'); }
                        system_text.push_str(&text);
                    } else {
                        messages.push(json!({"role": role, "content": text}));
                    }
                }
                "function_call" => {
                    let input: Value = item.get("arguments")
                        .and_then(|a| a.as_str())
                        .and_then(|a| serde_json::from_str(a).ok())
                        .unwrap_or(json!({}));
                    messages.push(json!({
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use",
                            "id": item.get("call_id").and_then(|c| c.as_str()).unwrap_or(""),
                            "name": item.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                            "input": input,
                        }],
                    }));
                }
                "function_call_output" => {
                    let output = item.get("output").and_then(|o| o.as_str()).unwrap_or("");
                    messages.push(json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": item.get("call_id").and_then(|c| c.as_str()).unwrap_or(""),
                            "content": output,
                        }],
                    }));
                }
                _ => {}
            }
        }
    }

    let mut payload = json!({
        "messages": messages,
        "stream": stream,
    });
    if let Some(m) = body.get("model") {
        payload["model"] = m.clone();
    }
    if !system_text.is_empty() {
        payload["system"] = json!(system_text);
    }
    // Responses tools -> Anthropic tools
    if let Some(arr) = body.get("tools").and_then(|t| t.as_array()) {
        let out: Vec<Value> = arr
            .iter()
            .filter_map(|t| {
                if t.get("type")?.as_str()? != "function" { return None; }
                Some(json!({
                    "name": t.get("name")?,
                    "description": t.get("description"),
                    "input_schema": t.get("parameters"),
                }))
            })
            .filter(|t| t.get("name").and_then(|n| n.as_str()).is_some())
            .collect();
        if !out.is_empty() { payload["tools"] = Value::Array(out); }
    }
    for k in ["temperature", "top_p"] {
        if let Some(v) = body.get(k) {
            payload[k] = v.clone();
        }
    }
    if let Some(mt) = body.get("max_output_tokens") {
        payload["max_tokens"] = mt.clone();
    }
    if !model_override.is_empty() {
        payload["model"] = json!(model_override);
    }
    payload
}

// ---------- SSE 解析与流式转换（上游 Responses -> Chat Completions）----------

fn make_chunk(model: &str, delta: Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": format!("chatcmpl-{}", next_request_id()),
        "object": "chat.completion.chunk",
        "created": 1_700_000_000u64,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]
    })
}

/// 消费上游 Responses API 的字节流，把转换后的 Chat Completions 流块直接写入 tx。
/// 返回 (输出字符数估计, 是否用到工具调用)。
/// on_tokens: 可选回调，每处理一个 SSE 事件时调用，参数为当前累计输出字符数。
async fn convert_stream(
    upstream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    model: String,
    tx: &tokio::sync::mpsc::Sender<Result<Vec<u8>, std::io::Error>>,
    on_tokens: Option<Box<dyn Fn(u64) + Send + 'static>>,
) -> (u64, bool, stats::TokenCounts) {
    let mut stream = upstream;
    let mut buf: Vec<u8> = Vec::new();
    let mut tool_buffers: std::collections::HashMap<i64, (String, String, String)> =
        std::collections::HashMap::new(); // output_index -> (call_id, name, arguments)
    let mut used_tool_calls = false;
    let mut out_chars: u64 = 0;
    let mut usage = stats::TokenCounts { input: 0, output: 0, cached: 0 };

    'outer: while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        buf.extend_from_slice(&bytes);

        while let Some((end, delim)) = find_sse_boundary(&buf) {
            let event_bytes: Vec<u8> = buf.drain(..end + delim).collect();
            // 提取最后一个 data: 行
            let text = String::from_utf8_lossy(&event_bytes);
            let data_line = text
                .lines()
                .rev()
                .find(|l| l.trim_start().starts_with("data:"));
            let Some(data_line) = data_line else { continue };
            let payload = data_line.trim_start()["data:".len()..].trim();
            if payload == "[DONE]" {
                break 'outer;
            }
            let Ok(evt) = serde_json::from_str::<Value>(payload) else { continue };

            let evt_type = evt.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let output_index = evt.get("output_index").and_then(|i| i.as_i64()).unwrap_or(0);

            // 每处理一个事件就通知前端刷新
            if let Some(ref cb) = on_tokens {
                cb(out_chars);
            }

            match evt_type {
                "error" | "response.failed" => {
                    eprintln!("!!! upstream error event: {evt}");
                    let _ = tx.send(Ok(sse_frame(&make_chunk(&model, json!({}), Some("stop"))))).await;
                    return (out_chars, used_tool_calls, usage);
                }
                "response.output_text.delta" => {
                    if let Some(delta) = evt.get("delta").and_then(|d| d.as_str()) {
                        out_chars += delta.chars().count() as u64;
                        let _ = tx
                            .send(Ok(sse_frame(&make_chunk(
                                &model,
                                json!({"content": delta}),
                                None,
                            ))))
                            .await;
                    }
                }
                "response.output_item.added" => {
                    let item = evt.get("item").cloned().unwrap_or(json!({}));
                    if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                        used_tool_calls = true;
                        let call_id = item.get("call_id").and_then(|c| c.as_str()).unwrap_or("").to_string();
                        let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                        tool_buffers.insert(output_index, (call_id.clone(), name.clone(), String::new()));
                        let _ = tx
                            .send(Ok(sse_frame(&make_chunk(
                                &model,
                                json!({"tool_calls": [{
                                    "index": output_index,
                                    "id": call_id,
                                    "function": {"name": name, "arguments": ""}
                                }]}),
                                None,
                            ))))
                            .await;
                    }
                }
                "response.function_call_arguments.delta" => {
                    if let Some((call_id, name, args)) = tool_buffers.get_mut(&output_index) {
                        let delta_args =
                            evt.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                        args.push_str(delta_args);
                        out_chars += delta_args.chars().count() as u64;
                        let id = call_id.clone();
                        let nm = name.clone();
                        let _ = tx
                            .send(Ok(sse_frame(&make_chunk(
                                &model,
                                json!({"tool_calls": [{
                                    "index": output_index,
                                    "id": id,
                                    "function": {"name": nm, "arguments": delta_args}
                                }]}),
                                None,
                            ))))
                            .await;
                    }
                }
                "response.completed" => {
                    if let Some(u) = evt.pointer("/response/usage") {
                        usage.input = u.pointer("/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        usage.output = u.pointer("/output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        usage.cached = u.pointer("/cached_tokens").and_then(|t| t.as_u64())
                            .or_else(|| u.pointer("/input_tokens_details/cached_tokens").and_then(|t| t.as_u64()))
                            .unwrap_or(0);
                    }
                }
                _ => {}
            }
        }
    }

    // 如果上游没有返回 usage，用字符数估算 output
    if usage.output == 0 {
        usage.output = out_chars.div_ceil(3);
    }
    (out_chars, used_tool_calls, usage)
}

/// 若切片以行结束符开头，返回其长度。
///
/// SSE 规范允许 `\n`、`\r\n`、`\r` 三种行结束符。
fn line_ending_len(s: &[u8]) -> Option<usize> {
    if s.starts_with(b"\r\n") {
        Some(2)
    } else if s.starts_with(b"\n") || s.starts_with(b"\r") {
        Some(1)
    } else {
        None
    }
}

/// 找到一个完整 SSE 帧的边界，返回 `(分隔符起点, 分隔符长度)`。
/// 调用方据此 `drain(..start + len)` 取走整帧（含空行）。
///
/// 曾只匹配 `\n\n`，于是使用 `\r\n\r\n` 分帧的上游（或中间的反向代理/CDN）
/// 会让帧边界**永远找不到**：转换路径整段丢弃内容（客户端只收到骨架帧，
/// HTTP 200 却无正文），直通路径则 token 统计恒为 0，同时缓冲区按整条流的
/// 长度增长。这里按规范识别全部三种行结束符及其混合形式。
fn find_sse_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0usize;
    while i < buf.len() {
        if line_ending_len(&buf[i..]).is_none() {
            i += 1;
            continue;
        }
        // 贪婪吞掉连续的行结束符：空行（≥2 个行结束符）即帧边界
        let start = i;
        let mut j = i;
        let mut endings = 0usize;
        while let Some(l) = line_ending_len(&buf[j..]) {
            j += l;
            endings += 1;
        }
        if endings >= 2 {
            return Some((start, j - start));
        }
        i = j;
    }
    None
}

fn sse_frame(v: &Value) -> Vec<u8> {
    let mut b = b"data: ".to_vec();
    b.extend_from_slice(serde_json::to_string(v).unwrap_or_default().as_bytes());
    b.extend_from_slice(b"\n\n");
    b
}

// ---------- SSE 解析与流式转换（上游 Responses -> Anthropic Messages）----------

fn anthropic_sse(event: &str, v: &Value) -> Vec<u8> {
    let mut b = format!("event: {event}\ndata: ").into_bytes();
    b.extend_from_slice(serde_json::to_string(v).unwrap_or_default().as_bytes());
    b.extend_from_slice(b"\n\n");
    b
}

/// 消费上游 Responses API 字节流，转换为 Anthropic Messages SSE 事件写入 tx。
/// 返回输出字符数估计。事件序列遵循 Anthropic 规范：
/// message_start -> (content_block_start -> content_block_delta* -> content_block_stop)*
/// -> message_delta(stop_reason/usage) -> message_stop
#[allow(unused_assignments)] // 宏展开后最后一次 started = true 不再被读，属预期
async fn convert_stream_anthropic(
    upstream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    model: String,
    tx: &tokio::sync::mpsc::Sender<Result<Vec<u8>, std::io::Error>>,
    on_tokens: Option<Box<dyn Fn(u64) + Send + 'static>>,
) -> (u64, stats::TokenCounts) {
    let mut stream = upstream;
    let mut buf: Vec<u8> = Vec::new();
    let mut out_chars: u64 = 0;
    let mut usage = stats::TokenCounts { input: 0, output: 0, cached: 0 };

    let mut started = false; // 已发送 message_start
    let mut text_index: Option<usize> = None; // 文本块的 block index（懒打开）
    let mut next_index = 0usize; // 下一个可分配的 block index
    let mut open_blocks: Vec<usize> = Vec::new(); // 已打开、尚未关闭的块（含文本与工具）
    let mut tool_blocks: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    let mut used_tool_calls = false;

    macro_rules! ensure_started {
        () => {
            if !started {
                started = true;
                let _ = tx
                    .send(Ok(anthropic_sse(
                        "message_start",
                        &json!({
                            "type": "message_start",
                            "message": {
                                "id": format!("msg_{}", next_request_id()),
                                "type": "message",
                                "role": "assistant",
                                "model": model,
                                "content": [],
                                "stop_reason": Value::Null,
                                "stop_sequence": Value::Null,
                                "usage": {"input_tokens": 0, "output_tokens": 0},
                            },
                        }),
                    )))
                    .await;
            }
        };
    }

    'outer: while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        buf.extend_from_slice(&bytes);

        while let Some((end, delim)) = find_sse_boundary(&buf) {
            let event_bytes: Vec<u8> = buf.drain(..end + delim).collect();
            let text = String::from_utf8_lossy(&event_bytes);
            let data_line = text
                .lines()
                .rev()
                .find(|l| l.trim_start().starts_with("data:"));
            let Some(data_line) = data_line else { continue };
            let payload = data_line.trim_start()["data:".len()..].trim();
            if payload == "[DONE]" {
                break 'outer;
            }
            let Ok(evt) = serde_json::from_str::<Value>(payload) else { continue };

            let evt_type = evt.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let output_index = evt.get("output_index").and_then(|i| i.as_i64()).unwrap_or(0);

            // 每处理一个事件就通知前端刷新
            if let Some(ref cb) = on_tokens {
                cb(out_chars);
            }

            match evt_type {
                "error" | "response.failed" => {
                    eprintln!("!!! upstream error event: {evt}");
                    ensure_started!();
                    let msg = evt
                        .pointer("/error/message")
                        .or_else(|| evt.pointer("/response/error/message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("upstream error")
                        .to_string();
                    let _ = tx
                        .send(Ok(anthropic_sse(
                            "error",
                            &json!({"type": "error", "error": {"type": "api_error", "message": msg}}),
                        )))
                        .await;
                    let _ = tx.send(Ok(anthropic_sse("message_stop", &json!({"type": "message_stop"})))).await;
                    return (out_chars, usage);
                }
                "response.output_text.delta" => {
                    if let Some(delta) = evt.get("delta").and_then(|d| d.as_str()) {
                        ensure_started!();
                        let ti = match text_index {
                            Some(i) => i,
                            None => {
                                let i = next_index;
                                next_index += 1;
                                text_index = Some(i);
                                open_blocks.push(i);
                                let _ = tx
                                    .send(Ok(anthropic_sse(
                                        "content_block_start",
                                        &json!({
                                            "type": "content_block_start",
                                            "index": i,
                                            "content_block": {"type": "text", "text": ""},
                                        }),
                                    )))
                                    .await;
                                i
                            }
                        };
                        out_chars += delta.chars().count() as u64;
                        let _ = tx
                            .send(Ok(anthropic_sse(
                                "content_block_delta",
                                &json!({
                                    "type": "content_block_delta",
                                    "index": ti,
                                    "delta": {"type": "text_delta", "text": delta},
                                }),
                            )))
                            .await;
                    }
                }
                "response.output_item.added" => {
                    let item = evt.get("item").cloned().unwrap_or(json!({}));
                    if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                        ensure_started!();
                        used_tool_calls = true;
                        let bi = next_index;
                        next_index += 1;
                        tool_blocks.insert(output_index, bi);
                        open_blocks.push(bi);
                        let _ = tx
                            .send(Ok(anthropic_sse(
                                "content_block_start",
                                &json!({
                                    "type": "content_block_start",
                                    "index": bi,
                                    "content_block": {
                                        "type": "tool_use",
                                        "id": item.get("call_id").and_then(|c| c.as_str()).unwrap_or(""),
                                        "name": item.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                                        "input": {},
                                    },
                                }),
                            )))
                            .await;
                    }
                }
                "response.function_call_arguments.delta" => {
                    if let Some(&bi) = tool_blocks.get(&output_index) {
                        let delta_args = evt.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                        out_chars += delta_args.chars().count() as u64;
                        let _ = tx
                            .send(Ok(anthropic_sse(
                                "content_block_delta",
                                &json!({
                                    "type": "content_block_delta",
                                    "index": bi,
                                    "delta": {"type": "input_json_delta", "partial_json": delta_args},
                                }),
                            )))
                            .await;
                    }
                }
                "response.completed" => {
                    if let Some(u) = evt.pointer("/response/usage") {
                        usage.input = u.pointer("/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        usage.output = u.pointer("/output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        usage.cached = u.pointer("/cached_tokens").and_then(|t| t.as_u64())
                            .or_else(|| u.pointer("/input_tokens_details/cached_tokens").and_then(|t| t.as_u64()))
                            .unwrap_or(0);
                    }
                }
                _ => {}
            }
        }
    }

    // 收尾：关闭所有已打开的块（Anthropic 规范要求 start/stop 配对，
    // 缺少 content_block_stop 会导致客户端无法解析工具调用流）-> message_delta -> message_stop
    ensure_started!();
    for i in open_blocks.drain(..) {
        let _ = tx
            .send(Ok(anthropic_sse(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": i}),
            )))
            .await;
    }
    let _ = tx
        .send(Ok(anthropic_sse(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": if used_tool_calls { "tool_use" } else { "end_turn" }, "stop_sequence": Value::Null},
                "usage": {"output_tokens": out_chars.div_ceil(3)},
            }),
        )))
        .await;
    let _ = tx.send(Ok(anthropic_sse("message_stop", &json!({"type": "message_stop"})))).await;

    // 上游未给 usage 时用字符数估算
    if usage.output == 0 {
        usage.output = out_chars.div_ceil(3);
    }
    (out_chars, usage)
}

// ---------- 响应侧转换：直通 + 非流式聚合 ----------

/// 上游 Chat Completions SSE 直通到客户端 Chat Completions（+ token 统计）
async fn passthrough_chat_stream(
    upstream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    _model: String,
    tx: &tokio::sync::mpsc::Sender<Result<Vec<u8>, std::io::Error>>,
    on_tokens: Option<Box<dyn Fn(u64) + Send + 'static>>,
) -> (u64, stats::TokenCounts) {
    let mut stream = upstream;
    let mut buf: Vec<u8> = Vec::new();
    let mut out_chars: u64 = 0;
    let mut usage = stats::TokenCounts { input: 0, output: 0, cached: 0 };
    'outer: while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        if tx.send(Ok(bytes.to_vec())).await.is_err() { break; }
        buf.extend_from_slice(&bytes);
        while let Some((end, delim)) = find_sse_boundary(&buf) {
            let event_bytes: Vec<u8> = buf.drain(..end + delim).collect();
            let text = String::from_utf8_lossy(&event_bytes);
            if let Some(data_line) = text.lines().rev().find(|l| l.trim_start().starts_with("data:")) {
                let payload = data_line.trim_start()["data:".len()..].trim();
                if payload == "[DONE]" { break 'outer; }
                if let Ok(obj) = serde_json::from_str::<Value>(payload) {
                    if let Some(delta) = obj.pointer("/choices/0/delta/content").and_then(|c| c.as_str()) {
                        out_chars += delta.chars().count() as u64;
                    }
                    if let Some(u) = obj.get("usage") {
                        usage.input = u.pointer("/prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        usage.output = u.pointer("/completion_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        usage.cached = u.pointer("/prompt_tokens_details/cached_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                    }
                }
            }
        }
    }
    if let Some(ref cb) = on_tokens { cb(out_chars); }
    if usage.output == 0 { usage.output = out_chars.div_ceil(3); }
    (out_chars, usage)
}

/// 上游 Anthropic SSE 直通到客户端 Anthropic Messages（+ token 统计）
async fn passthrough_anthropic_stream(
    upstream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    _model: String,
    tx: &tokio::sync::mpsc::Sender<Result<Vec<u8>, std::io::Error>>,
    on_tokens: Option<Box<dyn Fn(u64) + Send + 'static>>,
) -> (u64, stats::TokenCounts) {
    let mut stream = upstream;
    let mut buf: Vec<u8> = Vec::new();
    let mut out_chars: u64 = 0;
    let mut usage = stats::TokenCounts { input: 0, output: 0, cached: 0 };
    'outer: while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        if tx.send(Ok(bytes.to_vec())).await.is_err() { break; }
        buf.extend_from_slice(&bytes);
        while let Some((end, delim)) = find_sse_boundary(&buf) {
            let event_bytes: Vec<u8> = buf.drain(..end + delim).collect();
            let text = String::from_utf8_lossy(&event_bytes);
            if let Some(data_line) = text.lines().rev().find(|l| l.trim_start().starts_with("data:")) {
                let payload = data_line.trim_start()["data:".len()..].trim();
                if let Ok(obj) = serde_json::from_str::<Value>(payload) {
                    let evt_type = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if evt_type == "content_block_delta" {
                        if let Some(d) = obj.pointer("/delta/text").and_then(|t| t.as_str()) {
                            out_chars += d.chars().count() as u64;
                        }
                    } else if evt_type == "message_delta" {
                        if let Some(u) = obj.get("usage") {
                            usage.output = u.pointer("/output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        }
                    } else if evt_type == "message_start" {
                        if let Some(u) = obj.pointer("/message/usage") {
                            usage.input = u.pointer("/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                            usage.cached = u.pointer("/cache_read_input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        }
                    }
                }
            }
        }
    }
    if let Some(ref cb) = on_tokens { cb(out_chars); }
    if usage.output == 0 { usage.output = out_chars.div_ceil(3); }
    (out_chars, usage)
}

/// 非流式聚合：上游 Chat Completions JSON -> 客户端 Responses JSON
fn chat_json_to_responses(v: &Value, model: &str) -> Value {
    let msg = v.pointer("/choices/0/message").cloned().unwrap_or(json!({}));
    let mut output_items = Vec::new();
    if let Some(t) = msg.get("content").and_then(|c| c.as_str()) {
        if !t.is_empty() {
            output_items.push(json!({
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": t}],
            }));
        }
    }
    for tc in msg.get("tool_calls").and_then(|t| t.as_array()).unwrap_or(&vec![]) {
        output_items.push(json!({
            "type": "function_call",
            "call_id": tc.get("id").and_then(|i| i.as_str()).unwrap_or(""),
            "name": tc.pointer("/function/name").and_then(|n| n.as_str()).unwrap_or(""),
            "arguments": tc.pointer("/function/arguments").and_then(|a| a.as_str()).unwrap_or("{}"),
        }));
    }
    let u = v.get("usage").cloned().unwrap_or(json!({}));
    json!({
        "id": format!("resp-{}", next_request_id()),
        "object": "response",
        "model": model,
        "output": output_items,
        "usage": {
            "input_tokens": u.get("prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
            "output_tokens": u.get("completion_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
            "cached_tokens": u.pointer("/prompt_tokens_details/cached_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
        },
    })
}

/// 非流式聚合：上游 Anthropic JSON -> 客户端 Responses JSON
fn anthropic_json_to_responses(v: &Value, model: &str) -> Value {
    let mut output_items = Vec::new();
    if let Some(content) = v.get("content").and_then(|c| c.as_array()) {
        for block in content {
            match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "text" => {
                    let t = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    if !t.is_empty() {
                        output_items.push(json!({
                            "type": "message", "role": "assistant",
                            "content": [{"type": "output_text", "text": t}],
                        }));
                    }
                }
                "tool_use" => {
                    output_items.push(json!({
                        "type": "function_call",
                        "call_id": block.get("id").and_then(|i| i.as_str()).unwrap_or(""),
                        "name": block.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                        "arguments": serde_json::to_string(block.get("input").unwrap_or(&json!({}))).unwrap_or_else(|_| "{}".into()),
                    }));
                }
                _ => {}
            }
        }
    }
    let u = v.get("usage").cloned().unwrap_or(json!({}));
    json!({
        "id": format!("resp-{}", next_request_id()),
        "object": "response",
        "model": model,
        "output": output_items,
        "usage": {
            "input_tokens": u.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
            "output_tokens": u.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
        },
    })
}

/// 非流式聚合：上游 Anthropic JSON -> 客户端 Chat Completions JSON
fn anthropic_json_to_chat(v: &Value) -> Value {
    let mut content_parts = Vec::new();
    let mut tool_calls = Vec::new();
    if let Some(content) = v.get("content").and_then(|c| c.as_array()) {
        for block in content {
            match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "text" => {
                    let t = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    if !t.is_empty() { content_parts.push(t.to_string()); }
                }
                "tool_use" => {
                    tool_calls.push(json!({
                        "id": block.get("id").and_then(|i| i.as_str()).unwrap_or(""),
                        "type": "function",
                        "function": {
                            "name": block.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                            "arguments": serde_json::to_string(block.get("input").unwrap_or(&json!({}))).unwrap_or_else(|_| "{}".into()),
                        },
                    }));
                }
                _ => {}
            }
        }
    }
    let text = content_parts.concat();
    let stop = v.get("stop_reason").and_then(|s| s.as_str()).unwrap_or("end_turn");
    let finish_reason = if stop == "tool_use" { "tool_calls" } else { "stop" };
    let mut message = json!({
        "role": "assistant",
        "content": if text.is_empty() { Value::Null } else { json!(text) },
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    json!({
        "id": format!("chatcmpl-{}", next_request_id()),
        "object": "chat.completion",
        "created": 1_700_000_000u64,
        "model": v.get("model").cloned().unwrap_or(json!("")),
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": v.get("usage").cloned().unwrap_or(json!({})),
    })
}

/// 非流式聚合：上游 Responses JSON -> 客户端 Chat Completions JSON
fn responses_to_chat_json(v: &Value, model: &str) -> Value {
    let mut content_parts = Vec::new();
    let mut tool_calls = Vec::new();
    if let Some(items) = v.get("output").and_then(|o| o.as_array()) {
        for item in items {
            match item.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "message" => {
                    if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                        for part in content {
                            if part.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                                    if !t.is_empty() { content_parts.push(t.to_string()); }
                                }
                            }
                        }
                    }
                }
                "function_call" => {
                    tool_calls.push(json!({
                        "id": item.get("call_id").and_then(|i| i.as_str()).unwrap_or(""),
                        "type": "function",
                        "function": {
                            "name": item.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                            "arguments": item.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}"),
                        },
                    }));
                }
                _ => {}
            }
        }
    }
    let text = content_parts.concat();
    let has_tools = !tool_calls.is_empty();
    let mut message = json!({
        "role": "assistant",
        "content": if text.is_empty() { Value::Null } else { json!(text) },
    });
    if has_tools {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let u = v.get("usage").cloned().unwrap_or(json!({}));
    json!({
        "id": format!("chatcmpl-{}", next_request_id()),
        "object": "chat.completion",
        "created": 1_700_000_000u64,
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": if has_tools { "tool_calls" } else { "stop" }}],
        "usage": {
            "prompt_tokens": u.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
            "completion_tokens": u.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
            "total_tokens": (u.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0) + u.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0)),
        },
    })
}

/// 非流式聚合：上游 Chat Completions JSON -> Anthropic Messages JSON
fn chat_json_to_anthropic(v: &Value) -> Value {
    let model = v.get("model").cloned().unwrap_or(json!(""));
    let msg = v.pointer("/choices/0/message").cloned().unwrap_or(json!({}));

    let mut content: Vec<Value> = Vec::new();
    if let Some(t) = msg.get("content").and_then(|c| c.as_str()) {
        if !t.is_empty() {
            content.push(json!({"type": "text", "text": t}));
        }
    }
    for tc in msg.get("tool_calls").and_then(|t| t.as_array()).unwrap_or(&vec![]) {
        let args = tc.pointer("/function/arguments").and_then(|a| a.as_str()).unwrap_or("{}");
        let input: Value = serde_json::from_str(args).unwrap_or(json!({}));
        content.push(json!({
            "type": "tool_use",
            "id": tc.get("id").and_then(|i| i.as_str()).unwrap_or(""),
            "name": tc.pointer("/function/name").and_then(|n| n.as_str()).unwrap_or(""),
            "input": input,
        }));
    }
    if content.is_empty() {
        content.push(json!({"type": "text", "text": ""}));
    }
    let stop_reason = if v.pointer("/choices/0/finish_reason").and_then(|f| f.as_str()) == Some("tool_calls") {
        "tool_use"
    } else {
        "end_turn"
    };
    let usage = v.get("usage").cloned().unwrap_or(json!({}));
    json!({
        "id": format!("msg_{}", next_request_id()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
            "output_tokens": usage.get("completion_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
        },
    })
}

// ---------- 响应侧流式转换：上游 Chat SSE -> 客户端 Responses SSE ----------

async fn convert_stream_chat_to_responses(
    upstream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    model: String,
    tx: &tokio::sync::mpsc::Sender<Result<Vec<u8>, std::io::Error>>,
    on_tokens: Option<Box<dyn Fn(u64) + Send + 'static>>,
) -> (u64, bool, stats::TokenCounts) {
    let mut stream = upstream;
    let mut buf: Vec<u8> = Vec::new();
    let mut out_chars: u64 = 0;
    let mut used_tool_calls = false;
    let mut usage = stats::TokenCounts { input: 0, output: 0, cached: 0 };
    let mut text_started = false;

    'outer: while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        buf.extend_from_slice(&bytes);
        while let Some((end, delim)) = find_sse_boundary(&buf) {
            let event_bytes: Vec<u8> = buf.drain(..end + delim).collect();
            let text = String::from_utf8_lossy(&event_bytes);
            let data_line = text.lines().rev().find(|l| l.trim_start().starts_with("data:"));
            let Some(data_line) = data_line else { continue };
            let payload = data_line.trim_start()["data:".len()..].trim();
            if payload == "[DONE]" { break 'outer; }
            let Ok(obj) = serde_json::from_str::<Value>(payload) else { continue };

            if let Some(ref cb) = on_tokens { cb(out_chars); }

            // 文本 delta
            if let Some(content) = obj.pointer("/choices/0/delta/content").and_then(|c| c.as_str()) {
                if !text_started {
                    text_started = true;
                    let _ = tx.send(Ok(sse_frame(&json!({
                        "type": "response.output_text.delta",
                        "delta": "",
                    })))).await;
                }
                out_chars += content.chars().count() as u64;
                let _ = tx.send(Ok(sse_frame(&json!({
                    "type": "response.output_text.delta",
                    "delta": content,
                })))).await;
            }
            // tool_calls delta
            if let Some(tcs) = obj.pointer("/choices/0/delta/tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    used_tool_calls = true;
                    if let Some(name) = tc.pointer("/function/name").and_then(|n| n.as_str()) {
                        let _ = tx.send(Ok(sse_frame(&json!({
                            "type": "response.output_item.added",
                            "item": {"type": "function_call", "name": name},
                        })))).await;
                    }
                    if let Some(args) = tc.pointer("/function/arguments").and_then(|a| a.as_str()) {
                        out_chars += args.chars().count() as u64;
                        let _ = tx.send(Ok(sse_frame(&json!({
                            "type": "response.function_call_arguments.delta",
                            "delta": args,
                        })))).await;
                    }
                }
            }
            // usage
            if let Some(u) = obj.get("usage") {
                usage.input = u.pointer("/prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                usage.output = u.pointer("/completion_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                usage.cached = u.pointer("/prompt_tokens_details/cached_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
            }
        }
    }
    if let Some(ref cb) = on_tokens { cb(out_chars); }
    if usage.output == 0 { usage.output = out_chars.div_ceil(3); }
    (out_chars, used_tool_calls, usage)
}

// ---------- 响应侧流式转换：上游 Anthropic SSE -> 客户端 Responses SSE ----------

async fn convert_stream_anthropic_to_responses(
    upstream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    model: String,
    tx: &tokio::sync::mpsc::Sender<Result<Vec<u8>, std::io::Error>>,
    on_tokens: Option<Box<dyn Fn(u64) + Send + 'static>>,
) -> (u64, bool, stats::TokenCounts) {
    let mut stream = upstream;
    let mut buf: Vec<u8> = Vec::new();
    let mut out_chars: u64 = 0;
    let mut used_tool_calls = false;
    let mut usage = stats::TokenCounts { input: 0, output: 0, cached: 0 };

    'outer: while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        buf.extend_from_slice(&bytes);
        while let Some((end, delim)) = find_sse_boundary(&buf) {
            let event_bytes: Vec<u8> = buf.drain(..end + delim).collect();
            let text = String::from_utf8_lossy(&event_bytes);
            let data_line = text.lines().rev().find(|l| l.trim_start().starts_with("data:"));
            let Some(data_line) = data_line else { continue };
            let payload = data_line.trim_start()["data:".len()..].trim();
            if let Ok(obj) = serde_json::from_str::<Value>(payload) {
                let evt_type = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if let Some(ref cb) = on_tokens { cb(out_chars); }
                match evt_type {
                    "content_block_delta" => {
                        // 文本 delta -> Responses output_text.delta
                        if let Some(d) = obj.pointer("/delta/text").and_then(|t| t.as_str()) {
                            out_chars += d.chars().count() as u64;
                            let _ = tx.send(Ok(sse_frame(&json!({
                                "type": "response.output_text.delta",
                                "delta": d,
                            })))).await;
                        }
                        // tool input_json_delta -> Responses function_call_arguments.delta
                        if obj.pointer("/delta/type").and_then(|t| t.as_str()) == Some("input_json_delta") {
                            if let Some(args) = obj.pointer("/delta/partial_json").and_then(|a| a.as_str()) {
                                out_chars += args.chars().count() as u64;
                                let _ = tx.send(Ok(sse_frame(&json!({
                                    "type": "response.function_call_arguments.delta",
                                    "delta": args,
                                })))).await;
                            }
                        }
                    }
                    "content_block_start" => {
                        let block = obj.get("content_block").cloned().unwrap_or(json!({}));
                        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            used_tool_calls = true;
                            let _ = tx.send(Ok(sse_frame(&json!({
                                "type": "response.output_item.added",
                                "item": {
                                    "type": "function_call",
                                    "name": block.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                                },
                            })))).await;
                        }
                    }
                    "message_start" => {
                        if let Some(u) = obj.pointer("/message/usage") {
                            usage.input = u.pointer("/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                            usage.cached = u.pointer("/cache_read_input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        }
                    }
                    "message_delta" => {
                        if let Some(u) = obj.get("usage") {
                            usage.output = u.pointer("/output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(ref cb) = on_tokens { cb(out_chars); }
    if usage.output == 0 { usage.output = out_chars.div_ceil(3); }
    (out_chars, used_tool_calls, usage)
}

// ---------- 响应侧流式转换：上游 Chat SSE -> 客户端 Anthropic SSE ----------

async fn convert_stream_chat_to_anthropic(
    upstream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    model: String,
    tx: &tokio::sync::mpsc::Sender<Result<Vec<u8>, std::io::Error>>,
    on_tokens: Option<Box<dyn Fn(u64) + Send + 'static>>,
) -> (u64, stats::TokenCounts) {
    let mut stream = upstream;
    let mut buf: Vec<u8> = Vec::new();
    let mut out_chars: u64 = 0;
    let mut usage = stats::TokenCounts { input: 0, output: 0, cached: 0 };
    let mut started = false;
    let mut text_index: Option<usize> = None;
    let mut next_index = 0usize;
    // 所有已打开但尚未闭合的 content block（文本 + 工具），收尾时统一关闭。
    // 每个 content_block_start 都**必须**有对应的 content_block_stop，
    // 否则 Anthropic SDK 无法收束 input_json 分片，工具调用静默失效。
    let mut open_blocks: Vec<usize> = Vec::new();
    // OpenAI `tool_calls[].index` -> 已分配的本协议 block index。
    // 必须按该 index 映射：并行工具调用会交错到达，且 arguments 可能出现在
    // 不带 `function.name` 的后续分片里，不能靠「最后分配的那个索引」推断。
    let mut tool_blocks: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    let mut used_tool_calls = false;

    macro_rules! ensure_started {
        () => {
            if !started {
                started = true;
                let _ = tx.send(Ok(anthropic_sse("message_start", &json!({
                    "type": "message_start",
                    "message": {
                        "id": format!("msg_{}", next_request_id()),
                        "type": "message",
                        "role": "assistant",
                        "model": model,
                        "content": [],
                        "stop_reason": Value::Null,
                        "stop_sequence": Value::Null,
                        "usage": {"input_tokens": 0, "output_tokens": 0},
                    },
                })))).await;
            }
        };
    }

    'outer: while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        buf.extend_from_slice(&bytes);
        while let Some((end, delim)) = find_sse_boundary(&buf) {
            let event_bytes: Vec<u8> = buf.drain(..end + delim).collect();
            let text = String::from_utf8_lossy(&event_bytes);
            let data_line = text.lines().rev().find(|l| l.trim_start().starts_with("data:"));
            let Some(data_line) = data_line else { continue };
            let payload = data_line.trim_start()["data:".len()..].trim();
            if payload == "[DONE]" { break 'outer; }
            let Ok(obj) = serde_json::from_str::<Value>(payload) else { continue };

            if let Some(ref cb) = on_tokens { cb(out_chars); }

            if let Some(content) = obj.pointer("/choices/0/delta/content").and_then(|c| c.as_str()) {
                ensure_started!();
                let ti = match text_index {
                    Some(i) => i,
                    None => {
                        let i = next_index;
                        next_index += 1;
                        text_index = Some(i);
                        open_blocks.push(i);
                        let _ = tx.send(Ok(anthropic_sse("content_block_start", &json!({
                            "type": "content_block_start",
                            "index": i,
                            "content_block": {"type": "text", "text": ""},
                        })))).await;
                        i
                    }
                };
                out_chars += content.chars().count() as u64;
                let _ = tx.send(Ok(anthropic_sse("content_block_delta", &json!({
                    "type": "content_block_delta",
                    "index": ti,
                    "delta": {"type": "text_delta", "text": content},
                })))).await;
            }
            // tool_calls
            if let Some(tcs) = obj.pointer("/choices/0/delta/tool_calls").and_then(|t| t.as_array()) {
                ensure_started!();
                for tc in tcs {
                    // OpenAI 用 index 标识同一工具调用的增量分片，缺省按 0 处理
                    let tc_index = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                    if let Some(name) = tc.pointer("/function/name").and_then(|n| n.as_str()) {
                        let bi = next_index;
                        next_index += 1;
                        tool_blocks.insert(tc_index, bi);
                        open_blocks.push(bi);
                        used_tool_calls = true;
                        let _ = tx.send(Ok(anthropic_sse("content_block_start", &json!({
                            "type": "content_block_start",
                            "index": bi,
                            "content_block": {
                                "type": "tool_use",
                                "id": tc.get("id").and_then(|i| i.as_str()).unwrap_or(""),
                                "name": name,
                                "input": {},
                            },
                        })))).await;
                    }
                    if let Some(args) = tc.pointer("/function/arguments").and_then(|a| a.as_str()) {
                        // 只能打到该工具自己的块上。查不到说明 arguments 先于 name 到达，
                        // 此时跳过，而不是回退到「最后分配的索引」——那会串到别的工具上，
                        // 且在 next_index 为 0 时发生 usize 下溢（dev 构建 panic）。
                        let Some(&bi) = tool_blocks.get(&tc_index) else { continue };
                        out_chars += args.chars().count() as u64;
                        let _ = tx.send(Ok(anthropic_sse("content_block_delta", &json!({
                            "type": "content_block_delta",
                            "index": bi,
                            "delta": {"type": "input_json_delta", "partial_json": args},
                        })))).await;
                    }
                }
            }
            // usage from final chunk
            if let Some(u) = obj.get("usage") {
                usage.input = u.pointer("/prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                usage.output = u.pointer("/completion_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                usage.cached = u.pointer("/prompt_tokens_details/cached_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
            }
        }
    }
    ensure_started!();
    // 关闭所有已打开的块——文本块与工具块一视同仁。
    // 曾只关闭文本块，导致 tool_use 块永远停在不闭合状态，工具调用静默失效。
    // 顺序即打开顺序；Anthropic 规范按 index 寻址，不依赖闭合顺序。
    for i in open_blocks {
        let _ = tx.send(Ok(anthropic_sse("content_block_stop", &json!({
            "type": "content_block_stop", "index": i,
        })))).await;
    }
    // 有工具调用时必须报 tool_use。报 end_turn 会让客户端认为模型自然结束、
    // 无工具待执行，从而直接退出 agent 循环。
    let stop_reason = if used_tool_calls { "tool_use" } else { "end_turn" };
    let _ = tx.send(Ok(anthropic_sse("message_delta", &json!({
        "type": "message_delta",
        "delta": {"stop_reason": stop_reason, "stop_sequence": Value::Null},
        "usage": {"output_tokens": out_chars.div_ceil(3)},
    })))).await;
    let _ = tx.send(Ok(anthropic_sse("message_stop", &json!({"type": "message_stop"})))).await;

    if let Some(ref cb) = on_tokens { cb(out_chars); }
    if usage.output == 0 { usage.output = out_chars.div_ceil(3); }
    (out_chars, usage)
}

// ---------- 响应侧流式转换：上游 Anthropic SSE -> 客户端 Chat SSE ----------

async fn convert_stream_anthropic_to_chat(
    upstream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    model: String,
    tx: &tokio::sync::mpsc::Sender<Result<Vec<u8>, std::io::Error>>,
    on_tokens: Option<Box<dyn Fn(u64) + Send + 'static>>,
) -> (u64, bool, stats::TokenCounts) {
    let mut stream = upstream;
    let mut buf: Vec<u8> = Vec::new();
    let mut out_chars: u64 = 0;
    let mut used_tool_calls = false;
    let mut usage = stats::TokenCounts { input: 0, output: 0, cached: 0 };
    let mut started = false;
    // Anthropic content block index -> Chat `tool_calls[].index`。
    // Anthropic 的 index 空间同时包含 text 与 tool_use 块，而 Chat 的
    // tool_calls index 只数工具，因此必须显式映射，不能把上游 index 直接照搬，
    // 更不能硬编码 0——那会把多个并行工具压成同一个调用、参数互相拼接成非法 JSON。
    let mut tool_index_map: std::collections::HashMap<i64, u32> = std::collections::HashMap::new();
    let mut next_tool_index: u32 = 0;

    macro_rules! send_role {
        () => {
            if !started {
                started = true;
                let _ = tx.send(Ok(sse_frame(&make_chunk(&model, json!({"role": "assistant"}), None)))).await;
            }
        };
    }

    'outer: while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        buf.extend_from_slice(&bytes);
        while let Some((end, delim)) = find_sse_boundary(&buf) {
            let event_bytes: Vec<u8> = buf.drain(..end + delim).collect();
            let text = String::from_utf8_lossy(&event_bytes);
            let data_line = text.lines().rev().find(|l| l.trim_start().starts_with("data:"));
            let Some(data_line) = data_line else { continue };
            let payload = data_line.trim_start()["data:".len()..].trim();
            let Ok(obj) = serde_json::from_str::<Value>(payload) else { continue };
            let evt_type = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");

            if let Some(ref cb) = on_tokens { cb(out_chars); }

            match evt_type {
                "content_block_delta" => {
                    send_role!();
                    if let Some(d) = obj.pointer("/delta/text").and_then(|t| t.as_str()) {
                        out_chars += d.chars().count() as u64;
                        let _ = tx.send(Ok(sse_frame(&make_chunk(&model, json!({"content": d}), None)))).await;
                    }
                    if obj.pointer("/delta/type").and_then(|t| t.as_str()) == Some("input_json_delta") {
                        if let Some(args) = obj.pointer("/delta/partial_json").and_then(|a| a.as_str()) {
                            // 用该块自己的 Chat 工具索引。查不到说明这个 delta
                            // 没有对应的 content_block_start（异常流），跳过即可，
                            // 不要回退到 0 —— 那会串到别的工具上。
                            let Some(&ti) = obj
                                .get("index")
                                .and_then(|i| i.as_i64())
                                .and_then(|ai| tool_index_map.get(&ai))
                            else {
                                continue;
                            };
                            out_chars += args.chars().count() as u64;
                            let _ = tx.send(Ok(sse_frame(&make_chunk(&model, json!({
                                "tool_calls": [{"index": ti, "function": {"arguments": args}}],
                            }), None)))).await;
                        }
                    }
                }
                "content_block_start" => {
                    let block = obj.get("content_block").cloned().unwrap_or(json!({}));
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        send_role!();
                        used_tool_calls = true;
                        // 为该块分配一个 Chat 工具索引并登记，后续 input_json_delta
                        // 据此寻址；并行工具调用各得其所，不再全部塌缩到 0。
                        let anthropic_index = obj.get("index").and_then(|i| i.as_i64()).unwrap_or(-1);
                        let ti = next_tool_index;
                        next_tool_index += 1;
                        tool_index_map.insert(anthropic_index, ti);
                        let _ = tx.send(Ok(sse_frame(&make_chunk(&model, json!({
                            "tool_calls": [{
                                "index": ti,
                                "id": block.get("id").and_then(|i| i.as_str()).unwrap_or(""),
                                "function": {"name": block.get("name").and_then(|n| n.as_str()).unwrap_or(""), "arguments": ""},
                            }],
                        }), None)))).await;
                    }
                }
                "message_start" => {
                    if let Some(u) = obj.pointer("/message/usage") {
                        usage.input = u.pointer("/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        usage.cached = u.pointer("/cache_read_input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                    }
                }
                "message_delta" => {
                    if let Some(u) = obj.get("usage") {
                        usage.output = u.pointer("/output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(ref cb) = on_tokens { cb(out_chars); }
    if usage.output == 0 { usage.output = out_chars.div_ceil(3); }
    (out_chars, used_tool_calls, usage)
}

// ---------- 上游请求（含重试）----------

/// 渠道并发槽位守卫：drop 时自动归还渠道槽位并通知前端刷新。
///
/// 所有走闸门的路径（探针 / 余额 / 代理转发）都用 RAII 而非手动
/// `release_slot`，避免中途 `return` 泄漏槽位。
pub(crate) struct ChannelSlotGuard {
    profile_id: Option<String>,
}

impl ChannelSlotGuard {
    fn new(profile_id: String) -> Self {
        Self { profile_id: Some(profile_id) }
    }

    /// 未占用渠道槽位（无调度器时的降级路径）
    fn none() -> Self {
        Self { profile_id: None }
    }
}

impl Drop for ChannelSlotGuard {
    fn drop(&mut self) {
        if let Some(id) = self.profile_id.take() {
            if let Some(s) = SCHEDULER.get() {
                s.release_slot(&id);
            }
        }
        if let Some(h) = APP_HANDLE.get() {
            let _ = h.emit("stats-updated", ());
        }
    }
}

/// 全局闸门上限 = 已启用渠道并发之和。
/// 取 `max(1)` 兜底：`try_acquire(0)` 会因 `cur >= 0` 恒真而永远拿不到槽位。
fn global_gate_max(sched: &crate::scheduler::Scheduler) -> u64 {
    sched.total_concurrency().max(1) as u64
}

/// 尝试同时占用「全局闸门 + 指定渠道」两个槽位。
///
/// 全局拿到但渠道已满时立即归还全局，不占着闸门让其它请求空等。
/// 返回的全局守卫由调用方负责释放；渠道槽位见 `ChannelSlotGuard`。
fn try_acquire_pair(
    sched: &crate::scheduler::Scheduler,
    profile_id: &str,
    gate: u64,
) -> Option<stats::SlotGuard> {
    let id = stats::try_acquire(gate)?;
    let global = stats::SlotGuard::new(id);
    if sched.acquire_slot(profile_id) {
        return Some(global);
    }
    drop(global); // 渠道已满，归还全局槽位继续等待
    None
}

/// 并发租约：持有全局与渠道槽位，drop 时自动归还。
///
/// 代理转发与探针 / 余额共用同一抽象。归还交给 `Drop` 而不是手动调用
/// `release_slot`，这样上游报错等**早退路径**也不会漏掉槽位。
///
/// `profile_id` 为空字符串表示未占用渠道槽位（无调度器时的降级路径）。
pub(crate) struct Lease {
    pub profile_id: String,
    pub channel: ChannelConfig,
    global: Option<stats::SlotGuard>,
    /// 声明在最后：drop 时最后归还渠道槽位，与获取顺序相反。
    /// 该字段只用于 `Drop` 副作用，代码中不直接读取。
    #[allow(dead_code)]
    channel_slot: ChannelSlotGuard,
}

impl Lease {
    /// 收尾：摘除全局槽位，把释放责任交给 `stats::update_last_tokens`。
    ///
    /// 非流式与流式**共用这一条路径**。`update_last_tokens` 一次完成三件事：
    /// 递减 ACTIVE、写入 token 数、把行标记为 `in_flight = 0`。
    /// 因此不能用「只减计数器」的释放方式收尾——那样行会永远停在
    /// `in_flight = 1`，被统计聚合（`WHERE in_flight = 0`）整体排除。
    ///
    /// 返回 SQLite rowid，<= 0 表示未占槽（DB 未就绪的降级路径）。
    pub fn disarm_global(&mut self) -> i64 {
        self.global.take().map(|g| g.disarm()).unwrap_or(0)
    }

    /// 交出**仍上膛**的全局守卫，供调用方移入 spawn 任务后再释放。
    ///
    /// 流式路径必须用这个而不是 `disarm_global()`：后者会立刻摘掉守卫，
    /// 一旦 spawn 出去的任务 panic，就没人再递减 ACTIVE，槽位永久泄漏。
    /// 守卫留在任务里，panic 展开时其 `Drop` 仍会归还。
    pub fn take_global_guard(&mut self) -> Option<stats::SlotGuard> {
        self.global.take()
    }
}

/// 租约的兜底收尾：若全局守卫**从未被摘除**，说明调用方没有走正常收尾
/// （探针 / 余额查询、以及 handler 的各类提前 return），此时补上收尾动作。
///
/// 不这么做的话，这些请求会留下永久 `in_flight = 1` 的行：
/// `in_flight` 语义是「正在执行中」，永久停在 1 会让任何按该字段过滤的查询
/// 看到幽灵请求，且这些行只能等 7 天保留策略清理。
impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(g) = self.global.take() {
            // 先 disarm 再交给 update_last_tokens：它内部会递减 ACTIVE，
            // 守卫若仍上膛则会在随后 Drop 时再减一次，导致计数下溢。
            let idx = g.disarm();
            stats::update_last_tokens(idx, stats::TokenCounts::default());
            if let Some(h) = APP_HANDLE.get() {
                let _ = h.emit("stats-updated", ());
            }
        }
    }
}

/// 为探针 / 余额获取一次并发租约，目标 = 首个启用渠道。
///
/// 走与代理转发**同一条闸门**：先占全局，再占渠道槽位；渠道满则归还全局后重试。
/// 最长等待 120 秒，超时或全禁用返回 Err（附可读原因）。
pub(crate) async fn acquire_lease() -> Result<Lease, String> {
    // 调度器未初始化（启动早期）：降级为全局闸门，目标取全局配置
    let Some(sched) = SCHEDULER.get() else {
        return match acquire_slot().await {
            Some(g) => Ok(Lease {
                profile_id: String::new(),
                channel: probe_channel(),
                global: Some(g),
                channel_slot: ChannelSlotGuard::none(),
            }),
            None => Err("并发已满，等待超时".to_string()),
        };
    };

    for _ in 0..600 {
        // 每轮重新采样闸门并重新选路。曾经把目标锁定为 `first_enabled_id()`，
        // 于是首个渠道一旦饱和且被探针/余额反复撞上，就会空等 120 秒后
        // 报「并发已满」——而此时其它渠道可能完全空闲。
        let gate = global_gate_max(sched);
        if let Some(target) = sched.select_channel() {
            if let Some(global) = try_acquire_pair(sched, &target, gate) {
                // 凭据必须与所占槽位的渠道一致，避免两者取到不同渠道
                let channel = channel_config(&target).unwrap_or_else(probe_channel);
                return Ok(Lease {
                    profile_id: target.clone(),
                    channel,
                    global: Some(global),
                    channel_slot: ChannelSlotGuard::new(target),
                });
            }
        } else if !sched.has_enabled_channel() {
            // 全禁用：继续等待也不会好转，立即返回可读原因
            return Err("所有渠道均已禁用".to_string());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    Err("并发已满，等待槽位超时".to_string())
}

/// 软并发调度：最多等待 120 秒获取一个并发槽位；返回 None 表示等待超时。
///
/// 仅在调度器缺席（启动早期）时使用；正常路径见 `acquire_lease` / `wait_for_slot`。
pub(crate) async fn acquire_slot() -> Option<stats::SlotGuard> {
    // 动态调度下全局配置不再承载并发上限（各渠道自有限制），
    // 0 表示不设闸门——直接传给 try_acquire 会永远拿不到槽位。
    let max = cfg().max_concurrency;
    let gate = if max == 0 { u64::MAX } else { max as u64 };
    let mut waited = false;
    for _ in 0..600 {
        if let Some(id) = stats::try_acquire(gate) {
            if waited {
                println!("[proxy] 并发达到上限，已等待排空；当前并发 {}", stats::active());
            }
            if let Some(h) = APP_HANDLE.get() {
                let _ = h.emit("stats-updated", ());
            }
            return Some(stats::SlotGuard::new(id));
        }
        waited = true;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    eprintln!(
        "[proxy] 并发持续占满 {}（当前 {}），等待 120s 超时",
        max,
        stats::active()
    );
    None
}

/// 构造 429 响应
fn too_many_requests(message: String) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({"error": {"message": message, "type": "concurrency_limit_exceeded"}})),
    )
        .into_response()
}

/// 软并发调度：先由调度器选出可用渠道，再获取全局并发槽位与渠道并发槽位。
/// 最长等待 120 秒；超时返回 429。
///
/// 返回的 `Lease` 按 `Drop` 归还两个槽位，调用方在任何路径提前返回
/// （上游报错、格式转换失败等）都不会泄漏渠道槽位。
async fn wait_for_slot() -> Result<Lease, Response> {
    // 调度器未初始化：降级为全局并发控制
    let Some(sched) = SCHEDULER.get() else {
        return match acquire_slot().await {
            Some(g) => Ok(Lease {
                profile_id: String::new(),
                channel: resolve_channel(""),
                global: Some(g),
                channel_slot: ChannelSlotGuard::none(),
            }),
            None => {
                // 真实生效的上限是用户配置的 `max_concurrency`（口径同 `acquire_slot`），
                // 不是某个硬编码常量；报错必须反映真实值，否则会误导排查方向。
                let max = cfg().max_concurrency;
                Err(too_many_requests(format!(
                    "并发请求已达上限（{max}），排队等待 2 分钟仍未获取到槽位，请稍后重试"
                )))
            }
        };
    };

    let mut waited = false;

    for _ in 0..600 {
        // 每轮重新采样全局闸门：AIMD 会随上游反馈收紧/放宽，
        // 在循环外只取一次会让闸门陈旧最长 120 秒——降速后即使上限已恢复，
        // 排队的请求仍被旧的小闸门卡住，吞吐被钉死在收紧时的值。
        let gate = global_gate_max(sched);
        // 每轮重新选路，渠道释放后能重新被选中，而不是锁定首次选择
        if let Some(profile_id) = sched.select_channel() {
            if let Some(global) = try_acquire_pair(sched, &profile_id, gate) {
                if waited {
                    println!("[proxy] 并发达到上限，已等待排空；当前并发 {}", stats::active());
                }
                if let Some(h) = APP_HANDLE.get() {
                    let _ = h.emit("stats-updated", ());
                }
                // 凭据必须与所占槽位的渠道一致，避免两者取到不同渠道
                let channel = resolve_channel(&profile_id);
                let channel_slot = ChannelSlotGuard::new(profile_id.clone());
                return Ok(Lease {
                    channel,
                    profile_id,
                    global: Some(global),
                    channel_slot,
                });
            }
        } else if !sched.has_enabled_channel() {
            // 无任何启用渠道：继续等待也不会好转，立即返回明确错误
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": {
                        "message": "所有渠道均已禁用：请在设置中至少开启一个渠道参与调度",
                        "type": "no_enabled_channel",
                    }
                })),
            )
                .into_response());
        }
        waited = true;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    eprintln!(
        "[proxy] 所有渠道并发持续占满（当前 {}），等待 120s 超时",
        stats::active()
    );
    Err(too_many_requests(
        "所有渠道并发均已占满，排队等待 2 分钟仍未获取到槽位，请稍后重试".to_string(),
    ))
}

async fn send_upstream(
    payload: &Value,
    user_agent: &str,
    ch: &ChannelConfig,
) -> Result<reqwest::Response, (StatusCode, Value)> {
    let api_key = ch.api_key.trim();
    let url = ch.upstream_url.trim();

    // 空 Key 直接拒绝，避免打到上游才收到难懂的 401
    if api_key.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": {"message": "渠道 API Key 未配置：请在设置面板的该渠道中填写", "type": "proxy_config_error"}}),
        ));
    }

    // 空上游地址拒绝
    if url.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": {"message": "渠道转发目标地址未配置：请在设置面板的该渠道中填写", "type": "proxy_config_error"}}),
        ));
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": {"message": format!("client build failed: {e}"), "type": "proxy_exception"}}),
            )
        })?;

    let mut last_err_body = String::new();
    let mut last_status = StatusCode::BAD_GATEWAY;
    for attempt in 0..MAX_RETRIES {
        let resp = client
            .post(url)
            .bearer_auth(api_key)
            // 与参考实现一致：accept 固定 text/event-stream（网关据此决定是否流式返回），
            // 并转发客户端 user-agent
            .header("accept", "text/event-stream")
            .header("user-agent", user_agent)
            .json(payload)
            .send()
            .await;

        match resp {
            Ok(r) => {
                let status = r.status();
                println!("[upstream] responses status: {} (attempt {})", status.as_u16(), attempt + 1);
                if status.is_server_error() && attempt < MAX_RETRIES - 1 {
                    last_status = status;
                    last_err_body = r.text().await.unwrap_or_default();
                    tokio::time::sleep(std::time::Duration::from_millis(1000 * (attempt as u64 + 1))).await;
                    continue;
                }
                return Ok(r);
            }
            Err(e) => {
                last_status = StatusCode::BAD_GATEWAY;
                last_err_body = format!("{e}");
                if attempt < MAX_RETRIES - 1 {
                    tokio::time::sleep(std::time::Duration::from_millis(1000 * (attempt as u64 + 1))).await;
                    continue;
                }
            }
        }
    }
    Err((
        last_status,
        json!({"error": {"message": format!("上游网关错误（已重试 {MAX_RETRIES} 次）：{last_err_body}"), "type": "upstream_error"}}),
    ))
}

/// 把上游（SSE）响应聚合为完整 chat.completion JSON，返回 (JSON, 输出字符数)。
/// 把上游的 Responses SSE 流聚合为一个完整的 Chat Completions JSON。
///
/// 接受任意字节流而非 `reqwest::Response`：非流式路径可能已经把响应体读进了
/// 内存（用于判断它是 JSON 还是 SSE），需要从字节重建流再走同一套聚合逻辑。
async fn aggregate_chat_completion<S>(upstream: S, model: String) -> (Value, u64, stats::TokenCounts)
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin + Send + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
    let (usage_tx, usage_rx) = tokio::sync::oneshot::channel::<stats::TokenCounts>();
    let model_producer = model.clone();
    tokio::spawn(async move {
        let (_, tool_used, usage) =
            convert_stream(upstream, model_producer.clone(), &tx, None).await;
        let _ = usage_tx.send(usage);
        let finish = make_chunk(
            &model_producer,
            json!({}),
            Some(if tool_used { "tool_calls" } else { "stop" }),
        );
        let _ = tx.send(Ok(sse_frame(&finish))).await;
        let _ = tx.send(Ok(Vec::new())).await; // 空帧哨兵：转换结束
    });

    let mut content_parts: Vec<String> = Vec::new();
    // index -> {id, type, function:{name, arguments}}
    let mut tool_calls: std::collections::BTreeMap<i64, Value> = std::collections::BTreeMap::new();

    while let Some(item) = rx.recv().await {
        let Ok(bytes) = item else { continue };
        if bytes.is_empty() {
            break; // 哨兵：转换完成
        }
        let line = String::from_utf8_lossy(&bytes);
        let Some(payload) = line.strip_prefix("data: ").map(str::trim) else { continue };
        if payload == "[DONE]" {
            break;
        }
        let Ok(obj) = serde_json::from_str::<Value>(payload) else { continue };
        let delta = obj.pointer("/choices/0/delta").cloned().unwrap_or(json!({}));
        if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
            content_parts.push(c.to_string());
        }
        for tc in delta.get("tool_calls").and_then(|t| t.as_array()).unwrap_or(&vec![]) {
            let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
            let slot = tool_calls.entry(idx).or_insert_with(|| {
                json!({"id": "", "type": "function", "function": {"name": "", "arguments": ""}})
            });
            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                slot["id"] = json!(id);
            }
            if let Some(n) = tc.pointer("/function/name").and_then(|n| n.as_str()) {
                let cur = slot.pointer("/function/name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                slot["function"]["name"] = json!(format!("{cur}{n}"));
            }
            if let Some(a) = tc.pointer("/function/arguments").and_then(|a| a.as_str()) {
                let cur = slot.pointer("/function/arguments").and_then(|x| x.as_str()).unwrap_or("").to_string();
                slot["function"]["arguments"] = json!(format!("{cur}{a}"));
            }
        }
    }

    let content = content_parts.concat();
    let chars = content.chars().count() as u64;
    let mut upstream_usage = usage_rx.await.unwrap_or(stats::TokenCounts { input: 0, output: 0, cached: 0 });
    // 上游未给出 usage 时，用实际聚合出的输出字符估算，避免统计一直为 0
    if upstream_usage.output == 0 {
        upstream_usage.output = chars.div_ceil(3);
    }
    let has_tools = !tool_calls.is_empty();
    let mut message = json!({
        "role": "assistant",
        "content": if content.is_empty() { Value::Null } else { json!(content) },
    });
    if has_tools {
        message["tool_calls"] = Value::Array(tool_calls.into_values().collect());
    }
    let completion = json!({
        "id": format!("chatcmpl-{}", next_request_id()),
        "object": "chat.completion",
        "created": 1_700_000_000u64,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": if has_tools { "tool_calls" } else { "stop" },
        }]
    });
    (completion, chars, upstream_usage)
}

/// chat.completion -> Anthropic message（非流式 /v1/messages 响应）
fn chat_completion_to_anthropic(cc: &Value, usage: &stats::TokenCounts) -> Value {
    let model = cc.get("model").cloned().unwrap_or(json!(""));
    let msg = cc.pointer("/choices/0/message").cloned().unwrap_or(json!({}));

    let mut content: Vec<Value> = Vec::new();
    if let Some(t) = msg.get("content").and_then(|c| c.as_str()) {
        if !t.is_empty() {
            content.push(json!({"type": "text", "text": t}));
        }
    }
    for tc in msg.get("tool_calls").and_then(|t| t.as_array()).unwrap_or(&vec![]) {
        let args = tc.pointer("/function/arguments").and_then(|a| a.as_str()).unwrap_or("{}");
        let input: Value = serde_json::from_str(args).unwrap_or(json!({}));
        content.push(json!({
            "type": "tool_use",
            "id": tc.get("id").and_then(|i| i.as_str()).unwrap_or(""),
            "name": tc.pointer("/function/name").and_then(|n| n.as_str()).unwrap_or(""),
            "input": input,
        }));
    }
    if content.is_empty() {
        content.push(json!({"type": "text", "text": ""}));
    }
    let stop_reason = if cc.pointer("/choices/0/finish_reason").and_then(|f| f.as_str()) == Some("tool_calls") {
        "tool_use"
    } else {
        "end_turn"
    };
    json!({
        "id": format!("msg_{}", next_request_id()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": usage.input, "output_tokens": usage.output},
    })
}

/// 从非流式 Responses JSON 里估算输出字符数（遍历 output[].content[] 里的 output_text）
fn estimate_response_output_chars(v: &Value) -> u64 {
    let mut n = 0u64;
    if let Some(items) = v.get("output").and_then(|o| o.as_array()) {
        for it in items {
            if let Some(parts) = it.get("content").and_then(|c| c.as_array()) {
                for p in parts {
                    if p.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                        n += p.get("text").and_then(|t| t.as_str())
                            .map(|s| s.chars().count() as u64)
                            .unwrap_or(0);
                    }
                }
            }
        }
    }
    n
}

// ---------- Axum 路由处理器 ----------

fn request_user_agent(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("trae-proxy/1.0")
        .to_string()
}

async fn upstream_error_response(upstream: reqwest::Response) -> Response {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = upstream.text().await.unwrap_or_default();
    (
        status,
        Json(json!({"error": {"message": format!("上游错误：{}", body), "type": "upstream_error"}})),
    )
        .into_response()
}

/// 把上游状态反馈给调度器的 AIMD 自适应并发控制。
///
/// - 429 → 乘性降速：上游在说「打太快了」，立即收紧
/// - 2xx → 加性提速：带冷却，逐步探回配置的天花板
///
/// 其余状态码不改变并发：4xx 多为参数问题、5xx 为上游自身故障，
/// 把它们当成限流信号会让并发毫无依据地塌到下限。
fn note_upstream_feedback(profile_id: &str, status: StatusCode) {
    let Some(sched) = SCHEDULER.get() else { return };
    if status == StatusCode::TOO_MANY_REQUESTS {
        sched.record_rate_limit(profile_id);
    } else if status.is_success() {
        sched.record_success(profile_id);
    }
}

/// 模式 1：OpenAI 传统 Chat Completions（/v1/chat/completions）
async fn chat_completions(
    State(_): State<()>,
    headers: axum::http::HeaderMap,
    Json(chat_body): Json<Value>,
) -> Response {
    let user_agent = request_user_agent(&headers);
    let wants_stream = chat_body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    // 先选路：渠道的上游格式决定载荷如何转换，必须早于 payload 构造
    let mut lease = match wait_for_slot().await {
        Ok(l) => l,
        Err(resp) => return resp,
    };
    let ch = lease.channel.clone();
    let upstream_fmt = ch.upstream_format;

    let (payload, to_responses) = match upstream_fmt {
        UpstreamFormat::Responses => (chat_to_responses_payload(&chat_body, wants_stream, &ch.model_override), true),
        UpstreamFormat::Anthropic => (chat_to_anthropic_payload(&chat_body, wants_stream, &ch.model_override), false),
        UpstreamFormat::ChatCompletions => {
            // 直通：只替换 model_override
            let mut b = chat_body.clone();
            if !ch.model_override.is_empty() { b["model"] = json!(ch.model_override); }
            (b, false)
        }
    };
    let model = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    let upstream = match send_upstream(&payload, &user_agent, &ch).await {
        Ok(r) => r,
        Err((status, body)) => return (status, Json(body)).into_response(),
    };
    note_upstream_feedback(&lease.profile_id, upstream.status());

    if !upstream.status().is_success() {
        return upstream_error_response(upstream).await;
    }

    if !wants_stream {
        if to_responses {
            // 上游返回 Responses 格式 -> 转为 Chat Completions。
            //
            // 响应体可能是两种形态，必须都处理：
            //  - SSE：`send_upstream` 恒定发送 `accept: text/event-stream`，
            //    网关据此流式返回（实测线上网关即如此）；
            //  - JSON：上游忽略 accept、直接返回完整对象。
            // 曾只按 JSON 解析，遇到 SSE 时解析失败退化成 `json!({})`，
            // 转出的 chat.completion 里 `content: null`、状态码却是 200 ——
            // 非流式请求全部拿到空回复，客户端无法察觉。
            let full = upstream.text().await.unwrap_or_default();
            let is_json = full.trim_start().starts_with('{');
            let (cc, tc) = if is_json {
                let v = serde_json::from_str::<Value>(&full).unwrap_or(json!({}));
                let tc = stats::TokenCounts {
                    input: v.pointer("/usage/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
                    output: v.pointer("/usage/output_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
                    cached: v.pointer("/usage/cached_tokens").and_then(|t| t.as_u64())
                        .or_else(|| v.pointer("/usage/input_tokens_details/cached_tokens").and_then(|t| t.as_u64()))
                        .unwrap_or(0),
                };
                (responses_to_chat_json(&v, &model), tc)
            } else {
                // 把已读入的响应体重新包成流，复用流式聚合逻辑
                let bytes = bytes::Bytes::from(full.into_bytes());
                let stream = futures_util::stream::iter(vec![
                    Ok::<_, reqwest::Error>(bytes),
                ]);
                let (cc, chars, usage) = aggregate_chat_completion(stream, model.clone()).await;
                let tc = if usage.input > 0 || usage.output > 0 {
                    usage
                } else {
                    stats::TokenCounts { input: 0, output: chars.div_ceil(3), cached: 0 }
                };
                (cc, tc)
            };
            let total_tokens = tc.input + tc.output;
            let idx = lease.disarm_global();
            stats::update_last_tokens(idx, tc);
            if let Some(sched) = SCHEDULER.get() { sched.record_request(&lease.profile_id, total_tokens); }
            if let Some(h) = APP_HANDLE.get() { let _ = h.emit("stats-updated", ()); }
            return (StatusCode::OK, Json(cc)).into_response();
        } else if upstream_fmt == UpstreamFormat::ChatCompletions {
            // 上游返回 Chat Completions 格式 -> 直通（已经是客户端期望的格式）
            let content_type = upstream.headers().get("content-type")
                .and_then(|v| v.to_str().ok()).unwrap_or("application/json").to_string();
            let full = upstream.text().await.unwrap_or_default();
            let tc = match serde_json::from_str::<Value>(&full) {
                Ok(v) => {
                    let input = v.pointer("/usage/prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                    let output = v.pointer("/usage/completion_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                    let cached = v.pointer("/usage/prompt_tokens_details/cached_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                    stats::TokenCounts { input, output, cached }
                }
                Err(_) => stats::TokenCounts { input: 0, output: 0, cached: 0 },
            };
            let total_tokens = tc.input + tc.output;
            let idx = lease.disarm_global();
            stats::update_last_tokens(idx, tc);
            if let Some(sched) = SCHEDULER.get() { sched.record_request(&lease.profile_id, total_tokens); }
            if let Some(h) = APP_HANDLE.get() { let _ = h.emit("stats-updated", ()); }
            return Response::builder().status(StatusCode::OK)
                .header("content-type", content_type)
                .body(Body::from(full)).unwrap();
        } else {
            // 上游返回 Anthropic 格式 -> 转为 Chat Completions
            let full = upstream.text().await.unwrap_or_default();
            let v = serde_json::from_str::<Value>(&full).unwrap_or(json!({}));
            let tc = stats::TokenCounts {
                input: v.pointer("/usage/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
                output: v.pointer("/usage/output_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
                cached: 0,
            };
            let cc = anthropic_json_to_chat(&v);
            let total_tokens = tc.input + tc.output;
            let idx = lease.disarm_global();
            stats::update_last_tokens(idx, tc);
            if let Some(sched) = SCHEDULER.get() { sched.record_request(&lease.profile_id, total_tokens); }
            if let Some(h) = APP_HANDLE.get() { let _ = h.emit("stats-updated", ()); }
            return (StatusCode::OK, Json(cc)).into_response();
        }
    }

    // 流式：边读边转换，实时更新 token 数
    let byte_stream = upstream.bytes_stream();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
    // 守卫**移入** spawn 任务再释放：若这里就 disarm（放弃守卫的所有权），
    // 任务一旦 panic 就没人再递减 ACTIVE，槽位永久泄漏。
    let global_guard = lease.take_global_guard();
    let idx = global_guard.as_ref().map(|g| g.idx()).unwrap_or(0);

    tokio::spawn(async move {
        let first = make_chunk(&model, json!({"role": "assistant"}), None);
        let _ = tx.send(Ok(sse_frame(&first))).await;
        let cb: Box<dyn Fn(u64) + Send> = Box::new(move |chars: u64| {
            stats::update_tokens_db_only(idx, stats::TokenCounts {
                input: 0, output: chars.div_ceil(3), cached: 0,
            });
            if let Some(h) = APP_HANDLE.get() { let _ = h.emit("stats-updated", ()); }
        });
        let result = tokio::time::timeout(STREAM_TIMEOUT, async {
            if to_responses {
                // 上游 Responses SSE -> Chat Completions SSE。
                // 必须用 `convert_stream`（它解析 response.output_text.delta /
                // response.usage 并产出 Chat chunk）。曾误用反方向的
                // `convert_stream_chat_to_responses`，它只认 choices[].delta，
                // 而 Responses 事件里没有该字段 —— 结果是流式对话返回空内容且
                // 状态码 200，客户端完全无法察觉。
                convert_stream(byte_stream, model.clone(), &tx, Some(cb)).await
            } else if upstream_fmt == UpstreamFormat::ChatCompletions {
                // 上游 Chat SSE -> Chat SSE 直通
                let (chars, tc) = passthrough_chat_stream(byte_stream, model.clone(), &tx, Some(cb)).await;
                (chars, false, tc)
            } else {
                // 上游 Anthropic SSE -> Chat SSE
                let (_c, used, tc) = convert_stream_anthropic_to_chat(byte_stream, model.clone(), &tx, Some(cb)).await;
                // 必须原样透传工具标志位：丢成 false 会让 finish_reason 恒为 "stop"，
                // OpenAI 客户端据此认为模型自然结束，工具被静默丢弃。
                (_c, used, tc)
            }
        }).await;
        let (chars, tool_used, upstream_usage) = match result {
            Ok(v) => v,
            Err(_) => { (0u64, false, stats::TokenCounts { input: 0, output: 0, cached: 0 }) }
        };
        let tc = if upstream_usage.input > 0 || upstream_usage.output > 0 { upstream_usage }
        else { stats::TokenCounts { input: 0, output: chars.div_ceil(3), cached: 0 } };
        let total_tokens = tc.input + tc.output;
        // 先完成统计与槽位归还，**再**做尾部发送。
        //
        // 尾部 send 在客户端停止读取（通道满）时会长时间阻塞，而它不在
        // STREAM_TIMEOUT 保护内。若把释放排在发送之后，任务一旦卡住，
        // 全局槽位与渠道槽位会随任务永久挂起，累积到上限后整个代理对
        // **所有**请求返回「并发已满」。
        //
        // 守卫在此 disarm 后交给 update_last_tokens 统一归还；若上面任何一步
        // panic，守卫仍上膛，随栈展开 Drop 时归还 —— 两条路都恰好减一次。
        let idx = global_guard.map(|g| g.disarm()).unwrap_or(0);
        stats::update_last_tokens(idx, tc);
        if let Some(sched) = SCHEDULER.get() { sched.record_request(&lease.profile_id, total_tokens); }
        drop(lease); // 归还渠道槽位；此后不再使用 lease
        if let Some(h) = APP_HANDLE.get() { let _ = h.emit("stats-updated", ()); }

        let finish = make_chunk(&model, json!({}), Some(if tool_used { "tool_calls" } else { "stop" }));
        // 有界发送：客户端已断开时最多多活 TAIL_SEND_TIMEOUT，而不是永久挂着
        let _ = tokio::time::timeout(TAIL_SEND_TIMEOUT, tx.send(Ok(sse_frame(&finish)))).await;
        let _ = tokio::time::timeout(TAIL_SEND_TIMEOUT, tx.send(Ok(b"data: [DONE]\n\n".to_vec()))).await;
    });

    sse_response(rx)
}

/// 模式 2：OpenAI Responses API（/v1/responses），原样透传到上游
async fn responses_api(
    State(_): State<()>,
    headers: axum::http::HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    let user_agent = request_user_agent(&headers);
    let wants_stream = body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let mut lease = match wait_for_slot().await {
        Ok(l) => l,
        Err(resp) => return resp,
    };
    let ch = lease.channel.clone();
    let upstream_fmt = ch.upstream_format;

    // 按上游格式转换请求；模型强制覆盖取自选中渠道
    if !ch.model_override.is_empty() {
        body["model"] = json!(ch.model_override);
    }
    let body = match upstream_fmt {
        UpstreamFormat::Responses => body,  // 直通
        UpstreamFormat::ChatCompletions => responses_to_chat_payload(&body, wants_stream, &ch.model_override),
        UpstreamFormat::Anthropic => responses_to_anthropic_payload(&body, wants_stream, &ch.model_override),
    };
    let model = body.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string();

    let upstream = match send_upstream(&body, &user_agent, &ch).await {
        Ok(r) => r,
        Err((status, err)) => return (status, Json(err)).into_response(),
    };
    note_upstream_feedback(&lease.profile_id, upstream.status());

    if !upstream.status().is_success() {
        return upstream_error_response(upstream).await;
    }

    if !wants_stream {
        let content_type = upstream.headers().get("content-type")
            .and_then(|v| v.to_str().ok()).unwrap_or("application/json").to_string();
        let full = upstream.text().await.unwrap_or_default();
        let upstream_val = serde_json::from_str::<Value>(&full).unwrap_or(json!({}));
        // 转换响应为 Responses 格式
        let response_val = match upstream_fmt {
            UpstreamFormat::Responses => upstream_val.clone(),
            UpstreamFormat::ChatCompletions => chat_json_to_responses(&upstream_val, &model),
            UpstreamFormat::Anthropic => anthropic_json_to_responses(&upstream_val, &model),
        };
        let tc = match upstream_fmt {
            UpstreamFormat::Responses => {
                let input = upstream_val.pointer("/usage/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                let output = upstream_val.pointer("/usage/output_tokens").and_then(|t| t.as_u64())
                    .unwrap_or_else(|| estimate_response_output_chars(&upstream_val).div_ceil(3));
                let cached = upstream_val.pointer("/usage/cached_tokens").and_then(|t| t.as_u64())
                    .or_else(|| upstream_val.pointer("/usage/input_tokens_details/cached_tokens").and_then(|t| t.as_u64()))
                    .unwrap_or(0);
                stats::TokenCounts { input, output, cached }
            }
            UpstreamFormat::ChatCompletions => {
                let input = upstream_val.pointer("/usage/prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                let output = upstream_val.pointer("/usage/completion_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                let cached = upstream_val.pointer("/usage/prompt_tokens_details/cached_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                stats::TokenCounts { input, output, cached }
            }
            UpstreamFormat::Anthropic => {
                let input = upstream_val.pointer("/usage/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                let output = upstream_val.pointer("/usage/output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                stats::TokenCounts { input, output, cached: 0 }
            }
        };
        let total_tokens = tc.input + tc.output;
        let idx = lease.disarm_global();
        stats::update_last_tokens(idx, tc);
        if let Some(sched) = SCHEDULER.get() { sched.record_request(&lease.profile_id, total_tokens); }
        if let Some(h) = APP_HANDLE.get() { let _ = h.emit("stats-updated", ()); }
        return Response::builder().status(StatusCode::OK)
            .header("content-type", content_type)
            .body(Body::from(serde_json::to_string(&response_val).unwrap_or_default())).unwrap();
    }

    // 流式：字节原样透传，同时旁路统计 output_text.delta 的字符数
    let byte_stream = upstream.bytes_stream();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
    // 守卫移入任务再释放，panic 时靠 Drop 归还（见 chat_completions 的说明）
    let global_guard = lease.take_global_guard();
    let idx = global_guard.as_ref().map(|g| g.idx()).unwrap_or(0);
    tokio::spawn(async move {
        let mut chars: u64 = 0;
        let mut usage = stats::TokenCounts { input: 0, output: 0, cached: 0 };
        let result = tokio::time::timeout(STREAM_TIMEOUT, async {
            match upstream_fmt {
                UpstreamFormat::Responses => {
                    // 原有逻辑：字节原样透传 + 统计
                    let mut stream = byte_stream;
                    let mut pending: Vec<u8> = Vec::new();
                    loop {
                        let next = match tokio::time::timeout(STREAM_TIMEOUT, stream.next()).await {
                            Ok(Some(chunk)) => chunk, Ok(None) => break, Err(_) => break,
                        };
                        let Ok(bytes) = next else { break };
                        if tx.send(Ok(bytes.to_vec())).await.is_err() { break; }
                        pending.extend_from_slice(&bytes);
                        while let Some((end, delim)) = find_sse_boundary(&pending) {
                            let event_bytes: Vec<u8> = pending.drain(..end + delim).collect();
                            let text = String::from_utf8_lossy(&event_bytes);
                            if let Some(data_line) = text.lines().rev().find(|l| l.trim_start().starts_with("data:")) {
                                let payload_str = data_line.trim_start()["data:".len()..].trim();
                                if let Ok(evt) = serde_json::from_str::<Value>(payload_str) {
                                    let evt_type = evt.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                    if evt_type == "response.output_text.delta" {
                                        chars += evt.get("delta").and_then(|d| d.as_str()).map(|s| s.chars().count() as u64).unwrap_or(0);
                                        stats::update_tokens_db_only(idx, stats::TokenCounts { input: 0, output: chars.div_ceil(3), cached: 0 });
                                        if let Some(h) = APP_HANDLE.get() { let _ = h.emit("stats-updated", ()); }
                                    } else if evt_type == "response.completed" {
                                        if let Some(u) = evt.pointer("/response/usage") {
                                            usage.input = u.pointer("/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                                            usage.output = u.pointer("/output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                                            usage.cached = u.pointer("/cached_tokens").and_then(|t| t.as_u64())
                                                .or_else(|| u.pointer("/input_tokens_details/cached_tokens").and_then(|t| t.as_u64())).unwrap_or(0);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                UpstreamFormat::ChatCompletions => {
                    // 上游 Chat SSE -> Responses SSE
                    let (c, _, u) = convert_stream_chat_to_responses(byte_stream, model.clone(), &tx, Some(Box::new(move |chars: u64| {
                        stats::update_tokens_db_only(idx, stats::TokenCounts { input: 0, output: chars.div_ceil(3), cached: 0 });
                        if let Some(h) = APP_HANDLE.get() { let _ = h.emit("stats-updated", ()); }
                    }))).await;
                    chars = c; usage = u;
                }
                UpstreamFormat::Anthropic => {
                    // 上游 Anthropic SSE -> Responses SSE
                    let (c, _, u) = convert_stream_anthropic_to_responses(byte_stream, model.clone(), &tx, Some(Box::new(move |chars: u64| {
                        stats::update_tokens_db_only(idx, stats::TokenCounts { input: 0, output: chars.div_ceil(3), cached: 0 });
                        if let Some(h) = APP_HANDLE.get() { let _ = h.emit("stats-updated", ()); }
                    }))).await;
                    chars = c; usage = u;
                }
            }
        }).await;
        let _ = result; // timeout already handled inside
        drop(tx);
        let tc = if usage.input > 0 || usage.output > 0 { usage }
        else { stats::TokenCounts { input: 0, output: chars.div_ceil(3), cached: 0 } };
        let total_tokens = tc.input + tc.output;
        // 守卫 disarm 后交给 update_last_tokens；若上面 panic，守卫仍上膛，
        // 随栈展开 Drop 时归还 —— 两条路都恰好减一次（见 chat_completions 说明）
        let idx = global_guard.map(|g| g.disarm()).unwrap_or(0);
        stats::update_last_tokens(idx, tc);
        if let Some(sched) = SCHEDULER.get() { sched.record_request(&lease.profile_id, total_tokens); }
        if let Some(h) = APP_HANDLE.get() { let _ = h.emit("stats-updated", ()); }
    });

    sse_response(rx)
}

/// 模式 3：Anthropic Messages API（/v1/messages），转换为 Responses 上游后按 Anthropic 协议返回
async fn anthropic_messages(
    State(_): State<()>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let user_agent = request_user_agent(&headers);
    let wants_stream = body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    // 先选路：渠道的上游格式决定载荷如何转换，必须早于 payload 构造
    let mut lease = match wait_for_slot().await {
        Ok(l) => l,
        Err(resp) => return resp,
    };
    let ch = lease.channel.clone();
    let upstream_fmt = ch.upstream_format;

    let (payload, to_responses) = match upstream_fmt {
        UpstreamFormat::Responses => (anthropic_to_responses_payload(&body, wants_stream, &ch.model_override), true),
        UpstreamFormat::ChatCompletions => (anthropic_to_chat_payload(&body, wants_stream, &ch.model_override), false),
        UpstreamFormat::Anthropic => {
            // 直通：只替换 model_override
            let mut b = body.clone();
            if !ch.model_override.is_empty() { b["model"] = json!(ch.model_override); }
            (b, false)
        }
    };
    let model = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    let upstream = match send_upstream(&payload, &user_agent, &ch).await {
        Ok(r) => r,
        Err((status, err)) => return (status, Json(err)).into_response(),
    };
    note_upstream_feedback(&lease.profile_id, upstream.status());

    if !upstream.status().is_success() {
        return upstream_error_response(upstream).await;
    }

    if !wants_stream {
        if to_responses {
            // 上游返回 Responses 格式 -> 转为 Anthropic（复用现有 aggregate_chat_completion + chat_completion_to_anthropic）
            let (cc, chars, upstream_usage) =
                aggregate_chat_completion(upstream.bytes_stream(), model).await;
            let tc = if upstream_usage.input > 0 || upstream_usage.output > 0 { upstream_usage }
            else { stats::TokenCounts { input: 0, output: chars.div_ceil(3), cached: 0 } };
            let total_tokens = tc.input + tc.output;
            let idx = lease.disarm_global();
            stats::update_last_tokens(idx, tc.clone());
            if let Some(sched) = SCHEDULER.get() { sched.record_request(&lease.profile_id, total_tokens); }
            if let Some(h) = APP_HANDLE.get() { let _ = h.emit("stats-updated", ()); }
            return (StatusCode::OK, Json(chat_completion_to_anthropic(&cc, &tc))).into_response();
        } else {
            // 上游返回 Chat 或 Anthropic 格式
            let full = upstream.text().await.unwrap_or_default();
            let upstream_val = serde_json::from_str::<Value>(&full).unwrap_or(json!({}));
            let (anthropic_val, tc) = if upstream_fmt == UpstreamFormat::ChatCompletions {
                let cc = upstream_val.clone();
                let tc = stats::TokenCounts {
                    input: cc.pointer("/usage/prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
                    output: cc.pointer("/usage/completion_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
                    cached: 0,
                };
                (chat_json_to_anthropic(&cc), tc)
            } else {
                // Anthropic 直通
                let tc = stats::TokenCounts {
                    input: upstream_val.pointer("/usage/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
                    output: upstream_val.pointer("/usage/output_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
                    cached: 0,
                };
                (upstream_val, tc)
            };
            let total_tokens = tc.input + tc.output;
            let idx = lease.disarm_global();
            stats::update_last_tokens(idx, tc);
            if let Some(sched) = SCHEDULER.get() { sched.record_request(&lease.profile_id, total_tokens); }
            if let Some(h) = APP_HANDLE.get() { let _ = h.emit("stats-updated", ()); }
            return (StatusCode::OK, Json(anthropic_val)).into_response();
        }
    }

    // 流式：转换为 Anthropic SSE 事件序列，实时更新 token 数
    let byte_stream = upstream.bytes_stream();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
    // 守卫移入任务再释放，panic 时靠 Drop 归还（见 chat_completions 的说明）
    let global_guard = lease.take_global_guard();
    let idx = global_guard.as_ref().map(|g| g.idx()).unwrap_or(0);
    tokio::spawn(async move {
        let cb: Box<dyn Fn(u64) + Send> = Box::new(move |chars: u64| {
            stats::update_tokens_db_only(idx, stats::TokenCounts { input: 0, output: chars.div_ceil(3), cached: 0 });
            if let Some(h) = APP_HANDLE.get() { let _ = h.emit("stats-updated", ()); }
        });
        let result = tokio::time::timeout(STREAM_TIMEOUT, async {
            if to_responses {
                // 上游 Responses SSE -> Anthropic SSE（复用现有 convert_stream_anthropic）
                convert_stream_anthropic(byte_stream, model.clone(), &tx, Some(cb)).await
            } else if upstream_fmt == UpstreamFormat::ChatCompletions {
                // 上游 Chat SSE -> Anthropic SSE
                let (c, tc) = convert_stream_chat_to_anthropic(byte_stream, model.clone(), &tx, Some(cb)).await;
                (c, tc)
            } else {
                // Anthropic 直通
                let (c, tc) = passthrough_anthropic_stream(byte_stream, model.clone(), &tx, Some(cb)).await;
                (c, tc)
            }
        }).await;
        let (chars, upstream_usage) = match result {
            Ok(v) => v,
            Err(_) => { eprintln!("[proxy] Anthropic 流式超时"); (0, stats::TokenCounts { input: 0, output: 0, cached: 0 }) }
        };
        drop(tx);
        let tc = if upstream_usage.input > 0 || upstream_usage.output > 0 { upstream_usage }
        else { stats::TokenCounts { input: 0, output: chars.div_ceil(3), cached: 0 } };
        let total_tokens = tc.input + tc.output;
        // 守卫 disarm 后交给 update_last_tokens；若上面 panic，守卫仍上膛，
        // 随栈展开 Drop 时归还 —— 两条路都恰好减一次（见 chat_completions 说明）
        let idx = global_guard.map(|g| g.disarm()).unwrap_or(0);
        stats::update_last_tokens(idx, tc);
        if let Some(sched) = SCHEDULER.get() { sched.record_request(&lease.profile_id, total_tokens); }
        if let Some(h) = APP_HANDLE.get() { let _ = h.emit("stats-updated", ()); }
    });

    sse_response(rx)
}

fn sse_response(rx: tokio::sync::mpsc::Receiver<Result<Vec<u8>, std::io::Error>>) -> Response {
    let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(body)
        .unwrap()
}

async fn health() -> &'static str {
    "ok"
}

// ---------- Axum 路由 ----------

fn build_router() -> Router<()> {
    use tower_http::cors::CorsLayer;
    // 允许 WebView（tauri://localhost）里的测试按钮直接 fetch 本代理
    let cors = CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([axum::http::Method::POST, axum::http::Method::GET, axum::http::Method::OPTIONS])
        .allow_headers(tower_http::cors::Any);
    Router::new()
        .route("/v1/chat/completions", post(chat_completions)) // OpenAI Chat Completions
        .route("/v1/responses", post(responses_api))           // OpenAI Responses
        .route("/v1/messages", post(anthropic_messages))       // Anthropic Messages
        .route("/health", axum::routing::get(health))
        .layer(cors)
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(())
}

/// 进程级单例（CONFIG / SCHEDULER / 渠道凭据表）在测试间共享，
/// 并行执行会互相污染（例如一个用例的渠道被另一个用例的 `first_enabled_id` 看见）。
/// 所有触碰这些单例的测试统一用这把锁串行化。
#[cfg(test)]
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod channel_routing_tests {
    use super::*;

    /// 见 `TEST_LOCK` 的说明：串行化对进程级单例的访问
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        test_lock()
    }

    fn test_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn mk_config(api_key: &str, upstream_url: &str) -> ChannelConfig {
        ChannelConfig {
            api_key: api_key.into(),
            upstream_url: upstream_url.into(),
            upstream_format: UpstreamFormat::Responses,
            model_override: String::new(),
        }
    }

    /// 渠道当前在飞并发数（与并发上限无关，适合做稳定的断言）
    fn in_flight(sched: &crate::scheduler::Scheduler, profile_id: &str) -> usize {
        sched
            .snapshot()
            .iter()
            .find(|c| c.profile_id == profile_id)
            .map(|c| c.current_concurrency)
            .unwrap_or(0)
    }

    /// 回归保护：修复前请求路径读的是一个「空调度器」实例，
    /// `select_channel()` 恒为 None，于是所有请求一律 429。
    /// 这里验证注册渠道后调度器确实能选出渠道，且能解析出对应上游凭据。
    #[test]
    fn registered_channel_is_selectable_and_resolvable() {
        let _g = lock();
        let sched = init_scheduler();

        // ch1 禁用、ch2 启用
        sched.upsert_channel("ch1", false, 20, 0, 0, 100);
        sched.upsert_channel("ch2", true, 30, 0, 0, 100);
        set_channel_config(
            "ch2",
            ChannelConfig {
                api_key: "sk-ch2".into(),
                upstream_url: "https://ch2.example.com/v1/responses".into(),
                upstream_format: UpstreamFormat::Anthropic,
                model_override: "m-ch2".into(),
            },
        );

        // 仅 ch2 启用 → 必须选中 ch2 而不是 None（None 会让请求直接 429）
        let picked = sched.select_channel();
        assert_eq!(picked.as_deref(), Some("ch2"), "启用渠道未被选中，请求会直接 429");

        // 凭据必须解析到选中渠道自身，而不是全局配置
        let ch = resolve_channel("ch2");
        assert_eq!(ch.api_key, "sk-ch2");
        assert_eq!(ch.upstream_url, "https://ch2.example.com/v1/responses");
        assert_eq!(ch.upstream_format, UpstreamFormat::Anthropic);
        assert_eq!(ch.model_override, "m-ch2");

        // 未注册的 id 不产生凭据。
        // 这里只断言 channel_config 的查找结果，不走 resolve_channel 的
        // 「回退全局配置」分支——那条分支读进程级 CONFIG，会与其它测试相互干扰。
        assert!(channel_config("no-such-channel").is_none());

        sched.remove_channel("ch1");
        sched.remove_channel("ch2");
        remove_channel_config("ch1");
        remove_channel_config("ch2");
    }

    /// 回归保护：「全禁用」必须与「全满」区分开。
    /// 前者继续等待也不会好转，应立即报错；旧实现一律当成后者并静默返回 429。
    #[test]
    fn all_disabled_is_distinguishable_from_all_busy() {
        let s = crate::scheduler::Scheduler::new();
        s.upsert_channel("a", false, 10, 0, 0, 100);
        s.upsert_channel("b", false, 10, 0, 0, 100);

        assert_eq!(s.select_channel(), None);
        assert!(
            !s.has_enabled_channel(),
            "全禁用必须能被识别，否则会被误判为「已满」并让请求白等 2 分钟"
        );

        // 启用一个之后，就应该能选出来
        s.upsert_channel("b", true, 10, 0, 0, 100);
        assert!(s.has_enabled_channel());
        assert_eq!(s.select_channel().as_deref(), Some("b"));
    }

    /// 回归保护：修复前探针 / 余额走的是 `acquire_slot()` 这条遗留全局路径，
    /// 且全局 `max_concurrency` 被置为 0，导致 `try_acquire(0)` 恒返回 None
    /// —— 探针每次都要空等 120 秒后才报「并发已满」。
    #[test]
    fn zero_global_max_must_not_deadlock_slot_acquisition() {
        let _g = lock();
        // 直接暴露底层行为：max=0 时 cur >= 0 恒真，永远拿不到槽位
        assert!(
            stats::try_acquire(0).is_none(),
            "try_acquire(0) 竟然成功了，前提假设有变"
        );

        // acquire_slot 必须把 0 当作「不设闸门」，而不是「零容量」
        *CONFIG.write().unwrap_or_else(|e| e.into_inner()) = Some(ProxyConfig {
            api_key: String::new(),
            model_override: String::new(),
            port: 0,
            upstream_url: String::new(),
            max_concurrency: 0,
            upstream_format: UpstreamFormat::Responses,
        });
        let got = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(acquire_slot());
        assert!(
            got.is_some(),
            "全局 max_concurrency=0 时 acquire_slot 拿不到槽位，探针会永远失败"
        );
    }

    /// 回归保护：租约必须经调度闸门获取，并占用选中渠道的槽位。
    /// 修复前探针不选渠道、不占渠道槽位，完全绕开渠道的并发 / RPM / TPM 限制。
    #[test]
    fn probe_lease_occupies_channel_slot() {
        let _g = lock();
        let sched = init_scheduler();
        sched.upsert_channel("probe-ch", true, 8, 0, 0, 100);
        set_channel_config("probe-ch", mk_config("sk-probe", "https://probe.example.com/v1/responses"));

        let rt = test_rt();
        let lease = rt.block_on(acquire_lease()).expect("应能取到租约");
        let leased_id = lease.profile_id.clone();

        // 断言与并发上限无关：直接看该渠道的在飞计数是否 +1
        assert_eq!(
            in_flight(sched, &leased_id),
            1,
            "租约没有占用渠道并发槽位，探针绕过了渠道限制"
        );
        assert_eq!(lease.channel.api_key, "sk-probe", "租约凭据与所持槽位渠道不一致");

        drop(lease);
        assert_eq!(
            in_flight(sched, &leased_id),
            0,
            "租约释放后渠道槽位未归还，渠道会被探针占死"
        );

        sched.remove_channel("probe-ch");
        remove_channel_config("probe-ch");
    }

    /// 回归保护：上游返回 429（或任何非 2xx）时 handler 会提前返回。
    /// 修复前这些早退路径漏了 `release_slot`，渠道槽位被永久占用：
    /// 每来一次上游 429 就漏一个槽位，累积到渠道并发上限后该渠道
    /// 从调度器候选中消失；而 `enabled` 仍为 true，请求不会快速失败，
    /// 而是空等 120 秒后才拿到 429。单渠道部署下整个代理就此瘫痪。
    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_429_releases_channel_slot() {
        use tower::ServiceExt;

        let _g = lock();
        let sched = init_scheduler();

        // 本地 mock 上游：一律返回 429，避免依赖外网
        let upstream = Router::new().route(
            "/v1/responses",
            post(|| async {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({"error": {"message": "rate limited"}})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });

        sched.upsert_channel("err-ch", true, 4, 0, 0, 100);
        set_channel_config(
            "err-ch",
            ChannelConfig {
                api_key: "sk-err".into(),
                upstream_url: format!("http://{addr}/v1/responses"),
                upstream_format: UpstreamFormat::Responses,
                model_override: String::new(),
            },
        );

        let resp = build_router()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model": "m", "input": "hi"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "上游 429 应原样透传给客户端"
        );
        assert_eq!(
            in_flight(sched, "err-ch"),
            0,
            "上游错误早退后渠道槽位未归还：渠道会被 429 逐步占死直到永久不可用"
        );

        // 429 必须反馈给调度器：否则上游在限流、本地却继续按配置上限猛打，
        // 把一次限流放大成持续雪崩。
        let limit = sched
            .snapshot()
            .iter()
            .find(|c| c.profile_id == "err-ch")
            .map(|c| c.effective_limit)
            .unwrap();
        assert_eq!(
            limit, 3,
            "429 未反馈给调度器：自适应上限应 4→3 收敛，实际 {limit}"
        );

        sched.remove_channel("err-ch");
        remove_channel_config("err-ch");
    }

    /// 回归保护：非流式请求的 token 消耗曾按 0 计入 TPM 窗口，
    /// 流式也只记输出部分，导致 `max_tpm` 远达不到配置的拦截效果。
    /// 这里断言按 input + output 全量记账。
    #[tokio::test(flavor = "multi_thread")]
    async fn non_streaming_request_records_input_and_output_tokens() {
        use tower::ServiceExt;

        let _g = lock();
        let sched = init_scheduler();

        // mock 上游：返回带 usage 的 Chat Completions 响应
        let upstream = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                Json(json!({
                    "id": "cmpl-1",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "hi"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 50}
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });

        // max_tpm 设大：本用例只验证记账口径，不触发限流
        sched.upsert_channel("tpm-ch", true, 4, 0, 1_000_000, 100);
        set_channel_config(
            "tpm-ch",
            ChannelConfig {
                api_key: "sk-tpm".into(),
                upstream_url: format!("http://{addr}/v1/chat/completions"),
                upstream_format: UpstreamFormat::ChatCompletions,
                model_override: String::new(),
            },
        );

        let resp = build_router()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "mock 上游应返回成功");

        let snap = sched.snapshot();
        let ch = snap.iter().find(|c| c.profile_id == "tpm-ch").unwrap();
        assert_eq!(
            ch.current_tpm, 150,
            "TPM 未按 input(100)+output(50) 全量记账，max_tpm 限额会形同虚设"
        );

        sched.remove_channel("tpm-ch");
        remove_channel_config("tpm-ch");
    }

    /// 回归保护：非流式请求曾用 `update_tokens_db_only` 收尾，它只写 token 数、
    /// **不写 `in_flight = 0`**，于是这些行永远停在 `in_flight = 1`，
    /// 被 token 聚合（`WHERE in_flight = 0`）整体排除 —— 仪表盘的历史用量
    /// 看不到任何非流式请求，而它们恰恰是大多数客户端的默认调用方式。
    #[tokio::test(flavor = "multi_thread")]
    async fn non_streaming_usage_is_visible_in_stats_aggregation() {
        use tower::ServiceExt;

        let _g = lock();
        let sched = init_scheduler();
        // 用内存库，避免把伪造数据写进用户真实的统计文件
        stats::use_memory_db_for_test();

        let upstream = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                Json(json!({
                    "id": "cmpl-agg",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "hi"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 50}
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });

        sched.upsert_channel("agg-ch", true, 4, 0, 0, 100);
        set_channel_config(
            "agg-ch",
            ChannelConfig {
                api_key: "sk-agg".into(),
                upstream_url: format!("http://{addr}/v1/chat/completions"),
                upstream_format: UpstreamFormat::ChatCompletions,
                model_override: String::new(),
            },
        );

        let resp = build_router()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // 非流式路径在返回响应前就完成收尾，读到 body 即代表落库已完成
        let _ = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();

        let total: u64 = stats::buckets(60)
            .iter()
            .map(|b| b.input_tokens + b.output_tokens)
            .sum();
        assert_eq!(
            total, 150,
            "非流式请求的 token 没进统计聚合：行停在 in_flight=1 被 WHERE 过滤掉了"
        );

        sched.remove_channel("agg-ch");
        remove_channel_config("agg-ch");
    }

    /// 回归保护：流式路径的 TPM 记账口径。修复前流式只记输出、非流式记 0，
    /// 两者都不能反映真实消耗。非流式已由上一个用例覆盖，这里补上**流式**这一缺口：
    /// 断言流式请求结束后 TPM 按 mock 上游返回的 input + output 全量记账。
    #[tokio::test(flavor = "multi_thread")]
    async fn streaming_request_records_input_and_output_tokens() {
        use tower::ServiceExt;

        let _g = lock();
        let sched = init_scheduler();

        // mock 上游：返回 Chat 格式 SSE（含 usage 的那一帧是记账依据）
        let upstream = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                let sse = concat!(
                    "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"He\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50}}\n\n",
                    "data: [DONE]\n\n"
                );
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    sse,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });

        // max_tpm 设大：本用例只验证记账口径，不触发限流
        sched.upsert_channel("sse-ch", true, 4, 0, 1_000_000, 100);
        set_channel_config(
            "sse-ch",
            ChannelConfig {
                api_key: "sk-sse".into(),
                upstream_url: format!("http://{addr}/v1/chat/completions"),
                upstream_format: UpstreamFormat::ChatCompletions,
                model_override: String::new(),
            },
        );

        let resp = build_router()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "m",
                            "stream": true,
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "mock 上游应返回成功");

        // 必须把 SSE 响应体读到结束，否则流式任务的 tx 端会阻塞，统计永远不会写入
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&body).contains("[DONE]"),
            "流式响应未读到结尾，mock 上游的 SSE 没有被完整透传"
        );

        // 统计写在 spawn 出的流式任务里，存在真实竞态；但「渠道在飞并发归零」是确定性的同步点：
        // 任务内的顺序是 update_last_tokens → record_request → 闭包结束（lease 在此 Drop 归还渠道槽位），
        // 所以并发归零必然意味着 record_request 已经执行完，无需 sleep 硬猜时长。
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while in_flight(sched, "sse-ch") != 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            in_flight(sched, "sse-ch"),
            0,
            "等待流式任务归还渠道槽位超时，统计可能尚未写入"
        );

        let snap = sched.snapshot();
        let ch = snap.iter().find(|c| c.profile_id == "sse-ch").unwrap();
        assert_eq!(
            ch.current_tpm, 150,
            "流式 TPM 未按 input(100)+output(50) 全量记账，max_tpm 限额会形同虚设"
        );

        sched.remove_channel("sse-ch");
        remove_channel_config("sse-ch");
    }

    /// 与上一条互补：若上游**忽略** `accept: text/event-stream` 而直接返回
    /// 完整 JSON，也必须正确转换。修 SSE 分支时不能把 JSON 分支改坏。
    #[tokio::test(flavor = "multi_thread")]
    async fn non_streaming_chat_with_responses_upstream_accepts_plain_json() {
        use tower::ServiceExt;

        let _g = lock();
        let sched = init_scheduler();

        let upstream = Router::new().route(
            "/v1/responses",
            post(|| async {
                Json(json!({
                    "id": "resp_1",
                    "object": "response",
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "你好世界"}]
                    }],
                    "usage": {"input_tokens": 10, "output_tokens": 5}
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });

        sched.upsert_channel("json-ch", true, 4, 0, 0, 100);
        set_channel_config(
            "json-ch",
            ChannelConfig {
                api_key: "sk-json".into(),
                upstream_url: format!("http://{addr}/v1/responses"),
                upstream_format: UpstreamFormat::Responses,
                model_override: String::new(),
            },
        );

        let resp = build_router()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
        assert_eq!(
            v.pointer("/choices/0/message/content").and_then(|c| c.as_str()),
            Some("你好世界"),
            "上游返回纯 JSON 时内容丢失：{}",
            String::from_utf8_lossy(&bytes)
        );
        assert_eq!(
            v.pointer("/usage/prompt_tokens").and_then(|t| t.as_u64()),
            Some(10),
            "JSON 分支的 usage 未正确透传"
        );

        sched.remove_channel("json-ch");
        remove_channel_config("json-ch");
    }

    /// 回归保护：**非流式**请求打 Responses 上游时，上游仍可能按
    /// `accept: text/event-stream`（`send_upstream` 恒定发送该头）返回 SSE ——
    /// 但这条分支把响应体当 JSON 解析，解析失败就退化成 `json!({})`，
    /// 转出来的 chat.completion 里 `content: null`，HTTP 状态码却是 200。
    ///
    /// 这不是推测：线上实例 `POST /v1/chat/completions`（不带 stream）
    /// 实测返回 `{"message":{"content":null,"role":"assistant"}}`。
    #[tokio::test(flavor = "multi_thread")]
    async fn non_streaming_chat_with_responses_upstream_returns_content() {
        use tower::ServiceExt;

        let _g = lock();
        let sched = init_scheduler();

        // 上游对非流式请求也返回 SSE —— 这正是线上网关的行为
        const SSE: &str = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"你好\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"世界\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
            "data: [DONE]\n\n",
        );

        let upstream = Router::new().route(
            "/v1/responses",
            post(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    SSE,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });

        sched.upsert_channel("ns-ch", true, 4, 0, 0, 100);
        set_channel_config(
            "ns-ch",
            ChannelConfig {
                api_key: "sk-ns".into(),
                upstream_url: format!("http://{addr}/v1/responses"),
                upstream_format: UpstreamFormat::Responses,
                model_override: String::new(),
            },
        );

        // 注意：不带 stream（或显式 false）—— 走非流式路径
        let resp = build_router()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "m",
                            "messages": [{"role": "user", "content": "hi"}],
                            "stream": false
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
        let content = v.pointer("/choices/0/message/content");

        assert_eq!(
            content.and_then(|c| c.as_str()),
            Some("你好世界"),
            "非流式请求 + Responses 上游返回了空内容（上游以 SSE 返回，却被当 JSON 解析）。\
             实际响应：{}",
            String::from_utf8_lossy(&bytes)
        );

        sched.remove_channel("ns-ch");
        remove_channel_config("ns-ch");
    }

    /// 回归保护：`to_responses` 分支曾调用**反方向**的转换器
    /// （`convert_stream_chat_to_responses` 只解析 `choices[].delta`），
    /// 而 Responses 上游的事件里根本没有该字段 —— 结果是流式对话返回**空内容**，
    /// 状态码却是 200，客户端完全无法察觉。这是默认配置下的主路径。
    #[tokio::test(flavor = "multi_thread")]
    async fn streaming_chat_with_responses_upstream_returns_content() {
        use tower::ServiceExt;

        let _g = lock();
        let sched = init_scheduler();

        // Responses 格式的 SSE：内容藏在 response.output_text.delta 里
        const SSE: &str = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"你好\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"世界\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
            "data: [DONE]\n\n",
        );

        let upstream = Router::new().route(
            "/v1/responses",
            post(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    SSE,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });

        sched.upsert_channel("resp-ch", true, 4, 0, 0, 100);
        set_channel_config(
            "resp-ch",
            ChannelConfig {
                api_key: "sk-resp".into(),
                upstream_url: format!("http://{addr}/v1/responses"),
                upstream_format: UpstreamFormat::Responses,
                model_override: String::new(),
            },
        );

        let resp = build_router()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "m",
                            "messages": [{"role": "user", "content": "hi"}],
                            "stream": true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);

        assert!(
            body.contains("你好") && body.contains("世界"),
            "流式 Chat + Responses 上游返回了空内容（转换器方向接反）。实际响应体：{body}"
        );

        sched.remove_channel("resp-ch");
        remove_channel_config("resp-ch");
    }

    /// 回归保护：Chat 上游 + `/v1/messages` 流式的工具调用路径曾**整体失效**——
    /// 工具块从不发 `content_block_stop`、`stop_reason` 恒为 `end_turn`
    /// （源码里写成 `if usage.output > 0 { "end_turn" } else { "end_turn" }` 的恒真式），
    /// 且 `input_json_delta` 用「最后分配的索引」而非该工具自己的索引。
    /// 客户端表现为工具被静默丢弃、或并行工具参数互相串味。
    #[tokio::test(flavor = "multi_thread")]
    async fn streaming_anthropic_with_chat_upstream_emits_complete_tool_calls() {
        use tower::ServiceExt;

        let _g = lock();
        let sched = init_scheduler();

        // 用 json! 构造再序列化，避免手写多层转义出错。
        // 两个并行工具调用，参数分片交错到达——这正是 index 映射会被考验的地方。
        let frame_a = json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_a","function":{"name":"get_weather","arguments":""}},
            {"index":1,"id":"call_b","function":{"name":"get_stock","arguments":""}}
        ]}}]}).to_string();
        let frame_b = json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"{\"city\":"}},
            {"index":1,"function":{"arguments":"{\"sym\":"}}
        ]}}]}).to_string();
        let sse: &'static str =
            Box::leak(format!("data: {frame_a}\n\ndata: {frame_b}\n\ndata: [DONE]\n\n").into_boxed_str());

        let upstream = Router::new().route(
            "/v1/chat/completions",
            post(move || async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    sse,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });

        sched.upsert_channel("tool-ch", true, 4, 0, 0, 100);
        set_channel_config(
            "tool-ch",
            ChannelConfig {
                api_key: "sk-tool".into(),
                upstream_url: format!("http://{addr}/v1/chat/completions"),
                upstream_format: UpstreamFormat::ChatCompletions,
                model_override: String::new(),
            },
        );

        let resp = build_router()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "m",
                            "messages": [{"role": "user", "content": "hi"}],
                            "stream": true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        let events: Vec<Value> = body
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|p| serde_json::from_str::<Value>(p).ok())
            .collect();
        assert!(!events.is_empty(), "未解析到任何 SSE 事件：{body}");

        // 1) 每个已打开的块都必须闭合
        let starts: Vec<i64> = events
            .iter()
            .filter(|e| e["type"] == "content_block_start")
            .filter_map(|e| e["index"].as_i64())
            .collect();
        let stops: Vec<i64> = events
            .iter()
            .filter(|e| e["type"] == "content_block_stop")
            .filter_map(|e| e["index"].as_i64())
            .collect();
        assert_eq!(starts.len(), 2, "应打开两个工具块，实际 {starts:?}");
        for i in &starts {
            assert!(
                stops.contains(i),
                "块 {i} 缺少 content_block_stop，Anthropic SDK 无法解析工具调用"
            );
        }

        // 2) 并行工具的参数必须各归各的块，且索引不得下溢
        let json_deltas: Vec<(i64, String)> = events
            .iter()
            .filter(|e| e["type"] == "content_block_delta")
            .filter(|e| e["delta"]["type"] == "input_json_delta")
            .filter_map(|e| {
                Some((
                    e["index"].as_i64()?,
                    e["delta"]["partial_json"].as_str()?.to_string(),
                ))
            })
            .collect();
        let city = json_deltas.iter().find(|(_, j)| j.contains("city"));
        let sym = json_deltas.iter().find(|(_, j)| j.contains("sym"));
        let (city, sym) = (city.expect("缺少 city 参数分片"), sym.expect("缺少 sym 参数分片"));
        assert_ne!(
            city.0, sym.0,
            "两个并行工具的参数被打到同一个块上，客户端会拿到非法 JSON"
        );

        // 3) 有工具调用时必须报 tool_use
        let stop = events
            .iter()
            .find(|e| e["type"] == "message_delta")
            .and_then(|e| e["delta"]["stop_reason"].as_str())
            .unwrap_or("");
        assert_eq!(
            stop, "tool_use",
            "有工具调用却报 end_turn，客户端会认为模型自然结束、不执行工具"
        );

        sched.remove_channel("tool-ch");
        remove_channel_config("tool-ch");
    }

    /// 回归保护：Anthropic 上游 + `/v1/chat/completions` 流式的工具调用曾有
    /// 两处缺陷——`tool_calls[].index` 硬编码 0（并行工具全部塌缩成一个、
    /// 参数互相拼接成非法 JSON），以及调用方把工具标志位丢成 `false`
    /// （`finish_reason` 恒为 `stop`，客户端不执行工具）。
    ///
    /// 用例特意在前面放一个**文本块**：Anthropic 的 index 空间同时包含文本与
    /// 工具块，所以上遊 index 1/2 必须映射成 Chat tool_calls 索引 0/1，
    /// 直接照搬上游 index 也是错的。
    #[tokio::test(flavor = "multi_thread")]
    async fn streaming_chat_with_anthropic_upstream_keeps_parallel_tool_indices() {
        use tower::ServiceExt;

        let _g = lock();
        let sched = init_scheduler();

        let frames = [
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_a","name":"get_weather","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_b","name":"get_stock","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"sym\":"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
            json!({"type":"message_stop"}),
        ];
        let sse: &'static str = Box::leak(
            frames
                .iter()
                .map(|f| format!("data: {f}\n\n"))
                .collect::<String>()
                .into_boxed_str(),
        );

        let upstream = Router::new().route(
            "/v1/messages",
            post(move || async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    sse,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });

        sched.upsert_channel("anth-ch", true, 4, 0, 0, 100);
        set_channel_config(
            "anth-ch",
            ChannelConfig {
                api_key: "sk-anth".into(),
                upstream_url: format!("http://{addr}/v1/messages"),
                upstream_format: UpstreamFormat::Anthropic,
                model_override: String::new(),
            },
        );

        let resp = build_router()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "m",
                            "messages": [{"role": "user", "content": "hi"}],
                            "stream": true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        let events: Vec<Value> = body
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|p| serde_json::from_str::<Value>(p).ok())
            .collect();

        assert!(body.contains("hi"), "正文未透传：{body}");

        let mut named: Vec<(String, i64)> = Vec::new();
        let mut args: Vec<(String, i64)> = Vec::new();
        for e in &events {
            let Some(tc) = e["choices"][0]["delta"]["tool_calls"]
                .as_array()
                .and_then(|a| a.first())
            else {
                continue;
            };
            let idx = tc["index"].as_i64().unwrap_or(-1);
            if let Some(n) = tc["function"]["name"].as_str().filter(|n| !n.is_empty()) {
                named.push((n.to_string(), idx));
            }
            if let Some(a) = tc["function"]["arguments"].as_str().filter(|a| !a.is_empty()) {
                args.push((a.to_string(), idx));
            }
        }

        assert_eq!(named.len(), 2, "应声明两个工具，实际 {named:?}");
        assert_eq!(named[0].1, 0, "第一个工具应拿到 Chat 索引 0，实际 {named:?}");
        assert_eq!(
            named[1].1, 1,
            "第二个工具应与第一个分开（上游 index 2 应映射为 Chat 索引 1），实际 {named:?}"
        );

        let city = args.iter().find(|(a, _)| a.contains("city")).expect("缺 city 分片");
        let sym = args.iter().find(|(a, _)| a.contains("sym")).expect("缺 sym 分片");
        assert_eq!(city.1, 0, "city 参数打到了错误的工具索引，客户端会拿到非法 JSON");
        assert_eq!(sym.1, 1, "sym 参数打到了错误的工具索引，客户端会拿到非法 JSON");

        let finish = events
            .iter()
            .find_map(|e| e["choices"][0]["finish_reason"].as_str());
        assert_eq!(
            finish,
            Some("tool_calls"),
            "有工具调用却报 stop，OpenAI 客户端会跳过工具执行"
        );

        sched.remove_channel("anth-ch");
        remove_channel_config("anth-ch");
    }

    /// `find_sse_boundary` 必须识别 SSE 规范允许的全部三种行结束符
    /// （`\n` / `\r\n` / `\r`）及其混合形式。
    /// 只认 `\n\n` 时，CRLF 上游会让帧边界**永远找不到**。
    #[test]
    fn sse_boundary_handles_all_line_endings() {
        // (输入, 期望分隔符起点, 期望分隔符长度)
        let cases: &[(&[u8], usize, usize)] = &[
            (b"data: {}\n\nnext", 8, 2),
            (b"data: {}\r\n\r\nnext", 8, 4),
            (b"data: {}\r\rnext", 8, 2),
            (b"data: {}\r\n\nnext", 8, 3),
            (b"data: {}\n\r\nnext", 8, 3),
        ];
        for (input, want_start, want_len) in cases {
            assert_eq!(
                find_sse_boundary(input),
                Some((*want_start, *want_len)),
                "帧边界解析错误：{:?}",
                String::from_utf8_lossy(input)
            );
        }

        // 未完成的帧不能被当作边界，否则会截断正在到达的事件
        assert_eq!(find_sse_boundary(b"data: {}\n"), None, "单个行结束符不是空行");
        assert_eq!(find_sse_boundary(b"data: {}"), None);
    }

    /// 回归保护：CRLF 分帧的上游曾导致**转换路径整段丢失内容**——
    /// 帧边界永远找不到，客户端收到 HTTP 200 却只有骨架帧。
    #[tokio::test(flavor = "multi_thread")]
    async fn crlf_framed_upstream_still_delivers_content() {
        use tower::ServiceExt;

        let _g = lock();
        let sched = init_scheduler();

        // 与 LF 用例等价的内容，但用 CRLF 作为行结束符
        const SSE: &str = concat!(
            "event: response.output_text.delta\r\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"你好\"}\r\n\r\n",
            "event: response.output_text.delta\r\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"世界\"}\r\n\r\n",
            "data: [DONE]\r\n\r\n",
        );

        let upstream = Router::new().route(
            "/v1/responses",
            post(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    SSE,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });

        sched.upsert_channel("crlf-ch", true, 4, 0, 0, 100);
        set_channel_config(
            "crlf-ch",
            ChannelConfig {
                api_key: "sk-crlf".into(),
                upstream_url: format!("http://{addr}/v1/responses"),
                upstream_format: UpstreamFormat::Responses,
                model_override: String::new(),
            },
        );

        let resp = build_router()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "m",
                            "messages": [{"role": "user", "content": "hi"}],
                            "stream": true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("你好") && body.contains("世界"),
            "CRLF 分帧的上游导致内容整段丢失（帧边界识别不到）。实际响应体：{body}"
        );

        sched.remove_channel("crlf-ch");
        remove_channel_config("crlf-ch");
    }

    /// 端到端验证**换端口成功**的完整流程（此前只测了失败路径）：
    /// 在新端口上确实能收到 HTTP 响应，且旧端口已停止监听。
    ///
    /// 用真实 HTTP 请求而不是只看 `SERVER` 是否存在——后者无法发现
    /// 「服务注册了但实际没在监听」这类问题。
    #[tokio::test(flavor = "multi_thread")]
    async fn switching_port_moves_listener_to_new_port() {
        let _g = lock();

        // restart_server 会更新 CONFIG.port，cfg() 读取它做断言
        *CONFIG.write().unwrap_or_else(|e| e.into_inner()) = Some(ProxyConfig {
            api_key: String::new(),
            model_override: String::new(),
            port: 0,
            upstream_url: String::new(),
            max_concurrency: 20,
            upstream_format: UpstreamFormat::Responses,
        });

        // 取一个空闲端口：与 restart_server 一样绑 0.0.0.0，再释放供其使用
        let pick_free_port = || {
            let l = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };
        let port_a = pick_free_port();
        let port_b = pick_free_port();

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();

        // 1. 在 port_a 启动，确认真的能提供服务
        restart_server(port_a).await.expect("port_a 应能启动");
        let r = client
            .get(format!("http://127.0.0.1:{port_a}/health"))
            .send()
            .await
            .expect("port_a 上应能收到响应");
        assert_eq!(r.status(), 200);
        assert_eq!(r.text().await.unwrap(), "ok");

        // 2. 换到 port_b
        restart_server(port_b).await.expect("换到 port_b 应成功");

        // 3. 新端口必须能服务
        let r = client
            .get(format!("http://127.0.0.1:{port_b}/health"))
            .send()
            .await
            .expect("换端口后新端口应能收到响应");
        assert_eq!(r.status(), 200);
        assert_eq!(r.text().await.unwrap(), "ok");

        // 4. 旧端口必须已经停止监听（优雅关闭是异步的，给一点时间）
        let mut still_alive = true;
        for _ in 0..20 {
            if client
                .get(format!("http://127.0.0.1:{port_a}/health"))
                .send()
                .await
                .is_err()
            {
                still_alive = false;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            !still_alive,
            "换端口后旧端口仍在监听：两个实例并存会重复计费/重复转发"
        );

        // 5. 配置里的端口也应已更新
        assert_eq!(cfg().port, port_b, "换端口后配置未同步");

        // 清理
        if let Some(h) = SERVER.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.shutdown.send(true);
        }
    }

    /// 回归保护：探针 / 余额通过 `acquire_lease()` 走同一条闸门，
    /// `try_acquire` 会 INSERT 一行；但它们从不调用 `update_last_tokens`，
    /// 于是行永久停在 `in_flight = 1` —— 不进入 token 聚合、
    /// 只能等 7 天保留策略清理、任何按该字段过滤的查询都会看到幽灵请求。
    ///
    /// 现在由 `Lease::Drop` 统一兜底收尾，调用方无需记得手动处理。
    #[test]
    fn dropped_lease_completes_its_request_row() {
        let _g = lock();
        let sched = init_scheduler();
        stats::use_memory_db_for_test();
        sched.upsert_channel("f9-ch", true, 4, 0, 0, 100);
        set_channel_config("f9-ch", mk_config("sk-9", "https://f9.example.com"));

        let before = stats::in_flight_row_count_for_test();
        let lease = test_rt().block_on(acquire_lease()).expect("应能取到租约");
        assert_eq!(
            stats::in_flight_row_count_for_test(),
            before + 1,
            "前置条件：租约应登记一行 in_flight = 1"
        );

        // 模拟探针/余额的用法：拿到租约、用完直接丢弃，不做任何手动收尾
        drop(lease);

        assert_eq!(
            stats::in_flight_row_count_for_test(),
            before,
            "租约释放后行仍停在 in_flight = 1：探针/余额会不断累积幽灵行"
        );

        sched.remove_channel("f9-ch");
        remove_channel_config("f9-ch");
    }

    /// 回归保护：流式路径曾用 `disarm_global()` 把全局守卫**提前摘掉**，
    /// 释放责任全交给 spawn 任务末尾的 `update_last_tokens`。
    /// 任务一旦 panic，就没人再递减 ACTIVE，槽位永久泄漏。
    ///
    /// 现在守卫被移入任务、保持上膛，panic 时随栈展开归还。
    /// （端到端触发 panic 没有稳定入口，这里直接验证该机制本身。）
    #[test]
    fn armed_guard_is_released_even_on_panic() {
        let _g = lock();
        stats::use_memory_db_for_test();

        let rowid = stats::try_acquire(u64::MAX).expect("应能取到槽位");
        assert!(rowid > 0, "前置条件：内存库应就绪");
        let held = stats::active();

        // 守卫上膛并被 panic 打断：Drop 必须在栈展开时归还槽位
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = stats::SlotGuard::new(rowid);
            panic!("模拟流式任务 panic");
        }));
        assert!(caught.is_err(), "前置条件：闭包应 panic");

        assert_eq!(
            stats::active(),
            held - 1,
            "panic 后全局槽位未归还：ACTIVE 只增不减，累积到上限后整个代理会假死"
        );
    }

    /// `take_global_guard` 交出的守卫必须**仍是上膛**的：
    /// 若它已被 disarm，panic 时就没人归还槽位，与修复前无异。
    #[test]
    fn taken_guard_stays_armed_until_explicitly_released() {
        let _g = lock();
        stats::use_memory_db_for_test();

        let rowid = stats::try_acquire(u64::MAX).expect("应能取到槽位");
        let held = stats::active();

        let mut lease = Lease {
            profile_id: "armed".into(),
            channel: mk_config("sk", "https://x.example.com"),
            global: Some(stats::SlotGuard::new(rowid)),
            channel_slot: ChannelSlotGuard::none(),
        };

        let guard = lease.take_global_guard().expect("应取出守卫");
        assert_eq!(stats::active(), held, "取出守卫不应立即释放槽位");

        // 显式 disarm（正常收尾路径）
        let idx = guard.disarm();
        assert_eq!(idx, rowid, "disarm 应归还原始 rowid");
        stats::update_last_tokens(idx, stats::TokenCounts::default());
        assert_eq!(stats::active(), held - 1, "正常收尾应归还一次");

        // 租约此时已不持有全局守卫，Drop 不得再减一次（否则计数下溢）
        drop(lease);
        assert_eq!(
            stats::active(),
            held - 1,
            "重复释放导致 ACTIVE 下溢：并发上限会被虚高放行"
        );
    }

    /// 回归保护：代理启动失败曾只写 stderr，而 release 构建带
    /// `windows_subsystem = "windows"`、没有控制台 —— 用户看到窗口和托盘都正常，
    /// 却没有任何监听，只能在客户端连不上时才发现。
    /// 失败原因必须留在进程内供界面读取，并在成功后清空（避免过期告警常驻）。
    #[tokio::test(flavor = "multi_thread")]
    async fn server_failure_is_recorded_and_cleared_on_success() {
        let _g = lock();

        *CONFIG.write().unwrap_or_else(|e| e.into_inner()) = Some(ProxyConfig {
            api_key: String::new(),
            model_override: String::new(),
            port: 0,
            upstream_url: String::new(),
            max_concurrency: 20,
            upstream_format: UpstreamFormat::Responses,
        });

        // 清掉可能来自其它用例的状态
        if let Some(h) = SERVER.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.shutdown.send(true);
        }
        set_last_server_error(None);

        // 占住一个端口（绑 0.0.0.0，与 restart_server 一致）
        let squatter = std::net::TcpListener::bind("0.0.0.0:0").expect("应能占住端口");
        let occupied = squatter.local_addr().unwrap().port();

        // 失败：必须记录原因，且不能谎报为在监听
        let err = restart_server(occupied).await.expect_err("被占用应失败");
        assert!(
            last_server_error().is_some_and(|m| m.contains("监听失败")),
            "启动失败未记录原因，界面无从告知用户（实际错误：{err}）"
        );
        assert!(
            !is_listening(),
            "绑定失败却报告为在监听，用户会以为代理可用"
        );

        // 成功：必须清空错误，避免过期告警常驻
        let free = {
            let l = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };
        restart_server(free).await.expect("空闲端口应成功");
        assert!(
            last_server_error().is_none(),
            "启动成功后错误未清空，界面会一直显示过期告警"
        );
        assert!(is_listening(), "启动成功后应报告为在监听");

        if let Some(h) = SERVER.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.shutdown.send(true);
        }
        set_last_server_error(None);
    }

    /// 回归保护：`restart_server` 曾「先停旧服务，再绑定新端口」。
    /// 绑定失败时旧服务已经停掉、`SERVER` 已被取空 —— 代理彻底掉线，
    /// 而调用方此时已把坏端口落盘，重启也起不来。
    ///
    /// 这里验证关键性质：**绑定失败不得影响正在运行的服务**。
    #[tokio::test(flavor = "multi_thread")]
    async fn failed_port_rebind_keeps_existing_server_alive() {
        let _g = lock();

        // 防御性清理：避免上一个用例异常退出留下的服务影响判断
        if let Some(h) = SERVER.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.shutdown.send(true);
        }

        // 占住一个端口，模拟「已被其它程序占用」。
        // 必须绑通配地址 0.0.0.0，与 restart_server 的绑定方式一致——
        // 只绑 127.0.0.1 的话，Windows 允许 0.0.0.0 的通配绑定与之共存，
        // 构不成真正的占用。
        let squatter = std::net::TcpListener::bind("0.0.0.0:0").expect("应能占住端口");
        let occupied = squatter.local_addr().unwrap().port();

        // 先在一个空闲端口上正常启动。listener 取到端口号后立即释放，
        // 以便 restart_server 能重新绑定它。
        let free = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };
        restart_server(free).await.expect("空闲端口应能启动");
        assert!(
            SERVER.lock().unwrap_or_else(|e| e.into_inner()).is_some(),
            "前置条件：服务应已在运行"
        );

        // 切到被占用的端口 → 必须失败，且不能动到正在跑的服务
        let err = restart_server(occupied)
            .await
            .expect_err("被占用的端口不应绑定成功");
        assert!(err.contains("监听失败"), "错误信息应说明绑定失败，实际：{err}");

        assert!(
            SERVER.lock().unwrap_or_else(|e| e.into_inner()).is_some(),
            "绑定失败后旧服务被停掉了：代理会彻底掉线，而调用方已把坏端口落盘，重启也起不来"
        );

        // 清理：停掉本用例启动的服务
        if let Some(h) = SERVER.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.shutdown.send(true);
        }
    }

    /// 流式语义：`disarm_global()` 摘除全局槽位（其释放移交给 `update_last_tokens`），
    /// 但渠道槽位仍必须在任务结束时归还，否则长连接一旦结束就漏一个槽位。
    #[test]
    fn disarmed_lease_still_releases_channel_slot() {
        let _g = lock();
        let sched = init_scheduler();
        sched.upsert_channel("d-ch", true, 4, 0, 0, 100);
        assert!(sched.acquire_slot("d-ch"));

        let mut lease = Lease {
            profile_id: "d-ch".into(),
            channel: mk_config("sk-d", "https://d.example.com"),
            // idx=5 仅用于验证「摘除后确实交还了 rowid」。
            // disarm 只清 armed 标志，不触碰 ACTIVE 计数器，故不会污染全局计数。
            global: Some(stats::SlotGuard::new(5)),
            channel_slot: ChannelSlotGuard::new("d-ch".into()),
        };
        assert_eq!(
            lease.disarm_global(),
            5,
            "disarm_global 应摘除并交还全局槽位 rowid，否则 Drop 会与 update_last_tokens 重复递减"
        );

        drop(lease);
        assert_eq!(
            in_flight(sched, "d-ch"),
            0,
            "流式任务结束后渠道槽位未归还，渠道会被逐步占死"
        );

        sched.remove_channel("d-ch");
    }

    /// 回归保护：探针 / 余额的租约曾锁定 `first_enabled_id()`，首个渠道一旦饱和，
    /// 即便其它渠道完全空闲，也会空等 120 秒后报「并发已满」——用户看到
    /// 「连接测试失败」，而实际上游健康。
    #[test]
    fn probe_lease_falls_back_to_an_available_channel() {
        let _g = lock();
        let sched = init_scheduler();

        // pin-a 容量 1 且已被占满；pin-b 空闲
        sched.upsert_channel("pin-a", true, 1, 0, 0, 100);
        sched.upsert_channel("pin-b", true, 5, 0, 0, 100);
        // 注册凭据，避免走到读全局 CONFIG 的降级分支
        set_channel_config("pin-b", mk_config("sk-b", "https://b.example.com"));
        assert!(sched.acquire_slot("pin-a"), "前置条件：占满 pin-a");

        let lease = test_rt()
            .block_on(acquire_lease())
            .expect("首个渠道饱和时，探针应能改用其它可用渠道，而不是空等 120 秒");
        assert_eq!(
            lease.profile_id, "pin-b",
            "租约锁定在了饱和渠道上，探针会一直失败"
        );

        drop(lease);
        sched.remove_channel("pin-a");
        sched.remove_channel("pin-b");
        remove_channel_config("pin-b");
    }

    /// 全部渠道禁用时，探针应立刻拿到可读原因，而不是空等 120 秒
    #[test]
    fn probe_lease_reports_disabled_channels_immediately() {
        let _g = lock();
        let sched = init_scheduler();
        sched.upsert_channel("probe-off", false, 10, 0, 0, 100);

        let err = test_rt()
            .block_on(acquire_lease())
            .err()
            .expect("全禁用时不应拿到租约");
        assert!(err.contains("禁用"), "错误信息应说明渠道被禁用，实际：{err}");

        sched.remove_channel("probe-off");
    }

    /// 渠道凭据按 profile_id 隔离，互不串用
    #[test]
    fn channel_configs_are_isolated_per_profile() {
        let _g = lock();
        set_channel_config("iso-a", mk_config("sk-a", "https://a.example.com"));
        set_channel_config("iso-b", mk_config("sk-b", "https://b.example.com"));

        assert_eq!(resolve_channel("iso-a").api_key, "sk-a");
        assert_eq!(resolve_channel("iso-b").upstream_url, "https://b.example.com");

        // 移除后凭据应消失，请求不再可能路由到已删除的渠道
        remove_channel_config("iso-a");
        assert!(channel_config("iso-a").is_none());
        assert_eq!(channel_config("iso-b").map(|c| c.api_key), Some("sk-b".into()));

        remove_channel_config("iso-b");
    }
}

#[cfg(test)]
mod body_limit_tests {
    use super::*;
    use tower::ServiceExt;

    /// 回归保护：axum 对 Json 提取器默认只放行 2MB，长上下文请求会被本地 413 拦下。
    /// build_router 必须放宽该上限，这里用 3MB 请求体验证不再被拦截。
    #[tokio::test]
    async fn accepts_body_larger_than_axum_default_two_mb() {
        let _g = test_lock();
        // 上游地址留空 → handler 立即返回配置错误，不会发起真实网络请求
        *CONFIG.write().unwrap_or_else(|e| e.into_inner()) = Some(ProxyConfig {
            api_key: "test".into(),
            model_override: String::new(),
            port: 0,
            upstream_url: String::new(),
            max_concurrency: 20,
            upstream_format: UpstreamFormat::Responses,
        });

        let payload = json!({
            "model": "test",
            "messages": [{ "role": "user", "content": "x".repeat(3 * 1024 * 1024) }]
        })
        .to_string();
        assert!(payload.len() > 2 * 1024 * 1024, "测试样本需大于 axum 默认上限");

        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(payload))
            .unwrap();

        let response = build_router().oneshot(request).await.unwrap();
        assert_ne!(
            response.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "3MB 请求体被本地 body limit 拦截，MAX_REQUEST_BODY_BYTES 未生效"
        );
    }
}
