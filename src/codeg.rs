//! Codeg server 连接：配置持久化、`POST /api/*` Bearer 调用、会话快照聚合，
//! 以及 Windows 下 Node/agent 进程与系统资源统计。
//!
//! Codeg server 的真实数据接口（来自 codeg 源码 `web/router.rs` / `handlers/acp.rs`）：
//! - `POST /api/health`                   检查服务与 Token
//! - `POST /api/acp_list_connections`     返回活跃 AgentConnection 列表
//! - `POST /api/acp_get_session_snapshot` 返回单个活跃连接的状态快照
//! - `POST /api/acp_cancel`               停止当前 turn（保留连接）
//! - `POST /api/acp_prompt`               向连接发送用户输入
//! - `POST /api/acp_disconnect`           断开连接（停止 agent 进程）
//!
//! `ConnectionStatus` 序列化为 snake_case：
//! `connecting` / `connected` / `prompting` / `disconnected` / `error`。
//! `prompting` / `connecting` 表示 turn 正在运行；等待输入以 snapshot 中的
//! `pending_question` / `pending_permission` / `pending_plan_approval` 为准。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const CODEG_DEFAULT_PORT: u16 = 3080;
const DEFAULT_POLL_SECS: u64 = 5;
const DEFAULT_INACTIVITY_TIMEOUT_SECS: u64 = 120;
#[cfg(target_os = "windows")]
const AGENT_PROCESS_NAMES: &[&str] = &[
    "claude.exe",
    "pi.exe",
    "opencode.exe",
    "codex.exe",
    "gemini.exe",
    "grok.exe",
    "hermes.exe",
    "cline.exe",
    "antigravity.exe",
    "openclaw.exe",
    "freebuff.exe",
];

#[cfg(not(target_os = "windows"))]
const AGENT_PROCESS_NAMES: &[&str] = &[
    "claude",
    "pi",
    "opencode",
    "codex",
    "gemini",
    "grok",
    "hermes",
    "cline",
    "antigravity",
    "openclaw",
    "freebuff",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodegConfig {
    pub enabled: bool,
    pub server_url: String,
    pub token: String,
    /// 自动处理“活跃但长时间无输出”的会话：停止当前 turn 后重新发送继续提示。
    pub auto_recovery: bool,
    /// 不活跃判定阈值（秒），默认 120 秒。
    pub inactivity_timeout_secs: u64,
}

impl Default for CodegConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_url: format!("http://127.0.0.1:{CODEG_DEFAULT_PORT}"),
            token: String::new(),
            auto_recovery: true,
            inactivity_timeout_secs: DEFAULT_INACTIVITY_TIMEOUT_SECS,
        }
    }
}

