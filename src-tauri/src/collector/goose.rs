//! Goose 适配器（M2-16）：旁路读取 Goose（aaif-goose/goose，Block 出品）的
//! 会话库 sessions.db。勘察（2026-10-09 源码级核实：main 分支 session_manager.rs
//! ＋config/paths.rs＋桌面端类型定义，网络调研无装机，实机项列回补清单）：
//!   ① 存储代际：旧版为 sessions/*.jsonl（legacy.rs 兼容层残留），现行版本已迁
//!     **SQLite**（SESSIONS_FOLDER="sessions"＋DB_NAME="sessions.db"，schema v17，
//!     sqlx 连接池）——本适配器只读新库，旧 JSONL 不采（历史迁移产物，装机后
//!     再议是否回补）；
//!   ② 库路径：`Paths::data_dir()/sessions/sessions.db`（session_manager.rs:63
//!     SESSION_STORAGE LazyLock 生产单例）。data_dir 解析（config/paths.rs）：
//!     `GOOSE_PATH_ROOT` env（须绝对路径）→ `<root>/data`；否则 etcetera
//!     choose_app_strategy（top_level_domain="Block"，app_name="goose"）：
//!     Windows=`%APPDATA%\Block\goose\data`（Roaming），macOS=
//!     `~/Library/Application Support/Block/goose/data`，Linux=`$XDG_DATA_HOME/
//!     goose`（缺省 `~/.local/share/goose`——XDG 策略 data_dir 无 data 子目录，
//!     与 Windows/macOS 的 `/data` 后缀差异是 etcetera 三平台语义，适配器分平台
//!     组装）；
//!   ③ 三表数据面（v17 建表语句逐列核实）：sessions（会话面：id/name/
//!     session_type/working_dir/provider_name/model_config_json/updated_at…）、
//!     messages（消息面：role/content_json/created_timestamp/tokens）、
//!     **usage_ledger（逐调用流水：session_id/created_timestamp/model/五项
//!     token/cost/cost_source/is_compaction）**——goose 是库内自带流水表的
//!     理想数据源，采集走 ledger 增量，无需累计快照重采；
//!   ④ 时间编码：ledger/messages 的 created_timestamp 为 INTEGER，goose 内部以
//!     MILLISECOND_TIMESTAMP_THRESHOLD=10^10 判别秒/毫秒双编码——读取同款防御
//!     （<10^10 视为秒 ×1000）；sessions.created_at/updated_at 是 TIMESTAMP
//!     字符串（sqlx 编码，格式漂移风险），**时间轴一律走 created_timestamp
//!     整数列，不碰字符串列**；
//!   ⑤ 模型/供应商：sessions.provider_name 与 model_config_json($.model_name)
//!     会话级直取（比流水回填更准）；ledger.model 逐行权威；
//!   ⑥ 已知口径差：ledger.cost 列在库但 UsageRow 无 cost 列（成本核算走
//!     CC cost-state 快照机制，goose 的 accumulated_cost 不分模型不适配快照
//!     口径）——成本采集列装机后增强档；goose 无显式错误列（assistant 错误
//!     不落 messages），error_type 恒 None，出错状态由进程/水位语义兜底；
//!     is_compaction=1 为上下文压缩后台调用，is_background=true 标记
//!     （token 计入消耗、不冒充用户回合）；
//!   ⑦ 快轮信号：sessions.db 与 -wal 双 File 信号（Hermes 同款——库未启 WAL
//!     时 -wal 恒不存在，信号无害）；无 hooks 注入（goose hooks/recipes 为
//!     自有扩展体系，SQLite 通道已覆盖，不适用）；进程关键词 "goose"。

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use super::engine::{open_sqlite_readonly, now_ms, HotSignal, ProcessMatch, RANGE_CUTOFF_MS, ScanBudget};
use super::{AgentAdapter, CollectOutput, SessionInfo};
use crate::store::UsageRow;

/// SQLite 档扫描节流预算（毫秒）：scan/collect 各持一份，与 opencode 同款
const SCAN_THROTTLE_MS: u64 = 2_000;

/// 秒/毫秒双编码判别阈值（goose session_manager.rs 同款常量语义）：
/// created_timestamp < 10^10 视为秒，×1000 归一到毫秒
const MILLISECOND_TIMESTAMP_THRESHOLD: i64 = 10_000_000_000;

