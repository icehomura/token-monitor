//! 渠道账户余额查询。
//!
//! 支持两类上游，按 host 分派：
//!   - DeepSeek 官方（host == api.deepseek.com）：`GET /user/balance`
//!   - sub2api 兼容中转站：`GET {origin}/v1/usage`，Bearer 认证
//!
//! sub2api 采用「探测即识别」：该接口返回 `mode` 字段即视为支持，
//! 不必在渠道配置里声明渠道类型。确认不支持的地址会被缓存，避免反复探测。
//! 配置挂在 token-monitor.json 的 `balance` 子对象下。
//!
//! DeepSeek 余额接口返回形如：
//! `{ "is_available": true, "balance_infos": [ { "currency": "CNY",
//!    "total_balance": "110.00", "granted_balance": "10.00",
//!    "topped_up_balance": "100.00" } ] }`
//!
//! sub2api 三种模式（钱包 / 订阅 / 额度受限）统一取余额字段：
//! `{ "mode": "unrestricted", "planName": "钱包余额",
//!    "balance": 21.18, "remaining": 21.18, "unit": "USD" }`
//!
//! 金额一律以字符串透传给前端，绝不解析成浮点参与运算，避免精度损失。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

/// DeepSeek 官方余额接口（无参数、Bearer 鉴权）。
const BALANCE_ENDPOINT: &str = "https://api.deepseek.com/user/balance";

/// 官方上游 host，用于判定是否值得查询余额。
const OFFICIAL_HOST: &str = "api.deepseek.com";

const DEFAULT_INTERVAL_SECS: u64 = 15;
const MIN_INTERVAL_SECS: u64 = 1;
const MAX_INTERVAL_SECS: u64 = 1800;

/// 查询超时。余额接口很轻，15 秒足够，且避免长时间占用并发槽位。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// 错误信息里响应体摘要的最大字符数（按字符计，避免切断中文）。
const ERROR_BODY_MAX_CHARS: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceConfig {
    pub enabled: bool,
    pub currency: String,
    pub interval_secs: u64,
}

impl Default for BalanceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            currency: "CNY".into(),
            interval_secs: DEFAULT_INTERVAL_SECS,
        }
    }
}

static CONFIG: OnceLock<RwLock<BalanceConfig>> = OnceLock::new();

fn store() -> &'static RwLock<BalanceConfig> {
    CONFIG.get_or_init(|| RwLock::new(BalanceConfig::default()))
}

pub fn config() -> BalanceConfig {
    store()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// 写入配置（顺带做一次规范化：币种大写、间隔 clamp），保证内存中的值始终合法。
pub fn set_config(cfg: BalanceConfig) {
    *store().write().unwrap_or_else(|e| e.into_inner()) = normalize(cfg);
}

/// 币种只认 CNY / USD，大小写不敏感；其余返回 None。
fn parse_currency(raw: &str) -> Option<String> {
    let upper = raw.trim().to_ascii_uppercase();
    match upper.as_str() {
        "CNY" => Some("CNY".into()),
        "USD" => Some("USD".into()),
        _ => None,
    }
}

fn clamp_interval(secs: u64) -> u64 {
    secs.clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS)
}

fn normalize(cfg: BalanceConfig) -> BalanceConfig {
    BalanceConfig {
        enabled: cfg.enabled,
        currency: parse_currency(&cfg.currency).unwrap_or_else(|| BalanceConfig::default().currency),
        interval_secs: clamp_interval(cfg.interval_secs),
    }
}

/// 从 token-monitor.json 读取并初始化余额配置；字段缺失时用默认值，不阻塞启动。
pub fn load_from_json(v: &serde_json::Value) -> BalanceConfig {
    let mut cfg = BalanceConfig::default();
    if let Some(o) = v.get("balance") {
        cfg.enabled = o.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false);
        cfg.currency = o
            .get("currency")
            .and_then(|x| x.as_str())
            .and_then(parse_currency)
            .unwrap_or_else(|| cfg.currency.clone());
        cfg.interval_secs = o
            .get("interval_secs")
            .and_then(|x| x.as_u64())
            .unwrap_or(DEFAULT_INTERVAL_SECS);
    }
    let cfg = normalize(cfg);
    set_config(cfg.clone());
    cfg
}

