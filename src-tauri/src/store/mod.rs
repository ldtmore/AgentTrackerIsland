//! 本地存储层：AgentTrackerIsland 自库（SQLite）的打开/迁移/读写封装。
//! 设计依据 docs/02-DESIGN.md §3；红线③（顺序无关）由幂等键与水位保证。
//! 线程模型：Connection 非 Sync，用 Mutex 包裹，单写多读经同一锁串行（M0 规模足够）。
//! 锁策略：中毒后自恢复（审查 1.1）——单次 panic 不应让后续所有调用连锁失败。

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension};

/// 初始化迁移脚本（0001）
const MIGRATION_0001: &str = include_str!("migrations/0001_init.sql");
/// 0002：用量表会话索引（会话级批量聚合加速）+ 移除从未使用的 watermarks.last_offset 列
const MIGRATION_0002: &str = include_str!("migrations/0002_indexes.sql");
/// 0003：幂等键升级 source_id + 后台用量标记 is_background（数据准确性治理 2026-09-21）；
/// 重建空表并在 app_settings 写 usage_rebuild_pending，由聚合器首轮归零水位全量回溯
const MIGRATION_0003: &str = include_str!("migrations/0003_source_key.sql");
/// 0004：无条件重建 usage_records（0003 落地修复）——0003 首版 SQL 的
/// CREATE IF NOT EXISTS 在已有旧表的库上被静默跳过但版本号已消耗，此类库的表
/// 停留在旧结构；0004 对任何中间状态一次收敛（已是新结构的库多重建一次，幂等无害）
const MIGRATION_0004: &str = include_str!("migrations/0004_rebuild_usage.sql");

/// 重建标志键（0003 写入，聚合器消费后删除）
pub const REBUILD_PENDING_KEY: &str = "usage_rebuild_pending";

/// 一条 token 用量流水（来自任一 Agent 适配器的增量采集）
#[derive(Debug, Clone)]
pub struct UsageRow {
    pub session_id: String,
    pub agent: String,
    pub model: String,
    pub provider: Option<String>,
    pub ts: i64, // Unix 毫秒
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub duration_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub error_type: Option<String>,
    /// 真实消息身份（2026-09-21 双计根治）：CC=message.id(+requestId)、
    /// ZCode=源库行 id、cost 校准行='cost:{sid}:{model}'；同源多行靠它幂等
    pub source_id: Option<String>,
    /// 后台用量校准行（cost-state 差值）：token 计入消耗，调用次数不计
    pub is_background: bool,
}

/// 一条额度快照（来自 Provider 适配器）
#[derive(Debug, Clone)]
pub struct QuotaRow {
    pub provider: String,
    pub window_kind: String, // '5h' | 'weekly'
    pub used_percent: Option<f64>,
    pub used_tokens: Option<i64>,
    pub reset_at: Option<i64>,
    pub fetched_at: i64,
}

/// 会话用量四项拆解（2026-09-18 展示改造）：相加即总消耗，与官方账单同口径——
/// 此前只展示 input+output，不含缓存，导致与任何官方后台数字都对不上
#[derive(Debug, Clone, Default)]
pub struct TokenBreakdown {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
}

impl TokenBreakdown {
    /// 总消耗（四项相加，账单口径）
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_creation
    }
}

