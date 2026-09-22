//! Codex 适配器（M2-6）：tail `$CODEX_HOME/sessions/**/rollout-*.jsonl` 转录。
//! 数据面勘察（2026-09-23 源码级核实，openai/codex main 分支）见 docs/01-RESEARCH.md §11：
//!   ① 行 envelope：`{timestamp: ISO 毫秒字符串, ordinal?, type, payload}`（flatten 平铺）；
//!   ② token 用量两条通路：`token_usage_record` 行（payload.response_id 作幂等键，
//!      新版权威记账通路）与 `event_msg→token_count` 行（info.last_token_usage 单次量，
//!      无 id 的旧通路）——**同轮增量内 record 优先、token_count 整体丢弃**（文件级互斥
//!      防双计：新版本同一调用两通路都会写）；
//!   ③ TokenUsage 字段 snake_case：input_tokens / cached_input_tokens /
//!      cache_write_input_tokens / output_tokens / reasoning_output_tokens / total_tokens；
//!   ④ 缓存语义差异：OpenAI 的 cached_input_tokens 是 input_tokens 的**子集**
//!      （Anthropic 的 cache_read 是独立计费分项），按四项互斥口径拆分
//!      input -= cached（装机对账验证点，见 collect_usage 注释）；
//!   ⑤ model 不在 session_meta，随 `turn_context` 行逐 turn 更新。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use super::engine::{
    body_after_partial, iso_to_ms, mtime_ms, GlobWalker, HotSignal, IncrementalFileReader,
    ProcessMatch,
};
use super::{AgentAdapter, CollectOutput, SessionInfo, provider_from_model};
use crate::store::UsageRow;

pub use hooks::{hooks_installed, install_hooks, uninstall_hooks};

/// 转录根目录（$CODEX_HOME/sessions，缺省 ~/.codex/sessions）。
/// CODEX_HOME 重定向与官方同规则：非空时目标必须已存在且是目录，异常即回落默认（§2.8）
fn sessions_root() -> Option<PathBuf> {
    Some(codex_home()?.join("sessions"))
}

/// Codex 数据根（CODEX_HOME 环境变量 > ~/.codex）
fn codex_home() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("CODEX_HOME") {
        let p = PathBuf::from(v);
        if p.is_dir() {
            return Some(p);
        }
        log::debug!("[codex] CODEX_HOME 指向的目录不存在，回落默认路径：{}", p.display());
        return None;
    }
    let home = std::env::var_os("USERPROFILE")?;
    Some(PathBuf::from(home).join(".codex"))
}

/// 从 rollout 文件名提取 thread id（会话主键）：
/// `rollout-<UTC时间戳>-<thread_uuid>.jsonl`（时间戳与 uuid 都含 `-`，uuid 恒为
/// 末 5 段 8-4-4-4-12）；revert 变体 `...-<thread_uuid>_<rollout_id>` 先按 `_` 取前段
fn thread_id_from_stem(stem: &str) -> Option<String> {
    let rest = stem.strip_prefix("rollout-")?;
    let main = rest.split('_').next()?;
    let segs: Vec<&str> = main.split('-').collect();
    if segs.len() < 5 {
        return None;
    }
    Some(segs[segs.len() - 5..].join("-"))
}

/// 转录行 envelope（宽松解析：payload 用 Value 二次取字段，容忍格式漂移）
#[derive(serde::Deserialize)]
struct RolloutLine {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: serde_json::Value,
}

/// 从行 JSON 顶层取 `"type":"session_meta"` 行 payload.cwd（转录头部读取）。
/// 头部可能先出现其他类型行，逐行解析直到命中（读取量 64KB，与 CC 同策略）
fn first_cwd(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 64 * 1024];
    let n = f.read(&mut buf).ok()?;
    let head = String::from_utf8_lossy(&buf[..n]);
    for line in head.lines() {
        let Ok(j) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if j.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
            continue;
        }
        if let Some(cwd) = j.pointer("/payload/cwd").and_then(|c| c.as_str()) {
            if !cwd.is_empty() {
                return Some(cwd.to_string());
            }
        }
    }
    None
}