pub fn save_to_json(v: &mut serde_json::Value, cfg: &BalanceConfig) {
    v["balance"] = json!({
        "enabled": cfg.enabled,
        "currency": cfg.currency.trim(),
        "interval_secs": cfg.interval_secs,
    });
}

/// 上游是否为 DeepSeek 官方：只看 host，忽略大小写与路径 / 端口差异。
/// 空串与畸形 URL 一律按“非官方”处理（宁可不查，也不把 Key 打到第三方）。
pub fn is_official_upstream(upstream_url: &str) -> bool {
    let raw = upstream_url.trim();
    if raw.is_empty() {
        return false;
    }
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    match reqwest::Url::parse(&with_scheme) {
        Ok(u) => u
            .host_str()
            .map(|h| h.eq_ignore_ascii_case(OFFICIAL_HOST))
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// 从渠道上游地址推导 sub2api 用量 / 余额接口地址。
///
/// 取**最后一个** `/v1/` 之前的部分拼接 `usage`，以兼容带路径前缀的部署：
///   - `https://sub.callai.one/v1/responses`    → `https://sub.callai.one/v1/usage`
///   - `https://host/proxy/v1/chat/completions` → `https://host/proxy/v1/usage`
///   - `https://host`（无 `/v1/`）               → `https://host/v1/usage`
///
/// 地址为空或畸形时返回 None —— 宁可不查，也不猜一个目标地址把 Key 发出去。
fn usage_endpoint(upstream_url: &str) -> Option<String> {
    let raw = upstream_url.trim();
    if raw.is_empty() {
        return None;
    }
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    let u = reqwest::Url::parse(&with_scheme).ok()?;
    let host = u.host_str().filter(|h| !h.is_empty())?;
    let origin = match u.port() {
        Some(p) => format!("{}://{}:{}", u.scheme(), host, p),
        None => format!("{}://{}", u.scheme(), host),
    };
    let path = u.path();
    Some(match path.rfind("/v1/") {
        Some(idx) => format!("{}{}usage", origin, &path[..idx + 4]),
        None => format!("{origin}/v1/usage"),
    })
}

/// 已确认不提供兼容余额接口的地址集合。
///
/// 以推导出的端点为键：渠道地址一改，键随之改变、缓存自然失效，
/// 因此无需在渠道保存 / 删除时做额外联动清理。
static UNSUPPORTED_ENDPOINTS: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();

fn unsupported_endpoints() -> &'static RwLock<HashSet<String>> {
    UNSUPPORTED_ENDPOINTS.get_or_init(|| RwLock::new(HashSet::new()))
}

fn is_known_unsupported(endpoint: &str) -> bool {
    unsupported_endpoints()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains(endpoint)
}

fn mark_unsupported(endpoint: &str) {
    unsupported_endpoints()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(endpoint.to_string());
}

/// 该渠道是否值得发起余额查询。
///
/// 探针事件与 `get_balance` 的免占槽早退共用这一个判据，避免两处口径分叉。
/// 已知不支持的地址直接返回 false —— 余额轮询不该为它们白占并发槽位。
pub fn is_balance_candidate(upstream_url: &str) -> bool {
    if is_official_upstream(upstream_url) {
        return true;
    }
    match usage_endpoint(upstream_url) {
        Some(ep) => !is_known_unsupported(&ep),
        None => false,
    }
}

/// 金额统一转成两位小数字符串。
///
/// DeepSeek 给的是字符串、sub2api 给的是 JSON number，而前端不做浮点运算，
/// 因此在这里定型，保持「金额以字符串透传」的既有约定。
fn amount_string(v: &Value) -> Option<String> {
    if let Some(n) = v.as_f64() {
        return Some(format!("{n:.2}"));
    }
    v.as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 解析 sub2api `GET /v1/usage` 响应（纯函数，便于单测）。
///
/// 调用方已确认响应带 `mode` 字段。三种模式取余额的优先级一致：
/// 钱包 `balance` → `remaining` → `quota.remaining`，
/// 分别覆盖钱包、订阅、额度受限三种形态。
fn parse_sub2api_balance(body: &Value) -> Value {
    let mode = body.get("mode").and_then(|m| m.as_str()).unwrap_or("");
    let unit = body
        .get("unit")
        .and_then(|u| u.as_str())
        .filter(|u| !u.trim().is_empty())
        .unwrap_or("USD");
    let plan = body.get("planName").and_then(|p| p.as_str()).unwrap_or("");

    let total = body
        .get("balance")
        .and_then(amount_string)
        .or_else(|| body.get("remaining").and_then(amount_string))
        .or_else(|| {
            body.get("quota")
                .and_then(|q| q.get("remaining"))
                .and_then(amount_string)
        });

    match total {
        Some(total) => json!({
            "supported": true,
            "available": true,
            "currency": unit,
            "total": total,
            "granted": Value::Null,
            "topped_up": Value::Null,
            "mode": mode,
            "plan": plan,
            "reason": Value::Null,
        }),
        None => json!({
            "supported": true,
            "available": false,
            "currency": unit,
            "mode": mode,
            "plan": plan,
            "reason": "余额接口未返回可用金额",
        }),
    }
}

fn config_to_json(cfg: &BalanceConfig) -> Value {
    json!({
        "enabled": cfg.enabled,
        "currency": cfg.currency,
        "interval_secs": cfg.interval_secs,
    })
}

#[tauri::command]
pub fn get_balance_settings() -> serde_json::Value {
    config_to_json(&config())
}

/// 校验并保存余额设置。缺失字段沿用当前值，间隔超范围按 clamp 处理而不是报错。
#[tauri::command]
pub fn set_balance_settings(config: serde_json::Value) -> Result<serde_json::Value, String> {
    let current = self::config();
    let enabled = config
        .get("enabled")
        .and_then(|x| x.as_bool())
        .unwrap_or(current.enabled);
    let currency = match config.get("currency").and_then(|x| x.as_str()) {
        Some(s) if !s.trim().is_empty() => parse_currency(s)
            .ok_or_else(|| format!("币种仅支持 CNY 或 USD，收到：{}", s.trim()))?,
        _ => current.currency,
    };
    let interval_secs = config
        .get("interval_secs")
        .and_then(|x| x.as_u64())
        .map(clamp_interval)
        .unwrap_or(current.interval_secs);

    let cfg = BalanceConfig {
        enabled,
        currency,
        interval_secs,
    };

    // 落盘：读改写整个 JSON，保留文件中的其它配置（与 probe 同范式）
    let path = crate::config_path().ok_or("无法确定配置文件路径")?;
    let mut v: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));
    save_to_json(&mut v, &cfg);
    let text = serde_json::to_string_pretty(&v).map_err(|e| format!("序列化配置失败：{e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("写入配置失败：{e}"))?;

    set_config(cfg.clone());
    Ok(config_to_json(&cfg))
}

/// 解析余额响应为前端契约结构（纯函数，便于单测）。
/// 命中目标币种时返回完整金额（字符串原样透传）；未命中返回 available=false + 原因。
fn parse_balance(body: &serde_json::Value, currency: &str) -> Value {
    let available = body
        .get("is_available")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let hit = body
        .get("balance_infos")
        .and_then(|x| x.as_array())
        .and_then(|arr| {
            arr.iter().find(|item| {
                item.get("currency")
                    .and_then(|c| c.as_str())
                    .map(|c| c.eq_ignore_ascii_case(currency))
                    .unwrap_or(false)
            })
        });

    match hit {
        Some(item) => json!({
            "supported": true,
            "available": available,
            "currency": currency,
            "total": amount(item, "total_balance"),
            "granted": amount(item, "granted_balance"),
            "topped_up": amount(item, "topped_up_balance"),
            "reason": Value::Null,
        }),
        None => json!({
            "supported": true,
            "available": false,
            "currency": currency,
            "reason": "账号无该币种余额",
        }),
    }
}

/// 金额字段原样透传（API 给的就是字符串），缺失时为 null。
fn amount(item: &serde_json::Value, key: &str) -> Value {
    item.get(key).cloned().unwrap_or(Value::Null)
}

/// 截断响应体用于错误提示：限制长度并保持 UTF-8 边界，避免中文被切坏。
fn summarize(text: &str) -> String {
    let t = text.trim();
    if t.chars().count() <= ERROR_BODY_MAX_CHARS {
        return t.to_string();
    }
    let head: String = t.chars().take(ERROR_BODY_MAX_CHARS).collect();
    format!("{head}…")
}

/// 构建余额查询用的 HTTP 客户端（超时统一）。
fn balance_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| format!("余额查询 HTTP 客户端创建失败：{e}"))
}

