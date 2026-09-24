//! Copilot CLI 适配器（M2-15）：旁路读取 GitHub Copilot CLI 的全局 session store。
//! 勘察（2026-09-24 发行产物逆向级核实：闭源发行——Node SEA 引导器解包应用包，
//! app.js bundle＋SDK 声明＋事件 schema＋Rust runtime.node 的 store DDL）见
//! docs/01-RESEARCH.md §16，与总纲预想的三处差异：
//!   ① 通道裁定 SQLite 单通道：store 的 assistant_usage_events 是逐调用流水
//!     （五桶＋时长＋首字＋initiator 逐行全有），信息密度完胜事件面——
//!     events.jsonl 的 assistant.usage 事件 ephemeral 不落盘，无需 FileTail；
//!   ② initiator 非空＝辅助调用（sub-agent/mcp-sampling 等，官方注释明示
//!     「absent for user-initiated calls」）——直接映射 is_background
//!     （token 计入、次数不计，对齐 CC cost-state/Hermes task 裁定）；
//!   ③ store 无 API 错误载体（session.error 仅事件面；usage 行 finish_reason
//!     是模型终止原因）→ 无错误行，错误状态纯启发式。
//! 其余要点：
//!   - 状态根：COPILOT_HOME env（目录须存在，异常忽略留痕）＞ 默认根
//!     ~/.copilot（Windows=%USERPROFILE%\.copilot）；XDG 目录是迁移源不扫描；
//!     无 profiles 机制；tag=default/env 消歧；
//!   - 库＝<root>/session-store.db（WAL；/chronicle 数据源；CLI 自身用只读
//!     DuckDB 查询，我们直接只读 SQLite 更轻）；
//!   - scan：sessions 表 90 天窗口，标题=summary（AI 会话摘要），cwd 直取；
//!     时间列 TEXT `datetime('now')`（UTC 秒精度，字典序可比）转毫秒；
//!   - collect：rowid 水位增量（id INTEGER AUTOINCREMENT 跨轮单调，per-tag
//!     记录防跨库不可比），重启后进程内水位归零回退调用方时间水位防全量；
//!     幂等键 `cp:{tag}:{session_id}:{rowid}`（逐调用流水重放全忽略，无快照
//!     升级语义）；
//!   - 已知口径差：usage persistence 上线前的旧会话无 usage 行（官方内置文档
//!     明示），装机对账评估 turns 兜底；provider 库内无列，走模型名推断；
//!   - 无 hooks 注入（Copilot 有 lifecycle hooks 体系但形态/成本未核实，
//!     SQLite 通道已覆盖，列装机后增强档）；
//!   - 快轮信号：每 home 两个 File 信号（session-store.db 与 -wal——WAL 模式
//!     主库 mtime 在 checkpoint 前不动，OpenCode/Hermes 同款教训）。

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use super::engine::{open_sqlite_readonly, HotSignal, ProcessMatch, ScanBudget};
use super::{AgentAdapter, CollectOutput, SessionInfo, provider_from_model};
use crate::store::UsageRow;

/// SQLite 档扫描节流预算（毫秒，M2-3，与 Hermes/OpenClaw 同款）
const SCAN_THROTTLE_MS: u64 = 2_000;
/// 会话扫描/采集范围（90 天，与其他适配器口径一致）
const RANGE_CUTOFF_MS: i64 = 90 * 24 * 3600 * 1000;
/// 单 home 会话扫描上限（跨 home 混排后再全局截断同值）
const SCAN_LIMIT: i64 = 100;
/// 单轮用量行读取上限（逐调用流水行数=调用次数量级，水位保证下轮续读）
const ACTIVE_ROW_LIMIT: i64 = 2_000;
/// 采集水位回看窗（毫秒）：调用方水位与库内时间列存在时钟差，回看成本由
/// 进程内 rowid 水位与幂等 upsert 双保险消化
const WATERMARK_LOOKBACK_MS: i64 = 60_000;

