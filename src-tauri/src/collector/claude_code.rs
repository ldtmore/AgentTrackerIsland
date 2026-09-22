//! Claude Code 适配器：解析 `~\.claude\projects\**\*.jsonl` 转录文件。
//! 数据源勘察见 docs/01-RESEARCH.md §2；采集策略：
//!   文件级 mtime 过滤（旧文件必无新行）→ **per-file 字节偏移增量读**（2026-09-17
//!   审查 1.3，替代旧的"有变动即整文件重读"）→ 行级时间过滤 → 幂等键去重入库。
//! Claude Code JSONL 的时间戳是 ISO 8601，需转 Unix 毫秒；usage 字段为 snake_case。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use super::engine::{body_after_partial, inject_json_hooks, iso_to_ms, mtime_ms, uninstall_json_hooks, GlobWalker, HotSignal, IncrementalFileReader, ProcessMatch};
use super::{AgentAdapter, CollectOutput, CostSnapshot, SessionInfo, provider_from_model};
use crate::store::UsageRow;

/// 转录根目录（%USERPROFILE%\.claude\projects）
fn projects_root() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")?;
    Some(PathBuf::from(home).join(".claude").join("projects"))
}

/// 单条 assistant 消息的 usage 结构（仅取我们关心的字段，未知字段忽略）
#[derive(serde::Deserialize, Clone)]
struct MessageUsage {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
    cache_creation_input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens_details: Option<OutputDetails>,
}

#[derive(serde::Deserialize, Clone)]
struct OutputDetails {
    /// 思考 token 在 details 里（对应 ZCode 的 reasoning_tokens）
    thinking_tokens: Option<i64>,
}

#[derive(serde::Deserialize, Clone)]
struct MessageBody {
    /// API 消息唯一 id：JSONL 中同一消息会重复出现多行（流式写入/会话恢复），
    /// 必须按它去重，否则统计虚高约 3 倍（与 ccusage 同口径）
    id: Option<String>,
    model: Option<String>,
    usage: Option<MessageUsage>,
}

/// JSONL 行结构（宽松解析，字段缺失即跳过该行）
#[derive(serde::Deserialize)]
struct TranscriptLine {
    #[serde(rename = "type")]
    kind: String,
    message: Option<MessageBody>,
    timestamp: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    /// 请求 id：与 message.id 组成去重键（ccusage 同口径）
    /// 同消息多 requestId = 多次真实 API 调用（重试/恢复重发），各自计消耗
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    /// cost-state 行的会话级累计快照（camelCase 字段，2026-09-21 校准补采）：
    /// 键是模型名（可能带 [1m] 上下文后缀），值是该模型四项用量的会话累计
    #[serde(rename = "modelUsage")]
    model_usage: Option<std::collections::HashMap<String, CostModelUsage>>,
    /// cost-state 行没有 ISO timestamp 字段，只有毫秒数 startTime（实测 2026-09-21）
    #[serde(rename = "startTime")]
    start_time: Option<i64>,
    /// ai-title 行的会话标题（CC 终端里显示的标题；随对话推进多次重写，取最后写入）
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
}

/// cost-state 行 modelUsage 的单模型累计结构（camelCase，与 assistant 行 snake_case 不同）
#[derive(serde::Deserialize, Clone)]
struct CostModelUsage {
    #[serde(rename = "inputTokens", default)]
    input_tokens: i64,
    #[serde(rename = "outputTokens", default)]
    output_tokens: i64,
    #[serde(rename = "cacheReadInputTokens", default)]
    cache_read_input_tokens: i64,
    #[serde(rename = "cacheCreationInputTokens", default)]
    cache_creation_input_tokens: i64,
}

/// 归一化 cost-state 的模型名：去掉 [1m] 等上下文后缀（glm-5.3[1m] → glm-5.3），
/// 与 assistant 行的 message.model 对齐——差值计算按同模型相减才不虚
fn normalize_cost_model(model: &str) -> String {
    model.split('[').next().unwrap_or(model).to_string()
}

pub struct ClaudeCodeAdapter {
    root: PathBuf,
    /// 目录枚举走树器（M2-3 机制迁移至 engine）：`**/*.jsonl` + 目录 mtime 剪枝缓存，
    /// 仅内存态，重启后首轮全量重枚举——与旧实现一致
    walker: Mutex<GlobWalker>,
    /// 单文件增量游标（M2-3 机制迁移至 engine）：mtime 过滤/64KB 回退/重建归零
    reader: Mutex<IncrementalFileReader>,
    /// cwd 提取缓存（审查 2.2.3）：路径 → （mtime_ms， 项目目录）。转录头部 cwd
    /// 恒定，旧实现每 tick 对每文件重读 8KB；缓存后仅 mtime 变化时重读
    cwd_cache: Mutex<HashMap<PathBuf, (i64, Option<String>)>>,
}