/// 存储句柄：克隆 Arc 后全局共享
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// 打开（或创建）数据库并执行迁移；父目录不存在时自动创建
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::migrate(&conn)?;
        // SQLite 官方建议周期性执行：优化查询规划器统计（开销极小）
        let _ = conn.execute_batch("PRAGMA optimize;");
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// 迁移：按 user_version 顺序执行（0001 建库，0002 索引，后续版本递增）
    fn migrate(conn: &Connection) -> rusqlite::Result<()> {
        let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if ver < 1 {
            conn.execute_batch(MIGRATION_0001)?;
            conn.pragma_update(None, "user_version", 1)?;
            log::debug!("[存储] 迁移 0001 执行完成（首次建库）");
        }
        if ver < 2 {
            conn.execute_batch(MIGRATION_0002)?;
            conn.pragma_update(None, "user_version", 2)?;
            log::debug!("[存储] 迁移 0002 执行完成（用量索引）");
        }
        if ver < 3 {
            conn.execute_batch(MIGRATION_0003)?;
            conn.pragma_update(None, "user_version", 3)?;
            log::info!("[存储] 迁移 0003 执行完成（幂等键升级 source_id，用量表已清空待全量回溯重建）");
        }
        if ver < 4 {
            conn.execute_batch(MIGRATION_0004)?;
            conn.pragma_update(None, "user_version", 4)?;
            log::info!("[存储] 迁移 0004 执行完成（无条件重建用量表，修复 0003 可能的静默跳过）");
        }
        Ok(())
    }

    /// 取连接：Mutex 中毒后直接恢复内容继续用（Connection 内容在事务边界始终一致，
    /// 单次 panic 不应放大为全应用连锁失败——审查 1.1）
    fn lock_conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 读取某 Agent 的采集水位（时间戳，毫秒）；无记录返回 0
    pub fn get_watermark(&self, agent: &str) -> i64 {
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT last_ts FROM watermarks WHERE agent = ?1",
            params![agent],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
        .unwrap_or(0)
    }

    /// 更新采集水位（仅前进，不回退）
    pub fn set_watermark(&self, agent: &str, ts: i64) {
        let conn = self.lock_conn();
        if let Err(e) = conn.execute(
            "INSERT INTO watermarks(agent, last_ts) VALUES(?1, ?2)
             ON CONFLICT(agent) DO UPDATE SET last_ts = MAX(last_ts, excluded.last_ts)",
            params![agent, ts],
        ) {
            log::warn!("[存储] 水位写入失败（agent={agent}，下轮幂等重写）：{e}");
        }
    }

    /// upsert 会话元数据（首见时间不覆盖，最新状态全量刷新）
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_session(
        &self,
        id: &str,
        agent: &str,
        provider: Option<&str>,
        model: Option<&str>,
        project_dir: Option<&str>,
        title: Option<&str>,
        last_seen_at: i64,
        state: &str,
        state_reason: Option<&str>,
    ) {
        let conn = self.lock_conn();
        if let Err(e) = conn.execute(
            "INSERT INTO sessions(id, agent, provider, model, project_dir, title,
                                  first_seen_at, last_seen_at, state, state_reason)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?7,?8,?9)
             ON CONFLICT(id) DO UPDATE SET
               provider = COALESCE(excluded.provider, provider),
               model    = COALESCE(excluded.model, model),
               project_dir = COALESCE(excluded.project_dir, project_dir),
               title    = COALESCE(excluded.title, title),
               last_seen_at = MAX(last_seen_at, excluded.last_seen_at),
               state = excluded.state,
               state_reason = excluded.state_reason",
            params![id, agent, provider, model, project_dir, title, last_seen_at, state, state_reason],
        ) {
            log::warn!("[存储] 会话元数据写入失败（id={id}，下轮重写）：{e}");
        }
    }

    /// 幂等插入用量流水（2026-09-21 双计根治）：
    /// 幂等键 = (agent, session_id, source_id)——source_id 是真实消息身份，
    /// CC 流式复制快照行（同消息 timestamp 各异）在增量采集里靠它归并；
    /// 冲突时：普通行仅当新行四项合计更大才整行覆盖（"保留最大快照"口径），
    /// 后台校准行（存量 is_background=1）无条件覆盖——差值随 assistant 明细
    /// 增长会缩小，必须跟随重算值而非保留历史最大。
    /// source_id 为 NULL 的行不触发冲突（SQLite NULL≠NULL），多行共存仅作防御兜底；
    /// 返回实际变更行数（新插入或覆盖）。
    /// 事务/写失败不再 panic（审查 1.1）：记日志返回 0，等下一轮重采
    pub fn insert_usage(&self, rows: &[UsageRow]) -> usize {
        let mut conn = self.lock_conn();
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                log::warn!("insert_usage 开启事务失败（本轮 {} 行放弃，下轮重采）：{e}", rows.len());
                return 0;
            }
        };
        let mut changed = 0usize;
        for r in rows {
            let n = tx.execute(
                "INSERT INTO usage_records(
                   session_id, agent, model, provider, ts,
                   input_tokens, output_tokens, reasoning_tokens,
                   cache_read_tokens, cache_creation_tokens,
                   duration_ms, ttft_ms, error_type, source_id, is_background)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
                 ON CONFLICT(agent, session_id, source_id) DO UPDATE SET
                   input_tokens = excluded.input_tokens,
                   output_tokens = excluded.output_tokens,
                   reasoning_tokens = excluded.reasoning_tokens,
                   cache_read_tokens = excluded.cache_read_tokens,
                   cache_creation_tokens = excluded.cache_creation_tokens,
                   duration_ms = excluded.duration_ms,
                   ttft_ms = excluded.ttft_ms,
                   error_type = excluded.error_type,
                   ts = excluded.ts
                 WHERE is_background = 1
                    OR (COALESCE(excluded.input_tokens,0) + COALESCE(excluded.output_tokens,0)
                      + COALESCE(excluded.cache_read_tokens,0) + COALESCE(excluded.cache_creation_tokens,0))
                       >
                       (COALESCE(input_tokens,0) + COALESCE(output_tokens,0)
                      + COALESCE(cache_read_tokens,0) + COALESCE(cache_creation_tokens,0))",
                params![
                    r.session_id, r.agent, r.model, r.provider, r.ts,
                    r.input_tokens, r.output_tokens, r.reasoning_tokens,
                    r.cache_read_tokens, r.cache_creation_tokens,
                    r.duration_ms, r.ttft_ms, r.error_type,
                    r.source_id, r.is_background as i64
                ],
            );
            match n {
                Ok(v) => changed += v,
                Err(e) => log::warn!("insert_usage 单行写库失败（ts={} model={}）：{e}", r.ts, r.model),
            }
        }
        if let Err(e) = tx.commit() {
            log::warn!("insert_usage 提交事务失败（本轮全部回滚，下轮重采）：{e}");
            return 0;
        }
        changed
    }

    /// 插入额度快照
    pub fn insert_quota(&self, row: &QuotaRow) {
        let conn = self.lock_conn();
        if let Err(e) = conn.execute(
            "INSERT INTO quota_snapshots(provider, window_kind, used_percent, used_tokens, reset_at, fetched_at)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![row.provider, row.window_kind, row.used_percent, row.used_tokens, row.reset_at, row.fetched_at],
        ) {
            log::warn!("[存储] 额度快照写入失败（{} {}%，下轮重查补上）：{e}", row.window_kind, row.used_percent.unwrap_or(0.0) as i64);
        }
    }

    /// 插入原始状态事件（hooks/采集审计）
    pub fn insert_status_event(&self, agent: &str, session_id: Option<&str>, hook: &str, payload: &str, ts: i64) {
        let conn = self.lock_conn();
        let _ = conn.execute(
            "INSERT INTO status_events(agent, session_id, hook, payload, ts) VALUES(?1,?2,?3,?4,?5)",
            params![agent, session_id, hook, payload, ts],
        );
    }

    /// 设置项读写
    pub fn get_setting(&self, key: &str) -> Option<String> {
        let conn = self.lock_conn();
        conn.query_row("SELECT value FROM app_settings WHERE key = ?1", params![key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) {
        let conn = self.lock_conn();
        let _ = conn.execute(
            "INSERT INTO app_settings(key, value) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        );
    }

    /// 某会话累计 token（input+output）。单会话点查，仅供测试与工具用途；
    /// 展示层走 session_usage_breakdown 批量版（账单口径含缓存，审查 2.2.2 治理 N+1）
    pub fn session_usage_total(&self, session_id: &str) -> i64 {
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT COALESCE(SUM(COALESCE(input_tokens,0)+COALESCE(output_tokens,0)),0)
             FROM usage_records WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .unwrap_or(0)
    }

    /// 批量：一组会话的用量四项拆解（2026-09-18 展示改造，替代旧的 input+output
    /// 单总和版 session_usage_totals）。一次 GROUP BY 替代每会话一次点查
    /// （审查 2.2.2），前端 tooltip 拆解与总消耗（账单口径）共用本查询
    pub fn session_usage_breakdown(
        &self,
        session_ids: &[String],
    ) -> std::collections::HashMap<String, TokenBreakdown> {
        let mut out = std::collections::HashMap::new();
        if session_ids.is_empty() {
            return out;
        }
        let conn = self.lock_conn();
        // 占位符仅由内部拼接（元素为自产会话 id），无外部输入
        let placeholders = session_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT session_id,
                    COALESCE(SUM(COALESCE(input_tokens,0)),0),
                    COALESCE(SUM(COALESCE(output_tokens,0)),0),
                    COALESCE(SUM(COALESCE(cache_read_tokens,0)),0),
                    COALESCE(SUM(COALESCE(cache_creation_tokens,0)),0)
             FROM usage_records WHERE session_id IN ({placeholders}) GROUP BY session_id"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            log::warn!("session_usage_breakdown 查询准备失败");
            return out;
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                TokenBreakdown {
                    input: r.get(1)?,
                    output: r.get(2)?,
                    cache_read: r.get(3)?,
                    cache_creation: r.get(4)?,
                },
            ))
        });
        if let Ok(it) = rows {
            for (sid, breakdown) in it.filter_map(|x| x.ok()) {
                out.insert(sid, breakdown);
            }
        }
        out
    }

    /// 今日用量汇总（账单口径四项相加）： 本机今日零点之后的 token 总量与调用次数。
    /// 供胶囊/面板"今日"口径展示（2026-09-18 展示改造，替代误导性的全历史"累计"）。
    /// 调用次数不含后台校准行（is_background，cost-state 差值非真实调用）
    pub fn today_usage(&self, day_start_ms: i64) -> (i64, i64) {
        let conn = self.lock_conn();
        let result: rusqlite::Result<(i64, i64)> = conn.query_row(
            "SELECT COALESCE(SUM(COALESCE(input_tokens,0)+COALESCE(output_tokens,0)
                    +COALESCE(cache_read_tokens,0)+COALESCE(cache_creation_tokens,0)),0),
                    COALESCE(SUM(NOT is_background),0)
             FROM usage_records WHERE ts >= ?1",
            params![day_start_ms],
            |r| Ok((r.get(0)?, r.get(1)?)),
        );
        match result {
            Ok(v) => v,
            Err(e) => {
                log::warn!("today_usage 查询失败（按 0 处理）：{e}");
                (0, 0)
            }
        }
    }

    /// 批量：每个会话最近一次出错的错误类型与时间（无错误的会话不在结果中）。
    /// 供面板"出错 · 限流/额度耗尽"等具体原因展示（2026-09-18 展示改造）
    pub fn latest_session_errors(
        &self,
        session_ids: &[String],
    ) -> std::collections::HashMap<String, (String, i64)> {
        let mut out = std::collections::HashMap::new();
        if session_ids.is_empty() {
            return out;
        }
        let conn = self.lock_conn();
        let placeholders = session_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        // 与 latest_session_models 同构：窗口函数取每会话最近一条有 error_type 的记录
        let sql = format!(
            "SELECT session_id, error_type, ts FROM (
               SELECT session_id, error_type, ts,
                      ROW_NUMBER() OVER (PARTITION BY session_id ORDER BY ts DESC) AS rn
               FROM usage_records
               WHERE session_id IN ({placeholders}) AND error_type IS NOT NULL
             ) WHERE rn = 1"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            log::warn!("latest_session_errors 查询准备失败");
            return out;
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        });
        if let Ok(it) = rows {
            for (sid, err, ts) in it.filter_map(|x| x.ok()) {
                out.insert(sid, (err, ts));
            }
        }
        out
    }

    /// 批量：每个会话最近一次调用所用模型（Claude Code scan 阶段拿不到 model，
    /// 展示时兜底回填）。窗口函数取每会话 ts 最大一行，替代逐会话点查（审查 2.2.2）。
    /// 排除后台校准行：cost-state 差值行的模型（如 flash）不代表会话主模型
    pub fn latest_session_models(&self, session_ids: &[String]) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        if session_ids.is_empty() {
            return out;
        }
        let conn = self.lock_conn();
        let placeholders = session_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT session_id, model FROM (
               SELECT session_id, model,
                      ROW_NUMBER() OVER (PARTITION BY session_id ORDER BY ts DESC) AS rn
               FROM usage_records WHERE session_id IN ({placeholders}) AND is_background = 0
             ) WHERE rn = 1"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            log::warn!("latest_session_models 查询准备失败");
            return out;
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        });
        if let Ok(it) = rows {
            for (sid, model) in it.filter_map(|x| x.ok()) {
                out.insert(sid, model);
            }
        }
        out
    }

    /// 每个供应商+窗口的最新额度快照
    pub fn latest_quotas(&self) -> Vec<QuotaRow> {
        let conn = self.lock_conn();
        let mut stmt = match conn.prepare(
            "SELECT provider, window_kind, used_percent, used_tokens, reset_at, fetched_at
             FROM quota_snapshots q
             WHERE id = (SELECT MAX(id) FROM quota_snapshots
                          WHERE provider=q.provider AND window_kind=q.window_kind)",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows = stmt.query_map([], |r| {
            Ok(QuotaRow {
                provider: r.get(0)?,
                window_kind: r.get(1)?,
                used_percent: r.get(2)?,
                used_tokens: r.get(3)?,
                reset_at: r.get(4)?,
                fetched_at: r.get(5)?,
            })
        });
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(_) => vec![],
        }
    }

    /// 查会话元数据（project_dir/agent），跳转窗口用
    pub fn get_session_meta(&self, id: &str) -> Option<(String, Option<String>)> {
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT agent, project_dir FROM sessions WHERE id = ?1",
            params![id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .ok()
        .flatten()
    }

    /// 某会话某模型的真实调用（非后台）四项用量合计。
    /// cost-state 校准（2026-09-21）用：后台差值 = cost 累计快照 − 本值（下限 0）
    pub fn assistant_model_total(&self, session_id: &str, model_lower: &str) -> i64 {
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT COALESCE(SUM(COALESCE(input_tokens,0)+COALESCE(output_tokens,0)
                    +COALESCE(cache_read_tokens,0)+COALESCE(cache_creation_tokens,0)),0)
             FROM usage_records
             WHERE session_id = ?1 AND is_background = 0 AND LOWER(model) = ?2",
            params![session_id, model_lower],
            |r| r.get(0),
        )
        .unwrap_or(0)
    }

    /// 批量：一组会话已入库的标题（sessions 表持久层）。
    /// 岛面板快照兜底用——CC 标题只随增量采集入库，快照不能依赖"本轮恰好采到"，
    /// 否则文件无新行时标题丢失回退目录名（2026-09-21 实测踩中）
    pub fn session_titles(&self, session_ids: &[String]) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        if session_ids.is_empty() {
            return out;
        }
        let conn = self.lock_conn();
        let placeholders = session_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, title FROM sessions
             WHERE id IN ({placeholders}) AND title IS NOT NULL AND title != ''"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            log::warn!("session_titles 查询准备失败");
            return out;
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        });
        if let Ok(it) = rows {
            for (sid, title) in it.filter_map(|x| x.ok()) {
                out.insert(sid, title);
            }
        }
        out
    }

    /// 清空采集水位（0003 重建流程：配合已清空的用量表，触发采集层全量回溯）
    pub fn clear_watermarks(&self) {
        let conn = self.lock_conn();
        if let Err(e) = conn.execute("DELETE FROM watermarks", []) {
            log::warn!("[存储] 水位清空失败（重建流程）：{e}");
        }
    }

    /// 读取全部设置（设置页展示）
    pub fn all_settings(&self) -> std::collections::HashMap<String, String> {
        let conn = self.lock_conn();
        let Ok(mut stmt) = conn.prepare("SELECT key, value FROM app_settings") else {
            return std::collections::HashMap::new();
        };
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)));
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(_) => std::collections::HashMap::new(),
        }
    }

    /// 数据清理：删除 before_ts 之前的用量/快照/事件（设置页滚动周期用）
    pub fn cleanup_older_than(&self, before_ts: i64) -> u64 {
        let conn = self.lock_conn();
        let mut total = 0usize;
        for sql in [
            "DELETE FROM usage_records WHERE ts < ?1",
            "DELETE FROM quota_snapshots WHERE fetched_at < ?1",
            "DELETE FROM status_events WHERE ts < ?1",
        ] {
            total += conn.execute(sql, params![before_ts]).unwrap_or(0);
        }
        total as u64
    }
}