pub struct GooseAdapter {
    db_path: PathBuf,
    /// scan 节流预算：窗口内返回上轮缓存
    scan_budget: Mutex<ScanBudget>,
    /// collect 节流预算：同上，独立于 scan 防止相互吃配额
    collect_budget: Mutex<ScanBudget>,
    /// 上轮 scan 结果缓存（首扫前为空 → 返回空列表）
    scan_cache: Mutex<Option<Vec<SessionInfo>>>,
    /// 上轮 collect 结果缓存：以旧水位算出的行是超集，自库幂等键保证重复入库零副作用
    collect_cache: Mutex<Option<CollectOutput>>,
}

impl GooseAdapter {
    /// 生产档：解析真实环境变量下的库路径，启用 2s 节流
    pub fn new() -> Self {
        Self::with_db(current_db_path().unwrap_or_default(), SCAN_THROTTLE_MS)
    }

    /// 指定库路径构造（单测注入用：throttle_ms=0 即每轮都实扫）
    pub(crate) fn with_db(db_path: PathBuf, throttle_ms: u64) -> Self {
        Self {
            db_path,
            scan_budget: Mutex::new(ScanBudget::new(throttle_ms)),
            collect_budget: Mutex::new(ScanBudget::new(throttle_ms)),
            scan_cache: Mutex::new(None),
            collect_cache: Mutex::new(None),
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

    /// 只读打开（SQLite 并发读安全；Agent 运行与否均可读）
    fn open(&self) -> anyhow::Result<rusqlite::Connection> {
        if !self.db_path.exists() {
            anyhow::bail!("Goose 会话库不存在：{}", self.db_path.display());
        }
        open_sqlite_readonly(&self.db_path)
    }
}

impl Default for GooseAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// created_timestamp 秒/毫秒归一（goose 同款阈值判别，模块注释④）
fn normalize_ms(ts: i64) -> i64 {
    if ts > 0 && ts < MILLISECOND_TIMESTAMP_THRESHOLD {
        ts * 1000
    } else {
        ts
    }
}

/// 目标平台（etcetera 三平台 data_dir 语义差异，模块注释②）：
/// 参数化以便单测覆盖全部分支（生产按编译期平台传入）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostPlatform {
    Windows,
    Macos,
    Unix,
}

#[cfg(test)]
impl HostPlatform {
    fn all() -> [HostPlatform; 3] {
        [HostPlatform::Windows, HostPlatform::Macos, HostPlatform::Unix]
    }
}

/// 库路径解析（模块注释②）：
/// GOOSE_PATH_ROOT（须绝对路径）> 平台默认根（Windows/macOS 带 /data 后缀，Unix XDG 无）
fn resolve_db_path(
    platform: HostPlatform,
    path_root: Option<OsString>,
    appdata: Option<OsString>,
    home: Option<OsString>,
    xdg_data_home: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(v) = path_root {
        let p = PathBuf::from(&v);
        if p.is_absolute() {
            return Some(p.join("data").join("sessions").join("sessions.db"));
        }
        log::debug!("[goose] GOOSE_PATH_ROOT 非绝对路径被忽略：{}", p.display());
    }
    match platform {
        HostPlatform::Windows => {
            // Windows：etcetera RoamingAppData\Block\goose\data
            let appdata = appdata?;
            Some(PathBuf::from(appdata).join("Block").join("goose").join("data").join("sessions").join("sessions.db"))
        }
        HostPlatform::Macos => {
            // macOS：~/Library/Application Support/Block/goose/data
            let home = home?;
            Some(PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("Block")
                .join("goose")
                .join("data")
                .join("sessions")
                .join("sessions.db"))
        }
        HostPlatform::Unix => {
            // Linux 等：XDG_DATA_HOME（非空才生效）> ~/.local/share，下接 goose/sessions
            let base = match xdg_data_home.filter(|s| !s.is_empty()) {
                Some(v) => PathBuf::from(v),
                None => PathBuf::from(home?).join(".local").join("share"),
            };
            Some(base.join("goose").join("sessions").join("sessions.db"))
        }
    }
}

/// 生产档路径解析（读真实环境变量，平台按编译期目标）
fn current_db_path() -> Option<PathBuf> {
    let platform = if cfg!(target_os = "windows") {
        HostPlatform::Windows
    } else if cfg!(target_os = "macos") {
        HostPlatform::Macos
    } else {
        HostPlatform::Unix
    };
    resolve_db_path(
        platform,
        std::env::var_os("GOOSE_PATH_ROOT"),
        std::env::var_os("APPDATA"),
        std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")),
        std::env::var_os("XDG_DATA_HOME"),
    )
}

impl AgentAdapter for GooseAdapter {
    fn id(&self) -> &'static str {
        "goose"
    }