impl ClaudeCodeAdapter {
    pub fn new() -> Self {
        Self::with_root(projects_root().unwrap_or_else(|| PathBuf::from("")))
    }

    /// 指定根目录构造（单测注入临时目录用）
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            walker: Mutex::new(GlobWalker::default()),
            reader: Mutex::new(IncrementalFileReader::default()),
            cwd_cache: Mutex::new(HashMap::new()),
        }
    }

    /// 锁中毒自恢复（与 store 同策略，审查 1.1：单次 panic 不放大为连锁失败）
    fn lock_walker(&self) -> MutexGuard<'_, GlobWalker> {
        self.walker.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_reader(&self) -> MutexGuard<'_, IncrementalFileReader> {
        self.reader.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_cwd(&self) -> MutexGuard<'_, HashMap<PathBuf, (i64, Option<String>)>> {
        self.cwd_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 遍历所有转录文件（projects/{项目编码目录}/{sessionId}.jsonl）。
    /// M2-3：枚举改走 GlobWalker（`**/*.jsonl` 递归 + 目录 mtime 剪枝缓存），
    /// 原两级 read_dir 手写循环的行为由模式等价覆盖；根不存在返回空（未装静默降级，红线④）
    fn transcript_files(&self) -> Vec<PathBuf> {
        self.lock_walker().list(&self.root, "**/*.jsonl")
    }

    /// 从转录文件头部提取首个带 cwd 的行，得到真实项目路径（R2）。
    /// 展示与窗口跳转匹配都依赖真实路径（编码目录名无法与窗口标题匹配）；
    /// 头部无 cwd（罕见，如全是 summary 行）时由调用方退回编码目录名。
    /// 读取量 64KB（2026-09-21 实测：8KB 只覆盖 19/43 文件——头部可能连续多行
    /// summary/attachment/ai-title 等不带 cwd 的行；64KB 覆盖 43/43）
    fn first_cwd(path: &std::path::Path) -> Option<String> {
        use std::io::Read;
        let mut f = std::fs::File::open(path).ok()?;
        let mut buf = vec![0u8; 64 * 1024];
        let n = f.read(&mut buf).ok()?;
        let head = String::from_utf8_lossy(&buf[..n]);
        for line in head.lines() {
            let Ok(j) = serde_json::from_str::<serde_json::Value>(line) else {
                continue; // 头部截断的半行等，跳过
            };
            if let Some(cwd) = j.get("cwd").and_then(|c| c.as_str()) {
                if !cwd.is_empty() {
                    return Some(cwd.to_string());
                }
            }
        }
        None
    }
}

impl Default for ClaudeCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentAdapter for ClaudeCodeAdapter {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    /// 快轮信号（M2-1）：①hooks 事件文件（装了增强档：回合起点即时可达，
    /// 从无到有/内容追加都算变化）；②转录树浅枚举（未装 hooks 的降级信号源）
    fn hot_signals(&self) -> Vec<HotSignal> {
        vec![
            HotSignal::File(Arc::new(|| super::hook_events::events_file_path("claude-code"))),
            HotSignal::DirScan {
                root: self.root.clone(),
                ext: Some(".jsonl"),
                depth: 2,
                max_files: 400,
            },
        ]
    }

    /// 进程匹配（M2-4 声明化，自 service.rs probe_processes 注释迁移）：
    /// claude CLI 是 npm shim，真实进程名 node.exe 不含 claude，须兼查命令行；
    /// claude-menu（菜单工具）、本工具自身、hook-bridge（node 进程寿命 ≤2s）均不算，
    /// 防止已退出的会话被误判存活
    fn process_match(&self) -> Option<ProcessMatch> {
        Some(ProcessMatch {
            name_keywords: &["claude"],
            cmd_keywords: &["claude"],
            cmd_excludes: &["claude-menu", "agenttrackerisland", "hook-bridge"],
        })
    }

    /// 会话发现：每个 jsonl 文件即一个会话；最近 90 天有修改的才纳入
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        let cutoff = chrono::Utc::now().timestamp_millis() - 90 * 24 * 3600 * 1000;
        let mut out = vec![];
        for f in self.transcript_files() {
            let mtime = mtime_ms(&f);
            if mtime < cutoff {
                continue;
            }
            let session_id = f.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            if session_id.is_empty() {
                continue;
            }
            // 项目目录：优先转录行内真实 cwd（R2）；取不到退回编码目录名（仅展示兜底）。
            // cwd 恒定，按 mtime 缓存避免每 tick 对每文件重读 8KB 头部（审查 2.2.3）
            let project = {
                let mut map = self.lock_cwd();
                match map.get(&f).cloned() {
                    Some((m0, cached)) if m0 == mtime => cached,
                    _ => {
                        let fresh = Self::first_cwd(&f).or_else(|| {
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
                id: format!("claude-code:{session_id}"),
                agent: "claude-code".into(),
                provider: None, // 由 collect_usage 按实际模型回填，scan 阶段未知
                model: None,
                project_dir: project,
                title: None,
                first_seen_at: mtime,
                last_seen_at: mtime,
                last_usage_at: Some(mtime),
            });
        }
        // 最近修改在前，截断 100（超出截断留痕：多项目用户"少了会话"的排障线索）
        out.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));
        if out.len() > 100 {
            log::debug!("[claude-code] 会话 {} 个，截断保留最近 100", out.len());
        }
        out.truncate(100);
        Ok(out)
    }

