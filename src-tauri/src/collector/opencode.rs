//! OpenCode 同族适配器（M2-10a）：OpenCode + MiMo Code 共用一份实现，
//! 差异仅 FamilyProfile 三五处参数（agent id/数据根/db 文件名/重定向变量/进程关键词）。
//! 勘察（2026-09-23 源码级核实：anomalyco/opencode v1.18.32 + XiaomiMiMo/MiMo-Code
//! v0.1.15，Schema 字段级+drizzle 表列级）见 docs/01-RESEARCH.md §12：
//!   ① 主存储为 SQLite v2（drizzle）：缺省 %USERPROFILE%\.local\share\{opencode,mimocode}\
//!     {opencode,mimocode}.db——xdg-basedir 包对 Windows 无特判，只认 XDG_DATA_HOME；
//!     MiMo 另支持 MIMOCODE_HOME 整根重定向（须绝对路径，取其 data 子目录）；
//!   ② session 表：id/title/directory/agent + time_created/time_updated（Unix 毫秒）；
//!     OpenCode 另有 tokens/cost 会话级聚合列（聚合缓存非真源），MiMo 无（fork 基线早）
//!     ——用量统一逐行取 message，两家族口径一致；
//!   ③ message 表 data JSON 列，assistant 行 tokens{input,output,reasoning,
//!     cache.read,cache.write}+cost：cache 两项是独立分项（Anthropic 语义，非 OpenAI
//!     子集），四项互斥直取无需拆分；model 字段两代形态（OpenCode 对象 $.model.id、
//!     MiMo 平铺 $.modelID）解析函数双兼容；
//!   ④ 幂等键 msg_{id} 天然唯一（oc:/mc: 前缀分家）；水位走 time_updated 而非
//!     time_created——流式步骤的 tokens 在 step.ended 才齐备，同一行会原地更新，
//!     按 updated 过滤可让未完成行在补全后重新进入增量窗口，自库幂等键去重；
//!   ⑤ 错误信号：assistant 行 data.error 非空 → UsageRow.error_type，走现有
//!     recent_error 链路喂状态机 error（无需 last_failure 信号位，那是 hooks 专属）；
//!   ⑥ 快轮信号：-wal 文件 mtime（WAL 模式写入先落 -wal，主 db 文件 mtime 不动）；
//!   ⑦ SSE（opencode serve /event，session.next.step.ended 带 usage）为 opt-in
//!     实时增强档，M2-10b 装机核实后实施（端口发现/事件流实测为前提，总纲 §2.3.4）。

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use super::engine::{open_sqlite_readonly, HotSignal, ProcessMatch, ScanBudget};
use super::{AgentAdapter, CollectOutput, SessionInfo, provider_from_model};
use crate::store::UsageRow;

/// SQLite 档扫描节流预算（毫秒，04-EXPANSION §2.3.2）：窗口内重复扫描返回上轮
/// 缓存——scan 与 collect 各持一份预算，避免相互吃掉配额（与 zcode 同款）
const SCAN_THROTTLE_MS: u64 = 2_000;

/// 同族档案：OpenCode 家族两家的全部差异点收敛在这一个结构里（04-EXPANSION
/// §1.3 结论 4「同族参数化」的落地——新增同内核 Agent = 加一份常量，零代码复制）
#[derive(Clone, Copy)]
pub(crate) struct FamilyProfile {
    /// Agent 标识（自库 agent 维度：'opencode' | 'mimo-code'）
    agent_id: &'static str,
    /// 幂等键前缀（分家命名空间）：oc: | mc:
    source_prefix: &'static str,
    /// xdg data 根下的目录名
    app_dir: &'static str,
    /// 数据库文件名
    db_file: &'static str,
    /// 整根重定向环境变量（MiMo: MIMOCODE_HOME → 其 data 子目录；
    /// OpenCode 无官方 HOME 变量，恒 None）
    home_env: Option<&'static str>,
    /// 进程名关键词（M2-4 声明化）
    process_keywords: &'static [&'static str],
}

