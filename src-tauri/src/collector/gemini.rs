//! Gemini CLI 适配器（M2-11）：tail `~/.gemini/tmp/<项目slug>/chats/session-*.jsonl` 转录。
//! 调研依据：01-RESEARCH §13（google-gemini/gemini-cli v0.60.0 源码级核实，2026-09-23）。
//! 要点：
//! - 会话文件已 JSONL 化（旧单 JSON 由 CLI 自动迁移，目录名由 sha256 hash 改为
//!   projects.json 注册表的 slug）：首行 metadata ＋消息行＋`$set`/`$rewindTo` 控制行；
//!   追加写入，但 token 元数据后到时**同 id 消息整条重 append**——靠 `gm:` 幂等键
//!   ＋库层 UPSERT 后写覆盖（store 0003 语义），适配器内同轮保留用量大者（与 CC 同策略）。
//! - token 口径：官方 /stats 明确 input = promptTokenCount − cachedContentTokenCount
//!   （cached ⊆ prompt，OpenAI 语义）；转录 tokens.input 为 prompt 原值，入库前拆分。
//! - error 消息行（type:"error"）→ UsageRow.error_type 走 recent_error 链路。
//! - hooks：11 事件 CC 式（settings.json `hooks` 键），注入器走 engine 公共 JSON
//!   注入器；⚠️ Gemini 无 async 字段（同步执行 hook），timeout 单位为毫秒。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use super::engine::{
    body_after_partial, inject_json_hooks, iso_to_ms, mtime_ms, uninstall_json_hooks, GlobWalker,
    HotSignal, IncrementalFileReader, ProcessMatch,
};
use super::otel::{self, OtelOutfileSink};
use super::{AgentAdapter, CollectOutput, SessionInfo, provider_from_model};
use crate::store::UsageRow;

/// 数据根（GEMINI_CLI_HOME 重定向整个 ~/.gemini；校验目录存在，异常回落默认并留痕）
fn gemini_home() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("GEMINI_CLI_HOME") {
        let p = PathBuf::from(v);
        if p.is_dir() {
            return Some(p);
        }
        log::debug!("[gemini] GEMINI_CLI_HOME 指向的目录不存在，回落默认路径：{}", p.display());
    }
    Some(PathBuf::from(std::env::var_os("USERPROFILE")?).join(".gemini"))
}

/// 转录头部元数据（首行 metadata ＋头内首条 user 消息；三者一经写入永不变，
/// 缓存一次永续——summary 的动态更新走 $set 行的 titles 通道，不依赖头部）
#[derive(Debug, Clone)]
struct HeadMeta {
    /// 会话 id（首行 metadata.sessionId；文件名只有 id 前 8 位，不足为凭）
    session_id: String,
    /// 项目目录（metadata.directories 首项）
    project_dir: Option<String>,
    /// 标题兜底：头内首条 user 消息文本（官方 loadConversationRecord 同策略）
    title: Option<String>,
}

pub struct GeminiAdapter {
    root: PathBuf,
    /// 目录枚举走树器（`tmp/*/chats/session-*.jsonl`：slug 与旧 hash 目录同构；
    /// 子代理嵌套在 chats/<父id>/ 下两层，pattern 天然排除——是否补采留装机核实）
    walker: Mutex<GlobWalker>,
    /// 单文件增量游标（mtime 过滤/64KB 回退/重建归零，引擎机制）
    reader: Mutex<IncrementalFileReader>,
    /// 头部元数据缓存：路径 → HeadMeta（内容永不变，不随 mtime 失效）
    head_cache: Mutex<HashMap<PathBuf, Option<HeadMeta>>>,
    /// per-file 最近模型缓存：模型随消息行更新，增量读时旧行不在本轮增量内
    model_cache: Mutex<HashMap<PathBuf, String>>,
    /// OTel outfile 增强通道（M2-12，见 collector/otel.rs 模块注释；token 行暂不入库）
    otel: OtelOutfileSink,
}

impl GeminiAdapter {
    pub fn new() -> Self {
        // home 定位失败同样落空路径：settings.json 永远发现不到，outfile 通道静默降级
        Self::with_root(gemini_home().unwrap_or_else(|| PathBuf::from("")))
    }

