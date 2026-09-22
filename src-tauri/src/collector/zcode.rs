//! ZCode 适配器：只读 `~\.zcode\cli\db\db.sqlite` 的 model_usage/session 表。
//! 勘察结论见 docs/01-RESEARCH.md §1：数据齐全，无需 hooks，纯只读零侵入。
//! 时间戳均为 Unix 毫秒，与自库约定一致，零转换。
//!
//! M2 系列改造（04-EXPANSION）：
//!   ① M2-2 活动信号升级：`rollout/model-io-sess_*.jsonl` 的 per-session mtime
//!      纳入 last_usage_at——model_usage 行是调用完成后才落库，长回答期间（>90s）
//!      会从 working 掉回 idle；rollout 在每次模型调用边界追加，弥补该盲区；
//!   ② M2-3 机制迁移：只读打开走 engine 助手，scan/collect 按 ScanBudget 自律
//!      节流（SQLite 打开比文件 stat 贵一个量级，不跟随 1s 快节奏，§2.3.2）；
//!   ③ M2-1/M2-4：快轮信号（当日日志 + rollout 目录）与进程匹配声明化。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use super::engine::{open_sqlite_readonly, HotSignal, ProcessMatch, ScanBudget};
use super::{AgentAdapter, CollectOutput, SessionInfo, provider_from_model};
use crate::store::UsageRow;

/// SQLite 档扫描节流预算（毫秒，04-EXPANSION §2.3.2）：窗口内重复扫描直接返回
/// 上轮缓存——scan 与 collect 各持一份预算，避免相互吃掉配额
const SCAN_THROTTLE_MS: u64 = 2_000;

/// ZCode 数据库默认路径解析（%USERPROFILE%\.zcode\cli\db\db.sqlite）
fn default_db_path() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")?;
    let mut p = PathBuf::from(home);
    p.push(".zcode");
    p.push("cli");
    p.push("db");
    p.push("db.sqlite");
    Some(p)
}

/// rollout 转录目录（%USERPROFILE%\.zcode\cli\rollout，M2-2 活动信号源）
fn rollout_dir() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")?;
    Some(PathBuf::from(home).join(".zcode").join("cli").join("rollout"))
}

/// 当日 CLI 日志路径（zcode-YYYY-MM-DD.jsonl，每次工具调用边界实时追加——
/// 2026-09-22 实测；按日轮转，路径闭包内现算以覆盖跨零点切换）
fn today_log_path() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")?;
    let name = format!("zcode-{}.jsonl", chrono::Local::now().format("%Y-%m-%d"));
    Some(PathBuf::from(home).join(".zcode").join("cli").join("log").join(name))
}

/// rollout 目录的 per-session mtime 表：文件名 model-io-{sid}.jsonl → {sid} 的
/// 最新写入时间。一次 read_dir 全量带回，供 scan_sessions 逐会话增强活动信号
fn rollout_mtimes() -> HashMap<String, i64> {
    let mut out = HashMap::new();
    let Some(dir) = rollout_dir() else { return out };
    let Ok(rd) = std::fs::read_dir(&dir) else { return out };
    for e in rd.filter_map(|x| x.ok()) {
        let p = e.path();
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else { continue };
        let Some(sid) = stem.strip_prefix("model-io-") else { continue };
        out.insert(sid.to_string(), super::engine::mtime_ms(&p));
    }
    out
}

pub struct ZcodeAdapter {
    db_path: PathBuf,
    /// scan 节流预算（M2-3）：窗口内返回上轮缓存
    scan_budget: Mutex<ScanBudget>,
    /// collect 节流预算（M2-3）：同上，独立于 scan 防止相互吃配额
    collect_budget: Mutex<ScanBudget>,
    /// 上轮 scan 结果缓存（首扫前为空 → 返回空列表）
    scan_cache: Mutex<Option<Vec<SessionInfo>>>,
    /// 上轮 collect 结果缓存：以旧水位算出的行是超集，自库幂等键保证重复
    /// 入库零副作用，缓存返回安全
    collect_cache: Mutex<Option<CollectOutput>>,
}

