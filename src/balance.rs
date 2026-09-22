//! DeepSeek 账户余额查询。
//!
//! 仅当上游为 DeepSeek 官方（host == api.deepseek.com）时才发起查询；
//! 中转站上游一律跳过并返回原因，因为中转 Key 打官方余额接口没有意义。
//! 配置挂在 token-monitor.json 的 `balance` 子对象下。
//!
//! 余额接口 `GET https://api.deepseek.com/user/balance` 返回形如：
//! `{ "is_available": true, "balance_infos": [ { "currency": "CNY",
//!    "total_balance": "110.00", "granted_balance": "10.00",
//!    "topped_up_balance": "100.00" } ] }`
//!
//! 金额一律以字符串原样透传给前端，绝不解析成浮点，避免精度损失。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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

    // 落盘：读改写整个 JSON，保留文件中的其它配置（与 probe / codeg 同范式）
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

#[tauri::command]
pub async fn get_balance() -> Result<serde_json::Value, String> {
    let cfg = config();
    if !cfg.enabled {
        return Ok(json!({ "supported": false, "reason": "余额查询未开启" }));
    }
    if !is_official_upstream(&crate::proxy::upstream_url()) {
        return Ok(json!({
            "supported": false,
            "reason": "当前上游非 DeepSeek 官方，余额查询不可用",
        }));
    }
    let api_key = crate::proxy::default_api_key().trim().to_string();
    if api_key.is_empty() {
        return Err("API Key 未配置，无法查询余额".into());
    }

    // 计入并发：与代理请求共享同一并发上限，避免余额轮询把槽位挤爆。
    // guard 在此作用域结束时自动 Drop 释放，无需手动 release。
    let _guard = match crate::proxy::acquire_slot().await {
        Some(g) => g,
        None => return Err("并发已满，等待超时".into()),
    };

    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| format!("余额查询 HTTP 客户端创建失败：{e}"))?;
    let resp = client
        .get(BALANCE_ENDPOINT)
        .bearer_auth(&api_key)
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
    Ok(parse_balance(&body, &cfg.currency))
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
        assert!(!is_official_upstream("https://666sub2.i7a.top/v1/responses"));
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
}