impl CodegConfig {
    pub fn configured(&self) -> bool {
        !self.server_url.trim().is_empty() && !self.token.trim().is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodegSessionSummary {
    pub connection_id: String,
    pub status: String,
    pub agent_type: String,
    pub session_name: String,
    /// ACP live_message 中最新文本，作为标题不足时的会话内容摘要。
    pub latest_reply: String,
    /// 等待输入原因。Codeg 的 snapshot 中可能为 question / permission / plan_approval。
    pub waiting_for: Option<String>,
    pub idle_secs: u64,
    pub last_activity_at: Option<i64>,
    pub automatic_restart_count: u64,
    pub last_restart_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SystemStats {
    pub cpu_percent: f64,
    pub memory_used_gb: f64,
    pub memory_total_gb: f64,
    pub memory_percent: f64,
    pub gpu_used_gb: Option<f64>,
    pub gpu_total_gb: Option<f64>,
    pub gpu_percent: Option<f64>,
    pub c_drive_used_gb: f64,
    pub c_drive_total_gb: f64,
    pub c_drive_free_gb: f64,
    pub c_drive_percent: f64,
    pub node_processes: u64,
    pub agent_processes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodegStatus {
    pub connected: bool,
    pub error: Option<String>,
    pub session_count: u64,
    pub running_count: u64,
    pub stopped_count: u64,
    pub error_count: u64,
    pub waiting_input_count: u64,
    pub active_session_name: Option<String>,
    pub sessions: Vec<CodegSessionSummary>,
    pub system: SystemStats,
    /// 最近一次自动恢复执行结果，便于前端和日志诊断。
    pub recovery_events: Vec<String>,
    pub auto_recovery_enabled: bool,
    pub inactivity_timeout_secs: u64,
}

static CONFIG: OnceLock<std::sync::RwLock<CodegConfig>> = OnceLock::new();

fn config() -> std::sync::RwLockReadGuard<'static, CodegConfig> {
    CONFIG
        .get_or_init(|| std::sync::RwLock::new(CodegConfig::default()))
        .read()
        .unwrap_or_else(|e| e.into_inner())
}

pub fn update_config(cfg: CodegConfig) -> CodegConfig {
    *CONFIG
        .get_or_init(|| std::sync::RwLock::new(CodegConfig::default()))
        .write()
        .unwrap_or_else(|e| e.into_inner()) = cfg.clone();
    cfg
}

/// 从 token-monitor.json 读取并初始化 Codeg 配置；未配置时使用默认值，不阻塞启动。
pub fn load_from_json(v: &serde_json::Value) -> CodegConfig {
    let mut cfg = CodegConfig::default();
    if let Some(o) = v.get("codeg") {
        cfg.enabled = o.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false);
        cfg.server_url = o
            .get("server_url")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        cfg.token = o
            .get("token")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        cfg.auto_recovery = o
            .get("auto_recovery")
            .and_then(|x| x.as_bool())
            .unwrap_or(true);
        cfg.inactivity_timeout_secs = o
            .get("inactivity_timeout_secs")
            .and_then(|x| x.as_u64())
            .unwrap_or(DEFAULT_INACTIVITY_TIMEOUT_SECS)
            .max(10);
    }
    if cfg.server_url.is_empty() {
        cfg.server_url = format!("http://127.0.0.1:{CODEG_DEFAULT_PORT}");
    }
    update_config(cfg.clone());
    cfg
}

pub fn save_to_json(v: &mut serde_json::Value, cfg: &CodegConfig) {
    v["codeg"] = serde_json::json!({
        "enabled": cfg.enabled,
        "server_url": cfg.server_url.trim(),
        "token": cfg.token,
        "auto_recovery": cfg.auto_recovery,
        "inactivity_timeout_secs": cfg.inactivity_timeout_secs,
    });
}

async fn post_api<T: Serialize + ?Sized>(
    command: &str,
    body: &T,
) -> Result<serde_json::Value, String> {
    let server_url = config().server_url.trim().to_string();
    let token = config().token.trim().to_string();
    if server_url.is_empty() {
        return Err("Codeg 服务器地址未配置".into());
    }
    if token.is_empty() {
        return Err("Codeg 服务器 Token 未配置".into());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| format!("Codeg HTTP 客户端创建失败：{e}"))?;
    let url = {
        let base = server_url.trim_end_matches('/');
        format!("{base}/api/{command}")
    };
    let resp = client
        .post(url)
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("Codeg 请求失败：{e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        serde_json::from_str(&text)
            .map_err(|e| format!("Codeg 返回不是有效 JSON：{e}（{}）", text.trim()))
    } else if status.as_u16() == 401 {
        Err(format!("Codeg Token 无效或未授权（HTTP 401）：{}", text.trim()))
    } else {
        Err(format!(
            "Codeg 接口错误（HTTP {}）：{}",
            status.as_u16(),
            text.trim()
        ))
    }
}

#[derive(Deserialize, Clone)]
#[allow(dead_code)]
struct CodegConnection {
    id: String,
    agent_type: String,
    status: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    session_name: Option<String>,
    #[serde(default)]
    external_id: Option<String>,
}

#[derive(Deserialize, Clone, Default)]
#[allow(dead_code)]
struct CodegSnapshot {
    #[serde(default)]
    external_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    agent_type: Option<String>,
    #[serde(default)]
    pending_question: Option<serde_json::Value>,
    #[serde(default)]
    pending_permission: Option<serde_json::Value>,
    #[serde(default)]
    pending_plan_approval: Option<serde_json::Value>,
    #[serde(default)]
    live_message: Option<LiveMessageInfo>,
    #[serde(default)]
    usage: Option<UsageInfo>,
    // 补充字段：Codeg snapshot 可能返回的会话名称相关字段
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    session_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    project_path: Option<String>,
}

#[derive(Deserialize, Clone, Default)]
struct LiveMessageInfo {
    #[serde(default)]
    content: Vec<LiveContentBlock>,
}

#[derive(Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LiveContentBlock {
    Text { text: String },
    Thinking { text: String },
    #[serde(other)]
    Other,
}

impl LiveMessageInfo {
    fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                LiveContentBlock::Text { text } => Some(text.clone()),
                LiveContentBlock::Thinking { text } => Some(text.clone()),
                LiveContentBlock::Other => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }
}