pub struct CopilotAdapter {
    /// 状态根集合（构造时解析；（消歧 tag，根路径）；测试注入临时目录）
    homes: Vec<(String, PathBuf)>,
    /// scan 节流预算（M2-3）：窗口内返回上轮缓存
    scan_budget: Mutex<ScanBudget>,
    /// collect 节流预算（M2-3）：独立于 scan 防相互吃配额
    collect_budget: Mutex<ScanBudget>,
    /// 上轮 scan 结果缓存
    scan_cache: Mutex<Option<Vec<SessionInfo>>>,
    /// 上轮 collect 结果缓存：以旧水位算出的行是超集，自库幂等键保证重复入库零副作用
    collect_cache: Mutex<Option<CollectOutput>>,
    /// 进程内 rowid 高水位（per-tag：rowid 跨库不可比）：只解析新于此的用量行；
    /// 重启后为空即按调用方时间水位续读，两者皆无则全量重放，幂等键兜底
    row_watermarks: Mutex<HashMap<String, i64>>,
}

impl CopilotAdapter {
    /// 生产构造：解析真实环境（COPILOT_HOME/用户主目录）
    pub fn new() -> Self {
        Self::with_homes(current_homes(), SCAN_THROTTLE_MS)
    }

    /// 指定状态根构造（单测注入；throttle_ms=0 即每轮实扫）
    pub(crate) fn with_homes(homes: Vec<(String, PathBuf)>, throttle_ms: u64) -> Self {
        Self {
            homes,
            scan_budget: Mutex::new(ScanBudget::new(throttle_ms)),
            collect_budget: Mutex::new(ScanBudget::new(throttle_ms)),
            scan_cache: Mutex::new(None),
            collect_cache: Mutex::new(None),
            row_watermarks: Mutex::new(HashMap::new()),
        }
    }