/// 为指定渠道查询余额（不走 acquire_lease，直接用该渠道凭据）。
/// 用于请求完成后按渠道查询余额，避免占用全局并发槽位。
/// 按 host 分派 DeepSeek 官方 / sub2api 兼容中转站两类上游。
async fn query_channel_balance(upstream_url: &str, api_key: &str, currency: &str) -> Result<Value, String> {
    let key = api_key.trim().to_string();
    if key.is_empty() {
        return Err("API Key 未配置".into());
    }
    if is_official_upstream(upstream_url) {
        return query_deepseek_balance(&key, currency).await;
    }
    query_sub2api_balance(upstream_url, &key).await
}

/// DeepSeek 官方余额：`GET /user/balance`，金额为字符串，原样透传。
async fn query_deepseek_balance(api_key: &str, currency: &str) -> Result<Value, String> {
    let resp = balance_client()?
        .get(BALANCE_ENDPOINT)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| format!("余额查询请求失败：{e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "余额查询失败（HTTP {}）：{}",
            status.as_u16(),
            summarize(&text)
        ));
    }
    let body: Value = serde_json::from_str(&text)
        .map_err(|e| format!("余额接口返回不是有效 JSON：{e}（{}）", summarize(&text)))?;
    Ok(parse_balance(&body, currency))
}