#[derive(Deserialize, Clone, Default)]
struct UsageInfo {
    #[serde(default)]
    used: Option<u64>,
}

fn is_running(status: &str) -> bool {
    matches!(status, "prompting" | "connecting")
}

fn is_stopped(status: &str) -> bool {
    status == "disconnected"
}

fn is_error(status: &str) -> bool {
    status == "error"
}

fn waiting_label(snap: &CodegSnapshot) -> Option<String> {
    if snap.pending_question.is_some() {
        return Some("question".into());
    }
    if snap.pending_permission.is_some() {
        return Some("permission".into());
    }
    if snap.pending_plan_approval.is_some() {
        return Some("plan_approval".into());
    }
    None
}

/// 每连接最后观测到“活动”（usage 或 live turn）的时间。
static LAST_ACTIVITY: OnceLock<Mutex<HashMap<String, (Instant, u64)>>> = OnceLock::new();

/// 自动恢复扫描限频：最短间隔 60 秒（与请求频率保持一致再收敛为可配置）。
const RECOVERY_MIN_INTERVAL: Duration = Duration::from_secs(60);

fn last_activity_map() -> &'static Mutex<HashMap<String, (Instant, u64)>> {
    LAST_ACTIVITY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 上次执行自动恢复的时间点。`None` 表示从未执行过。
static LAST_RECOVERY_RUN: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

fn should_run_recovery() -> bool {
    let mut guard = LAST_RECOVERY_RUN
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let should = match *guard {
        Some(last) => now.duration_since(last) >= RECOVERY_MIN_INTERVAL,
        None => true,
    };
    if should {
        *guard = Some(now);
    }
    should
}

#[derive(Deserialize, Clone, Default)]
#[allow(dead_code)]
struct CodegConversationTitle {
    #[serde(default)]
    id: Option<i32>,
    #[serde(default)]
    external_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    session_name: Option<String>,
}

async fn fetch_conversation_titles() -> HashMap<String, String> {
    let mut titles = HashMap::new();
    if let Ok(v) = post_api(
        "list_all_conversations",
        &serde_json::json!({ "includeChildren": false }),
    )
    .await
    {
        let rows: Vec<CodegConversationTitle> =
            serde_json::from_value(v).unwrap_or_default();
        for row in rows {
            let key = row.external_id.clone().unwrap_or_default();
            let title = row
                .title
                .or(row.name)
                .or(row.session_name)
                .filter(|s| !s.trim().is_empty());
            if !key.is_empty() {
                if let Some(t) = title {
                    titles.insert(key, t);
                }
            } else if let Some(id) = row.id {
                if let Some(t) = title {
                    titles.insert(id.to_string(), t);
                }
            }
        }
    }
    titles
}