// ===== 报表聚合查询（M1-R1 重构：范围档＋维度筛选＋整页快照） =====
// 设计：单一 report_snapshot 一次返回整页数据（各图口径不会瞬间不一致）；
// 会话明细独立分页命令翻页。所有过滤值参数化绑定，维度/粒度表达式只出自
// 内部白名单 match（沿用审查 3.3"结构上不可能注入"的原则）。
// 查询统一 LEFT JOIN sessions（取项目维度，idx_usage_session 索引覆盖）

/// 四项用量合计表达式（账单口径，与岛面板/官方账单同口径）
const TOTAL_EXPR: &str = "COALESCE(u.input_tokens,0)+COALESCE(u.output_tokens,0)+COALESCE(u.cache_read_tokens,0)+COALESCE(u.cache_creation_tokens,0)";

/// 单维度分组行（Agent/项目/模型/供应商共用）：token 总量 + 调用次数
#[derive(Debug, Clone, serde::Serialize)]
pub struct SliceUsage {
    pub label: String,
    pub total: i64,
    pub calls: i64,
}

/// 汇总卡指标（当前范围＋筛选下的全量口径）。
/// R2 扩展：duration/ttft 为 Option——转录无时长字段的 Agent（Claude Code）
/// 全 NULL 时 SUM/AVG 得 None，前端显示 —（降级而非误报 0）
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SummaryStats {
    pub total_tokens: i64,
    pub calls: i64,
    pub sessions: i64,
    pub projects: i64,
    pub errors: i64,
    /// 模型生成时长合计（毫秒）
    pub duration_ms: Option<i64>,
    /// 平均首字延迟 TTFT（毫秒）
    pub ttft_avg_ms: Option<i64>,
    /// 思考 token 合计（思考占比分子）
    pub reasoning_tokens: i64,
    /// 输入+输出合计（思考占比分母；缓存读写不计入，口径见报表页脚注）
    pub billable_tokens: i64,
}

/// 趋势行：时间桶 + 四项用量 + 调用次数 + 生成时长（前端切换指标不再回查）
#[derive(Debug, Clone, serde::Serialize)]
pub struct TrendRow {
    pub bucket: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    pub calls: i64,
    /// 该桶生成时长合计（毫秒）；无时长数据为 None
    pub duration_ms: Option<i64>,
}

/// 热力图单元：星期×小时，token 与次数双指标（前端切换）
#[derive(Debug, Clone, serde::Serialize)]
pub struct HeatCell {
    pub weekday: i32, // 0=周日 … 6=周六（SQLite strftime %w）
    pub hour: i32,    // 0–23
    pub total: i64,
    pub calls: i64,
}

/// 会话中心行（会话窗口主查询：范围＋筛选下逐会话聚合，状态取自 sessions 表最后已知值）
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionRow {
    pub session_id: String,
    pub agent: String,
    /// 最后已知状态（聚合器每 tick 落库；无 sessions 行时为 None，按已结束展示）
    pub state: Option<String>,
    pub model: Option<String>,
    pub project_dir: Option<String>,
    pub title: Option<String>,
    pub first_ts: i64,
    pub last_ts: i64,
    pub calls: i64,
    pub total_tokens: i64,
    /// 四项拆解（悬浮明细用，与账单同口径）
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    /// 思考 token 合计（M1-11 补充露出；报表「思考占比」同源字段）
    pub reasoning_tokens: i64,
    /// 模型生成时长合计（毫秒）；转录无时长字段的 Agent（Claude Code）为 None
    pub duration_ms: Option<i64>,
    /// 平均首字延迟（毫秒）；无 ttft 记录的会话为 None
    pub ttft_avg_ms: Option<i64>,
    /// 出错的调用条数（0 = 从未出错）
    pub errors: i64,
    /// 出现过的错误类型（逗号分隔去重；从未出错为 None）
    pub error_types: Option<String>,
}

/// 会话分页结果（page_size 随行下发，前端页数计算免双源常量）
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionPage {
    pub total: i64,
    pub page_size: i64,
    pub rows: Vec<SessionRow>,
}

/// 会话详情：单次模型调用行（抽屉「调用流水」，M1-11）
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionCallRow {
    pub ts: i64,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub duration_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub error_type: Option<String>,
}

/// 会话详情：状态事件行（抽屉「状态时间线」，M1-11）
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionEventRow {
    pub ts: i64,
    pub hook: Option<String>,
    pub payload: Option<String>,
}

/// 会话详情包（调用流水 + 状态时间线）。
/// calls_total / events_total 为库中真实总数：列表受 LIMIT 截断时，
/// 前端据此诚实展示「N / 共 M」，避免截断后的 N 冒充全量
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SessionDetail {
    pub calls: Vec<SessionCallRow>,
    pub events: Vec<SessionEventRow>,
    pub calls_total: i64,
    pub events_total: i64,
}

/// 筛选下拉选项（只受范围影响、不受其他筛选影响——保证任意组合都能选中）
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FilterOptions {
    pub agents: Vec<String>,
    /// 项目目录全路径列表；空串表示"未知项目"（project_dir 缺失的会话）
    pub projects: Vec<String>,
    pub models: Vec<String>,
}

/// 整页报表快照（单命令返回，图与图之间口径一致）
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReportSnapshot {
    pub summary: SummaryStats,
    pub trend: Vec<TrendRow>,
    pub by_agent: Vec<SliceUsage>,
    pub by_project: Vec<SliceUsage>,
    pub by_model: Vec<SliceUsage>,
    pub by_provider: Vec<SliceUsage>,
    /// 错误类型分布（仅 error_type 非空的记录；限流/取消/网络等）
    pub by_error: Vec<SliceUsage>,
    pub heatmap: Vec<HeatCell>,
    pub options: FilterOptions,
}

/// 会话中心每页行数（Rust 侧权威值，随 SessionPage 下发给前端）
const SESSION_PAGE_SIZE: i64 = 20;

/// 「已结束」判定阈值：空闲超过该时长按已结束展示（与前端 shared/sessionDisplay.ts
/// 的 ENDED_AFTER_MS 同值同义，两处改动必须同步——岛面板与会话窗口靠它对齐口径）
const ENDED_AFTER_MS: i64 = 2 * 3_600_000;

/// 报表筛选上下文：范围起点 + 三个维度过滤（None=不过滤；项目空串=未知项目）
struct ReportFilter {
    cutoff_ms: i64,
    agent: Option<String>,
    project: Option<String>,
    model: Option<String>,
}

/// 当前 Unix 毫秒
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 本机今日零点（毫秒）：交给 SQLite 按本机时区计算（'localtime' 读 OS 时区），
/// 与按日聚合口径同源，避免 Rust 侧手写时区/夏令时换算。
/// 末尾 'utc' 必不可少：把"本地零点"字符串按本地时区转回 UTC 再取 epoch
/// （缺了会把本地字面时间当 UTC，偏移一个时区差）
fn today_start_ms(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT CAST(strftime('%s','now','localtime','start of day','utc') AS INTEGER)*1000",
        [],
        |r| r.get(0),
    )
    .ok()
}

/// 范围档白名单解析 + cutoff 计算。
/// 档位固定五种：today（今日零点）｜7d/30d/90d（滚动窗口）｜all（全部历史）；
/// 白名单外的值返回 None（命令层向前端报错，防任意参数透传）
fn build_filter(
    conn: &Connection,
    range: &str,
    agent: Option<&str>,
    project: Option<&str>,
    model: Option<&str>,
) -> Option<ReportFilter> {
    let cutoff_ms = match range {
        "today" => today_start_ms(conn)?,
        "all" => 0,
        "7d" | "30d" | "90d" => {
            let d: i64 = range.trim_end_matches('d').parse().ok()?;
            now_ms() - d * 86_400_000
        }
        _ => return None,
    };
    Some(ReportFilter {
        cutoff_ms,
        agent: agent.map(String::from),
        project: project.map(String::from),
        model: model.map(String::from),
    })
}

/// 趋势时间桶表达式（粒度随范围自动派生，业界分析工具标准做法）：
/// today→小时（看当天节奏）｜7d/30d→日｜90d→周（30 根日柱太密）｜all→月
fn bucket_expr(range: &str) -> &'static str {
    match range {
        "today" => "strftime('%Y-%m-%d %H:00', u.ts/1000,'unixepoch','localtime')",
        "7d" | "30d" => "date(u.ts/1000,'unixepoch','localtime')",
        "90d" => "date(u.ts/1000,'unixepoch','localtime','-6 days','weekday 1')",
        _ => "strftime('%Y-%m', u.ts/1000,'unixepoch','localtime')",
    }
}

/// 维度分组表达式（白名单 match，空串=未知项目与下拉"全部"（null）区分）
fn dim_expr(dim: &str) -> &'static str {
    match dim {
        "agent" => "u.agent",
        "project" => "COALESCE(s.project_dir,'')",
        "model" => "LOWER(u.model)",
        _ => "COALESCE(u.provider,'unknown')",
    }
}

