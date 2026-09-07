//! CodeG server 连接：配置持久化、`POST /api/*` Bearer 调用、会话快照聚合，
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
        .unwrap()
}

pub fn update_config(cfg: CodegConfig) -> CodegConfig {
    *CONFIG
        .get_or_init(|| std::sync::RwLock::new(CodegConfig::default()))
        .write()
        .unwrap() = cfg.clone();
    cfg
}

/// 从 token-monitor.json 读取并初始化 CodeG 配置；未配置时使用默认值，不阻塞启动。
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
        return Err("CodeG 服务器地址未配置".into());
    }
    if token.is_empty() {
        return Err("CodeG 服务器 Token 未配置".into());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| format!("CodeG HTTP 客户端创建失败：{e}"))?;
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
        .map_err(|e| format!("CodeG 请求失败：{e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        serde_json::from_str(&text)
            .map_err(|e| format!("CodeG 返回不是有效 JSON：{e}（{}）", text.trim()))
    } else if status.as_u16() == 401 {
        Err(format!("CodeG Token 无效或未授权（HTTP 401）：{}", text.trim()))
    } else {
        Err(format!(
            "CodeG 接口错误（HTTP {}）：{}",
            status.as_u16(),
            text.trim()
        ))
    }
}

#[derive(Deserialize)]
struct CodegConnection {
    id: String,
    agent_type: String,
    status: String,
}

#[derive(Deserialize, Clone, Default)]
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
}

#[derive(Deserialize, Clone, Default)]
struct LiveMessageInfo {} // live turn 存在即视为有活动，无需具体字段

#[derive(Deserialize, Clone, Default)]
struct UsageInfo {
    #[serde(default)]
    used: Option<u64>,
}

fn is_running(status: &str) -> bool {
    matches!(status, "prompting" | "connecting")
}

fn is_stopped(status: &str) -> bool {
    matches!(status, "disconnected" | "error")
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

fn last_activity_map() -> &'static Mutex<HashMap<String, (Instant, u64)>> {
    LAST_ACTIVITY.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn fetch_session_summaries(
    recover: bool,
    events: &mut Vec<String>,
) -> Result<Vec<CodegSessionSummary>, String> {
    let conns: Vec<CodegConnection> =
        serde_json::from_value(post_api("acp_list_connections", &serde_json::json!({})).await?)
            .map_err(|e| format!("解析 CodeG 连接列表失败：{e}"))?;

    let mut summaries = Vec::with_capacity(conns.len());
    for c in conns {
        let mut summary = CodegSessionSummary {
            connection_id: c.id,
            status: c.status.clone(),
            agent_type: c.agent_type.clone(),
            session_name: String::new(),
            waiting_for: None,
            idle_secs: 0,
            last_activity_at: None,
            automatic_restart_count: 0,
            last_restart_reason: None,
        };
        let mut snap = CodegSnapshot::default();
        if let Ok(v) = post_api(
            "acp_get_session_snapshot",
            &serde_json::json!({ "connectionId": summary.connection_id }),
        )
        .await
        {
            if let Ok(s) = serde_json::from_value::<CodegSnapshot>(v) {
                snap = s.clone();
                summary.session_name = snap
                    .external_id
                    .as_deref()
                    .map(str::to_string)
                    .or_else(|| snap.agent_type.as_deref().map(str::to_string))
                    .or_else(|| Some(summary.agent_type.clone()))
                    .unwrap_or_default();
                if let Some(status) = snap.status.as_deref() {
                    summary.status = status.to_string();
                }
                summary.waiting_for = waiting_label(&snap);
            }
        }

        // 不活跃判定：仅对存活且正在运行的连接生效；等待用户输入不算无输出。
        if recover && is_running(&summary.status) && summary.waiting_for.is_none() {
            let active_now = snap
                .usage
                .as_ref()
                .and_then(|u| u.used)
                .unwrap_or(0)
                > 0
                || snap.live_message.is_some();
            let threshold = config().inactivity_timeout_secs.max(10);
            let should_restart = {
                // 锁只在该同步块内使用，避免 std MutexGuard 跨 await。
                let mut map = last_activity_map().lock().unwrap();
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
            };
            if should_restart {
                let reason = format!(
                    "活跃会话 {} 秒无输出，触发自动停止并重启",
                    summary.idle_secs
                );
                match session_action("restart", &summary.connection_id, Some("继续")).await {
                    Ok(_) => {
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
        return Err("CodeG 行显示已关闭".into());
    }
    if !configured {
        return Err("CodeG 服务器地址或 Token 未配置".into());
    }

    let mut events = Vec::new();
    let summaries = fetch_session_summaries(auto_recovery, &mut events).await?;
    let running = summaries.iter().filter(|s| is_running(&s.status)).count() as u64;
    let stopped = summaries.iter().filter(|s| is_stopped(&s.status)).count() as u64;
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
        waiting_input_count: waiting,
        active_session_name,
        sessions: summaries,
        system: collect_system_stats(),
        recovery_events: events,
        auto_recovery_enabled: auto_recovery,
        inactivity_timeout_secs,
    })
}

// ──────────────── 会话操作封装 ────────────────

/// 可扩展的 CodeG 会话操作。当前可直接对接 Codeg web API；
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
        other => Err(format!("不支持的 CodeG 会话操作：{other}")),
    }
}

// ──────────────── Windows 系统统计 ────────────────

/// 统计 node / 编程智能体进程。使用 PowerShell 的原生 `Get-Process`，
/// 不依赖 tasklist 的 `/FO` 参数，避免 MSYS 路径转换问题。
fn count_processes(names: &[&str]) -> u64 {
    let pattern = names
        .iter()
        .map(|n| n.trim_end_matches(".exe").to_owned())
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        "Get-Process -ErrorAction SilentlyContinue | Where-Object {{ '{pattern}' -split ',' -contains $_.ProcessName.ToLowerInvariant() }} | Measure-Object | Select-Object -ExpandProperty Count"
    );
    run_powershell(&script)
        .and_then(|s| s.trim().parse::<u64>().map_err(|e| e.to_string()))
        .unwrap_or(0)
}