async fn fetch_session_summaries(
    recover: bool,
    events: &mut Vec<String>,
) -> Result<Vec<CodegSessionSummary>, String> {
    let raw_conns = post_api("acp_list_connections", &serde_json::json!({})).await?;
    let conns: Vec<CodegConnection> =
        serde_json::from_value(raw_conns)
            .map_err(|e| format!("解析 Codeg 连接列表失败：{e}"))?;

    let title_map = fetch_conversation_titles().await;
    let recovery_now = recover && should_run_recovery();
    let mut summaries = Vec::with_capacity(conns.len());
    for c in conns {
        // 1. 先获取 snapshot（拿到 external_id 用于匹配标题）
        let mut snap = CodegSnapshot::default();
        let mut summary = CodegSessionSummary {
            connection_id: c.id.clone(),
            status: c.status.clone(),
            agent_type: c.agent_type.clone(),
            session_name: String::new(),
            latest_reply: String::new(),
            waiting_for: None,
            idle_secs: 0,
            last_activity_at: None,
            automatic_restart_count: 0,
            last_restart_reason: None,
        };
        if let Ok(v) = post_api(
            "acp_get_session_snapshot",
            &serde_json::json!({ "connectionId": summary.connection_id }),
        )
        .await
        {
            if let Ok(s) = serde_json::from_value::<CodegSnapshot>(v) {
                snap = s;
                summary.latest_reply = snap.live_message.as_ref().map(|m| m.text()).unwrap_or_default();
                if let Some(status) = snap.status.as_deref() {
                    summary.status = status.to_string();
                }
                summary.waiting_for = waiting_label(&snap);
            }
        }

        // 2. 用 snapshot.external_id 匹配 list_all_conversations 的标题
        summary.session_name = snap.external_id.as_deref()
            .and_then(|eid| title_map.get(eid))
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| summary.agent_type.clone());

        // 错误状态：每 60 秒检查一次，自动重试（先 cancel 后 prompt）。
        // 运行/连接中无输出：按不活跃阈值触发同样恢复。
        // 仅在 Codeg API 正常（2xx）且到了恢复检查窗口时才做。
        if recovery_now && summary.waiting_for.is_none() {
            let error_reason = if is_error(&summary.status) {
                Some(format!("Codeg 会话进入错误状态（{}）", summary.status))
            } else if is_stopped(&summary.status) {
                Some(format!("Codeg 会话已停止（{}），尝试重启", summary.status))
            } else {
                None
            };

            let active_now = snap
                .usage
                .as_ref()
                .and_then(|u| u.used)
                .unwrap_or(0)
                > 0
                || snap.live_message.is_some();
            let threshold = config().inactivity_timeout_secs.max(10);
            let should_restart = if error_reason.is_some() {
                true
            } else if is_running(&summary.status) {
                // 锁只在该同步块内使用，避免 std MutexGuard 跨 await。
                let mut map = last_activity_map().lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                if active_now {
                    map.insert(summary.connection_id.clone(), (now, 0));
                    summary.idle_secs = 0;
                    summary.last_activity_at = None;
                    false
                } else {
                    let entry = map
                        .entry(summary.connection_id.clone())
                        .or_insert_with(|| (now, 0));
                    entry.1 += 1; // 每轮轮询约 5 秒，用轮次数近似累计不活跃秒数
                    summary.idle_secs = entry.1 * DEFAULT_POLL_SECS;
                    summary.last_activity_at = Some(chrono::Utc::now().timestamp_millis());
                    entry.1 * DEFAULT_POLL_SECS >= threshold
                }
            } else {
                false
            };

            if should_restart {
                let reason = error_reason.unwrap_or_else(|| {
                    format!(
                        "活跃会话 {} 秒无输出，触发自动停止并重启",
                        summary.idle_secs
                    )
                });
                match session_action("restart", &summary.connection_id, Some("继续")).await {
                    Ok(_) => {
                        eprintln!("[codeg] 重启成功：{} (reason={})", summary.connection_id, reason);
                        last_activity_map()
                            .lock()
                            .unwrap()
                            .insert(summary.connection_id.clone(), (Instant::now(), 0));
                        summary.automatic_restart_count += 1;
                        summary.last_restart_reason = Some(reason.clone());
                        events.push(format!("[{}] {reason}", summary.connection_id));
                    }
                    Err(e) => {
                        events.push(format!(
                            "[{}] 自动恢复失败：{e}",
                            summary.connection_id
                        ));
                    }
                }
            }
        }
        summaries.push(summary);
    }
    Ok(summaries)
}