/// 公共 WHERE 片段与参数（调用方 FROM 统一为
/// usage_records u LEFT JOIN sessions s ON s.id=u.session_id）
fn filter_where(f: &ReportFilter) -> (String, Vec<rusqlite::types::Value>) {
    let mut sql = String::new();
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    sql.push_str(" WHERE u.ts >= ?");
    params.push(f.cutoff_ms.into());
    if let Some(a) = &f.agent {
        sql.push_str(" AND u.agent = ?");
        params.push(a.clone().into());
    }
    if let Some(p) = &f.project {
        if p.is_empty() {
            // 空串=未知项目：匹配 project_dir 缺失（NULL 或空）的会话
            sql.push_str(" AND COALESCE(s.project_dir,'') = ''");
        } else {
            sql.push_str(" AND s.project_dir = ?");
            params.push(p.clone().into());
        }
    }
    if let Some(m) = &f.model {
        sql.push_str(" AND LOWER(u.model) = LOWER(?)");
        params.push(m.clone().into());
    }
    (sql, params)
}

impl Store {
    /// 构建报表筛选上下文（范围白名单外的档位返回 None）
    fn report_filter(
        &self,
        range: &str,
        agent: Option<&str>,
        project: Option<&str>,
        model: Option<&str>,
    ) -> Option<ReportFilter> {
        let conn = self.lock_conn();
        build_filter(&conn, range, agent, project, model)
    }

