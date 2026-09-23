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

// ──────── AIMD 自适应并发控制 ────────
//
// 用户配置的 `max_concurrency` 是**天花板**，实际放行量由 AIMD 动态收敛：
// 上游 429 说明打得太快 → 乘性降速；连续成功 → 加性提速，逐步探回天花板。
// 上游额度变化时无需人工改配置。
//
// 为什么不直接用配置值：429 只出现在上游，本地调度器原本对此一无所知，
// 于是「配置 20 并发」会在上游早已限流时继续猛打，把 429 放大成雪崩。

/// 每次成功最多增加的并发数（加性增）
const ADAPTIVE_INCREASE_STEP: usize = 1;
/// 429 时按当前上限的 1/N 降速（乘性减）
const ADAPTIVE_DECREASE_DIVISOR: usize = 4;
/// 自适应下限：永不为 0，否则渠道会彻底不可用
const ADAPTIVE_MIN_LIMIT: usize = 1;
/// 两次提速之间的最小间隔。降速有独立的（更长的）冷却，见 `ADAPTIVE_DECREASE_COOLDOWN`。
const ADAPTIVE_INCREASE_COOLDOWN: Duration = Duration::from_millis(500);
/// 降速冷却：**一次拥塞事件只降一次**。
///
/// 上游限流时，同一批并发请求会几乎同时收到 429。若每个 429 都独立降速，
/// 上限会被除以 4^k（k = 429 个数）——配置 100 并发的渠道在十几次 429 后
/// 就被打到下限 1，一次秒级抖动换来分钟级的吞吐塌方（恢复是 +1 加性增）。
/// 用冷却把同一批 429 收敛成一次降速。
const ADAPTIVE_DECREASE_COOLDOWN: Duration = Duration::from_millis(1000);

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

// ──────── AIMD 自适应状态 ────────

