//! RPM/TPM 统计：用 SQLite 持久化请求记录，支持任意时间窗口的快速聚合。
//! 数据存储在 exe 同目录的 token-monitor-data.db（WAL 模式），支持 7 天历史。

use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// 输出的 token 类型，分为三类
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
}

impl TokenCounts {
    #[allow(dead_code)]
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cached
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MinuteBucket {
    pub minute: String,
    pub rpm: u64,
    pub output_tokens: u64,
    pub input_tokens: u64,
    pub cached_tokens: u64,
}

// ──────────────── SQLite 存储 ────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LegacyRecord {
    at_ms: i64,
    tokens: TokenCounts,
}

const SEVEN_DAYS_MS: i64 = 7 * 24 * 3600 * 1000;

static DB: Mutex<Option<Connection>> = Mutex::new(None);
static ACTIVE: AtomicU64 = AtomicU64::new(0);

// ──────────────── 初始化 ────────────────

/// 数据库与旧版数据的存放位置。
/// macOS 在 `.app` 包外、Linux AppImage 在 .AppImage 文件旁，避免升级即丢失；
/// 其余情况仍是 exe 同目录。详见 main.rs 的 app_data_override。
fn db_path() -> std::path::PathBuf {
    if let Some(dir) = crate::app_data_override() {
        return dir.join("token-monitor-data.db");
    }
    std::env::current_exe()
        .ok()
        .and_then(|d| d.parent().map(|p| p.join("token-monitor-data.db")))
        .unwrap_or_else(|| std::path::PathBuf::from("token-monitor-data.db"))
}

fn legacy_data_path() -> std::path::PathBuf {
    if let Some(dir) = crate::app_data_override() {
        return dir.join("token-monitor-data.json");
    }
    std::env::current_exe()
        .ok()
        .and_then(|d| d.parent().map(|p| p.join("token-monitor-data.json")))
        .unwrap_or_else(|| std::path::PathBuf::from("token-monitor-data.json"))
}

/// 初始化数据库连接，迁移旧版数据，启动时调用一次
pub fn init_db() {
    let path = db_path();
    match Connection::open(&path) {
        Ok(conn) => {
            conn.execute_batch("PRAGMA journal_mode=WAL;").ok();
            conn.execute_batch("PRAGMA busy_timeout=3000;").ok();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS requests (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    at_ms INTEGER NOT NULL,
                    token_at_ms INTEGER NOT NULL DEFAULT 0,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    cached_tokens INTEGER NOT NULL DEFAULT 0,
                    in_flight INTEGER NOT NULL DEFAULT 1
                );",
            ).expect("创建 requests 表失败");
            conn.execute_batch(
                "ALTER TABLE requests ADD COLUMN token_at_ms INTEGER NOT NULL DEFAULT 0;",
            ).ok();
            // 只更新最近 7 天内 token_at_ms 还没填的行，避免每次启动全表扫描
            let cutoff_mig = chrono::Local::now().timestamp_millis() - SEVEN_DAYS_MS;
            conn.execute(
                "UPDATE requests SET token_at_ms = at_ms WHERE token_at_ms = 0 AND at_ms >= ?1",
                params![cutoff_mig],
            ).ok();
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_requests_at ON requests(at_ms);",
            ).ok();
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_requests_token_at ON requests(token_at_ms);",
            ).ok();
            migrate_legacy(&conn);
            // 先删旧数据再去重，减少去重扫描行数
            let cutoff = chrono::Local::now().timestamp_millis() - SEVEN_DAYS_MS;
            conn.execute("DELETE FROM requests WHERE at_ms < ?1", params![cutoff]).ok();
            dedupe_requests(&conn);
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
                .unwrap_or(0);
            println!("[stats] SQLite 已就绪：{}，{} 条记录", path.display(), count);
            *DB.lock().unwrap_or_else(|e| e.into_inner()) = Some(conn);
        }
        Err(e) => {
            eprintln!("[stats] SQLite 打开失败：{e}，统计功能不可用");
        }
    }
}