    /// 水位增量：跳过 mtime 早于水位的文件；**per-file 字节偏移增量读**（审查 1.3）：
    /// mtime 未变 → 无新字节直接跳过；有变化 → 从（上次偏移 - 64KB 回退量）读到尾，
    /// 只解析新增部分——替代旧"有变动即整文件重读+逐行解析"，长会话文件数十 MB 时
    /// 每 tick 的开销从 O（全文件） 降为 O（新增）。回读与乱序兜底靠调用内去重 +
    /// 自库幂等键，不会重复入库。
    /// 去重键（2026-09-21 双计根治）：message.id(+requestId) 作为 source_id 直接入库，
    /// 自库幂等键 (agent, session_id, source_id) 与此对齐——同消息的流式复制快照行
    /// 即使 timestamp 各异、跨 tick 分裂，也会原地合并而非各自成行（实测双计 +27.8% 的根因）。
    /// 同时解析 cost-state 行的会话级累计快照返回，供 service 层重算后台差值
    fn collect_usage(&self, watermark_ts: i64) -> anyhow::Result<CollectOutput> {
        let mut dedup: std::collections::HashMap<String, UsageRow> = std::collections::HashMap::new();
        // cost 累计快照：会话×归一化模型 → （最大累计， 最新行时间）
        let mut cost: std::collections::HashMap<(String, String), (i64, i64)> = std::collections::HashMap::new();
        // 会话标题（ai-title 行）：后写覆盖=取最新（CC 随对话推进多次重写标题）
        let mut titles: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        // 异常统计（2026-09-17 埋点审查）：本轮结束若有异常汇总一条 debug；
        // 正常轮零输出零噪音，持续出现即"采集源异常"的排障线索
        let (mut err_open, mut bad_lines) = (0usize, 0usize);
        for f in self.transcript_files() {
            if mtime_ms(&f) <= watermark_ts {
                continue; // 文件未变，必无新行
            }
            // 增量读（M2-3 机制迁移至 engine）：mtime 未变 None；打开/读失败计数留痕
            // 后跳过本轮（文件被占用等，下次再试）——与原实现行为一致
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
                .unwrap_or("")
                .to_string();
            // 起点落在半行中间（回退导致）：跳过该残行，从下一个换行起解析（M2-3 助手迁移）
            let Some(body) = body_after_partial(&text, start) else {
                continue; // 无完整新行
            };
            for line in body.lines() {
                let Ok(j) = serde_json::from_str::<TranscriptLine>(line) else {
                    bad_lines += 1;
                    continue; // 容忍坏行（红线：解析失败不阻塞）
                };
                // ---- cost-state 行：会话级累计快照（后台用量校准的数据源） ----
                if j.kind == "cost-state" {
                    let Some(models) = j.model_usage else { continue };
                    // 该行型无 ISO timestamp，回退毫秒 startTime；两者皆缺才跳过
                    let ts = match j.timestamp.as_deref().and_then(iso_to_ms) {
                        Some(t) => t,
                        None => match j.start_time {
                            Some(t) if t > 0 => t,
                            _ => continue,
                        },
                    };
                    if ts <= watermark_ts {
                        continue; // 旧行：累计未变化，免重算
                    }
                    let sid = j.session_id.unwrap_or_else(|| file_session.clone());
                    for (raw_model, u) in models {
                        let cum = u.input_tokens + u.output_tokens
                            + u.cache_read_input_tokens + u.cache_creation_input_tokens;
                        let key = (sid.clone(), normalize_cost_model(&raw_model));
                        // 同一会话同一模型多行累计快照：只保留最大值（会话级总量单调增）
                        cost.entry(key)
                            .and_modify(|e| {
                                if cum > e.0 {
                                    e.0 = cum;
                                    e.1 = ts;
                                }
                            })
                            .or_insert((cum, ts));
                    }
                    continue;
                }
                // ---- ai-title 行：会话标题（无 timestamp 字段，不做水位过滤；
                //      后写覆盖=保留最新标题）----
                if j.kind == "ai-title" {
                    if let Some(t) = j.ai_title {
                        if !t.is_empty() {
                            let sid = j.session_id.unwrap_or_else(|| file_session.clone());
                            titles.insert(format!("claude-code:{sid}"), t);
                        }
                    }
                    continue;
                }
                if j.kind != "assistant" {
                    continue;
                }
                let Some(msg) = j.message else { continue };
                let (Some(model), Some(usage)) = (msg.model.clone(), msg.usage.clone()) else {
                    continue;
                };
                // "<synthetic>" 等本地合成消息非真实模型调用，不进统计
                if model.starts_with('<') {
                    continue;
                }
                // 无 message.id 的行无法去重，防御性跳过（实测数据中不存在）
                let Some(msg_id) = msg.id.clone() else { continue };
                // source_id 与 ccusage/better-ccusage 同口径：messageId+requestId 组合，
                // 缺 requestId 时退化为纯 messageId（实测本机数据无 requestId）
                let source_id = match j.request_id.as_deref() {
                    Some(rid) if !rid.is_empty() => format!("{msg_id}:{rid}"),
                    _ => msg_id,
                };
                let Some(ts) = j.timestamp.as_deref().and_then(iso_to_ms) else { continue };
                if ts <= watermark_ts {
                    continue;
                }
                let sid = j.session_id.unwrap_or_else(|| file_session.clone());
                let provider = provider_from_model(&model);
                let row = UsageRow {
                    session_id: format!("claude-code:{sid}"),
                    agent: "claude-code".into(),
                    model,
                    provider,
                    ts,
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    reasoning_tokens: usage
                        .output_tokens_details
                        .and_then(|d| d.thinking_tokens),
                    cache_read_tokens: usage.cache_read_input_tokens,
                    cache_creation_tokens: usage.cache_creation_input_tokens,
                    duration_ms: None, // JSONL 无时长字段（ZCode 独有）
                    ttft_ms: None,
                    error_type: None,
                    source_id: Some(source_id),
                    is_background: false,
                };
                // 同源消息多行（流式复制快照）：保留用量快照最大者（与文件遍历顺序无关）
                let total = row_total(&row);
                let key = row.source_id.clone().unwrap_or_default();
                dedup.entry(key)
                    .and_modify(|old| {
                        if total > row_total(old) {
                            *old = row.clone();
                        }
                    })
                    .or_insert(row);
            }
        }
        // 本轮异常汇总：坏行持续出现 = Claude Code 升级改了 JSONL 格式或文件损坏，
        // 若无此留痕，用量会"静默归零"（2026-09-17 埋点审查补的最大盲点）。
        // M2-3 起打开/读失败统一由 engine 的 Err 通道返回，计数合并
        if err_open + bad_lines > 0 {
            log::debug!(
                "[claude-code] 采集异常统计：文件打开/读取失败 {err_open}，坏行 {bad_lines}"
            );
        }
        let mut rows: Vec<UsageRow> = dedup.into_values().collect();
        rows.sort_by_key(|r| r.ts);
        let cost_snapshots = cost
            .into_iter()
            .map(|((sid, model), (cum, ts))| CostSnapshot {
                session_id: format!("claude-code:{sid}"),
                model,
                cumulative_tokens: cum,
                ts,
            })
            .collect();
        Ok(CollectOutput {
            rows,
            cost_snapshots,
            titles: titles.into_iter().collect(),
        })
    }
}