    /// 汇总卡：总量/次数/会话数/活跃项目数/错误次数 + 时长/TTFT/思考占比原料
    fn report_summary(&self, f: &ReportFilter) -> SummaryStats {
        let conn = self.lock_conn();
        let (where_sql, params) = filter_where(f);
        let sql = format!(
            "SELECT COALESCE(SUM({TOTAL_EXPR}),0), COALESCE(SUM(NOT u.is_background),0),
                    COUNT(DISTINCT u.session_id),
                    COUNT(DISTINCT COALESCE(s.project_dir,'')),
                    COALESCE(SUM(u.error_type IS NOT NULL),0),
                    SUM(u.duration_ms),
                    CAST(AVG(u.ttft_ms) AS INTEGER),
                    COALESCE(SUM(COALESCE(u.reasoning_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.input_tokens,0)+COALESCE(u.output_tokens,0)),0)
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return SummaryStats::default();
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(SummaryStats {
                total_tokens: r.get(0)?,
                calls: r.get(1)?,
                sessions: r.get(2)?,
                projects: r.get(3)?,
                errors: r.get(4)?,
                duration_ms: r.get(5)?,
                ttft_avg_ms: r.get(6)?,
                reasoning_tokens: r.get(7)?,
                billable_tokens: r.get(8)?,
            })
        });
        match rows {
            Ok(mut it) => it.next().unwrap_or(Ok(SummaryStats::default())).unwrap_or_default(),
            Err(_) => SummaryStats::default(),
        }
    }

    /// 趋势：按范围派生粒度分桶，四项用量 + 次数 + 生成时长
    fn report_trend(&self, f: &ReportFilter, range: &str) -> Vec<TrendRow> {
        let conn = self.lock_conn();
        let bucket = bucket_expr(range);
        let (where_sql, params) = filter_where(f);
        let sql = format!(
            "SELECT {bucket} AS b,
                    COALESCE(SUM(COALESCE(u.input_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.output_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.cache_read_tokens,0)),0),
                    COALESCE(SUM(COALESCE(u.cache_creation_tokens,0)),0),
                    COALESCE(SUM(NOT u.is_background),0),
                    SUM(u.duration_ms)
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}
             GROUP BY b ORDER BY b"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return vec![];
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(TrendRow {
                bucket: r.get(0)?,
                input: r.get(1)?,
                output: r.get(2)?,
                cache_read: r.get(3)?,
                cache_creation: r.get(4)?,
                calls: r.get(5)?,
                duration_ms: r.get(6)?,
            })
        });
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(_) => vec![],
        }
    }

    /// 维度分组（Agent/项目/模型/供应商，dim 白名单见 dim_expr）
    fn report_by_dim(&self, f: &ReportFilter, dim: &str) -> Vec<SliceUsage> {
        let conn = self.lock_conn();
        let group = dim_expr(dim);
        let (where_sql, params) = filter_where(f);
        let sql = format!(
            "SELECT {group} AS label, COALESCE(SUM({TOTAL_EXPR}),0), COALESCE(SUM(NOT u.is_background),0)
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}
             GROUP BY label ORDER BY 2 DESC"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return vec![];
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(SliceUsage {
                label: r.get(0)?,
                total: r.get(1)?,
                calls: r.get(2)?,
            })
        });
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(_) => vec![],
        }
    }

    /// 周×小时热力图（本机时区，token 与次数双指标）
    fn report_heatmap(&self, f: &ReportFilter) -> Vec<HeatCell> {
        let conn = self.lock_conn();
        let (where_sql, params) = filter_where(f);
        let sql = format!(
            "SELECT CAST(strftime('%w', u.ts/1000,'unixepoch','localtime') AS INTEGER),
                    CAST(strftime('%H', u.ts/1000,'unixepoch','localtime') AS INTEGER),
                    COALESCE(SUM({TOTAL_EXPR}),0), COALESCE(SUM(NOT u.is_background),0)
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}
             GROUP BY 1,2"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return vec![];
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(HeatCell {
                weekday: r.get(0)?,
                hour: r.get(1)?,
                total: r.get(2)?,
                calls: r.get(3)?,
            })
        });
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(_) => vec![],
        }
    }

    /// 筛选下拉选项（仅按范围过滤，维度互不遮蔽）
    fn report_options(&self, f: &ReportFilter) -> FilterOptions {
        let conn = self.lock_conn();
        let mut out = FilterOptions::default();
        let lists: [(&str, &str); 3] = [
            ("agent", "SELECT DISTINCT u.agent FROM usage_records u WHERE u.ts >= ? ORDER BY 1"),
            ("project", "SELECT DISTINCT COALESCE(s.project_dir,'') FROM usage_records u
                         LEFT JOIN sessions s ON s.id = u.session_id WHERE u.ts >= ? ORDER BY 1"),
            ("model", "SELECT DISTINCT LOWER(u.model) FROM usage_records u WHERE u.ts >= ? ORDER BY 1"),
        ];
        for (key, sql) in lists {
            let Ok(mut stmt) = conn.prepare(sql) else {
                continue;
            };
            let rows = stmt.query_map([f.cutoff_ms], |r| r.get::<_, String>(0));
            if let Ok(it) = rows {
                let vals: Vec<String> = it.filter_map(|x| x.ok()).collect();
                match key {
                    "agent" => out.agents = vals,
                    "project" => out.projects = vals,
                    _ => out.models = vals,
                }
            }
        }
        out
    }

    /// 错误类型分布（限流/取消/网络等；仅统计 error_type 非空的记录）
    fn report_by_error(&self, f: &ReportFilter) -> Vec<SliceUsage> {
        let conn = self.lock_conn();
        let (where_sql, params) = filter_where(f);
        let sql = format!(
            "SELECT u.error_type AS label, COUNT(*)
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}
               AND u.error_type IS NOT NULL
             GROUP BY label ORDER BY 2 DESC"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return vec![];
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(SliceUsage {
                label: r.get(0)?,
                total: r.get(1)?,
                calls: r.get(1)?,
            })
        });
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(_) => vec![],
        }
    }

    /// 整页报表快照（一次调用返回全部图数据）
    pub fn report_snapshot(
        &self,
        range: &str,
        agent: Option<&str>,
        project: Option<&str>,
        model: Option<&str>,
    ) -> Option<ReportSnapshot> {
        let f = self.report_filter(range, agent, project, model)?;
        Some(ReportSnapshot {
            summary: self.report_summary(&f),
            trend: self.report_trend(&f, range),
            by_agent: self.report_by_dim(&f, "agent"),
            by_project: self.report_by_dim(&f, "project"),
            by_model: self.report_by_dim(&f, "model"),
            by_provider: self.report_by_dim(&f, "provider"),
            by_error: self.report_by_error(&f),
            heatmap: self.report_heatmap(&f),
            options: self.report_options(&f),
        })
    }

    /// 会话中心筛选下拉选项（轻查询：会话窗口不需要整页报表快照，
    /// 只要 Agent/项目/模型三张选项表；只受范围影响、不受维度筛选影响，
    /// 保证任意筛选组合下选项仍然齐全可切换）
    pub fn session_options(&self, range: &str) -> Option<FilterOptions> {
        let f = self.report_filter(range, None, None, None)?;
        Some(self.report_options(&f))
    }

    /// 会话中心分页（会话窗口主查询，M1-10）：范围＋三维度筛选同报表口径，
    /// 另加状态档（all｜active｜ended｜errored）/关键字（标题、项目路径模糊匹配）/
    /// 排序键（recent｜tokens｜calls｜duration，白名单映射防注入）/
    /// 每页行数（0 或负值回退默认，钳制 ≤200 限制 LIMIT 注入面）。
    /// 结构=内层按会话聚合子查询＋外层套状态/关键字过滤。
    /// 状态口径（2026-09-21 冻结修复）：active = 状态非 offline 且最近活动在
    /// ENDED_AFTER_MS 内——原来三态（working/waiting/error）无时间条件，会话离开
    /// 90 天观测列表后 sessions.state 冻结在最后值会被永久当"活跃"；统一时间窗后
    /// 与前端 shared/sessionDisplay.ts 的 ENDED_AFTER_MS 语义对齐；
    /// ended=其余（offline、空闲超时、冻结）。errored=出过错。
    /// 状态判定依赖聚合值 MAX(u.ts)，故过滤必须套在聚合之后
    pub fn session_page(
        &self,
        range: &str,
        agent: Option<&str>,
        project: Option<&str>,
        model: Option<&str>,
        status: &str,
        keyword: &str,
        sort: &str,
        page_size: i64,
        offset: i64,
    ) -> Option<SessionPage> {
        // 每页行数：非法值回退默认，上限 200
        let page_size = if page_size <= 0 { SESSION_PAGE_SIZE } else { page_size.min(200) };
        // 状态档与排序键先行白名单校验（未知值返回 None，命令层向前端报错）
        let status_sql: &str = match status {
            "all" => "",
            "active" => " AND (state != 'offline' AND last_ts >= ?)",
            "ended" => " AND NOT (state != 'offline' AND last_ts >= ?)",
            "errored" => " AND errors > 0",
            _ => return None,
        };
        let order_sql: &str = match sort {
            "recent" => "last_ts DESC",
            "tokens" => "total_tokens DESC",
            "calls" => "calls DESC",
            "duration" => "duration_ms DESC",
            _ => return None,
        };
        let f = self.report_filter(range, agent, project, model)?;
        let conn = self.lock_conn();
        let (where_sql, mut params) = filter_where(&f);
        // 内层子查询：按会话聚合（别名供外层过滤/排序引用，避免脆弱的列号）；
        // calls 不含后台校准行（cost-state 差值非真实调用）
        let inner = format!(
            "SELECT u.session_id, COALESCE(s.agent, u.agent) AS agent,
                    COALESCE(s.state,'offline') AS state, s.model, s.project_dir, s.title,
                    MIN(u.ts) AS first_ts, MAX(u.ts) AS last_ts,
                    COALESCE(SUM(NOT u.is_background),0) AS calls,
                    COALESCE(SUM(COALESCE(u.input_tokens,0)),0) AS input_tokens,
                    COALESCE(SUM(COALESCE(u.output_tokens,0)),0) AS output_tokens,
                    COALESCE(SUM(COALESCE(u.cache_read_tokens,0)),0) AS cache_read_tokens,
                    COALESCE(SUM(COALESCE(u.cache_creation_tokens,0)),0) AS cache_creation_tokens,
                    COALESCE(SUM(COALESCE(u.reasoning_tokens,0)),0) AS reasoning_tokens,
                    COALESCE(SUM({TOTAL_EXPR}),0) AS total_tokens,
                    SUM(u.duration_ms) AS duration_ms,
                    CAST(AVG(u.ttft_ms) AS INTEGER) AS ttft_avg_ms,
                    COALESCE(SUM(u.error_type IS NOT NULL),0) AS errors,
                    GROUP_CONCAT(DISTINCT u.error_type) AS error_types
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}
             GROUP BY u.session_id"
        );
        // 关键字：转义 LIKE 通配符后模糊匹配标题与项目全路径（参数化，空/纯空白=不搜）
        let kw = keyword.trim();
        if !kw.is_empty() {
            let pat = format!("%{}%", escape_like(kw));
            params.push(pat.clone().into());
            params.push(pat.into());
        }
        // active/ended 的时间阈值参数（NOW − 2h），排在关键字之后
        if status == "active" || status == "ended" {
            params.push((now_ms() - ENDED_AFTER_MS).into());
        }
        // 占位顺序＝关键字两枚 → 状态一枚，与下方 SQL 文本顺序一致
        let kw_sql = if kw.is_empty() {
            ""
        } else {
            " AND (COALESCE(title,'') LIKE ? ESCAPE '\\' OR COALESCE(project_dir,'') LIKE ? ESCAPE '\\')"
        };
        // 总数：同一聚合子查询＋同一套过滤，口径与行查询完全一致
        let count_sql =
            format!("SELECT COUNT(*) FROM ({inner}) WHERE 1=1{kw_sql}{status_sql}");
        let total: i64 = conn
            .prepare(&count_sql)
            .and_then(|mut s| s.query_row(rusqlite::params_from_iter(params.iter()), |r| r.get(0)))
            .unwrap_or(0);
        // 行查询：LIMIT/OFFSET 参数追加在过滤参数之后
        let mut all_params = params;
        all_params.push(page_size.into());
        all_params.push(offset.into());
        let rows_sql =
            format!("SELECT * FROM ({inner}) WHERE 1=1{kw_sql}{status_sql} ORDER BY {order_sql} LIMIT ? OFFSET ?");
        let mut rows = Vec::new();
        if let Ok(mut stmt) = conn.prepare(&rows_sql) {
            let mapped = stmt.query_map(rusqlite::params_from_iter(all_params.iter()), |r| {
                Ok(SessionRow {
                    session_id: r.get("session_id")?,
                    agent: r.get("agent")?,
                    state: r.get("state")?,
                    model: r.get("model")?,
                    project_dir: r.get("project_dir")?,
                    title: r.get("title")?,
                    first_ts: r.get("first_ts")?,
                    last_ts: r.get("last_ts")?,
                    calls: r.get("calls")?,
                    input_tokens: r.get("input_tokens")?,
                    output_tokens: r.get("output_tokens")?,
                    cache_read_tokens: r.get("cache_read_tokens")?,
                    cache_creation_tokens: r.get("cache_creation_tokens")?,
                    reasoning_tokens: r.get("reasoning_tokens")?,
                    total_tokens: r.get("total_tokens")?,
                    duration_ms: r.get("duration_ms")?,
                    ttft_avg_ms: r.get("ttft_avg_ms")?,
                    errors: r.get("errors")?,
                    error_types: r.get("error_types")?,
                })
            });
            if let Ok(it) = mapped {
                rows = it.filter_map(|x| x.ok()).collect();
            }
        }
        Some(SessionPage {
            total,
            page_size,
            rows,
        })
    }

    /// 会话详情（M1-11 抽屉）：单会话的调用流水 + 状态事件时间线。
    /// 调用流水倒序（最新在上），上限 500 条防极端会话拖爆前端；
    /// 状态事件的 session_id 存在两代口径——hook 事件按原始 id 记账、
    /// 快照按 "{agent}:{id}" 命名空间 id——两个都查才不漏；
    /// 另查两表真实总数（COUNT），列表被 LIMIT 截断时前端诚实展示「N / 共 M」
    pub fn session_detail(&self, session_id: &str) -> SessionDetail {
        let conn = self.lock_conn();
        let mut detail = SessionDetail::default();
        // 原始 id：剥掉 "{agent}:" 前缀（无前缀时原样，防御）
        let raw_id = session_id.split_once(':').map(|x| x.1).unwrap_or(session_id);
        // 真实总数：一条查询带两个标量子查询（状态事件同样兼容两代 session_id 口径）；
        // 调用总数排除后台校准行（cost-state 差值非真实调用，流水同样不展示）
        if let Ok(mut stmt) = conn.prepare(
            "SELECT (SELECT COUNT(*) FROM usage_records
                      WHERE session_id = ?1 AND is_background = 0),
                    (SELECT COUNT(*) FROM status_events WHERE session_id = ?1 OR session_id = ?2)",
        ) {
            if let Ok(mut it) = stmt.query(rusqlite::params![session_id, raw_id]) {
                if let Ok(Some(row)) = it.next() {
                    detail.calls_total = row.get(0).unwrap_or(0);
                    detail.events_total = row.get(1).unwrap_or(0);
                }
            }
        }
        if let Ok(mut stmt) = conn.prepare(
            "SELECT ts, model,
                    COALESCE(input_tokens,0), COALESCE(output_tokens,0),
                    COALESCE(reasoning_tokens,0),
                    COALESCE(cache_read_tokens,0), COALESCE(cache_creation_tokens,0),
                    duration_ms, ttft_ms, error_type
             FROM usage_records WHERE session_id = ?1 AND is_background = 0 ORDER BY ts DESC LIMIT 500",
        ) {
            if let Ok(it) = stmt.query_map([session_id], |r| {
                Ok(SessionCallRow {
                    ts: r.get(0)?,
                    model: r.get(1)?,
                    input_tokens: r.get(2)?,
                    output_tokens: r.get(3)?,
                    reasoning_tokens: r.get(4)?,
                    cache_read_tokens: r.get(5)?,
                    cache_creation_tokens: r.get(6)?,
                    duration_ms: r.get(7)?,
                    ttft_ms: r.get(8)?,
                    error_type: r.get(9)?,
                })
            }) {
                detail.calls = it.filter_map(|x| x.ok()).collect();
            }
        }
        // 状态事件查询：命名空间 id 与原始 id 双口径都命中才不漏
        if let Ok(mut stmt) = conn.prepare(
            "SELECT ts, hook, payload FROM status_events
             WHERE session_id = ?1 OR session_id = ?2 ORDER BY ts DESC LIMIT 200",
        ) {
            if let Ok(it) = stmt.query_map(rusqlite::params![session_id, raw_id], |r| {
                Ok(SessionEventRow {
                    ts: r.get(0)?,
                    hook: r.get(1)?,
                    payload: r.get(2)?,
                })
            }) {
                detail.events = it.filter_map(|x| x.ok()).collect();
            }
        }
        detail
    }

    /// 按当前范围＋筛选＋状态档＋关键字导出会话列表 CSV（M1-10 随会话窗口迁移；
    /// 与 session_page 同一套子查询与占位顺序，所见即所得——排序也保留）。
    /// 纯函数只产字符串，写文件/落路径由命令层负责（可单测）；
    /// 带 UTF-8 BOM——Excel 直接打开中文表头不乱码；
    /// 时间列由 SQLite 按本机时区格式化（与页面口径同源）
    pub fn build_sessions_csv(
        &self,
        range: &str,
        agent: Option<&str>,
        project: Option<&str>,
        model: Option<&str>,
        status: &str,
        keyword: &str,
        sort: &str,
    ) -> Option<String> {
        // 白名单校验与 session_page 同款（状态口径同其 2026-09-21 冻结修复版）
        let status_sql: &str = match status {
            "all" => "",
            "active" => " AND (state != 'offline' AND last_ts >= ?)",
            "ended" => " AND NOT (state != 'offline' AND last_ts >= ?)",
            "errored" => " AND errors > 0",
            _ => return None,
        };
        let order_sql: &str = match sort {
            "recent" => "last_ts DESC",
            "tokens" => "total_tokens DESC",
            "calls" => "calls DESC",
            "duration" => "duration_ms DESC",
            _ => return None,
        };
        let f = self.report_filter(range, agent, project, model)?;
        let conn = self.lock_conn();
        let (where_sql, mut params) = filter_where(&f);
        // 内层子查询按会话聚合；first_ts/last_ts 保持整数毫秒——状态档要拿它和
        // ENDED_AFTER_MS 阈值做数值比较（若在此处格式化成文本，SQLite 类型序
        // TEXT>INTEGER 会让"空闲超时"判断永远为真，ended 档全空——单测逮住过）
        let inner = format!(
            "SELECT u.session_id, COALESCE(s.agent, u.agent) AS agent,
                    COALESCE(s.state,'offline') AS state, s.model,
                    COALESCE(s.project_dir,'') AS project_dir, COALESCE(s.title,'') AS title,
                    MIN(u.ts) AS first_ts, MAX(u.ts) AS last_ts,
                    COALESCE(SUM(NOT u.is_background),0) AS calls,
                    COALESCE(SUM(COALESCE(u.input_tokens,0)),0) AS input_tokens,
                    COALESCE(SUM(COALESCE(u.output_tokens,0)),0) AS output_tokens,
                    COALESCE(SUM(COALESCE(u.cache_read_tokens,0)),0) AS cache_read_tokens,
                    COALESCE(SUM(COALESCE(u.cache_creation_tokens,0)),0) AS cache_creation_tokens,
                    COALESCE(SUM(COALESCE(u.reasoning_tokens,0)),0) AS reasoning_tokens,
                    COALESCE(SUM({TOTAL_EXPR}),0) AS total_tokens,
                    SUM(u.duration_ms) AS duration_ms,
                    CAST(AVG(u.ttft_ms) AS INTEGER) AS ttft_avg,
                    COALESCE(SUM(u.error_type IS NOT NULL),0) AS errors,
                    GROUP_CONCAT(DISTINCT u.error_type) AS error_types
             FROM usage_records u LEFT JOIN sessions s ON s.id = u.session_id{where_sql}
             GROUP BY u.session_id"
        );
        let kw = keyword.trim();
        if !kw.is_empty() {
            let pat = format!("%{}%", escape_like(kw));
            params.push(pat.clone().into());
            params.push(pat.into());
        }
        if status == "active" || status == "ended" {
            params.push((now_ms() - ENDED_AFTER_MS).into());
        }
        let kw_sql = if kw.is_empty() {
            ""
        } else {
            " AND (title LIKE ? ESCAPE '\\' OR project_dir LIKE ? ESCAPE '\\')"
        };
        // 外层：时间列才格式化为本机时区文本；时长/首字/错误类型补默认值，单元格免判空
        let sql = format!(
            "SELECT session_id, agent, state, model, project_dir, title,
                    datetime(first_ts/1000,'unixepoch','localtime') AS first_local,
                    datetime(last_ts/1000,'unixepoch','localtime') AS last_local,
                    calls, input_tokens, output_tokens, reasoning_tokens, cache_read_tokens,
                    cache_creation_tokens, total_tokens,
                    COALESCE(duration_ms,0) AS duration_ms,
                    COALESCE(CAST(ttft_avg AS TEXT),'') AS ttft, errors,
                    COALESCE(error_types,'') AS error_types
             FROM ({inner}) WHERE 1=1{kw_sql}{status_sql} ORDER BY {order_sql}"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return Some(String::new());
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            // 文本列当场转义，数字列直接转字符串（无逗号/引号无需转义）
            Ok(vec![
                csv_cell(&r.get::<_, String>("session_id")?),
                csv_cell(&r.get::<_, String>("agent")?),
                csv_cell(&r.get::<_, String>("state")?),
                csv_cell(&r.get::<_, Option<String>>("model")?.unwrap_or_default()),
                csv_cell(&r.get::<_, String>("project_dir")?),
                csv_cell(&r.get::<_, String>("title")?),
                csv_cell(&r.get::<_, String>("first_local")?),
                csv_cell(&r.get::<_, String>("last_local")?),
                r.get::<_, i64>("calls")?.to_string(),
                r.get::<_, i64>("input_tokens")?.to_string(),
                r.get::<_, i64>("output_tokens")?.to_string(),
                r.get::<_, i64>("reasoning_tokens")?.to_string(),
                r.get::<_, i64>("cache_read_tokens")?.to_string(),
                r.get::<_, i64>("cache_creation_tokens")?.to_string(),
                r.get::<_, i64>("total_tokens")?.to_string(),
                r.get::<_, i64>("duration_ms")?.to_string(),
                csv_cell(&r.get::<_, String>("ttft")?),
                r.get::<_, i64>("errors")?.to_string(),
                csv_cell(&r.get::<_, String>("error_types")?),
            ])
        });
        let mut out = String::from("\u{feff}会话ID,Agent,状态,模型,项目,标题,首次调用,最近调用,调用次数,输入,输出,思考,缓存读,缓存写,总Token,生成时长(毫秒),平均首字(毫秒),出错次数,错误类型\r\n");
        if let Ok(it) = rows {
            for cells in it.filter_map(|x| x.ok()) {
                out.push_str(&cells.join(","));
                out.push_str("\r\n");
            }
        }
        Some(out)
    }
}