/// 启动时一次性去重：同一 at_ms（请求登记毫秒戳）保留第一行，删除其余重复行。
/// 旧版本存在同一请求被多次登记的问题（token 中间态/微秒差异导致签名去不掉），
/// 官方统计口径按请求时间计，因此以 at_ms 作为去重键。只清理历史脏数据，
/// 不影响运行时每次请求的去重开销。
fn dedupe_requests(conn: &Connection) {
    // 只对最近 7 天的数据去重，避免启动时全表扫描导致卡顿
    let cutoff = chrono::Local::now().timestamp_millis() - SEVEN_DAYS_MS;
    let deleted = conn.execute(
        "DELETE FROM requests WHERE id NOT IN (
            SELECT MIN(id) FROM requests WHERE at_ms >= ?1 GROUP BY at_ms
        ) AND at_ms >= ?1",
        params![cutoff],
    );
    match deleted {
        Ok(n) if n > 0 => println!("[stats] 启动去重完成，删除 {} 条重复记录", n),
        Ok(_) => {}
        Err(e) => eprintln!("[stats] 启动去重失败：{e}"),
    }
}

fn migrate_legacy(conn: &Connection) {
    let path = legacy_data_path();
    if !path.exists() { return; }
    let text = match std::fs::read_to_string(&path) { Ok(t) => t, Err(_) => return };
    let records: Vec<LegacyRecord> = match serde_json::from_str(&text) { Ok(r) => r, Err(_) => return };
    let cutoff = chrono::Local::now().timestamp_millis() - SEVEN_DAYS_MS;
    let mut inserted = 0i64;
    for r in records.iter().filter(|r| r.at_ms >= cutoff) {
        let _ = conn.execute(
            "INSERT INTO requests (at_ms, token_at_ms, input_tokens, output_tokens, cached_tokens, in_flight)
             VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            params![r.at_ms, r.at_ms, r.tokens.input, r.tokens.output, r.tokens.cached],
        );
        inserted += 1;
    }
    if inserted > 0 {
        println!("[stats] 从旧版 JSON 迁移了 {} 条记录", inserted);
        let _ = std::fs::rename(&path, path.with_extension("json.bak"));
    }
}

// ──────────────── 并发追踪 ────────────────

pub fn active() -> u64 {
    ACTIVE.load(Ordering::Relaxed)
}

/// 原子尝试获取一个并发槽位。超过 max 时返回 None（调用方负责等待后重试）。
/// 成功时 active +1 并立即写入请求记录，返回 SQLite rowid。
pub fn try_acquire(max: u64) -> Option<i64> {
    loop {
        let cur = ACTIVE.load(Ordering::Relaxed);
        if cur >= max {
            return None;
        }
        // 先检查 DB 是否就绪，避免 counter +1 后 DB 不可用导致泄漏
        {
            let guard = DB.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_none() {
                // DB 未就绪，不增加计数器，直接返回 0（调用方负责后续减一）
                return Some(0);
            }
        }
        if ACTIVE
            .compare_exchange(cur, cur + 1, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            let now = chrono::Local::now().timestamp_millis();
            let guard = DB.lock().unwrap_or_else(|e| e.into_inner());
            let conn = match guard.as_ref() {
                // 上面已检查过，理论上不会再 None，防御性处理
                None => {
                    ACTIVE.fetch_sub(1, Ordering::Relaxed);
                    return Some(0);
                }
                Some(c) => c,
            };
            return match conn.execute(
                "INSERT INTO requests (at_ms, in_flight) VALUES (?1, 1)",
                params![now],
            ) {
                Ok(_) => Some(conn.last_insert_rowid()),
                Err(e) => {
                    eprintln!("[stats] 插入请求记录失败：{e}");
                    ACTIVE.fetch_sub(1, Ordering::Relaxed);
                    Some(0)
                }
            };
        }
    }
}

