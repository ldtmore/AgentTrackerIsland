//! OpenClaw 适配器（M2-13）：旁路读取各 agent 的 openclaw-agent.sqlite。
//! 勘察（2026-09-24 源码级核实：openclaw/openclaw main 分支）见 docs/01-RESEARCH.md §14：
//!   ① 状态根：$OPENCLAW_STATE_DIR 重定向（须已存在目录，异常回落默认并留痕）>
//!     ~/.openclaw（新版默认）与 ~/.openclaw-<profile>（命名 profile，`-<小写名>`
//!     后缀）> ~/.clawdbot（更名前旧目录，新版不存在时 OpenClaw 自身也回退读它）；
//!   ② 每 agent 一库：<root>/agents/<agentId>/agent/openclaw-agent.sqlite（多 agent
//!     多库须目录枚举；incognito-openclaw-agent.sqlite 是内存态哨兵保留名，不匹配
//!     文件名自然排除；自库会话 id 必须带 agentId 消歧——各库 session_key 同名）；
//!   ③ 会话三层：session_nodes（逻辑会话，session_key 跨代稳定，标题=
//!     display_name>label，updated_at/last_activity_at 毫秒）> session_windows
//!     （转录代：compact/reset 时轮换 session_id，model 列在窗口上）>
//!     transcript_events（事件流水，(session_id, seq) 主键，event_json 明文或
//!     event_zstd 压缩——≥1KB 且省≥10% 的事件转 zstd，须解压，event_utf8_bytes
//!     记录解压后字节数可校验）；
//!   ④ 用量取自 assistant 消息事件：type=="message" 且 message.role=="assistant"，
//!     usage 四桶 camelCase（input/output/cacheRead/cacheWrite，落盘前已按
//!     「input 不含缓存」归一），provider/model 直用，message.timestamp 毫秒
//!     （缺失回退列 created_at）；totalTokens 是上下文快照语义，不采；
//!   ⑤ 幂等键：assistantIdempotencyKey 存在时 ocm:{key}，否则
//!     oc:{agentId}:{session_key}:{seq}；回合完成后一次性落盘（无流式中间快照），
//!     无双计面；增量=窗口 updated_at>水位-60s 选窗 + 进程内 per-window seq 水位
//!     只解析新增，重启后全窗重放靠幂等键兜底；
//!   ⑥ 错误信号：stopReason=="error" → error_type（弱信号，精确错误载体装机核实）；
//!     stopReason=="aborted" 为用户主动打断不计错误（ZCode cancelled 同口径）；
//!     无 usage 且非 error 的回合（CLI 后端不回 usage）无统计价值，跳过；
//!   ⑦ 快轮信号：每状态根一个 DirScan（agents 目录限深 2 层枚举 *.sqlite 的
//!     mtime 集合），库 mtime 变化即唤醒全量 tick；
//!   ⑧ 无 hooks 体系（Gateway 有 HTTP 端点，总纲 §1.2），hooks 增强档不适用；
//!     message.cost 源库有值但自库无成本列，不入库（装机对账时核实口径）。

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use super::engine::{open_sqlite_readonly, HotSignal, ProcessMatch, ScanBudget};
use super::{AgentAdapter, CollectOutput, SessionInfo, provider_from_model};
use crate::store::UsageRow;

/// SQLite 档扫描节流预算（毫秒，M2-3，与 OpenCode 同款）
const SCAN_THROTTLE_MS: u64 = 2_000;
/// 会话扫描/采集范围（90 天，与其他适配器口径一致）
const RANGE_CUTOFF_MS: i64 = 90 * 24 * 3600 * 1000;
/// 单窗口单轮事件读取上限（防超大历史窗口拖垮采集；seq 水位保证下轮续读）
const WINDOW_EVENT_LIMIT: i64 = 5_000;
/// 每库每轮最多处理的活跃转录窗口数
const ACTIVE_WINDOW_LIMIT: i64 = 200;
/// 采集水位回看窗（毫秒）：窗口 updated_at 与事件 timestamp 存在时钟差，
/// 回看成本由 seq 水位消化（幂等 upsert 双保险），换增量不漏行
const WATERMARK_LOOKBACK_MS: i64 = 60_000;

pub struct OpenClawAdapter {
    /// 状态根集合（构造时解析一次；测试注入临时目录）
    roots: Vec<PathBuf>,
    /// scan 节流预算（M2-3）：窗口内返回上轮缓存
    scan_budget: Mutex<ScanBudget>,
    /// collect 节流预算（M2-3）：独立于 scan 防相互吃配额
    collect_budget: Mutex<ScanBudget>,
    /// 上轮 scan 结果缓存
    scan_cache: Mutex<Option<Vec<SessionInfo>>>,
    /// 上轮 collect 结果缓存：以旧水位算出的行是超集，自库幂等键保证重复入库零副作用
    collect_cache: Mutex<Option<CollectOutput>>,
    /// per-转录窗口 seq 高水位（进程内）：窗口内只解析新增 seq；
    /// 重启后为空即全窗重放，自库幂等键兜底
    seq_watermarks: Mutex<HashMap<String, i64>>,
}