/// sub2api 兼容中转站余额：`GET {origin}/v1/usage`。
///
/// 「探测即识别」：200 且响应体带 `mode` 字段即认定为支持。
/// 404 / 405 / 501 说明该站点根本没有这个接口，记入负缓存后不再反复探测；
/// 其余非 2xx（401 / 5xx 等）不入缓存 —— 多半是临时故障或 Key 配置问题，值得重试。
async fn query_sub2api_balance(upstream_url: &str, api_key: &str) -> Result<Value, String> {
    let Some(endpoint) = usage_endpoint(upstream_url) else {
        return Ok(json!({
            "supported": false,
            "reason": "渠道上游地址无效，无法推导余额接口",
        }));
    };
    if is_known_unsupported(&endpoint) {
        return Ok(json!({
            "supported": false,
            "reason": "该渠道未提供兼容的余额接口",
        }));
    }

    let resp = balance_client()?
        .get(&endpoint)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| format!("余额查询请求失败：{e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if matches!(
        status,
        reqwest::StatusCode::NOT_FOUND
            | reqwest::StatusCode::METHOD_NOT_ALLOWED
            | reqwest::StatusCode::NOT_IMPLEMENTED
    ) {
        mark_unsupported(&endpoint);
        return Ok(json!({
            "supported": false,
            "reason": format!("该渠道未提供兼容的余额接口（HTTP {}）", status.as_u16()),
        }));
    }
    if !status.is_success() {
        return Ok(json!({
            "supported": false,
            "reason": format!("余额接口返回 HTTP {}", status.as_u16()),
        }));
    }

    let body: Value = match serde_json::from_str(&text) {
        Ok(b) => b,
        Err(_) => {
            mark_unsupported(&endpoint);
            return Ok(json!({
                "supported": false,
                "reason": "该渠道余额接口返回的不是有效 JSON",
            }));
        }
    };
    if body.get("mode").and_then(|m| m.as_str()).is_none() {
        mark_unsupported(&endpoint);
        return Ok(json!({
            "supported": false,
            "reason": "该渠道未提供兼容的余额接口",
        }));
    }
    Ok(parse_sub2api_balance(&body))
}