/// OpenCode（anomalyco/opencode）
const PROFILE_OPENCODE: FamilyProfile = FamilyProfile {
    agent_id: "opencode",
    source_prefix: "oc",
    app_dir: "opencode",
    db_file: "opencode.db",
    home_env: None,
    process_keywords: &["opencode"],
};

/// MiMo Code（XiaomiMiMo/MiMo-Code，OpenCode fork）
const PROFILE_MIMO: FamilyProfile = FamilyProfile {
    agent_id: "mimo-code",
    source_prefix: "mc",
    app_dir: "mimocode",
    db_file: "mimocode.db",
    home_env: Some("MIMOCODE_HOME"),
    process_keywords: &["mimo"],
};

/// xdg data 根解析（xdg-basedir 包同规则，2026-09-23 源码核实无 Windows 特判）：
/// XDG_DATA_HOME（非空才生效，JS 侧空串为 falsy）> USERPROFILE\.local\share
fn xdg_data_root(xdg_env: Option<OsString>, userprofile: Option<OsString>) -> Option<PathBuf> {
    if let Some(v) = xdg_env.filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(v));
    }
    let home = userprofile?;
    Some(PathBuf::from(home).join(".local").join("share"))
}

/// 数据库路径解析：重定向环境变量（须绝对路径且 data 子目录已存在，异常回落默认
/// 并 debug 留痕，04-EXPANSION §2.8 规则）> xdg 默认根。
/// 环境变量值作为参数传入（生产包装 current_db_path；单测免 set_var 全局竞态）
fn resolve_db_path(
    profile: &FamilyProfile,
    home_env_value: Option<OsString>,
    xdg_env: Option<OsString>,
    userprofile: Option<OsString>,
) -> Option<PathBuf> {
    if let (Some(name), Some(v)) = (profile.home_env, home_env_value) {
        let p = PathBuf::from(&v);
        let data = p.join("data");
        if p.is_absolute() && data.is_dir() {
            return Some(data.join(profile.db_file));
        }
        log::debug!(
            "[{}] {} 指向的目录无效（须绝对路径且 data 子目录存在），回落默认路径：{}",
            profile.agent_id,
            name,
            p.display()
        );
    }
    Some(xdg_data_root(xdg_env, userprofile)?.join(profile.app_dir).join(profile.db_file))
}

/// 生产档路径解析（读真实环境变量）
fn current_db_path(profile: &FamilyProfile) -> Option<PathBuf> {
    let home_env_value = profile.home_env.and_then(std::env::var_os);
    resolve_db_path(
        profile,
        home_env_value,
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("USERPROFILE"),
    )
}

pub struct OpenCodeFamilyAdapter {
    profile: FamilyProfile,
    db_path: PathBuf,
    /// scan 节流预算（M2-3）：窗口内返回上轮缓存
    scan_budget: Mutex<ScanBudget>,
    /// collect 节流预算（M2-3）：同上，独立于 scan 防止相互吃配额
    collect_budget: Mutex<ScanBudget>,
    /// 上轮 scan 结果缓存（首扫前为空 → 返回空列表）
    scan_cache: Mutex<Option<Vec<SessionInfo>>>,
    /// 上轮 collect 结果缓存：以旧水位算出的行是超集，自库幂等键保证重复入库零副作用
    collect_cache: Mutex<Option<CollectOutput>>,
}

impl OpenCodeFamilyAdapter {
    /// OpenCode 生产档
    pub fn opencode() -> Self {
        Self::with_profile(PROFILE_OPENCODE)
    }

    /// MiMo Code 生产档
    pub fn mimo_code() -> Self {
        Self::with_profile(PROFILE_MIMO)
    }

    /// 按档案构造（生产档：启用 2s 节流）
    fn with_profile(profile: FamilyProfile) -> Self {
        Self::with_db(profile, current_db_path(&profile).unwrap_or_default(), SCAN_THROTTLE_MS)
    }