/// 非流式请求完成：同时更新 token 数、标记完成并减少并发计数
/// rowid == 0 表示 DB 未就绪时的 fallback，此时 ACTIVE 未递增，跳过 fetch_sub 防止下溢
pub fn update_last_tokens(rowid: i64, tokens: TokenCounts) {
    if rowid > 0 {
        ACTIVE.fetch_sub(1, Ordering::Relaxed);
        if let Some(conn) = DB.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let now = chrono::Local::now().timestamp_millis();
            let _ = conn.execute(
                "UPDATE requests SET token_at_ms = ?1, input_tokens = ?2, output_tokens = ?3, cached_tokens = ?4, in_flight = 0
                 WHERE id = ?5",
                params![now, tokens.input, tokens.output, tokens.cached, rowid],
            );
        }
    }
}

/// 仅更新 DB 中的 token 数，不操作 ACTIVE 计数器。
/// 用于流式任务内部：ACTIVE 的释放由 SlotGuard 统一管理。
pub fn update_tokens_db_only(rowid: i64, tokens: TokenCounts) {
    if rowid > 0 {
        if let Some(conn) = DB.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let now = chrono::Local::now().timestamp_millis();
            let _ = conn.execute(
                "UPDATE requests SET token_at_ms = ?1, input_tokens = ?2, output_tokens = ?3, cached_tokens = ?4
                 WHERE id = ?5",
                params![now, tokens.input, tokens.output, tokens.cached, rowid],
            );
        }
    }
}

/// 并发槽位守卫：确保 Drop 时 ACTIVE -1，防止 panic 导致槽位永久泄漏。
/// 正常路径调用 `disarm()` 后不再自动释放（由 `update_last_tokens` 统一处理）。
pub struct SlotGuard {
    idx: i64,
    armed: bool,
}

impl SlotGuard {
    pub fn new(idx: i64) -> Self {
        Self { idx, armed: true }
    }

    pub fn idx(&self) -> i64 {
        self.idx
    }

    /// 正常完成路径：手动释放槽位并取消 Drop 中的自动释放。
    pub fn release(mut self) {
        if self.armed && self.idx > 0 {
            ACTIVE.fetch_sub(1, Ordering::Relaxed);
        }
        self.armed = false;
    }

    /// 移交所有权给 spawned 任务：取消 Drop 自动释放，由调用方负责。
    pub fn disarm(mut self) -> i64 {
        self.armed = false;
        self.idx
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if self.armed && self.idx > 0 {
            ACTIVE.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

// ──────────────── 统计查询 ────────────────

pub fn buckets(window_minutes: u32) -> Vec<MinuteBucket> {
    let now = chrono::Local::now();
    let current_floor = floor_to_minute(now);
    let mut out = Vec::with_capacity(window_minutes as usize);

    let guard = DB.lock().unwrap_or_else(|e| e.into_inner());
    let conn = match guard.as_ref() {
        Some(c) => c,
        None => return out,
    };

    // 一次 SQL 聚合拿到窗口内所有分钟桶，避免窗口大时逐分钟查询拖慢 UI
    let start_ms = (current_floor - chrono::Duration::minutes(window_minutes as i64))
        .timestamp_millis();
    let end_ms = (current_floor + chrono::Duration::minutes(1)).timestamp_millis();

    // 按整分钟聚合 token 数（仅已完成请求，按 token 写入时间归入本分钟）
    let mut tokens_by_bucket: HashMap<i64, (u64, u64, u64)> = HashMap::new();
    {
        let mut stmt = match conn.prepare(
            "SELECT (token_at_ms / 60000) * 60000 AS minute_ms,
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cached_tokens), 0)
             FROM requests
             WHERE token_at_ms >= ?1 AND token_at_ms < ?2 AND in_flight = 0
             GROUP BY minute_ms"
        ) {
            Ok(s) => s,
            Err(_) => return out,
        };
        let rows: Vec<_> = match stmt.query_map(params![start_ms, end_ms], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
            ))
        }) {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(_) => Vec::new(),
        };
        for (minute_ms, input, output, cached) in rows {
            tokens_by_bucket.insert(minute_ms, (input, output, cached));
        }
    }

    // 按整分钟聚合请求数（含 in-flight）
    let mut rpm_by_bucket: HashMap<i64, u64> = HashMap::new();
    {
        let mut stmt = match conn.prepare(
            "SELECT (at_ms / 60000) * 60000 AS minute_ms, COUNT(*)
             FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2
             GROUP BY minute_ms"
        ) {
            Ok(s) => s,
            Err(_) => return out,
        };
        let rows: Vec<_> = match stmt.query_map(params![start_ms, end_ms], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, u64>(1)?))
        }) {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(_) => Vec::new(),
        };
        for (minute_ms, rpm) in rows {
            rpm_by_bucket.insert(minute_ms, rpm);
        }
    }

    for i in (0..window_minutes).rev() {
        let bucket_start = current_floor - chrono::Duration::minutes(i as i64);
        let bucket_ms = bucket_start.timestamp_millis();
        let minute_label = bucket_start.format("%H:%M").to_string();

        let (input, output, cached) = tokens_by_bucket.get(&bucket_ms).copied().unwrap_or((0, 0, 0));
        let rpm = rpm_by_bucket.get(&bucket_ms).copied().unwrap_or(0);

        out.push(MinuteBucket {
            minute: minute_label,
            rpm,
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
        });
    }
    out
}

