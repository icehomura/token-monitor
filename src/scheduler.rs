//! 渠道调度器：根据并发 / RPM / TPM 限制和权重，智能选择最优渠道。
//!
//! - RPM：维护最近 60 秒的请求时间戳
//! - TPM：维护最近 60 秒的 token 消耗
//! - 调度算法：过滤不可用渠道后，按权重加权随机选择

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

// ──────── 窗口期限常量 ────────

const WINDOW_SECS: u64 = 60;

// ──────── RPM 滑动窗口 ────────

struct RpmWindow {
    timestamps: VecDeque<Instant>,
}

impl RpmWindow {
    fn new() -> Self {
        Self {
            timestamps: VecDeque::new(),
        }
    }

    /// 移除过期时间戳，返回当前 RPM
    fn count(&mut self) -> u64 {
        let cutoff = Instant::now() - Duration::from_secs(WINDOW_SECS);
        while self.timestamps.front().is_some_and(|t| *t < cutoff) {
            self.timestamps.pop_front();
        }
        self.timestamps.len() as u64
    }

    /// 记录一次请求
    fn record(&mut self) {
        self.timestamps.push_back(Instant::now());
    }
}

// ──────── TPM 滑动窗口 ────────

struct TpmWindow {
    entries: VecDeque<(Instant, u64)>,
}

impl TpmWindow {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    /// 移除过期条目，返回当前 token 消耗
    fn count(&mut self) -> u64 {
        let cutoff = Instant::now() - Duration::from_secs(WINDOW_SECS);
        while self.entries.front().is_some_and(|(t, _)| *t < cutoff) {
            self.entries.pop_front();
        }
        self.entries.iter().map(|(_, tokens)| tokens).sum()
    }

    /// 记录一次请求的 token 消耗
    fn record(&mut self, tokens: u64) {
        self.entries.push_back((Instant::now(), tokens));
    }
}

// ──────── 渠道运行时状态 ────────

/// 渠道运行时状态。配置字段使用原子类型，支持通过 `&self` 更新。
pub struct ChannelState {
    pub profile_id: String,
    enabled: AtomicBool,
    max_concurrency: AtomicUsize,
    current_concurrency: AtomicUsize,
    max_rpm: AtomicU64,
    rpm_window: Mutex<RpmWindow>,
    max_tpm: AtomicU64,
    tpm_window: Mutex<TpmWindow>,
    weight: AtomicU32,
}

use std::sync::atomic::AtomicU64;

