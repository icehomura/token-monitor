//! AI 服务连接探针。
//!
//! 复用代理的上游配置（`proxy::upstream_url()` + `proxy::cfg().api_key` /
//! `model_override`），按固定间隔发送一个最小的 Responses 请求，检测服务可用性与延迟。
//!
//! 请求计入代理的并发统计（`proxy::acquire_slot()`），不会绕过并发上限偷偷打上游。
//! 配置挂在 token-monitor.json 的 `probe` 子对象下：
//! `{ "probe": { "enabled": true, "interval_secs": 15 } }`

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// 探针间隔下限（秒）：避免前端填 0 造成无限循环探测
const MIN_INTERVAL_SECS: u64 = 1;
/// 探针间隔上限（秒）：30 分钟，再长就失去监控意义
const MAX_INTERVAL_SECS: u64 = 1800;
/// 默认探测间隔（秒）
const DEFAULT_INTERVAL_SECS: u64 = 15;
/// 单次探测请求超时：探针只判断连通性，不等待长推理
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeConfig {
    pub enabled: bool,
    pub interval_secs: u64,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: DEFAULT_INTERVAL_SECS,
        }
    }
}

/// 把间隔收敛到 [1, 1800]；缺省与非法值都走这里
fn clamp_interval(secs: u64) -> u64 {
    secs.clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS)
}

// ──────────────── 配置持久化 ────────────────

static CONFIG: OnceLock<std::sync::RwLock<ProbeConfig>> = OnceLock::new();

