#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod proxy;
mod stats;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{
    Emitter, Listener, Manager,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    WindowEvent,
};

// ──────── Profile 数据结构 ────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Profile {
    id: String,
    name: String,
    upstream_url: String,
    api_key: String,
    model_override: String,
    max_concurrency: usize,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            id: uuid_simple(),
            name: "默认".into(),
            upstream_url: String::new(),
            api_key: String::new(),
            model_override: String::new(),
            max_concurrency: 20,
        }
    }
}

fn uuid_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{:x}-{:04x}", t.as_secs(), t.subsec_nanos() & 0xFFFF)
}

#[derive(Serialize)]
struct StatsResponse {
    buckets: Vec<stats::MinuteBucket>,
    window_minutes: u32,
    concurrency: u64,
}

#[tauri::command]
fn get_stats(window_minutes: i32, start_ms: Option<i64>, end_ms: Option<i64>) -> StatsResponse {
    // 自定义时间范围模式：前端传入绝对时间戳
    if let (Some(start), Some(end)) = (start_ms, end_ms) {
        let buckets = stats::buckets_range(start, end);
        let total_minutes = ((end - start) / 60000).max(1) as u32;
        return StatsResponse {
            buckets,
            window_minutes: total_minutes,
            concurrency: stats::active(),
        };
    }
    let window = match window_minutes {
        5 | 30 | 60 | 300 => window_minutes as u32,
        0 => {
            // 今日：本地零点到现在，含当前不完整分钟
            use chrono::Timelike;
            let now = chrono::Local::now();
            ((now.hour() * 60 + now.minute()) as u32).max(1)
        }
        -1 => {
            // 本周：周一零点到现在
            use chrono::{Datelike, Timelike};
            let now = chrono::Local::now();
            let weekday = now.weekday().num_days_from_monday(); // 0=Mon .. 6=Sun
            let today_minutes = (now.hour() * 60 + now.minute()) as u64;
            (weekday as u64 * 24 * 60 + today_minutes).max(1) as u32
        }
        _ => 10,
    };
    StatsResponse {
        buckets: stats::buckets(window),
        window_minutes: window,
        concurrency: stats::active(),
    }
}

#[tauri::command]
fn get_server_info() -> serde_json::Value {
    let c = proxy::cfg();
    serde_json::json!({
        "port": c.port,
        "endpoint": format!("http://127.0.0.1:{}/v1", c.port),
        "endpoints": [
            "/v1/chat/completions",
            "/v1/responses",
            "/v1/messages",
        ],
        "model_override": c.model_override,
    })
}

fn proxy_port() -> u16 {
    std::env::var("TOKEN_MONITOR_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8188)
}

fn config_path() -> Option<std::path::PathBuf> {
    [
        std::env::current_exe().ok().map(|d| {
            d.parent()
                .unwrap_or(std::path::Path::new("."))
                .join("token-monitor.json")
        }),
        std::env::current_dir().ok().map(|d| d.join("token-monitor.json")),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.exists())
    .or_else(|| {
        // 不存在时默认写到 exe 旁边
        std::env::current_exe()
            .ok()
            .and_then(|d| d.parent().map(|p| p.join("token-monitor.json")))
    })
}

fn save_port(port: u16) -> Result<(), String> {
    let path = config_path().ok_or("无法确定配置文件路径")?;
    let mut v: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    v["port"] = json!(port);
    if v.get("api_key").is_none() {
        v["api_key"] = json!(proxy::cfg().api_key);
    }
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap())
        .map_err(|e| format!("写入配置失败：{e}"))?;
    println!("saved port {} to {}", port, path.display());
    Ok(())
}

/// 保存模型名与 API Key：写入配置文件并立即热更新运行时（无需重启服务）
#[tauri::command]
fn set_model_config(api_key: String, model_override: String) -> Result<serde_json::Value, String> {
    let path = config_path().ok_or("无法确定配置文件路径")?;
    let mut v: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let key_trimmed = api_key.trim().to_string();
    let model_trimmed = model_override.trim().to_string();
    if !key_trimmed.is_empty() {
        v["api_key"] = json!(key_trimmed);
    }
    v["model_override"] = json!(model_trimmed); // 空字符串 = 不覆盖，用请求原始 model
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap())
        .map_err(|e| format!("写入配置失败：{e}"))?;

    // 热更新运行时配置（后续请求立即生效）
    proxy::update_runtime(
        (!key_trimmed.is_empty()).then_some(key_trimmed.clone()),
        Some(model_trimmed.clone()),
        None,
        None,
    );

    println!("saved model config to {}", path.display());
    if let Some(handle) = proxy::app_handle() {
        let _ = handle.emit("server-info-changed", ());
    }
    Ok(serde_json::json!({"model_override": model_trimmed, "has_api_key": !key_trimmed.is_empty()}))
}