    /// 指定根目录构造（单测注入临时目录用）
    pub fn with_root(root: PathBuf) -> Self {
        // OTel outfile 通道的 settings 路径跟随注入根目录（单测注入语义；
        // sink 需在 root 被 move 前构造）
        let sink = OtelOutfileSink::new(otel::GEMINI_PROFILE, root.join("settings.json"));
        Self {
            root,
            walker: Mutex::new(GlobWalker::default()),
            reader: Mutex::new(IncrementalFileReader::default()),
            head_cache: Mutex::new(HashMap::new()),
            model_cache: Mutex::new(HashMap::new()),
            otel: sink,
        }
    }

    fn lock_walker(&self) -> MutexGuard<'_, GlobWalker> {
        self.walker.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_reader(&self) -> MutexGuard<'_, IncrementalFileReader> {
        self.reader.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_head(&self) -> MutexGuard<'_, HashMap<PathBuf, Option<HeadMeta>>> {
        self.head_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_model(&self) -> MutexGuard<'_, HashMap<PathBuf, String>> {
        self.model_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn transcript_files(&self) -> Vec<PathBuf> {
        self.lock_walker().list(&self.root, "tmp/*/chats/session-*.jsonl")
    }

    /// 头部元数据（64KB 头读，缓存永续；解析不出时返回 None 且不缓存，下轮重试）
    fn head_meta(&self, path: &Path) -> Option<HeadMeta> {
        let hit = self.lock_head().get(path).cloned();
        if let Some(cached) = hit {
            return cached;
        }
        use std::io::Read;
        let mut f = std::fs::File::open(path).ok()?;
        let mut buf = vec![0u8; 64 * 1024];
        let n = f.read(&mut buf).ok()?;
        let head = String::from_utf8_lossy(&buf[..n]);
        let mut session_id = String::new();
        let mut project_dir = None;
        let mut title = None;
        for line in head.lines() {
            let Ok(j) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            // 首行 metadata：含 sessionId＋directories（消息行/控制行均无此组合）
            if session_id.is_empty() {
                if let (Some(sid), Some(dirs)) = (
                    j.get("sessionId").and_then(|v| v.as_str()),
                    j.get("directories").and_then(|v| v.as_array()),
                ) {
                    session_id = sid.to_string();
                    project_dir = dirs
                        .first()
                        .and_then(|d| d.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string());
                    continue;
                }
            }
            // 头内首条 user 消息：displayContent 优先（用户可见文本），
            // content 是字符串时兜底（PartListUnion 数组形态取首个 text 分片）
            if title.is_none() && j.get("type").and_then(|t| t.as_str()) == Some("user") {
                let text = j
                    .get("displayContent")
                    .and_then(text_of)
                    .or_else(|| j.get("content").and_then(text_of));
                title = text.filter(|s| !s.is_empty()).map(|s| s.chars().take(60).collect());
            }
            if !session_id.is_empty() && title.is_some() {
                break;
            }
        }
        if session_id.is_empty() {
            // 头部读不出会话 id（文件尚未写完首行等）：不缓存，下轮重试
            return None;
        }
        let meta = HeadMeta { session_id, project_dir, title };
        self.lock_head().insert(path.to_path_buf(), Some(meta.clone()));
        Some(meta)
    }
}

impl Default for GeminiAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// 从 JSON 值提取文本：字符串原样；part 数组取首个 text 分片（PartListUnion 宽松形态）
fn text_of(v: &serde_json::Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    v.as_array().and_then(|arr| {
        arr.iter().find_map(|p| p.get("text").and_then(|t| t.as_str())).map(|s| s.to_string())
    })
}

/// 转录行宽松解析（字段缺失容忍格式漂移；$set/$rewindTo 控制行同 struct 承载）
#[derive(serde::Deserialize)]
struct GeminiLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    timestamp: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tokens: Option<serde_json::Value>,
    #[serde(rename = "displayContent", default)]
    display: Option<serde_json::Value>,
    #[serde(default)]
    content: Option<serde_json::Value>,
    #[serde(rename = "$set", default)]
    set: Option<serde_json::Value>,
    #[serde(rename = "$rewindTo", default)]
    rewind_to: Option<String>,
}

/// tokens 落盘形态 {input,output,cached,thoughts?,tool?,total} 拆分四项：
/// input 为 prompt 原值（含 cached 子集）——按官方 /stats 口径 input −= cached；
/// input < cached 视为分立语义照抄不动（与 Codex 适配器同一防御）
fn split_tokens(t: &serde_json::Value) -> (i64, i64, i64, i64) {
    let g = |k: &str| t.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let prompt = g("input");
    let cached = g("cached");
    let (input, cache_read) = if prompt >= cached { (prompt - cached, cached) } else { (prompt, cached) };
    (input, g("output"), g("thoughts"), cache_read)
}