/// CSV 字段转义：含逗号/引号/换行时双引号包裹并双写内部引号（RFC 4180）
/// LIKE 通配符转义（\ % _）：用户关键字原样匹配而非当通配符解释，
/// 配合 SQL 里的 ESCAPE '\\' 使用（反斜杠本身先翻转，避免二次歧义）
fn escape_like(kw: &str) -> String {
    kw.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// CSV 字段转义：含逗号/引号/换行时双引号包裹并双写内部引号（RFC 4180）
fn csv_cell(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成临时测试库路径（进程级唯一，避免并行测试互踩）
    fn tmp_db(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("at-test-{}-{}.db", tag, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn sample_usage(ts: i64) -> UsageRow {
        UsageRow {
            session_id: "zcode:abc".into(),
            agent: "zcode".into(),
            model: "glm-5.3".into(),
            provider: Some("glm".into()),
            ts,
            input_tokens: Some(1000),
            output_tokens: Some(200),
            reasoning_tokens: Some(50),
            cache_read_tokens: Some(3000),
            cache_creation_tokens: Some(0),
            duration_ms: Some(7812),
            ttft_ms: Some(900),
            error_type: None,
            source_id: Some(format!("test:{ts}")),
            is_background: false,
        }
    }

    #[test]
    fn test_open_and_migrate() {
        let path = tmp_db("migrate");
        {
            let store = Store::open(&path).unwrap();
            store.set_setting("k", "v");
            assert_eq!(store.get_setting("k").as_deref(), Some("v"));
        }
        // 重复打开：迁移幂等，数据仍在
        let store2 = Store::open(&path).unwrap();
        assert_eq!(store2.get_setting("k").as_deref(), Some("v"));
        let _ = std::fs::remove_file(&path);
    }

    /// 0003 首版 SQL 曾在已有旧表的库上被 CREATE IF NOT EXISTS 静默跳过，
    /// 但 user_version 已消耗到 3——此类"被骗库"必须由 0004 无条件重建收敛。
    /// 回归锁定：ver=3 + 旧 13 列结构的库，open 后应升级为可用新结构
    /// （带 source_id/is_background 的行能成功写入）
    #[test]
    fn test_migrate_rescues_deceived_v3_db() {
        let path = tmp_db("deceived");
        {
            // 手工构造被骗库：旧 13 列结构 + user_version=3 + 重建标志已消费
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE watermarks(agent TEXT PRIMARY KEY, last_ts INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE usage_records (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   session_id TEXT NOT NULL, agent TEXT NOT NULL, model TEXT NOT NULL,
                   provider TEXT, ts INTEGER NOT NULL,
                   input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER,
                   cache_read_tokens INTEGER, cache_creation_tokens INTEGER,
                   duration_ms INTEGER, ttft_ms INTEGER, error_type TEXT,
                   UNIQUE(agent, session_id, ts, model)
                 );
                 PRAGMA user_version = 3;
                 INSERT INTO app_settings(key, value) VALUES('usage_rebuild_pending', '0');",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        // 0004 应已重建为新结构：带新列的行可写入，重建标志重新置位
        let mut row = sample_usage(1_000);
        row.source_id = Some("probe".into());
        assert_eq!(store.insert_usage(&[row]), 1, "被骗库经 0004 后应支持新结构写入");
        assert_eq!(
            store.get_setting(crate::store::REBUILD_PENDING_KEY).as_deref(),
            Some("1"),
            "重建标志应重新置位以触发全量回溯"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_usage_idempotent() {
        let path = tmp_db("idem");
        let store = Store::open(&path).unwrap();
        let rows = vec![sample_usage(1_000), sample_usage(2_000)];
        assert_eq!(store.insert_usage(&rows), 2);
        // 同一批再插：全部命中幂等键，0 行新增
        assert_eq!(store.insert_usage(&rows), 0);
        // 交错重复：仅新行入库
        let mut again = rows.clone();
        again.push(sample_usage(3_000));
        assert_eq!(store.insert_usage(&again), 1);
        let _ = std::fs::remove_file(&path);
    }

    /// 同幂等键多快照：仅四项合计更大的行才覆盖（与 CC 流式去重口径一致）；
    /// 更小快照重复采集不回退
    #[test]
    fn test_usage_upsert_keeps_max_snapshot() {
        let path = tmp_db("upsert");
        let store = Store::open(&path).unwrap();
        // 首插：流式中途的小快照
        assert_eq!(store.insert_usage(&[sample_usage(1_000)]), 1);
        // 同键更大快照（流式写全）：覆盖
        let mut bigger = sample_usage(1_000);
        bigger.input_tokens = Some(5_000);
        assert_eq!(store.insert_usage(&[bigger]), 1);
        // 库中为覆盖后的值（session_usage_total = input+output）
        assert_eq!(store.session_usage_total("zcode:abc"), 5_200);
        // 更小快照重复采到：不覆盖、不变更
        assert_eq!(store.insert_usage(&[sample_usage(1_000)]), 0);
        assert_eq!(store.session_usage_total("zcode:abc"), 5_200);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_watermark_monotonic() {
        let path = tmp_db("wm");
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get_watermark("zcode"), 0);
        store.set_watermark("zcode", 500);
        store.set_watermark("zcode", 300); // 回退值不生效
        assert_eq!(store.get_watermark("zcode"), 500);
        let _ = std::fs::remove_file(&path);
    }

    /// 2026-09-21 双计根治回归：
    /// ① 同 source_id（同消息）timestamp 各异的流式复制行 → 合并为一行、保留最大快照
    ///   （旧幂等键 (sid,ts,model) 不含消息身份，跨 tick 分裂时双计 +27.8%）
    /// ② 后台校准行无条件覆盖（差值随 assistant 增长缩小，须跟随重算值而非保留最大）
    /// ③ 调用次数口径：today_usage / 会话页 calls 排除后台行，token 计入
    #[test]
    fn test_source_key_merges_and_background_calls() {
        let path = tmp_db("srckey");
        let store = Store::open(&path).unwrap();
        // 同消息（source_id 相同）两行快照，timestamp 不同（流式复制行实测形态）
        let mut small = sample_usage(1_000);
        small.source_id = Some("msg_a".into());
        small.input_tokens = Some(40);
        let mut big = sample_usage(5_000);
        big.source_id = Some("msg_a".into());
        big.input_tokens = Some(100);
        assert_eq!(store.insert_usage(&[small, big.clone()]), 2);
        // 同批次重复重采（水位余量回退场景）：不再新增
        assert_eq!(store.insert_usage(&[big.clone()]), 0);
        // 库中该消息只有一行（旧键实现下会是两行）
        let n: i64 = {
            let mut conn = store.lock_conn();
            // 测试内直达连接仅此一处用途：精确断言行数
            conn.query_row(
                "SELECT COUNT(*) FROM usage_records WHERE source_id = 'msg_a'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(n, 1, "同 source_id 必须合并为一行");

        // 后台校准行：差值行先大后小，两次都覆盖（跟随重算值）；
        // 字段形态与 service 层构造一致：只有 input_tokens 承载差值，其余为 None
        let mut bg1 = sample_usage(6_000);
        bg1.source_id = Some("cost:zcode:abc:glm-5.3".into());
        bg1.is_background = true;
        bg1.input_tokens = Some(500);
        bg1.output_tokens = None;
        bg1.reasoning_tokens = None;
        bg1.cache_read_tokens = None;
        bg1.cache_creation_tokens = None;
        bg1.duration_ms = None;
        bg1.ttft_ms = None;
        assert_eq!(store.insert_usage(&[bg1]), 1);
        let mut bg2 = sample_usage(7_000);
        bg2.source_id = Some("cost:zcode:abc:glm-5.3".into());
        bg2.is_background = true;
        bg2.input_tokens = Some(300); // 差值缩小：普通行的"保留最大"不适用
        bg2.output_tokens = None;
        bg2.reasoning_tokens = None;
        bg2.cache_read_tokens = None;
        bg2.cache_creation_tokens = None;
        bg2.duration_ms = None;
        bg2.ttft_ms = None;
        assert_eq!(store.insert_usage(&[bg2]), 1);
        let bg_val: i64 = {
            let mut conn = store.lock_conn();
            conn.query_row(
                "SELECT input_tokens FROM usage_records WHERE source_id = 'cost:zcode:abc:glm-5.3'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(bg_val, 300, "后台行必须无条件覆盖为最新差值");

        // 调用次数排除后台行；token 计入（100+200+3000+0 普通 + 300 后台）
        let (today_tokens, today_calls) = store.today_usage(0);
        assert_eq!(today_calls, 1, "调用次数不含后台校准行");
        assert_eq!(today_tokens, 3_300 + 300, "token 总量含后台校准行");
        // 会话页 calls 同口径（后台行计入 token 但不计次数）
        let page = store
            .session_page("all", None, None, None, "all", "", "recent", 20, 0)
            .unwrap();
        let row = page.rows.iter().find(|r| r.session_id == "zcode:abc").unwrap();
        assert_eq!(row.calls, 1);
        assert_eq!(row.total_tokens, 3_600);
        let _ = std::fs::remove_file(&path);
    }    #[test]
    fn test_cleanup() {
        let path = tmp_db("clean");
        let store = Store::open(&path).unwrap();
        store.insert_usage(&[sample_usage(1_000), sample_usage(9_000)]);
        store.insert_quota(&QuotaRow {
            provider: "glm".into(),
            window_kind: "5h".into(),
            used_percent: Some(35.0),
            used_tokens: None,
            reset_at: None,
            fetched_at: 800,
        });
        let removed = store.cleanup_older_than(5_000);
        assert_eq!(removed, 2); // 1 条 usage + 1 条快照
        let _ = std::fs::remove_file(&path);
    }

    /// 报表快照/分页（M1-R1）：守恒、维度分组、维度过滤、今日档、非法档拒绝。
    /// 时间用 now 附近样本（today 档必然覆盖；样本时间早于今日零点用 now-3 秒，
    /// 测试进程毫秒级完成，不会跨过午夜边界）
    #[test]
    fn test_report_snapshot() {
        let path = tmp_db("report2");
        let store = Store::open(&path).unwrap();
        let row_sum = 4_200i64; // 样本四项之和（1000+200+3000+0）
        let now = now_ms();

        // 样本工厂：两 Agent × 两项目 × 两模型，zcode 带时长、CC 无时长、1 条限流错误
        let mk = |agent: &str, sid: &str, model: &str, proj: &str, ts: i64, dur: Option<i64>, err: Option<&str>| {
            let full_sid = format!("{agent}:{sid}");
            store.upsert_session(&full_sid, agent, Some("glm"), Some(model), Some(proj), Some("标题"), ts, "idle", None);
            UsageRow {
                session_id: full_sid,
                agent: agent.into(),
                model: model.into(),
                provider: Some("glm".into()),
                ts,
                input_tokens: Some(1000),
                output_tokens: Some(200),
                reasoning_tokens: Some(50),
                cache_read_tokens: Some(3000),
                cache_creation_tokens: Some(0),
                duration_ms: dur,
                ttft_ms: Some(900),
                error_type: err.map(String::from),
                source_id: Some(format!("{agent}:{sid}:{ts}")),
                is_background: false,
            }
        };
        let rows = vec![
            mk("zcode", "s1", "GLM-5.3", r"F:\projA", now - 1_000, Some(5_000), None),
            mk("zcode", "s2", "glm-5.3-flash", r"F:\projB", now - 2_000, None, Some("rate_limited")),
            mk("claude-code", "s1", "claude-sonnet", r"F:\projA", now - 3_000, None, None),
        ];
        store.insert_usage(&rows);

        // 全量快照（all 档，按月分桶）
        let snap = store.report_snapshot("all", None, None, None).unwrap();
        assert_eq!(snap.summary.total_tokens, row_sum * 3);
        assert_eq!(snap.summary.calls, 3);
        assert_eq!(snap.summary.sessions, 3);
        assert_eq!(snap.summary.projects, 2);
        assert_eq!(snap.summary.errors, 1);

        // Agent 维度：zcode 2 行在前（降序）；模型名大小写归一无影响（此处 zcode 两条模型不同）
        assert_eq!(snap.by_agent.len(), 2);
        assert_eq!(snap.by_agent[0].label, "zcode");
        assert_eq!(snap.by_agent[0].total, row_sum * 2);
        assert_eq!(snap.by_agent[0].calls, 2);

        // 项目维度：projA 2 行在前
        assert_eq!(snap.by_project.len(), 2);
        assert_eq!(snap.by_project[0].label, r"F:\projA");
        assert_eq!(snap.by_project[0].total, row_sum * 2);

        // 模型维度：三条各一组（glm-5.3 大写样本应归一为小写标签）
        assert_eq!(snap.by_model.len(), 3);
        assert!(snap.by_model.iter().all(|m| m.label == m.label.to_lowercase()));

        // 供应商维度：全部 glm 单组
        assert_eq!(snap.by_provider.len(), 1);
        assert_eq!(snap.by_provider[0].total, row_sum * 3);

        // 热力图：token 与次数双守恒，坐标合法
        assert_eq!(snap.heatmap.iter().map(|c| c.total).sum::<i64>(), row_sum * 3);
        assert_eq!(snap.heatmap.iter().map(|c| c.calls).sum::<i64>(), 3);
        assert!(snap.heatmap.iter().all(|c| (0..=6).contains(&c.weekday) && (0..=23).contains(&c.hour)));

        // 趋势（all→月桶）：四项与次数守恒
        let t_sum: (i64, i64) = snap
            .trend
            .iter()
            .map(|t| (t.input + t.output + t.cache_read + t.cache_creation, t.calls))
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        assert_eq!(t_sum, (row_sum * 3, 3));

        // 下拉选项：只受范围影响
        assert_eq!(snap.options.agents.len(), 2);
        assert_eq!(snap.options.projects.len(), 2);
        assert_eq!(snap.options.models.len(), 3);

        // 维度过滤：agent=zcode → 只剩 2 行
        let z = store.report_snapshot("all", Some("zcode"), None, None).unwrap();
        assert_eq!(z.summary.calls, 2);
        assert_eq!(z.summary.total_tokens, row_sum * 2);
        // 项目过滤：projA → zcode s1 + CC s1
        let pa = store.report_snapshot("all", None, Some(r"F:\projA"), None).unwrap();
        assert_eq!(pa.summary.calls, 2);
        // 模型过滤（大小写不敏感）
        let m = store.report_snapshot("all", None, None, Some("GLM-5.3")).unwrap();
        assert_eq!(m.summary.calls, 1);

        // 今日档：now 样本必然落入（本机时区零点 <= now）
        let today = store.report_snapshot("today", None, None, None).unwrap();
        assert_eq!(today.summary.calls, 3);
        // 近 7 天档同量；非法档拒绝
        assert_eq!(store.report_snapshot("7d", None, None, None).unwrap().summary.calls, 3);
        assert!(store.report_snapshot("xyz", None, None, None).is_none());

        // ===== M1-10 会话中心：追加一个 4 小时前的已结束样本（插在报表断言之后，
        // 不影响上方 report_snapshot 对 3 行样本的守恒断言）=====
        let ended_rows = vec![mk(
            "claude-code",
            "s2",
            "claude-sonnet",
            r"F:\projC",
            now - 4 * 3_600_000,
            None,
            None,
        )];
        store.insert_usage(&ended_rows);

        // 会话中心分页：默认最近活动降序、total/页大小、时长与状态列口径（CC 无时长）
        let page1 = store
            .session_page("all", None, None, None, "all", "", "recent", SESSION_PAGE_SIZE, 0)
            .unwrap();
        assert_eq!(page1.total, 4);
        assert_eq!(page1.page_size, SESSION_PAGE_SIZE);
        assert_eq!(page1.rows.len(), 4);
        assert!(page1.rows.windows(2).all(|w| w[0].last_ts >= w[1].last_ts), "最近活动降序");
        let cc_row = page1.rows.iter().find(|r| r.agent == "claude-code" && r.session_id.ends_with("s1")).unwrap();
        assert_eq!(cc_row.duration_ms, None, "CC 转录无时长字段应为 None");
        assert_eq!(cc_row.state.as_deref(), Some("idle"), "sessions 行状态随行下发");
        let zc_row = page1.rows.iter().find(|r| r.session_id == "zcode:s1").unwrap();
        assert_eq!(zc_row.duration_ms, Some(5_000));
        assert_eq!(zc_row.reasoning_tokens, 50, "思考 token 随聚合下发");
        assert_eq!(zc_row.ttft_avg_ms, Some(900), "平均首字随聚合下发");
        // 四项拆解守恒：总量 = 四项之和（样本 1000+200+3000+0）
        assert_eq!(zc_row.input_tokens, 1_000);
        assert_eq!(zc_row.output_tokens, 200);
        assert_eq!(zc_row.cache_read_tokens, 3_000);
        assert_eq!(zc_row.cache_creation_tokens, 0);
        assert_eq!(
            zc_row.total_tokens,
            zc_row.input_tokens + zc_row.output_tokens + zc_row.cache_read_tokens + zc_row.cache_creation_tokens
        );
        // 错误统计：zcode:s2 有限流错误，其余为 0
        let err_row = page1.rows.iter().find(|r| r.session_id == "zcode:s2").unwrap();
        assert_eq!(err_row.errors, 1);
        assert_eq!(err_row.error_types.as_deref(), Some("rate_limited"));
        assert_eq!(zc_row.errors, 0);
        assert_eq!(zc_row.error_types, None);
        // 每页行数：自定义生效、0/负值回退默认、超上限钳 200
        let p2rows = store
            .session_page("all", None, None, None, "all", "", "recent", 2, 0)
            .unwrap();
        assert_eq!(p2rows.rows.len(), 2);
        assert_eq!(p2rows.page_size, 2);
        let p0 = store
            .session_page("all", None, None, None, "all", "", "recent", 0, 0)
            .unwrap();
        assert_eq!(p0.page_size, SESSION_PAGE_SIZE);
        let p999 = store
            .session_page("all", None, None, None, "all", "", "recent", 999, 0)
            .unwrap();
        assert_eq!(p999.page_size, 200);
        // 排序切换：按 token 降序（白名单第二键）
        let by_tokens = store
            .session_page("all", None, None, None, "all", "", "tokens", SESSION_PAGE_SIZE, 0)
            .unwrap();
        assert!(by_tokens.rows.windows(2).all(|w| w[0].total_tokens >= w[1].total_tokens));
        // 关键字：命中项目全路径；无匹配归零；LIKE 通配符按字面量转义
        let kw = store
            .session_page("all", None, None, None, "all", "projA", "recent", SESSION_PAGE_SIZE, 0)
            .unwrap();
        assert_eq!(kw.total, 2, "projA 两会话命中");
        assert_eq!(
            store.session_page("all", None, None, None, "all", "无此项目", "recent", SESSION_PAGE_SIZE, 0)
                .unwrap()
                .total,
            0
        );
        assert_eq!(
            store.session_page("all", None, None, None, "all", "proj_", "recent", SESSION_PAGE_SIZE, 0)
                .unwrap()
                .total,
            0,
            "_ 已转义为字面量，不当代通配符"
        );
        // 状态档：三个近期 idle 样本=active，4 小时前样本=ended；errored 只剩限流会话
        let active = store
            .session_page("all", None, None, None, "active", "", "recent", SESSION_PAGE_SIZE, 0)
            .unwrap();
        assert_eq!(active.total, 3, "idle 未超 2h 视为进行中");
        let ended = store
            .session_page("all", None, None, None, "ended", "", "recent", SESSION_PAGE_SIZE, 0)
            .unwrap();
        assert_eq!(ended.total, 1);
        assert_eq!(ended.rows[0].session_id, "claude-code:s2");
        let errored = store
            .session_page("all", None, None, None, "errored", "", "recent", SESSION_PAGE_SIZE, 0)
            .unwrap();
        assert_eq!(errored.total, 1);
        assert_eq!(errored.rows[0].session_id, "zcode:s2");
        // 白名单拒绝：非法状态档/排序键
        assert!(store.session_page("all", None, None, None, "xyz", "", "recent", SESSION_PAGE_SIZE, 0).is_none());
        assert!(store.session_page("all", None, None, None, "all", "", "xyz", SESSION_PAGE_SIZE, 0).is_none());
        // 翻页：offset 越界返回空页但 total 不变
        let page2 = store
            .session_page("all", None, None, None, "all", "", "recent", SESSION_PAGE_SIZE, SESSION_PAGE_SIZE)
            .unwrap();
        assert_eq!(page2.total, 4);
        assert!(page2.rows.is_empty());
        // 过滤联动：agent=zcode → 会话表只剩 2 个
        let zpage = store
            .session_page("all", Some("zcode"), None, None, "all", "", "recent", SESSION_PAGE_SIZE, 0)
            .unwrap();
        assert_eq!(zpage.total, 2);

        // 会话详情（M1-11）：流水倒序 + 状态事件双口径（命名空间 id 与原始 id 均命中）
        let detail = store.session_detail("zcode:s1");
        assert_eq!(detail.calls.len(), 1);
        assert_eq!(detail.calls[0].input_tokens, 1_000);
        assert_eq!(detail.calls[0].duration_ms, Some(5_000));
        // 真实总数与列表长度一致（未截断时相等；截断口径由 LIMIT 上限保证，此处验证 COUNT 正确性）
        assert_eq!(detail.calls_total, 1);
        assert_eq!(detail.events_total, 0);
        store.insert_status_event("claude-code", Some("s1"), "SessionStart", "{}", now);
        store.insert_status_event("claude-code", Some("claude-code:s1"), "UserPromptSubmit", "{}", now + 1);
        // zcode:s1 只能靠原始 id 命中 raw 事件；claude-code:s1 双口径（命名空间 id + 原始 id）各命中
        assert_eq!(store.session_detail("zcode:s1").events.len(), 1, "原始 id 口径命中");
        let cc_detail = store.session_detail("claude-code:s1");
        assert_eq!(
            cc_detail.events.len(),
            2,
            "原始 id 与命名空间 id 均可命中"
        );
        assert_eq!(cc_detail.events_total, 2, "状态事件总数同样兼容双口径");

        // R2 汇总扩展：时长合计/平均首字/思考占比原料（样本 ttft 全 900、reasoning 全 50）
        assert_eq!(snap.summary.duration_ms, Some(5_000));
        assert_eq!(snap.summary.ttft_avg_ms, Some(900));
        assert_eq!(snap.summary.reasoning_tokens, 150);
        assert_eq!(snap.summary.billable_tokens, 1_200 * 3);
        // R2 趋势行时长：仅 zcode s1 贡献 5000
        let trend_dur: i64 = snap.trend.iter().filter_map(|t| t.duration_ms).sum();
        assert_eq!(trend_dur, 5_000);
        // R2 错误分布：仅 1 条 rate_limited
        assert_eq!(snap.by_error.len(), 1);
        assert_eq!(snap.by_error[0].label, "rate_limited");
        assert_eq!(snap.by_error[0].calls, 1);

        // M1-10 CSV 导出：BOM 表头 + 4 行会话（最近活动降序），状态/过滤与页面同口径
        let csv = store
            .build_sessions_csv("all", None, None, None, "all", "", "recent")
            .unwrap();
        assert!(csv.starts_with('\u{feff}'));
        assert_eq!(csv.lines().count(), 5, "表头 + 4 行");
        assert!(csv.contains("zcode:s1"));
        assert!(csv.contains("claude-code:s2"));
        let zc_csv = store
            .build_sessions_csv("all", Some("zcode"), None, None, "all", "", "recent")
            .unwrap();
        assert_eq!(zc_csv.lines().count(), 3, "过滤后 2 行会话");
        assert!(!zc_csv.contains("claude-code"));
        assert!(store.build_sessions_csv("xyz", None, None, None, "all", "", "recent").is_none());
        assert!(store.build_sessions_csv("all", None, None, None, "xyz", "", "recent").is_none());
        let ended_csv = store
            .build_sessions_csv("all", None, None, None, "ended", "", "recent")
            .unwrap();
        assert_eq!(ended_csv.lines().count(), 2, "状态档过滤生效");
        let _ = std::fs::remove_file(&path);
    }

    /// CSV 字段转义（RFC 4180）：逗号/引号/换行触发包裹，引号双写
    #[test]
    fn test_csv_cell() {
        assert_eq!(csv_cell("普通"), "普通");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("说\"引号"), "\"说\"\"引号\"");
        assert_eq!(csv_cell("换\n行"), "\"换\n行\"");
    }
}