/// 行用量四项之和（去重时的比较口径）
fn row_total(r: &UsageRow) -> i64 {
    r.input_tokens.unwrap_or(0)
        + r.output_tokens.unwrap_or(0)
        + r.cache_read_tokens.unwrap_or(0)
        + r.cache_creation_tokens.unwrap_or(0)
}

/// ISO → 毫秒助手已提升至 engine::iso_to_ms（M2-6 Codex 复用同名格式）

/// 当前 Unix 毫秒助手已提升至 engine::now_ms（M2-11 hooks 注入器公共化时迁出）

// ===== hooks 安装/卸载（增强档，设置页一键装卸；02-DESIGN §4） =====

/// 桥脚本源码编译进二进制，安装时写出到家目录（单一已知位置，用户可审计）。
/// M2-6/7 起三家（claude-code/codex/kimi-code）共用同一份脚本（argv 区分 agent）
pub(crate) const BRIDGE_SOURCE: &str = include_str!("../../hook-bridge/hook-bridge.js");
/// 注入标记（卸载时按此识别自家条目）
const BRIDGE_MARK: &str = "hook-bridge.js";
/// 覆盖状态机全部迁移的事件清单
const HOOK_EVENTS: &[&str] = &[
    "SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse",
    "Notification", "Stop", "SessionEnd",
];

fn claude_settings_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("USERPROFILE")?).join(".claude").join("settings.json"))
}

fn bridge_script_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("USERPROFILE")?).join(".claude").join("hooks").join("hook-bridge.js"))
}