/// 行用量四项之和（同轮去重时的比较口径）
fn row_total(r: &UsageRow) -> i64 {
    r.input_tokens.unwrap_or(0)
        + r.output_tokens.unwrap_or(0)
        + r.cache_read_tokens.unwrap_or(0)
        + r.cache_creation_tokens.unwrap_or(0)
}

impl AgentAdapter for GeminiAdapter {
    fn id(&self) -> &'static str {
        "gemini"
    }

    /// 快轮信号：chats 目录浅枚举（转录追加即活动）＋hooks 事件文件（装了才有）
    fn hot_signals(&self) -> Vec<HotSignal> {
        vec![
            HotSignal::File(std::sync::Arc::new(|| {
                super::hook_events::events_file_path("gemini")
            })),
            HotSignal::DirScan {
                root: self.root.join("tmp"),
                ext: Some(".jsonl"),
                depth: 3,
                max_files: 400,
            },
            // OTel outfile 快轮信号：配置了 outfile 才有信号（未配置静默）
            self.otel.hot_signal(),
        ]
    }

    /// 进程匹配：npm 包经 node shim 运行（进程名无特征 node.exe，只按命令行
    /// 命中包路径）；本工具自身排除，防误判存活
    fn process_match(&self) -> Option<ProcessMatch> {
        Some(ProcessMatch {
            name_keywords: &[],
            cmd_keywords: &["gemini-cli"],
            cmd_excludes: &["agenttrackerisland"],
        })
    }

    /// 会话发现：每个 session-*.jsonl 即一个会话；最近 90 天有修改的才纳入
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        let cutoff = chrono::Utc::now().timestamp_millis() - 90 * 24 * 3600 * 1000;
        let mut out = vec![];
        for f in self.transcript_files() {
            let mtime = mtime_ms(&f);
            if mtime < cutoff {
                continue;
            }
            let Some(meta) = self.head_meta(&f) else {
                continue;
            };
            out.push(SessionInfo {
                id: format!("gemini:{}", meta.session_id),
                agent: "gemini".into(),
                provider: None, // collect 阶段按模型回填（gemini* → google）
                model: None,
                project_dir: meta.project_dir,
                // 首条 user 消息兜底；官方 summary（$set 行）经 titles 通道动态覆盖
                title: meta.title,
                first_seen_at: mtime,
                last_seen_at: mtime,
                last_usage_at: Some(mtime),
            });
        }
        out.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));
        if out.len() > 100 {
            log::debug!("[gemini] 会话 {} 个，截断保留最近 100", out.len());
        }
        out.truncate(100);
        Ok(out)
    }

    /// 水位增量采集：
    ///   消息行（type=gemini 且 tokens）key = "gm:{消息id}"（同 id 重 append 靠
    ///   幂等键＋库层 UPSERT 后写覆盖；同轮增量保留用量大者）；
    ///   error 行只携带 error_type（token 全缺），走 recent_error 链路；
    ///   $set 行只取 summary 作标题（忽略 messages 全量数组——内存与隐私双保险）
    fn collect_usage(&self, watermark_ts: i64) -> anyhow::Result<CollectOutput> {
        // 同轮内按幂等键合并（同 id 重 append/增量回退重读：保留用量大者）
        let mut rows: HashMap<String, UsageRow> = HashMap::new();
        let mut titles: HashMap<String, String> = HashMap::new();
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
            // 会话 id（头部缓存命中则零成本；新文件首读头部 64KB）
            let Some(head) = self.head_meta(&f) else {
                continue;
            };
            let namespaced = format!("gemini:{}", head.session_id);
            let Some(body) = body_after_partial(&text, start) else {
                continue;
            };
            // 本轮增量内最近的模型名（跨轮由缓存承接，同 Codex turn_context 策略）
            let mut model = self.lock_model().get(&f).cloned().unwrap_or_default();
            for line in body.lines() {
                let Ok(j) = serde_json::from_str::<GeminiLine>(line) else {
                    bad_lines += 1;
                    continue;
                };
                // $set 控制行：标题（summary）后写覆盖；messages 全量重建忽略
                if let Some(summary) = j.set.as_ref().and_then(|s| s.get("summary")).and_then(|v| v.as_str()) {
                    if !summary.is_empty() {
                        titles.insert(namespaced.clone(), summary.to_string());
                    }
                    continue;
                }
                if j.rewind_to.is_some() {
                    continue; // 回滚标记：低频操作，历史用量行不追溯删除（装机对账评估）
                }
                let kind = j.kind.as_deref().unwrap_or_default();
                // 用量行：gemini 回复且带 tokens
                if kind == "gemini" {
                    if let Some(m) = j.model.as_deref() {
                        if !m.is_empty() {
                            model = m.to_string();
                        }
                    }
                    let Some(tokens) = j.tokens.as_ref() else {
                        continue; // 流式中间态/无计量消息：忽略
                    };
                    let Some(ts) = j.timestamp.as_deref().and_then(iso_to_ms) else {
                        continue;
                    };
                    if ts <= watermark_ts {
                        continue;
                    }
                    let (input, output, reasoning, cache_read) = split_tokens(tokens);
                    // 幂等键：官方消息 id；缺失（格式漂移）时按文件＋毫秒指纹兜底
                    let source = match j.id.as_deref() {
                        Some(id) if !id.is_empty() => format!("gm:{id}"),
                        _ => format!("gm:{}:{}", head.session_id, ts),
                    };
                    let row = UsageRow {
                        session_id: namespaced.clone(),
                        agent: "gemini".into(),
                        model: model.clone(),
                        provider: provider_from_model(&model),
                        ts,
                        input_tokens: Some(input),
                        output_tokens: Some(output),
                        reasoning_tokens: Some(reasoning),
                        cache_read_tokens: Some(cache_read),
                        cache_creation_tokens: Some(0),
                        duration_ms: None,
                        ttft_ms: None,
                        error_type: None,
                        source_id: Some(source),
                        is_background: false,
                    };
                    merge_row(&mut rows, row);
                    continue;
                }
                // error 行：本地报错/上游错误消息（无 token），喂 recent_error 链路
                if kind == "error" {
                    let Some(ts) = j.timestamp.as_deref().and_then(iso_to_ms) else {
                        continue;
                    };
                    if ts <= watermark_ts {
                        continue;
                    }
                    let text = j
                        .display
                        .as_ref()
                        .and_then(text_of)
                        .or_else(|| j.content.as_ref().and_then(text_of))
                        .unwrap_or_else(|| "unknown".into());
                    let source = match j.id.as_deref() {
                        Some(id) if !id.is_empty() => format!("gm:{id}"),
                        _ => format!("gm:{}:{}", head.session_id, ts),
                    };
                    let row = UsageRow {
                        session_id: namespaced.clone(),
                        agent: "gemini".into(),
                        model: model.clone(),
                        provider: None,
                        ts,
                        input_tokens: None,
                        output_tokens: None,
                        reasoning_tokens: None,
                        cache_read_tokens: None,
                        cache_creation_tokens: None,
                        duration_ms: None,
                        ttft_ms: None,
                        error_type: Some(text.chars().take(120).collect()),
                        source_id: Some(source),
                        is_background: false,
                    };
                    merge_row(&mut rows, row);
                }
            }
            self.lock_model().insert(f.clone(), model);
        }
        if err_open + bad_lines > 0 {
            log::debug!(
                "[gemini] 采集异常统计：文件打开/读取失败 {err_open}，坏行 {bad_lines}（静默容忍）"
            );
        }
        // OTel outfile 通道（M2-12）：api_error 错误信号行并入本轮去重（与转录 error 行
        // 不同 source 空间，同次错误可能两行——token 全 None，只影响 recent_error 展示无实害）；
        // token 行按所有者裁定（2026-09-23）暂不入库：与转录通道同回合无公共 id 可对齐，
        // 入库必双计，装机对账后若切换主通道在此处把 batch.rows 一并并入即可
        let batch = self.otel.collect();
        for e in batch.errors {
            merge_row(&mut rows, e);
        }
        if !batch.rows.is_empty() {
            log::debug!("[gemini] otel outfile：token 行 {}（暂不入库）", batch.rows.len());
        }
        let mut rows: Vec<UsageRow> = rows.into_values().collect();
        rows.sort_by_key(|r| r.ts);
        Ok(CollectOutput {
            rows,
            titles: titles.into_iter().collect(),
            ..CollectOutput::default()
        })
    }
}

