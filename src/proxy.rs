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
}

static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();
static CONFIG: std::sync::RwLock<Option<ProxyConfig>> = std::sync::RwLock::new(None);

struct ServerHandle {
    shutdown: tokio::sync::watch::Sender<bool>,
    _join: tauri::async_runtime::JoinHandle<()>,
}

static SERVER: std::sync::Mutex<Option<ServerHandle>> = std::sync::Mutex::new(None);

/// 并发上限：超过后排队等待而不是返回 429
const MAX_CONCURRENCY: usize = 20;

/// 流式读取的绝对超时：防止上游流一直不停导致并发槽位被永久占用
const STREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

pub fn init(handle: tauri::AppHandle, cfg: ProxyConfig) {
    let _ = APP_HANDLE.set(handle);
    *CONFIG.write().unwrap_or_else(|e| e.into_inner()) = Some(cfg);
}

pub fn cfg() -> ProxyConfig {
    CONFIG
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .expect("proxy config not initialized")
}

/// 运行时热更新 Key / 模型名 / 上游地址 / 并发数，立即对后续请求生效，无需重启服务
pub fn update_runtime(
    api_key: Option<String>,
    model_override: Option<String>,
    upstream_url: Option<String>,
    max_concurrency: Option<usize>,
) {
    let mut c = CONFIG.write().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = c.as_mut() {
        if let Some(k) = api_key {
            c.api_key = k;
        }
        if let Some(m) = model_override {
            c.model_override = m;
        }
        if let Some(u) = upstream_url {
            c.upstream_url = u;
        }
        if let Some(mc) = max_concurrency {
            c.max_concurrency = mc;
        }
    }
}

/// 当前实际使用的上游地址（空配置时返回空字符串）
pub fn upstream_url() -> String {
    cfg().upstream_url.trim().to_string()
}

/// 获取 AppHandle（供 main.rs 调用事件通知）
pub fn app_handle() -> Option<&'static tauri::AppHandle> {
    APP_HANDLE.get()
}

/// 停掉旧服务（如有），在新端口重新绑定并监听。
/// 返回 Err 表示端口绑定失败（如被占用），此时旧服务已停止。
pub async fn restart_server(new_port: u16) -> Result<(), String> {
    // 1. 停止旧实例，等待优雅退出释放端口
    //    先把锁作用域结束，避免 MutexGuard 跨 await 导致 future 非 Send
    let existing = SERVER.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(handle) = existing {
        let _ = handle.shutdown.send(true);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // 2. 先绑定再注册：绑定失败直接报错给前端
    let app = build_router();
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], new_port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("端口 {} 监听失败：{e}", new_port))?;

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