/// 查询 hooks 是否已安装（settings.json 中存在自家注入条目）
pub fn hooks_installed() -> bool {
    let Some(path) = claude_settings_path() else { return false };
    if !path.exists() {
        return false;
    }
    std::fs::read_to_string(&path)
        .map(|raw| raw.contains(BRIDGE_MARK))
        .unwrap_or(false)
}

/// 安装：①桥脚本写出到 ~\.claude\hooks\hook-bridge.js；
/// ②settings.json 备份后合并注入 7 事件（防重复）；返回注入条数
pub fn install_hooks() -> anyhow::Result<usize> {
    let bridge = bridge_script_path().ok_or_else(|| anyhow::anyhow!("无法定位用户目录"))?;
    if let Some(dir) = bridge.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&bridge, BRIDGE_SOURCE)?;
    let settings = claude_settings_path().ok_or_else(|| anyhow::anyhow!("无法定位 settings.json"))?;
    // 第二参数是 Agent 标识：桥脚本按它把事件写入各自的 events/<agent>.jsonl
    // （M2-6/7 桥脚本参数化，一份脚本服务多家；缺省参数即 claude-code，行为不变）
    let cmd = format!(
        "node \"{}\" {}",
        bridge.to_string_lossy().replace('\\', "/"),
        "claude-code"
    );
    let injected = engine_inject(&settings, &cmd)?;
    log::info!("hooks 安装完成：注入 {injected} 个事件，桥脚本 {}", bridge.display());
    Ok(injected)
}

/// 卸载：移除全部自家注入条目（含空事件键清理）；返回移除条数。
/// 桥脚本文件保留（重装免复制，且无副作用）
pub fn uninstall_hooks() -> anyhow::Result<usize> {
    let settings = claude_settings_path().ok_or_else(|| anyhow::anyhow!("无法定位 settings.json"))?;
    let removed = uninstall_json_hooks(&settings, BRIDGE_MARK)?;
    log::info!("hooks 卸载完成：移除 {removed} 个注入条目");
    Ok(removed)
}

/// 组装注入条目并调公共注入器（M2-11 起机制迁入 engine，三家共用）：
/// CC 的 timeout 单位秒、支持 async——桥脚本后台写事件文件零阻塞
fn engine_inject(path: &std::path::Path, command: &str) -> anyhow::Result<usize> {
    let entry = serde_json::json!({
        "hooks": [{ "type": "command", "command": command, "timeout": 10, "async": true }]
    });
    inject_json_hooks(path, HOOK_EVENTS, entry, BRIDGE_MARK)
}

/// 原子写/备份清理/注入核心/卸载核心（M2-11 迁入 engine.rs 公共化，三家共用：
/// gemini/qwen-code 的 settings.json 同构；行为由 engine 单测与下方往返测试共同锁定）

#[cfg(test)]
mod tests {
    use super::*;