/// 自定义时间范围查询：按绝对时间戳聚合，每分钟一个桶
pub fn buckets_range(start_ms: i64, end_ms: i64) -> Vec<MinuteBucket> {
    let mut out = Vec::new();
    let guard = DB.lock().unwrap_or_else(|e| e.into_inner());
    let conn = match guard.as_ref() {
        Some(c) => c,
        None => return out,
    };

    // 按整分钟聚合 token 数（仅已完成请求）
    let mut tokens_by_bucket: HashMap<i64, (u64, u64, u64)> = HashMap::new();
    {
        let mut stmt = match conn.prepare(
            "SELECT (token_at_ms / 60000) * 60000 AS minute_ms,
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cached_tokens), 0)
             FROM requests
             WHERE token_at_ms >= ?1 AND token_at_ms < ?2 AND in_flight = 0
             GROUP BY minute_ms"
        ) {
            Ok(s) => s,
            Err(_) => return out,
        };
        let rows: Vec<_> = match stmt.query_map(params![start_ms, end_ms], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
            ))
        }) {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(_) => Vec::new(),
        };
        for (minute_ms, input, output, cached) in rows {
            tokens_by_bucket.insert(minute_ms, (input, output, cached));
        }
    }

    // 按整分钟聚合请求数（含 in-flight）
    let mut rpm_by_bucket: HashMap<i64, u64> = HashMap::new();
    {
        let mut stmt = match conn.prepare(
            "SELECT (at_ms / 60000) * 60000 AS minute_ms, COUNT(*)
             FROM requests
             WHERE at_ms >= ?1 AND at_ms < ?2
             GROUP BY minute_ms"
        ) {
            Ok(s) => s,
            Err(_) => return out,
        };
        let rows: Vec<_> = match stmt.query_map(params![start_ms, end_ms], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, u64>(1)?))
        }) {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(_) => Vec::new(),
        };
        for (minute_ms, rpm) in rows {
            rpm_by_bucket.insert(minute_ms, rpm);
        }
    }

    // 逐分钟填充，确保连续
    let total_minutes = ((end_ms - start_ms) / 60000) as i64;
    for i in 0..total_minutes {
        let bucket_ms = start_ms + i * 60000;
        let minute_label = {
            let dt = chrono::DateTime::from_timestamp_millis(bucket_ms)
                .unwrap_or_default()
                .with_timezone(&chrono::Local);
            dt.format("%m-%d %H:%M").to_string()
        };
        let (input, output, cached) = tokens_by_bucket.get(&bucket_ms).copied().unwrap_or((0, 0, 0));
        let rpm = rpm_by_bucket.get(&bucket_ms).copied().unwrap_or(0);
        out.push(MinuteBucket {
            minute: minute_label,
            rpm,
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
        });
    }
    out
}


fn floor_to_minute(t: chrono::DateTime<chrono::Local>) -> chrono::DateTime<chrono::Local> {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(t.timestamp() - t.timestamp().rem_euclid(60), 0)
        .single()
        .unwrap_or(t)
}