    fn lock_scan_budget(&self) -> MutexGuard<'_, ScanBudget> {
        self.scan_budget.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_collect_budget(&self) -> MutexGuard<'_, ScanBudget> {
        self.collect_budget.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_scan_cache(&self) -> MutexGuard<'_, Option<Vec<SessionInfo>>> {
        self.scan_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_collect_cache(&self) -> MutexGuard<'_, Option<CollectOutput>> {
        self.collect_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_row_watermarks(&self) -> MutexGuard<'_, HashMap<String, i64>> {
        self.row_watermarks.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for CopilotAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// 状态根候选解析（参数注入供单测免 set_var 全局竞态）。
/// 默认根 ~/.copilot（tag=default）先收集，COPILOT_HOME env 重定向最优先插头
/// （目录不存在时忽略留痕，不因坏配置失明）；env 与默认根同路径时跳过——
/// tag 保持稳定，_unset env 后会话 id 命名空间不变。
/// XDG_STATE_HOME/.copilot 等旧目录是 CLI 自身的迁移源（启动时并入默认根），
/// 不在此枚举（01-RESEARCH §16.1）。
fn resolve_homes(home_env: Option<OsString>, home_dir: Option<PathBuf>) -> Vec<(String, PathBuf)> {
    let mut homes: Vec<(String, PathBuf)> = vec![];
    if let Some(home) = home_dir {
        let default_root = home.join(".copilot");
        if default_root.is_dir() {
            homes.push(("default".to_string(), default_root));
        }
    }
    if let Some(v) = home_env.filter(|s| !s.is_empty()) {
        let p = PathBuf::from(&v);
        if p.is_dir() {
            if homes.iter().any(|(_, q)| *q == p) {
                log::debug!("[copilot] COPILOT_HOME 与默认根相同，沿用原 tag：{}", p.display());
            } else {
                homes.insert(0, ("env".to_string(), p));
            }
        } else {
            log::debug!("[copilot] COPILOT_HOME 指向的目录不存在，忽略：{}", p.display());
        }
    }
    homes
}

/// 生产档状态根解析（读真实环境变量；Windows=%USERPROFILE%\.copilot）
fn current_homes() -> Vec<(String, PathBuf)> {
    let home = dirs_home();
    resolve_homes(std::env::var_os("COPILOT_HOME"), home)
}

/// 用户主目录（USERPROFILE/ HOME；缺失回退 None——无默认根，env 独立根仍有效）
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// 枚举状态根下的库文件：<root>/session-store.db。返回（tag，库路径）。
fn discover_dbs(homes: &[(String, PathBuf)]) -> Vec<(String, PathBuf)> {
    homes
        .iter()
        .map(|(tag, root)| (tag.clone(), root.join("session-store.db")))
        .filter(|(_, db)| db.is_file())
        .collect()
}

/// 只读打开一个 session-store.db；不存在/打不开返回 None（静默降级，M1 起惯例）
fn open_db(db: &Path) -> Option<rusqlite::Connection> {
    if !db.is_file() {
        return None;
    }
    match open_sqlite_readonly(db) {
        Ok(c) => Some(c),
        Err(e) => {
            log::debug!("[copilot] 库打开失败（本轮跳过）：{}：{e:#}", db.display());
            None
        }
    }
}

/// 库内时间列 TEXT `datetime('now')`（UTC，'YYYY-MM-DD HH:MM:SS' 秒精度）→ 毫秒。
/// 容忍 ISO 'T' 分隔变体；解析失败返回 None（该行按无时间跳过，防止错账）。
fn parse_sql_datetime(s: &str) -> Option<i64> {
    let t = s.trim();
    let naive = chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    Some(naive.and_utc().timestamp_millis())
}

/// 毫秒时间戳 → 库内时间列同构字符串（UTC 'YYYY-MM-DD HH:MM:SS'，作 SQL 下界）
fn ms_to_sql_datetime(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

impl AgentAdapter for CopilotAdapter {
    fn id(&self) -> &'static str {
        "copilot"
    }

    /// 快轮信号（M2-1）：每 home 两个 File 信号——session-store.db 与
    /// session-store.db-wal。WAL 模式下高频写入先落 -wal（主库 mtime 在
    /// checkpoint 前不动，OpenCode/Hermes 同款教训）。
    fn hot_signals(&self) -> Vec<HotSignal> {
        let mut out = vec![];
        for (_, root) in &self.homes {
            for tail in ["session-store.db", "session-store.db-wal"] {
                let path = root.join(tail);
                out.push(HotSignal::File(Arc::new(move || Some(path.clone()))));
            }
        }
        out
    }

    /// 进程匹配（M2-4 声明化）：Windows 发行＝copilot.exe（Node SEA 引导器），
    /// npm bin 名 copilot——按命令行含 copilot 匹配；VS Code Copilot 扩展宿主
    /// 误报面装机核实（§16.3）
    fn process_match(&self) -> Option<ProcessMatch> {
        Some(ProcessMatch {
            name_keywords: &[],
            cmd_keywords: &["copilot"],
            cmd_excludes: &[],
        })
    }

    /// 扫描最近 90 天有活动的会话（sessions 表）。标题=summary（AI 会话摘要）；
    /// 时间 TEXT 转毫秒；model/provider 库内无列（模型列由聚合器以最近用量回填，
    /// 与 CC 同机制）。
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        if !self.lock_scan_budget().ready() {
            return Ok(self.lock_scan_cache().clone().unwrap_or_default());
        }
        let dbs = discover_dbs(&self.homes);
        if dbs.is_empty() {
            *self.lock_scan_cache() = Some(vec![]);
            return Ok(vec![]);
        }
        // 90 天窗口：TEXT 字典序可比，直接用同构字符串作下界
        let cutoff = ms_to_sql_datetime(now_ms() - RANGE_CUTOFF_MS);
        let mut out: Vec<SessionInfo> = vec![];
        for (tag, db) in &dbs {
            let Some(conn) = open_db(db) else { continue };
            let mut stmt = match conn.prepare(
                "SELECT id, summary, cwd,
                        COALESCE(created_at, ''), COALESCE(updated_at, '')
                 FROM sessions
                 WHERE COALESCE(NULLIF(updated_at, ''), NULLIF(created_at, '')) > ?1
                 ORDER BY COALESCE(NULLIF(updated_at, ''), NULLIF(created_at, '')) DESC
                 LIMIT ?2",
            ) {
                Ok(s) => s,
                Err(e) => {
                    // 表结构漂移（旧版库）：该库本轮跳过，debug 留痕
                    log::debug!("[copilot] {tag} 会话查询失败（schema 漂移？）：{e}");
                    continue;
                }
            };
            let rows = stmt.query_map(rusqlite::params![cutoff, SCAN_LIMIT], |r| {
                let id: String = r.get(0)?;
                let summary: Option<String> = r.get(1)?;
                let cwd: Option<String> = r.get(2)?;
                let created: String = r.get(3)?;
                let updated: String = r.get(4)?;
                Ok((id, summary, cwd, created, updated))
            });
            let Ok(rows) = rows else { continue };
            for (id, summary, cwd, created, updated) in rows.filter_map(|x| x.ok()) {
                // 时间列解析失败（异常值）按 0 处理：会话仍可见但不参与时效排序
                let first_seen_ms = parse_sql_datetime(&created).unwrap_or(0);
                let last_seen_ms = parse_sql_datetime(&updated)
                    .or_else(|| parse_sql_datetime(&created))
                    .unwrap_or(0);
                out.push(SessionInfo {
                    // 自库会话 id 带 tag 消歧：默认根/env 双根并存
                    id: format!("copilot:{tag}:{id}"),
                    agent: "copilot".into(),
                    // store 无 provider/model 列；model 由聚合器以最近用量回填
                    provider: None,
                    model: None,
                    project_dir: cwd,
                    // summary=AI 会话摘要（标题载体），空串归一为 None
                    title: summary.filter(|s| !s.is_empty()),
                    first_seen_at: first_seen_ms,
                    last_seen_at: last_seen_ms,
                    last_usage_at: Some(last_seen_ms),
                });
            }
        }
        // 跨库混排：按最近活动降序，全局截断 100（与单库口径一致）
        out.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));
        out.truncate(100);
        *self.lock_scan_cache() = Some(out.clone());
        Ok(out)
    }

    /// 水位增量采集：assistant_usage_events 逐调用流水行（rowid 新于水位）。
    /// rowid INTEGER AUTOINCREMENT 跨轮单调；per-tag 记录防跨库不可比；
    /// 重启后进程内水位缺失，回退调用方时间水位-60s 作 created_at 下界防全量
    /// 重放（水位也为 0 时才真全量，幂等键兜底零副作用）。
    fn collect_usage(&self, watermark_ts: i64) -> anyhow::Result<CollectOutput> {
        if !self.lock_collect_budget().ready() {
            return Ok(self.lock_collect_cache().clone().unwrap_or_default());
        }
        let dbs = discover_dbs(&self.homes);
        if dbs.is_empty() {
            return Ok(CollectOutput::default());
        }
        // created_at 下界：90 天窗与调用方时间水位-60s 取大者（同构字符串比较）
        let since = ms_to_sql_datetime(
            watermark_ts.saturating_sub(WATERMARK_LOOKBACK_MS).max(now_ms() - RANGE_CUTOFF_MS),
        );
        let mut rows: Vec<UsageRow> = vec![];
        for (tag, db) in &dbs {
            let Some(conn) = open_db(db) else { continue };
            let row_watermark = *self.lock_row_watermarks().get(tag).unwrap_or(&0);
            let mut stmt = match conn.prepare(
                "SELECT id, session_id, model, input_tokens, output_tokens,
                        cache_read_tokens, cache_write_tokens, reasoning_tokens,
                        duration_ms, time_to_first_token_ms, initiator, created_at
                 FROM assistant_usage_events
                 WHERE id > ?1 AND created_at > ?2
                 ORDER BY id ASC
                 LIMIT ?3",
            ) {
                Ok(s) => s,
                Err(e) => {
                    log::debug!("[copilot] {tag} 用量查询失败（schema 漂移？）：{e}");
                    continue;
                }
            };
            let rows_iter = stmt.query_map(rusqlite::params![row_watermark, since, ACTIVE_ROW_LIMIT], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                    r.get::<_, Option<String>>(10)?,
                    r.get::<_, Option<String>>(11)?,
                ))
            });
            let Ok(rows_iter) = rows_iter else {
                continue;
            };
            let mut max_row_id = row_watermark;
            for data in rows_iter.filter_map(|x| x.ok()) {
                let (
                    row_id,
                    session_id,
                    model,
                    input,
                    output,
                    cache_read,
                    cache_write,
                    reasoning,
                    duration_ms,
                    ttft_ms,
                    initiator,
                    created_at,
                ) = data;
                if row_id > max_row_id {
                    max_row_id = row_id;
                }
                // created_at 缺失/异常的行按无时间跳过：ts 是聚合与报表的分组键，
                // 错账比漏账危害大（01-RESEARCH §16.2 采集口径）
                let Some(ts) = created_at.as_deref().and_then(parse_sql_datetime) else {
                    continue;
                };
                // 幂等键：逐调用流水以 rowid 全局唯一（per-tag 防跨库同号）
                let source_id = format!("cp:{tag}:{session_id}:{row_id}");
                // provider 库内无列，模型名推断（copilot 模型名 gpt-*/claude-*/gemini-*）
                let provider = provider_from_model(&model);
                // initiator 非空＝辅助调用（sub-agent/mcp-sampling 等）：token 计入、
                // 次数不计（对齐 CC cost-state is_background 裁定）
                let is_background = initiator.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
                rows.push(UsageRow {
                    session_id: format!("copilot:{tag}:{session_id}"),
                    agent: "copilot".into(),
                    model,
                    provider,
                    ts,
                    input_tokens: input,
                    output_tokens: output,
                    reasoning_tokens: reasoning,
                    cache_read_tokens: cache_read,
                    cache_creation_tokens: cache_write,
                    duration_ms,
                    ttft_ms,
                    // store 无 API 错误载体（01-RESEARCH §16.2 差异③）
                    error_type: None,
                    source_id: Some(source_id),
                    is_background,
                });
            }
            if max_row_id > row_watermark {
                self.lock_row_watermarks().insert(tag.clone(), max_row_id);
            }
        }
        let result = CollectOutput::default().with_rows(rows);
        *self.lock_collect_cache() = Some(result.clone());
        Ok(result)
    }
}

