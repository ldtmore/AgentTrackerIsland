//! Hermes 适配器（M2-14）：旁路读取 Hermes Agent（NousResearch）的 state.db。
//! 勘察（2026-09-24 源码级核实：hermes_state_* 全模块＋agent/turn_usage）见
//! docs/01-RESEARCH.md §15，与总纲预想的四处差异：
//!   ① 库内无逐调用流水表（messages.token_count 仅单值）——唯一四桶数据面是
//!     累计快照，采集走「session_model_usage 行重采＋保留最大快照幂等」；
//!   ② 记账双通路：update_token_counts 唯一咽喉，CLI 面增量累加、gateway 面
//!     absolute 只覆盖 sessions 主行（增量表不受影响，快照单调安全）；
//!   ③ session_model_usage.task 列区分消耗类型（''=主循环，非空=vision/压缩/
//!     标题生成等辅助调用）——直接映射 is_background（token 计入、次数不计，
//!     对齐 CC cost-state 裁定）；
//!   ④ 库内无逐调用错误载体（api_request_error 仅 hooks 面）→ 无错误行，
//!     错误状态纯启发式。
//! 其余要点：
//!   - 状态根：HERMES_HOME env（目录须存在，异常忽略留痕）> 平台默认根
//!     （Windows=%LOCALAPPDATA%\hermes）；命名 profile 在 <默认根>/profiles/<名>/
//!     （根锚定平台默认而非当前 HERMES_HOME），一并枚举；tag 用于会话 id
//!     消歧（default/<名>/env）；库=<home>/state.db（WAL，schema v30）；
//!   - scan：sessions 表 90 天窗口，标题链 title>display_name，cwd/model/
//!     billing_provider 直取；时间列 REAL Unix 秒转毫秒；
//!   - collect：last_seen 水位（秒→毫秒），进程内 last_seen 高水位拦截已读行，
//!     重启全量重放靠 source_id 幂等键兜底；rewind 不清零累计、增量路径只加
//!     不减，「保留最大快照」语义天然适配；
//!   - 已知口径差：ts 取 last_seen（该累计行最后写入时刻），历史会话的 token
//!     记账日压缩到末日——装机对账评估（01-RESEARCH §15.3）；
//!   - 无 hooks 注入（Hermes 有 shell hooks 体系但为 YAML 配置＋consent
//!     allowlist，注入成本高且 SQLite 通道已覆盖，列装机后增强档）；
//!   - 快轮信号：每 home 两个 File 信号（state.db 与 state.db-wal——WAL 模式
//!     主库 mtime 不动，-wal 才是高频写入面，OpenCode 同款教训）。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use super::engine::{open_sqlite_readonly, HotSignal, ProcessMatch, ScanBudget};
use super::{AgentAdapter, CollectOutput, SessionInfo, provider_from_model};
use crate::store::UsageRow;

/// SQLite 档扫描节流预算（毫秒，M2-3，与 OpenClaw/OpenCode 同款）
const SCAN_THROTTLE_MS: u64 = 2_000;
/// 会话扫描/采集范围（90 天，与其他适配器口径一致）
const RANGE_CUTOFF_MS: i64 = 90 * 24 * 3600 * 1000;
/// 单 home 会话扫描上限（跨 home 混排后再全局截断同值）
const SCAN_LIMIT: i64 = 100;
/// 单轮累计行读取上限（行数=会话×模型量级，正常远达不到；水位保证下轮续读）
const ACTIVE_ROW_LIMIT: i64 = 2_000;
/// 采集水位回看窗（毫秒）：last_seen 与采集水位存在时钟差，回看成本由进程内
/// 水位与幂等 upsert 双保险消化
const WATERMARK_LOOKBACK_MS: i64 = 60_000;