#[derive(Serialize)]
struct SettingsInfo {
    port: u16,
    model_override: String,
    has_api_key: bool,
    upstream_url: String,
    max_concurrency: usize,
    active_profile_id: String,
}

#[tauri::command]
fn get_settings() -> SettingsInfo {
    let c = proxy::cfg();
    let active_id = read_saved_config_str("active_profile_id");
    SettingsInfo {
        port: c.port,
        model_override: c.model_override.clone(),
        has_api_key: !c.api_key.is_empty(),
        upstream_url: proxy::upstream_url(),
        max_concurrency: c.max_concurrency,
        active_profile_id: active_id,
    }
}

/// 保存转发目标地址并立即热更新（无需重启服务）
#[tauri::command]
fn set_upstream(url: String) -> Result<serde_json::Value, String> {
    let url = url.trim().to_string();
    if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("地址必须以 http:// 或 https:// 开头".into());
    }
    let path = config_path().ok_or("无法确定配置文件路径")?;
    let mut v: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    v["upstream_url"] = json!(url); // 空字符串 = 恢复默认地址
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap())
        .map_err(|e| format!("写入配置失败：{e}"))?;

    proxy::update_runtime(None, None, Some(url.clone()), None);
    println!("saved upstream url to {}", path.display());
    if let Some(handle) = proxy::app_handle() {
        let _ = handle.emit("server-info-changed", ());
    }
    Ok(serde_json::json!({"upstream_url": if url.is_empty() { proxy::upstream_url() } else { url }}))
}

/// 保存新端口并重启代理服务（重新监听）
#[tauri::command]
async fn set_port(port: u16) -> Result<serde_json::Value, String> {
    if port == 0 {
        return Err("端口范围 1-65535".into());
    }
    save_port(port)?;
    proxy::restart_server(port).await?;
    Ok(serde_json::json!({"port": port, "endpoint": format!("http://127.0.0.1:{}/v1", port)}))
}

/// 获取关闭按钮行为配置：quit / minimize / ask（默认 ask）
#[tauri::command]
fn get_close_action() -> serde_json::Value {
    let action = read_saved_config_str("close_action");
    let action = if action.is_empty() { "ask".into() } else { action };
    serde_json::json!({ "action": action })
}

/// 设置关闭按钮行为
#[tauri::command]
fn set_close_action(action: String) -> Result<serde_json::Value, String> {
    let valid = ["quit", "minimize", "ask"];
    if !valid.contains(&action.as_str()) {
        return Err(format!("无效值：{action}，可选 quit / minimize / ask"));
    }
    let path = config_path().ok_or("无法确定配置文件路径")?;
    let mut v: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    v["close_action"] = json!(action);
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap())
        .map_err(|e| format!("写入配置失败：{e}"))?;
    println!("saved close_action={action} to {}", path.display());
    Ok(serde_json::json!({ "action": action }))
}

/// 获取开机自启状态
#[tauri::command]
fn get_autostart(app: tauri::AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// 设置开机自启
#[tauri::command]
fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    if enabled {
        app.autolaunch().enable().map_err(|e| e.to_string())?;
    } else {
        app.autolaunch().disable().map_err(|e| e.to_string())?;
    }
    Ok(app.autolaunch().is_enabled().unwrap_or(false))
}

// ──────── Profile 管理 ────────

fn read_profiles_from_config() -> (Vec<Profile>, String) {
    let path = match config_path() {
        Some(p) => p,
        None => return (vec![], String::new()),
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return (vec![], String::new()),
    };
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(json!({}));
    let profiles: Vec<Profile> = v.get("profiles")
        .and_then(|p| serde_json::from_value(p.clone()).ok())
        .unwrap_or_default();
    let active_id = v.get("active_profile_id")
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_string();
    (profiles, active_id)
}

fn write_profiles_to_config(profiles: &[Profile], active_id: &str) -> Result<(), String> {
    let path = config_path().ok_or("无法确定配置文件路径")?;
    let mut v: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));
    v["profiles"] = serde_json::to_value(profiles).unwrap_or(json!([]));
    v["active_profile_id"] = json!(active_id);
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap())
        .map_err(|e| format!("写入配置失败：{e}"))
}

/// 获取所有配置文件
#[tauri::command]
fn get_profiles() -> serde_json::Value {
    let (profiles, active_id) = read_profiles_from_config();
    serde_json::json!({
        "profiles": profiles,
        "active_profile_id": active_id,
    })
}