/// 按 profile_id 查询指定渠道的余额（Tauri 命令）。
/// 若 profile_id 为空则退化为 get_balance 的行为（取首个可用渠道）。
#[tauri::command]
pub async fn get_channel_balance(profile_id: String) -> Result<serde_json::Value, String> {
    let cfg = config();
    if !cfg.enabled {
        return Ok(json!({ "supported": false, "reason": "余额查询未开启" }));
    }
    if !profile_id.is_empty() {
        if let Some(ch) = crate::proxy::channel_config(&profile_id) {
            return query_channel_balance(&ch.upstream_url, &ch.api_key, &cfg.currency).await;
        }
        return Ok(json!({ "supported": false, "reason": "未找到指定渠道" }));
    }
    get_balance().await
}

#[tauri::command]
pub async fn get_balance() -> Result<serde_json::Value, String> {
    let cfg = config();
    if !cfg.enabled {
        return Ok(json!({ "supported": false, "reason": "余额查询未开启" }));
    }
    // 免占槽早退：已知不支持余额的渠道不该为一次注定失败的轮询占用并发槽位。
    // 判据与探针事件共用 `is_balance_candidate`，避免两处口径分叉。
    if !is_balance_candidate(&crate::proxy::upstream_url()) {
        return Ok(json!({
            "supported": false,
            "reason": "当前上游未提供兼容的余额接口",
        }));
    }
    // 计入并发：走与代理转发同一条闸门（全局 + 首个启用渠道），
    // 避免余额轮询把槽位挤爆，也不会绕过渠道并发限制。
    // lease 在此作用域结束时自动 Drop 释放，无需手动 release。
    let lease = match crate::proxy::acquire_lease().await {
        Ok(l) => l,
        Err(e) => return Err(e),
    };
    // 用租约选中渠道的凭据，保证与所占槽位的渠道一致
    query_channel_balance(
        &lease.channel.upstream_url,
        &lease.channel.api_key,
        &cfg.currency,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn official_upstream_detection() {
        // 官方：带路径、裸域名、带端口都应命中
        assert!(is_official_upstream("https://api.deepseek.com/v1/responses"));
        assert!(is_official_upstream("https://api.deepseek.com"));
        assert!(is_official_upstream("https://API.DeepSeek.com/v1"));
        assert!(is_official_upstream("api.deepseek.com/v1"));
        assert!(is_official_upstream("https://api.deepseek.com:443/v1"));
        // 非官方中转站、空串、畸形 URL、仿冒域名
        assert!(!is_official_upstream("https://relay.example.com/v1/responses"));
        assert!(!is_official_upstream(""));
        assert!(!is_official_upstream("   "));
        assert!(!is_official_upstream("not a url"));
        assert!(!is_official_upstream("https://api.deepseek.com.evil.com/v1"));
        assert!(!is_official_upstream("https://foo.bar/api.deepseek.com"));
    }

    #[test]
    fn parse_single_currency() {
        let body = json!({
            "is_available": true,
            "balance_infos": [ {
                "currency": "CNY",
                "total_balance": "110.00",
                "granted_balance": "10.00",
                "topped_up_balance": "100.00"
            } ]
        });
        let out = parse_balance(&body, "CNY");
        assert_eq!(out["supported"], json!(true));
        assert_eq!(out["available"], json!(true));
        assert_eq!(out["currency"], json!("CNY"));
        assert_eq!(out["total"], json!("110.00"));
        assert_eq!(out["granted"], json!("10.00"));
        assert_eq!(out["topped_up"], json!("100.00"));
        assert_eq!(out["reason"], Value::Null);
    }

    #[test]
    fn parse_multi_currency_picks_target() {
        let body = json!({
            "is_available": true,
            "balance_infos": [
                { "currency": "USD", "total_balance": "20.00",
                  "granted_balance": "2.00", "topped_up_balance": "18.00" },
                { "currency": "CNY", "total_balance": "110.00",
                  "granted_balance": "10.00", "topped_up_balance": "100.00" }
            ]
        });
        let cny = parse_balance(&body, "CNY");
        assert_eq!(cny["available"], json!(true));
        assert_eq!(cny["currency"], json!("CNY"));
        assert_eq!(cny["total"], json!("110.00"));
        assert_eq!(cny["topped_up"], json!("100.00"));

        let usd = parse_balance(&body, "USD");
        assert_eq!(usd["available"], json!(true));
        assert_eq!(usd["currency"], json!("USD"));
        assert_eq!(usd["total"], json!("20.00"));
        assert_eq!(usd["granted"], json!("2.00"));
        assert_eq!(usd["topped_up"], json!("18.00"));
    }

    #[test]
    fn parse_missing_target_currency() {
        let body = json!({
            "is_available": true,
            "balance_infos": [
                { "currency": "USD", "total_balance": "20.00",
                  "granted_balance": "2.00", "topped_up_balance": "18.00" }
            ]
        });
        let out = parse_balance(&body, "CNY");
        assert_eq!(out["supported"], json!(true));
        assert_eq!(out["available"], json!(false));
        assert_eq!(out["currency"], json!("CNY"));
        assert_eq!(out["reason"], json!("账号无该币种余额"));
        assert!(out.get("total").is_none());

        // 数组整体缺失时同样按“无该币种”处理，不 panic
        let empty = parse_balance(&json!({}), "CNY");
        assert_eq!(empty["available"], json!(false));
        assert_eq!(empty["reason"], json!("账号无该币种余额"));
    }

    #[test]
    fn parse_unavailable_account() {
        let body = json!({
            "is_available": false,
            "balance_infos": [ {
                "currency": "CNY", "total_balance": "0.00",
                "granted_balance": "0.00", "topped_up_balance": "0.00"
            } ]
        });
        let out = parse_balance(&body, "CNY");
        assert_eq!(out["supported"], json!(true));
        assert_eq!(out["available"], json!(false));
        assert_eq!(out["total"], json!("0.00"));
    }

    #[test]
    fn config_json_roundtrip() {
        let cfg = BalanceConfig {
            enabled: true,
            currency: "USD".into(),
            interval_secs: 30,
        };
        let mut v = json!({});
        save_to_json(&mut v, &cfg);
        assert_eq!(v["balance"]["enabled"], json!(true));
        assert_eq!(v["balance"]["currency"], json!("USD"));
        assert_eq!(v["balance"]["interval_secs"], json!(30));

        let back = load_from_json(&v);
        assert_eq!(back.enabled, cfg.enabled);
        assert_eq!(back.currency, cfg.currency);
        assert_eq!(back.interval_secs, cfg.interval_secs);
    }

    #[test]
    fn load_tolerates_missing_and_invalid_fields() {
        // 完全缺失 → 默认值
        let d = load_from_json(&json!({}));
        assert!(!d.enabled);
        assert_eq!(d.currency, "CNY");
        assert_eq!(d.interval_secs, DEFAULT_INTERVAL_SECS);
        // 部分缺失 / 小写币种 / 非法币种
        let p = load_from_json(&json!({ "balance": { "enabled": true } }));
        assert!(p.enabled);
        assert_eq!(p.currency, "CNY");
        assert_eq!(p.interval_secs, DEFAULT_INTERVAL_SECS);
        assert_eq!(load_from_json(&json!({ "balance": { "currency": "usd" } })).currency, "USD");
        assert_eq!(load_from_json(&json!({ "balance": { "currency": "JPY" } })).currency, "CNY");
    }

    #[test]
    fn interval_is_clamped() {
        assert_eq!(
            load_from_json(&json!({ "balance": { "interval_secs": 0 } })).interval_secs,
            1
        );
        assert_eq!(
            load_from_json(&json!({ "balance": { "interval_secs": 99999 } })).interval_secs,
            1800
        );
        assert_eq!(
            load_from_json(&json!({ "balance": { "interval_secs": 60 } })).interval_secs,
            60
        );
    }

    #[test]
    fn usage_endpoint_derivation() {
        assert_eq!(
            usage_endpoint("https://sub.callai.one/v1/responses").as_deref(),
            Some("https://sub.callai.one/v1/usage")
        );
        assert_eq!(
            usage_endpoint("https://host/v1/chat/completions").as_deref(),
            Some("https://host/v1/usage")
        );
        // 带路径前缀的部署：取最后一个 /v1/
        assert_eq!(
            usage_endpoint("https://host/proxy/v1/responses").as_deref(),
            Some("https://host/proxy/v1/usage")
        );
        // 无 /v1/ 时回落到 origin
        assert_eq!(
            usage_endpoint("https://host").as_deref(),
            Some("https://host/v1/usage")
        );
        // 保留端口
        assert_eq!(
            usage_endpoint("http://127.0.0.1:8080/v1/responses").as_deref(),
            Some("http://127.0.0.1:8080/v1/usage")
        );
        // 空串 / 畸形地址一律 None：宁可不查，也不猜目标地址把 Key 发出去
        assert_eq!(usage_endpoint(""), None);
        assert_eq!(usage_endpoint("   "), None);
        assert_eq!(usage_endpoint("not a url"), None);
    }

    #[test]
    fn balance_candidate_covers_deepseek_and_sub2api() {
        assert!(is_balance_candidate("https://api.deepseek.com/v1/responses"));
        assert!(is_balance_candidate("https://sub.callai.one/v1/responses"));
        assert!(!is_balance_candidate(""));
        assert!(!is_balance_candidate("not a url"));
    }

    #[test]
    fn sub2api_wallet_mode_uses_balance() {
        // 形状取自 sub.callai.one/v1/usage 实测响应，金额为改写值
        let body = json!({
            "mode": "unrestricted",
            "isValid": true,
            "planName": "钱包余额",
            "balance": 21.18236255,
            "remaining": 21.18236255,
            "unit": "USD",
            "usage": { "today": { "requests": 1014 } },
            "model_stats": []
        });
        let out = parse_sub2api_balance(&body);
        assert_eq!(out["supported"], json!(true));
        assert_eq!(out["available"], json!(true));
        assert_eq!(out["currency"], json!("USD"));
        // 金额定型为两位小数字符串，前端不做浮点运算
        assert_eq!(out["total"], json!("21.18"));
        assert_eq!(out["mode"], json!("unrestricted"));
        assert_eq!(out["plan"], json!("钱包余额"));
        assert_eq!(out["granted"], Value::Null);
        assert_eq!(out["topped_up"], Value::Null);
        assert_eq!(out["reason"], Value::Null);
    }

    #[test]
    fn sub2api_quota_limited_falls_back_to_quota_remaining() {
        let body = json!({
            "mode": "quota_limited",
            "isValid": true,
            "status": "active",
            "quota": { "limit": 50.0, "used": 30.0, "remaining": 20.0, "unit": "USD" }
        });
        let out = parse_sub2api_balance(&body);
        assert_eq!(out["supported"], json!(true));
        assert_eq!(out["available"], json!(true));
        assert_eq!(out["total"], json!("20.00"));
        assert_eq!(out["mode"], json!("quota_limited"));
    }

    #[test]
    fn sub2api_subscription_mode_uses_remaining() {
        let body = json!({
            "mode": "unrestricted",
            "isValid": true,
            "planName": "Pro 订阅",
            "remaining": 7.5,
            "unit": "USD",
            "subscription": { "daily_limit_usd": 5.0, "daily_usage_usd": 1.0 }
        });
        let out = parse_sub2api_balance(&body);
        assert_eq!(out["available"], json!(true));
        assert_eq!(out["total"], json!("7.50"));
        assert_eq!(out["plan"], json!("Pro 订阅"));
    }

    #[test]
    fn sub2api_without_amount_is_supported_but_unavailable() {
        // 是 sub2api（带 mode）但没给金额：不算「不支持」，给出可读原因
        let body = json!({
            "mode": "unrestricted",
            "isValid": true,
            "planName": "钱包余额",
            "unit": "USD"
        });
        let out = parse_sub2api_balance(&body);
        assert_eq!(out["supported"], json!(true));
        assert_eq!(out["available"], json!(false));
        assert_eq!(out["reason"], json!("余额接口未返回可用金额"));

        // unit 缺失时回落到 USD；字符串金额同样定型为两位小数
        let no_unit = json!({ "mode": "unrestricted", "balance": "1.5" });
        let out2 = parse_sub2api_balance(&no_unit);
        assert_eq!(out2["currency"], json!("USD"));
        assert_eq!(out2["total"], json!("1.5"));
    }
}