pub async fn codeg_status() -> Result<CodegStatus, String> {
    let enabled = config().enabled;
    let configured = config().configured();
    let auto_recovery = config().auto_recovery;
    let inactivity_timeout_secs = config().inactivity_timeout_secs;
    if !enabled {
        return Err("Codeg 行显示已关闭".into());
    }
    if !configured {
        return Err("Codeg 服务器地址或 Token 未配置".into());
    }

    let mut events = Vec::new();
    let summaries = fetch_session_summaries(auto_recovery, &mut events).await?;
    let running = summaries.iter().filter(|s| is_running(&s.status)).count() as u64;
    let stopped = summaries.iter().filter(|s| is_stopped(&s.status)).count() as u64;
    let error_count = summaries.iter().filter(|s| is_error(&s.status)).count() as u64;
    let waiting = summaries.iter().filter(|s| s.waiting_for.is_some()).count() as u64;
    let active_session_name = summaries
        .iter()
        .find(|s| is_running(&s.status))
        .or_else(|| summaries.iter().find(|s| s.waiting_for.is_some()))
        .map(|s| {
            if s.session_name.is_empty() {
                s.agent_type.clone()
            } else {
                s.session_name.clone()
            }
        })
        .or_else(|| Some("(无活跃会话)".into()));

    Ok(CodegStatus {
        connected: true,
        error: None,
        session_count: summaries.len() as u64,
        running_count: running,
        stopped_count: stopped,
        error_count,
        waiting_input_count: waiting,
        active_session_name,
        sessions: summaries,
        // 异步轮询每 5 秒调用一次，放阻塞线程避免 PowerShell/CLI 采样阻塞 runtime。
        system: tokio::task::spawn_blocking(collect_system_stats)
            .await
            .unwrap_or_default(),
        recovery_events: events,
        auto_recovery_enabled: auto_recovery,
        inactivity_timeout_secs,
    })
}

// ──────────────── 会话操作封装 ────────────────

/// 可扩展的 Codeg 会话操作。当前可直接对接 Codeg web API；
/// 后续可在不改变调用方的前提下增加重试、审计或 UI 触发。
pub async fn session_action(
    action: &str,
    connection_id: &str,
    message: Option<&str>,
) -> Result<serde_json::Value, String> {
    match action {
        "stop" => post_api(
            "acp_cancel",
            &serde_json::json!({ "connectionId": connection_id }),
        )
        .await,
        "restart" | "resume" => {
            let text = message.unwrap_or("继续");
            // 先停止当前 turn，避免在 ACP 忙碌时直接 prompt 被判定为 TurnInProgress。
            post_api(
                "acp_cancel",
                &serde_json::json!({ "connectionId": connection_id }),
            )
            .await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            post_api(
                "acp_prompt",
                &serde_json::json!({
                    "connectionId": connection_id,
                    "blocks": [{ "type": "text", "text": text }],
                }),
            )
            .await
        }
        "disconnect" => post_api(
            "acp_disconnect",
            &serde_json::json!({ "connectionId": connection_id }),
        )
        .await,
        other => Err(format!("不支持的 Codeg 会话操作：{other}")),
    }
}

// ──────────────── 系统统计 ────────────────

/// 统计运行中进程数量。使用跨平台 sysinfo，
/// 不启动 PowerShell 或第三方命令，避免控制台窗口闪动。
fn count_processes(system: &sysinfo::System, names: &[&str]) -> u64 {
    let target: Vec<std::ffi::OsString> = names.iter().map(std::ffi::OsString::from).collect();
    target
        .iter()
        .map(|name| system.processes_by_exact_name(name.as_os_str()).count() as u64)
        .sum()
}