/// 同幂等键合并：保留四项用量之和更大者（同 id 重 append 的后行通常更完整，
/// 与 CC/Codex 的去重口径一致，乱序稳健）
fn merge_row(rows: &mut HashMap<String, UsageRow>, row: UsageRow) {
    let total = row_total(&row);
    let source = row.source_id.clone().unwrap_or_default();
    rows.entry(source)
        .and_modify(|old| {
            if total > row_total(old) {
                *old = row.clone();
            }
        })
        .or_insert(row);
}

// ===== hooks 安装/卸载（增强档；settings.json JSON 注入，走 engine 公共注入器） =====
// 注入形状按 settingsSchema.ts HookDefinition（2026-09-23 源码核对，校验宽松：
// 多余字段无害）：事件键 PascalCase → [{matcher?, hooks:[{type,command,name?,timeout?}]}]
// ⚠️ Gemini 与 CC 的差异：无 async 字段（同步执行 hook，桥脚本必须毫秒级退出），
// timeout 单位为毫秒（默认 60000）。

mod hooks {
    use std::path::PathBuf;

    use super::{gemini_home, inject_json_hooks, uninstall_json_hooks};

    /// 桥脚本注入标记（卸载识别自家条目；与 CC 共用同一份桥脚本源码）
    pub const BRIDGE_MARK: &str = "hook-bridge.js";
    /// 注入事件集（11 事件中状态机可消费的 7 个；语义对齐 CC 7 事件）：
    /// BeforeAgent↔UserPromptSubmit、BeforeTool/AfterTool↔Pre/PostToolUse、
    /// AfterAgent↔Stop；Notification=权限确认（waiting 信号）；
    /// BeforeModel/AfterModel/BeforeToolSelection/PreCompress 无增量价值不注入
    /// （同步 hook 每事件要 spawn 一次 PowerShell，精挑低频集）
    pub const HOOK_EVENTS: &[&str] = &[
        "SessionStart",
        "BeforeAgent",
        "BeforeTool",
        "AfterTool",
        "Notification",
        "AfterAgent",
        "SessionEnd",
    ];