impl ChannelState {
    pub fn new(profile_id: String, max_concurrency: usize, max_rpm: u64, max_tpm: u64, weight: u32) -> Self {
        Self {
            profile_id,
            enabled: AtomicBool::new(true),
            max_concurrency: AtomicUsize::new(max_concurrency),
            current_concurrency: AtomicUsize::new(0),
            max_rpm: AtomicU64::new(max_rpm),
            rpm_window: Mutex::new(RpmWindow::new()),
            max_tpm: AtomicU64::new(max_tpm),
            tpm_window: Mutex::new(TpmWindow::new()),
            weight: AtomicU32::new(weight),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, val: bool) {
        self.enabled.store(val, Ordering::Relaxed);
    }

    pub fn max_concurrency(&self) -> usize {
        self.max_concurrency.load(Ordering::Relaxed)
    }

    pub fn set_max_concurrency(&self, val: usize) {
        self.max_concurrency.store(val, Ordering::Relaxed);
    }

    pub fn max_rpm(&self) -> u64 {
        self.max_rpm.load(Ordering::Relaxed)
    }

    pub fn set_max_rpm(&self, val: u64) {
        self.max_rpm.store(val, Ordering::Relaxed);
    }

    pub fn max_tpm(&self) -> u64 {
        self.max_tpm.load(Ordering::Relaxed)
    }

    pub fn set_max_tpm(&self, val: u64) {
        self.max_tpm.store(val, Ordering::Relaxed);
    }

    pub fn weight(&self) -> u32 {
        self.weight.load(Ordering::Relaxed)
    }

    pub fn set_weight(&self, val: u32) {
        self.weight.store(val, Ordering::Relaxed);
    }
}

// ──────── 调度器 ────────

pub struct Scheduler {
    channels: RwLock<Vec<ChannelState>>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            channels: RwLock::new(Vec::new()),
        }
    }

    /// 新增或更新渠道（保存 profile 时调用）。
    /// 已存在则就地更新配置，不存在则创建。
    pub fn upsert_channel(
        &self,
        profile_id: &str,
        enabled: bool,
        max_concurrency: usize,
        max_rpm: u64,
        max_tpm: u64,
        weight: u32,
    ) {
        let mut channels = self.channels.write().unwrap();
        if let Some(ch) = channels.iter().find(|c| c.profile_id == profile_id) {
            ch.set_enabled(enabled);
            ch.set_max_concurrency(max_concurrency);
            ch.set_max_rpm(max_rpm);
            ch.set_max_tpm(max_tpm);
            ch.set_weight(weight);
            return;
        }
        let ch = ChannelState::new(profile_id.to_string(), max_concurrency, max_rpm, max_tpm, weight);
        ch.set_enabled(enabled);
        channels.push(ch);
    }

    /// 移除渠道（删除 profile 时调用）
    pub fn remove_channel(&self, profile_id: &str) {
        let mut channels = self.channels.write().unwrap();
        channels.retain(|c| c.profile_id != profile_id);
    }

    // ──────── 核心调度 ────────

    /// 检查渠道是否达到限制（并发 / RPM / TPM）
    fn check_limits(channel: &ChannelState) -> bool {
        if !channel.is_enabled() {
            return false;
        }
        if channel.current_concurrency.load(Ordering::Relaxed) >= channel.max_concurrency() {
            return false;
        }
        if channel.max_rpm() > 0 {
            let mut rpm = channel.rpm_window.lock().unwrap();
            if rpm.count() >= channel.max_rpm() {
                return false;
            }
        }
        if channel.max_tpm() > 0 {
            let mut tpm = channel.tpm_window.lock().unwrap();
            if tpm.count() >= channel.max_tpm() {
                return false;
            }
        }
        true
    }

    /// 根据权重和限制选择最优渠道。
    ///
    /// 调度算法：
    /// 1. 过滤 enabled=false 的渠道
    /// 2. 过滤已达到并发上限的渠道
    /// 3. 过滤已达到 RPM / TPM 上限的渠道
    /// 4. 对剩余渠道按权重加权随机选择
    /// 5. 所有渠道都满了返回 None
    pub fn select_channel(&self) -> Option<String> {
        let channels = self.channels.read().unwrap();

        let mut candidates: Vec<(&ChannelState, u64)> = Vec::new();
        let mut total_weight: u64 = 0;

        for ch in channels.iter() {
            if Self::check_limits(ch) {
                let w = ch.weight().max(1) as u64;
                total_weight += w;
                candidates.push((ch, total_weight));
            }
        }

        if candidates.is_empty() {
            return None;
        }

        let pick = random_u64() % total_weight;
        let selected = candidates.iter().find(|(_, cum)| pick < *cum).unwrap();
        Some(selected.0.profile_id.clone())
    }

    /// 第一个已启用渠道的 id。
    /// 探针 / 余额查询需要一个具体的上游目标，动态调度下用首个启用渠道代表。
    pub fn first_enabled_id(&self) -> Option<String> {
        self.channels
            .read()
            .unwrap()
            .iter()
            .find(|c| c.is_enabled())
            .map(|c| c.profile_id.clone())
    }

    /// 是否存在已启用的渠道。
    /// 用于区分「所有渠道被禁用」（应立即报错）与「所有渠道已满」（应排队等待）。
    pub fn has_enabled_channel(&self) -> bool {
        self.channels.read().unwrap().iter().any(|c| c.is_enabled())
    }

    /// 已启用渠道的并发上限之和，作为全局并发闸门。
    /// 不能沿用单个渠道的上限，否则大并发渠道会被小渠道的上限卡住。
    pub fn total_concurrency(&self) -> usize {
        self.channels
            .read()
            .unwrap()
            .iter()
            .filter(|c| c.is_enabled())
            .map(|c| c.max_concurrency())
            .sum()
    }

    // ──────── 并发槽位管理 ────────

    /// 获取并发槽位。成功返回 true，已满返回 false。
    pub fn acquire_slot(&self, profile_id: &str) -> bool {
        let channels = self.channels.read().unwrap();
        if let Some(ch) = channels.iter().find(|c| c.profile_id == profile_id) {
            let cur = ch.current_concurrency.load(Ordering::Relaxed);
            if cur < ch.max_concurrency() {
                ch.current_concurrency.store(cur + 1, Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    /// 释放并发槽位
    pub fn release_slot(&self, profile_id: &str) {
        let channels = self.channels.read().unwrap();
        if let Some(ch) = channels.iter().find(|c| c.profile_id == profile_id) {
            let cur = ch.current_concurrency.load(Ordering::Relaxed);
            if cur > 0 {
                ch.current_concurrency.store(cur - 1, Ordering::Relaxed);
            }
        }
    }

    // ──────── 请求记录 ────────

    /// 记录一次请求到 RPM / TPM 滑动窗口
    pub fn record_request(&self, profile_id: &str, tokens: u64) {
        let channels = self.channels.read().unwrap();
        if let Some(ch) = channels.iter().find(|c| c.profile_id == profile_id) {
            ch.rpm_window.lock().unwrap().record();
            ch.tpm_window.lock().unwrap().record(tokens);
        }
    }

    // ──────── 查询接口 ────────

    /// 获取所有渠道的运行时快照（用于前端展示）
    pub fn snapshot(&self) -> Vec<ChannelSnapshot> {
        let channels = self.channels.read().unwrap();
        channels
            .iter()
            .map(|ch| ChannelSnapshot {
                profile_id: ch.profile_id.clone(),
                enabled: ch.is_enabled(),
                current_concurrency: ch.current_concurrency.load(Ordering::Relaxed),
                max_concurrency: ch.max_concurrency(),
                current_rpm: ch.rpm_window.lock().unwrap().count(),
                max_rpm: ch.max_rpm(),
                current_tpm: ch.tpm_window.lock().unwrap().count(),
                max_tpm: ch.max_tpm(),
                weight: ch.weight(),
            })
            .collect()
    }
}

// ──────── 渠道快照（前端展示用） ────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct ChannelSnapshot {
    pub profile_id: String,
    pub enabled: bool,
    pub current_concurrency: usize,
    pub max_concurrency: usize,
    pub current_rpm: u64,
    pub max_rpm: u64,
    pub current_tpm: u64,
    pub max_tpm: u64,
    pub weight: u32,
}

// ──────── 伪随机数（无外部依赖） ────────

fn random_u64() -> u64 {
    // 基于纳秒时间戳的 xorshift 风格混合，避免引入 rand crate
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    nanos.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rpm_window() {
        let mut w = RpmWindow::new();
        w.record();
        w.record();
        assert_eq!(w.count(), 2);
    }

    #[test]
    fn test_tpm_window() {
        let mut w = TpmWindow::new();
        w.record(100);
        w.record(200);
        assert_eq!(w.count(), 300);
    }

    #[test]
    fn test_acquire_release_slot() {
        let s = Scheduler::new();
        s.upsert_channel("ch1", true, 2, 0, 0, 100);
        assert!(s.acquire_slot("ch1"));
        assert!(s.acquire_slot("ch1"));
        assert!(!s.acquire_slot("ch1")); // 已满
        s.release_slot("ch1");
        assert!(s.acquire_slot("ch1")); // 释放后可再获取
    }

    #[test]
    fn test_select_channel_respects_limits() {
        let s = Scheduler::new();
        s.upsert_channel("ch1", true, 1, 0, 0, 100);
        s.upsert_channel("ch2", true, 1, 0, 0, 100);

        // ch1 占满并发
        assert!(s.acquire_slot("ch1"));
        // 应该选 ch2
        let selected = s.select_channel();
        assert_eq!(selected.as_deref(), Some("ch2"));
    }

    #[test]
    fn test_select_channel_all_disabled() {
        let s = Scheduler::new();
        s.upsert_channel("ch1", false, 1, 0, 0, 100);
        assert_eq!(s.select_channel(), None);
        // 全禁用时应当能被识别出来，区别于「全满」
        assert!(!s.has_enabled_channel());
        assert_eq!(s.total_concurrency(), 0);
    }

    /// 回归保护：upsert 必须能新增，也能就地更新已有渠道
    #[test]
    fn test_upsert_channel_creates_then_updates() {
        let s = Scheduler::new();
        s.upsert_channel("ch1", true, 5, 10, 1000, 100);
        assert_eq!(s.snapshot().len(), 1);
        assert_eq!(s.total_concurrency(), 5);

        // 再次 upsert 同一个 id 应就地更新，而不是追加
        s.upsert_channel("ch1", false, 30, 60, 9000, 200);
        let snap = s.snapshot();
        assert_eq!(snap.len(), 1, "upsert 不应产生重复渠道");
        assert_eq!(snap[0].max_concurrency, 30);
        assert_eq!(snap[0].max_rpm, 60);
        assert_eq!(snap[0].max_tpm, 9000);
        assert_eq!(snap[0].weight, 200);
        assert!(!snap[0].enabled);
        // 禁用渠道不计入全局并发闸门
        assert_eq!(s.total_concurrency(), 0);
    }

    /// 回归保护：删除渠道后不应再参与调度
    #[test]
    fn test_remove_channel() {
        let s = Scheduler::new();
        s.upsert_channel("ch1", true, 5, 0, 0, 100);
        s.upsert_channel("ch2", true, 5, 0, 0, 100);
        assert_eq!(s.total_concurrency(), 10);

        s.remove_channel("ch1");
        assert_eq!(s.snapshot().len(), 1);
        assert_eq!(s.total_concurrency(), 5);
        assert_eq!(s.select_channel().as_deref(), Some("ch2"));
    }

    #[test]
    fn test_record_request() {
        let s = Scheduler::new();
        s.upsert_channel("ch1", true, 10, 0, 0, 100);
        s.record_request("ch1", 500);
        let snap = s.snapshot();
        assert_eq!(snap[0].current_rpm, 1);
        assert_eq!(snap[0].current_tpm, 500);
    }
}