    /// 快轮信号：sessions.db 与 -wal 双 File 信号（Hermes 同款，模块注释⑦）
    fn hot_signals(&self) -> Vec<HotSignal> {
        let db = self.db_path.clone();
        let wal = PathBuf::from(format!("{}-wal", db.display()));
        vec![
            HotSignal::File(std::sync::Arc::new(move || db.exists().then_some(db.clone()))),
            HotSignal::File(std::sync::Arc::new(move || wal.exists().then_some(wal.clone()))),
        ]
    }

    /// 进程匹配：goose CLI（goose/goose.exe；桌面版进程名装机回补核实）
    fn process_match(&self) -> Option<ProcessMatch> {
        Some(ProcessMatch {
            name_keywords: &["goose"],
            cmd_keywords: &[],
            cmd_excludes: &[],
        })
    }

    /// 扫描最近 90 天有 assistant 消息的会话。时间轴走 messages.created_timestamp
    /// 整数列（模块注释④，不碰 sessions.updated_at 字符串列）；model/provider
    /// 会话级直取（模块注释⑤），model 缺失时由用量流水回填机制兜底
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        if !self.lock_scan_budget().ready() {
            return Ok(self.lock_scan_cache().clone().unwrap_or_default());
        }
        let conn = match self.open() {
            Ok(c) => c,
            Err(e) => {
                // 库不存在 = 该 Agent 未装（预期降级，静默）；其余打开失败 debug 留痕
                if self.db_path.exists() {
                    log::debug!("[goose] 库打开失败（本轮按空处理）：{e:#}");
                }
                return Ok(vec![]);
            }
        };
        let cutoff = now_ms() - RANGE_CUTOFF_MS;
        let mut stmt = conn.prepare(
            "SELECT s.id, s.working_dir, s.name, s.provider_name,
                    json_extract(s.model_config_json, '$.model_name'),
                    MAX(m.created_timestamp)
             FROM sessions s
             JOIN messages m ON m.session_id = s.id AND m.role = 'assistant'
             GROUP BY s.id
             HAVING MAX(m.created_timestamp) > ?1
             ORDER BY 6 DESC
             LIMIT 100",
        )?;
        let rows = stmt.query_map([cutoff], |r| {
            let sid: String = r.get(0)?;
            let last_raw: i64 = r.get(5)?;
            Ok(SessionInfo {
                id: format!("goose:{sid}"),
                agent: "goose".into(),
                provider: r.get::<_, Option<String>>(3)?,
                model: r.get::<_, Option<String>>(4)?,
                project_dir: r.get::<_, Option<String>>(1)?,
                title: r.get::<_, Option<String>>(2)?,
                // first_seen 退化口径：源库 created_at 为字符串列，不解析（模块注释④），
                // 以最后活动时间代替——岛端首见展示容忍该近似
                first_seen_at: normalize_ms(last_raw),
                last_seen_at: normalize_ms(last_raw),
                last_usage_at: Some(normalize_ms(last_raw)),
            })
        })?;
        let mut err_rows = 0usize;
        let out = rows
            .filter_map(|x| match x {
                Ok(v) => Some(v),
                Err(_) => {
                    err_rows += 1;
                    None
                }
            })
            .collect::<Vec<_>>();
        if err_rows > 0 {
            log::debug!("[goose] 扫描 {err_rows} 行解析失败已跳过（Schema 漂移？）");
        }
        *self.lock_scan_cache() = Some(out.clone());
        Ok(out)
    }

    /// 水位增量读取 usage_ledger 逐调用流水（模块注释③）：created_timestamp 严格
    /// 大于水位；source_id = "gs:ledger_{id}"（同库行 id 唯一）；is_compaction=1
    /// 标记后台（模块注释⑥）
    fn collect_usage(&self, watermark_ts: i64) -> anyhow::Result<CollectOutput> {
        if !self.lock_collect_budget().ready() {
            return Ok(self.lock_collect_cache().clone().unwrap_or_default());
        }
        let conn = match self.open() {
            Ok(c) => c,
            Err(e) => {
                if self.db_path.exists() {
                    log::debug!("[goose] 库打开失败（本轮按空处理）：{e:#}");
                }
                return Ok(CollectOutput::default());
            }
        };
        let mut stmt = conn.prepare(
            "SELECT l.id, l.session_id, l.created_timestamp, l.model,
                    l.input_tokens, l.output_tokens,
                    l.cache_read_tokens, l.cache_write_tokens, l.is_compaction
             FROM usage_ledger l
             WHERE l.created_timestamp > ?1
             ORDER BY l.created_timestamp ASC
             LIMIT 5000",
        )?;
        let rows = stmt.query_map([watermark_ts], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, Option<i64>>(7)?,
                r.get::<_, Option<i64>>(8)?,
            ))
        })?;
        let mut err_rows = 0usize;
        let mut out = vec![];
        for x in rows {
            let (lid, sid, ts_raw, model, input, output, cache_read, cache_write, compaction) =
                match x {
                    Ok(v) => v,
                    Err(_) => {
                        err_rows += 1;
                        continue;
                    }
                };
            // 水位比较在毫秒域进行（SQL 过滤用的原始值可能为秒编码），
            // 秒编码行归一后 ≤ 水位的丢弃，防重复采集（自库幂等键兜底）
            let ts = normalize_ms(ts_raw);
            if ts <= watermark_ts {
                continue;
            }
            // provider 由 ledger 行直取不可得（无列），走模型名推断；
            // 会话级 provider_name 由 scan/回填链路呈现（先借用后 move）
            let provider = model.as_deref().and_then(super::provider_from_model);
            out.push(UsageRow {
                session_id: format!("goose:{sid}"),
                agent: "goose".into(),
                model: model.unwrap_or_default(),
                provider,
                ts,
                input_tokens: input,
                output_tokens: output,
                // goose ledger 无 reasoning 分项
                reasoning_tokens: None,
                cache_read_tokens: cache_read,
                cache_creation_tokens: cache_write,
                duration_ms: None,
                ttft_ms: None,
                // goose 无显式错误列（模块注释⑥）
                error_type: None,
                source_id: Some(format!("gs:ledger_{lid}")),
                is_background: compaction.unwrap_or(0) != 0,
            });
        }
        if err_rows > 0 {
            log::debug!("[goose] 采集 {err_rows} 行解析失败已跳过（Schema 漂移？）");
        }
        let result = CollectOutput { rows: out, cost_snapshots: vec![], titles: vec![] };
        *self.lock_collect_cache() = Some(result.clone());
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// 临时库建 v17 核心列表灌合成样本（时间戳相对当前时间生成——scan 有
    /// 90 天 cutoff，写死的历史时间会被过滤导致空结果）
    fn seed_db(db: &Path) -> i64 {
        let base = now_ms() - 3_600_000; // 一小时前，各事件依次后移
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, name TEXT NOT NULL DEFAULT '',
                session_type TEXT NOT NULL DEFAULT 'user',
                working_dir TEXT NOT NULL,
                created_at TIMESTAMP, updated_at TIMESTAMP,
                provider_name TEXT, model_config_json TEXT,
                accumulated_cost REAL, archived_at TIMESTAMP);
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL, role TEXT NOT NULL,
                created_timestamp INTEGER NOT NULL);
             CREATE TABLE usage_ledger (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL, created_timestamp INTEGER NOT NULL,
                model TEXT, input_tokens INTEGER, output_tokens INTEGER,
                total_tokens INTEGER, cache_read_tokens INTEGER,
                cache_write_tokens INTEGER, cost REAL, is_compaction INTEGER DEFAULT 0);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions VALUES ('sess_a', '修复登录', 'user', 'F:/demo', NULL, NULL, 'zhipu',
                '{\"model_name\":\"glm-5.3\",\"temperature\":0.7}', NULL, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions VALUES ('sess_b', '空会话', 'user', 'F:/other', NULL, NULL, NULL, NULL, NULL, NULL)",
            [],
        )
        .unwrap();
        // sess_a 的 assistant/user 消息（assistant 才算有效会话）
        conn.execute(
            "INSERT INTO messages (session_id, role, created_timestamp) VALUES ('sess_a', 'assistant', ?1)",
            [base + 1000],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, created_timestamp) VALUES ('sess_a', 'user', ?1)",
            [base + 800],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, created_timestamp) VALUES ('sess_b', 'user', ?1)",
            [base + 9000],
        )
        .unwrap();
        // ledger 三行：正常行 / 压缩后台行 / 秒编码行（阈值归一防御）
        conn.execute(
            "INSERT INTO usage_ledger (session_id, created_timestamp, model, input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens, is_compaction) VALUES ('sess_a', ?1, 'glm-5.3', 100, 50, 20, 30, 0)",
            [base + 1000],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage_ledger (session_id, created_timestamp, model, input_tokens, output_tokens, is_compaction)
                VALUES ('sess_a', ?1, 'glm-5.3', 5000, 800, 1)",
            [base + 2000],
        )
        .unwrap();
        // 秒编码行（base 约为 1.7~1.8×10^12 毫秒，÷1000 即秒编码形态）
        let sec_ts = (base + 3000) / 1000;
        conn.execute(
            "INSERT INTO usage_ledger (session_id, created_timestamp, model, input_tokens, output_tokens, is_compaction)
                VALUES ('sess_a', ?1, 'glm-5.3', 10, 5, 0)",
            [sec_ts],
        )
        .unwrap();
        base
    }

    fn goose_at(db: &Path) -> GooseAdapter {
        GooseAdapter::with_db(db.to_path_buf(), 0)
    }

    /// scan：只出有 assistant 行的会话；model/provider 会话级直取；秒编码归一
    #[test]
    fn test_scan_and_collect() {
        let dir = std::env::temp_dir().join(format!("at-goose-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("sessions.db");
        let _ = std::fs::remove_file(&db);
        let base = seed_db(&db);
        let ad = goose_at(&db);

        let sessions = ad.scan_sessions().unwrap();
        assert_eq!(sessions.len(), 1, "只有有 assistant 行的会话出现");
        let s = &sessions[0];
        assert_eq!(s.id, "goose:sess_a");
        assert_eq!(s.title.as_deref(), Some("修复登录"));
        assert_eq!(s.project_dir.as_deref(), Some("F:/demo"));
        assert_eq!(s.provider.as_deref(), Some("zhipu"), "sessions.provider_name 直取");
        assert_eq!(s.model.as_deref(), Some("glm-5.3"), "model_config_json 提取");
        assert_eq!(s.last_usage_at, Some(base + 1000), "MAX(messages.created_timestamp)");

        let usage = ad.collect_usage(0).unwrap().rows;
        assert_eq!(usage.len(), 3, "三条 ledger 行");
        let normal = usage.iter().find(|u| u.source_id == Some("gs:ledger_1".into())).unwrap();
        assert_eq!((normal.input_tokens, normal.output_tokens), (Some(100), Some(50)));
        assert_eq!((normal.cache_read_tokens, normal.cache_creation_tokens), (Some(20), Some(30)));
        assert_eq!(normal.model, "glm-5.3");
        // ledger 无 provider 列：走 modelcat 模型名推断（glm-5.3 → glm 家族）
        assert_eq!(normal.provider.as_deref(), Some("glm"));
        assert!(!normal.is_background);
        // 压缩行 → is_background
        let comp = usage.iter().find(|u| u.source_id == Some("gs:ledger_2".into())).unwrap();
        assert!(comp.is_background, "is_compaction=1 → 后台用量行");
        // 秒编码行归一（gs:ledger_3；整除截断的尾数随秒编码天然丢失）
        let sec = usage.iter().find(|u| u.source_id == Some("gs:ledger_3".into())).unwrap();
        assert_eq!(sec.ts, ((base + 3000) / 1000) * 1000, "秒编码 ×1000 归一");
        assert_eq!(sec.reasoning_tokens, None, "goose 无 reasoning 分项");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 水位按 created_timestamp 毫秒域增量：全部行之后采集无新行
    #[test]
    fn test_watermark_incremental() {
        let dir = std::env::temp_dir().join(format!("at-goose-wm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("sessions.db");
        let _ = std::fs::remove_file(&db);
        let base = seed_db(&db);
        let ad = goose_at(&db);

        assert_eq!(ad.collect_usage(0).unwrap().rows.len(), 3);
        assert!(ad.collect_usage(base + 60_000).unwrap().rows.is_empty(), "水位后无新行");
        // 新增一行 ledger 后重入窗口
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO usage_ledger (session_id, created_timestamp, model, input_tokens, output_tokens, is_compaction)
                VALUES ('sess_a', ?1, 'glm-5.3', 7, 3, 0)",
            [base + 60_000],
        )
        .unwrap();
        let rows = ad.collect_usage(base + 30_000).unwrap().rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source_id, Some("gs:ledger_4".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 路径解析：GOOSE_PATH_ROOT 绝对生效/相对忽略；三平台默认根组装正确
    /// （平台参数化，全分支可测）
    #[test]
    fn test_resolve_db_path() {
        // GOOSE_PATH_ROOT 绝对路径：三平台一致 → <root>/data/sessions/sessions.db
        for platform in HostPlatform::all() {
            let got = resolve_db_path(
                platform,
                Some("D:/goose-root".into()),
                Some("C:/appdata".into()),
                Some("C:/Users/u".into()),
                None,
            );
            assert_eq!(
                got,
                Some(PathBuf::from("D:/goose-root").join("data").join("sessions").join("sessions.db")),
                "{platform:?}: 绝对 GOOSE_PATH_ROOT 生效"
            );
        }
        // 相对路径忽略 → 回落各平台默认根
        let got = resolve_db_path(HostPlatform::Windows, Some("rel".into()), Some("C:/appdata".into()), None, None);
        assert_eq!(
            got,
            Some(PathBuf::from("C:/appdata").join("Block").join("goose").join("data").join("sessions").join("sessions.db"))
        );
        let got = resolve_db_path(HostPlatform::Macos, Some("rel".into()), None, Some("C:/Users/u".into()), None);
        assert_eq!(
            got,
            Some(PathBuf::from("C:/Users/u").join("Library").join("Application Support").join("Block").join("goose").join("data").join("sessions").join("sessions.db"))
        );
        // Unix 分支：XDG_DATA_HOME 优先于 HOME
        let got = resolve_db_path(HostPlatform::Unix, None, None, Some("C:/Users/u".into()), Some("D:/xdg".into()));
        assert_eq!(got, Some(PathBuf::from("D:/xdg").join("goose").join("sessions").join("sessions.db")));
        // Unix 分支：空串 XDG 视同未设（etcetera JS falsy 语义同款防御）
        let got = resolve_db_path(HostPlatform::Unix, None, None, Some("C:/Users/u".into()), Some("".into()));
        assert_eq!(
            got,
            Some(PathBuf::from("C:/Users/u").join(".local").join("share").join("goose").join("sessions").join("sessions.db"))
        );
        // Windows 分支缺 APPDATA / Unix 分支缺 HOME 且无 XDG → None（构造回落空路径，open 时静默降级）
        assert_eq!(resolve_db_path(HostPlatform::Windows, None, None, None, None), None);
        assert_eq!(resolve_db_path(HostPlatform::Unix, None, None, None, None), None);
    }

    /// 幂等入自库：普通行第二遍全部忽略；后台行（is_compaction）按存储层
    /// 「无条件覆盖」语义计入一次变更，但库内行数不增
    #[test]
    fn test_into_store_idempotent() {
        let dir = std::env::temp_dir().join(format!("at-goose-idem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("sessions.db");
        let _ = std::fs::remove_file(&db);
        seed_db(&db);
        let ad = goose_at(&db);
        let usage = ad.collect_usage(0).unwrap().rows;
        let mut store_db = dir.join("store.db");
        let _ = std::fs::remove_file(&store_db);
        let store = crate::store::Store::open(&store_db).unwrap();
        let n1 = store.insert_usage(&usage).unwrap();
        assert_eq!(n1, usage.len());
        let n2 = store.insert_usage(&usage).unwrap();
        assert_eq!(n2, 1, "仅后台行（is_compaction）按无条件覆盖语义计入一次变更，普通行幂等忽略");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
        std::mem::take(&mut store_db);
    }

    /// 集成测试：连接本机真实 Goose 库（未装跳过）
    /// 手动运行：cargo test -- --ignored
    #[test]
    #[ignore]
    fn test_real_goose_collect() {
        let ad = GooseAdapter::with_db(current_db_path().unwrap(), 0);
        let sessions = ad.scan_sessions().unwrap();
        assert!(!sessions.is_empty(), "本机应有 Goose 会话");
        let usage = ad.collect_usage(0).unwrap().rows;
        assert!(!usage.is_empty(), "本机应有历史用量");
        for u in usage.iter().take(5) {
            assert!(u.ts > 1_700_000_000_000, "时间戳应为毫秒：{}", u.ts);
            assert_eq!(u.agent, "goose");
            assert!(u.source_id.as_deref().unwrap_or("").starts_with("gs:"));
        }
        let max_updated = 9_999_999_999_999i64;
        assert!(ad.collect_usage(max_updated).unwrap().rows.is_empty(), "水位增量应为空");
    }
}
