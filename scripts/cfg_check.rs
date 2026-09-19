//! cfg 分支独占性验证：宿主机 / macOS / Linux 三个目标上，
//! app_data_override 必须恰好有一个定义（重复定义或多目标漏定义都会编译失败）。
//! 复刻自 src/main.rs，仅用于本地验证 cfg 谓词，不参与应用构建。

#![allow(dead_code)]

#[cfg(target_os = "macos")]
pub(crate) fn app_data_override() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let dir = home.join("Library/Application Support/com.icehomura.token-monitor");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir)
}

#[cfg(target_os = "linux")]
pub(crate) fn app_data_override() -> Option<std::path::PathBuf> {
    std::env::var_os("APPIMAGE")
        .map(std::path::PathBuf::from)
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn app_data_override() -> Option<std::path::PathBuf> {
    None
}

/// 调用点：确认函数在三个目标上都能解析到
pub fn probe_dir() -> Option<std::path::PathBuf> {
    app_data_override()
}
