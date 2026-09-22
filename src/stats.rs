//! RPM/TPM 统计：用 SQLite 持久化请求记录，支持任意时间窗口的快速聚合。
//! 数据存储在 exe 同目录的 token-monitor-data.db（WAL 模式），支持 7 天历史。

use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// 输出的 token 类型，分为三类
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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

/// 去重只作用于「足够旧」的行，原因见 `dedupe_requests`。
const DEDUPE_MIN_AGE_MS: i64 = 60 * 60 * 1000;

/// requests 表结构。`init_db` 与测试共用同一份定义，
/// 避免测试建出的表与生产不一致导致断言失真。
const REQUESTS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms INTEGER NOT NULL,
    token_at_ms INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cached_tokens INTEGER NOT NULL DEFAULT 0,
    in_flight INTEGER NOT NULL DEFAULT 1
);";

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
            conn.execute_batch(REQUESTS_SCHEMA).expect("创建 requests 表失败");
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

/// 测试专用：把全局 DB 换成**内存库**。
///
/// 不能让测试直接调 `init_db()` —— 它会打开真实的 `token-monitor-data.db`，
/// 把伪造的请求行写进用户的实际统计库里。
///
/// 建表语句与 `init_db` 的 schema 保持一致（只列本模块测试用到的列）。
#[cfg(test)]
pub(crate) fn use_memory_db_for_test() {
    let conn = Connection::open_in_memory().expect("内存库应可创建");
    conn.execute_batch(REQUESTS_SCHEMA).expect("建表失败");
    *DB.lock().unwrap_or_else(|e| e.into_inner()) = Some(conn);
}

/// 测试专用：统计仍停留在 `in_flight = 1` 的行数。
///
/// `in_flight` 的语义是「正在执行中」，正常路径下每次请求结束都应归零。
/// 永久停在 1 的行不会进入 token 聚合（`WHERE in_flight = 0`），
/// 只能等 7 天保留策略清理，任何按该字段过滤的查询都会看到幽灵请求。
#[cfg(test)]
pub(crate) fn in_flight_row_count_for_test() -> i64 {
    let guard = DB.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(conn) => conn
            .query_row("SELECT COUNT(*) FROM requests WHERE in_flight = 1", [], |r| r.get(0))
            .unwrap_or(0),
        None => 0,
    }
}

/// 启动时一次性去重：同一 at_ms（请求登记毫秒戳）保留第一行，删除其余重复行。
/// 旧版本存在同一请求被多次登记的问题（token 中间态/微秒差异导致签名去不掉），
/// 官方统计口径按请求时间计，因此以 at_ms 作为去重键。只清理历史脏数据，
/// 不影响运行时每次请求的去重开销。
/// 计算可去重的时间区间 `[floor, ceil)`（毫秒时间戳）。
/// 返回 `None` 表示区间为空。
///
/// `floor` = 7 天保留期，`ceil` = `DEDUPE_MIN_AGE_MS`。
fn dedupe_range(now_ms: i64) -> Option<(i64, i64)> {
    let floor = now_ms - SEVEN_DAYS_MS;
    let ceil = now_ms - DEDUPE_MIN_AGE_MS;
    (ceil > floor).then_some((floor, ceil))
}

/// 启动时一次性去重：同一 `at_ms`（请求登记毫秒戳）保留第一行，删除其余重复行。
/// 旧版本存在同一请求被多次登记的问题（token 中间态/微秒差异导致签名去不掉），
/// 官方统计口径按请求时间计，因此以 at_ms 作为去重键。只清理历史脏数据。
///
/// **必须限定时间上界**：`at_ms` 只是毫秒时间戳，同一毫秒内到达的两个**真实**
/// 请求会落在同一个键上。对刚写入的行做去重会删掉其中一个，连同它的 token
/// 一起永久丢失（该行还会被 `in_flight = 0` 的聚合排除，用户无从察觉）。
/// 历史脏数据必然是旧的，用时间下限把当前流量排除在外。
fn dedupe_requests(conn: &Connection) {
    let Some((floor, ceil)) = dedupe_range(chrono::Local::now().timestamp_millis()) else {
        return;
    };
    let deleted = conn.execute(
        "DELETE FROM requests WHERE id NOT IN (
            SELECT MIN(id) FROM requests WHERE at_ms >= ?1 AND at_ms < ?2 GROUP BY at_ms
        ) AND at_ms >= ?1 AND at_ms < ?2",
        params![floor, ceil],
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
        // 归档失败（文件被占用 / 权限）会让下次启动重复迁移一遍：
        // 数据不会重复（同一 at_ms 会被启动去重清掉），但白做功且日志误导。
        // 以前静默忽略，这里如实告知。
        let bak = path.with_extension("json.bak");
        if let Err(e) = std::fs::rename(&path, &bak) {
            eprintln!(
                "[stats] 旧版 JSON 归档失败：{e}（数据不会重复，但下次启动会再次迁移）"
            );
        }
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
            if let Err(e) = conn.execute(
                "UPDATE requests SET token_at_ms = ?1, input_tokens = ?2, output_tokens = ?3, cached_tokens = ?4, in_flight = 0
                 WHERE id = ?5",
                params![now, tokens.input, tokens.output, tokens.cached, rowid],
            ) {
                // 吞掉这个错误会让该行永久停在 in_flight = 1，被 token 聚合
                // （WHERE in_flight = 0）整体排除，这次用量从此在统计里消失。
                // 并发计数是对的（与槽位配套释放），但统计必须让用户能察觉。
                eprintln!(
                    "[stats] 请求 {rowid} 的用量写库失败：{e}\
                     （该行不会进入 token 统计，并发计数不受影响）"
                );
            }
        }
    }
}