pub fn config() -> ProbeConfig {
    CONFIG
        .get_or_init(|| std::sync::RwLock::new(ProbeConfig::default()))
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub fn set_config(cfg: ProbeConfig) {
    let mut cfg = cfg;
    cfg.interval_secs = clamp_interval(cfg.interval_secs);
    *CONFIG
        .get_or_init(|| std::sync::RwLock::new(ProbeConfig::default()))
        .write()
        .unwrap_or_else(|e| e.into_inner()) = cfg;
}

/// 解析 `{ enabled, interval_secs }` 形状的对象；字段缺失或类型不符时沿用 `base`。
/// 纯函数，便于测试；`interval_secs` 一律 clamp。
fn parse_settings(o: &Value, base: &ProbeConfig) -> ProbeConfig {
    ProbeConfig {
        enabled: o
            .get("enabled")
            .and_then(|x| x.as_bool())
            .unwrap_or(base.enabled),
        interval_secs: clamp_interval(
            o.get("interval_secs")
                .and_then(|x| x.as_u64())
                .unwrap_or(base.interval_secs),
        ),
    }
}

/// 从 token-monitor.json 读取并初始化探针配置；未配置时使用默认值（开启、15 秒）。
pub fn load_from_json(v: &Value) -> ProbeConfig {
    let default = ProbeConfig::default();
    let cfg = parse_settings(v.get("probe").unwrap_or(&Value::Null), &default);
    set_config(cfg.clone());
    cfg
}

pub fn save_to_json(v: &mut Value, cfg: &ProbeConfig) {
    v["probe"] = json!({
        "enabled": cfg.enabled,
        "interval_secs": clamp_interval(cfg.interval_secs),
    });
}

/// 立即测试一次上游连通性：返回 `{ success, status_code, latency_ms, message }`。

/// 探针设置的对外 JSON 形状（前端契约）
fn settings_json(cfg: &ProbeConfig) -> Value {
    json!({
        "enabled": cfg.enabled,
        "interval_secs": cfg.interval_secs,
    })
}

/// 失败结果的统一构造：status_code / latency_ms 均为 0，不携带任何网络信息
fn failure(message: impl Into<String>) -> Value {
    json!({
        "success": false,
        "status_code": 0u16,
        "latency_ms": 0u64,
        "message": message.into(),
    })
}

/// 状态码 → (success, message)。2xx 视为连接成功，其余原样回显状态码。
fn classify(status_code: u16) -> (bool, String) {
    if (200..300).contains(&status_code) {
        (true, format!("连接成功（HTTP {status_code}）"))
    } else {
        (false, format!("服务返回 HTTP {status_code}"))
    }
}

/// 前置校验：返回 Some(结果) 表示无需发请求，直接把该结果交给前端。
/// 抽成纯函数，便于不发网络请求地覆盖分支。
fn precheck(enabled: bool, upstream: &str, api_key: &str) -> Option<Value> {
    if !enabled {
        return Some(failure("探针未开启"));
    }
    if upstream.trim().is_empty() {
        return Some(failure("上游地址未配置"));
    }
    if api_key.trim().is_empty() {
        return Some(failure("API Key 未配置"));
    }
    None
}

/// 向 Responses 上游发送最小请求（`{ model, input: "hi" }`），只关心状态码。
/// 上游地址与代理转发共用同一个 `upstream_url`，不做路径拼接。
async fn probe_responses(upstream: &str, api_key: &str, model: &str) -> Result<u16, String> {
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(upstream)
        .bearer_auth(api_key)
        .json(&json!({
            "model": model,
            "input": "hi",
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(resp.status().as_u16())
}

/// 向 Chat Completions 上游发送最小请求（`{ model, messages: [...] }`），只关心状态码。
async fn probe_chat_completions(upstream: &str, api_key: &str, model: &str) -> Result<u16, String> {
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(upstream)
        .bearer_auth(api_key)
        .json(&json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(resp.status().as_u16())
}

/// 向 Anthropic Messages 上游发送最小请求，只关心状态码。
async fn probe_anthropic(upstream: &str, api_key: &str, model: &str) -> Result<u16, String> {
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(upstream)
        .bearer_auth(api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(resp.status().as_u16())
}

// ──────────────── Tauri 命令 ────────────────

/// 读取探针设置（供设置面板回显）
#[tauri::command]
pub fn get_probe_settings() -> Value {
    settings_json(&config())
}

/// 保存探针设置：字段缺失时沿用当前值，interval_secs 自动 clamp 到 1..=1800，
/// 写回 token-monitor.json（保留文件中的其它配置）并热更新运行时。
#[tauri::command]
pub fn set_probe_settings(config: Value) -> Result<Value, String> {
    let base = self::config();
    let new_cfg = parse_settings(&config, &base);

    let path = crate::config_path().ok_or("无法确定配置文件路径")?;
    let mut v: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));
    save_to_json(&mut v, &new_cfg);
    let text = serde_json::to_string_pretty(&v).map_err(|e| format!("序列化配置失败：{e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("写入配置失败：{e}"))?;

    set_config(new_cfg.clone());
    Ok(settings_json(&new_cfg))
}

/// 立即测试一次上游连通性：返回 `{ success, status_code, latency_ms, message }`。
///
/// 失败（未开启 / 未配置 / 非 2xx / 传输错误）同样编码进返回值，只有构造 JSON
/// 这类不可恢复场景才返回 Err；正常路径不 panic。
#[tauri::command]
pub async fn test_ai_connection() -> Result<Value, String> {
    // 探针目标 = 首个启用渠道（无启用渠道时回退全局配置）
    let ch = crate::proxy::probe_channel();
    let upstream = ch.upstream_url.trim().to_string();

    if let Some(v) = precheck(config().enabled, &upstream, &ch.api_key) {
        return Ok(v);
    }

    // 计入并发：与代理转发共用同一套槽位，等待超时直接给出可读提示
    let _slot = match crate::proxy::acquire_slot().await {
        Some(g) => g,
        None => return Ok(failure("并发已满，等待槽位超时")),
    };

    let start = Instant::now();
    let result = match ch.upstream_format {
        crate::proxy::UpstreamFormat::ChatCompletions => probe_chat_completions(&upstream, &ch.api_key, &ch.model_override).await,
        crate::proxy::UpstreamFormat::Anthropic => probe_anthropic(&upstream, &ch.api_key, &ch.model_override).await,
        crate::proxy::UpstreamFormat::Responses => probe_responses(&upstream, &ch.api_key, &ch.model_override).await,
    };
    let latency_ms = start.elapsed().as_millis() as u64;

    Ok(match result {
        Ok(code) => {
            let (success, message) = classify(code);
            json!({
                "success": success,
                "status_code": code,
                "latency_ms": latency_ms,
                "message": message,
            })
        }
        Err(e) => json!({
            "success": false,
            "status_code": 0u16,
            "latency_ms": latency_ms,
            "message": format!("连接失败：{e}"),
        }),
    })
}

// ──────────────── 测试 ────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 探针配置是全局单例，测试并行跑时会互相覆盖，统一串行化。
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn default_config_enabled_with_15s() {
        let cfg = ProbeConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.interval_secs, DEFAULT_INTERVAL_SECS);
    }

    #[test]
    fn json_roundtrip_keeps_values() {
        let _g = lock();
        let cfg = ProbeConfig {
            enabled: false,
            interval_secs: 42,
        };
        let mut v = json!({ "api_key": "sk-x" });
        save_to_json(&mut v, &cfg);
        // 不破坏文件里的其它字段
        assert_eq!(v["api_key"], "sk-x");
        assert_eq!(v["probe"]["enabled"], false);
        assert_eq!(v["probe"]["interval_secs"], 42);

        let back = load_from_json(&v);
        assert!(!back.enabled);
        assert_eq!(back.interval_secs, 42);
        assert_eq!(settings_json(&back)["interval_secs"], 42);
    }

    #[test]
    fn load_from_json_tolerates_missing_fields() {
        let _g = lock();

        let c = load_from_json(&json!({}));
        assert!(c.enabled);
        assert_eq!(c.interval_secs, DEFAULT_INTERVAL_SECS);

        let c = load_from_json(&json!({ "probe": {} }));
        assert!(c.enabled);
        assert_eq!(c.interval_secs, DEFAULT_INTERVAL_SECS);

        // 部分字段缺失：只覆盖给到的字段
        let c = load_from_json(&json!({ "probe": { "enabled": false } }));
        assert!(!c.enabled);
        assert_eq!(c.interval_secs, DEFAULT_INTERVAL_SECS);

        // 类型不符时按缺失处理，不 panic
        let c = load_from_json(&json!({ "probe": { "enabled": "yes", "interval_secs": -1 } }));
        assert!(c.enabled);
        assert_eq!(c.interval_secs, DEFAULT_INTERVAL_SECS);
    }

    #[test]
    fn interval_is_clamped() {
        assert_eq!(clamp_interval(0), MIN_INTERVAL_SECS);
        assert_eq!(clamp_interval(99999), MAX_INTERVAL_SECS);
        assert_eq!(clamp_interval(1), 1);
        assert_eq!(clamp_interval(15), 15);
        assert_eq!(clamp_interval(1800), 1800);

        let _g = lock();
        let c = load_from_json(&json!({ "probe": { "interval_secs": 0 } }));
        assert_eq!(c.interval_secs, 1);
        let c = load_from_json(&json!({ "probe": { "interval_secs": 99999 } }));
        assert_eq!(c.interval_secs, 1800);

        // set_config 同样收敛，避免绕过校验写入非法值
        set_config(ProbeConfig {
            enabled: false,
            interval_secs: 99999,
        });
        assert_eq!(config().interval_secs, MAX_INTERVAL_SECS);
    }

    #[test]
    fn set_config_updates_global() {
        let _g = lock();
        set_config(ProbeConfig {
            enabled: false,
            interval_secs: 30,
        });
        let c = config();
        assert!(!c.enabled);
        assert_eq!(c.interval_secs, 30);

        let s = settings_json(&config());
        assert_eq!(s["enabled"], false);
        assert_eq!(s["interval_secs"], 30);
    }

    #[test]
    fn classify_maps_status_code() {
        assert_eq!(classify(200), (true, "连接成功（HTTP 200）".to_string()));
        assert_eq!(classify(204), (true, "连接成功（HTTP 204）".to_string()));
        assert_eq!(classify(401), (false, "服务返回 HTTP 401".to_string()));
        assert_eq!(classify(500), (false, "服务返回 HTTP 500".to_string()));
        assert_eq!(classify(0), (false, "服务返回 HTTP 0".to_string()));
    }

    #[test]
    fn precheck_short_circuits_without_request() {
        // 未开启优先于未配置
        let v = precheck(false, "", "").expect("未开启应直接返回结果");
        assert_eq!(v["success"], false);
        assert_eq!(v["status_code"], 0);
        assert_eq!(v["latency_ms"], 0);
        assert_eq!(v["message"], "探针未开启");

        let v = precheck(true, "   ", "sk-x").expect("空地址应直接返回结果");
        assert_eq!(v["message"], "上游地址未配置");

        let v = precheck(true, "https://api.example.com", "  ").expect("空 Key 应直接返回结果");
        assert_eq!(v["message"], "API Key 未配置");

        assert!(precheck(true, "https://api.example.com", "sk-x").is_none());
    }

    #[test]
    fn set_probe_settings_parses_and_clamps() {
        let base = ProbeConfig {
            enabled: true,
            interval_secs: 60,
        };
        // 缺失字段沿用当前值
        let c = parse_settings(&json!({ "enabled": false }), &base);
        assert!(!c.enabled);
        assert_eq!(c.interval_secs, 60);

        // 越界收敛
        let c = parse_settings(&json!({ "interval_secs": 0u64 }), &base);
        assert_eq!(c.interval_secs, 1);
        let c = parse_settings(&json!({ "interval_secs": 99999u64 }), &base);
        assert_eq!(c.interval_secs, 1800);
    }
}