pub struct CodexAdapter {
    root: PathBuf,
    /// 目录枚举走树器（`**/rollout-*.jsonl` 覆盖 YYYY/MM/DD 日期分区与 revert 变体，
    /// 目录 mtime 剪枝缓存让新日期目录自动纳入且每轮枚举成本可控）
    walker: Mutex<GlobWalker>,
    /// 单文件增量游标（mtime 过滤/64KB 回退/重建归零，引擎机制）
    reader: Mutex<IncrementalFileReader>,
    /// cwd 提取缓存：路径 →（mtime_ms， 项目目录），与 CC 同策略
    cwd_cache: Mutex<HashMap<PathBuf, (i64, Option<String>)>>,
    /// per-file 当前模型缓存：model 随 `turn_context` 行逐 turn 更新，增量读时
    /// 旧行不在本轮增量内，跨轮记住最近模型供用量行回填（UsageRow.model 非空）
    model_cache: Mutex<HashMap<PathBuf, String>>,
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self::with_root(sessions_root().unwrap_or_else(|| PathBuf::from("")))
    }

    /// 指定根目录构造（单测注入临时目录用）
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            walker: Mutex::new(GlobWalker::default()),
            reader: Mutex::new(IncrementalFileReader::default()),
            cwd_cache: Mutex::new(HashMap::new()),
            model_cache: Mutex::new(HashMap::new()),
        }
    }

    fn lock_walker(&self) -> MutexGuard<'_, GlobWalker> {
        self.walker.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_reader(&self) -> MutexGuard<'_, IncrementalFileReader> {
        self.reader.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_cwd(&self) -> MutexGuard<'_, HashMap<PathBuf, (i64, Option<String>)>> {
        self.cwd_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_model(&self) -> MutexGuard<'_, HashMap<PathBuf, String>> {
        self.model_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn transcript_files(&self) -> Vec<PathBuf> {
        self.lock_walker().list(&self.root, "**/rollout-*.jsonl")
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// TokenUsage 四项 + reasoning 的提取结果（已按互斥口径拆分缓存）
#[derive(Debug, Clone, Copy)]
struct UsageFields {
    input: i64,
    output: i64,
    reasoning: i64,
    cache_read: i64,
    cache_creation: i64,
}

/// 从行 payload 的 usage 对象提取四项（字段缺失按 0）。
/// 缓存拆分（④）：OpenAI 的 input_tokens 含 cached_input_tokens（子集语义），
/// 而 Anthropic 口径下 cache_read 是独立分项——我们的总账 = 四项互斥相加，
/// 故 input 减去 cached 部分；若实测 input < cached（分立计费语义）则照抄不动
fn extract_usage(usage: &serde_json::Value) -> UsageFields {
    let g = |k: &str| usage.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let input_raw = g("input_tokens");
    let cached = g("cached_input_tokens");
    let (input, cache_read) = if input_raw >= cached {
        (input_raw - cached, cached)
    } else {
        (input_raw, cached)
    };
    UsageFields {
        input,
        output: g("output_tokens"),
        reasoning: g("reasoning_output_tokens"),
        cache_read,
        cache_creation: g("cache_write_input_tokens"),
    }
}

/// 行用量四项之和（去重时的比较口径）
fn row_total(r: &UsageRow) -> i64 {
    r.input_tokens.unwrap_or(0)
        + r.output_tokens.unwrap_or(0)
        + r.cache_read_tokens.unwrap_or(0)
        + r.cache_creation_tokens.unwrap_or(0)
}

impl AgentAdapter for CodexAdapter {
    fn id(&self) -> &'static str {
        "codex"
    }

    /// 快轮信号：sessions 目录浅枚举（未装 hooks 的降级信号源；日期分区 3 层）。
    /// 装了 hooks 时事件文件信号自动生效（service 层消费触发不了快轮——快轮由
    /// 调度器采样 hot_signals，故 hooks 事件文件也声明为信号，与 CC 同款双信号）
    fn hot_signals(&self) -> Vec<HotSignal> {
        vec![
            HotSignal::File(std::sync::Arc::new(|| {
                super::hook_events::events_file_path("codex")
            })),
            HotSignal::DirScan {
                root: self.root.clone(),
                ext: Some(".jsonl"),
                depth: 3,
                max_files: 400,
            },
        ]
    }

    /// 进程匹配（M2-4 声明化）：npm 包只是启动器，常驻进程是原生 codex.exe；
    /// 本工具自身排除，防误判存活
    fn process_match(&self) -> Option<ProcessMatch> {
        Some(ProcessMatch {
            name_keywords: &["codex"],
            cmd_keywords: &["codex"],
            cmd_excludes: &["agenttrackerisland"],
        })
    }

    /// 会话发现：每个 rollout 文件即一个会话；最近 90 天有修改的才纳入
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        let cutoff = chrono::Utc::now().timestamp_millis() - 90 * 24 * 3600 * 1000;
        let mut out = vec![];
        for f in self.transcript_files() {
            let mtime = mtime_ms(&f);
            if mtime < cutoff {
                continue;
            }
            // 会话主键 = 文件名里的 thread uuid（revert 变体归并回主 thread）
            let Some(session_id) = f
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(thread_id_from_stem)
            else {
                continue;
            };
            // 项目目录：session_meta 行的真实 cwd（头部 64KB，mtime 缓存）；
            // 取不到退回日期分区父目录名（展示兜底）
            let project = {
                let mut map = self.lock_cwd();
                match map.get(&f).cloned() {
                    Some((m0, cached)) if m0 == mtime => cached,
                    _ => {
                        let fresh = first_cwd(&f).or_else(|| {
                            f.parent()
                                .and_then(|p| p.file_name())
                                .and_then(|n| n.to_str())
                                .map(|s| s.to_string())
                        });
                        map.insert(f.clone(), (mtime, fresh.clone()));
                        fresh
                    }
                }
            };
            out.push(SessionInfo {
                id: format!("codex:{session_id}"),
                agent: "codex".into(),
                provider: None, // collect 阶段按模型回填（gpt* → openai）
                model: None,
                project_dir: project,
                title: None, // Codex 无会话标题源
                first_seen_at: mtime,
                last_seen_at: mtime,
                last_usage_at: Some(mtime),
            });
        }
        out.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));
        if out.len() > 100 {
            log::debug!("[codex] 会话 {} 个，截断保留最近 100", out.len());
        }
        out.truncate(100);
        Ok(out)
    }

    /// 水位增量采集（解析策略见模块注释）：
    ///   record 通路 key = "tur:{response_id}"（官方幂等键）；
    ///   token_count 退路 key = "tc:{行时间戳毫秒}"（无官方 id，同刻多响应按
    ///   last 量最大保留——流式中间快照合并，跨毫秒分裂的少量双计留待装机对账）；
    ///   两通路互斥：本轮增量中出现任何 record 行则整体丢弃 token_count 行
    fn collect_usage(&self, watermark_ts: i64) -> anyhow::Result<CollectOutput> {
        // record 通路：（response_id → 行）；token_count 退路：（ts → 行）
        let mut record_rows: HashMap<String, UsageRow> = HashMap::new();
        let mut event_rows: HashMap<String, UsageRow> = HashMap::new();
        let (mut err_open, mut bad_lines) = (0usize, 0usize);
        for f in self.transcript_files() {
            if mtime_ms(&f) <= watermark_ts {
                continue;
            }
            let (start, text) = match self.lock_reader().changed(&f) {
                Ok(Some(delta)) => (delta.start, delta.text),
                Ok(None) => continue,
                Err(_) => {
                    err_open += 1;
                    continue;
                }
            };
            let file_session = f
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(thread_id_from_stem)
                .unwrap_or_default();
            let Some(body) = body_after_partial(&text, start) else {
                continue;
            };
            // 本轮增量内最近的模型名（turn_context 行更新；跨轮由缓存承接）
            let mut model = self.lock_model().get(&f).cloned().unwrap_or_default();
            for line in body.lines() {
                let Ok(j) = serde_json::from_str::<RolloutLine>(line) else {
                    bad_lines += 1;
                    continue;
                };
                match j.kind.as_str() {
                    // 模型名随 turn 更新（session_meta 无 model，调研结论 ⑤）
                    "turn_context" => {
                        if let Some(m) = j.payload.get("model").and_then(|m| m.as_str()) {
                            if !m.is_empty() {
                                model = m.to_string();
                            }
                        }
                    }
                    // 权威记账通路：单响应 usage + response_id 幂等键
                    "token_usage_record" => {
                        let Some(usage_v) = j.payload.get("usage") else { continue };
                        let Some(response_id) =
                            j.payload.get("response_id").and_then(|r| r.as_str())
                        else {
                            continue;
                        };
                        let Some(ts) = j.timestamp.as_deref().and_then(iso_to_ms) else {
                            continue;
                        };
                        if ts <= watermark_ts {
                            continue;
                        }
                        let u = extract_usage(usage_v);
                        let row = UsageRow {
                            session_id: format!("codex:{file_session}"),
                            agent: "codex".into(),
                            model: model.clone(),
                            provider: provider_from_model(&model),
                            ts,
                            input_tokens: Some(u.input),
                            output_tokens: Some(u.output),
                            reasoning_tokens: Some(u.reasoning),
                            cache_read_tokens: Some(u.cache_read),
                            cache_creation_tokens: Some(u.cache_creation),
                            duration_ms: None,
                            ttft_ms: None,
                            error_type: None,
                            source_id: Some(format!("tur:{response_id}")),
                            is_background: false,
                        };
                        // 同 response 重复行（增量回退重读等）：保留用量大者
                        let total = row_total(&row);
                        record_rows
                            .entry(response_id.to_string())
                            .and_modify(|old| {
                                if total > row_total(old) {
                                    *old = row.clone();
                                }
                            })
                            .or_insert(row);
                    }
                    // 旧版退路：event_msg 的 token_count 行（last_token_usage = 单次量；
                    // total_token_usage 是会话累计，绝不能按行入库——双计放大器）
                    "event_msg" => {
                        if j.payload.get("type").and_then(|t| t.as_str()) != Some("token_count") {
                            continue;
                        }
                        let Some(info) = j.payload.get("info") else { continue };
                        let Some(last) = info.get("last_token_usage") else { continue };
                        let Some(ts) = j.timestamp.as_deref().and_then(iso_to_ms) else {
                            continue;
                        };
                        if ts <= watermark_ts {
                            continue;
                        }
                        let u = extract_usage(last);
                        let row = UsageRow {
                            session_id: format!("codex:{file_session}"),
                            agent: "codex".into(),
                            model: model.clone(),
                            provider: provider_from_model(&model),
                            ts,
                            input_tokens: Some(u.input),
                            output_tokens: Some(u.output),
                            reasoning_tokens: Some(u.reasoning),
                            cache_read_tokens: Some(u.cache_read),
                            cache_creation_tokens: Some(u.cache_creation),
                            duration_ms: None,
                            ttft_ms: None,
                            error_type: None,
                            source_id: Some(format!("tc:{file_session}:{ts}")),
                            is_background: false,
                        };
                        // 同毫秒多条 token_count（流式快照）：保留 last 总量最大者
                        let total = row_total(&row);
                        event_rows
                            .entry(format!("{file_session}:{ts}"))
                            .and_modify(|old| {
                                if total > row_total(old) {
                                    *old = row.clone();
                                }
                            })
                            .or_insert(row);
                    }
                    _ => {} // response_item/compacted 等无关行忽略
                }
            }
            self.lock_model().insert(f.clone(), model);
        }
        if err_open + bad_lines > 0 {
            log::debug!(
                "[codex] 采集异常统计：文件打开/读取失败 {err_open}，坏行 {bad_lines}\
                 （新版 rollout 压缩变体也可能计入坏行，静默容忍）"
            );
        }
        // 通路互斥：有权威 record 就整体丢弃 token_count 退路（防同调用双计）
        let mut rows: Vec<UsageRow> = if record_rows.is_empty() {
            event_rows.into_values().collect()
        } else {
            record_rows.into_values().collect()
        };
        rows.sort_by_key(|r| r.ts);
        Ok(CollectOutput::default().with_rows(rows))
    }
}