fn collect_resource_stats() -> SystemStats {
    let mut system = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing()
            .with_cpu(sysinfo::CpuRefreshKind::nothing().with_cpu_usage())
            .with_memory(sysinfo::MemoryRefreshKind::everything()),
    );
    // CPU 占用率基于两次采样；第一次填充基线，sleep 后再次刷新。
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    system.refresh_cpu_usage();

    let cpu_percent = system.global_cpu_usage().max(0.0) as f64;
    let mem_total = system.total_memory();
    let mem_used = system.used_memory();
    let mem_percent = if mem_total == 0 {
        0.0
    } else {
        (mem_used as f64 / mem_total as f64 * 1000.0).round() / 10.0
    };

    let disks = sysinfo::Disks::new_with_refreshed_list();
    #[cfg(target_os = "windows")]
    let root_disk = disks
        .list()
        .iter()
        .find(|d| d.mount_point().starts_with("C:\\") || d.mount_point() == std::path::Path::new("C:"));
    #[cfg(not(target_os = "windows"))]
    let root_disk = disks
        .list()
        .iter()
        .find(|d| d.mount_point() == std::path::Path::new("/"));
    let c_total = root_disk.map(|d| d.total_space()).unwrap_or(0);
    let c_free = root_disk.map(|d| d.available_space()).unwrap_or(0);
    let c_used = c_total.saturating_sub(c_free);
    let c_percent = if c_total == 0 {
        0.0
    } else {
        (c_used as f64 / c_total as f64 * 1000.0).round() / 10.0
    };

    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    #[cfg(target_os = "windows")]
    let node_processes = count_processes(&system, &["node.exe", "nodejs.exe"]);
    #[cfg(not(target_os = "windows"))]
    let node_processes = count_processes(&system, &["node"]);
    let agent_processes = count_processes(&system, AGENT_PROCESS_NAMES);
    let gpu_hint = gpu_usage_gb();

    SystemStats {
        cpu_percent,
        memory_used_gb: (mem_used as f64 / 1024.0 / 1024.0 / 1024.0 * 10.0).round() / 10.0,
        memory_total_gb: (mem_total as f64 / 1024.0 / 1024.0 / 1024.0 * 10.0).round() / 10.0,
        memory_percent: mem_percent,
        gpu_used_gb: gpu_hint.0,
        gpu_total_gb: gpu_hint.1,
        gpu_percent: gpu_hint.2,
        c_drive_used_gb: (c_used as f64 / 1024.0 / 1024.0 / 1024.0 * 10.0).round() / 10.0,
        c_drive_total_gb: (c_total as f64 / 1024.0 / 1024.0 / 1024.0 * 10.0).round() / 10.0,
        c_drive_free_gb: (c_free as f64 / 1024.0 / 1024.0 / 1024.0 * 10.0).round() / 10.0,
        c_drive_percent: c_percent,
        node_processes,
        agent_processes,
    }
}