fn run_powershell(script: &str) -> Result<String, String> {
    // Windows PowerShell 5.1 通过 -EncodedCommand 最稳定，但某些环境模块加载
    // 会把文本折行；这里使用 -Command 加显式的 ConvertTo-Json 全名。
    let output = std::process::Command::new("powershell.exe")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(script)
        .output()
        .map_err(|e| format!("无法执行 powershell.exe：{e}"))?;
    if !output.status.success() {
        return Err(format!("powershell.exe 退出状态：{:?}", output.status.code()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn collect_resource_stats() -> SystemStats {
    // 一行 PowerShell 返回：cpu, usedMemBytes, totalMemBytes, cUsedBytes, cTotalBytes
    // 避免外层 shell 展开 `$`（Rust Command 不会展开，但保持脚本简单可靠）。
    let script = "($cpu=(Get-CimInstance Win32_Processor -ErrorAction SilentlyContinue|Measure-Object -Property LoadPercentage -Average).Average); $os=Get-CimInstance Win32_OperatingSystem -ErrorAction SilentlyContinue; $total=[double]$os.TotalVisibleMemorySize*1KB; $free=[double]$os.FreePhysicalMemory*1KB; $drive=Get-PSDrive -Name C -ErrorAction SilentlyContinue; $cTotal=[double]($drive.Used+$drive.Free); $cUsed=[double]$drive.Used; Write-Output (\"{0} {1} {2} {3} {4}\" -f ($cpu -as [double]),($total-$free),$total,$cUsed,$cTotal)";
    let out = match run_powershell(script) {
        Ok(out) => out,
        Err(_) => return SystemStats::default(),
    };
    let parts: Vec<f64> = out
        .split_whitespace()
        .filter_map(|s| s.parse::<f64>().ok())
        .collect();
    if parts.len() < 5 {
        return SystemStats::default();
    }
    let cpu = parts[0];
    let mem_used = parts[1];
    let mem_total = parts[2];
    let c_used = parts[3];
    let c_total = parts[4];
    let gpu_hint = gpu_usage_gb();
    SystemStats {
        cpu_percent: cpu.round(),
        memory_used_gb: (mem_used / 1024.0 / 1024.0 / 1024.0).round(),
        memory_total_gb: (mem_total / 1024.0 / 1024.0 / 1024.0).round(),
        memory_percent: if mem_total > 0.0 {
            (mem_used / mem_total * 100.0).round()
        } else {
            0.0
        },
        gpu_used_gb: gpu_hint.0,
        gpu_total_gb: gpu_hint.1,
        gpu_percent: gpu_hint.2,
        c_drive_used_gb: (c_used / 1024.0 / 1024.0 / 1024.0).round(),
        c_drive_total_gb: (c_total / 1024.0 / 1024.0 / 1024.0).round(),
        c_drive_percent: if c_total > 0.0 {
            (c_used / c_total * 100.0).round()
        } else {
            0.0
        },
        node_processes: count_processes(&["node.exe", "nodejs.exe"]),
        agent_processes: count_processes(AGENT_PROCESS_NAMES),
    }
}

/// Windows GPU 专用内存采样。优先 nvidia-smi，其余厂商暂返回未可知状态。
fn gpu_usage_gb() -> (Option<f64>, Option<f64>, Option<f64>) {
    for tool in ["nvidia-smi", "nvidia-smi.exe"] {
        let out = std::process::Command::new(tool)
            .args([
                "--query-gpu=memory.used,memory.total,utilization.gpu",
                "--format=csv,noheader,nounits",
            ])
            .output();
        if let Ok(o) = out {
            if o.status.success() {
                let line = String::from_utf8_lossy(&o.stdout);
                if let Some(line) = line.lines().next() {
                    let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
                    if parts.len() >= 3 {
                        let used = parts[0].parse::<f64>().unwrap_or(0.0);
                        let total = parts[1].parse::<f64>().unwrap_or(1.0);
                        let percent = parts[2].parse::<f64>().unwrap_or(0.0);
                        return (Some(used / 1024.0), Some(total / 1024.0), Some(percent));
                    }
                }
            }
        }
    }
    (None, None, None)
}

/// 返回供底部状态行使用的系统统计。当前 Windows 使用 PowerShell；
/// 非 Windows 环境返回全空值，不安装额外依赖。
pub fn collect_system_stats() -> SystemStats {
    if cfg!(target_os = "windows") {
        collect_resource_stats()
    } else {
        SystemStats::default()
    }
}