/// 仅更新 DB 中的 token 数，不操作 ACTIVE 计数器。
/// 用于流式任务内部的中间态更新：ACTIVE 的释放由 SlotGuard 统一管理。
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
/// 正常路径调用 `disarm()` 把释放责任交给 `update_last_tokens`。
pub struct SlotGuard {
    idx: i64,
    armed: bool,
}

impl SlotGuard {
    pub fn new(idx: i64) -> Self {
        Self { idx, armed: true }
    }

    /// 读取 rowid 而不改变「上膛」状态。
    /// 流式任务需要先用 rowid 做中间态统计，最后才 disarm 收尾。
    pub fn idx(&self) -> i64 {
        self.idx
    }

    /// 移交所有权给调用方：取消 Drop 自动释放，返回 rowid。
    /// 释放（递减 ACTIVE + 标记 `in_flight = 0`）由 `update_last_tokens` 统一完成。
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

#[cfg(test)]
mod dedupe_tests {
    use super::*;

    /// 独立的**内存库**，不复用全局 DB——避免把伪造数据写进用户真实的统计文件，
    /// 也让这些用例无需与其它模块的全局单例串行化。
    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(REQUESTS_SCHEMA).unwrap();
        conn
    }

    fn insert_at(conn: &Connection, at_ms: i64) {
        conn.execute(
            "INSERT INTO requests (at_ms, token_at_ms, in_flight) VALUES (?1, ?1, 0)",
            params![at_ms],
        )
        .unwrap();
    }

    fn rows_at(conn: &Connection, at_ms: i64) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM requests WHERE at_ms = ?1",
            params![at_ms],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// 去重区间必须排除「太新」的行：`at_ms` 只到毫秒，
    /// 同一毫秒内到达的两个真实请求会撞在同一个键上。
    #[test]
    fn dedupe_range_excludes_recent_rows() {
        let now = 1_700_000_000_000i64;
        let (floor, ceil) = dedupe_range(now).expect("应存在可清理区间");
        assert_eq!(floor, now - SEVEN_DAYS_MS);
        assert_eq!(ceil, now - DEDUPE_MIN_AGE_MS);
        assert!(ceil > floor, "上界必须高于下界，否则会删到当前流量");
    }

    /// 回归保护：`dedupe_requests` 曾以毫秒时间戳为唯一去重键、且**没有时间上界**，
    /// 于是同一毫秒内到达的两个真实请求会被判定为重复，删掉其中一个——连同它的
    /// token 数据一起永久丢失（该行还会被 `in_flight = 0` 的聚合排除，用户无从察觉）。
    /// 历史脏数据必然是旧的，因此只清理足够旧的行。
    #[test]
    fn dedupe_spares_recent_rows_but_cleans_old_duplicates() {
        let conn = mem_db();
        let now = chrono::Local::now().timestamp_millis();

        // 历史脏数据：同一毫秒两行（旧版本重复登记的产物）→ 应被去重
        let old_ms = now - DEDUPE_MIN_AGE_MS - 60_000;
        insert_at(&conn, old_ms);
        insert_at(&conn, old_ms);
        assert_eq!(rows_at(&conn, old_ms), 2, "前置条件：应有两条重复行");

        // 当前流量：同一毫秒两行，可能是两个真实请求 → 必须原样保留
        let recent_ms = now - 1_000;
        insert_at(&conn, recent_ms);
        insert_at(&conn, recent_ms);

        dedupe_requests(&conn);

        assert_eq!(rows_at(&conn, old_ms), 1, "历史重复行未被清理");
        assert_eq!(
            rows_at(&conn, recent_ms),
            2,
            "刚写入的真实请求被误删，token 数据会永久丢失"
        );
    }

    /// 超出 7 天保留期的行不归去重管（由保留策略的 DELETE 清理），
    /// 去重不应越界触碰它们。
    #[test]
    fn dedupe_ignores_rows_beyond_retention() {
        let conn = mem_db();
        let now = chrono::Local::now().timestamp_millis();
        let ancient = now - SEVEN_DAYS_MS - 60_000;
        insert_at(&conn, ancient);
        insert_at(&conn, ancient);

        dedupe_requests(&conn);

        assert_eq!(
            rows_at(&conn, ancient),
            2,
            "保留期外的行应交给保留策略，去重不应越界处理"
        );
    }
}