    /// 指定档案与库路径构造（单测注入用：throttle_ms=0 即每轮都实扫）
    pub(crate) fn with_db(profile: FamilyProfile, db_path: PathBuf, throttle_ms: u64) -> Self {
        Self {
            profile,
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

    /// 只读打开（WAL 并发读安全，ZCode 同款已验证；Agent 运行与否均可读）
    fn open(&self) -> anyhow::Result<rusqlite::Connection> {
        if !self.db_path.exists() {
            anyhow::bail!("{} 数据库不存在：{}", self.profile.agent_id, self.db_path.display());
        }
        open_sqlite_readonly(&self.db_path)
    }
}

impl AgentAdapter for OpenCodeFamilyAdapter {
    fn id(&self) -> &'static str {
        self.profile.agent_id
    }

    /// 快轮信号（M2-1）：-wal 文件从无到有/mtime 变化即唤醒全量 tick。
    /// WAL 模式下提交先追加到 -wal（可能带 checkpoint 刷回主库），主 db 文件
    /// mtime 可能长期不动——探测 -wal 才是真实写入信号
    fn hot_signals(&self) -> Vec<HotSignal> {
        let profile = self.profile;
        vec![HotSignal::File(std::sync::Arc::new(move || {
            let db = current_db_path(&profile)?;
            let wal = PathBuf::from(format!("{}-wal", db.display()));
            wal.exists().then_some(wal)
        }))]
    }

    /// 进程匹配（M2-4 声明化）：opencode.exe / mimo（npm shim）/ mimocode.exe
    /// 均按关键词命中（exe 名装机核实，01-RESEARCH §12.3）
    fn process_match(&self) -> Option<ProcessMatch> {
        Some(ProcessMatch {
            name_keywords: self.profile.process_keywords,
            cmd_keywords: &[],
            cmd_excludes: &[],
        })
    }

    /// 扫描最近 90 天有活动、且有过 assistant 消息的会话（纯观测会话无意义）。
    /// M2-3 节流：预算窗口内返回上轮缓存。model 不在此取（OpenCode 有 session.model
    /// 列而 MiMo 无列）——统一由用量流水最近一次回填（latest_models 现成机制）
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        if !self.lock_scan_budget().ready() {
            return Ok(self.lock_scan_cache().clone().unwrap_or_default());
        }
        let conn = match self.open() {
            Ok(c) => c,
            Err(e) => {
                // 库不存在 = 该 Agent 未装（预期降级，静默）；其余打开失败 debug 留痕
                if self.db_path.exists() {
                    log::debug!("[{}] 库打开失败（本轮按空处理）：{e:#}", self.profile.agent_id);
                }
                return Ok(vec![]);
            }
        };
        let cutoff = now_ms() - 90 * 24 * 3600 * 1000;
        let mut stmt = conn.prepare(
            "SELECT s.id, s.directory, s.title, s.time_created, s.time_updated,
                    (SELECT MAX(m.time_created) FROM message m WHERE m.session_id = s.id)
             FROM session s
             WHERE s.time_updated > ?1
               AND EXISTS (SELECT 1 FROM message m
                            WHERE m.session_id = s.id
                              AND json_extract(m.data, '$.role') = 'assistant')
             ORDER BY s.time_updated DESC
             LIMIT 100",
        )?;
        let agent_id = self.profile.agent_id;
        let rows = stmt.query_map([cutoff], |r| {
            let sid: String = r.get(0)?;
            Ok(SessionInfo {
                id: format!("{agent_id}:{sid}"),
                agent: agent_id.into(),
                // provider/model 由用量流水回填（service 层 latest_models 现成机制）
                provider: None,
                model: None,
                project_dir: r.get::<_, Option<String>>(1)?,
                title: r.get::<_, Option<String>>(2)?,
                first_seen_at: r.get::<_, i64>(3)?,
                last_seen_at: r.get::<_, i64>(4)?,
                // 任意消息（含 user prompt）都算活动：比只看 assistant 更灵敏，
                // 与 CC/ZCode 的 last_usage 语义（最近信号时间）一致
                last_usage_at: r.get::<_, Option<i64>>(5)?,
            })
        })?;
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
            log::debug!("[{}] 扫描 {err_rows} 行解析失败已跳过（Schema 漂移？）", self.profile.agent_id);
        }
        *self.lock_scan_cache() = Some(out.clone());
        Ok(out)
    }

    /// 水位增量读取 message 表 assistant 行（列白名单 + 宽松 JSON，容忍 Schema 漂移）。
    /// 水位按 time_updated（见模块注释④）：step.ended 时 tokens 才齐备，流式行原地
    /// 更新后可重采，自库 source_id 幂等去重；source_id = "{oc|mc}:msg_{id}"。
    /// M2-3 节流：预算窗口内返回上轮缓存
    fn collect_usage(&self, watermark_ts: i64) -> anyhow::Result<CollectOutput> {
        if !self.lock_collect_budget().ready() {
            return Ok(self.lock_collect_cache().clone().unwrap_or_default());
        }
        let conn = match self.open() {
            Ok(c) => c,
            Err(e) => {
                if self.db_path.exists() {
                    log::debug!("[{}] 库打开失败（本轮按空处理）：{e:#}", self.profile.agent_id);
                }
                return Ok(CollectOutput::default());
            }
        };
        let mut stmt = conn.prepare(
            "SELECT m.id, m.session_id, m.time_created, m.data
             FROM message m
             WHERE m.time_updated > ?1
               AND json_extract(m.data, '$.role') = 'assistant'
             ORDER BY m.time_updated ASC
             LIMIT 5000",
        )?;
        let rows = stmt.query_map([watermark_ts], |r| {
            let id: String = r.get(0)?;
            let session_id: String = r.get(1)?;
            let ts: i64 = r.get(2)?;
            let data: String = r.get(3)?;
            Ok((id, session_id, ts, data))
        })?;
        let mut err_rows = 0usize;
        let mut out = vec![];
        for x in rows {
            let (id, session_id, ts, data) = match x {
                Ok(v) => v,
                Err(_) => {
                    err_rows += 1;
                    continue;
                }
            };
            match parse_message_row(&self.profile, &id, &session_id, ts, &data) {
                Some(row) => out.push(row),
                None => err_rows += 1,
            }
        }
        if err_rows > 0 {
            log::debug!("[{}] 采集 {err_rows} 行解析失败已跳过（Schema 漂移？）", self.profile.agent_id);
        }
        let result = CollectOutput { rows: out, cost_snapshots: vec![], titles: vec![] };
        *self.lock_collect_cache() = Some(result.clone());
        Ok(result)
    }
}