/// 保存配置文件（新增或更新）
#[tauri::command]
fn save_profile(profile: Profile) -> Result<serde_json::Value, String> {
    let (mut profiles, active_id) = read_profiles_from_config();
    let is_active = profile.id == active_id;
    if let Some(existing) = profiles.iter_mut().find(|p| p.id == profile.id) {
        *existing = profile.clone();
    } else {
        profiles.push(profile.clone());
    }
    write_profiles_to_config(&profiles, &active_id)?;
    // 如果保存的是当前激活的配置，立即应用到运行时
    if is_active {
        apply_profile(&profile)?;
    }
    Ok(serde_json::json!({ "profile": profile, "profiles": profiles }))
}

/// 删除配置文件
#[tauri::command]
fn delete_profile(id: String) -> Result<serde_json::Value, String> {
    let (mut profiles, active_id) = read_profiles_from_config();
    profiles.retain(|p| p.id != id);
    let new_active = if active_id == id {
        profiles.first().map(|p| p.id.clone()).unwrap_or_default()
    } else {
        active_id.clone()
    };
    write_profiles_to_config(&profiles, &new_active)?;
    // 如果删除的是当前激活的，切换到新的
    if active_id == id {
        if let Some(p) = profiles.first() {
            apply_profile(p)?;
        }
    }
    Ok(serde_json::json!({ "profiles": profiles, "active_profile_id": new_active }))
}

/// 切换激活的配置文件并立即生效
#[tauri::command]
fn set_active_profile(id: String) -> Result<serde_json::Value, String> {
    let (profiles, _old_id) = read_profiles_from_config();
    let profile = profiles.iter().find(|p| p.id == id)
        .ok_or_else(|| format!("配置文件不存在：{id}"))?;
    apply_profile(profile)?;
    write_profiles_to_config(&profiles, &id)?;
    Ok(serde_json::json!({ "active_profile_id": id }))
}

/// 将 Profile 的设置应用到运行时
fn apply_profile(p: &Profile) -> Result<(), String> {
    proxy::update_runtime(
        if p.api_key.is_empty() { None } else { Some(p.api_key.clone()) },
        Some(p.model_override.clone()),
        Some(p.upstream_url.clone()),
        Some(p.max_concurrency),
    );
    println!("[main] 已切换到配置文件：{}", p.name);
    // 通知前端刷新服务端信息（toolbar 地址栏、模型名等）
    if let Some(handle) = proxy::app_handle() {
        let _ = handle.emit("server-info-changed", ());
    }
    Ok(())
}