/// Windows GPU 专用内存采样。通过 NVML 动态加载读取，
/// 不启动 nvidia-smi 子进程，也不要求安装额外 CLI。
#[cfg(target_os = "windows")]
fn gpu_usage_gb() -> (Option<f64>, Option<f64>, Option<f64>) {
    const NVML_SUCCESS: i32 = 0;

    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    struct NvmlMemory {
        total: u64,
        free: u64,
        used: u64,
    }

    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    struct NvmlUtilization {
        gpu: u32,
        memory: u32,
    }

    let library = match unsafe { libloading::Library::new("nvml.dll") } {
        Ok(lib) => lib,
        Err(_) => return (None, None, None),
    };

    type NvmlInit = unsafe extern "C" fn() -> i32;
    type NvmlShutdown = unsafe extern "C" fn() -> i32;
    type NvmlDeviceGetCount = unsafe extern "C" fn(*mut u32) -> i32;
    type NvmlDeviceGetHandleByIndex = unsafe extern "C" fn(u32, *mut usize) -> i32;
    type NvmlDeviceGetMemoryInfo = unsafe extern "C" fn(usize, *mut NvmlMemory) -> i32;
    type NvmlDeviceGetUtilizationRates = unsafe extern "C" fn(usize, *mut NvmlUtilization) -> i32;

    let init: libloading::Symbol<NvmlInit> = match unsafe { library.get(b"nvmlInit_v2\0") } {
        Ok(sym) => sym,
        Err(_) => return (None, None, None),
    };
    let shutdown: libloading::Symbol<NvmlShutdown> = match unsafe { library.get(b"nvmlShutdown\0") } {
        Ok(sym) => sym,
        Err(_) => return (None, None, None),
    };
    let count_fn: libloading::Symbol<NvmlDeviceGetCount> =
        match unsafe { library.get(b"nvmlDeviceGetCount_v2\0") } {
            Ok(sym) => sym,
            Err(_) => return (None, None, None),
        };
    let handle_fn: libloading::Symbol<NvmlDeviceGetHandleByIndex> = match unsafe {
        library.get(b"nvmlDeviceGetHandleByIndex_v2\0")
    } {
        Ok(sym) => sym,
        Err(_) => return (None, None, None),
    };
    let memory_fn: libloading::Symbol<NvmlDeviceGetMemoryInfo> =
        match unsafe { library.get(b"nvmlDeviceGetMemoryInfo\0") } {
            Ok(sym) => sym,
            Err(_) => return (None, None, None),
        };
    let util_fn: libloading::Symbol<NvmlDeviceGetUtilizationRates> =
        match unsafe { library.get(b"nvmlDeviceGetUtilizationRates\0") } {
            Ok(sym) => sym,
            Err(_) => return (None, None, None),
        };

    let mut result = (None, None, None);
    unsafe {
        match init() {
            NVML_SUCCESS => {
                let mut dev_count = 0u32;
                if count_fn(&mut dev_count) == NVML_SUCCESS && dev_count > 0 {
                    for i in 0..dev_count {
                        let mut handle = 0usize;
                        if handle_fn(i, &mut handle) != NVML_SUCCESS {
                            continue;
                        }
                        let mut mem = NvmlMemory::default();
                        let mut util = NvmlUtilization::default();
                        if memory_fn(handle, &mut mem) == NVML_SUCCESS {
                            // NVML 返回的是字节；系统统计字段约定为 GB。
                            result = (
                                Some(mem.used as f64 / 1024.0 / 1024.0 / 1024.0),
                                Some(mem.total as f64 / 1024.0 / 1024.0 / 1024.0),
                                None,
                            );
                            if util_fn(handle, &mut util) == NVML_SUCCESS {
                                result.2 = Some(util.gpu as f64);
                            }
                            break;
                        }
                    }
                }
                shutdown();
            }
            _ => {}
        }
    }
    result
}

#[cfg(not(target_os = "windows"))]
fn gpu_usage_gb() -> (Option<f64>, Option<f64>, Option<f64>) {
    (None, None, None)
}

/// 返回供底部状态行使用的系统统计。使用跨平台 sysinfo + NVML，
/// 不启动 PowerShell / nvidia-smi 子进程。
pub fn collect_system_stats() -> SystemStats {
    collect_resource_stats()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classification_is_stable() {
        assert!(is_running("prompting"));
        assert!(is_running("connecting"));
        assert!(!is_running("connected"));
        assert!(is_stopped("disconnected"));
        assert!(!is_stopped("error"));
        assert!(is_error("error"));
        assert!(!is_error("disconnected"));
    }

    #[test]
    fn recovery_is_rate_limited_to_minute_interval() {
        // 第一次应放行，紧接着应被限频拒绝。
        assert!(should_run_recovery());
        assert!(!should_run_recovery());
    }
}