    fn settings_path() -> Option<PathBuf> {
        Some(gemini_home()?.join("settings.json"))
    }

    fn bridge_script_path() -> Option<PathBuf> {
        Some(gemini_home()?.join("hooks").join("hook-bridge.js"))
    }

    /// 查询 hooks 是否已安装（settings.json 中存在自家注入条目）
    pub fn hooks_installed() -> bool {
        let Some(path) = settings_path() else { return false };
        if !path.exists() {
            return false;
        }
        std::fs::read_to_string(&path)
            .map(|raw| raw.contains(BRIDGE_MARK))
            .unwrap_or(false)
    }

    /// 安装：①桥脚本写出到 ~\.gemini\hooks\hook-bridge.js；
    /// ②settings.json 备份后合并注入 7 事件（防重复）；返回注入条数
    pub fn install_hooks() -> anyhow::Result<usize> {
        let bridge = bridge_script_path().ok_or_else(|| anyhow::anyhow!("无法定位用户目录"))?;
        if let Some(dir) = bridge.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&bridge, super::super::claude_code::BRIDGE_SOURCE)?;
        let settings = settings_path().ok_or_else(|| anyhow::anyhow!("无法定位 settings.json"))?;
        // 第二参数是 Agent 标识：桥脚本按它把事件写入各自的 events/<agent>.jsonl
        let cmd = format!("node \"{}\" gemini", bridge.to_string_lossy().replace('\\', "/"));
        let entry = serde_json::json!({
            "hooks": [{ "type": "command", "command": cmd, "timeout": 5000 }]
        });
        let injected = inject_json_hooks(&settings, HOOK_EVENTS, entry, BRIDGE_MARK)?;
        log::info!("[gemini] hooks 安装完成：注入 {injected} 个事件，桥脚本 {}", bridge.display());
        Ok(injected)
    }

    /// 卸载：移除全部自家注入条目；桥脚本文件保留（重装免复制，且无副作用）
    pub fn uninstall_hooks() -> anyhow::Result<usize> {
        let settings = settings_path().ok_or_else(|| anyhow::anyhow!("无法定位 settings.json"))?;
        let removed = uninstall_json_hooks(&settings, BRIDGE_MARK)?;
        log::info!("[gemini] hooks 卸载完成：移除 {removed} 个注入条目");
        Ok(removed)
    }
}