impl OpenClawAdapter {
    /// 生产构造：解析真实环境（OPENCLAW_STATE_DIR/用户主目录）
    pub fn new() -> Self {
        Self::with_roots(current_state_roots(), SCAN_THROTTLE_MS)
    }

    /// 指定状态根构造（单测注入；throttle_ms=0 即每轮实扫）
    pub(crate) fn with_roots(roots: Vec<PathBuf>, throttle_ms: u64) -> Self {
        Self {
            roots,
            scan_budget: Mutex::new(ScanBudget::new(throttle_ms)),
            collect_budget: Mutex::new(ScanBudget::new(throttle_ms)),
            scan_cache: Mutex::new(None),
            collect_cache: Mutex::new(None),
            seq_watermarks: Mutex::new(HashMap::new()),
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

    fn lock_seq_watermarks(&self) -> MutexGuard<'_, HashMap<String, i64>> {
        self.seq_watermarks.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for OpenClawAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// 状态根候选解析（参数注入供单测免 set_var 全局竞态）。
/// 顺序即 OpenClaw 自身 resolveStateDir 语义：env 重定向最优先，旧目录兜底；
/// env 值目录不存在时忽略（留痕降级默认根，不因坏配置失明）。
fn resolve_state_roots(state_dir_env: Option<OsString>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut roots = vec![];
    if let Some(v) = state_dir_env.filter(|s| !s.is_empty()) {
        let p = PathBuf::from(&v);
        if p.is_dir() {
            roots.push(p);
        } else {
            log::debug!("[openclaw] OPENCLAW_STATE_DIR 指向的目录不存在，忽略：{}", p.display());
        }
    }
    let Some(home) = home else { return roots };
    // 新版默认根 + 命名 profile 根（~/.openclaw-<name>，01-RESEARCH §14.1）
    if let Ok(rd) = std::fs::read_dir(&home) {
        for e in rd.filter_map(|x| x.ok()) {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if e.path().is_dir() && (name == ".openclaw" || name.starts_with(".openclaw-")) {
                roots.push(e.path());
            }
        }
    }
    // 更名前旧目录（新版不存在时 OpenClaw 运行时也回退读这里，config/state-dir.ts）
    let legacy = home.join(".clawdbot");
    if legacy.is_dir() {
        roots.push(legacy);
    }
    roots
}

/// 生产档状态根解析（读真实环境变量）
fn current_state_roots() -> Vec<PathBuf> {
    resolve_state_roots(
        std::env::var_os("OPENCLAW_STATE_DIR"),
        std::env::var_os("USERPROFILE").map(PathBuf::from),
    )
}

/// 枚举状态根下的全部 agent 库：<root>/agents/<agentId>/agent/openclaw-agent.sqlite。
/// 返回（agentId，库路径）；incognito 哨兵不匹配文件名自然排除。
fn discover_dbs(roots: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut out = vec![];
    for root in roots {
        let agents_dir = root.join("agents");
        let Ok(rd) = std::fs::read_dir(&agents_dir) else { continue };
        for agent in rd.filter_map(|x| x.ok()) {
            if !agent.path().is_dir() {
                continue;
            }
            let agent_id = agent.file_name().to_string_lossy().to_string();
            let db = agent.path().join("agent").join("openclaw-agent.sqlite");
            if db.is_file() {
                out.push((agent_id, db));
            }
        }
    }
    out.sort();
    out
}

/// 只读打开一个 agent 库；不存在/打不开返回 None（静默降级，M1 起惯例）
fn open_db(db: &Path) -> Option<rusqlite::Connection> {
    if !db.is_file() {
        return None;
    }
    match open_sqlite_readonly(db) {
        Ok(c) => Some(c),
        Err(e) => {
            log::debug!("[openclaw] 库打开失败（本轮跳过）：{}：{e:#}", db.display());
            None
        }
    }
}

/// 解析一条转录事件为 UsageRow（宽松解析，容忍格式漂移）。
/// Ok(None) = 有意跳过（非 assistant 消息/无用量且无错误/aborted）；
/// Err = 行损坏（JSON 解析失败），计入失败计数留痕。
fn parse_event(
    agent_id: &str,
    session_key: &str,
    seq: i64,
    created_at: i64,
    event_json: &str,
) -> anyhow::Result<Option<UsageRow>> {
    let v: serde_json::Value = serde_json::from_str(event_json)
        .map_err(|e| anyhow::anyhow!("事件 JSON 解析失败 seq={seq}：{e}"))?;
    if v.get("type").and_then(|x| x.as_str()) != Some("message") {
        return Ok(None);
    }
    let Some(msg) = v.get("message").filter(|m| m.is_object()) else {
        return Ok(None);
    };
    if msg.get("role").and_then(|x| x.as_str()) != Some("assistant") {
        return Ok(None);
    }
    // usage 四桶（camelCase，落盘前已按「input 不含缓存」归一，直取即可）
    let num = |key: &str| -> i64 {
        msg.get("usage")
            .and_then(|u| u.get(key))
            .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f.round() as i64)))
            .unwrap_or(0)
    };
    let input = num("input");
    let output = num("output");
    let cache_read = num("cacheRead");
    let cache_write = num("cacheWrite");
    let has_usage = input != 0 || output != 0 || cache_read != 0 || cache_write != 0;
    let stop_reason = msg.get("stopReason").and_then(|x| x.as_str());
    // 用户主动打断：不计错误（对齐 ZCode cancelled），无用量即无统计价值
    if stop_reason == Some("aborted") {
        return Ok(None);
    }
    // 回合失败（常见于失败即中断无 usage）：只作为错误行入库
    let error_type = (stop_reason == Some("error")).then(|| "回合失败".to_string());
    if !has_usage && error_type.is_none() {
        return Ok(None);
    }
    let ts = msg
        .get("timestamp")
        .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f.round() as i64)))
        .filter(|t| *t > 0)
        .unwrap_or(created_at);
    let model = msg
        .get("model")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    // provider：源库 message.provider 权威直用，缺失回退模型名推断
    let provider = msg
        .get("provider")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| provider_from_model(&model));
    // 幂等键：官方键优先（ocm:），否则合成 oc:{agentId}:{session_key}:{seq}
    let source_id = msg
        .get("assistantIdempotencyKey")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|k| format!("ocm:{k}"))
        .unwrap_or_else(|| format!("oc:{agent_id}:{session_key}:{seq}"));
    Ok(Some(UsageRow {
        // 自库会话 id 带 agentId 消歧：各库 session_key 同名（如 main）
        session_id: format!("openclaw:{agent_id}:{session_key}"),
        agent: "openclaw".into(),
        model,
        provider,
        ts,
        input_tokens: Some(input),
        output_tokens: Some(output),
        reasoning_tokens: Some(0),
        cache_read_tokens: Some(cache_read),
        cache_creation_tokens: Some(cache_write),
        duration_ms: None,
        ttft_ms: None,
        error_type,
        source_id: Some(source_id),
        is_background: false,
    }))
}