/// 从 data JSON 提取一条 UsageRow（宽松解析：字段缺失按 0/None，容忍格式漂移；
/// 返回 None = 行整体不可解析，计入失败计数留痕）
fn parse_message_row(
    profile: &FamilyProfile,
    id: &str,
    session_id: &str,
    ts: i64,
    data: &str,
) -> Option<UsageRow> {
    let v: serde_json::Value = serde_json::from_str(data).ok()?;
    // tokens 五项：effect Schema 的 Finite 允许浮点、zod 为 int——统一先取整再兜浮点
    let num = |pointer: &str| -> i64 {
        match v.pointer(pointer) {
            Some(x) if x.is_number() => x
                .as_i64()
                .or_else(|| x.as_f64().map(|f| f.round() as i64))
                .unwrap_or(0),
            _ => 0,
        }
    };
    let input = num("/tokens/input");
    let output = num("/tokens/output");
    let reasoning = num("/tokens/reasoning");
    let cache_read = num("/tokens/cache/read");
    let cache_creation = num("/tokens/cache/write");
    let has_tokens = input != 0 || output != 0 || reasoning != 0 || cache_read != 0 || cache_creation != 0;
    // 错误信息：error.message 优先，回退 error.type（再无则整体序列化）；超长截断
    let error_raw: Option<String> = v
        .pointer("/error/message")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .or_else(|| {
            // error 非空但无 message：type/整体也算错误痕迹
            let e = v.pointer("/error").filter(|e| !e.is_null())?;
            Some(
                e.get("type")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| e.to_string()),
            )
        });
    let error_type = error_raw.map(|m| {
        if m.len() > 200 {
            // 回退到合法 UTF-8 边界再切（200 落在多字节字符内时直切会 panic）
            let mut end = 200;
            while !m.is_char_boundary(end) {
                end -= 1;
            }
            m[..end].to_string()
        } else {
            m
        }
    });
    // 无 tokens 且无错误 = 未完成的流式中间行：采之（ts/会话维度有值），
    // tokens 后补时行 time_updated 推进会再次进入增量窗口，自库按 source_id 幂等
    if !has_tokens && error_type.is_none() {
        return None;
    }
    // model 两代形态双兼容：OpenCode 对象 $.model.id / MiMo 平铺 $.modelID
    let model = v
        .pointer("/model/id")
        .and_then(|x| x.as_str())
        .or_else(|| v.pointer("/modelID").and_then(|x| x.as_str()))
        .unwrap_or("")
        .to_string();
    // provider：源库 providerID 权威直用（可读供应商名），缺失回退模型名推断
    let provider = v
        .pointer("/model/providerID")
        .and_then(|x| x.as_str())
        .or_else(|| v.pointer("/providerID").and_then(|x| x.as_str()))
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .or_else(|| provider_from_model(&model));
    // 本步耗时：time.completed - time.created（毫秒数；effect Schema 编码侧即毫秒，
    // 实测若出现旧 ISO 字符串编码再补解析，装机核实点 01-RESEARCH §12.3）
    let t_of = |key: &str| -> Option<i64> {
        match v.pointer(key)? {
            serde_json::Value::Number(n) => n.as_i64(),
            _ => None,
        }
    };
    let duration_ms = match (t_of("/time/created"), t_of("/time/completed")) {
        (Some(a), Some(b)) if b >= a => Some(b - a),
        _ => None,
    };
    Some(UsageRow {
        session_id: format!("{}:{}", profile.agent_id, session_id),
        agent: profile.agent_id.into(),
        model,
        provider,
        ts,
        input_tokens: Some(input),
        output_tokens: Some(output),
        reasoning_tokens: Some(reasoning),
        cache_read_tokens: Some(cache_read),
        cache_creation_tokens: Some(cache_creation),
        duration_ms,
        ttft_ms: None,
        error_type,
        source_id: Some(format!("{}:msg_{id}", profile.source_prefix)),
        is_background: false,
    })
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
    use std::path::Path;

    /// 临时库建表灌合成样本（列集与真实 schema 核心列对齐，非 assistant 行混入
    /// 验证过滤；两代 model 形态各行覆盖）。时间戳相对当前时间生成——scan 有
    /// 90 天 cutoff，写死的历史时间会被过滤导致空结果
    fn seed_db(db: &Path) -> i64 {
        let base = now_ms() - 3_600_000; // 一小时前，各事件依次后移
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY, directory TEXT, title TEXT, agent TEXT,
                time_created INTEGER, time_updated INTEGER);
             CREATE TABLE message (
                id TEXT PRIMARY KEY, session_id TEXT, data TEXT,
                time_created INTEGER, time_updated INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('sess_a', 'F:/demo', '修复登录', 'build', ?1, ?2)",
            [base, base + 5000],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('sess_b', 'F:/other', '重构采集', 'main', ?1, ?2)",
            [base + 10000, base + 9000],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('sess_c', 'F:/empty', '空会话', 'main', ?1, ?2)",
            [base + 20000, base + 21000],
        )
        .unwrap();
        // a1：tokens 全量 + duration（毫秒时间）
        let a1 = format!(
            r#"{{"role":"assistant","model":{{"providerID":"zhipu","id":"glm-5.3"}},
                "tokens":{{"input":100,"output":50,"reasoning":10,"cache":{{"read":20,"write":30}}}},
                "time":{{"created":{},"completed":{}}}}}"#,
            base + 1000,
            base + 3000
        );
        conn.execute(
            "INSERT INTO message VALUES ('msg_0001', 'sess_a', ?1, ?2, ?3)",
            rusqlite::params![a1, base + 1000, base + 3000],
        )
        .unwrap();
        // a2：user 行（应被 SQL 过滤）
        conn.execute(
            "INSERT INTO message VALUES ('msg_0002', 'sess_a', '{\"role\":\"user\",\"text\":\"你好\"}', ?1, ?1)",
            [base + 5000],
        )
        .unwrap();
        // a3：error 行（OpenCode 对象 model，error.message）
        let a3 = format!(
            r#"{{"role":"assistant","model":{{"providerID":"zhipu","id":"glm-5.3"}},
                "tokens":{{"input":10,"output":0,"reasoning":0,"cache":{{"read":0,"write":0}}}},
                "error":{{"type":"APIError","message":"boom 限流"}},
                "time":{{"created":{}}}}}"#,
            base + 6000
        );
        conn.execute(
            "INSERT INTO message VALUES ('msg_0003', 'sess_a', ?1, ?2, ?3)",
            rusqlite::params![a3, base + 6000, base + 7000],
        )
        .unwrap();
        // b1：MiMo 平铺形态 + 浮点 tokens（Finite 兼容）
        let b1 = format!(
            r#"{{"role":"assistant","modelID":"mimo-v2.5","providerID":"xiaomi",
                "tokens":{{"input":7.6,"output":3.2,"reasoning":0,"cache":{{"read":0,"write":0}}}},
                "time":{{"created":{},"completed":{}}}}}"#,
            base + 10000,
            base + 11000
        );
        conn.execute(
            "INSERT INTO message VALUES ('msg_0004', 'sess_b', ?1, ?2, ?3)",
            rusqlite::params![b1, base + 10000, base + 11000],
        )
        .unwrap();
        // c1：空会话的 user 行
        conn.execute(
            "INSERT INTO message VALUES ('msg_0005', 'sess_c', '{\"role\":\"user\",\"text\":\"hi\"}', ?1, ?1)",
            [base + 21000],
        )
        .unwrap();
        base
    }

    fn opencode_at(db: &Path) -> OpenCodeFamilyAdapter {
        OpenCodeFamilyAdapter::with_db(PROFILE_OPENCODE, db.to_path_buf(), 0)
    }

    /// scan：只出有 assistant 行的会话；collect：assistant 行提取正确
    #[test]
    fn test_scan_and_collect() {
        let dir = std::env::temp_dir().join(format!("at-oc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let _ = std::fs::remove_file(&db);
        let base = seed_db(&db);
        let ad = opencode_at(&db);

        let sessions = ad.scan_sessions().unwrap();
        assert_eq!(sessions.len(), 2, "只有有 assistant 行的会话出现");
        assert_eq!(sessions[0].id, "opencode:sess_b", "按 time_updated 降序");
        assert_eq!(sessions[1].title.as_deref(), Some("修复登录"));
        assert_eq!(sessions[1].project_dir.as_deref(), Some("F:/demo"));
        assert_eq!(sessions[1].last_usage_at, Some(base + 6000), "MAX(message.time_created)");

        let usage = ad.collect_usage(0).unwrap().rows;
        assert_eq!(usage.len(), 3, "三条 assistant 行（user 被过滤）");
        // a1：全量 tokens + duration + providerID 直用
        let a1 = usage.iter().find(|u| u.source_id == Some("oc:msg_msg_0001".into())).unwrap();
        assert_eq!((a1.input_tokens, a1.output_tokens, a1.reasoning_tokens), (Some(100), Some(50), Some(10)));
        assert_eq!((a1.cache_read_tokens, a1.cache_creation_tokens), (Some(20), Some(30)));
        assert_eq!(a1.model, "glm-5.3");
        assert_eq!(a1.provider.as_deref(), Some("zhipu"));
        assert_eq!(a1.duration_ms, Some(2000));
        assert_eq!(a1.ts, base + 1000);
        assert_eq!(a1.session_id, "opencode:sess_a");
        // a3：error.message 进 error_type
        let a3 = usage.iter().find(|u| u.ts == base + 6000).unwrap();
        assert_eq!(a3.error_type.as_deref(), Some("boom 限流"));
        // b1：MiMo 平铺形态 + 浮点 round
        let b1 = usage.iter().find(|u| u.session_id == "opencode:sess_b").unwrap();
        assert_eq!(b1.model, "mimo-v2.5");
        assert_eq!(b1.provider.as_deref(), Some("xiaomi"));
        assert_eq!((b1.input_tokens, b1.output_tokens), (Some(8), Some(3)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 水位按 time_updated：已完成行不再重采；原地更新（流式补全）后可重采新值
    #[test]
    fn test_watermark_incremental() {
        let dir = std::env::temp_dir().join(format!("at-oc-wm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let _ = std::fs::remove_file(&db);
        let base = seed_db(&db);
        let ad = opencode_at(&db);

        assert!(!ad.collect_usage(0).unwrap().rows.is_empty());
        // 全部 time_updated 之后采集：无新行
        assert!(ad.collect_usage(base + 50000).unwrap().rows.is_empty());

        // 模拟流式补全：msg_0001 的 tokens 被原地更新（time_updated 推进）
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE message SET data = json_set(data, '$.tokens.output', 99),
                    time_updated = ?1 WHERE id = 'msg_0001'",
            [base + 60000],
        )
        .unwrap();
        let rows = ad.collect_usage(base + 50000).unwrap().rows;
        assert_eq!(rows.len(), 1, "只有被更新的行重入窗口");
        assert_eq!(rows[0].output_tokens, Some(99));
        assert_eq!(rows[0].source_id, Some("oc:msg_msg_0001".into()), "同 id 幂等键");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// MiMo 档案走同一实现：agent id/幂等前缀/进程关键词正确
    #[test]
    fn test_mimo_family_profile() {
        let dir = std::env::temp_dir().join(format!("at-oc-mc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("mimocode.db");
        let _ = std::fs::remove_file(&db);
        seed_db(&db);
        let ad = OpenCodeFamilyAdapter::with_db(PROFILE_MIMO, db.clone(), 0);
        assert_eq!(ad.id(), "mimo-code");
        let usage = ad.collect_usage(0).unwrap().rows;
        assert!(usage.iter().all(|u| u.source_id.as_deref().unwrap_or("").starts_with("mc:")));
        let pm = ad.process_match().unwrap();
        assert_eq!(pm.name_keywords, &["mimo"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 幂等入自库：同批数据插两遍，第二遍 0 行
    #[test]
    fn test_into_store_idempotent() {
        let dir = std::env::temp_dir().join(format!("at-oc-idem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let _ = std::fs::remove_file(&db);
        seed_db(&db);
        let ad = opencode_at(&db);
        let usage = ad.collect_usage(0).unwrap().rows;
        let mut store_db = dir.join("store.db");
        let _ = std::fs::remove_file(&store_db);
        let store = crate::store::Store::open(&store_db).unwrap();
        let n1 = store.insert_usage(&usage);
        assert_eq!(n1, usage.len());
        let n2 = store.insert_usage(&usage);
        assert_eq!(n2, 0, "重复插入应全部被幂等键忽略");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
        std::mem::take(&mut store_db);
    }

    /// 路径解析：重定向变量无效回落默认；xdg 环境变量与 USERPROFILE 两分支
    #[test]
    fn test_resolve_db_path() {
        // MIMOCODE_HOME 绝对路径但 data 子目录不存在 → 回落
        let fake_home = std::env::temp_dir().join(format!("at-oc-fakehome-{}", std::process::id()));
        let got = resolve_db_path(&PROFILE_MIMO, Some(fake_home.clone().into_os_string()), None, Some("C:\\u".into()));
        assert_eq!(got, Some(PathBuf::from("C:\\u").join(".local\\share").join("mimocode").join("mimocode.db")));
        // 建出 data 子目录后生效
        std::fs::create_dir_all(fake_home.join("data")).unwrap();
        let got = resolve_db_path(&PROFILE_MIMO, Some(fake_home.clone().into_os_string()), None, Some("C:\\u".into()));
        assert_eq!(got, Some(fake_home.join("data").join("mimocode.db")));
        // 非绝对路径拒绝 → 回落
        let got = resolve_db_path(&PROFILE_MIMO, Some("rel/path".into()), None, Some("C:\\u".into()));
        assert_eq!(got, Some(PathBuf::from("C:\\u").join(".local\\share").join("mimocode").join("mimocode.db")));
        // OpenCode 无 HOME 变量：XDG_DATA_HOME 优先
        let got = resolve_db_path(&PROFILE_OPENCODE, None, Some("D:\\xdgdata".into()), Some("C:\\u".into()));
        assert_eq!(got, Some(PathBuf::from("D:\\xdgdata").join("opencode").join("opencode.db")));
        // 空串 XDG_DATA_HOME 视同未设（xdg-basedir 的 JS falsy 语义）
        let got = resolve_db_path(&PROFILE_OPENCODE, None, Some("".into()), Some("C:\\u".into()));
        assert_eq!(got, Some(PathBuf::from("C:\\u").join(".local\\share").join("opencode").join("opencode.db")));
        // USERPROFILE 也缺 → None（适配器构造回落空路径，open 时静默降级）
        let got = resolve_db_path(&PROFILE_OPENCODE, None, None, None);
        assert_eq!(got, None);
        let _ = std::fs::remove_dir_all(&fake_home);
    }

    /// 未完成流式中间行（无 tokens 无 error）不采（等 time_updated 推进后重采）；
    /// 非毫秒时间编码（异常行）不算 duration 不误报
    #[test]
    fn test_partial_row_and_bad_time() {
        let profile = PROFILE_OPENCODE;
        // 未完成行：全 0 tokens 无 error → None
        assert!(parse_message_row(&profile, "msg_x", "sess", 1, r#"{"role":"assistant"}"#).is_none());
        // 字符串时间（本适配器不支持的编码）→ duration 为 None，不误算
        let row = parse_message_row(
            &profile,
            "msg_y",
            "sess",
            1,
            r#"{"role":"assistant","modelID":"gpt-5",
                "tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}},
                "time":{"created":"2026-09-23T10:00:00Z","completed":"2026-09-23T10:00:05Z"}}"#,
        )
        .unwrap();
        assert_eq!(row.duration_ms, None);
        assert_eq!(row.provider.as_deref(), Some("openai"), "无 providerID 时模型名推断");
        // 超长 error 截断到 200 且不切断多字节字符
        let long = "错".repeat(300);
        let row = parse_message_row(
            &profile,
            "msg_z",
            "sess",
            1,
            &format!(r#"{{"role":"assistant","error":{{"message":"{long}"}}}}"#),
        )
        .unwrap();
        let msg = row.error_type.unwrap();
        assert!(msg.len() <= 200 * 4 && msg.chars().count() <= 300, "截断后仍为合法字符串");
    }

    /// 集成测试：连接本机真实 OpenCode 库（未装跳过）
    /// 手动运行：cargo test -- --ignored
    #[test]
    #[ignore]
    fn test_real_opencode_collect() {
        let ad = OpenCodeFamilyAdapter::with_db(
            PROFILE_OPENCODE,
            current_db_path(&PROFILE_OPENCODE).unwrap(),
            0,
        );
        let sessions = ad.scan_sessions().unwrap();
        assert!(!sessions.is_empty(), "本机应有 OpenCode 会话");
        let usage = ad.collect_usage(0).unwrap().rows;
        assert!(!usage.is_empty(), "本机应有历史用量");
        for u in usage.iter().take(5) {
            assert!(u.ts > 1_700_000_000_000, "时间戳应为毫秒：{}", u.ts);
            assert_eq!(u.agent, "opencode");
        }
        let max_updated = 9_999_999_999_999i64;
        assert!(ad.collect_usage(max_updated).unwrap().rows.is_empty(), "水位增量应为空");
    }

    /// 集成测试：连接本机真实 MiMo Code 库（未装跳过）
    #[test]
    #[ignore]
    fn test_real_mimo_collect() {
        let ad = OpenCodeFamilyAdapter::with_db(
            PROFILE_MIMO,
            current_db_path(&PROFILE_MIMO).unwrap(),
            0,
        );
        let sessions = ad.scan_sessions().unwrap();
        assert!(!sessions.is_empty(), "本机应有 MiMo Code 会话");
        let usage = ad.collect_usage(0).unwrap().rows;
        assert!(!usage.is_empty(), "本机应有历史用量");
        for u in usage.iter().take(5) {
            assert_eq!(u.agent, "mimo-code");
            assert!(u.source_id.as_deref().unwrap_or("").starts_with("mc:"));
        }
    }
}