pub use hooks::{hooks_installed, install_hooks, uninstall_hooks};

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("at-gem-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 合成转录解析：metadata 头/gemini 用量行/同 id 重 append 大者保留/
    /// $set 标题/$rewindTo 忽略/error 行/缓存拆分（input = prompt − cached）
    #[test]
    fn test_collect_parse_lines() {
        let dir = tmp_dir("parse");
        let chats = dir.join("tmp").join("my-project").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        let file = chats.join("session-2026-09-23T10-00-abcd1234.jsonl");
        let lines = [
            // 首行 metadata
            r#"{"sessionId":"sess-g1","projectHash":"h","startTime":"2026-09-23T10:00:00.000Z","lastUpdated":"2026-09-23T10:00:10.000Z","kind":"main","directories":["F:\\proj"]}"#,
            // user 行：无 tokens，忽略
            r#"{"id":"u1","timestamp":"2026-09-23T10:00:01.000Z","type":"user","content":"你好","displayContent":"你好"}"#,
            // gemini 用量行：prompt=500 cached=200 → input=300
            r#"{"id":"m1","timestamp":"2026-09-23T10:00:02.000Z","type":"gemini","model":"gemini-2.5-pro","content":"答","tokens":{"input":500,"output":100,"cached":200,"thoughts":30,"total":830}}"#,
            // 同 id 重 append（token 元数据后到更完整）：保留大者
            r#"{"id":"m1","timestamp":"2026-09-23T10:00:02.000Z","type":"gemini","model":"gemini-2.5-pro","content":"答","tokens":{"input":900,"output":150,"cached":200,"thoughts":40,"total":1290}}"#,
            // 独立消息
            r#"{"id":"m2","timestamp":"2026-09-23T10:00:05.000Z","type":"gemini","model":"gemini-2.5-flash","tokens":{"input":10,"output":5}}"#,
            // error 行：喂 recent_error
            r#"{"id":"e1","timestamp":"2026-09-23T10:00:07.000Z","type":"error","displayContent":"quota exceeded"}"#,
            // $set 标题（带 messages 全量数组：应被忽略不崩）
            r#"{""$set"":{""summary"":""项目会话标题"",""messages"":[]}}"#,
            // $rewindTo 控制行：忽略
            r#"{""$rewindTo"":""u1""}"#,
        ];
        // raw string 内以连续双引号表达字面双引号，统一替换回标准 JSON
        let content: String = lines
            .iter()
            .map(|l| l.replace("\"\"", "\""))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file, format!("{content}\n")).unwrap();

        let ad = GeminiAdapter::with_root(dir.clone());
        let out = ad.collect_usage(0).unwrap();
        // m1（大快照）＋m2＋e1 共 3 行
        assert_eq!(out.rows.len(), 3, "同 id 重 append 应合并，实际 {:?}", out.rows.iter().map(|r| r.source_id.clone()).collect::<Vec<_>>());
        assert!(out.rows.iter().all(|r| r.session_id == "gemini:sess-g1"));
        // 缓存拆分：prompt 900 − cached 200 = 700；thoughts 进 reasoning
        let m1 = out.rows.iter().find(|r| r.source_id.as_deref() == Some("gm:m1")).unwrap();
        assert_eq!(m1.input_tokens, Some(700));
        assert_eq!(m1.output_tokens, Some(150));
        assert_eq!(m1.reasoning_tokens, Some(40));
        assert_eq!(m1.cache_read_tokens, Some(200));
        assert_eq!(m1.model, "gemini-2.5-pro");
        assert_eq!(m1.provider.as_deref(), Some("google"));
        // m2 无 cached/thoughts：照抄
        let m2 = out.rows.iter().find(|r| r.source_id.as_deref() == Some("gm:m2")).unwrap();
        assert_eq!(m2.input_tokens, Some(10));
        assert_eq!(m2.reasoning_tokens, Some(0));
        assert_eq!(m2.model, "gemini-2.5-flash");
        // error 行
        let e1 = out.rows.iter().find(|r| r.source_id.as_deref() == Some("gm:e1")).unwrap();
        assert_eq!(e1.error_type.as_deref(), Some("quota exceeded"));
        assert_eq!(e1.input_tokens, None);
        // $set 标题
        assert_eq!(out.titles, vec![("gemini:sess-g1".to_string(), "项目会话标题".to_string())]);

        // 会话发现：头缓存承接（sessionId/directories/首条 user 标题）
        let sessions = ad.scan_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "gemini:sess-g1");
        assert_eq!(sessions[0].project_dir.as_deref(), Some("F:\\proj"));
        assert_eq!(sessions[0].title.as_deref(), Some("你好"));
    }

    /// 水位增量：同实例第二次采集只产出新增行（锁定增量路径不回退）
    #[test]
    fn test_collect_incremental() {
        let dir = tmp_dir("incr");
        let chats = dir.join("tmp").join("p").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        let file = chats.join("session-2026-09-15T11-00-abcd1234.jsonl");
        // 时间戳用明显过去的日期：文件级过滤按 mtime（现在）> watermark，
        // 若用"今天"的时间戳，本机时钟未到该 UTC 时刻时 mtime 会小于水位（CC 同款教训）
        let meta = r#"{"sessionId":"sess-g2","startTime":"2026-09-15T11:00:00.000Z","directories":["F:\\x"]}"#;
        let m1 = r#"{"id":"i1","timestamp":"2026-09-15T11:00:01.000Z","type":"gemini","model":"gemini-2.5-pro","tokens":{"input":100,"output":10}}"#;
        std::fs::write(&file, format!("{meta}\n{m1}\n")).unwrap();

        let ad = GeminiAdapter::with_root(dir.clone());
        let r1 = ad.collect_usage(0).unwrap().rows;
        assert_eq!(r1.len(), 1);
        let ts1 = chrono::DateTime::parse_from_rfc3339("2026-09-15T11:00:01.000Z")
            .unwrap()
            .timestamp_millis();

        // 追加第二条 → 只产出新增 1 行
        // （等待跨过 Windows mtime 定时器粒度：两次写间隔太近 mtime 相同，
        // changed() 会误判「无新增」——engine 自测同款处理）
        std::thread::sleep(std::time::Duration::from_millis(30));
        let m2 = r#"{"id":"i2","timestamp":"2026-09-15T11:00:02.000Z","type":"gemini","model":"gemini-2.5-pro","tokens":{"input":20,"output":8}}"#;
        std::fs::write(&file, format!("{meta}\n{m1}\n{m2}\n")).unwrap();
        let r2 = ad.collect_usage(ts1).unwrap().rows;
        assert_eq!(r2.len(), 1, "第二次采集应只含新增行");
        assert_eq!(r2[0].source_id.as_deref(), Some("gm:i2"));

        // 文件未变化 → 0 行
        let r3 = ad.collect_usage(0).unwrap().rows;
        assert!(r3.is_empty(), "mtime 未变时应跳过文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// hooks 注入/卸载往返：临时 settings.json，验证防重复与完整还原
    /// （条目形状为 Gemini 版：无 async、timeout 毫秒）
    #[test]
    fn test_hooks_install_uninstall_roundtrip() {
        let dir = tmp_dir("hooks");
        let settings = dir.join("settings.json");
        std::fs::write(&settings, r#"{
          "theme": "auto",
          "hooks": {
            "AfterAgent": [{"hooks": [{"type": "command", "command": "powershell.exe -File notify.ps1"}]}]
          }
        }"#).unwrap();

        let cmd = "node \"C:/x/.gemini/hooks/hook-bridge.js\" gemini";
        let entry = serde_json::json!({
            "hooks": [{ "type": "command", "command": cmd, "timeout": 5000 }]
        });
        let n = inject_json_hooks(&settings, hooks::HOOK_EVENTS, entry, hooks::BRIDGE_MARK).unwrap();
        assert_eq!(n, 7);
        let s1: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(s1["hooks"]["AfterAgent"].as_array().unwrap().len(), 2, "已有事件应追加");
        assert_eq!(s1["hooks"]["BeforeAgent"].as_array().unwrap().len(), 1);
        assert_eq!(s1["theme"], "auto", "其他配置不受影响");
        let injected = &s1["hooks"]["BeforeAgent"][0]["hooks"][0];
        assert_eq!(injected["timeout"], 5000, "Gemini timeout 单位毫秒");
        assert!(injected.get("async").is_none(), "Gemini 无 async 字段");

        // 重复注入：防重复，0 条
        let entry2 = serde_json::json!({
            "hooks": [{ "type": "command", "command": cmd, "timeout": 5000 }]
        });
        let n2 = inject_json_hooks(&settings, hooks::HOOK_EVENTS, entry2, hooks::BRIDGE_MARK).unwrap();
        assert_eq!(n2, 0);

        // 卸载：自家条目全清，用户配置原样保留
        let removed = uninstall_json_hooks(&settings, hooks::BRIDGE_MARK).unwrap();
        assert_eq!(removed, 7);
        let s2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(s2["hooks"]["AfterAgent"].as_array().unwrap().len(), 1);
        assert!(!s2["hooks"].as_object().unwrap().contains_key("BeforeAgent"));
        assert_eq!(s2["theme"], "auto");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OTel outfile 通道挂载（M2-12）：settings.json 配置 outfile 后，api_error 错误
    /// 信号行经适配器并入采集输出走 recent_error；api_response token 行按所有者裁定
    /// 暂不入库，不得出现。记录形态为 OTel SDK 真实序列化（pretty JSON，仅
    /// resource/instrumentationScope/attributes 三键，时间戳只在 attributes 里）
    #[test]
    fn test_otel_error_channel_integrated() {
        let dir = tmp_dir("otgem");
        let outfile = dir.join("telemetry.log");
        // OTel log 记录的 pretty JSON（模拟 safeJsonStringify(data,2)+'\n'）：
        // event.name 只存在于 attributes 内（OTel SDK 序列化真实形态）
        let log_record = |event: &str, mut attrs: serde_json::Value| {
            if let Some(obj) = attrs.as_object_mut() {
                obj.insert("event.name".into(), serde_json::json!(event));
            }
            let all = serde_json::json!({
                "resource": {"attributes": {"service.name": "gemini-cli"}},
                "instrumentationScope": {"name": "gemini-cli"},
                "attributes": attrs,
            });
            let mut s = serde_json::to_string_pretty(&all).unwrap();
            s.push('\n');
            s
        };
        // api_error：error.type 优先于 error（otel.rs error_row 的字段宽容序列）
        // 时间戳用过去明确日期：文件级过滤按 mtime（现在）> watermark，避开本机时钟边界
        // （文件里现有测试同款注释）
        let mut content = String::new();
        content.push_str(&log_record(
            "gemini_cli.api_error",
            serde_json::json!({
                "session.id": "sess-ot1",
                "event.timestamp": "2026-09-15T10:00:01.000Z",
                "model": "gemini-2.5-pro",
                "error": "429 Too Many Requests",
                "error.type": "rate_limit",
            }),
        ));
        // api_response token 记录：验证其不出现（暂不入库）
        content.push_str(&log_record(
            "gemini_cli.api_response",
            serde_json::json!({
                "session.id": "sess-ot1",
                "event.timestamp": "2026-09-15T10:00:02.000Z",
                "model": "gemini-2.5-pro",
                "input_token_count": 900,
                "output_token_count": 150,
                "cached_content_token_count": 200,
            }),
        ));
        std::fs::write(&outfile, &content).unwrap();
        // settings.json：outfile 写绝对路径（serde_json::to_string 生成路径字面量防
        // Windows 反斜杠转义坑）
        let settings = dir.join("settings.json");
        let settings_text = format!(
            r#"{{"telemetry": {{"enabled": true, "outfile": {}}}}}"#,
            serde_json::to_string(outfile.to_str().unwrap()).unwrap()
        );
        std::fs::write(&settings, settings_text).unwrap();

        let ad = GeminiAdapter::with_root(dir.clone());
        let out = ad.collect_usage(0).unwrap();
        // 恰 1 行：错误信号行并入，token 行不入
        assert_eq!(out.rows.len(), 1, "只应有 otel 错误行，实际 {:?}", out.rows);
        let e = &out.rows[0];
        assert_eq!(e.session_id, "gemini:sess-ot1");
        assert_eq!(e.error_type.as_deref(), Some("rate_limit"));
        assert!(e.source_id.as_deref().unwrap().starts_with("ote:"));
        assert_eq!(e.input_tokens, None);
        // token 行不出现（source 以 ote: 开头且带 input 的行一条都没有）
        assert!(out.rows.iter().all(|r| !matches!(
            (r.source_id.as_deref(), r.input_tokens),
            (Some(s), Some(_)) if s.starts_with("ote:")
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 集成：本机真实转录（手动：cargo test -- --ignored test_real_gemini；
    /// 装机后补跑，清单见 01-RESEARCH §13.3）
    #[test]
    #[ignore]
    fn test_real_gemini() {
        let ad = GeminiAdapter::new();
        let sessions = ad.scan_sessions().unwrap();
        assert!(!sessions.is_empty(), "本机应有 Gemini CLI 会话（未装则本测试不适用）");
        let out = ad.collect_usage(0).unwrap();
        assert!(!out.rows.is_empty(), "本机应有历史用量");
        assert!(out.rows.iter().all(|u| u.session_id.starts_with("gemini:")));
        // 幂等重采零重复：同 source_id 不会出现两行
        let mut ids: Vec<_> = out.rows.iter().filter_map(|r| r.source_id.clone()).collect();
        ids.sort();
        let n = ids.len();
        ids.dedup();
        assert_eq!(n, ids.len(), "source_id 不得重复");
        // 水位增量：紧接第二次采集应接近空
        let max_ts = out.rows.iter().map(|u| u.ts).max().unwrap();
        let second = ad.collect_usage(max_ts).unwrap().rows;
        assert!(second.len() <= 5, "水位增量应接近空，实际 {} 行", second.len());
    }
}