pub struct HermesAdapter {
    /// 状态根集合（构造时解析一次；（消歧 tag，home 路径）；测试注入临时目录）
    homes: Vec<(String, PathBuf)>,
    /// scan 节流预算（M2-3）：窗口内返回上轮缓存
    scan_budget: Mutex<ScanBudget>,
    /// collect 节流预算（M2-3）：独立于 scan 防相互吃配额
    collect_budget: Mutex<ScanBudget>,
    /// 上轮 scan 结果缓存
    scan_cache: Mutex<Option<Vec<SessionInfo>>>,
    /// 上轮 collect 结果缓存：以旧水位算出的行是超集，自库幂等键保证重复入库零副作用
    collect_cache: Mutex<Option<CollectOutput>>,
    /// 进程内 last_seen 高水位（毫秒）：只解析新于此的累计行；
    /// 重启后为 0 即全量重放，自库幂等键兜底
    last_seen_watermark: Mutex<i64>,
}

impl HermesAdapter {
    /// 生产构造：解析真实环境（HERMES_HOME/LOCALAPPDATA/用户主目录）
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
            last_seen_watermark: Mutex::new(0),
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

    fn lock_last_seen_watermark(&self) -> MutexGuard<'_, i64> {
        self.last_seen_watermark.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for HermesAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// 状态根候选解析（参数注入供单测免 set_var 全局竞态）。
/// 平台默认根（Windows=%LOCALAPPDATA%\hermes）与其下 profiles/<名> 子目录先收集，
/// HERMES_HOME env 重定向最优先插头（目录不存在时忽略留痕，不因坏配置失明）。
/// tag 用于跨库会话 id 消歧：默认根=default、profile=<目录名>、env 独立根=env
/// （env 与已有根同路径时跳过——tag 保持稳定，_unset env 后会话 id 命名空间不变）。
fn resolve_homes(
    home_env: Option<OsString>,
    local_appdata: Option<PathBuf>,
) -> Vec<(String, PathBuf)> {
    let mut homes: Vec<(String, PathBuf)> = vec![];
    let push = |tag: &str, p: PathBuf, homes: &mut Vec<(String, PathBuf)>| {
        // 去重：同一路径只采一次，先到先得（保 tag 稳定）
        if p.is_dir() && !homes.iter().any(|(_, q)| *q == p) {
            homes.push((tag.to_string(), p));
        }
    };
    let Some(appdata) = local_appdata else {
        // 无默认根时 env 独立根仍然有效
        if let Some(v) = home_env.filter(|s| !s.is_empty()) {
            let p = PathBuf::from(&v);
            if p.is_dir() {
                homes.push(("env".to_string(), p));
            } else {
                log::debug!("[hermes] HERMES_HOME 指向的目录不存在，忽略：{}", p.display());
            }
        }
        return homes;
    };
    let default_root = appdata.join("hermes");
    push("default", default_root.clone(), &mut homes);
    // 命名 profile：默认根/profiles/<名>（源码 hermes_cli/profiles.py:174，根锚定
    // 平台默认而非当前 HERMES_HOME）
    if let Ok(rd) = std::fs::read_dir(default_root.join("profiles")) {
        let mut names: Vec<(String, PathBuf)> = rd
            .filter_map(|x| x.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
            .collect();
        names.sort();
        for (name, p) in names {
            push(&name, p, &mut homes);
        }
    }
    // env 重定向最优先：插到头部（与默认根/profiles 同路径时跳过）
    if let Some(v) = home_env.filter(|s| !s.is_empty()) {
        let p = PathBuf::from(&v);
        if p.is_dir() {
            if homes.iter().any(|(_, q)| *q == p) {
                log::debug!("[hermes] HERMES_HOME 与已有状态根相同，沿用原 tag：{}", p.display());
            } else {
                homes.insert(0, ("env".to_string(), p));
            }
        } else {
            log::debug!("[hermes] HERMES_HOME 指向的目录不存在，忽略：{}", p.display());
        }
    }
    homes
}

/// 生产档状态根解析（读真实环境变量；非 Windows 默认根 ~/.hermes 同样兼容——
/// LOCALAPPDATA 缺失时回退 ~/.local 前不成立，直接无默认根，Windows 为主场）
fn current_homes() -> Vec<(String, PathBuf)> {
    let appdata = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    resolve_homes(std::env::var_os("HERMES_HOME"), appdata)
}

/// 枚举状态根下的库文件：<home>/state.db。返回（tag，库路径）。
fn discover_dbs(homes: &[(String, PathBuf)]) -> Vec<(String, PathBuf)> {
    homes
        .iter()
        .map(|(tag, home)| (tag.clone(), home.join("state.db")))
        .filter(|(_, db)| db.is_file())
        .collect()
}

/// 只读打开一个 state.db；不存在/打不开返回 None（静默降级，M1 起惯例）
fn open_db(db: &Path) -> Option<rusqlite::Connection> {
    if !db.is_file() {
        return None;
    }
    match open_sqlite_readonly(db) {
        Ok(c) => Some(c),
        Err(e) => {
            log::debug!("[hermes] 库打开失败（本轮跳过）：{}：{e:#}", db.display());
            None
        }
    }
}

/// REAL Unix 秒 → 毫秒整数（Hermes 时间列全部是 REAL 秒浮点）
fn sec_to_ms(sec: f64) -> i64 {
    (sec * 1000.0) as i64
}

impl AgentAdapter for HermesAdapter {
    fn id(&self) -> &'static str {
        "hermes"
    }

    /// 快轮信号（M2-1）：每 home 两个 File 信号——state.db 与 state.db-wal。
    /// WAL 模式下高频写入先落 -wal（主库 mtime 在 checkpoint 前不动），
    /// 只盯主库会漏「回合起点」的亚 10 秒感知（OpenCode 同款教训）。
    fn hot_signals(&self) -> Vec<HotSignal> {
        let mut out = vec![];
        for (_, home) in &self.homes {
            for tail in ["state.db", "state.db-wal"] {
                let path = home.join(tail);
                out.push(HotSignal::File(Arc::new(move || Some(path.clone()))));
            }
        }
        out
    }

    /// 进程匹配（M2-4 声明化）：Python 应用（gateway/TUI 常驻），进程名
    /// python 无特征，按命令行含 hermes 匹配——精确形态装机核实（§15.3）
    fn process_match(&self) -> Option<ProcessMatch> {
        Some(ProcessMatch {
            name_keywords: &[],
            cmd_keywords: &["hermes"],
            cmd_excludes: &[],
        })
    }

    /// 扫描最近 90 天有活动的会话（sessions 表）。标题回退链
    /// title > display_name；时间 REAL 秒转毫秒；last_usage 回退 started_at。
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        if !self.lock_scan_budget().ready() {
            return Ok(self.lock_scan_cache().clone().unwrap_or_default());
        }
        let dbs = discover_dbs(&self.homes);
        if dbs.is_empty() {
            *self.lock_scan_cache() = Some(vec![]);
            return Ok(vec![]);
        }
        // 90 天窗口（毫秒水位换算为库内 REAL 秒口径）
        let cutoff_sec = (now_ms() - RANGE_CUTOFF_MS) as f64 / 1000.0;
        let mut out: Vec<SessionInfo> = vec![];
        for (tag, db) in &dbs {
            let Some(conn) = open_db(db) else { continue };
            let mut stmt = match conn.prepare(
                "SELECT id, COALESCE(NULLIF(title, ''), NULLIF(display_name, '')),
                        cwd, model, billing_provider, started_at,
                        COALESCE(last_activity_at, started_at)
                 FROM sessions
                 WHERE COALESCE(last_activity_at, started_at) > ?1
                 ORDER BY COALESCE(last_activity_at, started_at) DESC
                 LIMIT ?2",
            ) {
                Ok(s) => s,
                Err(e) => {
                    // 表结构漂移（旧版库）：该库本轮跳过，debug 留痕
                    log::debug!("[hermes] {tag} 会话查询失败（schema 漂移？）：{e}");
                    continue;
                }
            };
            let rows = stmt.query_map(rusqlite::params![cutoff_sec, SCAN_LIMIT], |r| {
                let id: String = r.get(0)?;
                let started: f64 = r.get(5)?;
                let last_activity: f64 = r.get(6)?;
                Ok(SessionInfo {
                    // 自库会话 id 带 tag 消歧：默认根/profiles/env 多库并存
                    id: format!("hermes:{tag}:{id}"),
                    agent: "hermes".into(),
                    provider: r.get::<_, Option<String>>(4)?,
                    model: r.get::<_, Option<String>>(3)?,
                    project_dir: r.get::<_, Option<String>>(2)?,
                    title: r.get::<_, Option<String>>(1)?,
                    first_seen_at: sec_to_ms(started),
                    last_seen_at: sec_to_ms(last_activity),
                    last_usage_at: Some(sec_to_ms(last_activity)),
                })
            });
            let Ok(rows) = rows else { continue };
            for row in rows.filter_map(|x| x.ok()) {
                out.push(row);
            }
        }
        // 跨库混排：按最近活动降序，全局截断 100（与单库口径一致）
        out.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));
        out.truncate(100);
        *self.lock_scan_cache() = Some(out.clone());
        Ok(out)
    }

    /// 水位增量采集：session_model_usage 累计行（last_seen 新于水位-60s）重采。
    /// 无逐调用流水表（01-RESEARCH §15.2 差异①），每行是「会话×模型×task」的
    /// 累计快照——同 source_id 重入时靠自库「保留最大快照」幂等语义收敛。
    fn collect_usage(&self, watermark_ts: i64) -> anyhow::Result<CollectOutput> {
        if !self.lock_collect_budget().ready() {
            return Ok(self.lock_collect_cache().clone().unwrap_or_default());
        }
        let dbs = discover_dbs(&self.homes);
        if dbs.is_empty() {
            return Ok(CollectOutput::default());
        }
        // SQL 层水位：进程内 last_seen 高水位与调用方水位-60s 取大者
        // （重启后进程内水位归零，回退调用方语义；cutoff 独立过滤）
        let since_ms = self
            .lock_last_seen_watermark()
            .max(watermark_ts.saturating_sub(WATERMARK_LOOKBACK_MS));
        let since_sec = since_ms as f64 / 1000.0;
        let cutoff_sec = (now_ms() - RANGE_CUTOFF_MS) as f64 / 1000.0;
        let mut rows: Vec<UsageRow> = vec![];
        for (tag, db) in &dbs {
            let Some(conn) = open_db(db) else { continue };
            let mut stmt = match conn.prepare(
                "SELECT session_id, model, billing_provider, billing_base_url, billing_mode,
                        task, input_tokens, output_tokens, cache_read_tokens,
                        cache_write_tokens, reasoning_tokens, last_seen
                 FROM session_model_usage
                 WHERE last_seen > ?1 AND last_seen > ?2
                 ORDER BY last_seen ASC
                 LIMIT ?3",
            ) {
                Ok(s) => s,
                Err(e) => {
                    log::debug!("[hermes] {tag} 用量查询失败（schema 漂移？）：{e}");
                    continue;
                }
            };
            let rows_iter = stmt.query_map(
                rusqlite::params![since_sec, cutoff_sec, ACTIVE_ROW_LIMIT],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, i64>(7)?,
                        r.get::<_, i64>(8)?,
                        r.get::<_, i64>(9)?,
                        r.get::<_, i64>(10)?,
                        r.get::<_, Option<f64>>(11)?,
                    ))
                },
            );
            let Ok(rows_iter) = rows_iter else {
                continue;
            };
            let mut max_seen_ms = 0i64;
            for data in rows_iter.filter_map(|x| x.ok()) {
                let (
                    session_id,
                    model,
                    billing_provider,
                    billing_base_url,
                    billing_mode,
                    task,
                    input,
                    output,
                    cache_read,
                    cache_write,
                    reasoning,
                    last_seen,
                ) = data;
                let ts = last_seen.map(sec_to_ms).unwrap_or(0);
                if ts > max_seen_ms {
                    max_seen_ms = ts;
                }
                if ts <= 0 {
                    continue;
                }
                // 幂等键：累计行六元组主键全拼（01-RESEARCH §15.2 采集口径；
                // provider 用源库 billing_provider 原值，回退推断值不参与身份）
                let source_id = format!(
                    "hm:{tag}:{session_id}:{model}:{}:{}:{base}:{mode}",
                    task.as_deref().unwrap_or_default(),
                    billing_provider.as_deref().unwrap_or_default(),
                    base = billing_base_url.as_deref().unwrap_or_default(),
                    mode = billing_mode.as_deref().unwrap_or_default(),
                );
                // provider：billing_provider 权威直用，缺失回退模型名推断
                let provider = billing_provider
                    .filter(|s| !s.is_empty())
                    .or_else(|| provider_from_model(&model));
                // task 非空=辅助调用（vision/压缩/标题生成等）：token 计入、
                // 次数不计（对齐 CC cost-state is_background 裁定）
                let is_background = task.as_deref().map(|t| !t.is_empty()).unwrap_or(false);
                rows.push(UsageRow {
                    session_id: format!("hermes:{tag}:{session_id}"),
                    agent: "hermes".into(),
                    model,
                    provider,
                    ts,
                    input_tokens: Some(input),
                    output_tokens: Some(output),
                    reasoning_tokens: Some(reasoning),
                    cache_read_tokens: Some(cache_read),
                    cache_creation_tokens: Some(cache_write),
                    duration_ms: None,
                    ttft_ms: None,
                    // 库内无逐调用错误载体（01-RESEARCH §15.2 差异④）
                    error_type: None,
                    source_id: Some(source_id),
                    is_background,
                });
            }
            if max_seen_ms > since_ms {
                *self.lock_last_seen_watermark() = max_seen_ms;
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

    /// 建一个合成 state.db（核心表列集与真实 schema 对齐，01-RESEARCH §15.2），
    /// 返回基准毫秒时间。会话：s1 全量列（title/cwd/model/provider）＋s2
    /// 最小列（display_name 兜底标题、last_activity_at NULL 回退 started_at）。
    /// 用量：u1 主循环全桶、u2 辅助调用（title_generation）、u3 provider 空回退。
    fn seed_state_db(home: &Path) -> i64 {
        let base = now_ms() - 3_600_000;
        std::fs::create_dir_all(home).unwrap();
        let db = home.join("state.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                display_name TEXT,
                model TEXT,
                billing_provider TEXT,
                billing_base_url TEXT,
                billing_mode TEXT,
                started_at REAL NOT NULL,
                ended_at REAL,
                end_reason TEXT,
                input_tokens INTEGER DEFAULT 0,
                output_tokens INTEGER DEFAULT 0,
                cache_read_tokens INTEGER DEFAULT 0,
                cache_write_tokens INTEGER DEFAULT 0,
                reasoning_tokens INTEGER DEFAULT 0,
                cwd TEXT,
                title TEXT,
                title_source TEXT,
                last_activity_at REAL,
                parent_session_id TEXT,
                archived INTEGER NOT NULL DEFAULT 0,
                hidden INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE session_model_usage (
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                model TEXT NOT NULL,
                billing_provider TEXT NOT NULL DEFAULT '',
                billing_base_url TEXT NOT NULL DEFAULT '',
                billing_mode TEXT NOT NULL DEFAULT '',
                task TEXT NOT NULL DEFAULT '',
                api_call_count INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                estimated_cost_usd REAL NOT NULL DEFAULT 0,
                actual_cost_usd REAL NOT NULL DEFAULT 0,
                cost_status TEXT,
                cost_source TEXT,
                first_seen REAL,
                last_seen REAL,
                PRIMARY KEY (session_id, model, billing_provider, billing_base_url, billing_mode, task));",
        )
        .unwrap();
        let s1_start = base as f64 / 1000.0;
        conn.execute(
            "INSERT INTO sessions (id, source, display_name, model, billing_provider,
                 started_at, last_activity_at, cwd, title)
             VALUES ('20260924_101010_a1', 'cli', NULL, 'glm-5.3', 'zhipu',
                 ?1, ?2, 'F:\\demo', '部署脚本调试')",
            rusqlite::params![s1_start, (base + 6_000) as f64 / 1000.0],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, source, display_name, started_at)
             VALUES ('20260924_102020_b2', 'telegram', '夜间巡检', ?1)",
            rusqlite::params![s1_start + 10.0],
        )
        .unwrap();
        // u1：主循环累计行（全五桶＋provider 直用）
        conn.execute(
            "INSERT INTO session_model_usage
                (session_id, model, billing_provider, task,
                 input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                 reasoning_tokens, api_call_count, last_seen)
             VALUES ('20260924_101010_a1', 'glm-5.3', 'zhipu', '',
                     120, 60, 30, 10, 25, 3, ?1)",
            rusqlite::params![(base + 2_000) as f64 / 1000.0],
        )
        .unwrap();
        // u2：辅助调用（标题生成）——is_background 应为 true
        conn.execute(
            "INSERT INTO session_model_usage
                (session_id, model, billing_provider, task,
                 input_tokens, output_tokens, api_call_count, last_seen)
             VALUES ('20260924_101010_a1', 'glm-5.3-flash', 'zhipu', 'title_generation',
                     50, 8, 1, ?1)",
            rusqlite::params![(base + 3_000) as f64 / 1000.0],
        )
        .unwrap();
        // u3：billing_provider 空（回退模型名推断）＋display_name 会话
        conn.execute(
            "INSERT INTO session_model_usage
                (session_id, model, task, input_tokens, output_tokens, last_seen)
             VALUES ('20260924_102020_b2', 'claude-opus-4-6', '', 11, 7, ?1)",
            rusqlite::params![(base + 5_000) as f64 / 1000.0],
        )
        .unwrap();
        base
    }

    fn temp_home(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("at-hermes-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 状态根解析：env 优先、无效 env 忽略、默认根＋profiles 枚举、去重
    #[test]
    fn test_resolve_homes() {
        let appdata = temp_home("roots");
        // 无任何目录 → 空
        assert!(resolve_homes(None, Some(appdata.clone())).is_empty());
        // 默认根 + profiles 子目录
        let default_root = appdata.join("hermes");
        std::fs::create_dir_all(default_root.join("profiles").join("coder")).unwrap();
        std::fs::create_dir_all(default_root.join("profiles").join("writer")).unwrap();
        let homes = resolve_homes(None, Some(appdata.clone()));
        assert_eq!(homes.len(), 3, "默认根 + 两个 profile");
        assert_eq!(homes[0].0, "default");
        assert_eq!(homes[1].0, "coder", "profiles 按名排序");
        assert_eq!(homes[2].0, "writer");
        // env 根存在 → 最优先
        let env_root = appdata.join("hermes-env");
        std::fs::create_dir_all(&env_root).unwrap();
        let homes = resolve_homes(Some(env_root.clone().into_os_string()), Some(appdata.clone()));
        assert_eq!(homes[0], ("env".into(), env_root.clone()));
        // env 指向不存在目录 → 忽略；env 与默认根同路径 → 去重
        let homes = resolve_homes(Some(appdata.join("nope").into_os_string()), Some(appdata.clone()));
        assert_eq!(homes.len(), 3, "坏 env 不阻断");
        let homes = resolve_homes(Some(default_root.clone().into_os_string()), Some(appdata.clone()));
        assert_eq!(homes.len(), 3, "env==默认根去重");
        assert!(homes.iter().all(|(tag, _)| tag != "env"));
        let _ = std::fs::remove_dir_all(&appdata);
    }

    /// 库发现：<home>/state.db；无库的 home 跳过
    #[test]
    fn test_discover_dbs() {
        let appdata = temp_home("disc");
        let default_root = appdata.join("hermes");
        let profile = default_root.join("profiles").join("coder");
        std::fs::create_dir_all(&profile).unwrap();
        seed_state_db(&default_root);
        assert!(discover_dbs(&resolve_homes(None, Some(appdata.clone()))).len() == 1);
        seed_state_db(&profile);
        let dbs = discover_dbs(&resolve_homes(None, Some(appdata.clone())));
        assert_eq!(dbs.len(), 2);
        assert_eq!(dbs[0].0, "default");
        assert_eq!(dbs[1].0, "coder");
        let _ = std::fs::remove_dir_all(&appdata);
    }

    /// scan：标题回退链（title > display_name）、cwd/model/provider 直取、
    /// 秒转毫秒、last_usage 回退 started_at、按最近活动降序
    #[test]
    fn test_scan_sessions() {
        let appdata = temp_home("scan");
        let default_root = appdata.join("hermes");
        let base = seed_state_db(&default_root);
        let ad = HermesAdapter::with_homes(vec![("default".into(), default_root)], 0);
        let sessions = ad.scan_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        // s2 活动（base+10s，回退 started_at）晚于 s1（base+6s）→ 排前
        assert_eq!(sessions[0].id, "hermes:default:20260924_102020_b2", "按最近活动降序");
        assert_eq!(sessions[0].title.as_deref(), Some("夜间巡检"), "display_name 兜底");
        assert_eq!(
            sessions[0].last_usage_at,
            Some(sessions[0].first_seen_at),
            "last_activity_at 缺失回退 started_at"
        );
        assert_eq!(sessions[1].id, "hermes:default:20260924_101010_a1");
        assert_eq!(sessions[1].title.as_deref(), Some("部署脚本调试"));
        assert_eq!(sessions[1].project_dir.as_deref(), Some("F:\\demo"));
        assert_eq!(sessions[1].model.as_deref(), Some("glm-5.3"));
        assert_eq!(sessions[1].provider.as_deref(), Some("zhipu"));
        assert_eq!(sessions[1].last_seen_at, base + 6_000, "REAL 秒应转毫秒");
        let _ = std::fs::remove_dir_all(&appdata);
    }

    /// 采集端到端：主循环/辅助调用行解析、provider 回退、秒转毫秒、自库幂等
    #[test]
    fn test_collect_end_to_end() {
        let appdata = temp_home("coll");
        let default_root = appdata.join("hermes");
        let base = seed_state_db(&default_root);
        let ad = HermesAdapter::with_homes(vec![("default".into(), default_root.clone())], 0);

        let out = ad.collect_usage(0).unwrap().rows;
        assert_eq!(out.len(), 3);
        // u1：主循环全桶（source_id=model:{task}:{base}:{mode}，task/mode 空）
        let u1 = out
            .iter()
            .find(|r| r.source_id.as_deref() == Some("hm:default:20260924_101010_a1:glm-5.3::zhipu::"))
            .unwrap();
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
        assert_eq!(u1.ts, base + 2_000, "last_seen 秒应转毫秒");
        assert_eq!(u1.provider.as_deref(), Some("zhipu"));
        assert_eq!(u1.session_id, "hermes:default:20260924_101010_a1");
        assert!(!u1.is_background);
        assert_eq!(u1.error_type, None, "库内无错误载体");
        // u2：辅助调用（标题生成）→ is_background
        let u2 = out
            .iter()
            .find(|r| r.source_id.as_deref() == Some("hm:default:20260924_101010_a1:glm-5.3-flash:title_generation:zhipu::"))
            .unwrap();
        assert!(u2.is_background, "task 非空应标后台行");
        assert_eq!((u2.input_tokens, u2.output_tokens), (Some(50), Some(8)));
        // u3：provider 空回退模型名推断
        let u3 = out
            .iter()
            .find(|r| r.source_id.as_deref() == Some("hm:default:20260924_102020_b2:claude-opus-4-6::::"))
            .unwrap();
        assert_eq!(u3.provider.as_deref(), Some("anthropic"), "billing 缺失回退模型推断");

        // 自库幂等：主循环行重放全忽略；仅 u2 后台行按 M1-12 设计无条件覆盖计数
        let store = crate::store::Store::open(&appdata.join("store.db")).unwrap();
        assert_eq!(store.insert_usage(&out), 3);
        assert_eq!(
            store.insert_usage(&out),
            1,
            "is_background 行无条件覆盖是既有设计，主循环行重放应全忽略"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&appdata);
    }

    /// last_seen 水位：同实例二次采集不重出旧行；累计行值增大后重采
    /// 产出同 source_id 更大快照（自库「保留最大」语义承接）
    #[test]
    fn test_watermark_incremental() {
        let appdata = temp_home("wm");
        let default_root = appdata.join("hermes");
        let base = seed_state_db(&default_root);
        let ad = HermesAdapter::with_homes(vec![("default".into(), default_root.clone())], 0);
        assert_eq!(ad.collect_usage(0).unwrap().rows.len(), 3);
        // 再次采集：进程内 last_seen 水位拦截，无新行
        assert!(ad.collect_usage(0).unwrap().rows.is_empty());

        // u1 行累计推进（Hermes 增量记账的模拟）：值翻倍 + last_seen 推进
        let db = default_root.join("state.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE session_model_usage
             SET input_tokens = input_tokens * 2, output_tokens = output_tokens * 2,
                 api_call_count = api_call_count + 1, last_seen = ?1
             WHERE session_id = '20260924_101010_a1' AND model = 'glm-5.3' AND task = ''",
            rusqlite::params![(base + 30_000) as f64 / 1000.0],
        )
        .unwrap();
        let rows = ad.collect_usage(0).unwrap().rows;
        assert_eq!(rows.len(), 1, "只有 last_seen 推进的行重入");
        assert_eq!(
            (rows[0].input_tokens, rows[0].output_tokens),
            (Some(240), Some(120)),
            "重采应取到更大累计快照"
        );
        assert_eq!(rows[0].ts, base + 30_000);
        // 同 source_id 重入自库：保留最大快照语义承接（重复入库零新增）
        let store = crate::store::Store::open(&appdata.join("store.db")).unwrap();
        assert_eq!(store.insert_usage(&rows), 1);
        drop(store);
        let _ = std::fs::remove_dir_all(&appdata);
    }

    /// 快轮信号：每 home 两个（state.db 与 state.db-wal），采样能取到值
    #[test]
    fn test_hot_signals() {
        let appdata = temp_home("sig");
        let default_root = appdata.join("hermes");
        std::fs::create_dir_all(&default_root).unwrap();
        let ad = HermesAdapter::with_homes(vec![("default".into(), default_root.clone())], 0);
        let signals = ad.hot_signals();
        assert_eq!(signals.len(), 2, "state.db + state.db-wal");
        // 采样值不 panic（文件不存在时 None 也不应出错）
        for s in &signals {
            if let HotSignal::File(p) = s {
                let _ = p();
            }
        }
        std::fs::write(default_root.join("state.db"), b"x").unwrap();
        if let HotSignal::File(p) = &signals[0] {
            assert_eq!(p(), Some(default_root.join("state.db")));
        }
        let _ = std::fs::remove_dir_all(&appdata);
    }

    /// 集成测试：连接本机真实 Hermes 库（未装跳过；装机后手动
    /// `cargo test -- --ignored` 补跑对账，清单见 01-RESEARCH §15.3）
    #[test]
    #[ignore]
    fn test_real_hermes_collect() {
        let homes = current_homes();
        let dbs = discover_dbs(&homes);
        assert!(!dbs.is_empty(), "本机应有 Hermes state.db");
        let ad = HermesAdapter::with_homes(homes, 0);
        let sessions = ad.scan_sessions().unwrap();
        assert!(!sessions.is_empty(), "本机应有 Hermes 会话");
        let usage = ad.collect_usage(0).unwrap().rows;
        assert!(!usage.is_empty(), "本机应有历史用量");
        for u in usage.iter().take(5) {
            assert!(u.ts > 1_700_000_000_000, "时间戳应为毫秒：{}", u.ts);
            assert_eq!(u.agent, "hermes");
            assert!(u.session_id.starts_with("hermes:"));
        }
        assert!(
            ad.collect_usage(9_999_999_999_999).unwrap().rows.is_empty(),
            "水位增量应为空"
        );
    }
}