fn read_api_key() -> String {
    // 优先环境变量；其次工程根目录 token-monitor.json {"api_key": "..."}
    if let Ok(k) = std::env::var("SUB2API_API_KEY") {
        let k = k.trim().to_string();
        if !k.is_empty() {
            return k;
        }
    }
    let candidates: Vec<std::path::PathBuf> = [
        std::env::current_dir().ok().map(|d| d.join("token-monitor.json")),
        std::env::current_exe().ok().map(|d| {
            d.parent()
                .unwrap_or(std::path::Path::new("."))
                .join("token-monitor.json")
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    for candidate in candidates {
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(k) = v.get("api_key").and_then(|k| k.as_str()) {
                    let k = k.trim();
                    if !k.is_empty() {
                        println!("loaded api key from {}", candidate.display());
                        return k.to_string();
                    }
                }
            }
        }
    }
    String::new()
}

fn read_saved_config_str(key: &str) -> String {
    let candidates: Vec<std::path::PathBuf> = [
        std::env::current_exe().ok().map(|d| {
            d.parent()
                .unwrap_or(std::path::Path::new("."))
                .join("token-monitor.json")
        }),
        std::env::current_dir().ok().map(|d| d.join("token-monitor.json")),
    ]
    .into_iter()
    .flatten()
    .collect();
    for candidate in candidates {
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(m) = v.get(key).and_then(|m| m.as_str()) {
                    return m.trim().to_string();
                }
            }
        }
    }
    String::new()
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .setup(|app| {            // 初始化代理配置并启动后端服务（随程序自动运行）
            let saved_model = read_saved_config_str("model_override");
            let saved_upstream = read_saved_config_str("upstream_url");
            let saved_max_conc: usize = read_saved_config_str("max_concurrency")
                .parse()
                .unwrap_or(20);
            let mut cfg = proxy::ProxyConfig {
                api_key: read_api_key(),
                model_override: {
                    let env_m = std::env::var("SUB2API_MODEL_OVERRIDE")
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if env_m.is_empty() { saved_model } else { env_m }
                },
                port: proxy_port(),
                upstream_url: {
                    let env_u = std::env::var("SUB2API_UPSTREAM")
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if env_u.is_empty() { saved_upstream } else { env_u }
                },
                max_concurrency: saved_max_conc,
            };
            // 如果存在已激活的配置文件，用其值覆盖扁平变量（旧变量已弃用）
            let (profiles, active_id) = read_profiles_from_config();
            if !active_id.is_empty() {
                if let Some(active) = profiles.iter().find(|p| p.id == active_id) {
                    println!("[main] 启动时应用配置文件：{}", active.name);
                    if !active.api_key.is_empty() {
                        cfg.api_key = active.api_key.clone();
                    }
                    if !active.model_override.is_empty() {
                        cfg.model_override = active.model_override.clone();
                    }
                    if !active.upstream_url.is_empty() {
                        cfg.upstream_url = active.upstream_url.clone();
                    }
                    cfg.max_concurrency = active.max_concurrency;
                }
            }
            if cfg.api_key.is_empty() {
                eprintln!(
                    "警告：未配置 SUB2API_API_KEY，也未找到 token-monitor.json，代理将返回配置错误"
                );
            }
            // 同步初始化代理配置（前端 WebView 可能立即查询）
            proxy::init(app.handle().clone(), cfg.clone());

            // 立即启动代理服务（不等 DB 初始化）
            tauri::async_runtime::spawn(async move {
                if let Err(e) = proxy::restart_server(cfg.port).await {
                    eprintln!("代理服务启动失败：{e}");
                }
            });

            // DB 初始化完全独立后台线程（60 万行 dedupe 可能要几十秒）
            tauri::async_runtime::spawn(async {
                tokio::task::spawn_blocking(|| stats::init_db()).await.ok();
            });



            // 托盘：显示窗口 / 重启窗口 / 退出
            let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
            let restart = MenuItem::with_id(app, "restart", "重启窗口", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &restart, &quit])?;

            TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Token Monitor - RPM/TPM 统计")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                        }
                    }
                    "restart" => {
                        // 只重启 WebView 窗口，代理服务保持运行
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.hide();
                            let _ = w.eval("location.reload()");
                            // 等一小段时间让 reload 生效后再显示
                            let handle = app.clone();
                            tauri::async_runtime::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                                if let Some(w) = handle.get_webview_window("main") {
                                    let _ = w.show();
                                    let _ = w.set_focus();
                                }
                            });
                        }
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // 左键单击托盘图标切换窗口显隐
                    if let tauri::tray::TrayIconEvent::Click { button: tauri::tray::MouseButton::Left, button_state: tauri::tray::MouseButtonState::Up, .. } = event {
                        if let Some(w) = tray.app_handle().get_webview_window("main") {
                            if w.is_visible().unwrap_or(false) && w.is_focused().unwrap_or(false) {
                                let _ = w.hide();
                            } else {
                                let _ = w.show();
                                let _ = w.unminimize();
                                let _ = w.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            // 监听前端关闭选择事件
            let handle = app.handle().clone();
            app.listen("close-choice", move |event| {
                if let Ok(payload) = serde_json::from_str::<serde_json::Value>(event.payload()) {
                    let choice = payload.get("choice").and_then(|c| c.as_str()).unwrap_or("minimize");
                    let remember = payload.get("remember").and_then(|r| r.as_bool()).unwrap_or(false);
                    // 如果用户勾选"记住"，更新配置
                    if remember {
                        let _ = (|| -> Result<(), String> {
                            let path = config_path().ok_or("无法确定配置文件路径")?;
                            let mut v: serde_json::Value = std::fs::read_to_string(&path)
                                .ok()
                                .and_then(|t| serde_json::from_str(&t).ok())
                                .unwrap_or_else(|| serde_json::json!({}));
                            v["close_action"] = json!(choice);
                            std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap())
                                .map_err(|e| format!("写入配置失败：{e}"))
                        })();
                    }
                    match choice {
                        "quit" => { handle.exit(0); }
                        "minimize" => {
                            if let Some(w) = handle.get_webview_window("main") {
                                let _ = w.hide();
                            }
                        }
                        _ => {}
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_stats,
            get_server_info,
            get_settings,
            set_port,
            set_model_config,
            set_upstream,
            get_close_action,
            set_close_action,
            get_autostart,
            set_autostart,
            get_profiles,
            save_profile,
            delete_profile,
            set_active_profile,
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let action = read_saved_config_str("close_action");
                match action.as_str() {
                    "quit" => { /* 不阻止，程序退出 */ }
                    "minimize" => {
                        let _ = window.hide();
                        api.prevent_close();
                    }
                    _ => {
                        // "ask" 或未配置：发送事件让前端显示弹窗
                        api.prevent_close();
                        let _ = window.emit("close-requested", ());
                    }
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