/// 单个渠道的自适应并发状态。恒满足 `limit ∈ [ADAPTIVE_MIN_LIMIT, max_concurrency]`。
struct AdaptiveState {
    /// 当前放行上限
    limit: usize,
    /// 上次调整时刻，用于限制提速频率
    last_change: Instant,
    /// 上次降速时刻，用于把同一批并发 429 收敛成一次拥塞事件。
    /// `None` = 从未降速，首次 429 立即可降。
    /// 用 `Option` 而不是 `now - cooldown`：后者在 `Instant` 下界可能溢出。
    last_decrease: Option<Instant>,
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
    /// AIMD 自适应上限，恒 <= max_concurrency
    adaptive: Mutex<AdaptiveState>,
    /// 提速冷却时长。生产用常量；测试可调零以便快速验证收敛，不必真等时钟。
    increase_cooldown: Duration,
    /// 降速冷却时长，含义同上
    decrease_cooldown: Duration,
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
            adaptive: Mutex::new(AdaptiveState {
                // 初始放行量 = 天花板，随后由上游反馈收敛
                limit: max_concurrency.max(ADAPTIVE_MIN_LIMIT),
                last_change: Instant::now(),
                last_decrease: None,
            }),
            increase_cooldown: ADAPTIVE_INCREASE_COOLDOWN,
            decrease_cooldown: ADAPTIVE_DECREASE_COOLDOWN,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, val: bool) {
        self.enabled.store(val, Ordering::Relaxed);
    }

    /// 用户配置的并发天花板
    pub fn max_concurrency(&self) -> usize {
        self.max_concurrency.load(Ordering::Relaxed)
    }

    pub fn set_max_concurrency(&self, val: usize) {
        // swap 原子地取回旧值：需要它区分「改容量」与「从停用状态恢复」
        let prev = self.max_concurrency.swap(val, Ordering::Relaxed);
        // 天花板变了，自适应值必须夹回合法区间，否则缩容后仍按旧值放行
        let mut a = self.adaptive.lock().unwrap_or_else(|e| e.into_inner());
        if val == 0 {
            // 渠道停用：`effective_limit()` 恒为 0，limit 取值无意义，保留不动，
            // 这样短暂的停用不会丢掉已经收敛出来的结果。
        } else if prev == 0 {
            // 从停用恢复：对上游没有信任依据，回到天花板重新收敛。
            // 若只做 clamp，会从「停用前恰好等于下限 1」的残值起步，
            // 与新建渠道（初始 = 天花板）行为不一致。
            a.limit = val;
        } else {
            a.limit = a.limit.clamp(ADAPTIVE_MIN_LIMIT, val);
        }
    }

    /// 当前生效的并发上限（AIMD 自适应，恒 <= `max_concurrency`）。
    ///
    /// `max_concurrency == 0` 时返回 0，保持「0 = 该渠道不可用」的既有语义。
    pub fn effective_limit(&self) -> usize {
        let max = self.max_concurrency();
        if max == 0 {
            return 0;
        }
        self.adaptive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .limit
            .clamp(ADAPTIVE_MIN_LIMIT, max)
    }

    /// 上游返回 2xx：加性提速，逐步探回天花板。
    ///
    /// 受冷却时间约束——降速后立刻猛涨回去会导致震荡。
    pub fn record_success(&self) {
        let max = self.max_concurrency();
        if max == 0 {
            return;
        }
        let mut a = self.adaptive.lock().unwrap_or_else(|e| e.into_inner());
        if a.limit >= max || a.last_change.elapsed() < self.increase_cooldown {
            return;
        }
        a.limit = (a.limit + ADAPTIVE_INCREASE_STEP).min(max);
        a.last_change = Instant::now();
    }

    /// 上游返回 429：乘性降速。
    ///
    /// 首次 429 立即生效（限流必须马上响应），但**同一冷却窗口内的后续 429 被合并**，
    /// 视为同一次拥塞事件。否则一批并发请求同时收到 429 时会被重复降速 k 次，
    /// 上限除以 4^k，一次瞬时限流就把吞吐打穿到下限。
    pub fn record_rate_limit(&self) {
        let max = self.max_concurrency();
        if max == 0 {
            return;
        }
        let mut a = self.adaptive.lock().unwrap_or_else(|e| e.into_inner());

        // 一次拥塞事件只降一次。注意这里在**加锁之后**判断，
        // 保证并发进来的 429 中只有一个能穿过冷却检查。
        if a
            .last_decrease
            .is_some_and(|t| t.elapsed() < self.decrease_cooldown)
        {
            return;
        }

        let step = (a.limit / ADAPTIVE_DECREASE_DIVISOR).max(ADAPTIVE_MIN_LIMIT);
        let before = a.limit;
        a.limit = a
            .limit
            .saturating_sub(step)
            .max(ADAPTIVE_MIN_LIMIT)
            .min(max);
        let now = Instant::now();
        a.last_change = now;
        a.last_decrease = Some(now);
        // 降速是罕见且重要的运维信号：打印出来，便于定位上游限流。
        // 提速不打印——它频繁且无害，会淹没日志。
        if a.limit != before {
            println!(
                "[scheduler] 渠道 {} 收到 429，并发上限 {} → {}（配置上限 {}）",
                self.profile_id, before, a.limit, max
            );
        }
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
        // 用自适应有效值而非配置上限：上游限流时主动收敛放行量
        if channel.current_concurrency.load(Ordering::Relaxed) >= channel.effective_limit() {
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

    /// 已启用渠道的自适应并发上限之和，作为全局并发闸门。
    /// 不能沿用单个渠道的上限，否则大并发渠道会被小渠道的上限卡住。
    /// 取「有效值」而非配置值：上游限流收敛时，全局闸门应同步收紧。
    pub fn total_concurrency(&self) -> usize {
        self.channels
            .read()
            .unwrap()
            .iter()
            .filter(|c| c.is_enabled())
            .map(|c| c.effective_limit())
            .sum()
    }

    // ──────── 并发槽位管理 ────────

    /// 获取并发槽位。成功返回 true，已满返回 false。
    ///
    /// 必须用 CAS 循环而不是 `load` + `store`：后者不是原子读改写，
    /// 并发下两个请求会读到同一个旧值再各自写回，计数比实际占用少，
    /// 渠道并发上限会被突破。参见 `concurrent_acquire_never_exceeds_limit`。
    pub fn acquire_slot(&self, profile_id: &str) -> bool {
        let channels = self.channels.read().unwrap();
        if let Some(ch) = channels.iter().find(|c| c.profile_id == profile_id) {
            let max = ch.effective_limit();
            // 注意用惰性求值的 `then`：`then_some(cur + 1)` 会**无条件**先算出
            // 参数再丢弃，在 0 边界上（release 路径）会触发 usize 下溢 panic。
            return ch
                .current_concurrency
                .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |cur| {
                    (cur < max).then(|| cur + 1)
                })
                .is_ok();
        }
        false
    }

    /// 释放并发槽位
    ///
    /// 同 `acquire_slot`：必须 CAS，否则并发释放会互相覆盖导致只减一次，
    /// 计数虚高不归零，渠道被判为「永远已满」。参见
    /// `concurrent_release_drains_counter_completely`。
    pub fn release_slot(&self, profile_id: &str) {
        let channels = self.channels.read().unwrap();
        if let Some(ch) = channels.iter().find(|c| c.profile_id == profile_id) {
            // 计数为 0 时返回 Err，天然防止下溢（同样必须用惰性求值的 `then`）
            let _ = ch
                .current_concurrency
                .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |cur| {
                    (cur > 0).then(|| cur - 1)
                });
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

    // ──────── 上游反馈（AIMD） ────────

    /// 上游返回 2xx：该渠道加性提速
    pub fn record_success(&self, profile_id: &str) {
        let channels = self.channels.read().unwrap();
        if let Some(ch) = channels.iter().find(|c| c.profile_id == profile_id) {
            ch.record_success();
        }
    }

    /// 上游返回 429：该渠道乘性降速
    pub fn record_rate_limit(&self, profile_id: &str) {
        let channels = self.channels.read().unwrap();
        if let Some(ch) = channels.iter().find(|c| c.profile_id == profile_id) {
            ch.record_rate_limit();
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
                name: String::new(), // 由调用方（get_scheduler_status）从 profile 配置填充
                enabled: ch.is_enabled(),
                current_concurrency: ch.current_concurrency.load(Ordering::Relaxed),
                max_concurrency: ch.max_concurrency(),
                effective_limit: ch.effective_limit(),
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
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub enabled: bool,
    pub current_concurrency: usize,
    /// 用户配置的并发天花板
    pub max_concurrency: usize,
    /// AIMD 自适应后的实际放行上限（<= max_concurrency）
    pub effective_limit: usize,
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

    // ──────── 并发正确性 ────────

    /// 统计渠道在飞并发数（快照读取，与并发上限无关）
    fn in_flight(s: &Scheduler, id: &str) -> usize {
        s.snapshot()
            .iter()
            .find(|c| c.profile_id == id)
            .map(|c| c.current_concurrency)
            .unwrap_or(0)
    }

    /// 读取渠道当前的自适应有效上限
    fn limit_of(s: &Scheduler, id: &str) -> usize {
        s.snapshot()
            .iter()
            .find(|c| c.profile_id == id)
            .map(|c| c.effective_limit)
            .unwrap_or(0)
    }

    /// 模拟「降速冷却已过期」：直接清掉上次降速时刻。
    /// 用于表达**持续**限流（一波接一波，每波间隔超过冷却），
    /// 避免测试靠真实 sleep 累积秒级耗时。
    fn elapse_decrease_cooldown(s: &Scheduler, id: &str) {
        let channels = s.channels.read().unwrap();
        let ch = channels.iter().find(|c| c.profile_id == id).unwrap();
        ch.adaptive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_decrease = None;
    }

    /// 回归保护：`acquire_slot` 曾用 `load` + `store` 做读改写，这不是原子操作。
    /// 两个线程会读到同一个旧值再各自写回，导致计数比实际占用少：
    /// 上限 1 的渠道可能放进来 2 个以上请求，渠道限制形同虚设。
    /// 正确性依赖 CAS 循环，这里用高竞争压力验证。
    #[test]
    fn concurrent_acquire_never_exceeds_limit() {
        const THREADS: usize = 32;
        const ROUNDS: usize = 200;

        let s = Scheduler::new();
        s.upsert_channel("acq-race", true, 1, 0, 0, 100);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
        let mut over_limit = 0usize;

        for _ in 0..ROUNDS {
            let s = &s;
            let barrier = barrier.clone();
            let winners = AtomicUsize::new(0);

            std::thread::scope(|scope| {
                for _ in 0..THREADS {
                    let barrier = barrier.clone();
                    let winners = &winners;
                    scope.spawn(move || {
                        barrier.wait(); // 同时起跑，最大化竞争
                        if s.acquire_slot("acq-race") {
                            winners.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                }
            });

            let won = winners.load(Ordering::Relaxed);
            if won > 1 {
                over_limit += 1;
            }
            for _ in 0..won {
                s.release_slot("acq-race");
            }
        }

        assert_eq!(
            over_limit, 0,
            "并发下渠道并发上限被突破 {over_limit}/{ROUNDS} 轮，渠道限制失效"
        );
    }

    /// 回归保护：`release_slot` 的同类竞态。上限 32 的渠道占满后并发释放，
    /// 两个线程会读到同一个旧值并互相覆盖，只减掉一次，计数虚高不归零。
    /// 渠道随即被判为「永远已满」，从调度候选中消失。
    #[test]
    fn concurrent_release_drains_counter_completely() {
        const THREADS: usize = 32;
        const ROUNDS: usize = 200;

        let s = Scheduler::new();
        s.upsert_channel("rel-race", true, THREADS, 0, 0, 100);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
        let mut leftover = 0usize;

        for _ in 0..ROUNDS {
            for _ in 0..THREADS {
                assert!(s.acquire_slot("rel-race"));
            }

            std::thread::scope(|scope| {
                for _ in 0..THREADS {
                    let barrier = barrier.clone();
                    let s = &s;
                    scope.spawn(move || {
                        barrier.wait();
                        s.release_slot("rel-race");
                    });
                }
            });

            if in_flight(&s, "rel-race") > 0 {
                leftover += 1;
            }
            // 强制清空，保证下一轮从干净状态起跑
            for _ in 0..THREADS {
                s.release_slot("rel-race");
            }
        }

        assert_eq!(
            leftover, 0,
            "并发释放后有 {leftover}/{ROUNDS} 轮计数未归零，渠道会被判为永远已满而不可选"
        );
    }

    /// 回归保护：计数永不越过上限，也不因释放而下溢
    #[test]
    fn slot_counter_stays_within_bounds() {
        let s = Scheduler::new();
        s.upsert_channel("bounds", true, 2, 0, 0, 100);

        assert!(s.acquire_slot("bounds"));
        assert!(s.acquire_slot("bounds"));
        assert!(!s.acquire_slot("bounds"), "超过上限仍获取成功");
        assert_eq!(in_flight(&s, "bounds"), 2);

        // 多释放几次不应把计数压成负数
        for _ in 0..5 {
            s.release_slot("bounds");
        }
        assert_eq!(in_flight(&s, "bounds"), 0, "过度释放把计数压下溢");

        // 下溢后仍应能正常获取
        assert!(s.acquire_slot("bounds"));
        assert_eq!(in_flight(&s, "bounds"), 1);
    }

    // ──────── AIMD 自适应并发 ────────

    /// 429 乘性降速立即生效；成功后加性提速，但必须等冷却结束
    #[test]
    fn rate_limit_slows_down_and_success_recovers_after_cooldown() {
        let s = Scheduler::new();
        s.upsert_channel("aimd", true, 20, 0, 0, 100);
        assert_eq!(limit_of(&s, "aimd"), 20, "初始应放行到配置天花板");

        // 429 → 降速 1/4：20 → 15，立即生效，不等冷却
        s.record_rate_limit("aimd");
        assert_eq!(limit_of(&s, "aimd"), 15);
        // 下一次降速必须等冷却过去（模拟另一波独立的拥塞事件）
        elapse_decrease_cooldown(&s, "aimd");
        s.record_rate_limit("aimd");
        assert_eq!(limit_of(&s, "aimd"), 12, "15 的 1/4 是 3");

        // 冷却期内成功不得提速，否则 429 之后会立刻猛涨回去形成震荡
        s.record_success("aimd");
        assert_eq!(limit_of(&s, "aimd"), 12, "冷却期内不应提速");

        // 冷却结束后加性提速 +1
        std::thread::sleep(ADAPTIVE_INCREASE_COOLDOWN + Duration::from_millis(100));
        s.record_success("aimd");
        assert_eq!(limit_of(&s, "aimd"), 13, "冷却结束后应加性提速");
    }

    /// 回归保护：上游限流时同一批并发请求会**同时**收到 429。
    /// 若每个 429 都独立降速，上限会被除以 4^k —— 配置 100 并发的渠道
    /// 十几次 429 就被打到下限 1，一次秒级抖动换来分钟级吞吐塌方。
    /// 一批 429 必须只算作一次拥塞事件。
    #[test]
    fn burst_of_429s_counts_as_one_congestion_event() {
        let s = Scheduler::new();
        s.upsert_channel("burst", true, 100, 0, 0, 100);
        assert_eq!(limit_of(&s, "burst"), 100);

        // 100 个并发请求同时被上游 429
        for _ in 0..100 {
            s.record_rate_limit("burst");
        }

        assert_eq!(
            limit_of(&s, "burst"),
            75,
            "一批并发 429 被当成多次拥塞事件重复降速，一次抖动就会打穿上限"
        );
    }

    /// 持续限流（一波接一波，间隔超过冷却）应能逐步收敛到下限 1，
    /// 而不是因为冷却机制永不降速。
    #[test]
    fn sustained_rate_limiting_still_converges_to_floor() {
        let s = Scheduler::new();
        s.upsert_channel("floor", true, 100, 0, 0, 100);

        // 每轮代表一次独立的拥塞事件（冷却已过）
        for _ in 0..500 {
            elapse_decrease_cooldown(&s, "floor");
            s.record_rate_limit("floor");
        }
        assert_eq!(limit_of(&s, "floor"), 1, "持续限流应逐步收敛到下限 1");

        // 下限之上仍可获取一个槽位，上游恢复后能自愈
        assert!(s.acquire_slot("floor"));
        assert!(!s.acquire_slot("floor"));
    }

    /// 回归保护：`0 → N` 的恢复路径必须与新建渠道一致。
    /// 修复前 `val == 0` 分支把 limit 写成 1，导致「停用再启用」的渠道
    /// 从下限 1 起步，需要 19 次成功才爬回天花板。
    #[test]
    fn reenabling_channel_restores_full_ceiling() {
        let s = Scheduler::new();
        s.upsert_channel("re", true, 20, 0, 0, 100);
        assert_eq!(limit_of(&s, "re"), 20);

        // 停用：effective_limit 为 0（不可用），但不应破坏内部值
        s.upsert_channel("re", true, 0, 0, 0, 100);
        assert_eq!(limit_of(&s, "re"), 0, "0 上限应表示不可用");

        // 重新启用：应与新建渠道一样从天花板起步
        s.upsert_channel("re", true, 20, 0, 0, 100);
        assert_eq!(
            limit_of(&s, "re"),
            20,
            "从停用恢复的渠道未回到天花板，与新建渠道行为不一致"
        );
    }

    /// 自适应用于放行量控制：降速后新的获取必须真的被挡住
    #[test]
    fn rate_limit_immediately_blocks_new_acquisitions() {
        let s = Scheduler::new();
        s.upsert_channel("shed", true, 4, 0, 0, 100);
        s.record_rate_limit("shed"); // 4 → 3
        assert_eq!(limit_of(&s, "shed"), 3);

        assert!(s.acquire_slot("shed"));
        assert!(s.acquire_slot("shed"));
        assert!(s.acquire_slot("shed"));
        assert!(!s.acquire_slot("shed"), "超过自适应上限仍获取到槽位，降速没生效");
    }

    /// 用户改配置时，自适应值必须跟着夹回区间
    #[test]
    fn changing_max_concurrency_clamps_adaptive_limit() {
        let s = Scheduler::new();
        s.upsert_channel("clamp", true, 20, 0, 0, 100);
        assert_eq!(limit_of(&s, "clamp"), 20);

        // 缩容：自适应值不能停在旧天花板之上
        s.upsert_channel("clamp", true, 5, 0, 0, 100);
        assert_eq!(limit_of(&s, "clamp"), 5, "缩容后仍按旧上限放行");

        // 扩容：不会凭空把当前放行量抬高（要重新靠成功反馈爬升）
        s.upsert_channel("clamp", true, 30, 0, 0, 100);
        assert_eq!(limit_of(&s, "clamp"), 5, "扩容不应立即放大放行量");

        // 降到 0 保持「不可用」语义
        s.upsert_channel("clamp", true, 0, 0, 0, 100);
        assert_eq!(limit_of(&s, "clamp"), 0);
    }

    /// 全局闸门应随自适应上限同步收紧，否则单渠道降速会被其它渠道的配额掩盖
    #[test]
    fn global_gate_follows_adaptive_limit() {
        let s = Scheduler::new();
        s.upsert_channel("g1", true, 10, 0, 0, 100);
        s.upsert_channel("g2", true, 10, 0, 0, 100);
        assert_eq!(s.total_concurrency(), 20);

        s.record_rate_limit("g1"); // 10 → 8
        assert_eq!(s.total_concurrency(), 18, "全局闸门未跟随降速收紧");

        s.record_rate_limit("g2"); // 10 → 8
        assert_eq!(s.total_concurrency(), 16);
    }

    /// 并发上限为 0 的渠道保持「不可用」，反馈不得把它激活
    #[test]
    fn zero_max_concurrency_channel_stays_unusable() {
        let s = Scheduler::new();
        s.upsert_channel("zero", true, 0, 0, 0, 100);

        assert_eq!(limit_of(&s, "zero"), 0, "0 应保持「不可用」语义，而不是被抬成 1");
        assert!(!s.acquire_slot("zero"));
        assert_eq!(s.select_channel(), None);

        s.record_success("zero");
        s.record_rate_limit("zero");
        assert_eq!(limit_of(&s, "zero"), 0, "上游反馈不应激活 0 上限的渠道");
    }
}