/// 当前 Unix 毫秒
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个合成 session-store.db（核心表列集与真实 DDL 对齐，01-RESEARCH
    /// §16.2），返回基准毫秒时间。会话：s1 全列（summary/cwd/repository）＋
    /// s2 summary 空串（标题归 None）。用量：u1 主调用全桶＋时长首字、
    /// u2 辅助调用（sub-agent）、u3 gpt 模型 provider 推断、u4 窗外旧行。
    fn seed_store_db(root: &Path) -> i64 {
        // 对齐到整秒：库内时间列是 datetime('now') 秒精度，基准含亚秒尾数会破坏往返断言
        let base = (now_ms() - 3_600_000) / 1000 * 1000;
        std::fs::create_dir_all(root).unwrap();
        let db = root.join("session-store.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                cwd TEXT,
                repository TEXT,
                host_type TEXT,
                branch TEXT,
                summary TEXT,
                created_at TEXT DEFAULT (datetime('now')),
                updated_at TEXT DEFAULT (datetime('now')));
            CREATE TABLE IF NOT EXISTS assistant_usage_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id),
                turn_index INTEGER,
                agent_id TEXT,
                parent_tool_call_id TEXT,
                model TEXT NOT NULL,
                copilot_usage_model TEXT,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER,
                reasoning_tokens INTEGER,
                total_nano_aiu INTEGER,
                request_multiplier REAL,
                duration_ms INTEGER,
                time_to_first_token_ms INTEGER,
                output_ttft_ms REAL,
                inter_token_latency_ms INTEGER,
                initiator TEXT,
                api_endpoint TEXT,
                reasoning_effort TEXT,
                finish_reason TEXT,
                content_filter_triggered INTEGER,
                token_details_json TEXT,
                created_at TEXT DEFAULT (datetime('now')));",
        )
        .unwrap();
        let s1_created = ms_to_sql_datetime(base);
        let s1_updated = ms_to_sql_datetime(base + 6_000);
        conn.execute(
            "INSERT INTO sessions (id, cwd, repository, summary, created_at, updated_at)
             VALUES ('11111111-aaaa-4bbb-8ccc-111111111111', 'F:\\demo', 'AgentTrackerIsland',
                     '修复采集水位竞态', ?1, ?2)",
            rusqlite::params![s1_created, s1_updated],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, summary, created_at, updated_at) VALUES
             ('22222222-aaaa-4bbb-8ccc-222222222222', '', ?1, ?2)",
            rusqlite::params![s1_created, ms_to_sql_datetime(base + 10_000)],
        )
        .unwrap();
        let insert_usage = |args: [&str; 8]| {
            // （model, input, output, initiator, created_ms, duration, ttft, session）
            conn.execute(
                "INSERT INTO assistant_usage_events
                    (session_id, turn_index, model, input_tokens, output_tokens,
                     cache_read_tokens, cache_write_tokens, reasoning_tokens,
                     total_nano_aiu, duration_ms, time_to_first_token_ms,
                     initiator, created_at)
                 VALUES (?1, 1, ?2, ?3, ?4, 30, 10, 25, 1234, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    args[7], args[0], args[1], args[2], args[5].parse::<i64>().unwrap(),
                    args[6].parse::<i64>().unwrap(), args[3], args[4]
                ],
            )
            .unwrap();
        };
        // u1：主调用全桶（initiator NULL）
        insert_usage([
            "claude-sonnet-4", "120", "60", "", ms_to_sql_datetime(base + 2_000).as_str(),
            "4500", "380", "11111111-aaaa-4bbb-8ccc-111111111111",
        ]);
        // u2：辅助调用（sub-agent）——is_background 应为 true
        insert_usage([
            "claude-sonnet-4", "50", "8", "sub-agent", ms_to_sql_datetime(base + 3_000).as_str(),
            "900", "210", "11111111-aaaa-4bbb-8ccc-111111111111",
        ]);
        // u3：gpt 模型（provider 推断 openai）＋空 summary 会话
        insert_usage([
            "gpt-5.2", "11", "7", "", ms_to_sql_datetime(base + 5_000).as_str(),
            "800", "150", "22222222-aaaa-4bbb-8ccc-222222222222",
        ]);
        // u4：90 天窗外的旧行——不应被采集
        insert_usage([
            "gpt-5.2", "99", "99", "", ms_to_sql_datetime(base - RANGE_CUTOFF_MS - 86_400_000).as_str(),
            "100", "100", "11111111-aaaa-4bbb-8ccc-111111111111",
        ]);
        base
    }

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("at-copilot-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 时间列解析：datetime('now') 秒精度格式、ISO 'T' 变体、坏值 None
    #[test]
    fn test_parse_sql_datetime() {
        // 期望值用 chrono 同源换算（2026-09-24 04:30:05 UTC）
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 9, 24)
            .unwrap()
            .and_hms_opt(4, 30, 5)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        assert_eq!(parse_sql_datetime("2026-09-24 04:30:05"), Some(expected));
        assert_eq!(parse_sql_datetime("2026-09-24T04:30:05"), Some(expected));
        assert_eq!(parse_sql_datetime("not-a-date"), None);
        assert_eq!(parse_sql_datetime(""), None);
        assert_eq!(ms_to_sql_datetime(expected), "2026-09-24 04:30:05", "往返应无损失");
    }

    /// 状态根解析：env 优先、无效 env 忽略、默认根去重
    #[test]
    fn test_resolve_homes() {
        let home = temp_root("roots");
        // 无任何目录 → 空
        assert!(resolve_homes(None, Some(home.clone())).is_empty());
        // 默认根存在 → default
        let default_root = home.join(".copilot");
        std::fs::create_dir_all(&default_root).unwrap();
        let homes = resolve_homes(None, Some(home.clone()));
        assert_eq!(homes, vec![("default".to_string(), default_root.clone())]);
        // env 根存在 → 最优先
        let env_root = home.join("copilot-env");
        std::fs::create_dir_all(&env_root).unwrap();
        let homes = resolve_homes(Some(env_root.clone().into_os_string()), Some(home.clone()));
        assert_eq!(homes[0], ("env".into(), env_root.clone()));
        assert_eq!(homes.len(), 2);
        // env 指向不存在目录 → 忽略；env 与默认根同路径 → 去重
        let homes = resolve_homes(Some(home.join("nope").into_os_string()), Some(home.clone()));
        assert_eq!(homes.len(), 1, "坏 env 不阻断");
        let homes = resolve_homes(Some(default_root.clone().into_os_string()), Some(home.clone()));
        assert_eq!(homes.len(), 1, "env==默认根去重");
        assert!(homes.iter().all(|(tag, _)| tag != "env"));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 库发现：<root>/session-store.db；无库的根跳过
    #[test]
    fn test_discover_dbs() {
        let home = temp_root("disc");
        let default_root = home.join(".copilot");
        std::fs::create_dir_all(&default_root).unwrap();
        assert!(discover_dbs(&resolve_homes(None, Some(home.clone()))).is_empty());
        seed_store_db(&default_root);
        let dbs = discover_dbs(&resolve_homes(None, Some(home.clone())));
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].0, "default");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// scan：标题=summary、cwd 直取、TEXT 时间转毫秒、空 summary 归 None、
    /// 按最近活动降序、90 天窗口排除旧行
    #[test]
    fn test_scan_sessions() {
        let home = temp_root("scan");
        let default_root = home.join(".copilot");
        let base = seed_store_db(&default_root);
        let ad = CopilotAdapter::with_homes(vec![("default".into(), default_root)], 0);
        let sessions = ad.scan_sessions().unwrap();
        assert_eq!(sessions.len(), 2, "窗外会话无 usage 行也不应出现在 scan");
        // s2 活动（base+10s）晚于 s1（base+6s）→ 排前
        assert_eq!(
            sessions[0].id,
            "copilot:default:22222222-aaaa-4bbb-8ccc-222222222222",
            "按最近活动降序"
        );
        assert_eq!(sessions[0].title, None, "空 summary 应归 None");
        assert_eq!(sessions[1].id, "copilot:default:11111111-aaaa-4bbb-8ccc-111111111111");
        assert_eq!(sessions[1].title.as_deref(), Some("修复采集水位竞态"));
        assert_eq!(sessions[1].project_dir.as_deref(), Some("F:\\demo"));
        assert_eq!(sessions[1].model, None, "store 无模型列");
        assert_eq!(sessions[1].first_seen_at, base, "created_at 应转毫秒");
        assert_eq!(sessions[1].last_seen_at, base + 6_000, "updated_at 应转毫秒");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 采集端到端：逐调用行解析、initiator 背景行、provider 推断、窗口过滤、
    /// 自库幂等（逐调用流水重放全忽略，仅后台行按 M1-12 设计无条件覆盖）
    #[test]
    fn test_collect_end_to_end() {
        let home = temp_root("coll");
        let default_root = home.join(".copilot");
        let base = seed_store_db(&default_root);
        let ad = CopilotAdapter::with_homes(vec![("default".into(), default_root.clone())], 0);

        let out = ad.collect_usage(0).unwrap().rows;
        assert_eq!(out.len(), 3, "90 天窗外的 u4 不应入库");
        // u1：主调用全桶（rowid 1）
        let u1 = &out[0];
        assert_eq!(
            (
                u1.input_tokens,
                u1.output_tokens,
                u1.cache_read_tokens,
                u1.cache_creation_tokens,
                u1.reasoning_tokens
            ),
            (Some(120), Some(60), Some(30), Some(10), Some(25))
        );
        assert_eq!(u1.ts, base + 2_000, "created_at 应转毫秒");
        assert_eq!(u1.duration_ms, Some(4500));
        assert_eq!(u1.ttft_ms, Some(380));
        assert_eq!(u1.provider.as_deref(), Some("anthropic"), "claude 前缀应推断 anthropic");
        assert_eq!(
            u1.session_id,
            "copilot:default:11111111-aaaa-4bbb-8ccc-111111111111"
        );
        assert_eq!(u1.source_id.as_deref(), Some("cp:default:11111111-aaaa-4bbb-8ccc-111111111111:1"));
        assert!(!u1.is_background, "initiator NULL 应为主调用");
        assert_eq!(u1.error_type, None, "store 无错误载体");
        // u2：辅助调用（sub-agent）→ is_background
        let u2 = &out[1];
        assert!(u2.is_background, "initiator 非空应标后台行");
        assert_eq!((u2.input_tokens, u2.output_tokens), (Some(50), Some(8)));
        assert_eq!(u2.source_id.as_deref(), Some("cp:default:11111111-aaaa-4bbb-8ccc-111111111111:2"));
        // u3：gpt 模型 provider 推断
        let u3 = &out[2];
        assert_eq!(u3.provider.as_deref(), Some("openai"), "gpt 前缀应推断 openai");

        // 自库幂等：逐调用流水重放全忽略；仅 u2 后台行按 M1-12 设计无条件覆盖
        let store = crate::store::Store::open(&home.join("store.db")).unwrap();
        assert_eq!(store.insert_usage(&out), 3);
        assert_eq!(
            store.insert_usage(&out),
            1,
            "is_background 行无条件覆盖是既有设计，逐调用行重放应全忽略"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// rowid 水位：同实例二次采集不重出旧行（per-tag 记录）；新增行后只出新增
    #[test]
    fn test_watermark_incremental() {
        let home = temp_root("wm");
        let default_root = home.join(".copilot");
        let base = seed_store_db(&default_root);
        let ad = CopilotAdapter::with_homes(vec![("default".into(), default_root.clone())], 0);
        assert_eq!(ad.collect_usage(0).unwrap().rows.len(), 3);
        // 再次采集：进程内 rowid 水位拦截，无新行
        assert!(ad.collect_usage(0).unwrap().rows.is_empty());

        // 新调用落库（模拟 CLI 逐调用追加）：只出新增行
        let db = default_root.join("session-store.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO assistant_usage_events
                (session_id, turn_index, model, input_tokens, output_tokens, created_at)
             VALUES ('11111111-aaaa-4bbb-8ccc-111111111111', 2, 'claude-sonnet-4', 70, 20, ?1)",
            rusqlite::params![ms_to_sql_datetime(base + 30_000)],
        )
        .unwrap();
        let rows = ad.collect_usage(0).unwrap().rows;
        assert_eq!(rows.len(), 1, "只有新增 rowid 重入");
        assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (Some(70), Some(20)));
        assert_eq!(rows[0].ts, base + 30_000);
        // 同 source_id 重入自库：零新增
        let store = crate::store::Store::open(&home.join("store.db")).unwrap();
        assert_eq!(store.insert_usage(&rows), 1);
        drop(store);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 调用方时间水位防全量重放：重启后（进程内水位缺失）created_at 下界
    /// 取「水位-60s」与 90 天窗的大者——水位推过全部存量行后不重放；
    /// 水位回拨则回看窗内旧行重放（幂等键兜底零副作用）
    #[test]
    fn test_caller_watermark_floor() {
        let home = temp_root("floor");
        let default_root = home.join(".copilot");
        let base = seed_store_db(&default_root);
        // 新实例（进程内水位为空）＋调用方水位=base+65s → 下界=base+5s，
        // 存量三行（base+2/3/5s）全部过期不重放
        let ad = CopilotAdapter::with_homes(vec![("default".into(), default_root.clone())], 0);
        let rows = ad.collect_usage(base + 65_001).unwrap().rows;
        assert!(rows.is_empty(), "水位推过存量行后不应重放（实际 {}）", rows.len());

        // 水位回拨到 base-50s：下界=base-110s，u1/u2/u3（base+2/3/5s）重放
        let rows = ad.collect_usage(base - 50_000).unwrap().rows;
        assert_eq!(rows.len(), 3, "回看窗内旧行应重放");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 快轮信号：每 home 两个（session-store.db 与 -wal），采样能取到值
    #[test]
    fn test_hot_signals() {
        let home = temp_root("sig");
        let default_root = home.join(".copilot");
        std::fs::create_dir_all(&default_root).unwrap();
        let ad = CopilotAdapter::with_homes(vec![("default".into(), default_root.clone())], 0);
        let signals = ad.hot_signals();
        assert_eq!(signals.len(), 2, "session-store.db + -wal");
        // 采样值不 panic（文件不存在时 None 也不应出错）
        for s in &signals {
            if let HotSignal::File(p) = s {
                let _ = p();
            }
        }
        std::fs::write(default_root.join("session-store.db"), b"x").unwrap();
        if let HotSignal::File(p) = &signals[0] {
            assert_eq!(p(), Some(default_root.join("session-store.db")));
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 集成测试：连接本机真实 Copilot CLI 库（未装跳过；装机后手动
    /// `cargo test -- --ignored` 补跑对账，清单见 01-RESEARCH §16.3）
    #[test]
    #[ignore]
    fn test_real_copilot_collect() {
        let homes = current_homes();
        let dbs = discover_dbs(&homes);
        assert!(!dbs.is_empty(), "本机应有 Copilot CLI session-store.db");
        let ad = CopilotAdapter::with_homes(homes, 0);
        let sessions = ad.scan_sessions().unwrap();
        assert!(!sessions.is_empty(), "本机应有 Copilot CLI 会话");
        let usage = ad.collect_usage(0).unwrap().rows;
        assert!(!usage.is_empty(), "本机应有历史用量");
        for u in usage.iter().take(5) {
            assert!(u.ts > 1_700_000_000_000, "时间戳应为毫秒：{}", u.ts);
            assert_eq!(u.agent, "copilot");
            assert!(u.session_id.starts_with("copilot:"));
        }
        assert!(
            ad.collect_usage(9_999_999_999_999).unwrap().rows.is_empty(),
            "水位增量应为空"
        );
    }
}