/// 解压一条转录事件（≥1KB 的事件是 zstd 压缩体）；明文直用。
/// Ok(None) = 行既无明文也无压缩体（异常，跳过留痕）；
/// Err = 解压失败/长度校验不符/非 UTF-8（行损坏，不污染用量）。
fn decode_event(
    event_json: Option<String>,
    event_zstd: Option<Vec<u8>>,
    raw_bytes: Option<i64>,
) -> anyhow::Result<Option<String>> {
    if let Some(text) = event_json {
        return Ok(Some(text));
    }
    let Some(blob) = event_zstd else { return Ok(None) };
    let decoded = zstd::decode_all(blob.as_slice())
        .map_err(|e| anyhow::anyhow!("zstd 解压失败：{e}"))?;
    // OpenClaw 落库时记录了解压后字节数（event_utf8_bytes），一致才可信
    if let Some(expect) = raw_bytes.filter(|n| *n > 0) {
        if decoded.len() as i64 != expect {
            anyhow::bail!("zstd 解压长度不符：期望 {expect}，实际 {}", decoded.len());
        }
    }
    String::from_utf8(decoded)
        .map(Some)
        .map_err(|e| anyhow::anyhow!("解压后非合法 UTF-8：{e}"))
}

impl AgentAdapter for OpenClawAdapter {
    fn id(&self) -> &'static str {
        "openclaw"
    }

    /// 快轮信号（M2-1）：每状态根一个 DirScan——agents/<id>/agent/*.sqlite 的
    /// mtime 集合变化即唤醒（限深 2：agents/→<agentId>/→agent/→文件）。
    /// 根不存在时 sample 返回 None，不唤醒。
    fn hot_signals(&self) -> Vec<HotSignal> {
        self.roots
            .iter()
            .map(|root| HotSignal::DirScan {
                root: root.join("agents"),
                ext: Some(".sqlite"),
                depth: 2,
                max_files: 64,
            })
            .collect()
    }

    /// 进程匹配（M2-4 声明化）：openclaw gateway 常驻进程/node shim，
    /// 关键词装机核实（01-RESEARCH §14.3）
    fn process_match(&self) -> Option<ProcessMatch> {
        Some(ProcessMatch {
            name_keywords: &["openclaw"],
            cmd_keywords: &[],
            cmd_excludes: &[],
        })
    }

    /// 扫描最近 90 天有更新的逻辑会话（session_nodes；join 当前窗口取模型列）。
    /// 标题回退链 display_name > label；模型列缺失由用量流水回填（latest_models 机制）。
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        if !self.lock_scan_budget().ready() {
            return Ok(self.lock_scan_cache().clone().unwrap_or_default());
        }
        let dbs = discover_dbs(&self.roots);
        if dbs.is_empty() {
            *self.lock_scan_cache() = Some(vec![]);
            return Ok(vec![]);
        }
        let cutoff = now_ms() - RANGE_CUTOFF_MS;
        let mut out: Vec<SessionInfo> = vec![];
        for (agent_id, db) in &dbs {
            let Some(conn) = open_db(db) else { continue };
            let mut stmt = match conn.prepare(
                "SELECT n.session_key, COALESCE(NULLIF(n.display_name, ''), n.label),
                        n.created_at, n.updated_at, n.last_activity_at, w.model
                 FROM session_nodes n
                 LEFT JOIN session_windows w ON w.session_id = n.current_session_id
                 WHERE n.updated_at > ?1
                 ORDER BY n.updated_at DESC
                 LIMIT 100",
            ) {
                Ok(s) => s,
                Err(e) => {
                    // 表结构漂移（旧版库）：该库本轮跳过，debug 留痕
                    log::debug!("[openclaw] {agent_id} 会话查询失败（schema 漂移？）：{e}");
                    continue;
                }
            };
            let rows = stmt.query_map([cutoff], |r| {
                let key: String = r.get(0)?;
                Ok(SessionInfo {
                    id: format!("openclaw:{agent_id}:{key}"),
                    agent: "openclaw".into(),
                    provider: None,
                    model: r.get::<_, Option<String>>(5)?,
                    project_dir: None, // 源库无 cwd 列（工作区载体装机核实，§14.3）
                    title: r.get::<_, Option<String>>(1)?,
                    first_seen_at: r.get::<_, i64>(2)?,
                    last_seen_at: r.get::<_, i64>(3)?,
                    // 最近完成回合时间（语义即 last_usage）；缺失回退 updated_at
                    last_usage_at: r.get::<_, Option<i64>>(4)?.or(Some(r.get(3)?)),
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

    /// 水位增量采集：updated_at 新于水位-60s 的转录窗口 → 读其新增事件（seq 水位）
    /// → 解析 assistant 消息为用量行。重复窗口靠 seq 缓存与自库幂等键双重兜底。
    fn collect_usage(&self, watermark_ts: i64) -> anyhow::Result<CollectOutput> {
        if !self.lock_collect_budget().ready() {
            return Ok(self.lock_collect_cache().clone().unwrap_or_default());
        }
        let dbs = discover_dbs(&self.roots);
        if dbs.is_empty() {
            return Ok(CollectOutput::default());
        }
        let window_watermark = watermark_ts.saturating_sub(WATERMARK_LOOKBACK_MS);
        let cutoff = now_ms() - RANGE_CUTOFF_MS;
        let mut rows: Vec<UsageRow> = vec![];
        let mut err_rows = 0usize;
        for (agent_id, db) in &dbs {
            let Some(conn) = open_db(db) else { continue };
            // 活跃窗口：90 天内、updated_at 新于水位-60s 的转录窗口
            let mut stmt = match conn.prepare(
                "SELECT w.session_id, n.session_key
                 FROM session_windows w
                 JOIN session_nodes n ON n.session_key = w.session_key
                 WHERE w.updated_at > ?1 AND w.updated_at > ?2
                 ORDER BY w.updated_at ASC
                 LIMIT ?3",
            ) {
                Ok(s) => s,
                Err(e) => {
                    log::debug!("[openclaw] {agent_id} 窗口查询失败（schema 漂移？）：{e}");
                    continue;
                }
            };
            let windows: Vec<(String, String)> = stmt
                .query_map(
                    rusqlite::params![window_watermark, cutoff, ACTIVE_WINDOW_LIMIT],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )?
                .filter_map(|x| x.ok())
                .collect();
            for (window_id, session_key) in windows {
                let seq_wm = self.lock_seq_watermarks().get(&window_id).copied().unwrap_or(0);
                let mut ev = match conn.prepare(
                    "SELECT seq, created_at, event_json, event_zstd, event_utf8_bytes
                     FROM transcript_events
                     WHERE session_id = ?1 AND seq > ?2
                     ORDER BY seq ASC
                     LIMIT ?3",
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        log::debug!("[openclaw] {agent_id} 事件查询失败（schema 漂移？）：{e}");
                        break;
                    }
                };
                let events: Vec<(i64, i64, Option<String>, Option<Vec<u8>>, Option<i64>)> = ev
                    .query_map(
                        rusqlite::params![window_id, seq_wm, WINDOW_EVENT_LIMIT],
                        |r| {
                            Ok((
                                r.get::<_, i64>(0)?,
                                r.get::<_, i64>(1)?,
                                r.get::<_, Option<String>>(2)?,
                                r.get::<_, Option<Vec<u8>>>(3)?,
                                r.get::<_, Option<i64>>(4)?,
                            ))
                        },
                    )?
                    .filter_map(|x| x.ok())
                    .collect();
                let mut max_seq = seq_wm;
                for (seq, created_at, event_json, event_zstd, raw_bytes) in events {
                    max_seq = max_seq.max(seq);
                    // 解压（部分事件是 zstd）→ 解析；行损坏计数留痕不中断
                    let text = match decode_event(event_json, event_zstd, raw_bytes) {
                        Ok(Some(t)) => t,
                        Ok(None) => continue,
                        Err(e) => {
                            err_rows += 1;
                            log::debug!("[openclaw] {agent_id} 事件解码失败 seq={seq}：{e}");
                            continue;
                        }
                    };
                    match parse_event(agent_id, &session_key, seq, created_at, &text) {
                        Ok(Some(row)) => rows.push(row),
                        Ok(None) => {}
                        Err(e) => {
                            err_rows += 1;
                            log::debug!("[openclaw] {agent_id} 事件解析失败 seq={seq}：{e}");
                        }
                    }
                }
                if max_seq > seq_wm {
                    self.lock_seq_watermarks().insert(window_id, max_seq);
                }
            }
        }
        if err_rows > 0 {
            log::debug!("[openclaw] 采集 {err_rows} 行解析失败已跳过（schema 漂移？）");
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

    /// 建一个合成 agent 库（核心表列集与真实 schema 对齐，01-RESEARCH §14.2），
    /// 返回基准毫秒时间。事件：e1 明文 assistant（官方幂等键）、e2 user（跳过）、
    /// e3 assistant 失败（错误行）、e4 zstd 压缩 assistant（合成幂等键）、
    /// e5 aborted（跳过）。
    fn seed_agent_db(root: &Path, agent_id: &str) -> i64 {
        let base = now_ms() - 3_600_000;
        let agent_dir = root.join("agents").join(agent_id).join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let db = agent_dir.join("openclaw-agent.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_nodes (
                session_key TEXT PRIMARY KEY,
                current_session_id TEXT,
                entry_json TEXT,
                display_name TEXT,
                label TEXT,
                status TEXT,
                created_at INTEGER,
                updated_at INTEGER,
                last_activity_at INTEGER);
            CREATE TABLE session_windows (
                session_id TEXT PRIMARY KEY,
                session_key TEXT,
                model TEXT,
                model_provider TEXT,
                status TEXT,
                created_at INTEGER,
                updated_at INTEGER);
            CREATE TABLE transcript_events (
                session_id TEXT,
                seq INTEGER,
                event_json TEXT,
                event_zstd BLOB,
                event_utf8_bytes INTEGER,
                created_at INTEGER,
                PRIMARY KEY (session_id, seq));",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_nodes VALUES ('main', 'win_a', NULL, '部署脚本调试', NULL, 'done', ?1, ?2, ?2)",
            rusqlite::params![base, base + 6_000],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_nodes VALUES ('job-1', 'win_b', NULL, NULL, '夜间巡检', 'running', ?1, ?2, NULL)",
            rusqlite::params![base + 10_000, base + 20_000],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_windows VALUES ('win_a', 'main', 'claude-opus-4-6', 'anthropic', 'done', ?1, ?2)",
            rusqlite::params![base, base + 6_000],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_windows VALUES ('win_b', 'job-1', NULL, NULL, 'running', ?1, ?2)",
            rusqlite::params![base + 10_000, base + 20_000],
        )
        .unwrap();
        // e1：assistant 消息（明文，全量 usage + 官方幂等键）
        let e1 = format!(
            r#"{{"type":"message","message":{{"role":"assistant",
                "assistantIdempotencyKey":"turn-001","content":[{{"type":"text","text":"好的"}}],
                "api":"anthropic-messages","provider":"anthropic","model":"claude-opus-4-6",
                "usage":{{"input":120,"output":60,"cacheRead":30,"cacheWrite":10,"totalTokens":220}},
                "stopReason":"stop","timestamp":{}}}}}"#,
            base + 2_000
        );
        conn.execute(
            "INSERT INTO transcript_events VALUES ('win_a', 1, ?1, NULL, NULL, ?2)",
            rusqlite::params![e1, base + 2_000],
        )
        .unwrap();
        // e2：user 消息（应跳过）
        let e2 = format!(
            r#"{{"type":"message","message":{{"role":"user","content":[{{"type":"text","text":"帮我看下部署"}}],"timestamp":{}}}}}"#,
            base + 1_000
        );
        conn.execute(
            "INSERT INTO transcript_events VALUES ('win_a', 2, ?1, NULL, NULL, ?2)",
            rusqlite::params![e2, base + 1_000],
        )
        .unwrap();
        // e3：assistant 失败回合（stopReason=error，无 usage → 错误行）
        let e3 = format!(
            r#"{{"type":"message","message":{{"role":"assistant","provider":"anthropic",
                "model":"claude-opus-4-6","stopReason":"error","timestamp":{}}}}}"#,
            base + 5_000
        );
        conn.execute(
            "INSERT INTO transcript_events VALUES ('win_a', 3, ?1, NULL, NULL, ?2)",
            rusqlite::params![e3, base + 5_000],
        )
        .unwrap();
        // e4：assistant 消息（zstd 压缩体 + 正确 utf8 字节数；无官方幂等键 → 合成键）
        let e4_text = format!(
            r#"{{"type":"message","message":{{"role":"assistant",
                "content":[{{"type":"text","text":"{}"}}],
                "provider":"zhipu","model":"glm-5.3",
                "usage":{{"input":11,"output":7,"cacheRead":0,"cacheWrite":0,"totalTokens":18}},
                "stopReason":"stop","timestamp":{}}}}}"#,
            "巡检结论：一切正常。".repeat(40),
            base + 21_000
        );
        let e4_zstd = zstd::stream::encode_all(e4_text.as_bytes(), 1).unwrap();
        conn.execute(
            "INSERT INTO transcript_events VALUES ('win_b', 1, NULL, ?1, ?2, ?3)",
            rusqlite::params![e4_zstd, e4_text.len() as i64, base + 21_000],
        )
        .unwrap();
        // e5：aborted 回合（用户打断，跳过）
        let e5 = format!(
            r#"{{"type":"message","message":{{"role":"assistant","provider":"zhipu",
                "model":"glm-5.3","stopReason":"aborted",
                "usage":{{"input":5,"output":0,"cacheRead":0,"cacheWrite":0}},"timestamp":{}}}}}"#,
            base + 22_000
        );
        conn.execute(
            "INSERT INTO transcript_events VALUES ('win_b', 2, ?1, NULL, NULL, ?2)",
            rusqlite::params![e5, base + 22_000],
        )
        .unwrap();
        base
    }

    fn temp_home(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("at-claw-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 状态根解析：env 优先、无效 env 忽略、profile 目录、旧目录兜底
    #[test]
    fn test_resolve_state_roots() {
        let home = temp_home("roots");
        // 无任何目录 → 空
        assert!(resolve_state_roots(None, Some(home.clone())).is_empty());
        // env 目录存在 → 最优先
        let env_root = home.join("claw-state");
        std::fs::create_dir_all(&env_root).unwrap();
        let roots = resolve_state_roots(Some(env_root.clone().into_os_string()), Some(home.clone()));
        assert_eq!(roots[0], env_root);
        // env 指向不存在目录 → 忽略，不阻断后续默认根
        std::fs::create_dir_all(home.join(".openclaw")).unwrap();
        std::fs::create_dir_all(home.join(".openclaw-work")).unwrap();
        let roots = resolve_state_roots(Some(home.join("nope").into_os_string()), Some(home.clone()));
        assert_eq!(roots.len(), 2, "默认根 + work profile");
        assert!(roots.iter().any(|p| p.ends_with(".openclaw")));
        assert!(roots.iter().any(|p| p.ends_with(".openclaw-work")));
        // 旧目录 .clawdbot 兜底
        std::fs::create_dir_all(home.join(".clawdbot")).unwrap();
        let roots = resolve_state_roots(None, Some(home.clone()));
        assert!(roots.iter().any(|p| p.ends_with(".clawdbot")));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 库发现：agents/<id>/agent/openclaw-agent.sqlite；incognito 哨兵不匹配文件名
    #[test]
    fn test_discover_dbs() {
        let home = temp_home("disc");
        let root = home.join(".openclaw");
        seed_agent_db(&root, "main");
        let dbs = discover_dbs(&[root.clone()]);
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].0, "main");
        // incognito 哨兵（保留名，同目录不同名）不入列
        let inc = root
            .join("agents")
            .join("main")
            .join("agent")
            .join("incognito-openclaw-agent.sqlite");
        std::fs::write(&inc, b"x").unwrap();
        assert_eq!(discover_dbs(&[root]).len(), 1);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// scan：标题回退链（display_name > label）、窗口模型 join、last_usage 回退
    #[test]
    fn test_scan_sessions() {
        let home = temp_home("scan");
        let root = home.join(".openclaw");
        let base = seed_agent_db(&root, "main");
        let ad = OpenClawAdapter::with_roots(vec![root], 0);
        let sessions = ad.scan_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "openclaw:main:job-1", "按 updated_at 降序");
        assert_eq!(sessions[0].title.as_deref(), Some("夜间巡检"), "label 兜底");
        assert_eq!(sessions[0].model, None, "win_b 无模型列");
        assert_eq!(
            sessions[0].last_usage_at,
            Some(base + 20_000),
            "last_activity_at 缺失回退 updated_at"
        );
        assert_eq!(sessions[1].id, "openclaw:main:main");
        assert_eq!(sessions[1].title.as_deref(), Some("部署脚本调试"), "display_name 优先");
        assert_eq!(sessions[1].model.as_deref(), Some("claude-opus-4-6"));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 采集端到端：明文/zstd 事件解析、幂等键、错误行、aborted 跳过、自库幂等
    #[test]
    fn test_collect_end_to_end() {
        let home = temp_home("coll");
        let root = home.join(".openclaw");
        let base = seed_agent_db(&root, "main");
        let ad = OpenClawAdapter::with_roots(vec![root.clone()], 0);

        // 全量（水位 0）：e1/e3/e4 三行；e2 user、e5 aborted 跳过
        let out = ad.collect_usage(0).unwrap().rows;
        assert_eq!(out.len(), 3, "aborted 与 user 行应被跳过");
        let e1 = out
            .iter()
            .find(|r| r.source_id.as_deref() == Some("ocm:turn-001"))
            .unwrap();
        assert_eq!(
            (e1.input_tokens, e1.output_tokens, e1.cache_read_tokens, e1.cache_creation_tokens),
            (Some(120), Some(60), Some(30), Some(10))
        );
        assert_eq!(e1.ts, base + 2_000);
        assert_eq!(e1.model, "claude-opus-4-6");
        assert_eq!(e1.provider.as_deref(), Some("anthropic"));
        assert_eq!(e1.session_id, "openclaw:main:main", "自库会话 id 带 agentId 消歧");
        let e3 = out.iter().find(|r| r.error_type.is_some()).unwrap();
        assert_eq!(e3.error_type.as_deref(), Some("回合失败"));
        assert_eq!(e3.ts, base + 5_000, "无 timestamp 字段回退列 created_at");
        let e4 = out
            .iter()
            .find(|r| r.source_id.as_deref() == Some("oc:main:job-1:1"))
            .unwrap();
        assert_eq!(
            (e4.input_tokens, e4.output_tokens),
            (Some(11), Some(7)),
            "zstd 压缩事件应解压"
        );
        assert_eq!(e4.provider.as_deref(), Some("zhipu"));
        assert_eq!(e4.session_id, "openclaw:main:job-1");

        // 幂等：同批重放（新适配器实例 = seq 水位为空），靠自库幂等键兜底
        let store = crate::store::Store::open(&home.join("store.db")).unwrap();
        assert_eq!(store.insert_usage(&out), out.len());
        assert_eq!(store.insert_usage(&out), 0, "重放应被幂等键全部忽略");
        drop(store);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// seq 水位：同实例二次采集不重解析旧事件；窗口内新增 seq 后只增量产出
    #[test]
    fn test_seq_watermark_incremental() {
        let home = temp_home("seq");
        let root = home.join(".openclaw");
        let base = seed_agent_db(&root, "main");
        let ad = OpenClawAdapter::with_roots(vec![root.clone()], 0);
        assert_eq!(ad.collect_usage(0).unwrap().rows.len(), 3);
        // 再次采集：seq 水位拦截，无新行
        assert!(ad.collect_usage(0).unwrap().rows.is_empty());

        // win_a 追加一条 assistant 事件（seq=4）→ 仅它进入增量
        let db = root
            .join("agents")
            .join("main")
            .join("agent")
            .join("openclaw-agent.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        let e6 = format!(
            r#"{{"type":"message","message":{{"role":"assistant","provider":"anthropic",
                "model":"claude-opus-4-6","stopReason":"stop",
                "usage":{{"input":9,"output":4,"cacheRead":0,"cacheWrite":0}},"timestamp":{}}}}}"#,
            base + 30_000
        );
        conn.execute(
            "INSERT INTO transcript_events VALUES ('win_a', 4, ?1, NULL, NULL, ?2)",
            rusqlite::params![e6, base + 30_000],
        )
        .unwrap();
        // 窗口 updated_at 同步推进（模拟 OpenClaw 写入行为）
        conn.execute(
            "UPDATE session_windows SET updated_at = ?1 WHERE session_id = 'win_a'",
            [base + 30_000],
        )
        .unwrap();
        let rows = ad.collect_usage(0).unwrap().rows;
        assert_eq!(rows.len(), 1, "只有新增 seq=4 重入");
        assert_eq!(rows[0].source_id.as_deref(), Some("oc:main:main:4"));
        assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (Some(9), Some(4)));
        // 幂等键稳定：入自库不重复
        let store = crate::store::Store::open(&home.join("store.db")).unwrap();
        assert_eq!(store.insert_usage(&rows), 1);
        drop(store);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 解码校验：明文直通；zstd 解压＋长度校验；坏流/长度不符报错；双空跳过
    #[test]
    fn test_decode_event() {
        let payload = "压缩样本数据。".repeat(100);
        let blob = zstd::stream::encode_all(payload.as_bytes(), 1).unwrap();
        assert_eq!(
            decode_event(None, Some(blob.clone()), Some(payload.len() as i64))
                .unwrap()
                .unwrap(),
            payload
        );
        // 长度不符 → Err（防错误解码污染用量）
        assert!(decode_event(None, Some(blob), Some(1)).is_err());
        // 坏流 → Err
        assert!(decode_event(None, Some(vec![0u8; 8]), None).is_err());
        // 明文直通；双空 → None
        assert_eq!(
            decode_event(Some("明文".into()), None, None).unwrap().unwrap(),
            "明文"
        );
        assert!(decode_event(None, None, None).unwrap().is_none());
    }

    /// 解析边界：header/非 assistant 跳过；无 usage 非 error 跳过；
    /// 浮点 usage 取整；无 provider 回退模型名推断
    #[test]
    fn test_parse_edges() {
        // session header 事件
        assert!(
            parse_event("m", "k", 1, 1, r#"{"type":"session","id":"x"}"#)
                .unwrap()
                .is_none()
        );
        // user 消息
        assert!(
            parse_event(
                "m",
                "k",
                2,
                1,
                r#"{"type":"message","message":{"role":"user","text":"你好"}}"#
            )
            .unwrap()
            .is_none()
        );
        // assistant 无 usage 且非 error
        assert!(
            parse_event(
                "m",
                "k",
                3,
                1,
                r#"{"type":"message","message":{"role":"assistant","stopReason":"stop"}}"#
            )
            .unwrap()
            .is_none()
        );
        // 浮点 usage 取整 + timestamp 缺失回退 created_at + 模型名推断 provider
        let row = parse_event(
            "m",
            "k",
            4,
            7_777,
            r#"{"type":"message","message":{"role":"assistant","model":"glm-5.3",
                "usage":{"input":7.6,"output":3.2,"cacheRead":0,"cacheWrite":0},"stopReason":"stop"}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!((row.input_tokens, row.output_tokens), (Some(8), Some(3)));
        assert_eq!(row.ts, 7_777);
        assert_eq!(row.provider.as_deref(), Some("glm"));
        assert_eq!(row.source_id.as_deref(), Some("oc:m:k:4"));
    }

    /// 集成测试：连接本机真实 OpenClaw 库（未装跳过；装机后手动
    /// `cargo test -- --ignored` 补跑对账，清单见 01-RESEARCH §14.3）
    #[test]
    #[ignore]
    fn test_real_openclaw_collect() {
        let roots = current_state_roots();
        let dbs = discover_dbs(&roots);
        assert!(!dbs.is_empty(), "本机应有 OpenClaw agent 库");
        let ad = OpenClawAdapter::with_roots(roots, 0);
        let sessions = ad.scan_sessions().unwrap();
        assert!(!sessions.is_empty(), "本机应有 OpenClaw 会话");
        let usage = ad.collect_usage(0).unwrap().rows;
        assert!(!usage.is_empty(), "本机应有历史用量");
        for u in usage.iter().take(5) {
            assert!(u.ts > 1_700_000_000_000, "时间戳应为毫秒：{}", u.ts);
            assert_eq!(u.agent, "openclaw");
            assert!(u.session_id.starts_with("openclaw:"));
        }
        assert!(
            ad.collect_usage(9_999_999_999_999).unwrap().rows.is_empty(),
            "水位增量应为空"
        );
    }
}