/// 当前 Unix 毫秒
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ===== hooks 安装/卸载（增强档；注入器走 config.toml [hooks] 段） =====
// 注入形状按 hook_config.rs serde 结构（2026-09-23 源码核对，deny_unknown_fields）：
//   [[hooks.<Event>]]            ← Vec<MatcherGroup>（数组表）
//   [[hooks.<Event>.hooks]]      ← MatcherGroup.hooks: Vec<HookHandlerConfig>
//   type = "command" / command = ... / timeout = 秒 / async = bool

mod hooks {
    use std::path::{Path, PathBuf};

    use super::{codex_home, now_ms};
    use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

    /// 桥脚本注入标记（卸载识别自家条目；与 CC 共用同一份桥脚本源码）
    pub const BRIDGE_MARK: &str = "hook-bridge.js";
    /// 注入事件集：状态机可消费的 8 个（PreCompact/PostCompact/Subagent* 无状态价值不注入）
    pub const HOOK_EVENTS: &[&str] = &[
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "PermissionRequest",
        "Stop",
        "Interrupt",
        "SessionEnd",
    ];

    fn config_path() -> Option<PathBuf> {
        Some(codex_home()?.join("config.toml"))
    }

    fn bridge_script_path() -> Option<PathBuf> {
        Some(codex_home()?.join("hooks").join("hook-bridge.js"))
    }