    /// hooks 注入/卸载往返：临时 settings 文件，验证防重复与完整还原
    #[test]
    fn test_hooks_install_uninstall_roundtrip() {
        let dir = std::env::temp_dir().join(format!("at-t6-hooks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        // 模拟用户已有 hooks（与真实文件同构）+ 其他配置段
        std::fs::write(&settings, r#"{
          "statusLine": {"type": "command"},
          "permissions": {"defaultMode": "auto"},
          "hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "powershell.exe -File notify.ps1"}]}]
          }
        }"#).unwrap();

        // 注入：7 个事件（Stop 已存在→追加不覆盖）；M2-11 起走 engine 公共注入器
        let cmd = "node \"C:/x/.claude/hooks/hook-bridge.js\" claude-code";
        let entry = serde_json::json!({
            "hooks": [{ "type": "command", "command": cmd, "timeout": 10, "async": true }]
        });
        let n = super::super::engine::inject_json_hooks(&settings, HOOK_EVENTS, entry, BRIDGE_MARK).unwrap();
        assert_eq!(n, 7);
        let s1: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(s1["hooks"]["Stop"].as_array().unwrap().len(), 2, "Stop 应追加而非覆盖");
        assert_eq!(s1["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(s1["statusLine"]["type"], "command", "其他配置不受影响");

        // 重复注入：防重复，0 条
        let entry2 = serde_json::json!({
            "hooks": [{ "type": "command", "command": cmd, "timeout": 10, "async": true }]
        });
        let n2 = super::super::engine::inject_json_hooks(&settings, HOOK_EVENTS, entry2, BRIDGE_MARK).unwrap();
        assert_eq!(n2, 0);

        // 卸载：回到与原文件等价（自家条目全清，用户 hooks 原样保留）
        let removed = super::super::engine::uninstall_json_hooks(&settings, BRIDGE_MARK).unwrap();
        assert_eq!(removed, 7);
        let s2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(s2["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert!(s2["hooks"].as_object().unwrap().contains_key("Stop"));
        assert!(!s2["hooks"].as_object().unwrap().contains_key("PreToolUse"));
        assert_eq!(s2["statusLine"]["type"], "command");
        // 备份文件存在
        assert!(dir.read_dir().unwrap().any(|f| f.unwrap().file_name().to_string_lossy().starts_with("settings.json.bak-at-")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R2：转录头部 cwd 提取（真实路径；含 summary 行/截断半行容错）
    #[test]
    fn test_first_cwd() {
        let dir = std::env::temp_dir().join(format!("at-r2-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.jsonl");
        std::fs::write(&file, concat!(
            r#"{"type":"summary","summary":"无 cwd 的行应跳过"}"#, "\n",
            r#"{"type":"user","cwd":"F:\\MyProjectRepository\\AgentTrackerIsland","timestamp":"2026-09-16T10:00:00.000Z"}"#, "\n",
            r#"{"type":"user","cwd":"F:\\另一个目录不应被选中"}"#, "\n"
        )).unwrap();
        assert_eq!(
            ClaudeCodeAdapter::first_cwd(&file).as_deref(),
            Some("F:\\MyProjectRepository\\AgentTrackerIsland")
        );
        // 全部无 cwd：None（调用方退回编码目录名）
        let f2 = dir.join("empty.jsonl");
        std::fs::write(&f2, r#"{"type":"summary"}"#).unwrap();
        assert_eq!(ClaudeCodeAdapter::first_cwd(&f2), None);
        // 文件不存在：None
        assert_eq!(ClaudeCodeAdapter::first_cwd(&dir.join("nope.jsonl")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端到端（手动：cargo test -- --ignored test_real_hooks_e2e）：
    /// 安装 hook-bridge → headless 触发真实 hook 链 → 事件文件落盘 → Rust 消费 → 卸载还原
    #[test]
    #[ignore]
    fn test_real_hooks_e2e() {
        // Drop 守卫：测试 panic 也会执行卸载，杜绝注入残留
        struct HooksGuard;
        impl Drop for HooksGuard {
            fn drop(&mut self) {
                let _ = uninstall_hooks();
            }
        }
        let _guard = HooksGuard;
        // 幂等清理可能的历史残留
        let _ = uninstall_hooks();

        let settings = claude_settings_path().unwrap();
        let before = std::fs::read_to_string(&settings).unwrap();

        // 1) 安装
        let n = install_hooks().unwrap();
        assert!(n >= 1, "至少注入 1 个事件");

        // 2) headless 触发（无凭据也会走 SessionStart/UserPromptSubmit/SessionEnd）
        // Windows 上 claude 是 npm 的 .cmd shim，须经 cmd /c 调用（Rust 不走 PATHEXT）
        let out = std::process::Command::new("cmd")
            .args(["/c", "claude", "-p", "ok"])
            .current_dir(dirs_home())
            .output();
        assert!(out.is_ok(), "claude CLI 应可执行");
        let stderr = String::from_utf8_lossy(&out.as_ref().unwrap().stderr);
        println!("claude stderr: {}", stderr.lines().take(2).collect::<Vec<_>>().join(" | "));
        // async hook 后台写入，给足落盘时间
        std::thread::sleep(std::time::Duration::from_secs(3));

        // 3) 事件文件落盘且可消费
        let evfile = crate::collector::hook_events::events_file_path("claude-code").unwrap();
        let (events, offset) = crate::collector::hook_events::read_events(&evfile, 0).unwrap();
        let fresh: Vec<_> = events.iter().filter(|e| e.session_id != "manual-test").collect();
        println!("捕获事件（{} 条，偏移 {}）：{:?}", fresh.len(), offset,
            fresh.iter().map(|e| e.hook.as_str()).collect::<Vec<_>>());
        assert!(!fresh.is_empty(), "事件文件应有真实 hook 记录");
        assert!(fresh.iter().all(|e| !e.session_id.is_empty()), "事件应带 session_id");

        // 4) 卸载后 settings 完整还原（Drop 守卫兜底，此处显式验证）
        let removed = uninstall_hooks().unwrap();
        assert!(removed >= 1);
        let after: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        let before_v: serde_json::Value = serde_json::from_str(&before).unwrap();
        assert_eq!(after, before_v, "卸载后 settings.json 应与安装前语义等价");
    }

    fn dirs_home() -> std::path::PathBuf {
        std::path::PathBuf::from(std::env::var_os("USERPROFILE").unwrap())
    }

    /// 纯单测：构造临时转录文件验证解析/过滤/幂等键输入
    #[test]
    fn test_parse_transcript_lines() {
        let dir = std::env::temp_dir().join(format!("at-t4-{}", std::process::id()));
        let proj = dir.join("F--AgentTrackerIsland-test");
        std::fs::create_dir_all(&proj).unwrap();
        let file = proj.join("sess-test-0001.jsonl");
        let lines = [
            // 普通用户行：应被忽略
            r#"{"type":"user","timestamp":"2026-09-15T10:00:00.000Z","sessionId":"sess-test-0001"}"#,
            // assistant 行：有效，glm 模型
            r#"{"type":"assistant","timestamp":"2026-09-15T10:00:01.000Z","sessionId":"sess-test-0001","message":{"id":"msg_a","model":"glm-5.3","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":200,"cache_creation_input_tokens":10,"output_tokens_details":{"thinking_tokens":5}}}}"#,
            // 同 message.id 的重复行（流式中途快照，用量更小）：应被去重且保留大快照
            r#"{"type":"assistant","timestamp":"2026-09-15T10:00:01.000Z","sessionId":"sess-test-0001","message":{"id":"msg_a","model":"glm-5.3","usage":{"input_tokens":40,"output_tokens":20,"cache_read_input_tokens":80}}}"#,
            // 另一条独立消息
            r#"{"type":"assistant","timestamp":"2026-09-15T10:00:03.000Z","sessionId":"sess-test-0001","message":{"id":"msg_b","model":"glm-5.3","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0}}}"#,
            // 缺 usage 的 assistant 行：忽略
            r#"{"type":"assistant","timestamp":"2026-09-15T10:00:02.000Z","message":{"model":"glm-5.3"}}"#,
            // 坏 JSON 行：忽略
            r#"{broken"#,
        ];
        std::fs::write(&file, lines.join("\n")).unwrap();

        let ad = ClaudeCodeAdapter::with_root(dir.clone());
        let rows = ad.collect_usage(0).unwrap().rows;
        // msg_a 去重后保留大快照 + msg_b：共 2 行
        assert_eq!(rows.len(), 2, "同 message.id 必须去重");
        assert!(rows.iter().all(|r| r.session_id == "claude-code:sess-test-0001"));
        assert!(rows.iter().all(|r| r.agent == "claude-code" && r.provider.as_deref() == Some("glm")));
        let big = rows.iter().find(|r| r.input_tokens == Some(100)).expect("应保留用量大的快照");
        assert_eq!(big.output_tokens, Some(50));
        assert_eq!(big.reasoning_tokens, Some(5));
        assert!(rows.iter().all(|r| r.ts > 1_700_000_000_000));

        // 时间水位：全部行早于该水位 → 0 行
        let rows2 = ad.collect_usage(1_800_000_000_000_000).unwrap().rows;
        assert!(rows2.is_empty());
        // 但注意：该临时文件 mtime 是"现在"，> 大水位？不——水位比较用文件 mtime <= watermark 跳过，
        // 1.8e15 是远未来，mtime（现在）< 水位 → 文件被跳过，结果一致为空，验证文件级过滤也生效
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ai-title 行标题提取（2026-09-21）：提取标题、后写覆盖取最新、
    /// 无 timestamp 字段不受水位过滤影响
    #[test]
    fn test_collect_ai_titles() {
        let dir = std::env::temp_dir().join(format!("at-t12-title-{}", std::process::id()));
        let proj = dir.join("F--title-test");
        std::fs::create_dir_all(&proj).unwrap();
        let file = proj.join("sess-title.jsonl");
        let lines = [
            r#"{"type":"user","timestamp":"2026-09-20T10:00:00.000Z","sessionId":"sess-title"}"#,
            r#"{"type":"ai-title","aiTitle":"初始标题","sessionId":"sess-title"}"#,
            r#"{"type":"assistant","timestamp":"2026-09-20T10:00:01.000Z","sessionId":"sess-title","message":{"id":"msg_t1","model":"glm-5.3","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            // 标题随对话推进被 CC 重写：后写覆盖 = 取最新
            r#"{"type":"ai-title","aiTitle":"更新后的标题","sessionId":"sess-title"}"#,
            // 空标题行：忽略
            r#"{"type":"ai-title","aiTitle":"","sessionId":"sess-title"}"#,
        ];
        std::fs::write(&file, lines.join("\n")).unwrap();

        let ad = ClaudeCodeAdapter::with_root(dir.clone());
        let out = ad.collect_usage(0).unwrap();
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.titles.len(), 1, "同会话多行标题应合并");
        assert_eq!(
            out.titles[0],
            ("claude-code:sess-title".to_string(), "更新后的标题".to_string()),
            "应取最后写入的标题"
        );
        // 水位推进后：无新行 → titles 也为空（服务层靠库 COALESCE 保留旧标题）
        let out2 = ad.collect_usage(0).unwrap();
        assert!(out2.titles.is_empty(), "文件未变时不应重复产出标题");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 增量读回归（审查 1.3）：同实例重复采集，第二次只产出新追加的行；
    /// 未变化时产 0 行。旧实现整文件重读也能得到相同"结果"（靠水位过滤），
    /// 此测试锁定增量路径行为不回退
    #[test]
    fn test_collect_incremental_offset() {
        let dir = std::env::temp_dir().join(format!("at-t4-incr-{}", std::process::id()));
        let proj = dir.join("F--incr-test");
        std::fs::create_dir_all(&proj).unwrap();
        let file = proj.join("sess-incr.jsonl");
        let line1 = r#"{"type":"assistant","timestamp":"2026-09-15T10:00:01.000Z","sessionId":"sess-incr","message":{"id":"msg_1","model":"glm-5.3","usage":{"input_tokens":10,"output_tokens":5}}}"#;
        std::fs::write(&file, format!("{line1}\n")).unwrap();

        let ad = ClaudeCodeAdapter::with_root(dir.clone());
        let r1 = ad.collect_usage(0).unwrap().rows;
        assert_eq!(r1.len(), 1);
        let ts1 = chrono::DateTime::parse_from_rfc3339("2026-09-15T10:00:01.000Z")
            .unwrap()
            .timestamp_millis();

        // 追加第二条消息 → 旧行被水位过滤，只产出新增 1 行（与 service 层调用一致）
        let line2 = r#"{"type":"assistant","timestamp":"2026-09-15T10:00:02.000Z","sessionId":"sess-incr","message":{"id":"msg_2","model":"glm-5.3","usage":{"input_tokens":20,"output_tokens":8}}}"#;
        std::fs::write(&file, format!("{line1}\n{line2}\n")).unwrap();
        let r2 = ad.collect_usage(ts1).unwrap().rows;
        assert_eq!(r2.len(), 1, "第二次采集应只含新增行");
        assert_eq!(r2[0].input_tokens, Some(20));

        // 文件未变化 → 0 行
        let r3 = ad.collect_usage(0).unwrap().rows;
        assert!(r3.is_empty(), "mtime 未变时应跳过文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 集成：本机真实转录库（手动：cargo test -- --ignored）
    #[test]
    #[ignore]
    fn test_real_cc_collect() {
        let ad = ClaudeCodeAdapter::new();
        let sessions = ad.scan_sessions().unwrap();
        assert!(!sessions.is_empty(), "本机应有活跃 Claude Code 会话");
        let out = ad.collect_usage(0).unwrap();
        let usage = &out.rows;
        assert!(!usage.is_empty(), "本机应有历史用量");
        assert!(usage.iter().all(|u| u.session_id.starts_with("claude-code:")));
        assert!(usage
            .iter()
            .all(|u| u.model.to_ascii_lowercase().starts_with("glm")));
        // 水位增量：紧接的第二次采集应接近空（容忍正在写入的新消息，避免竞态误报）
        let max_ts = usage.iter().map(|u| u.ts).max().unwrap();
        let second = ad.collect_usage(max_ts).unwrap().rows;
        assert!(second.len() <= 5, "水位增量应接近空，实际 {} 行（增长中的会话）", second.len());
        // cost-state 快照：本机真实数据应有非空累计（后台校准数据源）
        assert!(!out.cost_snapshots.is_empty(), "本机应有 cost-state 累计快照");
        assert!(out.cost_snapshots.iter().all(|c| c.session_id.starts_with("claude-code:")));
        // ai-title 标题：本机真实转录应有标题（CC 终端会话标题同源）
        assert!(!out.titles.is_empty(), "本机应有 ai-title 会话标题");
        assert!(out.titles.iter().all(|(sid, t)| sid.starts_with("claude-code:") && !t.is_empty()));
    }

    /// A2 对账：全量分项汇总打印，与 `npx ccusage` 输出人工比对
    /// （手动：cargo test -- --ignored test_real_cc_reconcile -- --nocapture）
    #[test]
    #[ignore]
    fn test_real_cc_reconcile_totals() {
        let ad = ClaudeCodeAdapter::new();
        let usage = ad.collect_usage(0).unwrap().rows;
        let sum = |f: fn(&UsageRow) -> Option<i64>| usage.iter().filter_map(f).sum::<i64>();
        println!("行数（assistant 消息）：{}", usage.len());
        println!("input:  {}", sum(|r| r.input_tokens));
        println!("output: {}", sum(|r| r.output_tokens));
        println!("cache_read: {}", sum(|r| r.cache_read_tokens));
        println!("cache_creation: {}", sum(|r| r.cache_creation_tokens));
        println!(
            "total:  {}",
            sum(|r| r.input_tokens) + sum(|r| r.output_tokens)
                + sum(|r| r.cache_read_tokens) + sum(|r| r.cache_creation_tokens)
        );
    }
}