fn chat_to_responses_payload(chat_body: &Value, stream: bool) -> Value {
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
    if !cfg().model_override.is_empty() {
        payload["model"] = json!(cfg().model_override);
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
fn anthropic_to_responses_payload(body: &Value, stream: bool) -> Value {
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
    if !cfg().model_override.is_empty() {
        payload["model"] = json!(cfg().model_override);
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

        while let Some(pos) = find_double_newline(&buf) {
            let event_bytes: Vec<u8> = buf.drain(..pos + 2).collect();
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

fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
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

        while let Some(pos) = find_double_newline(&buf) {
            let event_bytes: Vec<u8> = buf.drain(..pos + 2).collect();
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

// ---------- 上游请求（含重试）----------

/// 软并发调度：最多等待 120 秒获取一个并发槽位；返回 None 表示等待超时。
/// 供 HTTP 代理与探针 / 余额查询共用，保证所有打上游的请求都计入并发统计。
pub(crate) async fn acquire_slot() -> Option<stats::SlotGuard> {
    let max = cfg().max_concurrency;
    let mut waited = false;
    for _ in 0..600 {
        if let Some(id) = stats::try_acquire(max as u64) {
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

/// 软并发调度：使用配置中的 max_concurrency，超过时先等待。
/// 最长等待 120 秒；超时后返回 429。
async fn wait_for_slot() -> Result<stats::SlotGuard, Response> {
    match acquire_slot().await {
        Some(g) => Ok(g),
        None => Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": {
                    "message": format!("并发请求已达上限（{}），排队等待 2 分钟仍未获取到槽位，请稍后重试", MAX_CONCURRENCY),
                    "type": "concurrency_limit_exceeded",
                }
            })),
        )
            .into_response()),
    }
}

async fn send_upstream(payload: &Value, user_agent: &str) -> Result<reqwest::Response, (StatusCode, Value)> {
    // 空 Key 直接拒绝，避免打到上游才收到难懂的 401
    if cfg().api_key.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": {"message": "API Key 未配置：请在设置面板填写，或写入 token-monitor.json 的 api_key 字段", "type": "proxy_config_error"}}),
        ));
    }

    // 空上游地址拒绝
    if upstream_url().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": {"message": "转发目标地址未配置：请在设置面板填写，或写入 token-monitor.json 的 upstream_url 字段", "type": "proxy_config_error"}}),
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
            .post(upstream_url())
            .bearer_auth(cfg().api_key.trim())
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
async fn aggregate_chat_completion(upstream: reqwest::Response, model: String) -> (Value, u64, stats::TokenCounts) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
    let (usage_tx, usage_rx) = tokio::sync::oneshot::channel::<stats::TokenCounts>();
    let model_producer = model.clone();
    tokio::spawn(async move {
        let (_, tool_used, usage) =
            convert_stream(upstream.bytes_stream(), model_producer.clone(), &tx, None).await;
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

/// 模式 1：OpenAI 传统 Chat Completions（/v1/chat/completions）
async fn chat_completions(
    State(_): State<()>,
    headers: axum::http::HeaderMap,
    Json(chat_body): Json<Value>,
) -> Response {
    let user_agent = request_user_agent(&headers);
    let wants_stream = chat_body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let payload = chat_to_responses_payload(&chat_body, wants_stream);
    let model = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    // 排队获取并发槽位（超时 120s 返回 429）
    let guard = match wait_for_slot().await {
        Ok(g) => g,
        Err(resp) => return resp,
    };

    let upstream = match send_upstream(&payload, &user_agent).await {
        Ok(r) => r,
        Err((status, body)) => return (status, Json(body)).into_response(),
    };

    if !upstream.status().is_success() {
        return upstream_error_response(upstream).await;
    }

    if !wants_stream {
        let (cc, chars, upstream_usage) = aggregate_chat_completion(upstream, model).await;
        let tc = if upstream_usage.input > 0 || upstream_usage.output > 0 {
            upstream_usage
        } else {
            stats::TokenCounts { input: 0, output: chars.div_ceil(3), cached: 0 }
        };
        stats::update_tokens_db_only(guard.idx(), tc);
        guard.release();
        if let Some(h) = APP_HANDLE.get() {
            let _ = h.emit("stats-updated", ());
        }
        return (StatusCode::OK, Json(cc)).into_response();
    }

    // 流式：边读边转换，实时更新 token 数
    let byte_stream = upstream.bytes_stream();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
    let idx = guard.disarm();

    tokio::spawn(async move {
        let first = make_chunk(&model, json!({"role": "assistant"}), None);
        let _ = tx.send(Ok(sse_frame(&first))).await;

        let cb: Box<dyn Fn(u64) + Send> = Box::new(move |chars: u64| {
            stats::update_tokens_db_only(idx, stats::TokenCounts {
                input: 0, output: chars.div_ceil(3), cached: 0,
            });
            if let Some(h) = APP_HANDLE.get() {
                let _ = h.emit("stats-updated", ());
            }
        });
        let result = tokio::time::timeout(
            STREAM_TIMEOUT,
            convert_stream(byte_stream, model.clone(), &tx, Some(cb)),
        )
        .await;
        let (chars, tool_used, upstream_usage) = match result {
            Ok(v) => v,
            Err(_) => {
                eprintln!("[proxy] 流式超时，强制释放并发槽位");
                (0, false, stats::TokenCounts { input: 0, output: 0, cached: 0 })
            }
        };

        let finish = make_chunk(&model, json!({}), Some(if tool_used { "tool_calls" } else { "stop" }));
        let _ = tx.send(Ok(sse_frame(&finish))).await;
        let _ = tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await;
        drop(tx);

        let tc = if upstream_usage.input > 0 || upstream_usage.output > 0 {
            upstream_usage
        } else {
            stats::TokenCounts { input: 0, output: chars.div_ceil(3), cached: 0 }
        };
        stats::update_last_tokens(idx, tc);
        if let Some(h) = APP_HANDLE.get() {
            let _ = h.emit("stats-updated", ());
        }
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
    // 强制模型名仍然生效
    if !cfg().model_override.is_empty() {
        body["model"] = json!(cfg().model_override);
    }

    let guard = match wait_for_slot().await {
        Ok(g) => g,
        Err(resp) => return resp,
    };

    let upstream = match send_upstream(&body, &user_agent).await {
        Ok(r) => r,
        Err((status, err)) => return (status, Json(err)).into_response(),
    };

    if !upstream.status().is_success() {
        return upstream_error_response(upstream).await;
    }

    if !wants_stream {
        let content_type = upstream
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_string();
        let full = upstream.text().await.unwrap_or_default();
        let tc = match serde_json::from_str::<Value>(&full) {
            Ok(v) => {
                let input = v.pointer("/usage/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                let output = v.pointer("/usage/output_tokens").and_then(|t| t.as_u64())
                    .unwrap_or_else(|| estimate_response_output_chars(&v).div_ceil(3));
                let cached = v.pointer("/usage/cached_tokens").and_then(|t| t.as_u64())
                    .or_else(|| v.pointer("/usage/input_tokens_details/cached_tokens").and_then(|t| t.as_u64()))
                    .unwrap_or(0);
                stats::TokenCounts { input, output, cached }
            }
            Err(_) => stats::TokenCounts { input: 0, output: 0, cached: 0 },
        };
        stats::update_tokens_db_only(guard.idx(), tc);
        guard.release();
        if let Some(h) = APP_HANDLE.get() {
            let _ = h.emit("stats-updated", ());
        }
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", content_type)
            .body(Body::from(full))
            .unwrap();
    }

    // 流式：字节原样透传，同时旁路统计 output_text.delta 的字符数
    let byte_stream = upstream.bytes_stream();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
    let idx = guard.disarm();
    tokio::spawn(async move {
        let mut stream = byte_stream;
        let mut pending: Vec<u8> = Vec::new();
        let mut chars: u64 = 0;
        let mut usage = stats::TokenCounts { input: 0, output: 0, cached: 0 };
        loop {
            let next = match tokio::time::timeout(STREAM_TIMEOUT, stream.next()).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(_) => {
                    eprintln!("[proxy] Responses 流式超时，强制释放并发槽位");
                    break;
                }
            };
            let Ok(bytes) = next else { break };
            if tx.send(Ok(bytes.to_vec())).await.is_err() {
                break;
            }
            pending.extend_from_slice(&bytes);
            while let Some(pos) = find_double_newline(&pending) {
                let event_bytes: Vec<u8> = pending.drain(..pos + 2).collect();
                let text = String::from_utf8_lossy(&event_bytes);
                if let Some(data_line) = text.lines().rev().find(|l| l.trim_start().starts_with("data:")) {
                    let payload = data_line.trim_start()["data:".len()..].trim();
                    if let Ok(evt) = serde_json::from_str::<Value>(payload) {
                        let evt_type = evt.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if evt_type == "response.output_text.delta" {
                            chars += evt
                                .get("delta")
                                .and_then(|d| d.as_str())
                                .map(|s| s.chars().count() as u64)
                                .unwrap_or(0);
                            stats::update_tokens_db_only(idx, stats::TokenCounts {
                                input: 0, output: chars.div_ceil(3), cached: 0,
                            });
                            if let Some(h) = APP_HANDLE.get() {
                                let _ = h.emit("stats-updated", ());
                            }
                        } else if evt_type == "response.completed" {
                            if let Some(u) = evt.pointer("/response/usage") {
                                usage.input = u.pointer("/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                                usage.output = u.pointer("/output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                                usage.cached = u.pointer("/cached_tokens").and_then(|t| t.as_u64())
                                    .or_else(|| u.pointer("/input_tokens_details/cached_tokens").and_then(|t| t.as_u64()))
                                    .unwrap_or(0);
                            }
                        }
                    }
                }
            }
        }
        drop(tx);
        let tc = if usage.input > 0 || usage.output > 0 {
            usage
        } else {
            stats::TokenCounts { input: 0, output: chars.div_ceil(3), cached: 0 }
        };
        stats::update_last_tokens(idx, tc);
        if let Some(h) = APP_HANDLE.get() {
            let _ = h.emit("stats-updated", ());
        }
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
    let payload = anthropic_to_responses_payload(&body, wants_stream);
    let model = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    let guard = match wait_for_slot().await {
        Ok(g) => g,
        Err(resp) => return resp,
    };
    let upstream = match send_upstream(&payload, &user_agent).await {
        Ok(r) => r,
        Err((status, err)) => return (status, Json(err)).into_response(),
    };

    if !upstream.status().is_success() {
        return upstream_error_response(upstream).await;
    }

    if !wants_stream {
        let (cc, chars, upstream_usage) = aggregate_chat_completion(upstream, model).await;
        let tc = if upstream_usage.input > 0 || upstream_usage.output > 0 {
            upstream_usage
        } else {
            stats::TokenCounts { input: 0, output: chars.div_ceil(3), cached: 0 }
        };
        stats::update_tokens_db_only(guard.idx(), tc.clone());
        guard.release();
        if let Some(h) = APP_HANDLE.get() {
            let _ = h.emit("stats-updated", ());
        }
        return (StatusCode::OK, Json(chat_completion_to_anthropic(&cc, &tc))).into_response();
    }

    // 流式：转换为 Anthropic SSE 事件序列，实时更新 token 数
    let byte_stream = upstream.bytes_stream();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
    let idx = guard.disarm();
    tokio::spawn(async move {
        let cb: Box<dyn Fn(u64) + Send> = Box::new(move |chars: u64| {
            stats::update_tokens_db_only(idx, stats::TokenCounts {
                input: 0, output: chars.div_ceil(3), cached: 0,
            });
            if let Some(h) = APP_HANDLE.get() {
                let _ = h.emit("stats-updated", ());
            }
        });
        let result = tokio::time::timeout(
            STREAM_TIMEOUT,
            convert_stream_anthropic(byte_stream, model.clone(), &tx, Some(cb)),
        )
        .await;
        let (chars, upstream_usage) = match result {
            Ok(v) => v,
            Err(_) => {
                eprintln!("[proxy] Anthropic 流式超时，强制释放并发槽位");
                (0, stats::TokenCounts { input: 0, output: 0, cached: 0 })
            }
        };
        drop(tx);
        let tc = if upstream_usage.input > 0 || upstream_usage.output > 0 {
            upstream_usage
        } else {
            stats::TokenCounts { input: 0, output: chars.div_ceil(3), cached: 0 }
        };
        stats::update_last_tokens(idx, tc);
        if let Some(h) = APP_HANDLE.get() {
            let _ = h.emit("stats-updated", ());
        }
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

#[cfg(test)]
mod body_limit_tests {
    use super::*;
    use tower::ServiceExt;

    /// 回归保护：axum 对 Json 提取器默认只放行 2MB，长上下文请求会被本地 413 拦下。
    /// build_router 必须放宽该上限，这里用 3MB 请求体验证不再被拦截。
    #[tokio::test]
    async fn accepts_body_larger_than_axum_default_two_mb() {
        // 上游地址留空 → handler 立即返回配置错误，不会发起真实网络请求
        *CONFIG.write().unwrap_or_else(|e| e.into_inner()) = Some(ProxyConfig {
            api_key: "test".into(),
            model_override: String::new(),
            port: 0,
            upstream_url: String::new(),
            max_concurrency: 20,
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