    /// 查询 hooks 是否已安装（config.toml 含自家注入标记）
    pub fn hooks_installed() -> bool {
        let Some(path) = config_path() else { return false };
        if !path.exists() {
            return false;
        }
        std::fs::read_to_string(&path)
            .map(|raw| raw.contains(BRIDGE_MARK))
            .unwrap_or(false)
    }

    /// 安装：桥脚本写出到 $CODEX_HOME/hooks/，config.toml 备份后注入 8 事件；
    /// 返回注入条数。config.toml 不存在 = Codex 未装过（创建新文件即可，Codex 首启识别）
    pub fn install_hooks() -> anyhow::Result<usize> {
        let bridge = bridge_script_path().ok_or_else(|| anyhow::anyhow!("无法定位 CODEX_HOME"))?;
        if let Some(dir) = bridge.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&bridge, super::super::claude_code::BRIDGE_SOURCE)?;
        let settings = config_path().ok_or_else(|| anyhow::anyhow!("无法定位 config.toml"))?;
        if !settings.exists() {
            std::fs::write(&settings, "")?;
        }
        // 第二参数是 Agent 标识：桥脚本据此写入 events/codex.jsonl
        let cmd = format!(
            "node \"{}\" codex",
            bridge.to_string_lossy().replace('\\', "/")
        );
        let injected = inject_into_config(&settings, &cmd)?;
        log::info!("[codex] hooks 安装完成：注入 {injected} 个事件，桥脚本 {}", bridge.display());
        Ok(injected)
    }

    /// 卸载：移除全部自家注入条目；返回移除条数（桥脚本文件保留）
    pub fn uninstall_hooks() -> anyhow::Result<usize> {
        let settings = config_path().ok_or_else(|| anyhow::anyhow!("无法定位 config.toml"))?;
        if !settings.exists() {
            return Ok(0);
        }
        let removed = uninstall_from_config(&settings)?;
        log::info!("[codex] hooks 卸载完成：移除 {removed} 个注入条目");
        Ok(removed)
    }

    /// 原子写 + 占用重试（与 CC 注入器同策略；toml_edit 序列化保留用户注释与格式）
    fn atomic_write_retry(path: &Path, data: &str) -> anyhow::Result<()> {
        let tmp = path.with_extension("toml.at-tmp");
        std::fs::write(&tmp, data)?;
        let mut last_err = None;
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            match std::fs::rename(&tmp, path) {
                Ok(()) => return Ok(()),
                Err(e) => last_err = Some(e),
            }
        }
        Err(anyhow::anyhow!(
            "写入 {} 失败（目标可能被占用，请关闭正在编辑该文件的程序后重试）：{}",
            path.display(),
            last_err.unwrap()
        ))
    }

    /// handler 表是否含自家桥脚本命令
    fn handler_is_ours(handler: &Table) -> bool {
        handler
            .get("command")
            .and_then(|c| c.as_str())
            .map(|c| c.contains(BRIDGE_MARK))
            .unwrap_or(false)
    }

    /// MatcherGroup 是否含自家条目
    fn group_is_ours(group: &Table) -> bool {
        group
            .get("hooks")
            .and_then(|i| i.as_array_of_tables())
            .map(|hs| hs.iter().any(handler_is_ours))
            .unwrap_or(false)
    }

    /// config.toml 注入核心（独立函数便于用临时文件单测）
    fn inject_into_config(path: &Path, command: &str) -> anyhow::Result<usize> {
        let raw = std::fs::read_to_string(path)?;
        // 备份（带毫秒时间戳，不覆盖历史备份）
        let bak = path.with_extension(format!("toml.bak-at-{}", now_ms()));
        std::fs::write(&bak, &raw)?;
        let mut doc = raw
            .parse::<DocumentMut>()
            .map_err(|e| anyhow::anyhow!("config.toml 解析失败（不碰用户配置）：{e}"))?;
        let root = doc.as_table_mut();
        // 确保 hooks 表存在（implicit：无直接键值时不落 [hooks] 头，保持文件整洁）
        if root.get("hooks").is_none() {
            let mut t = Table::new();
            t.set_implicit(true);
            root.insert("hooks", Item::Table(t));
        }
        let Some(hooks) = root.get_mut("hooks").and_then(|i| i.as_table_mut()) else {
            anyhow::bail!("config.toml 的 hooks 键不是表结构：保守跳过，不碰用户配置");
        };
        let mut injected = 0usize;
        for ev in HOOK_EVENTS {
            // 防重复：该事件任一 group 已含自家条目即跳过
            let already = hooks
                .get(*ev)
                .and_then(|i| i.as_array_of_tables())
                .map(|aot| aot.iter().any(group_is_ours))
                .unwrap_or(false);
            if already {
                continue;
            }
            // 事件键不存在 → 新建数组表；存在但不是数组表（用户写成普通表）→ 保守跳过
            if hooks.get(*ev).is_none() {
                hooks.insert(*ev, Item::ArrayOfTables(ArrayOfTables::new()));
            }
            let Some(aot) = hooks.get_mut(*ev).and_then(|i| i.as_array_of_tables_mut()) else {
                log::debug!("[codex] hooks.{ev} 非数组表结构，保守跳过");
                continue;
            };
            // MatcherGroup { hooks: [ { type="command", command, timeout=10, async=true } ] }
            let mut handler = Table::new();
            handler.insert("type", value("command"));
            handler.insert("command", value(command));
            handler.insert("timeout", value(10i64));
            handler.insert("async", value(true));
            let mut group = Table::new();
            let mut hs = ArrayOfTables::new();
            hs.push(handler);
            group.insert("hooks", Item::ArrayOfTables(hs));
            aot.push(group);
            injected += 1;
        }
        if injected > 0 {
            atomic_write_retry(path, &doc.to_string())?;
        }
        Ok(injected)
    }

    /// config.toml 卸载核心：按 group 粒度移除自家条目（每事件一个 group 一个 handler），
    /// 用户自配 group 无标记原样保留；事件数组空则删事件键，hooks 表仅剩用户内容则保留
    fn uninstall_from_config(path: &Path) -> anyhow::Result<usize> {
        let raw = std::fs::read_to_string(path)?;
        let mut doc = raw
            .parse::<DocumentMut>()
            .map_err(|e| anyhow::anyhow!("config.toml 解析失败：{e}"))?;
        let Some(hooks) = doc.get_mut("hooks").and_then(|i| i.as_table_mut()) else {
            return Ok(0);
        };
        let mut removed = 0usize;
        // 先收集事件键再逐个改（iter 借用与修改不能同时持有）
        let ev_keys: Vec<String> = hooks.iter().map(|(k, _)| k.to_string()).collect();
        for ev in ev_keys {
            let Some(aot) = hooks.get_mut(&ev).and_then(|i| i.as_array_of_tables_mut()) else {
                continue;
            };
            let before = aot.len();
            aot.retain(|g| !group_is_ours(g));
            removed += before - aot.len();
            if aot.is_empty() {
                hooks.remove(&ev);
            }
        }
        // hooks 表空（无 state 等用户残留子键）才整体移除
        if hooks.is_empty() {
            doc.as_table_mut().remove("hooks");
        }
        atomic_write_retry(path, &doc.to_string())?;
        Ok(removed)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn tmp_config(tag: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!("at-codex-hooks-{}-{}", tag, std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            dir.join("config.toml")
        }

        /// 注入/卸载往返：用户已有配置（含注释与自配 hooks）不受损，自家条目完整还原
        #[test]
        fn test_hooks_install_uninstall_roundtrip() {
            let cfg = tmp_config("rt");
            std::fs::write(&cfg, concat!(
                "# 用户注释必须保留\n",
                "model = \"gpt-5.2\"\n",
                "[[hooks.Stop]]\n",
                "[[hooks.Stop.hooks]]\n",
                "type = \"command\"\n",
                "command = \"python user_hook.py\"\n",
            )).unwrap();

            let cmd = "node \"C:/x/.codex/hooks/hook-bridge.js\" codex";
            let n = inject_into_config(&cfg, cmd).unwrap();
            assert_eq!(n, 8, "注入 8 个事件");
            let s1 = std::fs::read_to_string(&cfg).unwrap();
            assert!(s1.contains("# 用户注释必须保留"), "用户注释必须保留");
            assert!(s1.contains("python user_hook.py"), "用户自配 hooks 原样保留");
            assert!(s1.contains("[[hooks.PermissionRequest]]"), "数组表形状正确");
            assert!(s1.contains("async = true"), "handler 字段齐全");

            // 防重复
            let n2 = inject_into_config(&cfg, cmd).unwrap();
            assert_eq!(n2, 0);

            // 卸载：自家条目全清，用户内容还原
            let removed = uninstall_from_config(&cfg).unwrap();
            assert_eq!(removed, 8);
            let s2 = std::fs::read_to_string(&cfg).unwrap();
            assert!(s2.contains("python user_hook.py"), "用户 hooks 保留");
            assert!(!s2.contains("hook-bridge.js"), "自家条目全清");
            assert!(s2.contains("model = \"gpt-5.2\""), "其他配置不受影响");
            assert!(s2.contains("[hooks.Stop]"), "用户事件键保留");
            let _ = std::fs::remove_file(&cfg);
        }

        /// 空文件（Codex 未装过）注入：生成合法 TOML 且卸载后 hooks 段整体消失
        #[test]
        fn test_hooks_install_on_empty_config() {
            let cfg = tmp_config("empty");
            std::fs::write(&cfg, "").unwrap();
            // 命令必须含 BRIDGE_MARK：卸载按标记识别自家条目
            let n = inject_into_config(&cfg, "node \"C:/x/.codex/hooks/hook-bridge.js\" codex")
                .unwrap();
            assert_eq!(n, 8);
            let parsed: DocumentMut = std::fs::read_to_string(&cfg).unwrap().parse().unwrap();
            assert!(parsed.get("hooks").is_some());
            let removed = uninstall_from_config(&cfg).unwrap();
            assert_eq!(removed, 8);
            let s = std::fs::read_to_string(&cfg).unwrap();
            assert!(!s.contains("hooks"), "空配置卸载后 hooks 段整体消失：{s}");
            let _ = std::fs::remove_file(&cfg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("at-codex-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// thread id 提取：普通文件名 / revert 变体 / 异常短名
    #[test]
    fn test_thread_id_from_stem() {
        let normal = "rollout-2025-05-07T17-24-21-0f8a2b3c-4d5e-4f6a-8b7c-9d0e1f2a3b4c";
        assert_eq!(
            thread_id_from_stem(normal).as_deref(),
            Some("0f8a2b3c-4d5e-4f6a-8b7c-9d0e1f2a3b4c")
        );
        let revert =
            "rollout-2025-05-07T17-24-21-0f8a2b3c-4d5e-4f6a-8b7c-9d0e1f2a3b4c_11112222-3333-4444-5555-666677778888";
        assert_eq!(
            thread_id_from_stem(revert).as_deref(),
            Some("0f8a2b3c-4d5e-4f6a-8b7c-9d0e1f2a3b4c"),
            "revert 变体应归并回主 thread"
        );
        assert_eq!(thread_id_from_stem("rollout-short"), None);
        assert_eq!(thread_id_from_stem("other"), None);
    }

    /// 合成 rollout 端到端：record 通路解析 + 缓存拆分 + 幂等键 + token_count 互斥 +
    /// turn_context 模型回填 + 水位过滤 + 日期分区目录枚举
    #[test]
    fn test_collect_rollout() {
        let dir = tmp_dir("collect");
        let day = dir.join("2026").join("09").join("22");
        std::fs::create_dir_all(&day).unwrap();
        let f = day.join("rollout-2026-09-22T10-00-00-0f8a2b3c-4d5e-4f6a-8b7c-9d0e1f2a3b4c.jsonl");
        let ts = "2026-09-22T10:00:01.000Z";
        let lines = [
            // session_meta：scan 取 cwd；collect 忽略
            r#"{"timestamp":"2026-09-22T10:00:00.000Z","type":"session_meta","payload":{"id":"0f8a2b3c-4d5e-4f6a-8b7c-9d0e1f2a3b4c","cwd":"F:\\Demo\\proj","originator":"codex_cli_rs"}}"#,
            // turn_context：模型名来源
            r#"{"timestamp":"2026-09-22T10:00:00.500Z","type":"turn_context","payload":{"cwd":"F:\\Demo\\proj","model":"gpt-5.2"}}"#,
            // 权威 record 行（input 含 cached：300 含 100 → 拆分后 input=200/cache_read=100）
            r#"{"timestamp":"2026-09-22T10:00:01.000Z","type":"token_usage_record","payload":{"thread_id":"0f8a2b3c","response_id":"resp_1","usage":{"input_tokens":300,"cached_input_tokens":100,"cache_write_input_tokens":7,"output_tokens":50,"reasoning_output_tokens":20,"total_tokens":357}}}"#,
            // 无关行
            r#"{"timestamp":"2026-09-22T10:00:01.200Z","type":"response_item","payload":{"type":"message"}}"#,
        ];
        std::fs::write(&f, lines.join("\n")).unwrap();

        let ad = CodexAdapter::with_root(dir.clone());
        let sessions = ad.scan_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "codex:0f8a2b3c-4d5e-4f6a-8b7c-9d0e1f2a3b4c");
        assert_eq!(sessions[0].project_dir.as_deref(), Some("F:\\Demo\\proj"));

        let out = ad.collect_usage(0).unwrap();
        assert_eq!(out.rows.len(), 1, "只有 record 通路 1 行");
        let r = &out.rows[0];
        assert_eq!(r.session_id, "codex:0f8a2b3c-4d5e-4f6a-8b7c-9d0e1f2a3b4c");
        assert_eq!(r.model, "gpt-5.2", "turn_context 模型回填");
        assert_eq!(r.provider.as_deref(), Some("openai"));
        assert_eq!(r.input_tokens, Some(200), "缓存拆分：input 去掉 cached 子集");
        assert_eq!(r.cache_read_tokens, Some(100));
        assert_eq!(r.cache_creation_tokens, Some(7));
        assert_eq!(r.output_tokens, Some(50));
        assert_eq!(r.reasoning_tokens, Some(20));
        assert_eq!(r.source_id.as_deref(), Some("tur:resp_1"));
        // 幂等：同 response_id 重复入库被自库幂等键挡住，这里验证重复解析仍 1 行
        let out2 = ad.collect_usage(0).unwrap();
        assert!(out2.rows.is_empty(), "mtime 未变无新行");

        // 追加 token_count 行（新版 record 文件里混入 event 行）→ 互斥不双计
        let tc_line = r#"{"timestamp":"2026-09-22T10:00:02.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":300,"cached_input_tokens":100,"output_tokens":50,"reasoning_output_tokens":20,"total_tokens":357},"last_token_usage":{"input_tokens":300,"cached_input_tokens":100,"output_tokens":50,"reasoning_output_tokens":20,"total_tokens":357}}}}"#;
        std::fs::write(&f, format!("{}\n{tc_line}\n", lines.join("\n"))).unwrap();
        let out3 = ad.collect_usage(0).unwrap();
        assert_eq!(
            out3.rows.len(),
            1,
            "record 与 token_count 同现时 record 优先（互斥防双计）"
        );
        assert!(out3.rows[0].source_id.as_deref().unwrap().starts_with("tur:"));

        // 水位：远未来水位 → 全部行被过滤
        let out4 = ad.collect_usage(1_800_000_000_000_000).unwrap();
        assert!(out4.rows.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 旧版退路：无 record 行的文件用 event_msg/token_count 的 last_token_usage
    #[test]
    fn test_collect_token_count_fallback() {
        let dir = tmp_dir("fallback");
        let day = dir.join("2026").join("09").join("20");
        std::fs::create_dir_all(&day).unwrap();
        let f = day.join("rollout-2026-09-20T10-00-00-aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee.jsonl");
        let lines = [
            r#"{"timestamp":"2026-09-20T10:00:00.000Z","type":"session_meta","payload":{"id":"aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee","cwd":"F:\\Old"}}"#,
            r#"{"timestamp":"2026-09-20T10:00:01.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"reasoning_output_tokens":0,"total_tokens":110},"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"reasoning_output_tokens":0,"total_tokens":110}}}}"#,
        ];
        std::fs::write(&f, lines.join("\n")).unwrap();

        let ad = CodexAdapter::with_root(dir.clone());
        let out = ad.collect_usage(0).unwrap();
        assert_eq!(out.rows.len(), 1, "退路通路产出 1 行");
        assert!(
            out.rows[0]
                .source_id
                .as_deref()
                .unwrap()
                .starts_with("tc:aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee:"),
            "退路幂等键应为 tc:<thread_id>:<行毫秒>"
        );
        assert_eq!(out.rows[0].input_tokens, Some(100));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 集成：本机真实 Codex 数据（手动：cargo test -- --ignored test_real_codex）。
    /// 待所有者装机后执行：scan/collect 形状 + 水位增量 + 与 Codex 官方统计对账
    #[test]
    #[ignore]
    fn test_real_codex() {
        let ad = CodexAdapter::new();
        let sessions = ad.scan_sessions().unwrap();
        assert!(!sessions.is_empty(), "本机应有 Codex 会话（未装机时应跳过本测试）");
        let usage = ad.collect_usage(0).unwrap().rows;
        assert!(!usage.is_empty(), "本机应有历史用量");
        assert!(usage.iter().all(|u| u.session_id.starts_with("codex:")));
        // 水位增量：紧接第二次采集应接近空
        let max_ts = usage.iter().map(|u| u.ts).max().unwrap();
        let second = ad.collect_usage(max_ts).unwrap().rows;
        assert!(second.len() <= 5, "水位增量应接近空，实际 {} 行", second.len());
        println!("Codex 会话 {} 个，用量 {} 行；分项汇总见下：", sessions.len(), usage.len());
        let sum = |f: fn(&UsageRow) -> Option<i64>| usage.iter().filter_map(f).sum::<i64>();
        println!("input:  {}", sum(|r| r.input_tokens));
        println!("output: {}", sum(|r| r.output_tokens));
        println!("cache_read: {}", sum(|r| r.cache_read_tokens));
        println!("cache_creation: {}", sum(|r| r.cache_creation_tokens));
    }
}