impl ZcodeAdapter {
    /// 使用默认路径构造（生产档：启用 2s 节流）
    pub fn new() -> Self {
        Self::with_db(default_db_path().unwrap_or_else(|| PathBuf::from("")), SCAN_THROTTLE_MS)
    }

    /// 指定库路径与节流预算构造（单测注入用：throttle_ms=0 即每轮都实扫）
    pub fn with_db(db_path: PathBuf, throttle_ms: u64) -> Self {
        Self {
            db_path,
            scan_budget: Mutex::new(ScanBudget::new(throttle_ms)),
            collect_budget: Mutex::new(ScanBudget::new(throttle_ms)),
            scan_cache: Mutex::new(None),
            collect_cache: Mutex::new(None),
        }
    }

    /// 锁中毒自恢复（与 store 同策略，审查 1.1：单次 panic 不放大为连锁失败）
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

    /// 只读打开（WAL 并发读安全；ZCode 运行与否均可读）。打开逻辑在 engine（M2-3）
    fn open(&self) -> anyhow::Result<rusqlite::Connection> {
        if !self.db_path.exists() {
            anyhow::bail!("ZCode 数据库不存在：{}", self.db_path.display());
        }
        open_sqlite_readonly(&self.db_path)
    }
}

impl Default for ZcodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentAdapter for ZcodeAdapter {
    fn id(&self) -> &'static str {
        "zcode"
    }

    /// 快轮信号（M2-1）：①当日 CLI 日志（工具调用边界实时追加——回合起点的
    /// 最早可见信号）；②rollout 目录（每次模型调用边界追加）。日志按日轮转，
    /// 路径闭包内现算，跨零点自动切换到新文件（旧文件采样仍在，值不再变化）
    fn hot_signals(&self) -> Vec<HotSignal> {
        vec![
            HotSignal::File(std::sync::Arc::new(today_log_path)),
            match rollout_dir() {
                Some(dir) => HotSignal::DirScan { root: dir, ext: Some(".jsonl"), depth: 1, max_files: 200 },
                None => HotSignal::File(std::sync::Arc::new(|| None)),
            },
        ]
    }

    /// 进程匹配（M2-4 声明化，自 service.rs 迁移）：进程名含 zcode 即命中
    fn process_match(&self) -> Option<ProcessMatch> {
        Some(ProcessMatch { name_keywords: &["zcode"], cmd_keywords: &[], cmd_excludes: &[] })
    }

    /// 扫描最近 90 天有活动、且有过模型调用的会话（纯观测会话无意义）。
    /// M2-3 节流：预算窗口内返回上轮缓存；M2-2：scan 结果按 rollout per-session
    /// mtime 增强 last_usage_at（取与库内最近调用时间的较大者，增强的旧值会被
    /// 状态机 90s 活动窗口自然过滤，无副作用）
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        if !self.lock_scan_budget().ready() {
            return Ok(self.lock_scan_cache().clone().unwrap_or_default());
        }
        let conn = match self.open() {
            Ok(c) => c,
            Err(e) => {
                // 库不存在 = ZCode 未装（预期降级，静默）；其余打开失败 debug 留痕
                if self.db_path.exists() {
                    log::debug!("[zcode] 库打开失败（本轮按空处理）：{e:#}");
                }
                return Ok(vec![]);
            }
        };
        let cutoff = now_ms() - 90 * 24 * 3600 * 1000;
        let mut stmt = conn.prepare(
            "SELECT s.id, s.directory, s.title, s.time_created, s.time_updated,
                    (SELECT mu.model_id FROM model_usage mu
                      WHERE mu.session_id = s.id ORDER BY mu.started_at DESC LIMIT 1),
                    (SELECT MAX(mu.started_at) FROM model_usage mu WHERE mu.session_id = s.id)
             FROM session s
             WHERE s.time_updated > ?1
               AND EXISTS (SELECT 1 FROM model_usage mu WHERE mu.session_id = s.id)
             ORDER BY s.time_updated DESC
             LIMIT 100",
        )?;
        let rows = stmt.query_map([cutoff], |r| {
            let sid: String = r.get(0)?;
            let model: Option<String> = r.get(5)?;
            Ok(SessionInfo {
                id: format!("zcode:{sid}"),
                agent: "zcode".into(),
                provider: model.as_deref().and_then(provider_from_model),
                model,
                project_dir: r.get::<_, Option<String>>(1)?,
                title: r.get::<_, Option<String>>(2)?,
                first_seen_at: r.get::<_, i64>(3)?,
                last_seen_at: r.get::<_, i64>(4)?,
                last_usage_at: r.get::<_, Option<i64>>(6)?,
            })
        })?;
        // 行解析失败计数（Schema 漂移容忍，但持续失败需留痕排障）
        let mut err_rows = 0usize;
        let mut out = rows
            .filter_map(|x| match x {
                Ok(v) => Some(v),
                Err(_) => {
                    err_rows += 1;
                    None
                }
            })
            .collect::<Vec<_>>();
        if err_rows > 0 {
            log::debug!("[zcode] 扫描 {err_rows} 行解析失败已跳过（Schema 漂移？）");
        }
        // M2-2 活动信号增强：rollout 文件比库内最近调用更新 → 以文件 mtime 为准
        // （model_usage 只在调用完成后落库，长回答期间 rollout 已在推进——
        // 覆盖「>90s 无调用完成即掉 idle」的保真盲区，验收标准见 04-EXPANSION M2-2）
        let mtimes = rollout_mtimes();
        if !mtimes.is_empty() {
            for info in &mut out {
                let sid = info.id.trim_start_matches("zcode:");
                if let Some(m) = mtimes.get(sid) {
                    if *m > info.last_usage_at.unwrap_or(0) {
                        info.last_usage_at = Some(*m);
                    }
                }
            }
        }
        *self.lock_scan_cache() = Some(out.clone());
        Ok(out)
    }

    /// 水位增量读取 model_usage（列白名单，未知列忽略以容忍 Schema 漂移）。
    /// source_id = 源库行 id（"mu:{id}"，2026-09-21 幂等键升级）；
    /// error_type：用户主动取消（cancelled_by_user=1）不算错误——取消是正常操作
    /// 而非故障，计入"出错次数/错误分布"会污染口径（所有者 2026-09-21 拍板）。
    /// M2-3 节流：预算窗口内返回上轮缓存（旧行靠水位过滤 + 幂等键，重复入库零副作用）
    fn collect_usage(&self, watermark_ts: i64) -> anyhow::Result<CollectOutput> {
        if !self.lock_collect_budget().ready() {
            return Ok(self.lock_collect_cache().clone().unwrap_or_default());
        }
        let conn = match self.open() {
            Ok(c) => c,
            Err(e) => {
                // 库不存在 = ZCode 未装（预期降级，静默）；其余打开失败 debug 留痕
                if self.db_path.exists() {
                    log::debug!("[zcode] 库打开失败（本轮按空处理）：{e:#}");
                }
                return Ok(CollectOutput::default());
            }
        };
        let mut stmt = conn.prepare(
            "SELECT id, session_id, model_id, started_at,
                    input_tokens, output_tokens, reasoning_tokens,
                    cache_read_input_tokens, cache_creation_input_tokens,
                    duration_ms, time_to_first_token_ms,
                    CASE WHEN cancelled_by_user = 1 THEN NULL ELSE error_type END
             FROM model_usage
             WHERE started_at > ?1
             ORDER BY started_at ASC
             LIMIT 5000",
        )?;
        let rows = stmt.query_map([watermark_ts], |r| {
            // 源库 id 实测为 TEXT 型（uuid 类字符串），必须按 String 读
            let id: String = r.get(0)?;
            let session_id: String = r.get(1)?;
            let model: String = r.get(2)?;
            let provider = provider_from_model(&model);
            Ok(UsageRow {
                session_id: format!("zcode:{session_id}"),
                agent: "zcode".into(),
                model,
                provider,
                ts: r.get(3)?,
                input_tokens: r.get(4)?,
                output_tokens: r.get(5)?,
                reasoning_tokens: r.get(6)?,
                cache_read_tokens: r.get(7)?,
                cache_creation_tokens: r.get(8)?,
                duration_ms: r.get(9)?,
                ttft_ms: r.get(10)?,
                error_type: r.get(11)?,
                source_id: Some(format!("mu:{id}")),
                is_background: false,
            })
        })?;
        let mut err_rows = 0usize;
        let mut out = vec![];
        for x in rows {
            match x {
                Ok(v) => out.push(v),
                Err(_) => err_rows += 1,
            }
        }
        if err_rows > 0 {
            log::debug!("[zcode] 采集 {err_rows} 行解析失败已跳过（Schema 漂移？）");
        }
        let result = CollectOutput { rows: out, cost_snapshots: vec![], titles: vec![] };
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

    /// 集成测试：连接本机真实 ZCode 库（无 ZCode 环境跳过）
    /// 手动运行：cargo test -- --ignored
    /// 注意：节流会缓存 scan/collect 结果，增量断言须用无节流构造（04-EXPANSION §2.3.2）
    #[test]
    #[ignore]
    fn test_real_zcode_collect() {
        let ad = ZcodeAdapter::with_db(default_db_path().unwrap(), 0);
        let sessions = ad.scan_sessions().unwrap();
        assert!(!sessions.is_empty(), "本机应有活跃 ZCode 会话");
        assert!(sessions[0].id.starts_with("zcode:sess_"));
        assert!(sessions[0].last_usage_at.is_some());

        let usage = ad.collect_usage(0).unwrap().rows;
        assert!(!usage.is_empty(), "本机应有历史用量");
        for u in usage.iter().take(5) {
            assert!(u.ts > 1_700_000_000_000, "时间戳应为毫秒：{}", u.ts);
            assert_eq!(u.agent, "zcode");
            assert!(u.model.to_ascii_lowercase().starts_with("glm"), "当前主力模型：{}", u.model);
        }
        // 水位增量：用最大 ts 再采一次，应无新行
        let max_ts = usage.iter().map(|u| u.ts).max().unwrap();
        assert!(ad.collect_usage(max_ts).unwrap().rows.is_empty());
    }

    /// 幂等入自库：同批数据插两遍，第二遍 0 行
    #[test]
    #[ignore]
    fn test_real_zcode_into_store_idempotent() {
        let ad = ZcodeAdapter::with_db(default_db_path().unwrap(), 0);
        let usage = ad.collect_usage(0).unwrap().rows;
        let mut db = std::env::temp_dir();
        db.push(format!("at-t3-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let store = crate::store::Store::open(&db).unwrap();
        let n1 = store.insert_usage(&usage);
        assert_eq!(n1, usage.len());
        let n2 = store.insert_usage(&usage);
        assert_eq!(n2, 0, "重复插入应全部被幂等键忽略");
        let _ = std::fs::remove_file(&db);
    }

    /// M2-2：rollout 文件名解析为 per-session mtime 表（目录不存在=空表静默）
    #[test]
    fn test_rollout_mtimes_shape() {
        // 本机无 rollout 目录时恒为空（CI 场景）；有则键以 sess_ 开头
        let m = rollout_mtimes();
        for k in m.keys() {
            assert!(k.starts_with("sess_"), "rollout 文件名前缀约定 model-io-{{sid}}，实际键：{k}");
        }
    }
}
